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
mod ui; // Dioxus UI (Phase 2: rsx App now SSR-rendered source + client fetch reactivity; server uses real dioxus render, no mirror)

// Phase 3.2 smallest skeleton (wiki-first per plan/AGENTS; after all docs/decisions/log prepend).
// mod strategy declares the new artifact (FusionEngine + processors using exact existing patterns: rust_decimal, anyhow, tracing, journal attribution hooks, heavy risk comments, paper-only).
// No behavior change to any existing path; unused in this increment (full wiring in follow-ups). #[allow] inside strategy/mod.rs for clean clippy.
mod clob; // gated real/authenticated CLOB client (foundation for future order placement using derived L2 creds)
mod strategy;

use crate::config::Config;
use crate::db::create_pool;
use crate::ingester::{ingest_tick, ClobPublicClient, GammaClient};
use crate::journal::JournalWriter;
use crate::paper::PaperTradingEngine;
use crate::server::{start_server, AppState};
// 5-min DR cadence (additive; see wiki/strategies/goals-and-operational-cadence.md + strategy mod)
use crate::strategy::{DecisionReport, FeeContext, FusionEngine};
use rust_decimal_macros::dec;

#[tokio::main]
async fn main() -> Result<()> {
    // Very early fallback logging (in case tracing doesn't flush before fast exit)
    eprintln!("=== POLYTRADER MAIN ENTERED (pre-tracing) ===");

    // Structured logging (json for easy parsing by Hermes later)
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("info,polytrader=debug,sqlx=warn,axum=info,tower_http=debug")
    });

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
        fee_bps = cfg.paper_fee_bps,
        ingest_interval = cfg.ingest_interval_secs,
        bootstrap = ?cfg.bootstrap_market_list(),
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

    // Seed initial paper portfolio snapshot if none exists (uses config initial)
    seed_initial_portfolio_if_needed(&pool, cfg.initial_paper_usdc).await?;

    // === JOURNAL + ENGINE (shared) ===
    let journal = Arc::new(JournalWriter::new(pool.clone()));
    let _engine = Arc::new(PaperTradingEngine::new(
        pool.clone(),
        journal.clone(),
        cfg.paper_fee_bps,
    ));
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
        let interval = std::time::Duration::from_secs(300); // 5min per goals cadence
        tokio::spawn(async move {
            // initial immediately (so first reflection sees data)
            if let Err(e) = produce_5min_decision_report(&pool, &journal).await {
                warn!(error = %e, "initial 5min DR generation failed (will retry)");
            }
            loop {
                tokio::time::sleep(interval).await;
                if let Err(e) = produce_5min_decision_report(&pool, &journal).await {
                    warn!(error = %e, "periodic 5min DR generation failed (will retry)");
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

/// Minimal 5-min DR producer (called from dedicated spawn; reuses existing patterns).
/// Produces DecisionReport via fuse_net (net_edge_after_fees PRIMARY), journals 'decision_report' event (reuse jsonb).
/// No submit, no real, paper-only. See long RISK comment at spawn site.
async fn produce_5min_decision_report(
    pool: &sqlx::PgPool,
    journal: &Arc<JournalWriter>,
) -> anyhow::Result<()> {
    // Smallest/initial 5min DR generator (per plan/goals "skeleton vs production" + "separate future" + "orthogonal";
    // defers full ranked opportunities + risk/goal filters + change detection/rate limit per market to next when
    // "fuller generator active"). Hardcoded LIMIT + FeeContext + stub processors ok for limited wiring (can yield
    // sparse/zero DRs some ticks, as intended for skeleton; see strategy skeleton + non-overclaim in log/wiki).
    const DR_MARKET_LIMIT: i64 = 3;
    // Smallest: top few active markets with mids (same source as server strategy candidates).
    let markets: Vec<(
        String,
        Option<rust_decimal::Decimal>,
        Option<rust_decimal::Decimal>,
    )> = sqlx::query_as(
        "SELECT gamma_id, last_mid_yes, last_mid_no
         FROM market_data.markets
         WHERE active = true
           AND (last_mid_yes IS NOT NULL OR last_mid_no IS NOT NULL)
         ORDER BY updated_at DESC
         LIMIT $1",
    )
    .bind(DR_MARKET_LIMIT)
    .fetch_all(pool)
    .await?;

    let engine = FusionEngine::new();
    let fee_ctx = FeeContext {
        taker_bps: dec!(50), // conservative (see fees wiki; server uses env paper_fee_bps); DR_TAKER_BPS const for future alignment
        maker_bps: dec!(20),
        est_gas_usdc: dec!(0.01),
        rewards_offset_bps: dec!(10),
    };

    // Realistic fixed small virtual USD notional for costing (fixes notional misuse bug: target_mid is price ~[0,1],
    // not position size; passing it yielded only tiny fees ~0.01 instead of meaningful "net after fees" for
    // PRIMARY signal in DecisionReport/journaled 'decision_report'. Use dec!(10) for initial limited generator
    // ($10 virtual at $150 scale; conservative per goals). TODO: realistic position sizing (e.g. 1% bankroll or
    // from paper engine) + per-market liquidity in fuller generator (per goals "Ranked list... apply risk/goal
    // filters" + "skeleton vs production"); same pre-existing pattern in server.rs:7544 build_strategy_paper_candidates
    // (reuse when aligning). This ensures net_edge_after_fees is the "PRIMARY signal for deliberate 5-min tier".
    let realistic_notional = dec!(10);

    for (gamma_id, my, mn) in markets {
        let (target_outcome, target_mid) = if my.unwrap_or(dec!(0)) <= mn.unwrap_or(dec!(0)) {
            ("Yes", my.unwrap_or(dec!(0.5)))
        } else {
            ("No", mn.unwrap_or(dec!(0.5)))
        };
        // Skeleton choice (Issues 9/14 review): pick lower/equal-priced side as target for initial DR (arbitrary for limited wiring; fuller generator per goals "Ranked list of top opportunities" + multi-signal will use processor direction + filters). net_edge_after_fees remains PRIMARY signal regardless (per strategy/DecisionReport + fees wiki 4-6% min net); see long RISK at spawn + "limited (no full ranked... see goals)" non-overclaim.
        // Explicit bounds (Issue 15 review): mids from public ingester/market_data are trusted but validate 0..=1 at generator input boundary (PRIMARY net_edge signal); warn+skip malformed (additive defense; paper-only; no change to limited skeleton or "skeleton vs production"). See RISK at spawn.
        if !(target_mid >= dec!(0) && target_mid <= dec!(1)) {
            warn!(market = %gamma_id, mid = %target_mid, "invalid mid from market_data; skipping DR (robust)");
            continue;
        }
        let snapshot = serde_json::json!({
            "gamma_id": gamma_id,
            "target_outcome": target_outcome,
            "target_mid": target_mid.to_string(),
            "paper_only": true,
            "source": "5min_dr_generator"
        });
        let ctx = serde_json::json!({
            "paper_only": true,
            "tier": "5min_dr_cadence",
            "min_net_edge_for_trade": "0.04",
            "costing_notional": realistic_notional.to_string()
        });
        match engine.fuse_net(&snapshot, &ctx, Some(&fee_ctx), realistic_notional) {
            Ok((gross, net, attr)) => {
                let report = DecisionReport {
                    fused_gross_edge: gross,
                    net_edge_after_fees: net,
                    confidence: dec!(0.5),
                    attribution: attr,
                };
                let payload = serde_json::json!({
                    "report": {
                        "fused_gross_edge": report.fused_gross_edge.to_string(),
                        "net_edge_after_fees": report.net_edge_after_fees.to_string(),
                        "confidence": report.confidence.to_string(),
                        "attribution": report.attribution
                    },
                    "market_id": gamma_id,
                    "target_outcome": target_outcome,
                    "target_mid": target_mid.to_string(),
                    "generated_by": "5min_dr_cadence_in_main",
                    "paper_only": true,
                    "real_orders_enabled": false,
                    "note": "5-min DR per goals-and-operational-cadence.md + strategy/DecisionReport + fuse_net; net_edge_after_fees is PRIMARY signal for deliberate tier (4-6% min net per goals); journaled for Hermes clob_safety + reflections + future attribution (vs approvals/fills); no auto-submit (per goals optional behind flag). See wiki/strategies/goals-and-operational-cadence.md + decisions/real-order-approval-flow.md."
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
            }
            Err(e) => {
                warn!(error = %e, market = %gamma_id, "fuse_net failed for 5min DR (paper-only; degrade)");
            }
        }
    }
    Ok(())
}
