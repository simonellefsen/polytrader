//! Daily database garbage collection.
//!
//! The DB grows ~60MB/day, dominated by two fat, append-only tables: `orderbook_snapshots` (bids/asks
//! blobs, ~16k rows/day) and `journal.events` decision_reports (~1.7KB attribution payloads, ~5k/day).
//! The live system only needs a tiny working set (the latest book per market + recent reports); the
//! rest is history whose *useful* signal is thin (a book's mid, a report's per-signal fire counts).
//!
//! So this GC **rolls the thin signal into compact summaries, then prunes the fat raw rows** beyond the
//! hot/warm windows, always keeping the live working set. Runs once daily (spawned from main).
//! Retention plan + tiers: see wiki/roadmap.md.
//!
//! All deletes are batched (bounded statements, no giant table lock/bloat) and safe to re-run
//! (idempotent rollups via ON CONFLICT; keep-latest guards). Paper-only; touches no trading logic.

use anyhow::Result;
use sqlx::PgPool;

/// Keep full orderbook snapshots this recent (covers `recent_move`'s 3h window + buffer + recent
/// backtests). Older raw snapshots are rolled to hourly `price_history` then deleted.
const SNAPSHOT_RAW_HOURS: i64 = 48;
/// Keep raw decision_reports this recent. Older are rolled to daily `signal_daily` then deleted.
/// Was 30d; 14d on 2026-07-02; **7d on 2026-08-01** alongside the payload trim (compact zero-score
/// attribution + the 412-byte constant `note` removed), which together take the DR slice of
/// journal.events from ~340MB to roughly 100MB and paid for the DR_MARKET_LIMIT 40→50 raise.
/// 7d still covers every live consumer: the 24h scorecard, the 3h/24h/7d signal-health comparisons
/// (the longest baseline is exactly 7d), and Hermes's reflection window. What it gives up is
/// ad-hoc "what did we see 10 days ago" forensics at raw granularity — `signal_daily` keeps the
/// per-day aggregate indefinitely, so trend work is unaffected; only per-report detail ages out.
const REPORT_RAW_DAYS: i64 = 7;
/// Keep pure per-cycle telemetry (llm_health / real_account_balance) this recent; older is dropped.
const TELEMETRY_DAYS: i64 = 14;
/// Keep full-granularity (5-min) portfolio equity snapshots this recent (1D/1W chart); older
/// mark-to-market snapshots are thinned to 1/hour (fills/settlements/resets are always kept).
const PORTFOLIO_RAW_DAYS: i64 = 7;
/// Hard cap on the "keep the latest snapshot per (market, outcome)" exception in
/// `prune_orderbook_snapshots`. Found 2026-07-06: that exception is meant for the CURRENT live
/// working set, but a market that permanently rotates out of the ingest universe (arb-discovery/
/// rotation churn samples ~650 distinct markets/day) never gets a newer snapshot and its stale row
/// was kept FOREVER — 201 distinct markets stuck (183 not even formally closed) after ~3 weeks.
/// `rollup_price_history` already rolls up every row past `SNAPSHOT_RAW_HOURS` regardless of
/// latest-status, so the price signal is safe; nothing needs the raw book past this cap (any market
/// with a live purpose — bootstrap/rotation-active/held-position — gets fetched every ingest tick
/// via the must-track union, so it never goes this stale in the first place).
const STALE_LATEST_CAP_DAYS: i64 = 3;
/// Rows deleted per batch (bounds lock time / WAL per statement).
const BATCH: i64 = 10_000;
/// Decision reports kept per directionally-traded market, matched to what the consumer actually
/// reads.
///
/// The Signal Scorecard credits a signal from "the 20 MOST RECENT reports per market"
/// (`server.rs`, `rn <= 20`), NOT a time window. Exempting a directional market's reports wholesale
/// therefore over-retains badly: measured immediately after the exemption shipped, **31,635 of
/// 101,432 reports** were directional, because a market accumulates one report every 5 minutes for
/// as long as it is in the universe (~63 markets x ~500 reports each). Keeping 20 retains ~1,260
/// rows instead — the same information the scorecard can use, at 4% of the storage.
const REPORTS_KEPT_PER_DIRECTIONAL_MARKET: i64 = 20;

/// Keep shadow maker quotes (P5 increment 3b) this long after they finish. They are a measurement
/// sample, not a ledger — the conclusions get written to the roadmap, and the aggregate lives on in
/// the `maker_shadow_quotes` journal events. Only FINISHED quotes are eligible: an open quote is
/// live state and a filled one still awaiting its horizon mark has not produced its number yet, so
/// pruning either would silently delete the measurement mid-flight.
const SHADOW_QUOTE_DAYS: i64 = 30;

/// The six FusionEngine signals whose per-day fire counts we roll up. (overreaction_fade retired but
/// kept here so historical days that used it still summarize correctly.)
const SIGNALS: [&str; 6] = [
    "orderbook_momentum",
    "spike_divergence",
    "overreaction_fade",
    "theta_convergence",
    "yahoo_finance",
    "news_sentiment",
];

/// Row counts from one GC pass (journaled for observability).
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct GcStats {
    pub price_hours_rolled: u64,
    pub snapshots_deleted: u64,
    pub signal_days_rolled: u64,
    pub reports_deleted: u64,
    pub telemetry_deleted: u64,
    pub portfolio_snapshots_deleted: u64,
    pub shadow_quotes_deleted: u64,
}

/// Run one full GC pass: roll up, then prune. Non-fatal per step — a failure in one is logged and the
/// rest proceed (returns the partial stats).
pub async fn run_gc(pool: &PgPool) -> GcStats {
    let mut s = GcStats::default();

    match rollup_price_history(pool).await {
        Ok(n) => s.price_hours_rolled = n,
        Err(e) => tracing::warn!(error = %e, "gc: price_history rollup failed"),
    }
    match prune_orderbook_snapshots(pool).await {
        Ok(n) => s.snapshots_deleted = n,
        Err(e) => tracing::warn!(error = %e, "gc: orderbook_snapshots prune failed"),
    }
    match rollup_signal_daily(pool).await {
        Ok(n) => s.signal_days_rolled = n,
        Err(e) => tracing::warn!(error = %e, "gc: signal_daily rollup failed"),
    }
    match prune_decision_reports(pool).await {
        Ok(n) => s.reports_deleted = n,
        Err(e) => tracing::warn!(error = %e, "gc: decision_report prune failed"),
    }
    match prune_telemetry(pool).await {
        Ok(n) => s.telemetry_deleted = n,
        Err(e) => tracing::warn!(error = %e, "gc: telemetry prune failed"),
    }
    match prune_portfolio_snapshots(pool).await {
        Ok(n) => s.portfolio_snapshots_deleted = n,
        Err(e) => tracing::warn!(error = %e, "gc: portfolio snapshot prune failed"),
    }
    match prune_shadow_quotes(pool).await {
        Ok(n) => s.shadow_quotes_deleted = n,
        Err(e) => tracing::warn!(error = %e, "gc: shadow quote prune failed"),
    }
    tracing::info!(?s, "gc pass complete");
    s
}

/// Roll snapshots older than the raw window into hourly mids (idempotent). Must run BEFORE the prune.
async fn rollup_price_history(pool: &PgPool) -> Result<u64> {
    let q = format!(
        "INSERT INTO market_data.price_history (market_id, outcome, hour, mid)
         SELECT market_id, outcome, date_trunc('hour', fetched_at), avg(mid)
         FROM market_data.orderbook_snapshots
         WHERE fetched_at < now() - interval '{SNAPSHOT_RAW_HOURS} hours' AND mid IS NOT NULL
         GROUP BY market_id, outcome, date_trunc('hour', fetched_at)
         ON CONFLICT (market_id, outcome, hour) DO NOTHING"
    );
    Ok(sqlx::query(&q).execute(pool).await?.rows_affected())
}

/// Delete raw snapshots older than the raw window, keeping the latest per (market, outcome) — the
/// live working set (arb scanner / fetch_latest_book) — but ONLY up to `STALE_LATEST_CAP_DAYS`.
/// Batched.
async fn prune_orderbook_snapshots(pool: &PgPool) -> Result<u64> {
    // "old AND a newer snapshot exists for the same book" ⇒ not the latest ⇒ safe to drop. The EXISTS
    // uses idx_obs_market_outcome_fetched. The second arm is the hard cap: a "latest" row this stale
    // means the market permanently dropped out of the ingest universe (see STALE_LATEST_CAP_DAYS
    // doc) — prune it regardless of latest-status; price_history already has its signal.
    let q = format!(
        "DELETE FROM market_data.orderbook_snapshots
         WHERE id IN (
           SELECT s.id FROM market_data.orderbook_snapshots s
           WHERE s.fetched_at < now() - interval '{SNAPSHOT_RAW_HOURS} hours'
             AND (
               EXISTS (SELECT 1 FROM market_data.orderbook_snapshots s2
                       WHERE s2.market_id = s.market_id AND s2.outcome = s.outcome
                         AND s2.fetched_at > s.fetched_at)
               OR s.fetched_at < now() - interval '{STALE_LATEST_CAP_DAYS} days'
             )
           LIMIT {BATCH})"
    );
    delete_in_batches(pool, &q).await
}

/// Roll decision_reports older than the raw window into per-day per-signal fire counts (idempotent).
async fn rollup_signal_daily(pool: &PgPool) -> Result<u64> {
    let filters = SIGNALS
        .iter()
        .map(|s| format!("('{s}')"))
        .collect::<Vec<_>>()
        .join(",");
    let q = format!(
        "INSERT INTO journal.signal_daily (day, signal, reports, fired)
         SELECT e.created_at::date AS day, sig.signal, count(*) AS reports,
                count(*) FILTER (WHERE e.payload->'report'->'attribution'->sig.signal->>'score' ~ '[1-9]') AS fired
         FROM journal.events e
         CROSS JOIN (VALUES {filters}) AS sig(signal)
         WHERE e.event_type = 'decision_report'
           AND e.created_at < now() - interval '{REPORT_RAW_DAYS} days'
         GROUP BY 1, 2
         ON CONFLICT (day, signal) DO UPDATE SET reports = EXCLUDED.reports, fired = EXCLUDED.fired"
    );
    Ok(sqlx::query(&q).execute(pool).await?.rows_affected())
}

/// Delete raw decision_reports older than the raw window. Batched. (Active markets always have recent
/// reports, so the /board's latest-per-market lookup is unaffected; only long-stale reports go.)
///
/// EXCEPTION: reports for markets we actually traded DIRECTIONALLY are kept indefinitely.
///
/// Without it the Signal Scorecard's settled columns are structurally unable to populate, and on
/// 2026-08-13 they reached exactly that — **0 of 42 directional settlements still had reports**, so
/// every signal read "—". The arithmetic is unforgiving: directional is a deliberate control arm at
/// ~1 entry/day (`MAX_DAILY_ENTRIES=1`), reports expire after `REPORT_RAW_DAYS`, and crediting a
/// signal needs a market to BOTH settle AND still have its reports. That intersection trends to
/// empty, which silently killed the one instrument built to answer "is this signal any good?" — the
/// question the control arm exists to keep answerable.
///
/// The exemption is cheap and self-limiting: it covers only markets with an
/// `autonomous_paper_execution` fill, ~63 of them against ~100k decision reports, and it grows at the
/// control arm's 1/day. Arb-entered markets are deliberately NOT exempt — they are 90% of
/// settlements and read no signals, and keeping their reports is what made this column meaningless
/// before it was scoped (see the 2026-08-09 scorecard entry).
///
/// Both CTEs are materialised rather than left as correlated subqueries, and the difference is not
/// cosmetic. Measured on live data with `EXPLAIN ANALYZE`:
///
/// | form | plan | time |
/// |---|---|---|
/// | correlated `NOT EXISTS` over the fill set | nested loop, 14,296 rescans | **1,508 ms** |
/// | materialised exempt set, whole-market exemption | hash anti-join | 49 ms |
/// | materialised + 20-per-market ranking (this) | hash anti-join + window | **508 ms** |
///
/// The ranking costs ~10x the naive exemption and is still 3x cheaper than the correlated form. It
/// is worth paying because the naive version grows unboundedly (~1 MB/day of reports retained
/// forever), which is exactly how `market_data.markets` reached 6,787 rows before it got a retention
/// pass. GC runs `delete_in_batches` in a loop until a pass clears fewer than BATCH rows, so the
/// correlated form would have multiplied its cost across every batch of the first post-deploy pass.
///
/// `NOT EXISTS` rather than `NOT IN` deliberately: with `NOT IN`, a single NULL in the subquery makes
/// the whole predicate NULL and deletes nothing, and a report whose own `market_id` is NULL would
/// never match and would leak forever. `NOT EXISTS` has neither failure mode.
async fn prune_decision_reports(pool: &PgPool) -> Result<u64> {
    let q = format!(
        "DELETE FROM journal.events
         WHERE id IN (
           WITH directional AS MATERIALIZED (
             SELECT DISTINCT payload->>'market_id' AS market_id
             FROM journal.events
             WHERE event_type = 'autonomous_paper_execution'
               AND payload->>'action' = 'filled'
               AND payload->>'market_id' IS NOT NULL),
           keep AS MATERIALIZED (
             SELECT id FROM (
               SELECT r.id, row_number() OVER (
                        PARTITION BY r.payload->>'market_id' ORDER BY r.created_at DESC) AS rn
               FROM journal.events r
               JOIN directional d ON d.market_id = r.payload->>'market_id'
               WHERE r.event_type = 'decision_report') s
             WHERE s.rn <= {REPORTS_KEPT_PER_DIRECTIONAL_MARKET})
           SELECT r.id FROM journal.events r
           WHERE r.event_type = 'decision_report'
             AND r.created_at < now() - interval '{REPORT_RAW_DAYS} days'
             AND NOT EXISTS (SELECT 1 FROM keep k WHERE k.id = r.id)
           LIMIT {BATCH})"
    );
    delete_in_batches(pool, &q).await
}

/// Delete pure per-cycle telemetry (llm_health / real_account_balance) older than the window — routine
/// "ok"/balance rows with no lasting value. Alert events (llm_health_alert, …) are a different type and
/// untouched. Batched.
async fn prune_telemetry(pool: &PgPool) -> Result<u64> {
    let q = format!(
        "DELETE FROM journal.events
         WHERE id IN (
           SELECT id FROM journal.events
           WHERE event_type IN ('llm_health', 'real_account_balance')
             AND created_at < now() - interval '{TELEMETRY_DAYS} days'
           LIMIT {BATCH})"
    );
    delete_in_batches(pool, &q).await
}

/// Downsample the portfolio equity curve: beyond the raw window, thin the 5-min `mark_to_market`
/// snapshots to one per hour (enough granularity for the wide 1M/1Y/ALL P&L charts). Event-marker
/// snapshots (fills / settlements / resets) are ALWAYS kept — they mark real P&L step-changes — as is
/// everything within the raw window. Batched.
async fn prune_portfolio_snapshots(pool: &PgPool) -> Result<u64> {
    let q = format!(
        "DELETE FROM paper_trading.virtual_portfolio_snapshots
         WHERE id IN (
           SELECT id FROM paper_trading.virtual_portfolio_snapshots v
           WHERE v.as_of < now() - interval '{PORTFOLIO_RAW_DAYS} days'
             AND v.snapshot_reason = 'mark_to_market'
             AND v.id NOT IN (
               SELECT DISTINCT ON (date_trunc('hour', as_of)) id
               FROM paper_trading.virtual_portfolio_snapshots
               WHERE as_of < now() - interval '{PORTFOLIO_RAW_DAYS} days'
                 AND snapshot_reason = 'mark_to_market'
               ORDER BY date_trunc('hour', as_of), as_of DESC)
           LIMIT {BATCH})"
    );
    delete_in_batches(pool, &q).await
}

/// Prune FINISHED shadow maker quotes past the retention window (P5 increment 3b).
///
/// "Finished" is doing real work here. A quote is only eligible once it is cancelled, or filled AND
/// marked at its horizon — the two states where it has already yielded its number. An open quote is
/// live state, and a filled quote still inside its measurement horizon would be deleted before
/// producing the adverse-selection figure the whole increment exists for. Age alone is not a safe
/// predicate: a quote that never fills stays open indefinitely and would otherwise be pruned out
/// from under the duty-cycle measurement, which is precisely the case where a long-lived quote is
/// the most informative one we have.
async fn prune_shadow_quotes(pool: &PgPool) -> Result<u64> {
    let q = format!(
        "DELETE FROM paper_trading.shadow_quotes
         WHERE id IN (
           SELECT id FROM paper_trading.shadow_quotes
           WHERE placed_at < now() - interval '{SHADOW_QUOTE_DAYS} days'
             AND (status = 'cancelled'
                  OR (status = 'filled' AND mid_at_horizon IS NOT NULL))
           LIMIT {BATCH})"
    );
    delete_in_batches(pool, &q).await
}

/// Run a `DELETE … LIMIT BATCH` statement repeatedly until a pass deletes fewer than BATCH rows (the
/// backlog is drained). Bounds per-statement lock/WAL; the first post-deploy run may loop several times.
async fn delete_in_batches(pool: &PgPool, query: &str) -> Result<u64> {
    let mut total = 0u64;
    loop {
        let n = sqlx::query(query).execute(pool).await?.rows_affected();
        total += n;
        if (n as i64) < BATCH {
            break;
        }
    }
    Ok(total)
}
