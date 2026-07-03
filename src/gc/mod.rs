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
/// Keep raw decision_reports this recent (24h scorecard, 7d health, ~14d of harness backtest attribution).
/// Older are rolled to daily `signal_daily` then deleted. Was 30d; lowered to 14d on 2026-07-02 to cap the
/// events table (~5.2k reports/day × ~1.7KB was trending to ~270MB @30d) — see wiki/roadmap retention note.
const REPORT_RAW_DAYS: i64 = 14;
/// Keep pure per-cycle telemetry (llm_health / real_account_balance) this recent; older is dropped.
const TELEMETRY_DAYS: i64 = 14;
/// Keep full-granularity (5-min) portfolio equity snapshots this recent (1D/1W chart); older
/// mark-to-market snapshots are thinned to 1/hour (fills/settlements/resets are always kept).
const PORTFOLIO_RAW_DAYS: i64 = 7;
/// Rows deleted per batch (bounds lock time / WAL per statement).
const BATCH: i64 = 10_000;

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

/// Delete raw snapshots older than the raw window, ALWAYS keeping the latest per (market, outcome) —
/// the live working set (arb scanner / fetch_latest_book). Batched.
async fn prune_orderbook_snapshots(pool: &PgPool) -> Result<u64> {
    // "old AND a newer snapshot exists for the same book" ⇒ not the latest ⇒ safe to drop. The EXISTS
    // uses idx_obs_market_outcome_fetched. (Closed markets keep their last book: no newer exists.)
    let q = format!(
        "DELETE FROM market_data.orderbook_snapshots
         WHERE id IN (
           SELECT s.id FROM market_data.orderbook_snapshots s
           WHERE s.fetched_at < now() - interval '{SNAPSHOT_RAW_HOURS} hours'
             AND EXISTS (SELECT 1 FROM market_data.orderbook_snapshots s2
                         WHERE s2.market_id = s.market_id AND s2.outcome = s.outcome
                           AND s2.fetched_at > s.fetched_at)
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
async fn prune_decision_reports(pool: &PgPool) -> Result<u64> {
    let q = format!(
        "DELETE FROM journal.events
         WHERE id IN (
           SELECT id FROM journal.events
           WHERE event_type = 'decision_report'
             AND created_at < now() - interval '{REPORT_RAW_DAYS} days'
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
