//! Minimal Axum HTTP server + dashboard (Phase 2: real Dioxus SSR hydration of src/ui/app.rs rsx).
//! Routes: /health (root probes), /markets, /paper/portfolio, / (SSR from rsx + client live fetch reactivity).
//! Subpath <base> + rewrite compat + all Phase 0/1 behavior 100% preserved. No WASM assets (smallest).
//! No real trading endpoints. Paper-only observational.

use axum::{
    extract::State,
    response::{Html, IntoResponse, Json},
    routing::get,
    Router,
};
use dioxus::prelude::*;
use rust_decimal::Decimal;
use serde::Serialize;
use sqlx::PgPool;
use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::trace::TraceLayer;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    /// Normalized subpath prefix (e.g. "/polytrader"). Empty string means root deployment.
    pub subpath_prefix: String,
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
    mode: &'static str,
}

#[derive(Serialize, sqlx::FromRow)]
struct MarketRow {
    gamma_id: String,
    slug: String,
    question: String,
    last_mid_yes: Option<Decimal>,
    last_mid_no: Option<Decimal>,
    active: bool,
}

#[derive(Serialize, sqlx::FromRow)]
struct PortfolioSnapshot {
    as_of: chrono::DateTime<chrono::Utc>,
    virtual_usdc: Decimal,
    total_locked: Decimal,
    unrealized_pnl: Decimal,
    realized_pnl: Decimal,
}

pub async fn start_server(
    state: AppState,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let prefix = state.subpath_prefix.clone();

    // Routes that should always be available at the root for Kubernetes probes / internal use.
    // (Probes hit /health directly; ngrok policy with rewrite forwards stripped paths here.)
    let probe_routes = Router::new().route("/health", get(health_handler));

    // Main application routes mounted at clean root paths.
    // The SUBPATH_PREFIX (when set) is used *only* for <base href> in dashboard HTML
    // so browser links resolve to public /polytrader/* URLs; the rewrite rule in
    // the central NgrokTrafficPolicy then strips the prefix before forwarding to us.
    // This matches the recommended policy stanza (with url-rewrite) and "no bigger changes".
    let app_routes = Router::new()
        .route("/", get(dashboard_handler))
        .route("/markets", get(markets_handler))
        .route("/paper/portfolio", get(portfolio_handler))
        .layer(TraceLayer::new_for_http())
        .with_state(Arc::new(state));

    // Final router: always merge (health + UI at clean paths after any proxy rewrite).
    let app = probe_routes.merge(app_routes);

    let addr: SocketAddr = "0.0.0.0:8080"
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid listen addr: {}", e))?;

    tracing::info!(%addr, subpath_prefix = %prefix, "starting axum server");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("bind 8080 failed: {}", e))?;

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|e| anyhow::anyhow!("server error: {}", e))?;
    Ok(())
}

async fn health_handler() -> impl IntoResponse {
    Json(Health {
        status: "ok",
        mode: "paper",
    })
}

async fn markets_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let rows: Vec<MarketRow> = sqlx::query_as(
        "SELECT gamma_id, slug, question, last_mid_yes, last_mid_no, active
         FROM market_data.markets
         WHERE active = true
         ORDER BY updated_at DESC
         LIMIT 20",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();

    Json(rows)
}

async fn portfolio_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let snap: Option<PortfolioSnapshot> = sqlx::query_as(
        "SELECT as_of, virtual_usdc, total_locked, unrealized_pnl, realized_pnl
         FROM paper_trading.virtual_portfolio_snapshots
         ORDER BY as_of DESC
         LIMIT 1",
    )
    .fetch_optional(&state.pool)
    .await
    .unwrap_or(None);

    Json(snap.unwrap_or(PortfolioSnapshot {
        as_of: chrono::Utc::now(),
        virtual_usdc: rust_decimal::Decimal::from(10000u64),
        total_locked: rust_decimal::Decimal::ZERO,
        unrealized_pnl: rust_decimal::Decimal::ZERO,
        realized_pnl: rust_decimal::Decimal::ZERO,
    }))
}

async fn dashboard_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // Phase 2: Real Dioxus SSR render of the App rsx from src/ui/app.rs (now the authoritative
    // rendered source of truth, not a design reference). Smallest hydration: vdom rebuild + ssr,
    // plus base injection for subpath rewrite + <base> compat (preserves *exact* Phase 0/1
    // verified behavior for root /health probes, all JSON endpoints, public /polytrader/*).
    // No WASM bundle/asset serving (would require Dockerfile + build target changes = not smallest).
    // Client reactivity via the <script> rendered as part of the rsx (real fetch + card updates).
    // NOTE: trusted values only (no user content in this SSR path yet).
    let prefix = &state.subpath_prefix;
    let base = if prefix.is_empty() {
        "/".to_string()
    } else {
        format!("{}/", prefix)
    };

    // SSR the rsx App (structure + initial signals + embedded client script for live fetch)
    let mut vdom = VirtualDom::new(crate::ui::App);
    vdom.rebuild_in_place();
    let mut rendered = dioxus_ssr::render(&vdom);

    // Inject <base> for subpath (rsx App does not take props for it; this + wrapper keeps
    // smallest change + 100% compat with ngrok rewrite + prior verified public behavior).
    if let Some(head_pos) = rendered.find("<head>") {
        let insert_pos = head_pos + 6; // after <head>
        rendered.insert_str(insert_pos, &format!("<base href=\"{}\">", base));
    }
    if !rendered.contains("<base href") {
        tracing::warn!(prefix = %prefix, "subpath <base> injection may have failed (string post-process assumption); relative client fetches in SSR output may not resolve correctly under /polytrader rewrite");
    }

    // Full document wrapper (rsx provides head + body siblings)
    let html = format!(
        r#"<!doctype html>
<html>
{}
</html>"#,
        rendered
    );

    Html(html)
}
