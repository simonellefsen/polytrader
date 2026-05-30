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
