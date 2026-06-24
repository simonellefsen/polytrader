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
    // When a bootstrap allowlist is configured, fetch those markets by slug directly (guaranteed
    // to retrieve them). Otherwise fall back to generic active-market discovery.
    let candidates = if bootstrap.is_empty() {
        gamma.list_active_markets().await?
    } else {
        gamma.fetch_markets_by_slugs(bootstrap).await?
    };
    let mut processed = 0usize;

    for m in candidates {
        // Defensive: with the slug path this always matches; kept so discovery mode + any stray
        // results are still constrained to the allowlist.
        if !bootstrap.is_empty() && !bootstrap.iter().any(|b| b == &m.id || b == &m.slug) {
            continue;
        }

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
               (gamma_id, slug, question, outcomes, clob_token_ids, active, closed, updated_at, raw_json, outcome_prices, resolved_outcome)
               VALUES ($1, $2, $3, $4, $5, $6, $7, now(), $8, $9, $10)
               ON CONFLICT (gamma_id) DO UPDATE SET
                 slug = EXCLUDED.slug,
                 question = EXCLUDED.question,
                 active = EXCLUDED.active,
                 closed = EXCLUDED.closed,
                 outcome_prices = EXCLUDED.outcome_prices,
                 resolved_outcome = COALESCE(EXCLUDED.resolved_outcome, market_data.markets.resolved_outcome),
                 raw_json = EXCLUDED.raw_json,
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
        .execute(pool)
        .await?;

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
