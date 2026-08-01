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
    //     decision reports even when it sits outside the volume-ranked discovery top-N, PLUS
    //     predicted next-period recurring-ladder slugs (rotation::ladder) — force-tracks a new
    //     ladder instance the moment Gamma lists it, instead of waiting for it to organically rank
    //     into the volume-based discovery (UNION keeps this one query; all three sets share the
    //     "must never go blind to a market we care about" property).
    let must_track_slugs: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT m.slug FROM paper_trading.paper_positions p
         JOIN market_data.markets m ON m.gamma_id = p.market_id
         WHERE p.shares > 0 AND COALESCE(m.slug, '') <> ''
         UNION
         SELECT slug FROM market_data.directional_universe WHERE demoted_at IS NULL
         UNION
         SELECT slug FROM market_data.ladder_watchlist",
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

    // (4) negRisk EVENT COMPLETION. Steps (1)-(3) rank individual markets, so a multi-leg negRisk
    //     event enters the universe as whichever few of its legs trade heavily and the rest stay
    //     invisible. That is fatal for the event arb specifically: the line is `Σ(1 − ask_no) > 1`,
    //     so a 3-of-50 fragment sums nowhere near 1 however dislocated the event actually is.
    //     Measured 2026-08-01: 31 negRisk events touched, 530 live member books on Gamma, 76 held
    //     fresh — the scanner was judging 14% of each event and (correctly) reporting no arb.
    //     Books here are fetched No-side only (see `no_book_only`), so a member costs half a normal
    //     candidate.
    let completion_limit = std::env::var("POLYTRADER_NEGRISK_COMPLETION_LIMIT")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    // Member markets added purely to complete a negRisk basket: the event scanner reads the No book
    // only, so skip the Yes fetch and buy twice the coverage per second of tick budget. Nothing
    // regresses — these markets are not in the universe at all today, so no consumer loses a book
    // it currently has.
    let mut no_book_only: std::collections::HashSet<String> = std::collections::HashSet::new();
    if completion_limit > 0 {
        let event_ids: Vec<String> = candidates
            .iter()
            .filter(|m| m.neg_risk && !m.closed)
            .filter_map(|m| m.event_id.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        if !event_ids.is_empty() {
            match gamma.fetch_event_members(&event_ids).await {
                Ok(members) => {
                    // Group the members we do NOT already have by event.
                    let mut missing_by_event: std::collections::BTreeMap<
                        String,
                        Vec<gamma::Market>,
                    > = std::collections::BTreeMap::new();
                    for m in members {
                        if seen.contains(&m.id) {
                            continue;
                        }
                        if let Some(ev) = m.event_id.clone() {
                            missing_by_event.entry(ev).or_default().push(m);
                        }
                    }
                    let sizes: Vec<(String, usize)> = missing_by_event
                        .iter()
                        .map(|(ev, ms)| (ev.clone(), ms.len()))
                        .collect();
                    let chosen = select_completion_events(sizes, completion_limit);
                    let mut added = 0usize;
                    for (ev, ms) in missing_by_event {
                        if !chosen.contains(&ev) {
                            continue;
                        }
                        for m in ms {
                            if seen.insert(m.id.clone()) {
                                no_book_only.insert(m.id.clone());
                                candidates.push(m);
                                added += 1;
                            }
                        }
                    }
                    tracing::info!(
                        events_considered = event_ids.len(),
                        events_completed = chosen.len(),
                        members_added = added,
                        completion_limit,
                        "negRisk event completion"
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, "negRisk event completion failed; continuing without it")
                }
            }
        }
    }

    tracing::info!(
        universe = candidates.len(),
        discovery_limit,
        completion_members = no_book_only.len(),
        "ingest universe built"
    );
    let started = std::time::Instant::now();
    let mut processed = 0usize;

    for m in candidates {
        let no_only = no_book_only.contains(&m.id);
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

            // negRisk completion members are ingested for the buy-all-No basket only, which reads
            // the No book exclusively. Skipping the Yes fetch halves their cost, so the tick budget
            // buys twice the event coverage.
            if no_only && outcome != "No" {
                continue;
            }

            match clob.get_orderbook(token).await {
                Ok(None) => {
                    // Genuinely no live orderbook yet (404) — common+expected for arb-discovery-pool
                    // candidates that haven't started (scheduled esports/tennis matches) or just
                    // rolled over (5-min BTC updown rounds). Not a failure; debug-only.
                    tracing::debug!(token = %token, "no live orderbook for token (not yet started/thin; skipping this cycle)");
                }
                Ok(Some(book)) => {
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

    // Duration is the budget signal for POLYTRADER_NEGRISK_COMPLETION_LIMIT: the tick must finish
    // well inside POLYTRADER_INGEST_INTERVAL_SECS (300) or snapshots age past what the scanners
    // treat as fresh (the negRisk scan's join window is 30 minutes).
    tracing::info!(
        processed,
        elapsed_secs = started.elapsed().as_secs(),
        "ingestion tick complete (markets + orderbook snapshots)"
    );
    Ok(())
}

/// Choose which negRisk events to top up this tick, given each event's count of members we are
/// missing and a budget in member-markets. Returns the event ids to complete.
///
/// **Cheapest-first, whole events only.** Both halves matter:
/// - *Whole events*, because the arb line is `Σ(1 − ask_no) > 1` over the legs we can see. Spending
///   the budget half-covering one big ladder buys a basket that still cannot clear the line, so a
///   partially funded event is wasted budget rather than partial progress.
/// - *Cheapest-first*, because it maximises how many events become judgeable per book fetched. The
///   2026-08-01 distribution was long-tailed (missing counts 1,1,2,2,2,2,4,6,6,7,7,8,8,12,…,50), so
///   ascending order converts ~20 events for what the two largest alone would have cost.
///
/// Once the smallest remaining event does not fit, nothing later can either, so the scan stops.
fn select_completion_events(
    mut sizes: Vec<(String, usize)>,
    budget: usize,
) -> std::collections::HashSet<String> {
    // Ascending by missing-count; event id breaks ties so the choice is deterministic tick-to-tick.
    sizes.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    let mut chosen = std::collections::HashSet::new();
    let mut spent = 0usize;
    for (event_id, missing) in sizes {
        if missing == 0 {
            continue; // already fully covered — costs nothing, gains nothing
        }
        if spent + missing > budget {
            break;
        }
        spent += missing;
        chosen.insert(event_id);
    }
    chosen
}

#[cfg(test)]
mod tests {
    use super::select_completion_events;

    fn sizes(v: &[(&str, usize)]) -> Vec<(String, usize)> {
        v.iter().map(|(a, b)| (a.to_string(), *b)).collect()
    }

    #[test]
    fn completes_cheapest_events_first() {
        // Budget 10 buys the three small events (2+3+4=9); the 20-member one never fits.
        let chosen =
            select_completion_events(sizes(&[("big", 20), ("c", 4), ("a", 2), ("b", 3)]), 10);
        assert_eq!(chosen.len(), 3);
        assert!(chosen.contains("a") && chosen.contains("b") && chosen.contains("c"));
        assert!(!chosen.contains("big"));
    }

    #[test]
    fn never_part_funds_an_event() {
        // 6 of the 8 missing legs would fit, but a half-covered ladder cannot clear
        // `sum(1 - ask_no) > 1`, so the budget goes unspent rather than buying an unusable basket.
        let chosen = select_completion_events(sizes(&[("solo", 8)]), 6);
        assert!(chosen.is_empty());
    }

    #[test]
    fn stops_at_the_first_event_that_does_not_fit() {
        // Ascending order means a bigger event later can never fit either — and taking a LATER
        // small event after skipping one would make the choice depend on scan order.
        let chosen = select_completion_events(sizes(&[("a", 2), ("b", 5), ("c", 9)]), 6);
        assert_eq!(chosen.len(), 1);
        assert!(chosen.contains("a"));
    }

    #[test]
    fn fully_covered_events_cost_nothing() {
        // A 0-missing event must not consume budget that a real event could use.
        let chosen = select_completion_events(sizes(&[("done", 0), ("a", 3), ("b", 3)]), 6);
        assert_eq!(chosen.len(), 2);
        assert!(!chosen.contains("done"));
    }

    #[test]
    fn zero_budget_disables_the_pass() {
        assert!(select_completion_events(sizes(&[("a", 1)]), 0).is_empty());
    }

    #[test]
    fn selection_is_deterministic_across_ties() {
        // Equal missing-counts: id order decides, so consecutive ticks complete the same events
        // instead of thrashing a different half of the tie group each pass.
        let a = select_completion_events(sizes(&[("y", 3), ("x", 3), ("z", 3)]), 6);
        let b = select_completion_events(sizes(&[("z", 3), ("y", 3), ("x", 3)]), 6);
        assert_eq!(a, b);
        assert!(a.contains("x") && a.contains("y"));
    }
}
