//! polytrader — Autonomous Polymarket agent (paper trading first).
//!
//! Long-running service with minimal dashboard, paper trading engine,
//! journal, and integration point for Hermes self-improvement agent.
//!
//! PHASE 0 CORE: fully wired DB + live public ingester + realistic paper engine + journal + axum server.

use anyhow::Result;
use std::sync::Arc;
use tracing::{info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

mod config;
mod db;
mod ingester;
mod journal;
mod paper;
mod server;

// Phase 3.2 smallest skeleton (wiki-first per plan/AGENTS; after all docs/decisions/log prepend).
// mod strategy declares the new artifact (FusionEngine + processors using exact existing patterns: rust_decimal, anyhow, tracing, journal attribution hooks, heavy risk comments, paper-only).
// No behavior change to any existing path; unused in this increment (full wiring in follow-ups). #[allow] inside strategy/mod.rs for clean clippy.
mod backtest; // offline replay harness (read-only; `polytrader backtest` subcommand) — reuses risk+strategy pure cores
mod clob; // gated real/authenticated CLOB client (foundation for future order placement using derived L2 creds)
mod exits; // autonomous position exits (TP/SL/time-stop/signal-flip): bounded round-trips instead of hold-to-resolution
mod gc; // daily DB garbage collection: roll fat raw data into compact summaries, prune beyond retention windows
mod risk;
mod rotation; // directional market rotation: keeps the directional-eligible universe fresh (short-dated, liquid, non-sports)
mod strategy;

use crate::config::Config;
use crate::db::create_pool;
use crate::ingester::{ingest_tick, ClobPublicClient, GammaClient};
use crate::journal::JournalWriter;
use crate::paper::PaperTradingEngine;
use crate::server::{start_server, AppState};
// 5-min DR cadence (additive; see wiki/strategies/goals-and-operational-cadence.md + strategy mod)
use crate::risk::RiskManager;
use crate::strategy::{ArbitrageScanner, DecisionReport, FeeContext, FusionEngine};
use rust_decimal_macros::dec;

#[tokio::main]
async fn main() -> Result<()> {
    // Very early fallback logging (in case tracing doesn't flush before fast exit)
    eprintln!("=== POLYTRADER MAIN ENTERED (pre-tracing) ===");

    // Structured logging (json for easy parsing by Hermes later). The `backtest` subcommand prints its
    // own plain report to stdout and replays tens of thousands of decisions, so run it quiet (errors
    // only) — otherwise the per-trade gate logs + the bootstrap dump bury the report.
    let is_backtest = std::env::args().any(|a| a == "backtest");
    let env_filter = if is_backtest {
        EnvFilter::new("error,polytrader=error,sqlx=error")
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::new("info,polytrader=debug,sqlx=warn,axum=info,tower_http=debug")
        })
    };

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().json())
        .with(env_filter)
        .init();

    info!(
        version = env!("CARGO_PKG_VERSION"),
        "🚀 polytrader starting — PAPER MODE ONLY (safety gate active per AGENTS.md)"
    );

    eprintln!("=== TRACING INITIALIZED ===");

    // === CONFIG (with dotenv + hard paper gate) ===
    eprintln!("=== ABOUT TO LOAD CONFIG ===");
    let cfg = Config::load();
    eprintln!("=== CONFIG LOADED SUCCESSFULLY ===");
    info!(
        mode = %cfg.mode,
        ingest_interval = cfg.ingest_interval_secs,
        bootstrap = ?cfg.bootstrap_market_list(),
        arb_only = ?cfg.arb_only_market_list(),
        auth_enabled = cfg.auth_enabled(),
        "config loaded and validated (paper-only)"
    );
    assert_eq!(cfg.mode.to_lowercase(), "paper"); // belt + suspenders

    // === L2 Polymarket Auth (auto-derive on startup if key present) ===
    // Note: we treat a non-empty POLYMARKET_PRIVATE_KEY_FILE the same as a direct key
    // (K8s secret injection path). The actual reading + derivation logic lives in
    // try_auto_derive_l2_on_startup (which also emits useful DEBUG lines).
    if !std::env::var("POLYMARKET_PRIVATE_KEY")
        .map(|s| s.is_empty())
        .unwrap_or(true)
        || !std::env::var("PRIVATE_KEY")
            .map(|s| s.is_empty())
            .unwrap_or(true)
        || !std::env::var("POLYMARKET_PRIVATE_KEY_FILE")
            .map(|s| s.is_empty())
            .unwrap_or(true)
    {
        info!("POLYMARKET_PRIVATE_KEY detected — attempting native L2 credential derivation on startup...");
        // We call the same logic the UI button uses.
        // Errors are logged but do not crash the server (paper mode safety).
        match crate::server::try_auto_derive_l2_on_startup().await {
            Ok(Some(masked)) => {
                info!(masked_api_key = %masked, "L2 credentials successfully derived on startup using server key");
            }
            Ok(None) => {
                info!("No L2 credentials derived (key present but derivation returned empty)");
            }
            Err(e) => {
                tracing::error!("L2 auto-derive on startup failed: {}. The /l2/* endpoints will still work if you trigger derivation manually.", e);
            }
        }
    } else {
        info!(
            "No POLYMARKET_PRIVATE_KEY found — L2 will stay in 'not connected' state until derived"
        );
    }

    // === DB + MIGRATIONS (embedded sqlx) ===
    let pool = create_pool(&cfg.database_url).await?;
    info!("Postgres pool ready (migrations applied, paper_trading + journal schemas present)");

    // Backtest subcommand: `polytrader backtest [--min-net-edge X] [--weights n=v,..] [--since RFC3339]`.
    // One-shot, READ-ONLY — runs the offline replay harness against the live journal and exits before
    // any server, ingester, or executor loop starts. Reuses the pure Phase-0 cores (no duplicated
    // decision logic). Safe to run against the live DB (it only reads).
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "backtest") {
        backtest::run(&pool, &args).await?;
        return Ok(());
    }

    // Seed initial paper portfolio snapshot if none exists (uses config initial)
    seed_initial_portfolio_if_needed(&pool, cfg.initial_paper_usdc).await?;

    // === JOURNAL + ENGINE (shared) ===
    let journal = Arc::new(JournalWriter::new(pool.clone()));

    // P5 live orderbook store. Created unconditionally so its readers have one type to talk to:
    // without the `clob-ws` feature, or with POLYTRADER_ENABLE_CLOB_WS unset, nothing ever fills
    // it and every read falls back to the polled snapshot — the same path a read takes when a
    // live book is desynced. No cfg reaches the strategy or paper layers.
    let live_books = crate::ingester::clob_ws::LiveBookStore::new();

    // Fees are per-market (Polymarket's real taker model, resolved per fill from the stored
    // Gamma rate) — the engine takes no flat rate.
    // The engine shares the scanner's book so both read the same prices: negRisk legs are LIMIT
    // orders at the price the scanner saw, so a scanner on live prices and a matcher on snapshots
    // would simply never fill (see PaperTradingEngine::with_live_books).
    let engine = Arc::new(
        PaperTradingEngine::new(pool.clone(), journal.clone()).with_live_books(live_books.clone()),
    );
    info!("PaperTradingEngine + JournalWriter initialized (Decimal-only math, full journaling)");

    // === INGESTER CLIENTS (public only) ===
    let gamma = GammaClient::new();
    let clob = ClobPublicClient::new();
    let bootstrap_markets = cfg.bootstrap_market_list();

    // === SPAWN INGESTION LOOP (startup + periodic) ===
    {
        let gamma = gamma.clone(); // clients are cheap (http inside)
        let clob = clob.clone();
        let pool = pool.clone();
        let bootstrap = bootstrap_markets.clone();
        let interval = std::time::Duration::from_secs(cfg.ingest_interval_secs.max(60));

        tokio::spawn(async move {
            // Initial tick immediately (so dashboard + engine have data fast)
            if let Err(e) = ingest_tick(&gamma, &clob, &pool, &bootstrap).await {
                warn!(error = %e, "initial ingestion tick failed");
            }

            loop {
                tokio::time::sleep(interval).await;
                if let Err(e) = ingest_tick(&gamma, &clob, &pool, &bootstrap).await {
                    warn!(error = %e, "periodic ingestion tick failed (will retry)");
                }
            }
        });
        // TODO (post-Phase 0 / when submit exercised): use tokio::select! + cancellation token or JoinHandle::abort
        // for clean shutdown of the ingestion task (current fire-and-forget is acceptable for bootstrap).
    }

    // === P5: LIVE CLOB ORDERBOOK FEED (WebSocket) — SHADOW ONLY ===
    //
    // Doubly gated: compiled only with `--features clob-ws`, and started only when
    // POLYTRADER_ENABLE_CLOB_WS names an implemented mode. Nothing downstream reads these books
    // yet — this tier exists to establish that a delta-maintained book is *correct* before any
    // execution path depends on one. See src/ingester/clob_ws.rs for why the proof is a REST audit
    // rather than a comparison against the 5-minute snapshot table.
    #[cfg(feature = "clob-ws")]
    {
        use crate::ingester::clob_ws;
        let mode = std::env::var("POLYTRADER_ENABLE_CLOB_WS")
            .ok()
            .as_deref()
            .and_then(clob_ws::WsMode::from_env_value);

        match mode {
            None => info!("P5 CLOB WS feed compiled in but disabled (set POLYTRADER_ENABLE_CLOB_WS=shadow to run it read-only)"),
            Some(clob_ws::WsMode::Shadow) => {
                let pool = pool.clone();
                let clob = clob.clone();
                let store = live_books.clone();
                // Bounded so a runaway universe cannot open unlimited sockets against a public
                // endpoint; the cap is on tokens, and shards derive from it.
                let max_assets = std::env::var("POLYTRADER_CLOB_WS_MAX_ASSETS")
                    .ok()
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(500);
                let audit_sample = std::env::var("POLYTRADER_CLOB_WS_AUDIT_SAMPLE")
                    .ok()
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(8);

                info!(
                    max_assets,
                    audit_sample,
                    "P5 CLOB WS feed enabled in SHADOW mode — maintains live books and audits them \
                     against REST; no strategy or executor reads them (first subscribe in ~90s, \
                     once the opening ingest tick has defined a universe)"
                );

                tokio::spawn(async move {
                    // Wait for the first ingest tick to define a universe; subscribing to nothing
                    // and retrying is cheaper than guessing at a token list.
                    tokio::time::sleep(std::time::Duration::from_secs(90)).await;

                    let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
                    let mut subscribed: Vec<String> = Vec::new();

                    loop {
                        // The universe is whatever the poller is currently keeping fresh, which is
                        // the same set any future execution path would need books for.
                        let universe: Vec<String> = sqlx::query_scalar(
                            // Priority order matters more than it looks. The cap is a hard limit,
                            // so whatever sorts last is simply not subscribed — and ordering by
                            // token_id alone made that an arbitrary lexicographic slice of a
                            // 600-token universe. Measured 2026-08-01: 86 of 301 negRisk member
                            // books were snapshot-priced for no reason other than where their
                            // token id happened to fall.
                            //
                            // Two priority tiers, in order of what the budget is FOR:
                            //  1. negRisk No books — the arb scanner's own reads, and arb is the
                            //     strategy that actually earns.
                            //  2. Reward-paying markets' books — the maker-rewards shadow scan
                            //     estimates our share from qualifying DEPTH near the mid, which
                            //     moves far faster than the 30-minute snapshot window it otherwise
                            //     falls back to. Added 2026-08-02 after that scan reported
                            //     priced_from_live: 0 — tier 1 is No-side only, and reward markets
                            //     are priced off their Yes book, so none of them were subscribed.
                            // token_id remains the final tiebreak purely to keep the set
                            // deterministic for the resubscribe drift check.
                            "SELECT s.token_id
                               FROM (
                                    SELECT DISTINCT ON (token_id) token_id, market_id, outcome
                                      FROM market_data.orderbook_snapshots
                                     WHERE fetched_at > now() - interval '20 minutes'
                                     ORDER BY token_id, fetched_at DESC
                               ) s
                               LEFT JOIN market_data.markets m ON m.gamma_id = s.market_id
                              ORDER BY (COALESCE(m.neg_risk, false)
                                        AND s.outcome = 'No'
                                        AND COALESCE(m.active, false)
                                        AND NOT COALESCE(m.closed, true)) DESC,
                                       (m.rewards_daily_rate IS NOT NULL
                                        AND COALESCE(m.active, false)
                                        AND NOT COALESCE(m.closed, true)) DESC,
                                       s.token_id
                              LIMIT $1",
                        )
                        .bind(max_assets as i64)
                        .fetch_all(&pool)
                        .await
                        .unwrap_or_default();

                        // Resubscribe only on MATERIAL drift, never on any difference. The
                        // discovery pool rotates a handful of tokens every tick, so an
                        // exact-set comparison would tear down and rebuild every socket every
                        // five minutes — losing all delta continuity and hammering a public
                        // endpoint, to chase a few percent of coverage. The cost of drifting is
                        // small and bounded (a few dead subscriptions, a few unwatched new
                        // tokens); the cost of churning is the feed never settling at all.
                        let cur: std::collections::HashSet<&String> = subscribed.iter().collect();
                        let next: std::collections::HashSet<&String> = universe.iter().collect();
                        let added = next.difference(&cur).count();
                        let dropped = cur.difference(&next).count();
                        let drift_floor = (subscribed.len() / 10).max(20);

                        if !universe.is_empty()
                            && (subscribed.is_empty() || added.max(dropped) >= drift_floor)
                        {
                            // Resubscribing means new sockets, so tear the old ones down first —
                            // an aborted shard is the only way to stop its reconnect loop.
                            for h in handles.drain(..) {
                                h.abort();
                            }
                            let shards = clob_ws::spawn_feed(store.clone(), universe.clone());
                            info!(
                                tokens = universe.len(),
                                shards = shards.len(),
                                added,
                                dropped,
                                "P5 CLOB WS feed (shadow) subscribed"
                            );
                            handles = shards;
                            subscribed = universe;
                        }

                        // Audit + health on a 5-minute beat, matching the poll cadence so the two
                        // tiers' numbers are directly comparable in the log.
                        for _ in 0..5 {
                            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                            let h = store.health();
                            let a = clob_ws::audit_against_rest(&store, &clob, audit_sample).await;
                            info!(
                                tracked = h.tracked, fresh = h.fresh, desynced = h.desynced, stale = h.stale,
                                frames = h.frames, books = h.book_events, changes = h.change_events,
                                desync_events = h.desync_events, reconnects = h.reconnects,
                                orphan_changes = h.orphan_changes,
                                audit_sampled = a.sampled, audit_agreed = a.agreed,
                                audit_disagreed = a.disagreed, audit_no_rest_book = a.no_rest_book,
                                worst_ask_delta_bps = ?a.worst_ask_delta_bps,
                                "P5 CLOB WS shadow diagnostics"
                            );
                        }
                    }
                });
            }
        }
    }

    // Daily DB garbage collection: roll fat raw data (orderbook books, decision_report payloads) into
    // compact summaries + prune beyond the retention windows so the DB stays bounded (~60MB/day growth).
    // Runs ~2 min after boot (drains the initial backlog) then every 24h. Journals a `gc_run` event.
    {
        let pool = pool.clone();
        let journal = journal.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(120)).await;
            loop {
                let stats = gc::run_gc(&pool).await;
                let _ = journal
                    .record_journal_event(
                        "gc_run",
                        "polytrader_gc",
                        "info",
                        serde_json::to_value(&stats).unwrap_or(serde_json::json!({})),
                    )
                    .await;
                tokio::time::sleep(std::time::Duration::from_secs(24 * 60 * 60)).await;
            }
        });
        info!("Daily DB garbage-collection task spawned (rollup + prune; retention: 48h books / 14d reports)");
    }

    // Directional market rotation: keeps the directional-eligible universe fresh by promoting
    // short-dated, liquid, non-sports markets and demoting resolved ones (the hand-curated bootstrap
    // list decays as its markets resolve — see wiki/roadmap). Runs ~3 min after boot (first ingest
    // tick has landed) then every 6h. Journals a `directional_rotation` event. No-op when
    // POLYTRADER_ROTATION_LIMIT is unset/0 (demotes only, so a drained set stays clean).
    {
        let pool = pool.clone();
        let journal = journal.clone();
        let gamma = GammaClient::new();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(180)).await;
            loop {
                match rotation::run_rotation(&pool, &gamma, is_arb_only_market).await {
                    Ok(stats) => {
                        tracing::info!(?stats, "directional rotation pass complete");
                        let _ = journal
                            .record_journal_event(
                                "directional_rotation",
                                "polytrader_rotation",
                                "info",
                                serde_json::to_value(&stats).unwrap_or(serde_json::json!({})),
                            )
                            .await;
                    }
                    Err(e) => tracing::warn!(error = %e, "directional rotation pass failed"),
                }
                tokio::time::sleep(std::time::Duration::from_secs(6 * 60 * 60)).await;
            }
        });
        info!("Directional market-rotation task spawned (6h cadence; promote short-dated liquid non-sports, demote resolved)");
    }

    // Recurring-ladder detection (see wiki/roadmap "Recurring-ladder detection", 2026-07-17 review
    // #4): weekly NegRisk families (e.g. the Musk tweet-count ladder) get a new event/slug set days
    // before the prior period resolves, but only enter our tracked universe once volume ranks them
    // in — missing the early-book-inefficiency window. This predicts + watchlists the next period's
    // slugs (market_data.ladder_watchlist, UNIONed into ingest_tick's must-track query) ahead of
    // that. Runs ~4 min after boot then every 6h (offset from rotation so they don't contend for the
    // same tick). Journals a `ladder_watchlist_update` event. Lookahead window via
    // POLYTRADER_LADDER_LOOKAHEAD_DAYS (default 3 — only extends families actually nearing
    // resolution, not the whole tracked universe every pass).
    {
        let pool = pool.clone();
        let journal = journal.clone();
        let lookahead_days = std::env::var("POLYTRADER_LADDER_LOOKAHEAD_DAYS")
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(3);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(240)).await;
            loop {
                match rotation::ladder::detect_and_extend_ladders(&pool, lookahead_days).await {
                    Ok(stats) => {
                        tracing::info!(?stats, "ladder detection pass complete");
                        let _ = journal
                            .record_journal_event(
                                "ladder_watchlist_update",
                                "polytrader_ladder",
                                "info",
                                serde_json::to_value(&stats).unwrap_or(serde_json::json!({})),
                            )
                            .await;
                    }
                    Err(e) => tracing::warn!(error = %e, "ladder detection pass failed"),
                }
                if let Err(e) = rotation::ladder::prune_stale_ladder_watchlist(&pool, 21).await {
                    tracing::warn!(error = %e, "ladder watchlist prune failed (non-fatal)");
                }
                tokio::time::sleep(std::time::Duration::from_secs(6 * 60 * 60)).await;
            }
        });
        info!("Recurring-ladder detection task spawned (6h cadence; predicts + watchlists next-period NegRisk ladder slugs)");
    }
    info!(
        "Background ingestion task spawned (Gamma + CLOB public, {}s interval, rate-limited)",
        cfg.ingest_interval_secs
    );

    // === 5-MIN DECISION REPORT CADENCE GENERATOR (wiki-first per AGENTS; additive spawn) ===
    // Per wiki/strategies/goals-and-operational-cadence.md ("Every ~5 Minutes — 'Trader' / Opportunity Layer",
    // "Generate **Decision Report** ... logged to journal", "PRIMARY signal for deliberate 5-min tier",
    // "Extend do_reflection...", "no new DB tables ... reuse ... jsonb for decision reports",
    // "approval queue orthogonal"), docs/project-plan (post-DR-stub follow-up), log top (DR stub "Ready for next"
    // + this wiring makes actionable), decisions/real-order-approval-flow (DR generator section), strategy/mod.rs
    // (DecisionReport + fuse_net "PRIMARY signal for deliberate 5-min tier (see fees wiki + 4-6% min net in goals)"
    // + "decision_report_summary" + "ready for 5-min generator + jsonb journal"), hermes (now consumes real counts).
    // Smallest: piggy existing journal + pool + strategy skeleton (already declared); queries market_data (fed by
    // ingester); calls fuse_net with conservative FeeContext + minimal snapshot (processors are stubs but produce
    // net edge + attribution); journals 'decision_report' reuse events (via extended writer, exact server pattern);
    // no auto paper submit (goals: "Optional in this phase" behind flag; we log only); no real path.
    // RISK (AGENTS.md non-negotiable, heavily commented): net (not gross) is mandatory for $150 (fees destroy
    // small edges per fees-tax wiki + goals 4-6% min net); this is the "PRIMARY signal" gate; journals only for
    // Hermes closed-loop (P&L attr, per-signal, DR vs approvals later); paper-only always (real_orders_enabled
    // false, no unlocks here); Decimal exclusively; all context journaled before any future action. Reuses
    // ingest patterns for robustness. See also server build_strategy_paper_candidates (on-demand fuse_net usage).
    {
        let pool = pool.clone();
        let journal = journal.clone();
        let engine = engine.clone();
        let live_books = live_books.clone();
        let interval = std::time::Duration::from_secs(300); // 5min per goals cadence
        tokio::spawn(async move {
            // initial immediately (so first reflection sees data)
            if let Err(e) = produce_5min_decision_report(&pool, &journal, &engine).await {
                warn!(error = %e, "initial 5min DR generation failed (will retry)");
            }
            // Initial arb scan immediately so Hermes has data on first reflection.
            if let Err(e) = produce_arb_scan_journal(&pool, &journal, &engine, &live_books).await {
                warn!(error = %e, "initial arb scan failed (will retry)");
            }
            // Real PUSD balance of the proxy (read-only; for the /trades UI + funded gate).
            fetch_and_journal_real_balance(&journal).await;
            // Settle any positions on resolved markets to realized P&L (ground truth for "proven").
            if let Err(e) = settle_resolved_positions(&pool, &journal).await {
                warn!(error = %e, "initial settlement pass failed (will retry)");
            }
            // Mark-to-market snapshot so the /trades P&L chart has a fresh point each cycle.
            if let Err(e) = write_mark_to_market_snapshot(&pool).await {
                warn!(error = %e, "initial mark-to-market snapshot failed (will retry)");
            }
            loop {
                tokio::time::sleep(interval).await;
                if let Err(e) = produce_5min_decision_report(&pool, &journal, &engine).await {
                    warn!(error = %e, "periodic 5min DR generation failed (will retry)");
                }
                if let Err(e) =
                    produce_arb_scan_journal(&pool, &journal, &engine, &live_books).await
                {
                    warn!(error = %e, "periodic arb scan failed (will retry)");
                }
                fetch_and_journal_real_balance(&journal).await;
                if let Err(e) = settle_resolved_positions(&pool, &journal).await {
                    warn!(error = %e, "periodic settlement pass failed (will retry)");
                }
                // Autonomous exits AFTER the DR pass (signal-flip reads this cycle's fresh report)
                // and BEFORE mark-to-market (the chart point reflects the post-exit portfolio).
                exits::evaluate_exits(&pool, &engine, &journal).await;
                if let Err(e) = write_mark_to_market_snapshot(&pool).await {
                    warn!(error = %e, "periodic mark-to-market snapshot failed (will retry)");
                }
            }
        });
    }
    info!("Background 5-min Decision Report cadence generator spawned (paper-only; journals 'decision_report' events with net_edge_after_fees PRIMARY; Hermes will consume for self-imp)");
    // TODO (post skeleton / when 5min DR fuller per goals "Ranked list..."): use tokio::select! + cancellation token or JoinHandle::abort for clean shutdown of the 5min DR task (mirrors ingest spawn TODO at 149; prevents potential in-flight 'decision_report' loss on SIGTERM per AGENTS observability; low for current limited generator).

    // === MINIMAL DASHBOARD SERVER (axum on 0.0.0.0:8080 for k8s port-forward) ===
    let server_state = AppState {
        pool: pool.clone(),
        subpath_prefix: cfg.normalized_subpath_prefix(),
    };

    // Prominent safety banner in logs (and HTML)
    info!("==================================================================");
    info!("PAPER MODE ONLY — REAL TRADING DISABLED");
    info!("All activity is simulated against live public Gamma + CLOB data.");
    info!("Dashboard: http://localhost:8080 (or k8s port-forward 8080:80)");
    info!("Endpoints: /health | /markets | /paper/portfolio | /");
    info!("==================================================================");

    // Proper graceful shutdown on SIGTERM/SIGINT (robust for k8s/docker-desktop).
    // Replaces the previous pending() which had "early firing" issues in this env.
    // The server will now stay up until k8s sends SIGTERM on rollout/delete.
    let shutdown = shutdown_signal();

    // Run the server directly in main (standard axum + graceful shutdown pattern).
    // The spawned ingestion task will be dropped on shutdown (acceptable for Phase 0;
    // full cancellation token can be added later).
    info!("Starting server (direct await with graceful shutdown)...");
    eprintln!("=== ABOUT TO CALL start_server (this should block until SIGTERM) ===");
    start_server(server_state, shutdown).await?;
    eprintln!("=== start_server RETURNED after graceful shutdown ===");

    info!("polytrader shutdown complete cleanly.");
    info!("polytrader exiting. All paper activity was journaled.");
    Ok(())
}

/// Seed a starting virtual portfolio snapshot on first boot (if table empty).
/// Uses the configured initial paper USDC. Idempotent.
async fn seed_initial_portfolio_if_needed(pool: &sqlx::PgPool, initial_usdc: u64) -> Result<()> {
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM paper_trading.virtual_portfolio_snapshots")
            .fetch_one(pool)
            .await
            .unwrap_or(0);

    if count == 0 {
        let usdc = rust_decimal::Decimal::from(initial_usdc);
        sqlx::query(
            r#"INSERT INTO paper_trading.virtual_portfolio_snapshots
               (virtual_usdc, total_locked, unrealized_pnl, realized_pnl, snapshot_reason, positions)
               VALUES ($1, 0, 0, 0, 'startup', '[]'::jsonb)"#,
        )
        .bind(usdc)
        .execute(pool)
        .await?;
        info!(initial_usdc, "seeded initial virtual portfolio snapshot");
    }
    Ok(())
}

/// Live mark-to-market unrealized P&L over currently open positions: Σ shares·(current_mid −
/// avg_entry), current_mid from the latest cached market mids. Shared by the periodic
/// mark_to_market writer and the settlement writer — settlement calls this AFTER deleting the
/// just-settled positions so a market's own stale mark isn't double-counted alongside its new
/// realized delta (2026-07-20 fix: the settlement writer used to carry forward the PRIOR snapshot's
/// unrealized_pnl verbatim, which still included the settling position's last mark for one cycle —
/// double-counting it against the fresh realized delta and producing a one-tick spike-and-snap-back
/// on the P&L chart, self-correcting at the next mark_to_market tick ~5min later).
async fn compute_live_unrealized_pnl(pool: &sqlx::PgPool) -> rust_decimal::Decimal {
    sqlx::query_scalar::<_, rust_decimal::Decimal>(
        "SELECT COALESCE(SUM(
             p.shares * (
                 CASE WHEN p.outcome = 'Yes' THEN m.last_mid_yes ELSE m.last_mid_no END
                 - p.avg_entry_price
             )
         ), 0)
         FROM paper_trading.paper_positions p
         JOIN market_data.markets m ON m.gamma_id = p.market_id
         WHERE p.shares > 0
           AND (CASE WHEN p.outcome = 'Yes' THEN m.last_mid_yes ELSE m.last_mid_no END) IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(dec!(0))
}

/// Write a mark-to-market portfolio snapshot: live unrealized P&L over open positions, realized
/// carried forward. This gives the /trades P&L chart a fresh, truthful data point each decision
/// cycle (the per-fill snapshots stored unrealized_pnl = 0). Paper-only; no wallet or CLOB call.
async fn write_mark_to_market_snapshot(pool: &sqlx::PgPool) -> Result<()> {
    let unrealized = compute_live_unrealized_pnl(pool).await;

    let realized: rust_decimal::Decimal = sqlx::query_scalar(
        "SELECT realized_pnl FROM paper_trading.virtual_portfolio_snapshots ORDER BY as_of DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?
    .unwrap_or(dec!(0));
    let locked: rust_decimal::Decimal = sqlx::query_scalar(
        "SELECT COALESCE(SUM(collateral_locked), 0) FROM paper_trading.paper_positions WHERE shares > 0",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(dec!(0));
    // Fees SINCE THE LAST PAPER RESET only. `POST /paper/reset` re-baselines cash to the $10k seed but
    // PRESERVES fills for audit — so counting lifetime fees here would permanently re-subtract pre-reset
    // fees from the fresh seed, silently clawing the reset back toward the pre-reset equity (the bug that
    // made this snapshot recompute $10,000 → $9,953.20 right after the 2026-07-03 reset). Reset-boundary
    // filter mirrors the settlements fix; `realized` is already reset-aware (reads the latest snapshot).
    let fees: rust_decimal::Decimal = sqlx::query_scalar(
        "SELECT COALESCE(SUM(fee), 0) FROM paper_trading.paper_fills
         WHERE created_at >= COALESCE(
           (SELECT max(as_of) FROM paper_trading.virtual_portfolio_snapshots
            WHERE snapshot_reason = 'manual_paper_reset'), '-infinity'::timestamptz)",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(dec!(0));
    let usdc = (dec!(10000) - locked - fees + realized).max(dec!(0));

    sqlx::query(
        "INSERT INTO paper_trading.virtual_portfolio_snapshots
         (as_of, virtual_usdc, total_locked, unrealized_pnl, realized_pnl, snapshot_reason, positions)
         VALUES (now(), $1, $2, $3, $4, 'mark_to_market', '[]'::jsonb)",
    )
    .bind(usdc)
    .bind(locked)
    .bind(unrealized.round_dp(2))
    .bind(realized)
    .execute(pool)
    .await?;
    Ok(())
}

/// Create a future that resolves on SIGTERM or SIGINT.
/// This provides reliable graceful shutdown in k8s (replaces the previous pending() that had early-firing issues in this env).
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");

    tokio::select! {
        _ = sigterm.recv() => {
            info!("Received SIGTERM, initiating graceful shutdown");
        }
        _ = sigint.recv() => {
            info!("Received SIGINT, initiating graceful shutdown");
        }
    }
}

/// Read the latest virtual_usdc from the portfolio snapshot (used for Kelly sizing in DR).
async fn current_portfolio_usdc(pool: &sqlx::PgPool) -> rust_decimal::Decimal {
    sqlx::query_scalar::<_, rust_decimal::Decimal>(
        "SELECT virtual_usdc FROM paper_trading.virtual_portfolio_snapshots ORDER BY as_of DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .unwrap_or(dec!(150))
}

/// Minimal 5-min DR producer (called from dedicated spawn; reuses existing patterns).
/// Produces DecisionReport via fuse_net (net_edge_after_fees PRIMARY), journals 'decision_report' event (reuse jsonb).
/// No submit, no real, paper-only. See long RISK comment at spawn site.
/// One side's fused decision for a market (the result of evaluating "buy this outcome").
struct SideEval {
    outcome: &'static str,
    mid: rust_decimal::Decimal,
    gross: rust_decimal::Decimal,
    net: rust_decimal::Decimal,
    attr: serde_json::Value,
}

/// Build the per-side snapshot (recent move + book + shared external/resolution context) and fuse it,
/// returning the side's gross/net edge + attribution. Shared by the default (cheaper-side) path and the
/// opt-in both-sides path so the fusion math is identical for either. `external` and `days_to_resolution`
/// are market-level (fetched once by the caller and reused for both sides — important: news is metered).
/// Returns `None` on an invalid mid or a fusion error (logged; the market is then skipped/ignored).
#[allow(clippy::too_many_arguments)]
async fn evaluate_dr_side(
    pool: &sqlx::PgPool,
    engine: &FusionEngine,
    gamma_id: &str,
    outcome: &'static str,
    mid: rust_decimal::Decimal,
    external: &serde_json::Value,
    days_to_resolution: Option<rust_decimal::Decimal>,
    ctx: &serde_json::Value,
    taker_fee_rate: rust_decimal::Decimal,
    notional: rust_decimal::Decimal,
) -> Option<SideEval> {
    if !(mid >= dec!(0) && mid <= dec!(1)) {
        warn!(market = %gamma_id, outcome, mid = %mid, "invalid mid; skipping side (robust)");
        return None;
    }
    // Real Polymarket taker fee for THIS side: per-market rate + this side's price (the fee shape
    // p × (1−p) makes the cost side-dependent — a 0.10 side costs rate×0.90 of notional, a 0.90
    // side only rate×0.10). Makers pay nothing; we always cross, so taker is the right side.
    let fee_ctx = FeeContext {
        taker_fee_rate,
        price: mid,
        est_gas_usdc: dec!(0.01),
    };
    let recent_move_signed = compute_recent_move_signed(pool, gamma_id, outcome, mid).await;
    let recent_move = recent_move_signed.abs();
    let (book_bids, book_asks) = fetch_latest_book(pool, gamma_id, outcome).await;
    let mut snapshot = serde_json::json!({
        "gamma_id": gamma_id,
        "target_outcome": outcome,
        "target_mid": mid.to_string(),
        "recent_move": recent_move.to_string(),
        "recent_move_signed": recent_move_signed.to_string(),
        "bids": book_bids,
        "asks": book_asks,
        "external": external,
        "paper_only": true,
        "source": "5min_dr_generator"
    });
    if let Some(d) = days_to_resolution {
        snapshot["days_to_resolution"] = serde_json::json!(d.to_string());
    }
    match engine.fuse_net(&snapshot, ctx, Some(&fee_ctx), notional) {
        Ok((gross, net, attr)) => Some(SideEval {
            outcome,
            mid,
            gross,
            net,
            attr,
        }),
        Err(e) => {
            warn!(market = %gamma_id, outcome, error = %e, "fuse_net failed for side (robust; skip)");
            None
        }
    }
}

async fn produce_5min_decision_report(
    pool: &sqlx::PgPool,
    journal: &Arc<JournalWriter>,
    paper_engine: &Arc<PaperTradingEngine>,
) -> anyhow::Result<()> {
    // Smallest/initial 5min DR generator (per plan/goals "skeleton vs production" + "separate future" + "orthogonal";
    // defers full ranked opportunities + risk/goal filters + change detection/rate limit per market to next when
    // "fuller generator active"). Hardcoded LIMIT + FeeContext + stub processors ok for limited wiring (can yield
    // sparse/zero DRs some ticks, as intended for skeleton; see strategy skeleton + non-overclaim in log/wiki).
    // Cover the full directional-eligible set (bootstrap + rotation) with headroom for discovery.
    // 40 → 50 (2026-08-01), a deliberately small step. Measured cost profile before changing it:
    // a full 40-market cycle runs in 0.3–0.9s against a 5-min budget (compute is not the
    // constraint), and external calls are already capped (Yahoo only fires for ticker-linked slugs;
    // news is non-arb-only, 2h-cached, 180/day). DISK is the real cost — DRs average 2113 bytes and
    // GC keeps 14 days raw before rolling up into signal_daily, so steady state ≈ limit × 288 × 14 ×
    // 2.1KB (40 ⇒ ~340MB, matching the observed 323MB).
    // NOTE this does NOT increase trading: the ORDER BY already ranks the directional universe and
    // bootstrap slugs first, so every tradeable market always had a DR — 35 of the 40 slots go to
    // markets the executor vetoes as arb-only anyway, and zero executor rejections cite a missing
    // DR (they all cite net_edge/friction). This buys observability only.
    const DR_MARKET_LIMIT: i64 = 50;
    // Active markets with mids; slug drives arb-only routing + the newsdata query.
    // (gamma_id, slug, last_mid_yes, last_mid_no, end_date_iso, taker_fee_rate)
    type DrMarketRow = (
        String,
        String,
        Option<rust_decimal::Decimal>,
        Option<rust_decimal::Decimal>,
        Option<String>,
        Option<rust_decimal::Decimal>,
    );
    // Directional-eligible slugs (bootstrap env + active rotation rows) rank FIRST. Without this,
    // ORDER BY updated_at alone is a lottery: every ingest tick refreshes the whole ~140-market
    // universe within seconds, so the ~20 DR slots fill with arbitrary arb-only discovery markets
    // and the markets the executor is actually allowed to trade never get a decision report.
    let bootstrap_slugs: Vec<String> = std::env::var("POLYTRADER_BOOTSTRAP_MARKETS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let markets: Vec<DrMarketRow> = sqlx::query_as(
        // resolved_outcome IS NULL: never generate a DR for an already-resolved market. Without this a
        // resolved-but-still-`active` market is re-entered every cycle and `settle_resolved_positions`
        // settles it immediately (open→settle→open→settle), pumping phantom realized P&L and producing
        // the sawtooth P&L chart. (Surfaced 2026-06-24: us-iran-nuclear-deal-by-july-31 settled 20×/day
        // at +$0.20.) closed = false is the belt-and-suspenders companion: a market that has closed but
        // is not yet UMA-resolved still isn't tradeable, so it should never produce a DR or be entered
        // either (Polymarket leaves both `active=true` after a market closes/resolves).
        "SELECT gamma_id, slug, last_mid_yes, last_mid_no, raw_json->>'end_date', taker_fee_rate
         FROM market_data.markets m
         WHERE active = true
           AND resolved_outcome IS NULL
           AND closed = false
           AND (last_mid_yes IS NOT NULL OR last_mid_no IS NOT NULL)
         ORDER BY (m.slug = ANY($2)
                   OR EXISTS (SELECT 1 FROM market_data.directional_universe du
                               WHERE du.slug = m.slug AND du.demoted_at IS NULL)) DESC,
                  updated_at DESC
         LIMIT $1",
    )
    .bind(DR_MARKET_LIMIT)
    .bind(&bootstrap_slugs)
    .fetch_all(pool)
    .await?;

    // Load Hermes-learned processor weights (closed loop). Empty map → all 1.0 (neutral).
    let learned_weights = crate::strategy::load_processor_weights(pool).await;
    let engine = FusionEngine::with_weights(learned_weights);

    // Load Hermes's calibration reliability buckets (closed loop, same pattern as processor
    // weights): refines the crude win_prob estimate below once a probability band has enough
    // settled evidence (roadmap "Calibrate Kelly win_prob from settled records", 2026-07-17 review
    // #5). Ships enabled but inert until `risk::CALIBRATION_MIN_SAMPLES_PER_BUCKET` clears per band
    // — Null/empty (no reflection yet, or too thin) falls open to the raw estimate unchanged.
    let calibration_buckets: serde_json::Value = sqlx::query_scalar(
        "SELECT metrics->'calibration'->'reliability_buckets' FROM journal.reflections
         ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .unwrap_or(serde_json::Value::Null);

    // The POLYTRADER_EXTERNAL_SIGNALS gate and its HTTP client were removed 2026-08-02 with the
    // yahoo_finance / news_sentiment processors — the DR cycle no longer makes any outbound call.

    // Fee context is built PER SIDE inside the loop (evaluate_dr_side): the real Polymarket taker
    // fee needs the per-market rate (stored from Gamma; geopolitics FREE, crypto 0.07…) and the
    // side's price p (fee shape = rate × p × (1−p)). The old single flat-50bps context both mis-
    // priced categories AND (via a unit bug in fuse_net) over-penalized every DR ~10×.

    // Realistic fixed small virtual USD notional for costing (fixes notional misuse bug: target_mid is price ~[0,1],
    // not position size; passing it yielded only tiny fees ~0.01 instead of meaningful "net after fees" for
    // PRIMARY signal in DecisionReport/journaled 'decision_report'. Use dec!(10) for initial limited generator
    // ($10 virtual at $150 scale; conservative per goals). TODO: realistic position sizing (e.g. 1% bankroll or
    // from paper engine) + per-market liquidity in fuller generator (per goals "Ranked list... apply risk/goal
    // filters" + "skeleton vs production"); same pre-existing pattern in server.rs:7544 build_strategy_paper_candidates
    // (reuse when aligning). This ensures net_edge_after_fees is the "PRIMARY signal for deliberate 5-min tier".
    let realistic_notional = dec!(10);

    // Side selection mode. Default: target the cheaper side (historical behavior). Opt-in via
    // POLYTRADER_DR_EVAL_BOTH_SIDES=on: evaluate BOTH outcomes and target whichever has the higher net
    // edge after fees — so the DR/executor can buy a favorite when the signals support it (e.g. theta's
    // converging-favorite case, or the calibration finding that high-conviction bets are underpriced),
    // instead of being locked to the underdog. Paper-only; the net-edge gate + Kelly still govern.
    let eval_both_sides = std::env::var("POLYTRADER_DR_EVAL_BOTH_SIDES")
        .map(|v| v.trim().eq_ignore_ascii_case("on"))
        .unwrap_or(false);

    // Hoisted out of the per-market loop below. This is Kelly's bankroll term, and it cannot change
    // while the loop runs — the loop only READS and journals; nothing it does writes a portfolio
    // snapshot. It was re-running the identical `ORDER BY as_of DESC LIMIT 1` once per market, so at
    // DR_MARKET_LIMIT=50 that was 50 round-trips per cycle for one unchanging number.
    let portfolio_usdc = current_portfolio_usdc(pool).await;

    for (gamma_id, slug, my, mn, end_date_iso, stored_fee_rate) in markets {
        // Per-market taker fee rate: stored Gamma feeSchedule rate, else the category default.
        let taker_fee_rate = stored_fee_rate.unwrap_or_else(|| polymarket_taker_fee_rate(&slug));
        // External advisory context (Yahoo spot + newsdata.io headlines) was removed 2026-08-02
        // along with the two processors that consumed it — see strategy/mod.rs for why. The
        // attribution key stays present and empty so decision-report consumers (hermes, the
        // backtest harness, the scorecard) keep a stable payload shape across the change.
        let external = serde_json::Value::Object(serde_json::Map::new());
        // Days to resolution (for the theta/convergence signal), parsed from the market's gamma end
        // date. Absent/unparseable (e.g. not re-ingested with end_date yet) ⇒ omitted, and theta stays
        // dormant. Negative (already past end date, e.g. UMA dispute) is passed through; theta gates it.
        let days_to_resolution: Option<rust_decimal::Decimal> = end_date_iso
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|end| {
                let secs = (end.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_seconds();
                (rust_decimal::Decimal::from(secs) / dec!(86400)).round_dp(4)
            });
        let ctx = serde_json::json!({
            "paper_only": true,
            "tier": "5min_dr_cadence",
            "min_net_edge_for_trade": crate::risk::RiskConfig::from_env().min_net_edge.to_string(),
            "costing_notional": realistic_notional.to_string()
        });

        // Candidate side(s): the cheaper side always; the other side too when both-sides eval is on.
        let cheaper_is_yes = my.unwrap_or(dec!(0)) <= mn.unwrap_or(dec!(0));
        let (cheap_outcome, cheap_mid): (&'static str, rust_decimal::Decimal) = if cheaper_is_yes {
            ("Yes", my.unwrap_or(dec!(0.5)))
        } else {
            ("No", mn.unwrap_or(dec!(0.5)))
        };
        let mut candidates: Vec<(&'static str, rust_decimal::Decimal)> =
            vec![(cheap_outcome, cheap_mid)];
        if eval_both_sides {
            let (other_outcome, other_mid): (&'static str, rust_decimal::Decimal) =
                if cheaper_is_yes {
                    ("No", mn.unwrap_or(dec!(0.5)))
                } else {
                    ("Yes", my.unwrap_or(dec!(0.5)))
                };
            candidates.push((other_outcome, other_mid));
        }

        let mut evals: Vec<SideEval> = Vec::new();
        for (outcome, mid) in candidates {
            if let Some(e) = evaluate_dr_side(
                pool,
                &engine,
                &gamma_id,
                outcome,
                mid,
                &external,
                days_to_resolution,
                &ctx,
                taker_fee_rate,
                realistic_notional,
            )
            .await
            {
                evals.push(e);
            }
        }
        // Target the side with the highest net edge after fees (the single candidate when eval is off).
        let Some(chosen) = evals.into_iter().max_by(|a, b| a.net.cmp(&b.net)) else {
            continue; // no valid side (e.g. invalid mids) — skip this market
        };
        let target_outcome = chosen.outcome;
        let target_mid = chosen.mid;
        let side_selection = if eval_both_sides {
            "both_sides_best_net_edge"
        } else {
            "cheaper_side_default"
        };
        let (gross, net, attr) = (chosen.gross, chosen.net, chosen.attr);
        let report = DecisionReport {
            fused_gross_edge: gross,
            net_edge_after_fees: net,
            confidence: dec!(0.5),
            attribution: attr,
        };

        // Kelly sizing recommendation (for Hermes attribution; no auto-submit).
        // win_prob ≈ target_mid + net_edge (crude but directionally correct), refined by Hermes's
        // reliability buckets where a band has enough settled evidence (see calibration_buckets
        // above; a no-op until then).
        // RISK: this is a rough estimate; always apply fractional Kelly + caps.
        let win_prob_raw = (target_mid + net).min(dec!(0.99)).max(dec!(0.01));
        let win_prob = crate::risk::calibrate_win_prob(
            win_prob_raw,
            &calibration_buckets,
            crate::risk::CALIBRATION_MIN_SAMPLES_PER_BUCKET,
        );
        let risk_mgr = RiskManager::from_env();
        let sizing = risk_mgr.kelly_size(win_prob, target_mid, portfolio_usdc);

        let payload = serde_json::json!({
            "report": {
                "fused_gross_edge": report.fused_gross_edge.to_string(),
                "net_edge_after_fees": report.net_edge_after_fees.to_string(),
                "confidence": report.confidence.to_string(),
                "attribution": report.attribution
            },
            "kelly_sizing": {
                "recommended_usdc": sizing.recommended_usdc.to_string(),
                "kelly_usdc": sizing.kelly_usdc.to_string(),
                "capped_by": sizing.capped_by,
                "rationale": sizing.rationale,
                "portfolio_usdc": portfolio_usdc.to_string(),
                "win_prob_estimate": win_prob.to_string(),
                "win_prob_estimate_raw": win_prob_raw.to_string(),
                "win_prob_calibrated": win_prob != win_prob_raw,
            },
            "market_id": gamma_id,
            "target_outcome": target_outcome,
            "target_mid": target_mid.to_string(),
            "side_selection": side_selection,
            "generated_by": "5min_dr_cadence_in_main",
            "paper_only": true,
            "real_orders_enabled": false,
            // The prose that lived here (412 bytes, byte-identical on all 160k rows = ~66MB, 22% of
            // the DR payload) moved to the doc comment on this function and to
            // wiki/strategies/goals-and-operational-cadence.md — a constant string is documentation,
            // not per-row data, and the journal is not where documentation belongs.
            "spec": "wiki/strategies/goals-and-operational-cadence.md",
        });
        // Capture id for audit (Issue 10 review); writer logs at debug. Robust: warn on err, do not crash loop (per plan "smallest").
        match journal
            .record_journal_event("decision_report", "polytrader_5min_dr", "info", payload)
            .await
        {
            Ok(id) => {
                tracing::debug!(id = %id, market = %gamma_id, "decision_report journaled");
            }
            Err(e) => {
                warn!(error = %e, market = %gamma_id, "failed to journal decision_report (robust; continue)");
            }
        }

        // Autonomous executor: take this opportunity through the risk gate and (if approved
        // + enabled) place a Kelly-sized paper order. Gated + paper-only; errors never abort.
        if let Err(e) = maybe_execute_opportunity(
            pool,
            journal,
            paper_engine,
            &gamma_id,
            &slug,
            target_outcome,
            target_mid,
            net,
            &sizing,
        )
        .await
        {
            warn!(error = %e, market = %gamma_id, "autonomous execution error (paper-only; continue)");
        }
    }
    Ok(())
}

/// Autonomous paper executor: take a passing Decision Report through the risk gate and, if approved
/// Drawdown circuit-breaker threshold (percent) from `POLYTRADER_DRAWDOWN_HALT_PCT`. Returns `None`
/// (breaker DISABLED — default) unless a positive percent is configured. Opt-in, mirroring the other
/// autonomous gates; when set, new directional entries halt while NAV is >= this far below its peak.
fn drawdown_halt_threshold() -> Option<rust_decimal::Decimal> {
    std::env::var("POLYTRADER_DRAWDOWN_HALT_PCT")
        .ok()
        .and_then(|s| s.trim().parse::<rust_decimal::Decimal>().ok())
        .filter(|t| *t > dec!(0))
}

/// Pure: is the drawdown circuit-breaker tripped? Disabled (always false) when `threshold_pct` is
/// `None`; otherwise tripped once the current drawdown-from-peak reaches the threshold.
fn drawdown_breaker_tripped(
    current_dd_pct: rust_decimal::Decimal,
    threshold_pct: Option<rust_decimal::Decimal>,
) -> bool {
    matches!(threshold_pct, Some(t) if current_dd_pct >= t)
}

/// Daily directional-entry cap (roadmap P6 turnover budget). Friction scales linearly with trade
/// count and the directional book runs ~flat gross, so fewer, better trades is a direct P&L lever.
/// Default 6/day; 0 (or unparsable) disables. Env: POLYTRADER_MAX_DAILY_ENTRIES.
fn daily_entry_cap() -> i64 {
    std::env::var("POLYTRADER_MAX_DAILY_ENTRIES")
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(6)
}

/// Pure: is the daily turnover budget exhausted? Disabled when `cap <= 0`.
fn turnover_budget_exhausted(entries_today: i64, cap: i64) -> bool {
    cap > 0 && entries_today >= cap
}

/// Current paper-account NAV drawdown from its all-time peak, in percent (0 if there are no snapshots
/// or no positive peak). NAV = virtual_usdc + total_locked + unrealized_pnl — the same equity
/// definition as the /trades scorecard and the Hermes drawdown monitor.
async fn current_drawdown_pct(pool: &sqlx::PgPool) -> rust_decimal::Decimal {
    let row: Option<(rust_decimal::Decimal, rust_decimal::Decimal)> = sqlx::query_as(
        "WITH eq AS (
           SELECT as_of, (virtual_usdc + total_locked + unrealized_pnl) AS equity
           FROM paper_trading.virtual_portfolio_snapshots)
         SELECT COALESCE((SELECT equity FROM eq ORDER BY as_of DESC LIMIT 1), 0),
                COALESCE((SELECT max(equity) FROM eq), 0)",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    match row {
        Some((current, peak)) if peak > dec!(0) => {
            ((peak - current) / peak * dec!(100)).round_dp(2)
        }
        _ => dec!(0),
    }
}

/// and enabled, submit a Kelly-sized paper order. Gated by POLYTRADER_AUTONOMOUS_PAPER_EXECUTION=on
/// (default off — pure evaluation, no orders).
///
/// RISK (paper-only, heavily gated): never places a real order. Every order must clear
/// RiskManager::check_pre_trade (min net edge, PnL floor, total + per-market exposure caps) and the
/// Kelly position caps. One position per (market, outcome) at a time (no pyramiding). Both fills and
/// risk-gate rejections are journaled as `autonomous_paper_execution` for the Hermes closed loop.
#[allow(clippy::too_many_arguments)]
async fn maybe_execute_opportunity(
    pool: &sqlx::PgPool,
    journal: &Arc<JournalWriter>,
    engine: &Arc<PaperTradingEngine>,
    gamma_id: &str,
    slug: &str,
    target_outcome: &str,
    target_mid: rust_decimal::Decimal,
    net_edge: rust_decimal::Decimal,
    sizing: &crate::risk::PositionSizeResult,
) -> anyhow::Result<()> {
    // Gate: opt-in only.
    if std::env::var("POLYTRADER_AUTONOMOUS_PAPER_EXECUTION")
        .unwrap_or_default()
        .trim()
        .to_lowercase()
        != "on"
    {
        return Ok(());
    }

    // Drawdown circuit-breaker (opt-in via POLYTRADER_DRAWDOWN_HALT_PCT; default OFF). When the paper
    // account NAV has fallen >= the configured % from its all-time peak, HALT new directional entries
    // ("stop digging") until it recovers. Re-checked every cycle, so it AUTO-RESUMES once drawdown
    // falls back below the threshold. It does NOT liquidate existing positions, and does NOT block the
    // risk-free YES+NO arbitrage executor (which only reduces net risk). Same signal the Hermes
    // `drawdown` monitor / drawdown_alert surface. Zero overhead when disabled (no query unless set).
    if let Some(threshold) = drawdown_halt_threshold() {
        let dd = current_drawdown_pct(pool).await;
        if drawdown_breaker_tripped(dd, Some(threshold)) {
            // De-spam: while halted, every market would trip this each cycle — journal once / hour.
            let recent_halt: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM journal.events
                 WHERE event_type = 'autonomous_paper_execution'
                   AND payload->>'action' = 'halted_by_drawdown_circuit_breaker'
                   AND created_at > now() - interval '1 hour'",
            )
            .fetch_one(pool)
            .await
            .unwrap_or(0);
            if recent_halt == 0 {
                let _ = journal
                    .record_journal_event(
                        "autonomous_paper_execution",
                        "polytrader_executor",
                        "warn",
                        serde_json::json!({
                            "action": "halted_by_drawdown_circuit_breaker",
                            "current_drawdown_pct": dd.to_string(),
                            "threshold_pct": threshold.to_string(),
                            "paper_only": true,
                            "real_orders_enabled": false,
                            "note": "New directional entries paused: NAV drawdown from peak >= POLYTRADER_DRAWDOWN_HALT_PCT. Auto-resumes when drawdown recovers below the threshold. Existing positions untouched; risk-free arb executor unaffected.",
                        }),
                    )
                    .await;
            }
            tracing::warn!(drawdown_pct = %dd, threshold_pct = %threshold, "drawdown circuit-breaker tripped; halting new directional entries (paper)");
            return Ok(());
        }
    }

    // P6 — Daily turnover budget: cap autonomous directional ENTRIES per UTC day (default 6,
    // POLYTRADER_MAX_DAILY_ENTRIES, 0 disables). Friction is paid per trade while gross edge runs
    // ~flat, so the marginal entry is usually P&L-negative once the day's best opportunities are
    // taken. Greedy first-come within the day (the min-edge + friction gates already enforce
    // per-trade quality; a global best-first ranking isn't knowable mid-day). Exits, and the
    // risk-free arb executor (separate path), are deliberately NOT counted or capped.
    let cap = daily_entry_cap();
    if cap > 0 {
        // Deliberately NOT hoisted out of the caller's per-market loop, unlike
        // `current_portfolio_usdc`. This function journals its own `action: "filled"` events, so the
        // count it reads is one THIS loop increments — caching it across markets would let a cycle
        // fill `cap` orders per market instead of `cap` in total. A re-read per candidate is the
        // price of the cap actually binding.
        let entries_today: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM journal.events
             WHERE event_type = 'autonomous_paper_execution'
               AND payload->>'action' = 'filled'
               AND created_at >= date_trunc('day', now())",
        )
        .fetch_one(pool)
        .await
        .unwrap_or(0);
        if turnover_budget_exhausted(entries_today, cap) {
            // De-spam: journal the halt at most once per hour (every candidate market would
            // otherwise re-journal it each 5-min cycle for the rest of the day).
            let recent_halt: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM journal.events
                 WHERE event_type = 'autonomous_paper_execution'
                   AND payload->>'action' = 'halted_by_daily_turnover_budget'
                   AND created_at > now() - interval '1 hour'",
            )
            .fetch_one(pool)
            .await
            .unwrap_or(0);
            if recent_halt == 0 {
                let _ = journal
                    .record_journal_event(
                        "autonomous_paper_execution",
                        "polytrader_executor",
                        "info",
                        serde_json::json!({
                            "action": "halted_by_daily_turnover_budget",
                            "entries_today": entries_today,
                            "daily_cap": cap,
                            "paper_only": true,
                            "real_orders_enabled": false,
                            "note": "New directional entries paused until the next UTC day: filled-entry count reached POLYTRADER_MAX_DAILY_ENTRIES (P6 turnover budget). Exits and the risk-free arb executor are unaffected.",
                        }),
                    )
                    .await;
            }
            tracing::info!(entries_today, cap, "daily turnover budget exhausted; skipping new directional entries until next UTC day (paper)");
            return Ok(());
        }
    }

    // Arb-only markets (sports / World Cup): never take a directional bet — only the YES+NO
    // Directional eligibility, two grant paths with DIFFERENT category rules:
    //  - ROTATION-promoted (market_data.directional_universe): eligible AS GRANTED. The promotion
    //    pipeline already vetoed sports/esports via the event-TAG gate (Polymarket's own taxonomy),
    //    and `arb_category` must NOT re-veto here — it doubles as the FEE-category classifier, so
    //    adding a category for fee purposes would silently kill eligibility (exactly what happened
    //    2026-07-04→05: adding "mentions" for the 0.04 fee rate made the rotation-promoted
    //    musk-tweets market arb-only, and a 16%-net-edge DR was skipped without even a rejection).
    //  - BOOTSTRAP env slugs: the historical hand-curated list, which deliberately contains
    //    arb-only markets (World Cup, crypto) — the `is_arb_only_market` veto still applies.
    // Everything else (the volume-ranked discovery universe) is ARB-ONLY by default.
    let is_rotation_active = rotation::is_active(pool, slug).await;
    if !is_rotation_active && is_arb_only_market(slug) {
        tracing::debug!(slug = %slug, "arb-only market; directional executor skips");
        return Ok(());
    }
    let is_curated = is_rotation_active
        || std::env::var("POLYTRADER_BOOTSTRAP_MARKETS")
            .unwrap_or_default()
            .split(',')
            .any(|s| s.trim() == slug);
    if !is_curated {
        tracing::debug!(slug = %slug, "non-curated discovery market; directional executor skips (arb-only)");
        return Ok(());
    }
    if sizing.recommended_usdc <= dec!(0) || target_mid <= dec!(0) {
        return Ok(());
    }

    // RE-ENTRY COOLDOWN: after ANY autonomous exit on this market, block new directional entries
    // for POLYTRADER_REENTRY_COOLDOWN_HOURS (default 24). Without this, exits CHURN against the
    // entry loop (measured overnight 2026-07-05→06: −$54 across 9 stop-losses in 10h): a stop-loss
    // frees the market, the next 5-min DR still likes it → rebuy (england-mexico was bought 4×),
    // and the both-sides eval oscillates sides via signal-flip exits (WTI-85 flipped Yes⇄No 5×).
    // Each round trip pays the spread + fees. Per-market (not per-side) so a flip can't dodge it.
    let cooldown_hours: i64 = std::env::var("POLYTRADER_REENTRY_COOLDOWN_HOURS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(24);
    let recently_exited: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM journal.events
          WHERE event_type = 'autonomous_paper_exit' AND payload->>'market_id' = $1
            AND created_at > now() - make_interval(hours => $2::int))",
    )
    .bind(gamma_id)
    .bind(cooldown_hours as i32)
    .fetch_one(pool)
    .await
    .unwrap_or(false);
    if recently_exited {
        tracing::debug!(market = %gamma_id, slug = %slug, cooldown_hours,
            "recently exited; directional re-entry blocked by cooldown");
        return Ok(());
    }

    // One directional position per market: skip if we already hold ANY open position in this market,
    // on EITHER outcome. This blocks two things:
    //   (1) pyramiding the same side, and
    //   (2) opening the OPPOSITE side — holding both Yes and No in a binary market is an economic wash
    //       that only burns taker fees on both legs and locks collateral for ~zero net edge. (The
    //       cheaper side flips between cycles as prices move, so without this guard we accumulate both;
    //       the cluster cap bounds total exposure but does NOT prevent same-market hedging.)
    // The risk-free YES+NO arbitrage path is a separate executor (execute_arb_opportunity) and is not
    // affected by this directional guard.
    let held_outcome: Option<String> = sqlx::query_scalar(
        "SELECT outcome FROM paper_trading.paper_positions WHERE market_id = $1 AND shares > 0 LIMIT 1",
    )
    .bind(gamma_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    if let Some(held) = held_outcome {
        if held.eq_ignore_ascii_case(target_outcome) {
            tracing::debug!(market = %gamma_id, outcome = %target_outcome, "already positioned (same side); skipping autonomous entry");
        } else {
            tracing::debug!(market = %gamma_id, target = %target_outcome, held = %held, "already hold opposite side; skipping to avoid a both-sides wash");
        }
        return Ok(());
    }

    // Risk gate (PRIMARY): min net edge + PnL floor + exposure + concentration. May trim the size.
    let risk = RiskManager::from_env();
    // A/B gate attribution: does this trade also clear the stricter "shadow" threshold? The live gate
    // is min_net_edge (e.g. 2%); shadow_net_edge (e.g. 4%) is recorded but NOT enforced, so we can
    // compare how the stricter gate would have performed using the same live run.
    let shadow_threshold = risk.config().shadow_net_edge;
    let clears_shadow = net_edge >= shadow_threshold;
    let edge_band = if clears_shadow {
        "strict_ge_shadow"
    } else {
        "lenient_below_shadow"
    };
    let check = risk
        .check_pre_trade(
            pool,
            gamma_id,
            net_edge,
            sizing.recommended_usdc,
            target_mid,
        )
        .await?;
    if !check.approved {
        // De-spam: a permanently-unattractive market (e.g. a longshot with negative edge) would
        // otherwise journal an identical rejection every 5 min. Only record it if we haven't already
        // journaled a rejection for this market in the last hour.
        let recent_reject: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM journal.events
             WHERE event_type = 'autonomous_paper_execution'
               AND payload->>'action' = 'rejected_by_risk_gate'
               AND payload->>'market_id' = $1
               AND created_at > now() - interval '1 hour'",
        )
        .bind(gamma_id)
        .fetch_one(pool)
        .await
        .unwrap_or(0);
        if recent_reject == 0 {
            let _ = journal
                .record_journal_event(
                    "autonomous_paper_execution",
                    "polytrader_executor",
                    "info",
                    serde_json::json!({
                        "action": "rejected_by_risk_gate",
                        "market_id": gamma_id,
                        "outcome": target_outcome,
                        "net_edge": net_edge.to_string(),
                        "proposed_usdc": sizing.recommended_usdc.to_string(),
                        "reason": check.reason,
                        "paper_only": true,
                        "real_orders_enabled": false,
                    }),
                )
                .await;
        }
        return Ok(());
    }

    let approved_usdc = check.recommended_size.unwrap_or(sizing.recommended_usdc);

    // CRITICAL: use a LIMIT order (never a market order — a market order for many shares walks a
    // thin book and overspends wildly, e.g. 8000 shares of a 0.0025 longshot fills up to ~$1.00 →
    // ~$8000 on an intended $20). The limit is a generous ceiling so liquid markets fill, while we
    // size shares against that ceiling so worst-case spend is hard-bounded to approved_usdc:
    //   shares = approved_usdc / limit_price  ⇒  shares × limit_price = approved_usdc (max spend).
    // The engine fills at the real (better) ask, so realized cost ≤ approved_usdc and realized
    // entry price ≤ limit. Thin/extreme books (ask above the ceiling) simply don't fill — correct.
    const SLIPPAGE_CAP: rust_decimal::Decimal = dec!(1.20); // ceiling = 20% above mid (fill headroom)
    let limit_price = (target_mid * SLIPPAGE_CAP).min(dec!(0.99));
    let shares = (approved_usdc / limit_price).round_dp(2);
    if shares <= dec!(0) {
        return Ok(());
    }

    let order = crate::paper::PaperOrder {
        id: uuid::Uuid::new_v4(),
        market_id: gamma_id.to_string(),
        outcome: target_outcome.to_string(),
        side: crate::paper::OrderSide::Buy,
        order_type: crate::paper::OrderType::Limit,
        limit_price: Some(limit_price),
        size: shares,
        status: crate::paper::OrderStatus::Open,
        created_at: chrono::Utc::now(),
        decision_context: Some(serde_json::json!({
            "source": "autonomous_executor_5min_dr",
            "net_edge_after_fees": net_edge.to_string(),
            "kelly_recommended_usdc": sizing.recommended_usdc.to_string(),
            "risk_approved_usdc": approved_usdc.to_string(),
            "risk_reason": check.reason,
            "entry_price_proxy": target_mid.to_string(),
            "limit_price": limit_price.to_string(),
            "slippage_cap": SLIPPAGE_CAP.to_string(),
            "paper_only": true,
            "real_orders_enabled": false,
        })),
    };
    let order_id = order.id;

    match engine.submit_order(order).await {
        Ok(fills) if !fills.is_empty() => {
            let filled: rust_decimal::Decimal = fills.iter().map(|f| f.size).sum();
            let notional: rust_decimal::Decimal = fills.iter().map(|f| f.price * f.size).sum();
            info!(market = %gamma_id, outcome = %target_outcome, filled = %filled, notional = %notional, "autonomous paper order filled (paper-only)");
            let _ = journal
                .record_journal_event(
                    "autonomous_paper_execution",
                    "polytrader_executor",
                    "info",
                    serde_json::json!({
                        "action": "filled",
                        "order_id": order_id.to_string(),
                        "market_id": gamma_id,
                        "outcome": target_outcome,
                        "net_edge": net_edge.to_string(),
                        "edge_band": edge_band,
                        "clears_shadow_gate": clears_shadow,
                        "shadow_net_edge": shadow_threshold.to_string(),
                        "approved_usdc": approved_usdc.to_string(),
                        "shares_submitted": shares.to_string(),
                        "filled_shares": filled.to_string(),
                        "gross_notional": notional.to_string(),
                        "paper_only": true,
                        "real_orders_enabled": false,
                    }),
                )
                .await;

            // Toward live trading (fail-closed): construct + validate the REAL CLOB order this fill
            // WOULD send, run it through the fail-closed sender (refuses dispatch), journal it. $0 risk.
            shadow_real_order(
                pool,
                journal,
                gamma_id,
                target_outcome,
                "buy",
                "limit",
                shares,
                limit_price,
                &order_id.to_string(),
                &ShadowOrderContext {
                    // net_edge is a fraction (the live gate is 0.02 = 2%); the dry-run wants bps.
                    // Note the real bar is DELIBERATELY stricter than the paper gate: 400bps (4%)
                    // vs the 2% we fill on in paper, so a paper fill clearing the live gate can
                    // still — correctly — fail the real edge check. round_dp(1) to match the arb
                    // path (negrisk_basket_edge_bps) — unrounded, this logged 26 decimal places.
                    expected_edge_bps: Some((net_edge * dec!(10000)).round_dp(1)),
                    basket: None,
                },
            )
            .await;
        }
        Ok(_) => {
            tracing::warn!(market = %gamma_id, outcome = %target_outcome, %limit_price, "autonomous order: no fills (ask above limit ceiling — thin/extreme book)");
            let _ = journal
                .record_journal_event(
                    "autonomous_paper_execution",
                    "polytrader_executor",
                    "info",
                    serde_json::json!({
                        "action": "no_fill_at_limit",
                        "market_id": gamma_id,
                        "outcome": target_outcome,
                        "limit_price": limit_price.to_string(),
                        "shares_submitted": shares.to_string(),
                        "note": "ask above the limit ceiling (mid×1.20); correct for thin/extreme books",
                        "paper_only": true,
                        "real_orders_enabled": false,
                    }),
                )
                .await;
        }
        Err(e) => {
            tracing::warn!(error = %e, market = %gamma_id, "autonomous paper submit failed (paper-only)");
        }
    }
    Ok(())
}

/// Resolve the CLOB token_id for a (market, outcome) from the stored market metadata.
async fn resolve_token_id(pool: &sqlx::PgPool, gamma_id: &str, outcome: &str) -> Option<String> {
    let row: Option<(serde_json::Value, serde_json::Value)> = sqlx::query_as(
        "SELECT clob_token_ids, outcomes FROM market_data.markets WHERE gamma_id = $1",
    )
    .bind(gamma_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    let (tokens_j, outcomes_j) = row?;
    let tokens: Vec<String> = serde_json::from_value(tokens_j).ok()?;
    let outcomes: Vec<String> = serde_json::from_value(outcomes_j).ok()?;
    let idx = outcomes
        .iter()
        .position(|o| o.eq_ignore_ascii_case(outcome))?;
    tokens.get(idx).cloned()
}

/// Polymarket's collateral token: PUSD ("Polymarket USD"), 6 decimals, on Polygon. The user's cash
/// is held as PUSD in their proxy/funder wallet — NOT USDC.e, NOT on the signer EOA.
const PUSD_CONTRACT: &str = "0xC011a7E12a19f7B1f670d46F03B03f3342E82DFB";

/// Read the real PUSD balance of an address via the keyless Blockscout explorer (public Polygon RPCs
/// are auth-blocked; Blockscout works from the pod). Read-only; returns balance in PUSD (6-decimal).
async fn fetch_real_pusd_balance(
    client: &reqwest::Client,
    address: &str,
) -> Option<rust_decimal::Decimal> {
    let url = format!("https://polygon.blockscout.com/api/v2/addresses/{address}/token-balances");
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let arr: Vec<serde_json::Value> = resp.json().await.ok()?;
    for t in &arr {
        let tok = &t["token"];
        let addr = tok["address"]
            .as_str()
            .or_else(|| tok["address_hash"].as_str())
            .unwrap_or("");
        if addr.eq_ignore_ascii_case(PUSD_CONTRACT) {
            let raw = t["value"].as_str()?;
            let raw_dec: rust_decimal::Decimal = raw.parse().ok()?;
            return Some((raw_dec / dec!(1000000)).round_dp(2)); // PUSD has 6 decimals
        }
    }
    Some(dec!(0)) // address holds no PUSD
}

/// Fetch the real PUSD balance of the configured proxy (POLYMARKET_ADDRESS) and journal it as a
/// `real_account_balance` event for the /trades UI + the "funded" gate. Read-only; best-effort.
async fn fetch_and_journal_real_balance(journal: &Arc<JournalWriter>) {
    let proxy = std::env::var("POLYMARKET_ADDRESS").unwrap_or_default();
    let proxy = proxy.trim();
    if proxy.is_empty() {
        return;
    }
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    if let Some(balance) = fetch_real_pusd_balance(&client, proxy).await {
        let _ = journal
            .record_journal_event(
                "real_account_balance",
                "polytrader_balance",
                "info",
                serde_json::json!({
                    "proxy_address": proxy,
                    "collateral_token": "PUSD",
                    "collateral_contract": PUSD_CONTRACT,
                    "balance": balance.to_string(),
                    "source": "polygon.blockscout.com",
                    "paper_only": true,
                    "real_orders_enabled": false,
                }),
            )
            .await;
        tracing::info!(proxy = %proxy, pusd_balance = %balance, "real account balance fetched (read-only)");
    }
}

/// Pure settlement math: a winning binary share pays $1, a losing share pays $0; realized P&L is the
/// payout minus the cost basis. Returns (payout, realized_pnl), both rounded to cents.
fn settlement_payout_and_pnl(
    won: bool,
    shares: rust_decimal::Decimal,
    cost_basis: rust_decimal::Decimal,
) -> (rust_decimal::Decimal, rust_decimal::Decimal) {
    let payout = if won { shares } else { dec!(0) };
    (payout.round_dp(2), (payout - cost_basis).round_dp(2))
}

/// Settle paper positions on resolved markets to realized P&L — the ground-truth signal that finally
/// tells us whether the strategies make money. For each open position on a market whose
/// `resolved_outcome` is known: winning shares pay $1 each, losing shares pay $0. Realized P&L =
/// payout − cost basis (collateral_locked). Updates the portfolio snapshot (realized P&L, freed
/// collateral, cash) and journals `paper_position_settled` per position (for Hermes + the proven
/// gate). Idempotent: settled positions are deleted, so a market can't be settled twice. Paper-only.
async fn settle_resolved_positions(
    pool: &sqlx::PgPool,
    journal: &Arc<JournalWriter>,
) -> anyhow::Result<()> {
    type SettleRow = (
        String,
        String,
        rust_decimal::Decimal,
        rust_decimal::Decimal,
        rust_decimal::Decimal,
        String,
    );
    let rows: Vec<SettleRow> = sqlx::query_as(
        "SELECT p.market_id, p.outcome, p.shares, p.avg_entry_price, p.collateral_locked, m.resolved_outcome
         FROM paper_trading.paper_positions p
         JOIN market_data.markets m ON m.gamma_id = p.market_id
         WHERE p.shares > 0 AND m.resolved_outcome IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }

    let mut realized_delta = dec!(0);
    for (market_id, outcome, shares, avg_entry, cost_basis, winner) in rows {
        let won = outcome.eq_ignore_ascii_case(&winner);
        let (payout, pnl) = settlement_payout_and_pnl(won, shares, cost_basis);
        realized_delta += pnl;

        sqlx::query(
            "DELETE FROM paper_trading.paper_positions WHERE market_id = $1 AND outcome = $2",
        )
        .bind(&market_id)
        .bind(&outcome)
        .execute(pool)
        .await
        .ok();

        let _ = journal
            .record_journal_event(
                "paper_position_settled",
                "polytrader_settlement",
                "info",
                serde_json::json!({
                    "market_id": market_id,
                    "outcome": outcome,
                    "winning_outcome": winner,
                    "won": won,
                    "shares": shares.to_string(),
                    "avg_entry_price": avg_entry.to_string(),
                    "cost_basis": cost_basis.to_string(),
                    "payout": payout.to_string(),
                    "realized_pnl": pnl.to_string(),
                    "paper_only": true,
                    "real_orders_enabled": false,
                }),
            )
            .await;
        tracing::info!(market = %market_id, outcome = %outcome, won, pnl = %pnl, "paper position settled to realized P&L");
    }

    // Recompute the portfolio snapshot with the updated cumulative realized P&L. Unrealized is
    // recomputed LIVE (2026-07-20), not carried forward from the latest snapshot: carrying forward
    // (the 2026-07-15 fix, replacing an earlier hardcoded 0) still double-counted a just-settled
    // position for one cycle -- its P&L landed in the fresh realized delta AND stayed in the stale
    // carried-forward unrealized until the next mark_to_market tick, spiking the P&L chart down (or
    // up) and snapping back ~5min later. Positions were already deleted above, so this recompute
    // naturally excludes them -- see compute_live_unrealized_pnl's doc comment.
    let prev_realized: rust_decimal::Decimal = sqlx::query_scalar(
        "SELECT realized_pnl FROM paper_trading.virtual_portfolio_snapshots ORDER BY as_of DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?
    .unwrap_or(dec!(0));
    let new_realized = prev_realized + realized_delta;
    let new_unrealized = compute_live_unrealized_pnl(pool).await;
    let locked: rust_decimal::Decimal = sqlx::query_scalar(
        "SELECT COALESCE(SUM(collateral_locked), 0) FROM paper_trading.paper_positions WHERE shares > 0",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(dec!(0));
    // Fees SINCE THE LAST PAPER RESET only (2026-07-15): this writer was the last one still
    // summing LIFETIME fees — every settlement snapshot transiently re-subtracted pre-reset fees
    // from the $10k seed until the next mark_to_market corrected it. Same reset-boundary rule as
    // write_mark_to_market_snapshot and the engine's post-fill path (see their comments).
    let fees: rust_decimal::Decimal = sqlx::query_scalar(
        "SELECT COALESCE(SUM(fee), 0) FROM paper_trading.paper_fills
         WHERE created_at >= COALESCE(
           (SELECT max(as_of) FROM paper_trading.virtual_portfolio_snapshots
            WHERE snapshot_reason = 'manual_paper_reset'), '-infinity'::timestamptz)",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(dec!(0));
    let new_usdc = (dec!(10000) - locked - fees + new_realized).max(dec!(0));
    sqlx::query(
        "INSERT INTO paper_trading.virtual_portfolio_snapshots
         (as_of, virtual_usdc, total_locked, unrealized_pnl, realized_pnl, snapshot_reason, positions)
         VALUES (now(), $1, $2, $3, $4, 'settlement', '[]'::jsonb)",
    )
    .bind(new_usdc)
    .bind(locked)
    .bind(new_unrealized.round_dp(2))
    .bind(new_realized)
    .execute(pool)
    .await?;
    info!(realized_delta = %realized_delta, cumulative_realized = %new_realized, "settlement pass complete");
    Ok(())
}

/// The hard go-live precondition: real order dispatch may NEVER happen unless ALL are true.
/// Per the operator decision: proven (positive realized P&L over ≥N settled positions) + funded
/// (real collateral > 0) + approved (explicit operator approval after Claude confirms the strategies).
/// Returned as JSON for journaling/UI. NOTE: this build wires only the fail-closed sender, so even
/// if this gate were satisfied, dispatch is structurally impossible — the gate tracks how far we are.
async fn real_trading_precondition(pool: &sqlx::PgPool) -> serde_json::Value {
    const MIN_SETTLED: i64 = 10;

    // proven: realized P&L > 0 over ≥ MIN_SETTLED settled positions (markets we held that resolved).
    let realized: rust_decimal::Decimal = sqlx::query_scalar(
        "SELECT realized_pnl FROM paper_trading.virtual_portfolio_snapshots ORDER BY as_of DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .unwrap_or(dec!(0));
    // Settled positions = resolved-and-realized (paper_position_settled events). RESET-BOUNDARY:
    // reset preserves the journal, so a lifetime count would let pre-reset settlements (incl. the
    // 2026-06-24 phantoms) satisfy MIN_SETTLED against a post-reset track record that proved nothing.
    let settled: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM journal.events WHERE event_type = 'paper_position_settled'
           AND created_at >= COALESCE(
             (SELECT max(as_of) FROM paper_trading.virtual_portfolio_snapshots
              WHERE snapshot_reason = 'manual_paper_reset'), '-infinity'::timestamptz)",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    let proven = realized > dec!(0) && settled >= MIN_SETTLED;

    // funded: real PUSD collateral on the proxy (latest journaled real_account_balance; PUSD is
    // Polymarket's collateral token, held in the proxy/funder wallet — NOT USDC.e on the signer).
    let funded_balance: rust_decimal::Decimal = sqlx::query_scalar::<_, rust_decimal::Decimal>(
        "SELECT (payload->>'balance')::numeric
         FROM journal.events WHERE event_type = 'real_account_balance'
         ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .unwrap_or(dec!(0));
    let funded = funded_balance > dec!(0);

    // approved: explicit operator approval token, set only AFTER strategies are confirmed proven.
    let approved = !std::env::var("POLYTRADER_REAL_ORDERS_OPERATOR_APPROVAL")
        .unwrap_or_default()
        .trim()
        .is_empty();

    serde_json::json!({
        "ready_for_real_dispatch": proven && funded && approved,
        "proven": {
            "ok": proven,
            "realized_pnl": realized.to_string(),
            "settled_positions": settled,
            "min_required": MIN_SETTLED,
        },
        "funded": { "ok": funded, "pusd_balance": funded_balance.to_string(), "source": "PUSD on proxy (real_account_balance)" },
        "approved": {
            "ok": approved,
            "how": "set POLYTRADER_REAL_ORDERS_OPERATOR_APPROVAL only after Claude confirms positive realized P&L",
        },
        "note": "ALL three required before any real order. This build wires only the fail-closed sender, so real dispatch is structurally impossible regardless of this gate.",
    })
}

/// Strategy context for a shadow order — everything the real dry-run needs beyond the raw order
/// fields, kept in one struct so `shadow_real_order` doesn't grow a tenth positional argument.
#[derive(Debug, Clone, Default)]
struct ShadowOrderContext {
    /// Expected edge in basis points. Feeds the dry-run's `expected_edge_present_and_sufficient`
    /// blocker (default floor 400bps = 4%). Added 2026-07-28: this was hardcoded `None` since the
    /// shadow pipeline was built, so that blocker failed on EVERY shadow order (28/28 over the
    /// preceding week) and told us nothing — a permanently-red check is not a signal. `None` still
    /// means "no edge estimate available", which correctly keeps the check failing.
    expected_edge_bps: Option<rust_decimal::Decimal>,
    /// Set when this order is one leg of a multi-leg NegRisk basket: event id, leg count, this
    /// leg's index, and whether the paper leg filled. A per-leg shadow can only answer "is this
    /// order constructible"; assessing whether the BASKET was executable for real needs to know the
    /// legs belong together, since the arb only holds if all of them fill.
    basket: Option<serde_json::Value>,
}

/// Construct + validate the REAL Polymarket CLOB order that a paper fill WOULD send, run it through
/// the fail-closed live sender (which refuses before any network call), and journal a
/// `clob_shadow_order` event. SENDS NOTHING — no network order, no real money. This advances the
/// live-trading path (proves order construction + real account/market validation) while keeping
/// dispatch structurally impossible.
#[allow(clippy::too_many_arguments)]
async fn shadow_real_order(
    pool: &sqlx::PgPool,
    journal: &Arc<JournalWriter>,
    gamma_id: &str,
    outcome: &str,
    side: &str,
    order_type: &str,
    size: rust_decimal::Decimal,
    price: rust_decimal::Decimal,
    paper_order_id: &str,
    ctx: &ShadowOrderContext,
) {
    let token_id = resolve_token_id(pool, gamma_id, outcome).await;

    let intent = crate::clob::authenticated::RealOrderIntentDryRun {
        token_id: token_id.clone().unwrap_or_default(),
        side: side.to_string(),
        order_type: order_type.to_string(),
        size,
        price: Some(price),
        expected_edge_bps: ctx.expected_edge_bps,
        market_id: Some(gamma_id.to_string()),
        outcome: Some(outcome.to_string()),
    };

    // Best-effort real dry-run validation (read-only; needs POLYTRADER_ENABLE_REAL_CLOB_READS=true,
    // else the reads fail closed and we still record the constructed intent).
    let dry_run = match crate::clob::authenticated::RealClobClient::from_current_l2_session() {
        Some(client) => match client.dry_run_order_intent(&intent).await {
            Ok(report) => report,
            Err(e) => serde_json::json!({
                "validated": false,
                "error": e.to_string(),
                "note": "real account/market read failed (reads may be disabled); intent still recorded",
            }),
        },
        None => {
            serde_json::json!({"validated": false, "note": "no L2 session; cannot validate against real account"})
        }
    };

    // Fail-closed sender: prove dispatch is refused before any network call.
    let zero_uuid = "00000000-0000-0000-0000-000000000000".to_string();
    let send_request = crate::clob::live_sender::LiveOrderSendRequest {
        local_order_id: paper_order_id.to_string(),
        order_intent_event_id: String::new(),
        signed_payload_event_id: String::new(),
        human_approval_event_id: zero_uuid.clone(),
        final_review_decision_event_id: zero_uuid,
        market_id: gamma_id.to_string(),
        token_id: token_id.clone().unwrap_or_default(),
        side: side.to_string(),
        order_type: order_type.to_string(),
        size,
        price,
    };
    use crate::clob::live_sender::LiveOrderSender;
    let result = crate::clob::live_sender::FailClosedLiveOrderSender
        .send(&send_request)
        .await;

    let gate = real_trading_precondition(pool).await;

    let mut payload = serde_json::json!({
        "paper_order_id": paper_order_id,
        "would_send": {
            "market_id": gamma_id,
            "token_id": token_id,
            "outcome": outcome,
            "side": side,
            "order_type": order_type,
            "size": size.to_string(),
            "price": price.to_string(),
        },
        "dry_run_validation": dry_run,
        "fail_closed_result": {
            "sender": result.sender_name,
            "accepted_for_network_dispatch": result.accepted_for_network_dispatch,
            "request_sent": result.request_sent,
            "post_order_called": result.post_order_called,
            "rejection_reason": result.rejection_reason,
        },
        "go_live_gate": gate,
        "expected_edge_bps": ctx.expected_edge_bps.map(|e| e.to_string()),
        "paper_only": true,
        "real_orders_enabled": false,
        "note": "SHADOW ONLY — built + validated the real order that would be sent, then the fail-closed sender refused dispatch. No network call, no real money. Real dispatch needs the go_live_gate satisfied AND a real sender wired (absent in this build). When 'basket' is present this is ONE LEG of a NegRisk arb: the leg validating in isolation does NOT mean the basket was executable, which additionally needs every sibling leg to clear and to fill simultaneously (P5).",
    });
    // Insert `basket` only for real baskets — do NOT emit it as JSON null for directional orders.
    // Postgres treats JSONB null as a VALUE, so `payload->'basket' IS NOT NULL` is TRUE for a null
    // literal: the obvious "find the arb legs" query would silently scoop up every directional row
    // too (observed 2026-07-30, one directional order polluting an arb-leg query). Omitting the key
    // makes `payload ? 'basket'` and `IS NOT NULL` agree.
    if let Some(basket) = &ctx.basket {
        payload["basket"] = basket.clone();
    }
    let _ = journal
        .record_journal_event("clob_shadow_order", "polytrader_shadow", "info", payload)
        .await;
    tracing::info!(market = %gamma_id, outcome = %outcome, basket = ctx.basket.is_some(),
        "shadow real order recorded (fail-closed; nothing sent)");
}

/// Periodic arbitrage scan, journaled as an 'arb_scan' event for Hermes closed-loop learning.
///
/// Scans all active markets for YES+NO best-ask sums below $1.00 (net of taker fees) — risk-free
/// "missing probability" opportunities. The result is journaled (count + top opportunities) so the
/// Hermes meta-agent can track how often arb appears, how rich it is, and recommend execution wiring.
///
/// RISK (paper-only): snapshot-based identification ONLY. Real arb requires simultaneous fills via
/// live feeds; nothing here submits orders. Decimal exclusively; append-only journal write.
async fn produce_arb_scan_journal(
    pool: &sqlx::PgPool,
    journal: &Arc<JournalWriter>,
    paper_engine: &Arc<PaperTradingEngine>,
    live_books: &crate::ingester::clob_ws::LiveBookStore,
) -> anyhow::Result<()> {
    let scanner = ArbitrageScanner::with_default_fees();
    let (opps, diag) = scanner.scan_with_diagnostics(pool).await?;
    let best_net = opps.first().map(|o| o.net_profit_per_unit.to_string());
    // Sanity-check log: distinguishes "efficient market, no arb" (usable_books healthy, best_total_cost
    // > $1) from "scanner starved" (markets_scanned/usable_books ~0 → missing/stale snapshots).
    tracing::info!(
        markets_scanned = diag.markets_scanned,
        usable_books = diag.usable_books,
        sub_dollar = diag.sub_dollar_books,
        near_miss = diag.near_miss_books,
        net_arbs = diag.net_arb_books,
        best_total_cost = ?diag.best_total_cost,
        "arb scan diagnostics"
    );
    let payload = serde_json::json!({
        "strategy": "arbitrage_missing_probability",
        "opportunity_count": opps.len(),
        "best_net_profit_per_unit": best_net,
        "top_opportunities": opps.iter().take(5).collect::<Vec<_>>(),
        "diagnostics": diag,
        "paper_only": true,
        "real_orders_enabled": false,
        "note": "Periodic arb scan (YES+NO best-ask sum < $1 after fees) journaled for Hermes closed-loop. Snapshot-based; this is the ONLY trade type allowed on sports markets. 'diagnostics' disambiguates a zero opportunity_count: low usable_books = scanner starved (snapshot gap); healthy usable_books with best_total_cost > $1 = efficient market (no arb)."
    });
    let id = journal
        .record_journal_event("arb_scan", "polytrader_arb_scanner", "info", payload)
        .await?;
    tracing::debug!(id = %id, count = opps.len(), "arb_scan journaled");

    // Execute the best few arbs (gated). Arbitrage is risk-free on price and applies to ANY market,
    // including the arb-only sports markets the directional executor skips.
    let autonomous = std::env::var("POLYTRADER_AUTONOMOUS_PAPER_EXECUTION")
        .unwrap_or_default()
        .trim()
        .to_lowercase()
        == "on";
    if autonomous {
        for opp in opps.iter().take(3) {
            if let Err(e) = execute_arb_opportunity(pool, journal, paper_engine, opp).await {
                warn!(error = %e, market = %opp.market_id, "arb execution error (paper-only; continue)");
            }
        }
    }

    // NegRisk EVENT-level scan: buy-all-No baskets across a mutually-exclusive event (at most one
    // member resolves Yes ⇒ k No-shares pay ≥ k−1). This is where real dislocations live — the
    // single-market Yes+No scan above was measured structurally dead (best cost pinned at $1.001).
    match crate::strategy::negrisk::scan_negrisk(pool, Some(live_books)).await {
        Ok((nopps, ndiag)) => {
            tracing::info!(
                events_scanned = ndiag.events_scanned,
                member_books = ndiag.member_books,
                net_arbs = ndiag.net_arb_events,
                best_implied_yes_sum = ?ndiag.best_implied_yes_sum,
                best_line_shortfall = ?ndiag.best_line_shortfall,
                // The A/B: same events, same pass, snapshot prices only. If these two never
                // diverge, the 30-minute snapshot window was not the binding constraint and the
                // live feed bought this scanner nothing.
                best_line_shortfall_snapshot = ?ndiag.best_line_shortfall_snapshot,
                legs_live = ndiag.legs_from_live_book,
                legs_snapshot = ndiag.legs_from_snapshot,
                "negrisk arb scan diagnostics"
            );
            // MINIMUM BASKET EDGE. The scanner's own bar is MIN_NET_PROFIT — an ABSOLUTE $0.002 per
            // unit — which admits baskets whose return on locked collateral is negligible: the first
            // shadowed basket (2026-07-28, event 716159, 4 legs) came in at **29.1 bps** and earned
            // about a dollar. Of the 24 baskets executed to that date, the 8 at >=400bps produced
            // +$333.59 (+$41.70 avg) against +$22.03 (+$1.38 avg) for the 16 below — 33% of baskets,
            // 94% of the profit. That correlation is *partly mechanical* (P&L ~ units x
            // net-profit-per-unit), so it is NOT evidence that fat baskets are better predicted; it
            // is evidence about what a spread has to be to survive execution. A 57% guaranteed
            // spread absorbs a mis-filled leg; a 0.29% spread is destroyed by one tick of slippage,
            // and a partial fill degrades the payout floor to filled_legs-1 (see
            // execute_negrisk_opportunity).
            //
            // Gating here rather than inside the executor keeps "which baskets do we trade" at the
            // routing layer, and lets one scan record report both what cleared and what did not —
            // per-basket rejection events would re-fire every 5-minute cycle for as long as a
            // dislocation persists. `below_min_edge` is the tuning signal: raise the floor only
            // against observed edges, not against a guess.
            let min_basket_edge_bps = std::env::var("POLYTRADER_ARB_MIN_BASKET_EDGE_BPS")
                .ok()
                .and_then(|v| v.trim().parse::<rust_decimal::Decimal>().ok())
                .unwrap_or(dec!(0));
            let (tradeable, below_floor): (Vec<_>, Vec<_>) = nopps.iter().partition(|o| {
                negrisk_basket_edge_bps(o.net_profit_per_unit, o.total_cost) >= min_basket_edge_bps
            });
            let payload = serde_json::json!({
                "strategy": "negrisk_event_arbitrage",
                "opportunity_count": nopps.len(),
                "best_net_profit_per_unit": nopps.first().map(|o| o.net_profit_per_unit.to_string()),
                "top_opportunities": nopps.iter().take(3).collect::<Vec<_>>(),
                "diagnostics": ndiag,
                "min_basket_edge_bps": min_basket_edge_bps.to_string(),
                "below_min_edge_count": below_floor.len(),
                "best_below_min_edge_bps": below_floor
                    .first()
                    .map(|o| negrisk_basket_edge_bps(o.net_profit_per_unit, o.total_cost).to_string()),
                "paper_only": true,
                "real_orders_enabled": false,
                "note": "Event-level negRisk scan (buy No across k mutually-exclusive members pays >= k-1; arb when implied Yes probs sum over 100%). Works on partial event coverage, so it scans the books the universe already ingests. Legs are priced from the LIVE WebSocket book where one is fresh and in sync (diagnostics.legs_from_live_book), falling back to the polled snapshot otherwise. Read best_implied_yes_sum against best_arb_line, never against 1.00 — fees move the line. best_line_shortfall_snapshot is the same metric on snapshot prices alone, so the value of the live feed is visible in-process."
            });
            let _ = journal
                .record_journal_event(
                    "negrisk_arb_scan",
                    "polytrader_arb_scanner",
                    "info",
                    payload,
                )
                .await;
            if autonomous {
                for opp in tradeable.iter().take(2) {
                    if let Err(e) =
                        execute_negrisk_opportunity(pool, journal, paper_engine, opp).await
                    {
                        warn!(error = %e, event = %opp.event_id, "negrisk arb execution error (paper-only; continue)");
                    }
                }
            }
        }
        Err(e) => warn!(error = %e, "negrisk arb scan failed (will retry next cycle)"),
    }

    // MAKER LIQUIDITY-REWARDS SHADOW SCAN. Measures only — places nothing, and there is no executor
    // behind it. Polymarket pays a per-market daily budget to RESTING orders whether or not they
    // fill; this estimates what we could capture and journals it so the estimate accrues a track
    // record against real book dynamics before any of it becomes a strategy. Two caveats travel
    // with every number and are repeated in the payload: the share model is first-order (an upper
    // bound), and adverse selection — the entire risk of making — is not modelled at all.
    match crate::strategy::rewards::scan_rewards(pool, Some(live_books)).await {
        Ok((candidates, rdiag)) => {
            tracing::info!(
                markets_with_rewards = rdiag.markets_with_rewards,
                markets_priced = rdiag.markets_priced,
                pool_usd_day = %rdiag.pool_usd_day,
                est_capturable_usd_day = %rdiag.estimated_capturable_usd_day,
                capital_required_usd = %rdiag.capital_required_usd,
                priced_from_live = rdiag.priced_from_live,
                best = ?candidates.first().map(|c| (&c.question, c.estimated_daily_usd.to_string())),
                "maker rewards shadow scan"
            );
            let _ = journal
                .record_journal_event(
                    "maker_rewards_scan",
                    "polytrader_rewards_scanner",
                    "info",
                    serde_json::json!({
                        "strategy": "maker_liquidity_rewards",
                        "diagnostics": rdiag,
                        "top_candidates": candidates.iter().take(10).collect::<Vec<_>>(),
                        "paper_only": true,
                        "orders_placed": false,
                        "note": "SHADOW ONLY — no orders are placed and no executor exists. estimated_daily_usd is an UPPER BOUND: the share model is our_size/(qualifying_depth+our_size), while Polymarket scores by proximity to mid and generally rewards two-sided quoting. Adverse selection is NOT modelled — a resting order earns rewards precisely because it can be hit, and it is hit when the market moves against it. A positive number here is not a positive-expectancy claim. Rank is by capturable, never by pool size: measured 2026-08-02, a $1000/day pool against 1.94M shares of competing depth yields ~$0.10/day while a $200/day pool against 4.4k shares yields ~$4.49."
                    }),
                )
                .await;

            // P5 increment 3b — SHADOW MAKER QUOTES. Still places nothing, but unlike the scan
            // above this follows specific prices THROUGH TIME, which is the only way to reach the
            // two numbers the snapshot estimator structurally cannot produce: the duty cycle (the
            // scan annualises an instantaneous share as if a quote qualified all day) and adverse
            // selection (the scan's own docs call it "the entire risk of making" and model none of
            // it). Netting horizon_pnl against accrued_reward is the first honest read on whether
            // making pays here at all.
            match crate::strategy::maker::track_shadow_quotes(pool, Some(live_books), &candidates)
                .await
            {
                Ok(mdiag) => {
                    tracing::info!(
                        open_quotes = mdiag.open_quotes,
                        placed = mdiag.placed,
                        filled = mdiag.filled,
                        horizons_measured = mdiag.horizons_measured,
                        accrued_reward_usd = %mdiag.accrued_reward_usd,
                        horizon_pnl_usd = %mdiag.horizon_pnl_usd,
                        duty_cycle_pct = %mdiag.duty_cycle_pct,
                        unpriced = mdiag.unpriced,
                        fills_overdue = mdiag.fills_overdue,
                        fills_abandoned = mdiag.fills_abandoned,
                        "maker shadow quotes"
                    );
                    let _ = journal
                        .record_journal_event(
                            "maker_shadow_quotes",
                            "polytrader_maker_shadow",
                            "info",
                            serde_json::json!({
                                "strategy": "maker_liquidity_rewards",
                                "diagnostics": mdiag,
                                "paper_only": true,
                                "orders_placed": false,
                                "net_usd": (mdiag.accrued_reward_usd + mdiag.horizon_pnl_usd).round_dp(4).to_string(),
                                "note": "SHADOW ONLY — virtual quotes tracked against the live book; nothing is placed and no position is taken. accrued_reward_usd integrates the re-observed share per interval rather than annualising one instant. horizon_pnl_usd marks each filled quote ONE HOUR after the fill, not at the fill: the fill trigger IS the mid crossing our price, so an instant mark would report the trigger back as a finding. The fill rule (mid crosses the quote) UNDER-counts fills, so adverse selection is under-counted and net_usd is OPTIMISTIC. Read a positive net as 'not yet falsified', not as proven."
                            }),
                        )
                        .await;
                }
                Err(e) => {
                    warn!(error = %e, "maker shadow quote tracking failed (will retry next cycle)")
                }
            }
        }
        Err(e) => warn!(error = %e, "maker rewards scan failed (will retry next cycle)"),
    }
    Ok(())
}

/// P5 increment 3 — classify a basket's pre-flight prediction against what actually happened.
///
/// The four cells are not symmetric and the asymmetry is the whole decision:
///
/// - `predicted_complete` — pre-flight passed, basket filled. The plan held; nothing to learn.
/// - `preflight_missed` — pre-flight passed, basket came up SHORT. The book moved between the plan
///   and the commit. Irreducible: no pre-flight removes it, and its rate is the honest ceiling on
///   what this increment can achieve.
/// - `would_have_prevented` — pre-flight said abort, basket did come up short. The benefit, at the
///   measured -$2.57/event a partial costs.
/// - `would_have_forgone` — pre-flight said abort but the basket filled COMPLETELY anyway. The
///   cost, at +$12.78/event. Because that is ~5x the benefit per event, enforcement is only correct
///   if this cell is much rarer than `would_have_prevented` — which is exactly what shadow mode is
///   deployed to find out, and why the default is not `enforce`.
/// Whether a basket that pre-flight wants to ABORT should be let through anyway, as a measurement
/// holdout.
///
/// Enforcement has an epistemic cost that is easy to miss: an aborted basket commits nothing, so it
/// never produces an outcome to compare the plan against, so `would_have_forgone` — the one cell
/// that argues against enforcing — becomes **structurally unobservable**. Turning enforcement on
/// therefore switches off the only measurement that could show enforcement is wrong. That is exactly
/// the shape of mistake the pre-registered P5 criterion exists to prevent, and it applies to
/// operational settings too.
///
/// So a small fraction of would-abort baskets execute anyway and get classified normally. It is the
/// same reasoning that kept directional trading as a control arm at `ROTATION_LIMIT=5` rather than
/// switching it off: keep the regime observable, at a cost bounded to a trickle.
///
/// Cost, measured: ~12 aborts per 10h at ~$2.57 of expected partial loss each, so a 10% holdout
/// risks roughly **$0.31 per 10 hours** to keep the false-abort rate measurable. That is a very
/// cheap insurance premium against silently running a rule that has stopped being right.
///
/// Selection is a hash of the event id, not an RNG, so the decision is deterministic rather than
/// flickering between abort and holdout on retries of the same scan.
///
/// **This is necessary but NOT sufficient — see `recently_held_out`.** Determinism across retries is
/// what we want; determinism across repeated SCANS is not, and the first version of this had no way
/// to tell them apart. A chronically unfillable event is re-scanned every 5 minutes, hashes the same
/// way every time, and so was pinned permanently into the holdout arm: observed live 2026-08-10,
/// event 756280 held out 3 times in 10 minutes, committing $81.13 cumulatively into a basket already
/// known to be broken. Worse, it corrupted the very sample the holdout exists to protect — those
/// three observations are one event, so a "0 of 3" false-abort rate was really 0 of 1.
fn is_preflight_holdout(event_id: &str, holdout_pct: u32) -> bool {
    if holdout_pct == 0 {
        return false;
    }
    if holdout_pct >= 100 {
        return true;
    }
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    event_id.hash(&mut h);
    (h.finish() % 100) < holdout_pct as u64
}

/// How long an event stays ineligible for another holdout after being held out once.
///
/// Bounds both failure modes of a purely deterministic selection: the capital repeatedly committed
/// into one known-broken basket, and the fake independence of counting the same event N times in the
/// false-abort denominator. Six hours is comfortably longer than the 5-minute scan cadence that
/// caused the problem, while still letting a genuinely recurring event contribute more than one
/// observation across a multi-day sample.
const HOLDOUT_COOLDOWN_HOURS: i64 = 6;

/// Whether this event has already been held out recently, making it ineligible for another.
///
/// Fails CLOSED (returns true, i.e. "treat as recently held out, so abort normally") when the query
/// errors: the safe direction is to enforce, since a failed lookup should not become a licence to
/// commit capital into a basket pre-flight wants to reject.
async fn recently_held_out(pool: &sqlx::PgPool, event_id: &str) -> bool {
    let q = format!(
        "SELECT EXISTS (
             SELECT 1 FROM journal.events
             WHERE event_type = 'autonomous_negrisk_arb_execution'
               AND (payload->>'preflight_holdout')::bool IS TRUE
               AND payload->>'event_id' = $1
               AND created_at > now() - interval '{HOLDOUT_COOLDOWN_HOURS} hours')"
    );
    sqlx::query_scalar::<_, bool>(&q)
        .bind(event_id)
        .fetch_one(pool)
        .await
        .unwrap_or(true)
}

fn classify_preflight_outcome(executable: bool, complete: bool) -> &'static str {
    match (executable, complete) {
        (true, true) => "predicted_complete",
        (true, false) => "preflight_missed",
        (false, false) => "would_have_prevented",
        (false, true) => "would_have_forgone",
    }
}

/// Execute a NegRisk buy-all-No basket in paper: buy `units` No shares in every leg. At most one
/// member of the event resolves Yes, so the basket pays at least (legs−1)×units at resolution —
/// risk-free on price, same snapshot caveats as the two-leg arb. Paper-only; journaled.
/// Pure: units for a negrisk basket = thinnest-leg depth bound, further bounded so the basket's
/// total collateral (units × Σ leg asks) stays within the basket collateral cap. Degenerate
/// non-positive cost → 0 (callers also guard).
fn negrisk_basket_units(
    max_units: rust_decimal::Decimal,
    collateral_cap: rust_decimal::Decimal,
    total_cost: rust_decimal::Decimal,
) -> rust_decimal::Decimal {
    if total_cost <= dec!(0) {
        return dec!(0);
    }
    max_units.min(collateral_cap / total_cost).round_dp(2)
}

/// A NegRisk basket's expected edge in basis points = return on the collateral needed to assemble
/// it (net guaranteed profit per unit ÷ cost per unit). Fed to the real-order dry-run's edge gate.
/// Pure so the bps scaling is pinned by test — an off-by-100 here would silently pass or fail the
/// 400bps real-order blocker. Non-positive cost → 0 (callers guard, but never divide by zero).
fn negrisk_basket_edge_bps(
    net_profit_per_unit: rust_decimal::Decimal,
    total_cost: rust_decimal::Decimal,
) -> rust_decimal::Decimal {
    if total_cost <= dec!(0) {
        return dec!(0);
    }
    (net_profit_per_unit / total_cost * dec!(10000)).round_dp(1)
}

async fn execute_negrisk_opportunity(
    pool: &sqlx::PgPool,
    journal: &Arc<JournalWriter>,
    engine: &Arc<PaperTradingEngine>,
    opp: &crate::strategy::negrisk::NegRiskOpportunity,
) -> anyhow::Result<()> {
    if opp.total_cost <= dec!(0) || opp.legs.len() < 2 {
        return Ok(());
    }
    // No pyramiding the same basket: if EVERY leg already holds No shares we already own (at least)
    // one complete basket for this event — re-buying each scan cycle while the books stay mispriced
    // would pyramid. A partial hold does NOT block: the incremental complete basket is still
    // guaranteed-profitable on top of whatever we hold (mirrors the two-leg guard's logic).
    let market_ids: Vec<String> = opp.legs.iter().map(|l| l.market_id.clone()).collect();
    let held_no_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM paper_trading.paper_positions
         WHERE market_id = ANY($1) AND outcome = 'No' AND shares > 0",
    )
    .bind(&market_ids)
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    if held_no_count as usize == opp.legs.len() {
        return Ok(());
    }

    // Sizing on WORST-CASE LOSS, not notional (2026-07-17, operator-approved): a COMPLETE
    // buy-all-No basket's capital-at-risk is NOT its notional — mutual exclusivity guarantees a
    // payout of (legs−1) per unit, so the resolution worst case is the guaranteed spread (a PROFIT
    // ≥ MIN_NET_PROFIT per unit). The notional is collateral temporarily locked, not money that
    // can be lost. The generic $250 ARB_NOTIONAL_CAP therefore over-restricted the one
    // consistently-paying strategy (checkpoint #11: the Musk ladder was cap-bound and still paid
    // +$15.40). Baskets now wear their own collateral cap, POLYTRADER_ARB_MAX_BASKET_COLLATERAL
    // (default $750 = 3× the old bound), still bounded by (a) the thinnest leg's book depth via
    // max_units — deeper size than the book shows simply doesn't fill — and (b) the residual risk
    // that DOES scale with size: a PARTIAL fill (some legs unfilled) degrades the payout floor to
    // filled_legs−1, journaled as basket_partial_or_unfilled below. Accepted at paper scale; real
    // money needs P5 simultaneous multi-leg fills first (roadmap). The two-leg YES+NO executor
    // keeps the generic cap — same principle applies there but it is not the proven earner; see
    // the roadmap TODO before scaling it too.
    let basket_collateral_cap = std::env::var("POLYTRADER_ARB_MAX_BASKET_COLLATERAL")
        .ok()
        .and_then(|v| v.trim().parse::<rust_decimal::Decimal>().ok())
        .unwrap_or(dec!(750));
    let units = negrisk_basket_units(opp.max_units, basket_collateral_cap, opp.total_cost);
    if units <= dec!(0) {
        return Ok(());
    }

    // The arb's edge belongs to the BASKET, not to any single leg — no individual leg is
    // "mispriced"; the guaranteed spread only exists once all legs are held. So every leg carries
    // the basket's return on collateral as its expected edge.
    let basket_edge_bps = negrisk_basket_edge_bps(opp.net_profit_per_unit, opp.total_cost);

    // P5 increment 3 — build every leg's order UP FRONT so the whole basket can be pre-flighted
    // against all its books at one instant, before a single leg is committed. The loop below then
    // executes an already-vetted plan instead of discovering leg by leg that the basket is not
    // going to complete.
    let orders: Vec<crate::paper::PaperOrder> = opp
        .legs
        .iter()
        .map(|leg| crate::paper::PaperOrder {
            id: uuid::Uuid::new_v4(),
            market_id: leg.market_id.clone(),
            outcome: "No".to_string(),
            side: crate::paper::OrderSide::Buy,
            order_type: crate::paper::OrderType::Limit,
            limit_price: Some(leg.ask_no),
            size: units,
            status: crate::paper::OrderStatus::Open,
            created_at: chrono::Utc::now(),
            decision_context: Some(serde_json::json!({
                "source": "autonomous_negrisk_arb_executor",
                "strategy": "negrisk_event_arbitrage",
                "event_id": opp.event_id,
                "basket_legs": opp.legs.len(),
                "basket_total_cost": opp.total_cost.to_string(),
                "net_profit_per_unit": opp.net_profit_per_unit.to_string(),
                "paper_only": true,
                "real_orders_enabled": false,
            })),
        })
        .collect();

    let plan = match engine.plan_basket(&orders).await {
        Ok(p) => Some(p),
        Err(e) => {
            // A pre-flight that cannot run must not silently become a pre-flight that passes.
            // Fall through to the old sequential behaviour and say so — degrading to the previous
            // (worse but understood) path beats both blocking the strategy and pretending we
            // checked.
            warn!(event = %opp.event_id, error = %e, "basket pre-flight failed; executing unvetted");
            None
        }
    };

    // Shipped shadow-first, exactly as the WS feed itself did (`POLYTRADER_ENABLE_CLOB_WS=shadow`),
    // because the abort is never worse FOR A GIVEN BASKET but its opportunity cost was unmeasured:
    // a pre-flight that is too strict rejects baskets that would have filled, and forgoing
    // +$12.78/event costs ~5x the -$2.57/event it saves.
    //
    // 2026-08-09: the deployment flipped to `enforce` after that rate was read off real data —
    // 42 of 43 correct with `would_have_forgone` EMPTY across 22 abort opportunities (see the
    // deploy yaml for the full table). The code default stays `shadow`: an unset or misspelled
    // value must fall back to the mode that cannot forgo a profitable basket, never to the one that
    // can. Enforcement is a deployment decision, not a code default.
    let preflight_mode = std::env::var("POLYTRADER_BASKET_PREFLIGHT")
        .unwrap_or_else(|_| "shadow".to_string())
        .trim()
        .to_lowercase();
    let enforcing = preflight_mode == "enforce";
    let would_abort = plan.as_ref().is_some_and(|p| !p.is_executable());
    // Keep the false-abort rate measurable under enforcement — see `is_preflight_holdout`.
    let holdout_pct = std::env::var("POLYTRADER_BASKET_PREFLIGHT_HOLDOUT_PCT")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(10);
    // Both conditions are required: the hash makes the decision deterministic across retries, the
    // cooldown stops a re-scanned event from being pinned in the holdout arm forever.
    let holdout = would_abort
        && enforcing
        && is_preflight_holdout(&opp.event_id, holdout_pct)
        && !recently_held_out(pool, &opp.event_id).await;
    if holdout {
        info!(event = %opp.event_id, holdout_pct,
            "negrisk basket would ABORT but is a measurement holdout — executing to keep \
             would_have_forgone observable");
    }

    if let Some(p) = plan.as_ref() {
        info!(
            event = %opp.event_id, legs = opp.legs.len(), short_legs = p.short_legs(),
            worst_fill_ratio = %p.worst_fill_ratio().round_dp(4),
            executable = p.is_executable(), mode = %preflight_mode,
            "negrisk basket pre-flight"
        );
    }

    if would_abort && enforcing && !holdout {
        let p = plan.as_ref().expect("would_abort implies a plan exists");
        info!(event = %opp.event_id, short_legs = p.short_legs(),
            "negrisk basket aborted by pre-flight (nothing committed)");
        let _ = journal
            .record_journal_event(
                "autonomous_negrisk_arb_execution",
                "polytrader_arb_executor",
                "info",
                serde_json::json!({
                    "action": "basket_aborted_preflight",
                    "event_id": opp.event_id,
                    "legs": opp.legs.len(),
                    "short_legs": p.short_legs(),
                    "worst_fill_ratio": p.worst_fill_ratio().round_dp(4).to_string(),
                    "units": units.to_string(),
                    "capital_not_committed": p.committed_cost_if_executed().round_dp(2).to_string(),
                    "short_leg_detail": p.short_leg_detail(),
                    "note": "P5 increment 3: at least one leg could not fill in full, so the >= legs-1 payout floor was unreachable. Nothing was committed — the alternative is holding a directional position where an arb was intended.",
                    "paper_only": true,
                }),
            )
            .await;
        return Ok(());
    }

    let mut filled_legs = 0usize;
    let mut total_filled_cost = dec!(0);
    // Deferred until every leg has been committed — see the shadow call below the loop for why.
    let mut to_shadow: Vec<(usize, String, uuid::Uuid, bool)> = Vec::with_capacity(opp.legs.len());
    // The commit window: first leg submitted to last leg committed. This is the interval over which
    // the pre-flight's one-instant book read has to remain true, so it is the number that bounds how
    // well pre-flight can possibly work. Journaled so the effect of moving the exchange round-trip
    // out of this loop is verifiable rather than assumed.
    let commit_started = std::time::Instant::now();
    for (leg_index, (leg, order)) in opp.legs.iter().zip(orders).enumerate() {
        // The order is the one the pre-flight actually vetted, not a rebuild of it — a second
        // construction here could drift from the plan (a different limit, a different size) and the
        // pre-flight would then be vouching for an order that was never sent.
        let order_id = order.id;
        let leg_filled = match engine.submit_order(order).await {
            Ok(fills) if !fills.is_empty() => {
                filled_legs += 1;
                total_filled_cost += fills
                    .iter()
                    .map(|f| f.price * f.size)
                    .sum::<rust_decimal::Decimal>();
                true
            }
            Ok(_) => {
                warn!(event = %opp.event_id, market = %leg.market_id, "negrisk leg unfilled (stale/thin book)");
                false
            }
            Err(e) => {
                warn!(event = %opp.event_id, market = %leg.market_id, error = %e, "negrisk leg submit failed");
                false
            }
        };

        to_shadow.push((leg_index, leg.market_id.clone(), order_id, leg_filled));
    }
    let commit_window_ms = commit_started.elapsed().as_millis() as u64;

    // Shadow the REAL orders these legs would send (2026-07-28) — AFTER every leg is committed, not
    // between them (moved 2026-08-09).
    //
    // `shadow_real_order` makes a network round-trip to the live exchange (`dry_run_order_intent`)
    // plus a token lookup and a journal write. Awaiting that BETWEEN legs put it directly in the
    // window the pre-flight is trying to make instantaneous, and the cost was measured, not
    // theoretical: over 24h the fill window (first leg to last) averaged **16.4s** for baskets that
    // completed and **151.6s** for the ones that came up short despite passing pre-flight — a 9x
    // difference, with a 302s worst case. A pre-flight that reads every book at one instant is
    // worthless if the commits it authorises then straggle over two and a half minutes.
    //
    // Causality is not fully separable here (a struggling leg may be slow BECAUSE it is not filling,
    // rather than failing because it was slow) and n=2 on the broken side. The change is made anyway
    // because it is safe in both readings: the shadow is an audit artifact with $0 risk and nothing
    // downstream of it affects the fill, so there is no reason for it to sit in the critical path.
    // Every leg is still shadowed, filled or not — an unfilled leg is itself the finding.
    for (leg_index, market_id, order_id, leg_filled) in to_shadow {
        shadow_real_order(
            pool,
            journal,
            &market_id,
            "No",
            "buy",
            "limit",
            units,
            opp.legs[leg_index].ask_no,
            &order_id.to_string(),
            &ShadowOrderContext {
                expected_edge_bps: Some(basket_edge_bps),
                basket: Some(serde_json::json!({
                    "strategy": "negrisk_event_arbitrage",
                    "event_id": opp.event_id,
                    "legs": opp.legs.len(),
                    "leg_index": leg_index,
                    "leg_filled_in_paper": leg_filled,
                    "units": units.to_string(),
                    "basket_total_cost": opp.total_cost.to_string(),
                    "basket_collateral": (units * opp.total_cost).round_dp(2).to_string(),
                    "net_profit_per_unit": opp.net_profit_per_unit.to_string(),
                    "min_payout_per_unit": opp.min_payout.to_string(),
                })),
            },
        )
        .await;
    }
    let complete = filled_legs == opp.legs.len();
    info!(event = %opp.event_id, legs = opp.legs.len(), filled_legs, %units, complete,
        commit_window_ms, "autonomous negrisk arb executed (paper-only)");

    // Plan-vs-actual. This is the measurement that decides whether pre-flight is worth enforcing,
    // and it splits four ways:
    //   - predicted_complete   : pre-flight passed AND the basket filled → the plan held.
    //   - preflight_missed     : pre-flight PASSED but the basket came up short → the book moved
    //                            between the plan and the commit. This is the irreducible residual
    //                            that no amount of pre-flight removes, and its rate is the honest
    //                            ceiling on what increment 3 can deliver.
    //   - would_have_prevented : pre-flight said abort and the basket did indeed come up short →
    //                            in shadow mode, a partial we chose to take anyway; the saving
    //                            enforcement would have realised.
    //   - would_have_forgone   : pre-flight said abort but the basket filled COMPLETELY anyway →
    //                            the opportunity cost of enforcing, at +$12.78/event the number
    //                            that could make this change net-negative. This is precisely why
    //                            the default is shadow.
    let preflight_outcome = plan
        .as_ref()
        .map(|p| classify_preflight_outcome(p.is_executable(), complete));
    let _ = journal
        .record_journal_event(
            "autonomous_negrisk_arb_execution",
            "polytrader_arb_executor",
            "info",
            serde_json::json!({
                "action": if complete { "basket_filled" } else { "basket_partial_or_unfilled" },
                "event_id": opp.event_id,
                "legs": opp.legs.len(),
                "filled_legs": filled_legs,
                "units": units.to_string(),
                "basket_total_cost": opp.total_cost.to_string(),
                "total_filled_cost": total_filled_cost.to_string(),
                "min_payout_per_unit": opp.min_payout.to_string(),
                "net_profit_per_unit": opp.net_profit_per_unit.to_string(),
                "note": "Buy-all-No negRisk basket. A partial fill (filled_legs < legs) weakens the >= legs-1 payout floor toward filled_legs-1 — flagged via action; still bounded loss (paper).",
                "preflight_mode": preflight_mode,
                "preflight_executable": plan.as_ref().map(|p| p.is_executable()),
                "preflight_short_legs": plan.as_ref().map(|p| p.short_legs()),
                "preflight_worst_fill_ratio": plan
                    .as_ref()
                    .map(|p| p.worst_fill_ratio().round_dp(4).to_string()),
                "preflight_outcome": preflight_outcome,
                // First leg submitted to last leg committed. The pre-flight reads every book at one
                // instant; this is how long that reading has to stay true, so it bounds how well
                // pre-flight can work at all. Measured over 24h BEFORE the exchange round-trip was
                // moved out of this loop: 16.4s average for baskets that completed, 151.6s for those
                // that came up short despite passing, 302s worst case.
                "commit_window_ms": commit_window_ms,
                // True when pre-flight wanted to abort and this basket was let through anyway to
                // keep the measurement alive. Under `enforce`, `would_have_forgone` and
                // `would_have_prevented` can ONLY come from holdouts — every other would-abort
                // basket is stopped before it produces an outcome. So the false-abort rate must be
                // read as (forgone / (forgone + prevented)) over rows where this is true, NOT over
                // all rows, or it will look far better than it is.
                "preflight_holdout": holdout,
                // WHICH BOUND ACTUALLY BINDS. `units` is min(depth bound, collateral bound), and
                // without recording both there is no way to answer "would raising the cap have
                // helped?" — the sizing question that matters most on fixture days.
                //
                // Why it matters (measured 2026-08-11): the $1500 cap binds on only ~7% of baskets,
                // which is why it was dismissed on FREQUENCY as "not the constraint". Weighted by
                // P&L the conclusion inverts — 6 cap-bound baskets produced **$443.99, 23% of all
                // realized P&L**, because the cap binds precisely on the largest opportunities.
                // Raising it is still NOT obviously right: units would only grow if depth allowed,
                // and partial-fill residual risk scales with size (the one $803 `preflight_missed`
                // was a 779-unit basket). These two fields make that a measurement rather than an
                // argument — compare `max_units_depth_bound` against `units` on cap-bound rows.
                "max_units_depth_bound": opp.max_units.to_string(),
                "collateral_cap_used": basket_collateral_cap.to_string(),
                "bound_by": if units >= opp.max_units { "depth" } else { "collateral_cap" },
                // Recorded on the EXECUTION event, not only on the abort event, because the abort
                // event never fires in shadow mode — and shadow mode is where the learning happens.
                // Without this the shadow run reports THAT a leg was short but never WHICH, and
                // `had_book` is the field that separates "the book is thin at our limit" (a market
                // condition, size down or widen) from "we never ingested this market" (an ingest
                // gap, and a fix that belongs upstream of the executor entirely).
                "preflight_short_leg_detail": plan
                    .as_ref()
                    .filter(|p| !p.is_executable())
                    .map(|p| p.short_leg_detail()),
                "paper_only": true,
                "real_orders_enabled": false,
            }),
        )
        .await;
    Ok(())
}

/// Execute a two-leg risk-free arbitrage in paper: buy YES at its ask AND buy NO at its ask, so the
/// $1 resolution payout exceeds the combined cost no matter the outcome. Bounded by available depth
/// and a notional cap; no pyramiding (skip if already holding the market). Paper-only; journaled.
async fn execute_arb_opportunity(
    pool: &sqlx::PgPool,
    journal: &Arc<JournalWriter>,
    engine: &Arc<PaperTradingEngine>,
    opp: &crate::strategy::arbitrage::ArbitrageOpportunity,
) -> anyhow::Result<()> {
    if opp.total_cost <= dec!(0) {
        return Ok(());
    }
    // No pyramiding the SAME arb: skip only if we already hold BOTH legs (a complete YES+NO pair) —
    // that's an arb we already captured, and re-buying it every scan cycle while the book stays
    // mispriced would pyramid. A DIRECTIONAL single-side hold must NOT block a risk-free arb, though:
    // the incremental YES+NO pair is guaranteed-profitable on top of whatever else we hold. (This was
    // the bug that made us skip the 2026-07-01 0.968 arb on market 616902 because we held a legacy
    // directional No there.)
    let (yes_held, no_held): (rust_decimal::Decimal, rust_decimal::Decimal) = sqlx::query_as(
        "SELECT COALESCE(SUM(shares) FILTER (WHERE outcome = 'Yes'), 0),
                COALESCE(SUM(shares) FILTER (WHERE outcome = 'No'), 0)
         FROM paper_trading.paper_positions WHERE market_id = $1 AND shares > 0",
    )
    .bind(&opp.market_id)
    .fetch_one(pool)
    .await
    .unwrap_or((dec!(0), dec!(0)));
    if yes_held > dec!(0) && no_held > dec!(0) {
        return Ok(());
    }

    // Pairs to buy: bounded by available depth and a notional cap. Arbitrage is RISK-FREE on price
    // (one of YES/NO always resolves to $1), so it should NOT wear the directional per-position cap
    // ($20) — that throttled real arbs badly (e.g. a 2026-06-19 $0.90 book offered ~$27 of risk-free
    // profit at $270 depth, but the $20 cap captured only ~$2). Default raised to $250, env-tunable.
    // CAVEAT for any future REAL trading: snapshot-based legs can mis-fill (one leg fills, the other
    // doesn't), turning a "risk-free" arb into a directional position — so a large real-money cap needs
    // simultaneous-fill (WebSocket) execution first. Paper-only here; fills are deterministic.
    let arb_notional_cap = std::env::var("POLYTRADER_ARB_NOTIONAL_CAP")
        .ok()
        .and_then(|v| v.trim().parse::<rust_decimal::Decimal>().ok())
        .unwrap_or(dec!(250));
    let depth_pairs = opp.max_size_usdc / opp.total_cost;
    let cap_pairs = arb_notional_cap / opp.total_cost;
    let pairs = depth_pairs.min(cap_pairs).round_dp(2);
    if pairs <= dec!(0) {
        return Ok(());
    }

    let mk_leg = |outcome: &str, ask: rust_decimal::Decimal| crate::paper::PaperOrder {
        id: uuid::Uuid::new_v4(),
        market_id: opp.market_id.clone(),
        outcome: outcome.to_string(),
        side: crate::paper::OrderSide::Buy,
        order_type: crate::paper::OrderType::Limit,
        limit_price: Some(ask), // limit = the known best ask; fills that level after the sort fix
        size: pairs,
        status: crate::paper::OrderStatus::Open,
        created_at: chrono::Utc::now(),
        decision_context: Some(serde_json::json!({
            "source": "autonomous_arb_executor",
            "strategy": "arbitrage_missing_probability",
            "total_cost": opp.total_cost.to_string(),
            "net_profit_per_unit": opp.net_profit_per_unit.to_string(),
            "paper_only": true,
            "real_orders_enabled": false,
        })),
    };

    let yes_fills = engine.submit_order(mk_leg("Yes", opp.ask_yes)).await?;
    let no_fills = engine.submit_order(mk_leg("No", opp.ask_no)).await?;
    let yes_filled: rust_decimal::Decimal = yes_fills.iter().map(|f| f.size).sum();
    let no_filled: rust_decimal::Decimal = no_fills.iter().map(|f| f.size).sum();
    let both_legs = yes_filled > dec!(0) && no_filled > dec!(0);
    info!(market = %opp.market_id, pairs = %pairs, yes_filled = %yes_filled, no_filled = %no_filled, both_legs, "autonomous arb executed (paper-only)");
    let _ = journal
        .record_journal_event(
            "autonomous_arb_execution",
            "polytrader_arb_executor",
            "info",
            serde_json::json!({
                "action": if both_legs { "arb_filled_both_legs" } else { "arb_partial_or_unfilled" },
                "market_id": opp.market_id,
                "pairs": pairs.to_string(),
                "yes_filled_shares": yes_filled.to_string(),
                "no_filled_shares": no_filled.to_string(),
                "total_cost_per_pair": opp.total_cost.to_string(),
                "net_profit_per_unit": opp.net_profit_per_unit.to_string(),
                "both_legs_filled": both_legs,
                "note": "Two-leg paper arb (buy YES + buy NO). If only one leg fills, the result is a directional position, not arbitrage — flagged via both_legs_filled.",
                "paper_only": true,
                "real_orders_enabled": false,
            }),
        )
        .await;
    Ok(())
}

/// Whether a market is arb-only — the directional executor skips it; only execute_arb_opportunity
/// (risk-free YES+NO) may trade it. True if the slug is in the explicit `POLYTRADER_ARB_ONLY_MARKETS`
/// override list OR it matches a broad arb-eligible category.
///
/// EXPANDED 2026-06-29 from sports-only to the categories below. Edge-validation found the directional
/// engine net-NEGATIVE (−$77, 1W/7L over the clean settled set) while the only profitable structure
/// was a both-sides quasi-arb; so we route these categories away from directional prediction. (NOTE:
/// the arb scanner already scans ALL markets, so this does not widen arb's reach — it stops the
/// directional engine bleeding on markets where it has no demonstrated edge.)
fn is_arb_only_market(slug: &str) -> bool {
    let explicit = std::env::var("POLYTRADER_ARB_ONLY_MARKETS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .any(|s| !s.is_empty() && s == slug);
    explicit || arb_category(slug).is_some()
}

/// Classify a market slug into an arbitrage-eligible category, or None if it matches none (those few
/// stay on the directional path). Slug-based, deliberately broad, first-match-wins. Covers: sports,
/// esports, crypto, finance, economy, tech, geopolitics, elections, culture, weather.
fn arb_category(slug: &str) -> Option<&'static str> {
    let s = slug.to_lowercase();
    let has = |w: &[&str]| w.iter().any(|x| s.contains(x));
    // Prefix-only patterns: Polymarket's scheduled-game/match markets use league-prefixed slugs
    // ("wta-eala-swiatek-2026-07-03", "cs2-big5-nip-...", "val-rrq1-edg1-..."). These MUST be
    // prefix-anchored — as substrings they'd false-positive ("oval-office" contains "val-").
    // Added 2026-07-04 after the rotation job promoted 13 tennis/esports match markets that the
    // substring keywords missed (and the Pegula/Wimbledon market leaked a directional fill AGAIN).
    let pre = |w: &[&str]| w.iter().any(|x| s.starts_with(x));
    if pre(&[
        "wta-", "atp-", "mlb-", "nba-", "nhl-", "nfl-", "ufc-", "epl-", "ucl-", "cric", "crint-",
    ]) || has(&[
        "world-cup",
        "fifa",
        "fifwc",
        "nba",
        "nfl",
        "nhl",
        "mlb",
        "super-bowl",
        "-ufc",
        "soccer",
        "knicks",
        "-vs-",
        "champion",
        "playoff",
        "grand-prix",
        "premier-league",
        "-open-",
        "wimbledon",
        "tennis",
        "grand-slam",
        "cricket",
    ]) {
        Some("sports")
    } else if pre(&["cs2-", "csgo-", "val-", "lol-", "dota2-", "dota-", "ow-"])
        || has(&[
            "esports",
            "league-of-legends",
            "-dota",
            "valorant",
            "counter-strike",
            "-cs2",
            "overwatch",
            "starcraft",
            "rocket-league",
            "-msi-", // LoL Mid-Season Invitational (will-t1-win-msi-2026 leaked 2026-07-05)
        ])
    {
        Some("esports")
    } else if has(&[
        "bitcoin", "ethereum", "-btc-", "-eth-", "crypto", "solana", "dogecoin", "-xrp", "cardano",
        "memecoin", "-nft",
    ]) {
        Some("crypto")
    } else if has(&[
        "s-p-500",
        "sp500",
        "nasdaq",
        "-dow-",
        "stock",
        "share-price",
        "market-cap",
        "largest-company",
        "earnings",
        "-ipo",
        "treasury-yield",
    ]) {
        Some("finance")
    } else if has(&[
        "fed-rate",
        "rate-cut",
        "rate-hike",
        "interest-rate",
        "inflation",
        "recession",
        "-gdp",
        "unemployment",
        "jobs-report",
        "-cpi",
        "no-fed-rate",
    ]) {
        Some("economy")
    } else if has(&[
        "openai",
        "-gpt",
        "anthropic",
        "claude",
        "-grok",
        "nvidia",
        "-apple-",
        "google",
        "spacex",
        "semiconductor",
        "-chip",
        "ai-model",
        "-agi-",
        "self-driving",
    ]) {
        Some("tech")
    } else if has(&[
        "iran",
        "israel",
        "russia",
        "ukraine",
        "china",
        "taiwan",
        "gaza",
        "hormuz",
        "-war-",
        "ceasefire",
        "peace-deal",
        "nuclear",
        "sanction",
        "blockade",
        "tariff",
        "-nato",
        "north-korea",
        "hezbollah",
        "netanyahu",
        "putin",
        "zelensky",
    ]) {
        Some("geopolitics")
    } else if has(&[
        "election",
        "-president",
        "prime-minister",
        "midterm",
        "nominee",
        "nomination",
        "-senate",
        "-house-",
        "governor",
        "parliament",
        "ballot",
        "referendum",
        "balance-of-power",
        "democratic",
        "republican",
    ]) {
        Some("elections")
    } else if has(&[
        // Polymarket "Mentions" category (0.04 fee tier): will-X-say-Y / tweet-count markets.
        "-say-",
        "of-tweets",
        "-tweets-",
        "tweet-count",
        "-mention",
    ]) {
        Some("mentions")
    } else if has(&[
        "oscar",
        "grammy",
        "emmy",
        "box-office",
        "-movie",
        "-album",
        "spotify",
        "celebrity",
        "time-person",
        "met-gala",
        "rotten-tomatoes",
        "billboard",
    ]) {
        Some("culture")
    } else if has(&[
        "temperature",
        "hurricane",
        "rainfall",
        "-snow",
        "heat-wave",
        "weather",
        "climate",
        "el-nino",
        "-degrees",
        "wildfire",
    ]) {
        Some("weather")
    } else {
        None
    }
}

/// Polymarket taker fee RATE for a market, by category (makers are never charged). Source:
/// docs.polymarket.com/trading/fees. **Geopolitics is fee-free.** The fee is NOT a flat % of notional —
/// see [`polymarket_taker_fee`] for the formula. Unclassified markets default to the "Other" rate.
fn polymarket_taker_fee_rate(slug: &str) -> rust_decimal::Decimal {
    match arb_category(slug) {
        Some("geopolitics") => dec!(0),
        Some("sports") | Some("esports") => dec!(0.03),
        Some("crypto") => dec!(0.07),
        Some("finance") | Some("tech") | Some("elections") | Some("mentions") => dec!(0.04),
        Some("economy") | Some("culture") | Some("weather") => dec!(0.05),
        _ => dec!(0.05), // Other / General
    }
}

/// Polymarket taker fee for one fill given an explicit per-market `rate`:
/// `shares × rate × price × (1 − price)`. The `price × (1 − price)` shape peaks at p=0.50, is zero at
/// the extremes, and is symmetric (a fill at 0.30 costs the same as at 0.70). Source:
/// docs.polymarket.com/trading/fees. Prefer the per-market `taker_fee_rate` synced from Gamma; fall back
/// to [`polymarket_taker_fee_rate`] (category default) when none is stored.
fn polymarket_fee(
    rate: rust_decimal::Decimal,
    price: rust_decimal::Decimal,
    shares: rust_decimal::Decimal,
) -> rust_decimal::Decimal {
    rate * shares * price * (rust_decimal::Decimal::ONE - price)
}

/// Fee using the CATEGORY-default rate (the fallback when no per-market rate is available). Prod paths
/// resolve the rate explicitly (stored `taker_fee_rate` else category) and call [`polymarket_fee`]; this
/// convenience is used by tests and as a reference.
#[allow(dead_code)]
fn polymarket_taker_fee(
    slug: &str,
    price: rust_decimal::Decimal,
    shares: rust_decimal::Decimal,
) -> rust_decimal::Decimal {
    polymarket_fee(polymarket_taker_fee_rate(slug), price, shares)
}

/// Recent absolute price move for an outcome: |current_mid − oldest_mid| over the last ~3h of
/// orderbook snapshots. Feeds the overreaction volatility guard (fade only NEW extremes, i.e. recent
/// sharp moves, not long-standing correctly-priced extremes). Returns 0 when there's no history yet.
/// SIGNED price move over ~3h (current − oldest). Positive = price rose, negative = fell. Callers
/// take `.abs()` for the overreaction volatility guard; the sign feeds spike_divergence (fade a spike
/// only when the orderbook now leans against it). Returns 0 when there's no history yet.
async fn compute_recent_move_signed(
    pool: &sqlx::PgPool,
    gamma_id: &str,
    outcome: &str,
    current_mid: rust_decimal::Decimal,
) -> rust_decimal::Decimal {
    let oldest: Option<rust_decimal::Decimal> = sqlx::query_scalar(
        "SELECT mid FROM market_data.orderbook_snapshots
         WHERE market_id = $1 AND outcome = $2 AND mid IS NOT NULL
           AND fetched_at >= now() - interval '3 hours'
         ORDER BY fetched_at ASC LIMIT 1",
    )
    .bind(gamma_id)
    .bind(outcome)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    match oldest {
        Some(o) => current_mid - o,
        None => dec!(0),
    }
}

/// Latest stored orderbook (bids, asks) for a market's outcome token — the jsonb level arrays
/// (`[{"price","size"}, ...]`). Feeds the orderbook_momentum + spike_divergence processors (and the
/// overreaction book-confirmation bonus). Returns empty arrays when no snapshot exists.
async fn fetch_latest_book(
    pool: &sqlx::PgPool,
    gamma_id: &str,
    outcome: &str,
) -> (serde_json::Value, serde_json::Value) {
    let row: Option<(serde_json::Value, serde_json::Value)> = sqlx::query_as(
        "SELECT bids, asks FROM market_data.orderbook_snapshots
         WHERE market_id = $1 AND outcome = $2
         ORDER BY fetched_at DESC LIMIT 1",
    )
    .bind(gamma_id)
    .bind(outcome)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    match row {
        Some((b, a)) => (b, a),
        None => (serde_json::json!([]), serde_json::json!([])),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        arb_category, drawdown_breaker_tripped, polymarket_taker_fee, settlement_payout_and_pnl,
        turnover_budget_exhausted,
    };
    use rust_decimal_macros::dec;

    #[test]
    fn polymarket_fee_matches_published_tables() {
        let shares = dec!(100);
        // Per docs.polymarket.com/trading/fees (100-share tables, peak at p=0.50):
        // Crypto $1.75, Sports $0.75, Geopolitics free.
        assert_eq!(
            polymarket_taker_fee("will-bitcoin-hit-150k", dec!(0.50), shares),
            dec!(1.75)
        );
        let sport = "will-france-win-the-fifa-world-cup";
        assert_eq!(polymarket_taker_fee(sport, dec!(0.50), shares), dec!(0.75));
        assert_eq!(
            polymarket_taker_fee("us-iran-nuclear-deal-by-june-30", dec!(0.50), shares),
            dec!(0)
        );
        // Symmetric around 0.50, and 0.30 < the 0.50 peak.
        assert_eq!(
            polymarket_taker_fee(sport, dec!(0.30), shares),
            polymarket_taker_fee(sport, dec!(0.70), shares)
        );
        assert!(
            polymarket_taker_fee(sport, dec!(0.30), shares)
                < polymarket_taker_fee(sport, dec!(0.50), shares)
        );
    }

    #[test]
    fn arb_category_routes_real_bootstrap_slugs() {
        // Representative live slugs land in the expected arb categories (directional executor will skip).
        assert_eq!(
            arb_category("us-iran-nuclear-deal-by-june-30"),
            Some("geopolitics")
        );
        assert_eq!(
            arb_category("will-bitcoin-hit-150k-by-june-30-2026"),
            Some("crypto")
        );
        assert_eq!(
            arb_category("will-france-win-the-2026-fifa-world-cup-924"),
            Some("sports")
        );
        assert_eq!(
            arb_category("will-gavin-newsom-win-the-2028-democratic-presidential-nomination-568"),
            Some("elections")
        );
        assert_eq!(
            arb_category("will-no-fed-rate-cuts-happen-in-2026"),
            Some("economy")
        );
        // Prefix-anchored league/match slugs (the 2026-07-04 rotation leak): tennis + esports
        // game markets whose substring keywords missed.
        assert_eq!(arb_category("wta-eala-swiatek-2026-07-03"), Some("sports"));
        assert_eq!(
            arb_category("atp-khachan-cobolli-2026-07-04"),
            Some("sports")
        );
        assert_eq!(
            arb_category("will-jessica-pegula-be-the-2026-womens-wimbledon-winner"),
            Some("sports")
        );
        assert_eq!(arb_category("cs2-big5-nip-2026-07-04"), Some("esports"));
        assert_eq!(arb_category("val-rrq1-edg1-2026-07-04"), Some("esports"));
        assert_eq!(arb_category("lol-blg-t1-2026-07-04"), Some("esports"));
        // The 2026-07-05 rotation leaks: LoL MSI winner + Major League Cricket match slugs.
        assert_eq!(
            arb_category("will-t1-win-msi-2026-20260615160658468"),
            Some("esports")
        );
        assert_eq!(arb_category("cricmlc-was-san-2026-07-04"), Some("sports"));
        assert_eq!(arb_category("crint-gbr3-aus2-2026-07-05"), Some("sports"));
        // Prefixes must not fire as substrings ("oval-office" contains "val-").
        assert_eq!(arb_category("will-trump-leave-the-oval-office-early"), None);
        // A truly uncategorized slug stays directional.
        assert_eq!(arb_category("will-some-obscure-thing-happen"), None);
    }

    #[test]
    fn drawdown_breaker_gating_and_threshold() {
        // Disabled (no threshold configured) → never trips, even on a huge drawdown.
        assert!(!drawdown_breaker_tripped(dec!(99), None));
        // Configured at 15%: below → run, at/above → halt.
        assert!(!drawdown_breaker_tripped(dec!(14.99), Some(dec!(15))));
        assert!(drawdown_breaker_tripped(dec!(15), Some(dec!(15)))); // boundary halts
        assert!(drawdown_breaker_tripped(dec!(22.5), Some(dec!(15))));
        // At/near peak (0% drawdown) never trips a positive threshold.
        assert!(!drawdown_breaker_tripped(dec!(0), Some(dec!(15))));
    }

    #[test]
    fn negrisk_basket_units_bounded_by_depth_and_collateral() {
        use super::negrisk_basket_units;
        // Collateral cap binds: $750 cap / $2.85 basket cost = 263.15 units (depth allows more).
        assert_eq!(
            negrisk_basket_units(dec!(1000), dec!(750), dec!(2.85)),
            dec!(263.16)
        );
        // Depth binds when the book is thin.
        assert_eq!(
            negrisk_basket_units(dec!(50), dec!(750), dec!(2.85)),
            dec!(50)
        );
        // Degenerate cost → 0 (no divide-by-zero).
        assert_eq!(negrisk_basket_units(dec!(50), dec!(750), dec!(0)), dec!(0));
    }

    /// The two cells that decide whether pre-flight should be enforced are the ones where the
    /// prediction and the outcome DISAGREE, and they are easy to write backwards — the benefit case
    /// is (not executable, not complete) and the cost case is (not executable, complete), which
    /// differ by a single bool. Getting them the wrong way round would invert the enforcement
    /// decision while every number still looked plausible.
    /// The holdout is what keeps enforcement falsifiable, so its selection must be STABLE — an RNG
    /// would let a re-scanned basket flip between abort and holdout, biasing the very sample the
    /// revert decision rests on.
    #[test]
    fn the_preflight_holdout_is_stable_per_event_and_honours_its_bounds() {
        use super::is_preflight_holdout;
        // Same event, same answer, every time.
        let first = is_preflight_holdout("759351", 10);
        for _ in 0..50 {
            assert_eq!(is_preflight_holdout("759351", 10), first);
        }
        // 0 disables entirely (pure enforcement, measurement off); 100 holds everything out
        // (equivalent to shadow for would-abort baskets).
        assert!(!is_preflight_holdout("759351", 0));
        assert!(!is_preflight_holdout("anything", 0));
        assert!(is_preflight_holdout("759351", 100));
        assert!(is_preflight_holdout("anything", 100));

        // Roughly the requested rate across many distinct events. Loose bounds: this asserts the
        // knob is wired to reality, not that a hash is uniform.
        let n = 4000;
        let held = (0..n)
            .filter(|i| is_preflight_holdout(&format!("event-{i}"), 10))
            .count();
        assert!(
            (200..600).contains(&held),
            "10% holdout over {n} events produced {held}, which is not near 10%"
        );

        // The property that made this insufficient on its own, pinned so it is not mistaken for a
        // bug later: determinism means a RE-SCANNED event selects identically every cycle, which is
        // why `recently_held_out` has to gate it. Live on 2026-08-10 that gap held one event out 3
        // times in 10 minutes, committing $81.13 into a basket already known to be broken and making
        // a 0-of-3 false-abort rate really 0 of 1.
        assert!(
            is_preflight_holdout("756280", 100) && is_preflight_holdout("756280", 100),
            "selection is deterministic per event; the cooldown, not the hash, is what stops \
             a re-scanned event being held out repeatedly"
        );
    }

    #[test]
    fn preflight_outcomes_name_the_benefit_and_the_cost_the_right_way_round() {
        use super::classify_preflight_outcome;
        // Agreement: the plan held, or the book moved under it.
        assert_eq!(classify_preflight_outcome(true, true), "predicted_complete");
        assert_eq!(classify_preflight_outcome(true, false), "preflight_missed");
        // Disagreement. Aborting a basket that WOULD have come up short is the saving...
        assert_eq!(
            classify_preflight_outcome(false, false),
            "would_have_prevented"
        );
        // ...and aborting one that would have filled fine is the loss. At +$12.78 forgone against
        // -$2.57 saved, this cell is ~5x as expensive per event as the one above is valuable.
        assert_eq!(
            classify_preflight_outcome(false, true),
            "would_have_forgone"
        );
    }

    #[test]
    fn negrisk_basket_edge_bps_scales_return_on_collateral() {
        use super::negrisk_basket_edge_bps;
        // The real Jul-26 Brazilian-football basket: $0.6718 guaranteed net on a $1.315 basket is a
        // 51% return on collateral — comfortably past the 400bps (4%) real-order edge floor.
        assert_eq!(
            negrisk_basket_edge_bps(dec!(0.67180125), dec!(1.315)),
            dec!(5108.8)
        );
        // A 1% return is 100bps — i.e. it would FAIL the 400bps gate, which is the point of
        // reporting the real number rather than leaving the check permanently red.
        assert_eq!(negrisk_basket_edge_bps(dec!(0.01), dec!(1)), dec!(100));
        // Degenerate cost → 0 (no divide-by-zero), mirroring negrisk_basket_units.
        assert_eq!(negrisk_basket_edge_bps(dec!(0.5), dec!(0)), dec!(0));
    }

    #[test]
    fn min_basket_edge_floor_separates_the_two_real_baskets() {
        use super::negrisk_basket_edge_bps;
        // The routing gate (POLYTRADER_ARB_MIN_BASKET_EDGE_BPS) partitions on this value, so pin it
        // against the two baskets we actually executed rather than invented numbers.
        //
        // The FIRST shadowed basket (2026-07-28, event 716159, 4 legs, all filled), inputs taken
        // verbatim from its clob_shadow_order journal rows. It cleared the scanner's ABSOLUTE
        // MIN_NET_PROFIT bar ($0.002/unit) on a $2.966 basket and earned about a dollar — exactly
        // the trade the floor exists to decline.
        let thin = negrisk_basket_edge_bps(dec!(0.00863570), dec!(2.966));
        assert_eq!(thin, dec!(29.1));
        // The Jul-26 Brazilian basket, 2nd-fattest of the 24 executed.
        let fat = negrisk_basket_edge_bps(dec!(0.67180125), dec!(1.315));

        // The deployed paper floor declines the thin one and keeps the fat one.
        assert!(thin < dec!(50) && fat >= dec!(50));
        // The 400bps real-money bar (8 of 24 baskets, 94% of realized profit) agrees on both.
        assert!(thin < dec!(400) && fat >= dec!(400));

        // The two bars are NOT interchangeable, and a real basket sits between them: event 748763
        // (2026-07-29) at 147.6bps would trade under the deployed paper floor and be declined under
        // the real-money bar. Paper keeps it deliberately — it is a live data point about the
        // middle of the distribution, and paper carries no partial-fill risk to price.
        let middling = negrisk_basket_edge_bps(dec!(0.04328385), dec!(2.933));
        assert_eq!(middling, dec!(147.6));
        assert!(middling >= dec!(50) && middling < dec!(400));

        // A floor of 0 disables the gate — every scanner-admitted basket still trades.
        assert!(thin >= dec!(0));
    }

    #[test]
    fn turnover_budget_counts_and_disables() {
        // Under budget → run; at/over → halt (P6: friction is per-trade, cap the day's entries).
        assert!(!turnover_budget_exhausted(5, 6));
        assert!(turnover_budget_exhausted(6, 6));
        assert!(turnover_budget_exhausted(7, 6));
        // 0 (or negative) disables the budget entirely.
        assert!(!turnover_budget_exhausted(100, 0));
        assert!(!turnover_budget_exhausted(100, -1));
    }

    #[test]
    fn settlement_winner_pays_one_per_share() {
        // Bought 100 shares for $20 (avg 0.20). Wins → payout $100, realized P&L +$80.
        let (payout, pnl) = settlement_payout_and_pnl(true, dec!(100), dec!(20));
        assert_eq!(payout, dec!(100));
        assert_eq!(pnl, dec!(80));
    }

    #[test]
    fn settlement_loser_pays_zero() {
        // Same position loses → payout $0, realized P&L −$20 (the full cost basis).
        let (payout, pnl) = settlement_payout_and_pnl(false, dec!(100), dec!(20));
        assert_eq!(payout, dec!(0));
        assert_eq!(pnl, dec!(-20));
    }
}
