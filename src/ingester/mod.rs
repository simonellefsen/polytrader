//! Market data ingester: Gamma API + public CLOB (orderbooks, trades, prices).
//! Feeds both the UI and the PaperTradingEngine.
//!
//! Periodic task keeps markets + orderbook_snapshots fresh for paper engine.

mod clob_public;
mod gamma;

use anyhow::Result;
use sqlx::PgPool;

pub use clob_public::{ClobPublicClient, OrderbookSnapshot, PriceSize};
pub use gamma::GammaClient;

#[cfg(feature = "clob-ws")]
#[allow(unused_imports)]
pub use clob_public::ClobWsClient;

/// One ingestion tick: fetch configured bootstrap markets, upsert to DB + snapshots + mids.
/// Conservative sleeps between calls to be polite to public endpoints.
pub async fn ingest_tick(
    gamma: &GammaClient,
    clob: &ClobPublicClient,
    pool: &PgPool,
    bootstrap: &[String],
) -> Result<()> {
    // Build the scan universe, deduped by gamma id:
    //   (1) curated bootstrap slugs (or generic active discovery when no allowlist), PLUS
    //   (2) volume-ranked arb-discovery markets (opt-in via POLYTRADER_ARB_DISCOVERY_LIMIT) — breadth
    //       is the arb frequency lever: dislocations are rare per-market but scale with books watched, PLUS
    //   (3) every market we currently hold a position in — a CORRECTNESS guarantee: a held market that
    //       rotates out of the top-N discovery list must still be re-ingested so it resolves/settles
    //       (never go blind; same class of bug as the 2026-06-17 settlement blocker) — plus every
    //       active directional-rotation promotion, which needs books/DRs regardless of discovery rank.
    let discovery_limit = std::env::var("POLYTRADER_ARB_DISCOVERY_LIMIT")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);

    let mut candidates: Vec<gamma::Market> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // (1) curated bootstrap (or generic active discovery when no allowlist).
    let base_markets = if bootstrap.is_empty() {
        gamma.list_active_markets().await?
    } else {
        gamma.fetch_markets_by_slugs(bootstrap).await?
    };
    for m in base_markets {
        if !m.id.is_empty() && seen.insert(m.id.clone()) {
            candidates.push(m);
        }
    }

    // (2) volume-ranked arb discovery (opt-in). Non-fatal: bootstrap still ingests on failure.
    if discovery_limit > 0 {
        match gamma.discover_arb_markets(discovery_limit).await {
            Ok(ms) => {
                for m in ms {
                    if seen.insert(m.id.clone()) {
                        candidates.push(m);
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "arb-discovery fetch failed; continuing with bootstrap only")
            }
        }
    }

    // (3) held-position markets not already in the universe (settlement-tracking guarantee), PLUS
    //     active directional-rotation promotions — a promoted market needs orderbook snapshots and
    //     decision reports even when it sits outside the volume-ranked discovery top-N (UNION keeps
    //     this one query; both sets share the "must never go blind" property).
    let must_track_slugs: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT m.slug FROM paper_trading.paper_positions p
         JOIN market_data.markets m ON m.gamma_id = p.market_id
         WHERE p.shares > 0 AND COALESCE(m.slug, '') <> ''
         UNION
         SELECT slug FROM market_data.directional_universe WHERE demoted_at IS NULL",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let missing_must_track: Vec<String> = must_track_slugs
        .into_iter()
        .filter(|s| !candidates.iter().any(|m| &m.slug == s))
        .collect();
    if !missing_must_track.is_empty() {
        for m in gamma
            .fetch_markets_by_slugs(&missing_must_track)
            .await
            .unwrap_or_default()
        {
            if seen.insert(m.id.clone()) {
                candidates.push(m);
            }
        }
    }

    tracing::info!(
        universe = candidates.len(),
        discovery_limit,
        "ingest universe built"
    );
    let mut processed = 0usize;

    for m in candidates {
        let outcomes_j = serde_json::to_value(&m.outcomes)?;
        let tokens_j = serde_json::to_value(&m.clob_token_ids)?;
        let prices_j = serde_json::to_value(&m.outcome_prices)?;

        // Resolution: when closed and exactly one outcome is priced ~$1, that's the winner.
        // (Normalized to canonical "Yes"/"No" to match position rows.)
        let resolved_outcome: Option<String> = if m.closed {
            let winners: Vec<usize> = m
                .outcome_prices
                .iter()
                .enumerate()
                .filter(|(_, p)| p.parse::<f64>().map(|x| x >= 0.99).unwrap_or(false))
                .map(|(i, _)| i)
                .collect();
            if winners.len() == 1 {
                m.outcomes.get(winners[0]).map(|o| {
                    if o.eq_ignore_ascii_case("yes") {
                        "Yes".to_string()
                    } else if o.eq_ignore_ascii_case("no") {
                        "No".to_string()
                    } else {
                        o.clone()
                    }
                })
            } else {
                None
            }
        } else {
            None
        };

        // Upsert market (resolution fields refreshed on conflict so closes are captured).
        sqlx::query(
            r#"INSERT INTO market_data.markets
               (gamma_id, slug, question, outcomes, clob_token_ids, active, closed, updated_at, raw_json, outcome_prices, resolved_outcome, taker_fee_rate, event_id, neg_risk)
               VALUES ($1, $2, $3, $4, $5, $6, $7, now(), $8, $9, $10, $11, $12, $13)
               ON CONFLICT (gamma_id) DO UPDATE SET
                 slug = EXCLUDED.slug,
                 question = EXCLUDED.question,
                 active = EXCLUDED.active,
                 closed = EXCLUDED.closed,
                 outcome_prices = EXCLUDED.outcome_prices,
                 resolved_outcome = COALESCE(EXCLUDED.resolved_outcome, market_data.markets.resolved_outcome),
                 raw_json = EXCLUDED.raw_json,
                 -- refresh the fee rate when Gamma reports one; keep the last known value if absent
                 taker_fee_rate = COALESCE(EXCLUDED.taker_fee_rate, market_data.markets.taker_fee_rate),
                 event_id = COALESCE(EXCLUDED.event_id, market_data.markets.event_id),
                 neg_risk = EXCLUDED.neg_risk OR market_data.markets.neg_risk,
                 updated_at = now()"#,
        )
        .bind(&m.id)
        .bind(&m.slug)
        .bind(&m.question)
        .bind(outcomes_j)
        .bind(tokens_j)
        .bind(m.active)
        .bind(m.closed)
        .bind(serde_json::json!(&m)) // raw for now
        .bind(prices_j)
        .bind(&resolved_outcome)
        .bind(m.taker_fee_rate)
        .bind(&m.event_id)
        .bind(m.neg_risk)
        .execute(pool)
        .await?;

        // Closed/resolved markets have no live orderbook — their CLOB books return errors every cycle
        // (was ~29% of all book fetches: 16 of 50 tracked markets are closed, each failing 2 tokens ×
        // 12 cycles/h = the "CLOB orderbook fetch failed" log flood). We've already captured their
        // resolution from the Gamma market above, so skip the dead book fetch entirely.
        if m.closed {
            continue;
        }

        // For each outcome token, fetch book + mid, store snapshot, update market mids
        for (i, token) in m.clob_token_ids.iter().enumerate() {
            let outcome = m
                .outcomes
                .get(i)
                .cloned()
                .unwrap_or_else(|| "Unknown".to_string());
            // Normalize to canonical title-case for DB CHECK (outcome IN ('Yes','No')) and consistency
            let outcome = if outcome.eq_ignore_ascii_case("yes") {
                "Yes".to_string()
            } else if outcome.eq_ignore_ascii_case("no") {
                "No".to_string()
            } else {
                outcome
            };

            match clob.get_orderbook(token).await {
                Ok(book) => {
                    let bids_j = serde_json::to_value(&book.bids)?;
                    let asks_j = serde_json::to_value(&book.asks)?;
                    let mid = book.mid.or_else(|| ClobPublicClient::mid_from_book(&book));

                    sqlx::query(
                        r#"INSERT INTO market_data.orderbook_snapshots
                           (token_id, market_id, outcome, bids, asks, mid, fetched_at)
                           VALUES ($1, $2, $3, $4, $5, $6, $7)"#,
                    )
                    .bind(token)
                    .bind(&m.id)
                    .bind(&outcome)
                    .bind(bids_j)
                    .bind(asks_j)
                    .bind(mid)
                    .bind(book.fetched_at)
                    .execute(pool)
                    .await?;

                    // Update denormalized mid on market row (use outcome string after normalization for robustness, not index)
                    let mid_col = if outcome == "Yes" {
                        "last_mid_yes"
                    } else {
                        "last_mid_no"
                    };
                    let up = format!(
                        "UPDATE market_data.markets SET {} = $1 WHERE gamma_id = $2",
                        mid_col
                    );
                    sqlx::query(&up)
                        .bind(mid)
                        .bind(&m.id)
                        .execute(pool)
                        .await
                        .ok(); // best effort
                }
                Err(e) => {
                    tracing::warn!(token = %token, error = %e, "CLOB orderbook fetch failed during ingest");
                }
            }

            // Polite rate limit / backoff for public API (Phase 0 conservative)
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }

        processed += 1;
    }

    tracing::info!(
        processed,
        "ingestion tick complete (markets + orderbook snapshots)"
    );
    Ok(())
}
