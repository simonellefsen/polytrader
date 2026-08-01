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
                    // Per event: the FULL live member count and the worst per-leg taker rate. Both
                    // set the event's fee hurdle (see `completion_hurdle`), so they must be taken
                    // over every member, not just the ones we happen to be missing.
                    let mut size_by_event: std::collections::BTreeMap<String, usize> =
                        std::collections::BTreeMap::new();
                    let mut rate_by_event: std::collections::BTreeMap<
                        String,
                        rust_decimal::Decimal,
                    > = std::collections::BTreeMap::new();
                    // Group the members we do NOT already have by event.
                    let mut missing_by_event: std::collections::BTreeMap<
                        String,
                        Vec<gamma::Market>,
                    > = std::collections::BTreeMap::new();
                    for m in members {
                        let Some(ev) = m.event_id.clone() else {
                            continue;
                        };
                        *size_by_event.entry(ev.clone()).or_default() += 1;
                        let rate = m
                            .taker_fee_rate
                            .unwrap_or_else(|| crate::polymarket_taker_fee_rate(&m.slug));
                        let e = rate_by_event.entry(ev.clone()).or_default();
                        if rate > *e {
                            *e = rate;
                        }
                        if seen.contains(&m.id) {
                            continue;
                        }
                        missing_by_event.entry(ev).or_default().push(m);
                    }
                    let sizes: Vec<CompletionCandidate> = missing_by_event
                        .iter()
                        .map(|(ev, ms)| CompletionCandidate {
                            event_id: ev.clone(),
                            missing: ms.len(),
                            members: size_by_event.get(ev).copied().unwrap_or(ms.len()),
                            fee_rate: rate_by_event.get(ev).copied().unwrap_or_default(),
                        })
                        .collect();
                    // How the budget actually got spent, by fee class — the check on whether the
                    // hurdle ordering is doing its job. Cheapest-first sent 250 of ~300 books to
                    // 5%-fee events (bar ~1.05) and 50 to the fee-free ones (bar 1.00).
                    let fee_free_available = sizes.iter().filter(|c| c.fee_rate.is_zero()).count();
                    let chosen = select_completion_events(sizes, completion_limit);
                    let fee_free_chosen = rate_by_event
                        .iter()
                        .filter(|(ev, r)| r.is_zero() && chosen.contains(*ev))
                        .count();
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
                        fee_free_available,
                        fee_free_chosen,
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

/// One negRisk event competing for the completion budget.
#[derive(Debug, Clone)]
struct CompletionCandidate {
    event_id: String,
    /// Member books we do not yet hold — what completing this event costs.
    missing: usize,
    /// FULL live member count (k), which sets the basket's fee hurdle.
    members: usize,
    /// Worst per-leg taker rate across the event's members.
    fee_rate: rust_decimal::Decimal,
}

/// Estimated fee hurdle: how far above 1.00 this event's implied-Yes sum must reach before the
/// basket is an arb at all.
///
/// Buying No across k legs pays `rate × (S − Σq²)` in taker fees (`q_i = 1 − ask_no_i`, `S = Σq_i`).
/// With `S ≈ 1` and the legs evenly spread, `Σq² ≈ 1/k`, so the hurdle is **`rate × (1 − 1/k)`**.
/// Two properties fall straight out, and both are load-bearing:
/// - **Fee-free events have hurdle 0 at any k.** Their real arb line is 1.00, so any overround at
///   all is capturable.
/// - **The hurdle RISES with k.** Σq² shrinks as a basket spreads, so total fees climb toward the
///   full rate: 5% × (1 − 1/3) = 3.3% at 3 legs vs 5% × (1 − 1/30) = 4.8% at 30. This is the
///   missing explanation for a pattern already in our record — every profitable basket we have
///   executed ran **3–11 legs**.
fn completion_hurdle(fee_rate: rust_decimal::Decimal, members: usize) -> rust_decimal::Decimal {
    if members == 0 {
        return fee_rate;
    }
    fee_rate
        * (rust_decimal::Decimal::ONE
            - rust_decimal::Decimal::ONE / rust_decimal::Decimal::from(members as u64))
}

/// Choose which negRisk events to top up this tick, under a budget in member-markets.
///
/// **Lowest fee-hurdle first, whole events only.**
/// - *Whole events*, because the arb line is `Σ(1 − ask_no) > 1` over the legs we can see. Spending
///   the budget half-covering one ladder buys a basket that still cannot clear the line, so a
///   partially funded event is wasted budget rather than partial progress.
/// - *Lowest hurdle first* (2026-08-01, replacing cheapest-first). Cheapest-first is fee- and
///   shape-blind, and it showed: it spent 250 of ~300 books on 5%-fee events where the bar is ~1.05
///   and only 50 on the fee-free events where it is 1.00 — funding precisely the baskets least able
///   to pay. Ordering by `completion_hurdle` puts fee-free events first at any size, then
///   concentrated events over spread ones, which is the order of "could this basket ever clear?".
///
/// Ties break on cheapest-to-complete then event id, so the choice is deterministic tick-to-tick
/// rather than thrashing a different half of a tie group each pass.
///
/// An event that does not fit is SKIPPED, not a stopping point — unlike the old cost-ordered scan,
/// this order is not monotonic in cost, so a later event may still fit the remaining budget.
fn select_completion_events(
    mut candidates: Vec<CompletionCandidate>,
    budget: usize,
) -> std::collections::HashSet<String> {
    candidates.sort_by(|a, b| {
        completion_hurdle(a.fee_rate, a.members)
            .cmp(&completion_hurdle(b.fee_rate, b.members))
            .then_with(|| a.missing.cmp(&b.missing))
            .then_with(|| a.event_id.cmp(&b.event_id))
    });
    let mut chosen = std::collections::HashSet::new();
    let mut spent = 0usize;
    for c in candidates {
        if c.missing == 0 {
            continue; // already fully covered — costs nothing, gains nothing
        }
        if spent + c.missing > budget {
            continue; // does not fit; a cheaper later event still might
        }
        spent += c.missing;
        chosen.insert(c.event_id);
    }
    chosen
}

#[cfg(test)]
mod tests {
    use super::{completion_hurdle, select_completion_events, CompletionCandidate};
    use rust_decimal_macros::dec;

    /// (event_id, missing, total members, fee rate)
    fn cands(v: &[(&str, usize, usize, f64)]) -> Vec<CompletionCandidate> {
        v.iter()
            .map(|(id, missing, members, rate)| CompletionCandidate {
                event_id: id.to_string(),
                missing: *missing,
                members: *members,
                fee_rate: rust_decimal::Decimal::try_from(*rate).unwrap(),
            })
            .collect()
    }

    /// Same-shape events at one fee rate — isolates the cost tiebreak from the hurdle ordering.
    fn even(v: &[(&str, usize)]) -> Vec<CompletionCandidate> {
        cands(
            &v.iter()
                .map(|(id, missing)| (*id, *missing, 10usize, 0.05))
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn fee_free_events_win_the_budget_at_any_size() {
        // THE point of the 2026-08-01 reordering. Cheapest-first funded the 5%-fee events (bar
        // ~1.05) and starved the fee-free ones (bar exactly 1.00) purely because they were smaller
        // to complete. A fee-free 40-leg event is a better bet than a 5%-fee 3-leg one.
        let chosen = select_completion_events(
            cands(&[("feefree_big", 20, 40, 0.0), ("fee5_small", 3, 3, 0.05)]),
            20,
        );
        assert!(chosen.contains("feefree_big"));
        assert!(
            !chosen.contains("fee5_small"),
            "budget went to the wrong one"
        );
    }

    #[test]
    fn among_equal_fee_rates_concentrated_events_come_first() {
        // The hurdle rises with leg count (fees climb toward the full rate as sum q^2 shrinks), so
        // a 3-leg 5% basket clears on an overround a 30-leg 5% basket cannot.
        assert!(completion_hurdle(dec!(0.05), 3) < completion_hurdle(dec!(0.05), 30));
        let chosen =
            select_completion_events(cands(&[("spread", 5, 30, 0.05), ("tight", 5, 3, 0.05)]), 5);
        assert!(chosen.contains("tight") && !chosen.contains("spread"));
    }

    #[test]
    fn fee_free_hurdle_is_zero_regardless_of_leg_count() {
        for k in [2usize, 3, 30, 50] {
            assert_eq!(completion_hurdle(dec!(0), k), dec!(0), "k={k}");
        }
    }

    #[test]
    fn completes_cheapest_events_first_within_a_hurdle_tier() {
        // Budget 10 buys the three small events (2+3+4=9); the 20-member one never fits.
        let chosen =
            select_completion_events(even(&[("big", 20), ("c", 4), ("a", 2), ("b", 3)]), 10);
        assert_eq!(chosen.len(), 3);
        assert!(chosen.contains("a") && chosen.contains("b") && chosen.contains("c"));
        assert!(!chosen.contains("big"));
    }

    #[test]
    fn never_part_funds_an_event() {
        // 6 of the 8 missing legs would fit, but a half-covered ladder cannot clear
        // `sum(1 - ask_no) > 1`, so the budget goes unspent rather than buying an unusable basket.
        let chosen = select_completion_events(even(&[("solo", 8)]), 6);
        assert!(chosen.is_empty());
    }

    #[test]
    fn an_event_that_does_not_fit_is_skipped_not_a_stopping_point() {
        // Changed 2026-08-01 with the hurdle ordering: the scan is no longer monotonic in cost, so
        // stopping at the first misfit would strand budget a later, cheaper event can use. Here
        // "b" (5) does not fit alongside "a" (2) under a budget of 6, but "c" (3) does.
        let chosen = select_completion_events(even(&[("a", 2), ("b", 5), ("c", 3)]), 6);
        assert!(chosen.contains("a") && chosen.contains("c"));
        assert!(!chosen.contains("b"));
    }

    #[test]
    fn fully_covered_events_cost_nothing() {
        // A 0-missing event must not consume budget that a real event could use.
        let chosen = select_completion_events(even(&[("done", 0), ("a", 3), ("b", 3)]), 6);
        assert_eq!(chosen.len(), 2);
        assert!(!chosen.contains("done"));
    }

    #[test]
    fn zero_budget_disables_the_pass() {
        assert!(select_completion_events(even(&[("a", 1)]), 0).is_empty());
    }

    #[test]
    fn selection_is_deterministic_across_ties() {
        // Identical hurdle AND cost: id order decides, so consecutive ticks complete the same
        // events instead of thrashing a different half of the tie group each pass.
        let a = select_completion_events(even(&[("y", 3), ("x", 3), ("z", 3)]), 6);
        let b = select_completion_events(even(&[("z", 3), ("y", 3), ("x", 3)]), 6);
        assert_eq!(a, b);
        assert!(a.contains("x") && a.contains("y"));
    }
}
