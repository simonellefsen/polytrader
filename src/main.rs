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
mod risk;
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

    // Seed initial paper portfolio snapshot if none exists (uses config initial)
    seed_initial_portfolio_if_needed(&pool, cfg.initial_paper_usdc).await?;

    // === JOURNAL + ENGINE (shared) ===
    let journal = Arc::new(JournalWriter::new(pool.clone()));
    let engine = Arc::new(PaperTradingEngine::new(
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
        let engine = engine.clone();
        let interval = std::time::Duration::from_secs(300); // 5min per goals cadence
        tokio::spawn(async move {
            // initial immediately (so first reflection sees data)
            if let Err(e) = produce_5min_decision_report(&pool, &journal, &engine).await {
                warn!(error = %e, "initial 5min DR generation failed (will retry)");
            }
            // Initial arb scan immediately so Hermes has data on first reflection.
            if let Err(e) = produce_arb_scan_journal(&pool, &journal, &engine).await {
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
                if let Err(e) = produce_arb_scan_journal(&pool, &journal, &engine).await {
                    warn!(error = %e, "periodic arb scan failed (will retry)");
                }
                fetch_and_journal_real_balance(&journal).await;
                if let Err(e) = settle_resolved_positions(&pool, &journal).await {
                    warn!(error = %e, "periodic settlement pass failed (will retry)");
                }
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

/// Write a mark-to-market portfolio snapshot: live unrealized P&L = Σ shares·(current_mid − avg_entry)
/// over open positions (current_mid from the latest cached market mids), realized carried forward.
/// This gives the /trades P&L chart a fresh, truthful data point each decision cycle (the per-fill
/// snapshots stored unrealized_pnl = 0). Paper-only; no wallet or CLOB call.
async fn write_mark_to_market_snapshot(pool: &sqlx::PgPool) -> Result<()> {
    let unrealized: rust_decimal::Decimal = sqlx::query_scalar::<_, rust_decimal::Decimal>(
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
    .unwrap_or(dec!(0));

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
    let fees: rust_decimal::Decimal =
        sqlx::query_scalar("SELECT COALESCE(SUM(fee), 0) FROM paper_trading.paper_fills")
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
async fn produce_5min_decision_report(
    pool: &sqlx::PgPool,
    journal: &Arc<JournalWriter>,
    paper_engine: &Arc<PaperTradingEngine>,
) -> anyhow::Result<()> {
    // Smallest/initial 5min DR generator (per plan/goals "skeleton vs production" + "separate future" + "orthogonal";
    // defers full ranked opportunities + risk/goal filters + change detection/rate limit per market to next when
    // "fuller generator active"). Hardcoded LIMIT + FeeContext + stub processors ok for limited wiring (can yield
    // sparse/zero DRs some ticks, as intended for skeleton; see strategy skeleton + non-overclaim in log/wiki).
    // Cover the full curated bootstrap set (was 3 — too few once we hold >10 markets).
    const DR_MARKET_LIMIT: i64 = 20;
    // Active markets with mids; slug drives arb-only routing + the newsdata query.
    // (gamma_id, slug, last_mid_yes, last_mid_no)
    type DrMarketRow = (
        String,
        String,
        Option<rust_decimal::Decimal>,
        Option<rust_decimal::Decimal>,
    );
    let markets: Vec<DrMarketRow> = sqlx::query_as(
        "SELECT gamma_id, slug, last_mid_yes, last_mid_no
         FROM market_data.markets
         WHERE active = true
           AND (last_mid_yes IS NOT NULL OR last_mid_no IS NOT NULL)
         ORDER BY updated_at DESC
         LIMIT $1",
    )
    .bind(DR_MARKET_LIMIT)
    .fetch_all(pool)
    .await?;

    // Load Hermes-learned processor weights (closed loop). Empty map → all 1.0 (neutral).
    let learned_weights = crate::strategy::load_processor_weights(pool).await;
    let engine = FusionEngine::with_weights(learned_weights);

    // External advisory signals (Yahoo Finance + Google News RSS), gated by env. Build one HTTP
    // client with a short timeout; fetches are best-effort and never block a decision.
    let external_enabled = std::env::var("POLYTRADER_EXTERNAL_SIGNALS")
        .unwrap_or_default()
        .trim()
        .to_lowercase()
        == "on";
    let http_client = reqwest::Client::builder()
        .user_agent("polytrader/0.1 (paper-only)")
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ok();

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

    for (gamma_id, slug, my, mn) in markets {
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
        // Recent price move for this outcome (volatility guard input): compare current mid to the
        // oldest mid in the last ~3h of snapshots. A large move = the extreme price is NEW (genuine
        // overreaction to fade); a stable extreme is likely correctly priced (no overreaction).
        let recent_move = compute_recent_move(pool, &gamma_id, target_outcome, target_mid).await;
        // External advisory context: Yahoo spot (free, every cycle) + newsdata.io news (metered —
        // 200 credits/day, so cached per market with a daily budget cap). Best-effort.
        let mut external = serde_json::Map::new();
        if let (true, Some(c)) = (external_enabled, &http_client) {
            if let Some(y) = crate::strategy::fetch_yahoo_context(c, &slug).await {
                external.insert("yahoo".to_string(), y);
            }
            // News only for directional markets — arb-only (sports) markets don't use it, so don't
            // spend newsdata credits on them.
            if !is_arb_only_market(&slug) {
                if let Some(news) =
                    get_news_context_cached(pool, journal, c, &gamma_id, &slug).await
                {
                    external.insert("news".to_string(), news);
                }
            }
        }
        let external = serde_json::Value::Object(external);
        let snapshot = serde_json::json!({
            "gamma_id": gamma_id,
            "target_outcome": target_outcome,
            "target_mid": target_mid.to_string(),
            "recent_move": recent_move.to_string(),
            "external": external,
            "paper_only": true,
            "source": "5min_dr_generator"
        });
        let ctx = serde_json::json!({
            "paper_only": true,
            "tier": "5min_dr_cadence",
            "min_net_edge_for_trade": crate::risk::RiskConfig::from_env().min_net_edge.to_string(),
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

                // Kelly sizing recommendation (for Hermes attribution; no auto-submit).
                // win_prob ≈ target_mid + net_edge (crude but directionally correct).
                // RISK: this is a rough estimate; always apply fractional Kelly + caps.
                let portfolio_usdc = current_portfolio_usdc(pool).await;
                let win_prob = (target_mid + net).min(dec!(0.99)).max(dec!(0.01));
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
            Err(e) => {
                warn!(error = %e, market = %gamma_id, "fuse_net failed for 5min DR (paper-only; degrade)");
            }
        }
    }
    Ok(())
}

/// Autonomous paper executor: take a passing Decision Report through the risk gate and, if approved
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
    // Arb-only markets (sports / World Cup): never take a directional bet — only the YES+NO
    // arbitrage executor may trade them. Skip here.
    if is_arb_only_market(slug) {
        tracing::debug!(slug = %slug, "arb-only market; directional executor skips");
        return Ok(());
    }
    if sizing.recommended_usdc <= dec!(0) || target_mid <= dec!(0) {
        return Ok(());
    }

    // No pyramiding: skip if we already hold this (market, outcome).
    let existing: rust_decimal::Decimal = sqlx::query_scalar(
        "SELECT COALESCE(shares, 0) FROM paper_trading.paper_positions WHERE market_id = $1 AND outcome = $2",
    )
    .bind(gamma_id)
    .bind(target_outcome)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .unwrap_or(dec!(0));
    if existing > dec!(0) {
        tracing::debug!(market = %gamma_id, outcome = %target_outcome, "already positioned; skipping autonomous entry");
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
        .check_pre_trade(pool, gamma_id, net_edge, sizing.recommended_usdc)
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

    // Recompute the portfolio snapshot with the updated cumulative realized P&L.
    let prev_realized: rust_decimal::Decimal = sqlx::query_scalar(
        "SELECT realized_pnl FROM paper_trading.virtual_portfolio_snapshots ORDER BY as_of DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?
    .unwrap_or(dec!(0));
    let new_realized = prev_realized + realized_delta;
    let locked: rust_decimal::Decimal = sqlx::query_scalar(
        "SELECT COALESCE(SUM(collateral_locked), 0) FROM paper_trading.paper_positions WHERE shares > 0",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(dec!(0));
    let fees: rust_decimal::Decimal =
        sqlx::query_scalar("SELECT COALESCE(SUM(fee), 0) FROM paper_trading.paper_fills")
            .fetch_one(pool)
            .await
            .unwrap_or(dec!(0));
    let new_usdc = (dec!(10000) - locked - fees + new_realized).max(dec!(0));
    sqlx::query(
        "INSERT INTO paper_trading.virtual_portfolio_snapshots
         (as_of, virtual_usdc, total_locked, unrealized_pnl, realized_pnl, snapshot_reason, positions)
         VALUES (now(), $1, $2, 0, $3, 'settlement', '[]'::jsonb)",
    )
    .bind(new_usdc)
    .bind(locked)
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
    // Settled positions = resolved-and-realized (paper_position_settled events).
    let settled: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM journal.events WHERE event_type = 'paper_position_settled'",
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
) {
    let token_id = resolve_token_id(pool, gamma_id, outcome).await;

    let intent = crate::clob::authenticated::RealOrderIntentDryRun {
        token_id: token_id.clone().unwrap_or_default(),
        side: side.to_string(),
        order_type: order_type.to_string(),
        size,
        price: Some(price),
        expected_edge_bps: None,
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

    let _ = journal
        .record_journal_event(
            "clob_shadow_order",
            "polytrader_shadow",
            "info",
            serde_json::json!({
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
                "paper_only": true,
                "real_orders_enabled": false,
                "note": "SHADOW ONLY — built + validated the real order that would be sent, then the fail-closed sender refused dispatch. No network call, no real money. Real dispatch needs the go_live_gate satisfied AND a real sender wired (absent in this build).",
            }),
        )
        .await;
    tracing::info!(market = %gamma_id, outcome = %outcome, "shadow real order recorded (fail-closed; nothing sent)");
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
) -> anyhow::Result<()> {
    let scanner = ArbitrageScanner::with_default_fees();
    let opps = scanner.scan(pool).await?;
    let best_net = opps.first().map(|o| o.net_profit_per_unit.to_string());
    let payload = serde_json::json!({
        "strategy": "arbitrage_missing_probability",
        "opportunity_count": opps.len(),
        "best_net_profit_per_unit": best_net,
        "top_opportunities": opps.iter().take(5).collect::<Vec<_>>(),
        "paper_only": true,
        "real_orders_enabled": false,
        "note": "Periodic arb scan (YES+NO best-ask sum < $1 after fees) journaled for Hermes closed-loop. Snapshot-based; this is the ONLY trade type allowed on sports markets."
    });
    let id = journal
        .record_journal_event("arb_scan", "polytrader_arb_scanner", "info", payload)
        .await?;
    tracing::debug!(id = %id, count = opps.len(), "arb_scan journaled");

    // Execute the best few arbs (gated). Arbitrage is risk-free on price and applies to ANY market,
    // including the arb-only sports markets the directional executor skips.
    if std::env::var("POLYTRADER_AUTONOMOUS_PAPER_EXECUTION")
        .unwrap_or_default()
        .trim()
        .to_lowercase()
        == "on"
    {
        for opp in opps.iter().take(3) {
            if let Err(e) = execute_arb_opportunity(pool, journal, paper_engine, opp).await {
                warn!(error = %e, market = %opp.market_id, "arb execution error (paper-only; continue)");
            }
        }
    }
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
    // No pyramiding: skip if we already hold any leg of this market.
    let held: rust_decimal::Decimal = sqlx::query_scalar(
        "SELECT COALESCE(SUM(shares), 0) FROM paper_trading.paper_positions WHERE market_id = $1 AND shares > 0",
    )
    .bind(&opp.market_id)
    .fetch_one(pool)
    .await
    .unwrap_or(dec!(0));
    if held > dec!(0) {
        return Ok(());
    }

    // Pairs to buy: bounded by depth (max_size_usdc / total_cost) and a $20 notional cap.
    const ARB_NOTIONAL_CAP: rust_decimal::Decimal = dec!(20);
    let depth_pairs = opp.max_size_usdc / opp.total_cost;
    let cap_pairs = ARB_NOTIONAL_CAP / opp.total_cost;
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

/// Whether a market slug is in the arb-only set (POLYTRADER_ARB_ONLY_MARKETS) — sports / World Cup.
/// The directional executor skips these; only execute_arb_opportunity may trade them.
fn is_arb_only_market(slug: &str) -> bool {
    std::env::var("POLYTRADER_ARB_ONLY_MARKETS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim())
        .any(|s| !s.is_empty() && s == slug)
}

/// News context for a market via newsdata.io, with aggressive credit economy (free plan = 200/day).
///
/// Strategy: cache each market's news in `journal.events` (event_type `news_cache`) with a 2h TTL.
/// - Fresh cache (<2h) → reuse, 0 credits.
/// - Stale + under daily budget (180/day, headroom below the 200 cap) → fetch 1 credit, cache it.
/// - Stale + budget exhausted → fall back to the most recent stale cache (or no news).
///
/// Returns the `news` JSON object (or None). NEWSDATA_API_KEY comes from the environment (k8s secret).
async fn get_news_context_cached(
    pool: &sqlx::PgPool,
    journal: &Arc<JournalWriter>,
    client: &reqwest::Client,
    gamma_id: &str,
    slug: &str,
) -> Option<serde_json::Value> {
    const TTL: &str = "2 hours";
    const DAILY_CAP: i64 = 180;

    // 1. Fresh cache?
    let fresh: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT payload->'news' FROM journal.events
         WHERE event_type = 'news_cache' AND payload->>'market_id' = $1
           AND created_at > now() - interval '2 hours'
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(gamma_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    if let Some(n) = fresh {
        if n.is_object() {
            return Some(n);
        }
    }

    let api_key = std::env::var("NEWSDATA_API_KEY").unwrap_or_default();
    if api_key.trim().is_empty() {
        return None;
    }

    // 2. Daily budget: each news_cache write == 1 credit spent. Stay under the 200/day free cap.
    let used_today: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM journal.events WHERE event_type = 'news_cache' AND created_at > now() - interval '24 hours'",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(0);

    if used_today >= DAILY_CAP {
        // Budget exhausted — reuse the most recent stale cache rather than spend a credit.
        let stale: Option<serde_json::Value> = sqlx::query_scalar(
            "SELECT payload->'news' FROM journal.events
             WHERE event_type = 'news_cache' AND payload->>'market_id' = $1
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(gamma_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();
        return stale.filter(|n| n.is_object());
    }

    // 3. Fetch (1 credit) + cache.
    let query = crate::strategy::newsdata_query(slug);
    let (count, polarity, titles) =
        crate::strategy::fetch_newsdata_news(client, api_key.trim(), &query).await?;
    let news = serde_json::json!({
        "query": query,
        "headline_count": count,
        "polarity": polarity.to_string(),
        "top_titles": titles,
        "provider": "newsdata.io",
    });
    let _ = journal
        .record_journal_event(
            "news_cache",
            "newsdata_fetch",
            "info",
            serde_json::json!({
                "market_id": gamma_id,
                "ttl": TTL,
                "credits_used_today_before": used_today,
                "news": news,
            }),
        )
        .await;
    Some(news)
}

/// Recent absolute price move for an outcome: |current_mid − oldest_mid| over the last ~3h of
/// orderbook snapshots. Feeds the overreaction volatility guard (fade only NEW extremes, i.e. recent
/// sharp moves, not long-standing correctly-priced extremes). Returns 0 when there's no history yet.
async fn compute_recent_move(
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
        Some(o) => (current_mid - o).abs(),
        None => dec!(0),
    }
}

#[cfg(test)]
mod tests {
    use super::settlement_payout_and_pnl;
    use rust_decimal_macros::dec;

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
