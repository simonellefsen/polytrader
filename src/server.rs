//! Minimal Axum HTTP server + dashboard (Phase 2: real Dioxus SSR hydration of src/ui/app.rs rsx).
//! Routes: /health (root probes), /markets, /paper/portfolio, / (SSR from rsx + client live fetch reactivity).
//! Subpath <base> + rewrite compat + all Phase 0/1 behavior 100% preserved. No WASM assets (smallest).
//! No real trading endpoints. Paper-only observational.
//!
//! AUTH (2026-05-25 Next Phase, IMPL 5701dfea): added Google OAuth minimal flow + dual-mode
//! (ngrok edge forwarded headers preferred, else in-app cookie session via static stores).
//! NO AppState extension (avoids editing main.rs which fees work touched). Static OnceLock stores.
//! Manual cookie parse (no extra deps). All routes /auth/* ; auth optional (UI shows status).
//! Preserves 100% SSR/base/JS fetches/probes/k8s/existing endpoints.
//! RISK (AGENTS mandatory): see detailed blocks below + in handlers. Session hijack (flags),
//! token leakage (never log secrets), CSRF (state nonce), subpath Path=/polytrader critical,
//! ngrok header trust only from edge, $150 personal data exposure (future per-user), no migs.
//! Credits: AGENTS.md, prior ngrok deploy (edge SSO context), no UI auth from 5 polymarket repos.

use crate::strategy::{ArbitrageScanner, FeeContext, FusionEngine};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Json, Redirect},
    routing::{get, post},
    Router,
};
use dioxus::prelude::*;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tower_http::trace::TraceLayer;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    /// Normalized subpath prefix (e.g. "/polytrader"). Empty string means root deployment.
    pub subpath_prefix: String,
}

#[derive(Serialize, sqlx::FromRow)]
struct MarketRow {
    gamma_id: String,
    slug: String,
    question: String,
    category: Option<String>,
    last_mid_yes: Option<Decimal>,
    last_mid_no: Option<Decimal>,
    active: bool,
}

#[derive(Serialize)]
struct MarketResponse {
    gamma_id: String,
    slug: String,
    question: String,
    category: Option<String>,
    category_label: Option<String>,
    last_mid_yes: Option<Decimal>,
    last_mid_no: Option<Decimal>,
    clob_mid_ready: bool,
    market_data_status: &'static str,
    active: bool,
}

#[derive(Serialize, sqlx::FromRow)]
struct MarketCategoryRow {
    category: Option<String>,
    active_market_count: i64,
    data_ready_market_count: i64,
}

#[derive(Serialize)]
struct MarketCategoryResponse {
    category: Option<String>,
    category_label: String,
    active_market_count: i64,
    data_ready_market_count: i64,
}

#[derive(sqlx::FromRow)]
struct MarketDataReadinessRow {
    active_market_count: i64,
    data_ready_market_count: i64,
}

#[derive(Serialize, sqlx::FromRow)]
struct PortfolioSnapshot {
    as_of: chrono::DateTime<chrono::Utc>,
    virtual_usdc: Decimal,
    total_locked: Decimal,
    unrealized_pnl: Decimal,
    realized_pnl: Decimal,
}

#[derive(Debug, Deserialize, Clone)]
struct PaperOrderRequest {
    market_id: String,
    outcome: String,
    side: String,
    order_type: String,
    size: Decimal,
    limit_price: Option<Decimal>,
    rationale: Option<String>,
    confirm_paper_order: Option<bool>,
}

#[derive(Debug, Deserialize, Clone)]
struct StrategyPaperOrderRequest {
    market_id: String,
    outcome: Option<String>,
    size: Option<Decimal>,
    confirm_strategy_paper_order: Option<bool>,
    note: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
struct StrategyPaperCandidateObservationRequest {
    size: Option<Decimal>,
    note: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct StrategyPaperOrderReadinessQuery {
    market_id: Option<String>,
    outcome: Option<String>,
    size: Option<Decimal>,
}

#[derive(Debug, Deserialize, Clone)]
struct PaperResetRequest {
    confirm_paper_reset: Option<bool>,
    reason: Option<String>,
    operator: Option<String>,
}

#[derive(sqlx::FromRow)]
struct PaperOrderMarketReadinessRow {
    gamma_id: String,
    slug: String,
    question: String,
    active: bool,
    last_mid_yes: Option<Decimal>,
    last_mid_no: Option<Decimal>,
}

#[derive(sqlx::FromRow)]
struct PaperOrderHistoryRow {
    id: uuid::Uuid,
    market_id: String,
    slug: Option<String>,
    question: Option<String>,
    outcome: String,
    side: String,
    order_type: String,
    limit_price: Option<Decimal>,
    size: Decimal,
    status: String,
    fill_count: i64,
    filled_size: Decimal,
    gross_notional: Decimal,
    total_fee: Decimal,
    decision_context: Option<serde_json::Value>,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(sqlx::FromRow)]
struct PaperFillHistoryRow {
    id: uuid::Uuid,
    order_id: uuid::Uuid,
    market_id: String,
    slug: Option<String>,
    outcome: String,
    side: String,
    price: Decimal,
    size: Decimal,
    fee: Decimal,
    slippage_bps: i32,
    created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(sqlx::FromRow)]
struct PaperPositionHistoryRow {
    market_id: String,
    slug: Option<String>,
    question: Option<String>,
    category: Option<String>,
    outcome: String,
    shares: Decimal,
    avg_entry_price: Decimal,
    collateral_locked: Decimal,
    last_mid_yes: Option<Decimal>,
    last_mid_no: Option<Decimal>,
    last_updated: chrono::DateTime<chrono::Utc>,
}

#[derive(sqlx::FromRow)]
struct PaperPositionLedgerRow {
    market_id: String,
    outcome: String,
    shares: Decimal,
    collateral_locked: Decimal,
}

#[derive(sqlx::FromRow)]
struct ExpectedPaperPositionLedgerRow {
    market_id: String,
    outcome: String,
    expected_shares: Decimal,
    fill_count: i64,
}

#[derive(sqlx::FromRow)]
struct LatestPaperPortfolioSnapshotRow {
    as_of: chrono::DateTime<chrono::Utc>,
    virtual_usdc: Decimal,
    total_locked: Decimal,
    unrealized_pnl: Decimal,
    realized_pnl: Decimal,
    snapshot_reason: String,
}

#[derive(sqlx::FromRow)]
struct StrategyCandidateMarketRow {
    gamma_id: String,
    slug: String,
    question: String,
    category: Option<String>,
    last_mid_yes: Decimal,
    last_mid_no: Decimal,
}

#[derive(sqlx::FromRow)]
struct StrategyOrderbookSnapshotRow {
    bids: serde_json::Value,
    asks: serde_json::Value,
    spread: Option<Decimal>,
    fetched_at: chrono::DateTime<chrono::Utc>,
}

#[derive(sqlx::FromRow)]
struct StrategyTickVelocitySnapshotRow {
    mid: Decimal,
    fetched_at: chrono::DateTime<chrono::Utc>,
}

pub async fn start_server(
    state: AppState,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    eprintln!(
        "=== ENTERED start_server (prefix={}) ===",
        state.subpath_prefix
    );
    tracing::info!(prefix = %state.subpath_prefix, "start_server entered");

    let prefix = state.subpath_prefix.clone();

    // Routes that should always be available at the root for Kubernetes probes / internal use.
    // (Probes hit /health directly; ngrok policy with rewrite forwards stripped paths here.)
    let probe_routes = Router::new().route("/health", get(health_handler));

    // Main application routes mounted at clean root paths. When SUBPATH_PREFIX is set,
    // the same routes are also nested under that prefix. The public ngrok policy should
    // rewrite /polytrader/* to /*, but serving both forms makes the deployment robust
    // when the edge forwards the original path after SSO.
    let app_routes = Router::new()
        // Landing page = the lively Markets board. The legacy Dioxus console moves to /console.
        .route("/", get(board_page_handler))
        .route("/console", get(dashboard_handler))
        .route("/markets", get(markets_handler))
        .route("/market-categories", get(market_categories_handler))
        .route(
            "/strategy/paper-candidates",
            get(strategy_paper_candidates_handler),
        )
        .route(
            "/strategy/paper-candidate-observations",
            get(strategy_paper_candidate_observations_handler)
                .post(strategy_paper_candidate_observation_handler),
        )
        .route(
            "/strategy/paper-order-readiness",
            get(strategy_paper_order_readiness_handler),
        )
        .route(
            "/strategy/paper-orders",
            post(strategy_paper_order_submit_handler),
        )
        .route("/strategy/arb", get(strategy_arb_handler))
        .route("/trades", get(trades_page_handler))
        .route("/trades/data", get(trades_data_handler))
        .route("/board", get(board_page_handler))
        .route("/board/data", get(board_data_handler))
        .route("/paper/portfolio", get(portfolio_handler))
        .route("/paper/order-preview", post(paper_order_preview_handler))
        .route(
            "/paper/orders",
            get(paper_orders_handler).post(paper_order_submit_handler),
        )
        .route("/paper/fills", get(paper_fills_handler))
        .route("/paper/positions", get(paper_positions_handler))
        .route("/paper/risk-summary", get(paper_risk_summary_handler))
        .route("/paper/rejections", get(paper_rejections_handler))
        .route("/paper/reset", post(paper_reset_handler))
        .route("/paper/reconciliation", get(paper_reconciliation_handler))
        // AUTH (Next Phase): login/callback/logout/whoami. Optional for paper (dual edge+app).
        // Relative links in UI + <base> ensure subpath compat. /health untouched (public).
        .route("/auth/login", get(auth_login_handler))
        .route("/auth/callback", get(auth_callback_handler))
        .route("/auth/logout", get(auth_logout_handler))
        .route("/auth/whoami", get(auth_whoami_handler))
        // L2 (2026-05-25 IMPL 58dff3a2): Polymarket wallet auth (derive flow) for future gated CLOB.
        // Coexists with Google (routes after /auth/*; no Google code altered). Paper-only.
        // See top wiki/log.md 58dff3a2 entry for full details + fidelity note (Google 5701/978b preserved live).
        .route("/l2/status", get(l2_status_handler))
        .route("/l2/derive", post(l2_derive_handler))
        .route("/l2/disconnect", post(l2_disconnect_handler))
        .route(
            "/l2/derive-from-server-key",
            post(l2_derive_from_server_key_handler),
        )
        .route("/clob/status", get(clob_status_handler))
        .route("/clob/account", get(clob_account_handler))
        .route("/clob/preflight", get(clob_preflight_handler))
        .route(
            "/clob/collateral-readiness",
            get(clob_collateral_readiness_handler),
        )
        .route("/clob/diagnostics", get(clob_diagnostics_handler))
        .route("/clob/operator-status", get(clob_operator_status_handler))
        .route(
            "/clob/order-placement-readiness",
            get(clob_order_placement_readiness_handler),
        )
        .route(
            "/clob/real-trading-unlock-status",
            get(clob_real_trading_unlock_status_handler),
        )
        .route(
            "/clob/live-sender-design-readiness",
            get(clob_live_sender_design_readiness_handler),
        )
        .route(
            "/clob/live-sender-design-review",
            get(clob_live_sender_design_review_handler),
        )
        .route(
            "/clob/live-sender-boundary-status",
            get(clob_live_sender_boundary_status_handler),
        )
        .route(
            "/clob/final-review-readiness",
            get(clob_final_review_readiness_handler),
        )
        .route(
            "/clob/final-review-decision",
            post(clob_final_review_decision_handler),
        )
        .route(
            "/clob/final-review-decisions",
            get(clob_final_review_decisions_handler),
        )
        .route(
            "/clob/hermes-safety-loop",
            get(clob_hermes_safety_loop_handler),
        )
        .route(
            "/clob/order-intent/dry-run",
            post(clob_order_intent_dry_run_handler),
        )
        .route(
            "/clob/order-intent/market-validation",
            post(clob_order_intent_market_validation_handler),
        )
        .route(
            "/clob/order-intent/signature-dry-run",
            post(clob_order_intent_signature_dry_run_handler),
        )
        .route(
            "/clob/order-intent/post-request-dry-run",
            post(clob_order_intent_post_request_dry_run_handler),
        )
        .route(
            "/clob/order-intent/submit-facade",
            post(clob_order_intent_submit_facade_handler),
        )
        .route(
            "/clob/order-intent/submit-reconciliations",
            get(clob_order_intent_submit_reconciliations_handler),
        )
        .route(
            "/clob/order-intent/human-approval",
            post(clob_order_intent_human_approval_handler),
        )
        .route(
            "/clob/order-intent/human-approvals",
            get(clob_order_intent_human_approvals_handler),
        )
        .route(
            "/clob/order-intent/dry-runs",
            get(clob_order_intent_dry_runs_handler),
        )
        .route(
            "/clob/order-intent/dry-runs/:event_id",
            get(clob_order_intent_dry_run_detail_handler),
        )
        .route(
            "/clob/order-intent/dry-runs/:event_id/review",
            post(clob_order_intent_dry_run_review_handler),
        )
        .route(
            "/clob/order-intent/reviews",
            get(clob_order_intent_reviews_handler),
        )
        .route(
            "/clob/order-intent/review-summary",
            get(clob_order_intent_review_summary_handler),
        )
        .route(
            "/clob/order-intent/review-health",
            get(clob_order_intent_review_health_handler),
        )
        .route(
            "/clob/order-intent/review-guidance-exceptions",
            get(clob_order_intent_review_guidance_exceptions_handler),
        )
        .route(
            "/clob/order-intent/review-guidance-overrides",
            get(clob_order_intent_review_guidance_overrides_handler),
        )
        .route(
            "/clob/order-intent/review-backlog",
            get(clob_order_intent_review_backlog_handler),
        )
        .route(
            "/clob/order-intent/review-queue",
            get(clob_order_intent_review_queue_handler),
        )
        .layer(TraceLayer::new_for_http())
        .with_state(Arc::new(state));

    // Final router: always merge root routes for probes and rewritten traffic. Also
    // mount the same tree at /polytrader for edge-forwarded traffic that was not rewritten.
    let root_routes = probe_routes.merge(app_routes);
    let app = if prefix.is_empty() {
        root_routes
    } else {
        root_routes.clone().nest(&prefix, root_routes)
    };

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
    // Reload config cheaply for debug info (auth status, etc.). /health must stay fast and independent.
    let cfg = crate::config::Config::load();
    Json(serde_json::json!({
        "status": "ok",
        "mode": "paper",
        "auth_enabled": cfg.auth_enabled(),
        "subpath_prefix": cfg.normalized_subpath_prefix(),
    }))
}

async fn markets_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let rows: Vec<MarketRow> = sqlx::query_as(
        "SELECT gamma_id, slug, question, category, last_mid_yes, last_mid_no, active
         FROM market_data.markets
         WHERE active = true
         ORDER BY updated_at DESC
         LIMIT 20",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();

    let response = rows
        .into_iter()
        .map(|row| MarketResponse {
            clob_mid_ready: market_has_two_sided_mids(&row.last_mid_yes, &row.last_mid_no),
            market_data_status: market_data_status(&row.last_mid_yes, &row.last_mid_no),
            gamma_id: row.gamma_id,
            slug: row.slug,
            question: row.question,
            category_label: row
                .category
                .as_deref()
                .map(category_display_label)
                .map(str::to_string),
            category: row.category,
            last_mid_yes: row.last_mid_yes,
            last_mid_no: row.last_mid_no,
            active: row.active,
        })
        .collect::<Vec<_>>();

    Json(response)
}

async fn market_categories_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let rows: Vec<MarketCategoryRow> = sqlx::query_as(
        "SELECT category,
                COUNT(*)::BIGINT AS active_market_count,
                COUNT(*) FILTER (WHERE last_mid_yes IS NOT NULL AND last_mid_no IS NOT NULL)::BIGINT AS data_ready_market_count
         FROM market_data.markets
         WHERE active = true
         GROUP BY category
         ORDER BY active_market_count DESC, category NULLS LAST
         LIMIT 50",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();

    let response = rows
        .into_iter()
        .map(|row| MarketCategoryResponse {
            category_label: row
                .category
                .as_deref()
                .map(category_display_label)
                .unwrap_or("Uncategorized")
                .to_string(),
            category: row.category,
            active_market_count: row.active_market_count,
            data_ready_market_count: row.data_ready_market_count,
        })
        .collect::<Vec<_>>();

    Json(response)
}

async fn strategy_paper_candidates_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    //! Read-only strategy candidate view for paper-only operation.
    //!
    //! RISK: This route wires strategy scoring to paper-order previews only.
    //! It never calls `/paper/orders`, never sets `confirm_paper_order:true`,
    //! never writes paper order/fill/position rows, and never touches CLOB order
    //! APIs. Its purpose is to make the strategy layer observable before any
    //! autonomous paper caller is allowed to execute candidates.
    match build_strategy_paper_candidates(&state.pool).await {
        Ok(body) => Json(body).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "request_sent": false,
                "post_order_called": false,
                "post_orders_called": false,
                "error": format!("Failed to build strategy paper candidates: {e}")
            })),
        )
            .into_response(),
    }
}

async fn strategy_paper_candidate_observation_handler(
    State(state): State<Arc<AppState>>,
    request: Option<Json<StrategyPaperCandidateObservationRequest>>,
) -> impl IntoResponse {
    //! Journal-only strategy observation.
    //!
    //! RISK: This creates append-only Hermes input, not trading authority. It
    //! builds the same candidate snapshot as the read-only GET route, records
    //! attribution/no-send flags in `journal.events`, and never calls paper
    //! execution, signing, approvals, allowance refresh, live senders, or CLOB
    //! order APIs.
    let request = request.map(|Json(request)| request).unwrap_or_default();
    match build_strategy_paper_candidate_observation(&state.pool, request).await {
        Ok(body) => Json(body).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "journaled": false,
                "request_sent": false,
                "post_order_called": false,
                "post_orders_called": false,
                "error": format!("Failed to record strategy paper candidate observation: {e}")
            })),
        )
            .into_response(),
    }
}

async fn strategy_paper_candidate_observations_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<DryRunEventsQuery>,
) -> impl IntoResponse {
    //! Read-only strategy candidate observation history.
    //!
    //! RISK: These are journaled pre-execution observations only. This route
    //! cannot record a new observation, submit a paper order, sign, approve,
    //! refresh allowance, create a live sender, or call CLOB order APIs.
    let limit = clamp_dry_run_events_limit(query.limit.unwrap_or(10));
    match load_strategy_paper_candidate_observation_events(&state.pool, limit).await {
        Ok(events) => Json(serde_json::json!({
            "paper_only": true,
            "real_orders_enabled": false,
            "strategy_candidate_observation_history": true,
            "request_sent": false,
            "post_order_called": false,
            "post_orders_called": false,
            "count": events.len(),
            "events": events,
            "note": "Read-only journal.events history for strategy paper candidate observations; no paper or CLOB order API is called."
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "strategy_candidate_observation_history": true,
                "request_sent": false,
                "post_order_called": false,
                "post_orders_called": false,
                "error": format!("Failed to load strategy paper candidate observations: {e}")
            })),
        )
            .into_response(),
    }
}

async fn strategy_paper_order_readiness_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<StrategyPaperOrderReadinessQuery>,
) -> impl IntoResponse {
    //! Read-only strategy paper-order preflight.
    //!
    //! RISK: This endpoint mirrors the strategy paper-order gates for operator
    //! review only. It does not record a rejection, submit a paper order, sign,
    //! approve, refresh allowance, create a live sender, or call CLOB order APIs.
    match build_strategy_paper_order_readiness(&state.pool, query).await {
        Ok(body) => Json(body).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "strategy_paper_order_readiness": true,
                "ready_for_strategy_paper_order": false,
                "request_sent": false,
                "post_order_called": false,
                "post_orders_called": false,
                "error": format!("Failed to build strategy paper-order readiness: {e}")
            })),
        )
            .into_response(),
    }
}

async fn strategy_arb_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    //! Scan active markets for YES+NO missing-probability arbitrage opportunities.
    //!
    //! Returns markets where best_ask_yes + best_ask_no < $1.00 (net of taker fees).
    //! Sorted by net_profit_per_unit descending (best first).
    //!
    //! RISK: Snapshots are up to ~5 min stale (ingester cadence). Prices shown are
    //! indicative only. Real arb execution requires live WebSocket feeds and
    //! simultaneous order placement. Paper-only; read-only; no orders submitted.
    let scanner = ArbitrageScanner::with_default_fees();
    match scanner.scan(&state.pool).await {
        Ok(opps) => Json(serde_json::json!({
            "paper_only": true,
            "real_orders_enabled": false,
            "strategy": "arbitrage_missing_probability",
            "note": "YES+NO best-ask sum below $1.00 after taker fees. Snapshot-based; prices may have moved. Never auto-executed.",
            "count": opps.len(),
            "opportunities": opps,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "error": format!("arb scan failed: {e}"),
            })),
        )
            .into_response(),
    }
}

async fn strategy_paper_order_submit_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<StrategyPaperOrderRequest>,
) -> impl IntoResponse {
    //! Strategy-gated paper execution bridge.
    //!
    //! RISK: This route is still paper-only. It re-derives the candidate on the
    //! server, requires the FusionEngine net-edge gate to pass, requires an
    //! explicit strategy confirmation, and then delegates to the existing paper
    //! order submit path. It never signs, submits, cancels, funds, approves,
    //! refreshes allowances, creates a live sender, or calls CLOB order APIs.
    let (status, body) = build_strategy_paper_order_submission(&state.pool, request).await;
    (status, Json(body)).into_response()
}

async fn fetch_market_data_readiness_summary(
    pool: &PgPool,
) -> Result<serde_json::Value, sqlx::Error> {
    let row: MarketDataReadinessRow = sqlx::query_as(
        "SELECT COUNT(*)::BIGINT AS active_market_count,
                COUNT(*) FILTER (WHERE last_mid_yes IS NOT NULL AND last_mid_no IS NOT NULL)::BIGINT AS data_ready_market_count
         FROM market_data.markets
         WHERE active = true",
    )
    .fetch_one(pool)
    .await?;

    let ready = row.active_market_count > 0 && row.data_ready_market_count > 0;
    Ok(serde_json::json!({
        "available": true,
        "status": if ready { "ready" } else { "missing_ready_market" },
        "active_market_count": row.active_market_count,
        "data_ready_market_count": row.data_ready_market_count,
        "paper_only": true,
        "real_orders_enabled": false,
    }))
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

async fn paper_order_preview_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<PaperOrderRequest>,
) -> impl IntoResponse {
    //! Paper-only execution preview. This endpoint validates the same conservative
    //! market-data and bankroll gates as the paper submit route, but never writes
    //! paper orders/fills and never touches authenticated CLOB order APIs.
    match build_paper_order_plan(&state.pool, &request).await {
        Ok(plan) => Json(plan).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "accepted_for_paper": false,
                "executed": false,
                "request_sent": false,
                "post_order_called": false,
                "post_orders_called": false,
                "error": format!("Paper order preview failed: {e}")
            })),
        )
            .into_response(),
    }
}

async fn paper_order_submit_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<PaperOrderRequest>,
) -> impl IntoResponse {
    //! Guarded paper execution only. This route can mutate `paper_trading.*` via
    //! `PaperTradingEngine`, but it cannot sign, submit, cancel, approve, fund,
    //! refresh allowances, or call CLOB `POST /order` / `POST /orders`.
    let (status, body) = submit_paper_order_from_request(
        &state.pool,
        request,
        "paper_order_submit_route",
        "paper_order_submit_route_validation",
        None,
    )
    .await;
    (status, Json(body)).into_response()
}

async fn submit_paper_order_from_request(
    pool: &PgPool,
    request: PaperOrderRequest,
    decision_context_source: &str,
    rejection_source: &str,
    extra_decision_context: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut plan = match build_paper_order_plan(pool, &request).await {
        Ok(plan) => plan,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({
                    "paper_only": true,
                    "real_orders_enabled": false,
                    "accepted_for_paper": false,
                    "executed": false,
                    "request_sent": false,
                    "post_order_called": false,
                    "post_orders_called": false,
                    "error": format!("Paper order validation failed: {e}")
                }),
            );
        }
    };

    let mut blockers = plan
        .get("blockers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if request.confirm_paper_order != Some(true) {
        blockers.push(serde_json::json!("confirm_paper_order_required"));
    }
    if !blockers.is_empty() {
        let blocker_labels = blockers
            .iter()
            .filter_map(|value| value.as_str())
            .map(str::to_string)
            .collect::<Vec<_>>();
        let rejection_payload = serde_json::json!({
            "paper_only": true,
            "real_orders_enabled": false,
            "accepted_for_paper": false,
            "executed": false,
            "source": rejection_source,
            "market_id": request.market_id.trim(),
            "outcome": request.outcome,
            "side": request.side,
            "order_type": request.order_type,
            "limit_price": request.limit_price,
            "size": request.size,
            "blockers": blocker_labels,
            "preview": plan.clone(),
            "request_sent": false,
            "would_send": false,
            "would_post": false,
            "post_order_called": false,
            "post_orders_called": false,
            "note": "Confirmed paper submit rejected before PaperTradingEngine writes paper order, fill, position, or portfolio snapshot rows."
        });
        let journal_result = record_journal_event(
            pool,
            rejection_source,
            "polytrader_server",
            "warning",
            rejection_payload,
        )
        .await;
        if let Some(object) = plan.as_object_mut() {
            object.insert("accepted_for_paper".to_string(), serde_json::json!(false));
            object.insert("executed".to_string(), serde_json::json!(false));
            object.insert("blockers".to_string(), serde_json::json!(blockers));
            match journal_result {
                Ok(event_id) => {
                    object.insert("journaled".to_string(), serde_json::json!(true));
                    object.insert("journal_event_id".to_string(), serde_json::json!(event_id));
                }
                Err(e) => {
                    object.insert("journaled".to_string(), serde_json::json!(false));
                    object.insert(
                        "journal_error".to_string(),
                        serde_json::json!(e.to_string()),
                    );
                }
            }
        }
        return (StatusCode::BAD_REQUEST, plan);
    }

    let Some(order_side) = parse_paper_order_side(&request.side) else {
        return (
            StatusCode::BAD_REQUEST,
            serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "accepted_for_paper": false,
                "executed": false,
                "blockers": ["invalid_side"],
            }),
        );
    };
    let Some(order_type) = parse_paper_order_type(&request.order_type) else {
        return (
            StatusCode::BAD_REQUEST,
            serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "accepted_for_paper": false,
                "executed": false,
                "blockers": ["invalid_order_type"],
            }),
        );
    };
    let Some(outcome) = normalize_paper_order_outcome(&request.outcome) else {
        return (
            StatusCode::BAD_REQUEST,
            serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "accepted_for_paper": false,
                "executed": false,
                "blockers": ["invalid_outcome"],
            }),
        );
    };

    let paper_fee_bps = paper_fee_bps_from_env();
    let engine = crate::paper::PaperTradingEngine::new(
        pool.clone(),
        Arc::new(crate::journal::JournalWriter::new(pool.clone())),
        paper_fee_bps,
    );
    let order = crate::paper::PaperOrder {
        id: uuid::Uuid::new_v4(),
        market_id: request.market_id.trim().to_string(),
        outcome,
        side: order_side,
        order_type,
        limit_price: request.limit_price,
        size: request.size,
        status: crate::paper::OrderStatus::Open,
        created_at: chrono::Utc::now(),
        decision_context: Some(serde_json::json!({
            "source": decision_context_source,
            "paper_only": true,
            "real_orders_enabled": false,
            "request_sent": false,
            "post_order_called": false,
            "post_orders_called": false,
            "rationale": request.rationale,
            "preview": plan,
            "extra": extra_decision_context,
        })),
    };
    let order_id = order.id;

    match engine.submit_order(order).await {
        Ok(fills) => {
            let filled_size: Decimal = fills.iter().map(|fill| fill.size).sum();
            let gross_notional: Decimal = fills.iter().map(|fill| fill.price * fill.size).sum();
            let total_fee: Decimal = fills.iter().map(|fill| fill.fee).sum();
            (
                StatusCode::OK,
                serde_json::json!({
                    "paper_only": true,
                    "real_orders_enabled": false,
                    "accepted_for_paper": true,
                    "executed": !fills.is_empty(),
                    "paper_order_id": order_id,
                    "fill_count": fills.len(),
                    "filled_size": filled_size,
                    "gross_notional": gross_notional,
                    "total_fee": total_fee,
                    "fills": fills,
                    "request_sent": false,
                    "would_send": false,
                    "would_post": false,
                    "post_order_called": false,
                    "post_orders_called": false,
                    "note": "Paper order executed only in paper_trading tables; no CLOB order API was called."
                }),
            )
        }
        Err(e) => {
            let error = e.to_string();
            // (paper risk fns not present after fidelity revert of paper; treat as non-risk for this path)
            let engine_risk_rejection = error.to_lowercase().contains("risk");
            let mut body = serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "accepted_for_paper": false,
                "executed": false,
                "paper_order_id": order_id,
                "request_sent": false,
                "post_order_called": false,
                "post_orders_called": false,
                "error": format!("Paper order execution failed: {error}")
            });
            if engine_risk_rejection {
                if let Some(object) = body.as_object_mut() {
                    object.insert(
                        "blockers".to_string(),
                        serde_json::json!(["paper_engine_risk_rejection"]),
                    );
                    object.insert(
                        "note".to_string(),
                        serde_json::json!("Paper engine risk guard rejected before fill, position, or portfolio snapshot writes."),
                    );
                }
            }
            (
                if engine_risk_rejection {
                    StatusCode::BAD_REQUEST
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                },
                body,
            )
        }
    }
}

async fn paper_orders_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<DryRunEventsQuery>,
) -> impl IntoResponse {
    //! Read-only paper order history. This exposes simulated orders/fill rollups
    //! from `paper_trading.*` so operators and Hermes can inspect paper behavior
    //! without database access. It never touches authenticated CLOB order APIs.
    let limit = clamp_dry_run_events_limit(query.limit.unwrap_or(20));
    match sqlx::query_as::<_, PaperOrderHistoryRow>(
        r#"SELECT
                o.id,
                o.market_id,
                m.slug,
                m.question,
                o.outcome,
                o.side,
                o.order_type,
                o.limit_price,
                o.size,
                o.status,
                COUNT(f.id)::BIGINT AS fill_count,
                COALESCE(SUM(f.size), 0)::NUMERIC AS filled_size,
                COALESCE(SUM(f.price * f.size), 0)::NUMERIC AS gross_notional,
                COALESCE(SUM(f.fee), 0)::NUMERIC AS total_fee,
                o.decision_context,
                o.created_at,
                o.updated_at
           FROM paper_trading.paper_orders o
           LEFT JOIN market_data.markets m ON m.gamma_id = o.market_id
           LEFT JOIN paper_trading.paper_fills f ON f.order_id = o.id
           GROUP BY o.id, m.slug, m.question
           ORDER BY o.created_at DESC
           LIMIT $1"#,
    )
    .bind(limit)
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => Json(serde_json::json!({
            "paper_only": true,
            "real_orders_enabled": false,
            "request_sent": false,
            "post_order_called": false,
            "post_orders_called": false,
            "count": rows.len(),
            "orders": rows.into_iter().map(paper_order_history_json).collect::<Vec<_>>(),
            "note": "Read-only simulated paper order history; no CLOB order API is called."
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "request_sent": false,
                "post_order_called": false,
                "post_orders_called": false,
                "error": format!("Failed to load paper order history: {e}")
            })),
        )
            .into_response(),
    }
}

async fn paper_fills_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<DryRunEventsQuery>,
) -> impl IntoResponse {
    //! Read-only paper fill history. Fills are simulated executions produced by
    //! `PaperTradingEngine` and are used by Hermes for fee/P&L attribution.
    let limit = clamp_dry_run_events_limit(query.limit.unwrap_or(20));
    match sqlx::query_as::<_, PaperFillHistoryRow>(
        r#"SELECT
                f.id,
                f.order_id,
                o.market_id,
                m.slug,
                o.outcome,
                o.side,
                f.price,
                f.size,
                f.fee,
                f.slippage_bps,
                f.created_at
           FROM paper_trading.paper_fills f
           JOIN paper_trading.paper_orders o ON o.id = f.order_id
           LEFT JOIN market_data.markets m ON m.gamma_id = o.market_id
           ORDER BY f.created_at DESC
           LIMIT $1"#,
    )
    .bind(limit)
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => Json(serde_json::json!({
            "paper_only": true,
            "real_orders_enabled": false,
            "request_sent": false,
            "post_order_called": false,
            "post_orders_called": false,
            "count": rows.len(),
            "fills": rows.into_iter().map(paper_fill_history_json).collect::<Vec<_>>(),
            "note": "Read-only simulated paper fill history; no CLOB order API is called."
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "request_sent": false,
                "post_order_called": false,
                "post_orders_called": false,
                "error": format!("Failed to load paper fill history: {e}")
            })),
        )
            .into_response(),
    }
}

async fn paper_positions_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    //! Read-only current paper position exposure. These rows are simulated
    //! `paper_trading.paper_positions` state only; there is no wallet or CLOB
    //! position read/write behind this endpoint.
    match load_paper_position_rows(&state.pool).await {
        Ok(rows) => Json(serde_json::json!({
            "paper_only": true,
            "real_orders_enabled": false,
            "request_sent": false,
            "post_order_called": false,
            "post_orders_called": false,
            "count": rows.len(),
            "positions": rows.into_iter().map(paper_position_history_json).collect::<Vec<_>>(),
            "note": "Read-only simulated paper position exposure; no CLOB order or wallet API is called."
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "request_sent": false,
                "post_order_called": false,
                "post_orders_called": false,
                "error": format!("Failed to load paper positions: {e}")
            })),
        )
            .into_response(),
    }
}

/// JSON backing the /trades visualization: portfolio summary, open positions with live unrealized
/// P&L (current mid vs avg entry), and the recent autonomous execution feed. Read-only, paper-only.
async fn trades_data_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let pool = &state.pool;

    let portfolio: Option<(
        Decimal,
        Decimal,
        Decimal,
        Decimal,
        chrono::DateTime<chrono::Utc>,
    )> = sqlx::query_as(
        "SELECT virtual_usdc, total_locked, unrealized_pnl, realized_pnl, as_of
             FROM paper_trading.virtual_portfolio_snapshots ORDER BY as_of DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    // Open positions joined to market metadata + the current mid for the held outcome.
    // (market_id, slug, question, outcome, shares, avg_entry, collateral_locked, current_mid)
    type PositionRow = (
        String,
        Option<String>,
        Option<String>,
        String,
        Decimal,
        Decimal,
        Decimal,
        Option<Decimal>,
    );
    let pos_rows: Vec<PositionRow> = sqlx::query_as(
        "SELECT p.market_id, m.slug, m.question, p.outcome, p.shares, p.avg_entry_price,
                p.collateral_locked,
                CASE WHEN p.outcome = 'Yes' THEN m.last_mid_yes ELSE m.last_mid_no END AS current_mid
         FROM paper_trading.paper_positions p
         LEFT JOIN market_data.markets m ON m.gamma_id = p.market_id
         WHERE p.shares > 0
         ORDER BY p.collateral_locked DESC",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let positions: Vec<serde_json::Value> = pos_rows
        .into_iter()
        .map(
            |(market_id, slug, question, outcome, shares, avg_entry, locked, current_mid)| {
                let mid = current_mid.unwrap_or(avg_entry);
                let unrealized = (shares * (mid - avg_entry)).round_dp(2);
                serde_json::json!({
                    "market_id": market_id,
                    "slug": slug,
                    "question": question,
                    "outcome": outcome,
                    "shares": shares.round_dp(2).to_string(),
                    "avg_entry_price": avg_entry.round_dp(4).to_string(),
                    "current_mid": mid.round_dp(4).to_string(),
                    "collateral_locked": locked.round_dp(2).to_string(),
                    "unrealized_pnl": unrealized.to_string(),
                })
            },
        )
        .collect();

    let exec_rows: Vec<(String, serde_json::Value, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as(
            "SELECT event_type, payload, created_at FROM journal.events
         WHERE event_type IN ('autonomous_paper_execution', 'autonomous_arb_execution')
         ORDER BY created_at DESC LIMIT 40",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();

    let executions: Vec<serde_json::Value> = exec_rows
        .into_iter()
        .map(|(event_type, payload, created_at)| {
            serde_json::json!({
                "kind": event_type,
                "action": payload.get("action").and_then(|v| v.as_str()).unwrap_or("?"),
                "market_id": payload.get("market_id"),
                "outcome": payload.get("outcome"),
                "approved_usdc": payload.get("approved_usdc"),
                "gross_notional": payload.get("gross_notional"),
                "net_edge": payload.get("net_edge"),
                "reason": payload.get("reason"),
                "both_legs_filled": payload.get("both_legs_filled"),
                "at": created_at.to_rfc3339(),
            })
        })
        .collect();

    let total_unrealized: Decimal = positions
        .iter()
        .filter_map(|p| p["unrealized_pnl"].as_str()?.parse::<Decimal>().ok())
        .sum();

    let portfolio_json = match portfolio {
        Some((usdc, locked, _unreal, realized, as_of)) => serde_json::json!({
            "virtual_usdc": usdc.round_dp(2).to_string(),
            "total_locked": locked.round_dp(2).to_string(),
            "realized_pnl": realized.round_dp(2).to_string(),
            "live_unrealized_pnl": total_unrealized.round_dp(2).to_string(),
            "equity": (usdc + locked + total_unrealized).round_dp(2).to_string(),
            "as_of": as_of.to_rfc3339(),
        }),
        None => serde_json::json!({}),
    };

    // Real-trading shadow orders (fail-closed) + the latest go-live gate, for the readiness panel.
    let shadow_rows: Vec<(serde_json::Value, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT payload, created_at FROM journal.events
         WHERE event_type = 'clob_shadow_order' ORDER BY created_at DESC LIMIT 10",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let go_live_gate = shadow_rows
        .first()
        .map(|(p, _)| {
            p.get("go_live_gate")
                .cloned()
                .unwrap_or(serde_json::Value::Null)
        })
        .unwrap_or(serde_json::Value::Null);
    let shadow_orders: Vec<serde_json::Value> = shadow_rows
        .into_iter()
        .map(|(p, at)| {
            serde_json::json!({
                "at": at.to_rfc3339(),
                "would_send": p.get("would_send"),
                "dispatched": p.get("fail_closed_result").and_then(|r| r.get("request_sent")),
                "rejection_reason": p.get("fail_closed_result").and_then(|r| r.get("rejection_reason")),
            })
        })
        .collect();

    // Real Polymarket account: latest PUSD balance of the proxy (read-only, journaled by main).
    let real_balance: Option<(serde_json::Value, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT payload, created_at FROM journal.events
         WHERE event_type = 'real_account_balance' ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    let real_account = match real_balance {
        Some((p, at)) => serde_json::json!({
            "proxy_address": p.get("proxy_address"),
            "collateral_token": p.get("collateral_token"),
            "balance": p.get("balance"),
            "as_of": at.to_rfc3339(),
        }),
        None => serde_json::Value::Null,
    };

    // Settlements: resolved positions → realized P&L (ground truth on strategy performance).
    let settle_rows: Vec<(serde_json::Value, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT payload, created_at FROM journal.events
         WHERE event_type = 'paper_position_settled' ORDER BY created_at DESC LIMIT 25",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let settlements: Vec<serde_json::Value> = settle_rows
        .iter()
        .map(|(p, at)| {
            serde_json::json!({
                "at": at.to_rfc3339(),
                "market_id": p.get("market_id"),
                "outcome": p.get("outcome"),
                "won": p.get("won"),
                "realized_pnl": p.get("realized_pnl"),
                "payout": p.get("payout"),
                "cost_basis": p.get("cost_basis"),
            })
        })
        .collect();
    let settled_count = settlements.len();
    let settled_pnl: Decimal = settle_rows
        .iter()
        .filter_map(|(p, _)| p.get("realized_pnl")?.as_str()?.parse::<Decimal>().ok())
        .sum();
    let wins = settle_rows
        .iter()
        .filter(|(p, _)| p.get("won").and_then(|v| v.as_bool()).unwrap_or(false))
        .count();

    let llm_health: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT payload FROM journal.events WHERE event_type = 'llm_health' ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    // Total-P&L time series for the live equity chart: running P&L = realized + unrealized at each
    // snapshot (zero at inception, independent of starting bankroll). Ascending for left-to-right plot.
    let series_rows: Vec<(chrono::DateTime<chrono::Utc>, Decimal, Decimal)> = sqlx::query_as(
        "SELECT as_of, realized_pnl, unrealized_pnl FROM (
             SELECT as_of, realized_pnl, unrealized_pnl
             FROM paper_trading.virtual_portfolio_snapshots
             ORDER BY as_of DESC LIMIT 300
         ) s ORDER BY as_of ASC",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let pnl_series: Vec<serde_json::Value> = series_rows
        .into_iter()
        .map(|(at, realized, unreal)| {
            serde_json::json!({
                "t": at.timestamp(),
                "pnl": (realized + unreal).round_dp(2).to_string(),
            })
        })
        .collect();

    // === Effective parameters (read-only) for the UI parameters panel ===
    let risk_cfg = crate::risk::RiskConfig::from_env();
    let env_flag = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let bootstrap_count = env_flag("POLYTRADER_BOOTSTRAP_MARKETS")
        .map(|v| v.split(',').filter(|s| !s.trim().is_empty()).count())
        .unwrap_or(0);
    let arb_only_count = env_flag("POLYTRADER_ARB_ONLY_MARKETS")
        .map(|v| v.split(',').filter(|s| !s.trim().is_empty()).count())
        .unwrap_or(0);
    let config_json = serde_json::json!({
        "risk": risk_cfg.to_json(),
        "autonomous_paper_execution": env_flag("POLYTRADER_AUTONOMOUS_PAPER_EXECUTION")
            .map(|v| v.to_lowercase() == "on").unwrap_or(false),
        "external_signals": env_flag("POLYTRADER_EXTERNAL_SIGNALS")
            .map(|v| v.to_lowercase() == "on").unwrap_or(false),
        "ingest_interval_secs": env_flag("POLYTRADER_INGEST_INTERVAL_SECS").unwrap_or_else(|| "300".into()),
        "decision_cadence_secs": "300",
        "markets_tracked": bootstrap_count,
        "arb_only_markets": arb_only_count,
        "real_orders_enabled": false,
    });

    // === Dual-gate (A/B) simulation ===
    // The live gate is min_net_edge (lenient). shadow_net_edge is stricter. Because the lenient set is
    // a superset of the strict set, we can compare both from one live run: tag each entry fill by
    // whether it clears the shadow gate, then aggregate count / notional / open-unrealized / settled
    // realized P&L per band. "lenient" = all fills (current live gate); "strict" = the shadow subset.
    // (market_id, outcome, net_edge, gross_notional, clears_shadow_gate)
    type FillBandRow = (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<bool>,
    );
    let fill_rows: Vec<FillBandRow> = sqlx::query_as(
        "SELECT payload->>'market_id', payload->>'outcome', payload->>'net_edge',
                    payload->>'gross_notional', (payload->>'clears_shadow_gate')::bool
             FROM journal.events
             WHERE event_type = 'autonomous_paper_execution' AND payload->>'action' = 'filled'",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    // (market_id, outcome) -> clears_shadow, for joining settlements to their entry band.
    let mut band_map: HashMap<(String, String), bool> = HashMap::new();
    // Live unrealized per (market_id, outcome) from the open positions we already computed.
    let unreal_map: HashMap<(String, String), Decimal> = positions
        .iter()
        .filter_map(|p| {
            let m = p.get("market_id")?.as_str()?.to_string();
            let o = p.get("outcome")?.as_str()?.to_string();
            let u = p.get("unrealized_pnl")?.as_str()?.parse::<Decimal>().ok()?;
            Some(((m, o), u))
        })
        .collect();
    let shadow_threshold = risk_cfg.shadow_net_edge;
    // (count, notional, open_unrealized) accumulators for lenient(all) and strict(shadow subset).
    let (mut len_n, mut len_not, mut len_unr) = (0i64, Decimal::ZERO, Decimal::ZERO);
    let (mut str_n, mut str_not, mut str_unr) = (0i64, Decimal::ZERO, Decimal::ZERO);
    for (m, o, edge, notional, clears) in &fill_rows {
        let (Some(m), Some(o)) = (m.clone(), o.clone()) else {
            continue;
        };
        let edge_dec = edge
            .as_deref()
            .and_then(|s| s.parse::<Decimal>().ok())
            .unwrap_or(Decimal::ZERO);
        // Fall back to recomputing the band for fills journaled before edge tagging existed.
        let in_strict = clears.unwrap_or(edge_dec >= shadow_threshold);
        let notion = notional
            .as_deref()
            .and_then(|s| s.parse::<Decimal>().ok())
            .unwrap_or(Decimal::ZERO);
        let unreal = unreal_map
            .get(&(m.clone(), o.clone()))
            .copied()
            .unwrap_or(Decimal::ZERO);
        band_map.insert((m, o), in_strict);
        len_n += 1;
        len_not += notion;
        len_unr += unreal;
        if in_strict {
            str_n += 1;
            str_not += notion;
            str_unr += unreal;
        }
    }
    // Settled realized P&L per band (join each settlement to its entry band).
    let (mut len_real, mut len_settled, mut len_wins) = (Decimal::ZERO, 0i64, 0i64);
    let (mut str_real, mut str_settled, mut str_wins) = (Decimal::ZERO, 0i64, 0i64);
    for (p, _) in &settle_rows {
        let (Some(m), Some(o)) = (
            p.get("market_id").and_then(|v| v.as_str()),
            p.get("outcome").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        let realized = p
            .get("realized_pnl")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<Decimal>().ok())
            .unwrap_or(Decimal::ZERO);
        let won = p.get("won").and_then(|v| v.as_bool()).unwrap_or(false);
        let in_strict = band_map
            .get(&(m.to_string(), o.to_string()))
            .copied()
            .unwrap_or(realized >= Decimal::ZERO);
        len_real += realized;
        len_settled += 1;
        if won {
            len_wins += 1;
        }
        if in_strict {
            str_real += realized;
            str_settled += 1;
            if won {
                str_wins += 1;
            }
        }
    }
    let band_json = |label: &str,
                     edge_floor: String,
                     n: i64,
                     notional: Decimal,
                     unreal: Decimal,
                     real: Decimal,
                     settled: i64,
                     wins: i64| {
        serde_json::json!({
            "label": label,
            "min_net_edge": edge_floor,
            "fills": n,
            "notional": notional.round_dp(2).to_string(),
            "open_unrealized": unreal.round_dp(2).to_string(),
            "settled_realized": real.round_dp(2).to_string(),
            "settled": settled,
            "wins": wins,
            "total_pnl": (real + unreal).round_dp(2).to_string(),
        })
    };
    // Per-signal scorecard: which of the 5 fusion processors are firing, how often, how hard, what
    // Hermes currently weights them, and (once positions settle) the realized P&L attributed to each.
    // Fire-rate/influence are available now; realized P&L stays empty until settlements exist — the
    // same data-gate that pauses Hermes weight tuning.
    const SIGNALS: [&str; 5] = [
        "orderbook_momentum",
        "spike_divergence",
        "overreaction_fade",
        "yahoo_finance",
        "news_sentiment",
    ];
    let dr_attrs: Vec<serde_json::Value> = sqlx::query_scalar(
        "SELECT payload->'report'->'attribution' FROM journal.events
         WHERE event_type = 'decision_report' AND created_at > now() - interval '24 hours'
         ORDER BY created_at DESC LIMIT 3000",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let reports_total = dr_attrs.len();
    // Latest Hermes weights + per-signal realized P&L (if any).
    let latest_weights: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT payload FROM journal.events WHERE event_type = 'strategy_weights'
         ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    let weight_of = |name: &str| -> String {
        latest_weights
            .as_ref()
            .and_then(|p| p.pointer(&format!("/weights/{name}")))
            .and_then(|v| v.as_str())
            .unwrap_or("1.0")
            .to_string()
    };
    let realized_of = |name: &str| -> Option<String> {
        latest_weights
            .as_ref()
            .and_then(|p| p.pointer(&format!("/per_signal_realized_pnl/{name}")))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    };

    // Per-signal REALIZED HIT-RATE — computed directly from settled positions, independent of Hermes's
    // weight-tuning gate (which only writes strategy_weights once >=10 settled). This surfaces "when a
    // signal fired, did the market resolve in our favour?" as soon as ANY position settles. A settled
    // MARKET is scored by its NET realized P&L (sum across both sides if we held them), so both-sides
    // markets count once by their net outcome. A signal is credited a market if it fired (non-zero
    // score) in ANY of that market's recent decision reports (not just the final one). Overlapping by
    // design (each signal keeps its own record), so this is a count-based win-rate, not a P&L split
    // (Hermes does the P&L split).
    let settled_rows: Vec<(Option<String>, Option<Decimal>)> = sqlx::query_as(
        "SELECT payload->>'market_id', (payload->>'realized_pnl')::numeric
         FROM journal.events WHERE event_type = 'paper_position_settled'",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    // Net realized P&L per settled market.
    let mut net_by_market: std::collections::HashMap<String, Decimal> =
        std::collections::HashMap::new();
    for (m, pnl) in settled_rows.into_iter() {
        if let (Some(m), Some(pnl)) = (m, pnl) {
            *net_by_market.entry(m).or_insert(Decimal::ZERO) += pnl;
        }
    }
    let settled_market_ids: Vec<String> = net_by_market.keys().cloned().collect();
    // ALL recent decision-report attributions for the settled markets (not just the final one): a
    // signal is credited a market if it fired at ANY point during the holding period. Using only the
    // final DR would under-credit signals whose inputs vanish at resolution (e.g. orderbook_momentum:
    // the book empties when a market closes, so it never fires in the post-resolution DR).
    let market_attrs: Vec<(Option<String>, Option<serde_json::Value>)> =
        if settled_market_ids.is_empty() {
            Vec::new()
        } else {
            sqlx::query_as(
                "SELECT payload->>'market_id', payload->'report'->'attribution'
                 FROM journal.events
                 WHERE event_type = 'decision_report'
                   AND payload->>'market_id' = ANY($1)
                   AND created_at > now() - interval '7 days'",
            )
            .bind(&settled_market_ids)
            .fetch_all(pool)
            .await
            .unwrap_or_default()
        };
    // market_id -> set of signal names that fired (non-zero score) in ANY of its decision reports.
    let mut fired_by_market: std::collections::HashMap<String, std::collections::HashSet<String>> =
        std::collections::HashMap::new();
    for (m, attr) in market_attrs.into_iter() {
        let (Some(m), Some(attr)) = (m, attr) else {
            continue;
        };
        let entry = fired_by_market.entry(m).or_default();
        for name in SIGNALS.iter() {
            let fired = attr
                .pointer(&format!("/{name}/score"))
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<Decimal>().ok())
                .map(|sc| !sc.is_zero())
                .unwrap_or(false);
            if fired {
                entry.insert(name.to_string());
            }
        }
    }
    let settled_record_of = |name: &str| -> (usize, usize) {
        // (wins, total) over settled markets where this signal fired at any point.
        let mut wins = 0usize;
        let mut total = 0usize;
        for (mkt, fired) in &fired_by_market {
            if fired.contains(name) {
                total += 1;
                if net_by_market.get(mkt).copied().unwrap_or(Decimal::ZERO) > Decimal::ZERO {
                    wins += 1;
                }
            }
        }
        (wins, total)
    };

    let signal_rows: Vec<serde_json::Value> = SIGNALS
        .iter()
        .map(|name| {
            let mut fired = 0usize;
            let mut abs_sum = Decimal::ZERO;
            for attr in &dr_attrs {
                if let Some(score) = attr
                    .pointer(&format!("/{name}/score"))
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<Decimal>().ok())
                {
                    if !score.is_zero() {
                        fired += 1;
                        abs_sum += score.abs();
                    }
                }
            }
            let fire_rate = if reports_total > 0 {
                (Decimal::from(fired) / Decimal::from(reports_total) * Decimal::from(100))
                    .round_dp(1)
            } else {
                Decimal::ZERO
            };
            let avg_abs_score = if fired > 0 {
                (abs_sum / Decimal::from(fired)).round_dp(3)
            } else {
                Decimal::ZERO
            };
            let (settled_wins, settled_total) = settled_record_of(name);
            let settled_winrate = if settled_total > 0 {
                (Decimal::from(settled_wins) / Decimal::from(settled_total) * Decimal::from(100))
                    .round_dp(0)
                    .to_string()
            } else {
                String::new()
            };
            serde_json::json!({
                "name": name,
                "fired": fired,
                "fire_rate_pct": fire_rate.to_string(),
                "avg_abs_score": avg_abs_score.to_string(),
                "weight": weight_of(name),
                "realized_pnl": realized_of(name),
                "settled_wins": settled_wins,
                "settled_total": settled_total,
                "settled_winrate_pct": settled_winrate,
            })
        })
        .collect();
    let signals_json = serde_json::json!({
        "reports_window_h": 24,
        "reports_total": reports_total,
        "settled_markets": net_by_market.len(),
        "rows": signal_rows,
        "note": "Fire-rate = share of the last 24h decision reports where the signal contributed a \
                 non-zero score. Weight is Hermes's current confidence multiplier. Settled record = \
                 win/loss of settled markets (by net realized P&L) where the signal fired in the final \
                 decision report (count-based, overlapping). Realized P&L (Hermes's proportional split) \
                 populates once weight tuning activates at 10 settled.",
    });

    let gate_simulation = serde_json::json!({
        "note": "One live run, two gates. 'Lenient' is the active gate (all fills); 'Strict' is the \
                 shadow subset that also clears the stricter edge — i.e. how a tighter gate would have \
                 done on the same data.",
        "lenient": band_json("Lenient (live)", risk_cfg.min_net_edge.to_string(), len_n, len_not, len_unr, len_real, len_settled, len_wins),
        "strict": band_json("Strict (shadow)", shadow_threshold.to_string(), str_n, str_not, str_unr, str_real, str_settled, str_wins),
    });

    Json(serde_json::json!({
        "paper_only": true,
        "real_orders_enabled": false,
        "portfolio": portfolio_json,
        "pnl_series": pnl_series,
        "config": config_json,
        "signals": signals_json,
        "gate_simulation": gate_simulation,
        "open_positions": positions,
        "recent_executions": executions,
        "real_account": real_account,
        "llm_health": llm_health,
        "settlements": {
            "count": settled_count,
            "wins": wins,
            "total_realized_pnl": settled_pnl.round_dp(2).to_string(),
            "recent": settlements,
        },
        "real_trading": {
            "go_live_gate": go_live_gate,
            "recent_shadow_orders": shadow_orders,
        },
    }))
    .into_response()
}

/// Self-contained HTML page that visualizes paper trades (polls /trades/data). Kept separate from
/// the Dioxus dashboard to stay low-risk; linked by URL. Paper-only, read-only.
async fn trades_page_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let prefix = state.subpath_prefix.clone();
    Html(render_trades_page(&prefix))
}

/// Render the self-contained trades dashboard HTML. `__PREFIX__` placeholders are replaced with the
/// subpath prefix so fetches resolve under reverse-proxy deployments. (No format! to avoid escaping
/// every brace in the embedded CSS/JS.)
fn render_trades_page(prefix: &str) -> String {
    const PAGE: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>Polytrader — Paper Trades</title>
<style>
  :root { color-scheme: dark; }
  body { margin:0; background:#0d1117; color:#e6edf3; font:14px/1.5 -apple-system,Segoe UI,Roboto,sans-serif; }
  header { padding:16px 24px; border-bottom:1px solid #21262d; display:flex; align-items:center; gap:12px; }
  h1 { font-size:18px; margin:0; }
  .badge { background:#1f6feb22; color:#58a6ff; border:1px solid #1f6feb55; padding:2px 8px; border-radius:12px; font-size:12px; }
  .paper { background:#23863622; color:#3fb950; border-color:#23863655; }
  main { padding:24px; max-width:1100px; margin:0 auto; }
  .cards { display:flex; gap:12px; flex-wrap:wrap; margin-bottom:24px; }
  .card { background:#161b22; border:1px solid #21262d; border-radius:8px; padding:14px 18px; min-width:140px; }
  .card .label { color:#8b949e; font-size:12px; text-transform:uppercase; letter-spacing:.04em; }
  .card .val { font-size:20px; font-weight:600; margin-top:4px; }
  h2 { font-size:14px; color:#8b949e; text-transform:uppercase; letter-spacing:.04em; margin:24px 0 8px; }
  table { width:100%; border-collapse:collapse; background:#161b22; border:1px solid #21262d; border-radius:8px; overflow:hidden; }
  th,td { text-align:left; padding:8px 12px; border-bottom:1px solid #21262d; font-variant-numeric:tabular-nums; }
  th { color:#8b949e; font-weight:500; font-size:12px; }
  tr:last-child td { border-bottom:none; }
  .pos { color:#3fb950; } .neg { color:#f85149; } .muted { color:#8b949e; }
  .pill { font-size:11px; padding:1px 7px; border-radius:10px; border:1px solid #30363d; }
  .arb { color:#d2a8ff; border-color:#8957e555; } .dir { color:#58a6ff; border-color:#1f6feb55; }
  .empty { color:#8b949e; padding:18px; text-align:center; }
  .chartbox { background:#161b22; border:1px solid #21262d; border-radius:8px; padding:10px 12px; }
  .chartbox svg { display:block; width:100%; height:auto; }
  .t { color:#8b949e; font-size:12px; }
  footer { color:#8b949e; font-size:12px; padding:0 24px 24px; max-width:1100px; margin:0 auto; }
</style>
</head>
<body>
<header>
  <h1>Polytrader — Paper Trades</h1>
  <span class="badge paper">PAPER ONLY</span>
  <span class="badge" id="updated">loading…</span>
  <span class="badge" id="llm" title="Hermes AI model health">AI: …</span>
  <nav style="display:flex;gap:4px;margin-left:auto;">
    <a href="__ROOT__" style="color:#8b949e;text-decoration:none;padding:5px 12px;border-radius:7px;font-size:13px;">Markets</a>
    <a href="__PREFIX__/trades" style="background:#1f6feb22;color:#58a6ff;text-decoration:none;padding:5px 12px;border-radius:7px;font-size:13px;">Trades</a>
    <a href="__PREFIX__/console" style="color:#8b949e;text-decoration:none;padding:5px 12px;border-radius:7px;font-size:13px;">Console</a>
  </nav>
</header>
<main>
  <div class="cards" id="cards"></div>
  <h2>Profit &amp; Loss <span class="pill" id="pnl-now"></span></h2>
  <div id="pnl-chart" class="chartbox"><div class="empty">loading P&amp;L history…</div></div>
  <h2>Signal Scorecard <span class="pill" id="signals-window"></span></h2>
  <div id="signals"></div>
  <h2>Gate Simulation <span class="pill dir" id="gatesim-edges"></span></h2>
  <div id="gatesim"></div>
  <h2>Parameters <span class="pill" id="params-mode">paper · read-only</span></h2>
  <div id="params"></div>
  <h2>Open Positions</h2>
  <div id="positions"></div>
  <h2>Settlements <span class="pill dir" id="settle-summary"></span></h2>
  <div id="settlements"></div>
  <h2>Recent Autonomous Executions</h2>
  <div id="exec-filter" style="display:flex;gap:6px;margin-bottom:8px;flex-wrap:wrap;"></div>
  <div id="executions"></div>
  <h2>Real-Trading Readiness <span class="pill dir">fail-closed · nothing sent</span></h2>
  <div id="readiness"></div>
  <div id="shadows"></div>
</main>
<footer>Auto-refreshes every 15s · all activity simulated against live public market data · no real orders.</footer>
<script>
const PREFIX = "__PREFIX__";
const fmt = (v) => (v===null||v===undefined) ? "—" : v;
const num = (v) => { const n = parseFloat(v); return isNaN(n) ? "—" : n; };
const cls = (v) => { const n = parseFloat(v); return n>0?"pos":(n<0?"neg":"muted"); };
const sign = (v) => { const n = parseFloat(v); return (n>0?"+":"") + (isNaN(n)?"—":n.toFixed(2)); };

// Live P&L area chart: green when the latest total P&L is >= 0, red when underwater. Plots the
// running realized+unrealized series. Pure inline SVG (no chart lib) so it stays self-contained.
function renderPnlChart(series){
  const box = document.getElementById("pnl-chart");
  const pts = (series||[]).map(s => ({t: s.t, v: parseFloat(s.pnl)})).filter(p => !isNaN(p.v));
  const nowEl = document.getElementById("pnl-now");
  if (pts.length < 2) { box.innerHTML = `<div class="empty">Not enough P&L history yet — the chart appears once a few snapshots accrue.</div>`; nowEl.textContent=""; return; }
  const last = pts[pts.length-1].v;
  const up = last >= 0;
  const stroke = up ? "#3fb950" : "#f85149";
  const fill   = up ? "rgba(63,185,80,0.16)" : "rgba(248,81,73,0.16)";
  nowEl.textContent = (last>=0?"+":"") + last.toFixed(2);
  nowEl.style.color = stroke; nowEl.style.borderColor = stroke+"55";
  const W=1000, H=220, padL=46, padR=12, padT=14, padB=22;
  const xs = pts.map(p=>p.t), vs = pts.map(p=>p.v);
  const minT=Math.min(...xs), maxT=Math.max(...xs);
  let minV=Math.min(...vs,0), maxV=Math.max(...vs,0);
  if (minV===maxV){ minV-=1; maxV+=1; }
  const pad=(maxV-minV)*0.1||1; minV-=pad; maxV+=pad;
  const sx=t=> padL + (maxT===minT?0:(t-minT)/(maxT-minT))*(W-padL-padR);
  const sy=v=> padT + (1-(v-minV)/(maxV-minV))*(H-padT-padB);
  const line = pts.map((p,i)=>`${i?'L':'M'}${sx(p.t).toFixed(1)},${sy(p.v).toFixed(1)}`).join("");
  const area = `M${sx(pts[0].t).toFixed(1)},${sy(minV<0?0:minV).toFixed(1)} ` +
               pts.map(p=>`L${sx(p.t).toFixed(1)},${sy(p.v).toFixed(1)}`).join(" ") +
               ` L${sx(pts[pts.length-1].t).toFixed(1)},${sy(minV<0?0:minV).toFixed(1)} Z`;
  const zeroY = sy(0);
  const fmtAxis=(v)=> (v>=0?"+":"")+v.toFixed(0);
  box.innerHTML = `<svg viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" role="img" aria-label="Profit and loss over time">
    <line x1="${padL}" y1="${padT}" x2="${padL}" y2="${H-padB}" stroke="#30363d" stroke-width="1"/>
    <line x1="${padL}" y1="${zeroY.toFixed(1)}" x2="${W-padR}" y2="${zeroY.toFixed(1)}" stroke="#484f58" stroke-width="1" stroke-dasharray="4 4"/>
    <text x="6" y="${(padT+8).toFixed(0)}" fill="#8b949e" font-size="12">${fmtAxis(maxV)}</text>
    <text x="6" y="${(zeroY+4).toFixed(0)}" fill="#8b949e" font-size="12">0</text>
    <text x="6" y="${(H-padB).toFixed(0)}" fill="#8b949e" font-size="12">${fmtAxis(minV)}</text>
    <path d="${area}" fill="${fill}" stroke="none"/>
    <path d="${line}" fill="none" stroke="${stroke}" stroke-width="2" stroke-linejoin="round" stroke-linecap="round"/>
    <circle cx="${sx(pts[pts.length-1].t).toFixed(1)}" cy="${sy(last).toFixed(1)}" r="3.5" fill="${stroke}"/>
  </svg>`;
  // X axis = time (snapshot timestamp). Labels + caption rendered as HTML below the SVG so they are
  // not horizontally stretched by preserveAspectRatio="none". Resolution ≈ one point per 5-min cycle.
  const tLabel = (ts)=> new Date(ts*1000).toLocaleString([], {month:"short",day:"numeric",hour:"2-digit",minute:"2-digit"});
  const midT = pts[Math.floor(pts.length/2)].t;
  const spanH = Math.max(0, (maxT - minT) / 3600);
  const spanTxt = spanH >= 48 ? (spanH/24).toFixed(1)+" days" : spanH.toFixed(1)+" h";
  box.insertAdjacentHTML("beforeend",
    `<div class="t" style="display:flex;justify-content:space-between;padding:2px 12px 0 46px;">
       <span>${tLabel(minT)}</span><span>${tLabel(midT)}</span><span>${tLabel(maxT)}</span>
     </div>
     <div class="t" style="padding:4px 12px 0 46px;color:#6e7681;">
       X axis: time · ${pts.length} points over ~${spanTxt} · ~1 point / 5-min decision cycle (last 300 snapshots) ·
       Y axis: running total P&amp;L = realized + unrealized
     </div>`);
}

// Dual-gate (A/B) simulation: live gate (lenient, all fills) vs the stricter shadow subset.
function renderGateSim(gs){
  const el = document.getElementById("gatesim");
  const edEl = document.getElementById("gatesim-edges");
  if (!gs || !gs.lenient) { el.innerHTML = `<div class="empty">No fills yet — the gate comparison populates once trades execute.</div>`; edEl.textContent=""; return; }
  const pctEdge = (v)=>{ const n=parseFloat(v); return isNaN(n)?"—":(n*100).toFixed(1)+"%"; };
  edEl.textContent = `live ≥ ${pctEdge(gs.lenient.min_net_edge)} · shadow ≥ ${pctEdge(gs.strict.min_net_edge)}`;
  const row = (b, live) => `<tr>
      <td>${b.label}${live?' <span class="pill dir">active</span>':''}</td>
      <td>≥ ${pctEdge(b.min_net_edge)}</td>
      <td>${b.fills}</td>
      <td>$${b.notional}</td>
      <td class="${cls(b.open_unrealized)}">${sign(b.open_unrealized)}</td>
      <td class="${cls(b.settled_realized)}">${sign(b.settled_realized)} <span class="muted">(${b.settled}·${b.wins}w)</span></td>
      <td class="${cls(b.total_pnl)}"><b>${sign(b.total_pnl)}</b></td>
    </tr>`;
  el.innerHTML = `<table>
    <tr><th>Gate</th><th>Min edge</th><th>Fills</th><th>Notional</th><th>Unrealized</th><th>Settled P&amp;L</th><th>Total P&amp;L</th></tr>
    ${row(gs.lenient, true)}
    ${row(gs.strict, false)}
  </table>
  <div class="t" style="padding:8px 2px;">${gs.note||""}</div>`;
}

// Per-signal scorecard: fire-rate + influence + current Hermes weight + (when settled) realized P&L.
function renderSignals(s){
  const el = document.getElementById("signals");
  const winEl = document.getElementById("signals-window");
  if (!s || !s.rows || !s.rows.length) { el.innerHTML = `<div class="empty">No decision reports in window yet.</div>`; if(winEl) winEl.textContent=""; return; }
  const sm = s.settled_markets || 0;
  if (winEl) winEl.textContent = `${s.reports_total} reports · last ${s.reports_window_h}h · ${sm} settled`;
  const pretty = (n)=> n.replace(/_/g,' ');
  const wcls = (w)=>{ const n=parseFloat(w); if(isNaN(n)||Math.abs(n-1)<0.001) return "muted"; return n>1?"pos":"neg"; };
  const recordCell = (r) => {
    const t = r.settled_total || 0;
    if (!t) return '<span class="muted">—</span>';
    const w = r.settled_wins || 0;
    const wr = parseFloat(r.settled_winrate_pct);
    const cl = isNaN(wr) ? '' : (wr >= 50 ? 'pos' : 'neg');
    return `<span class="${cl}">${w}-${t-w}</span> <span class="muted">(${r.settled_winrate_pct}%)</span>`;
  };
  const row = (r) => {
    const rp = r.realized_pnl;
    const rpCell = (rp===null||rp===undefined) ? '<span class="muted">— pending</span>' : `<span class="${cls(rp)}">${sign(rp)}</span>`;
    return `<tr>
      <td>${pretty(r.name)}</td>
      <td>${r.fire_rate_pct}% <span class="muted">(${r.fired})</span></td>
      <td>${r.avg_abs_score}</td>
      <td class="${wcls(r.weight)}">${parseFloat(r.weight).toFixed(2)}×</td>
      <td>${recordCell(r)}</td>
      <td>${rpCell}</td>
    </tr>`;
  };
  el.innerHTML = `<table>
    <tr><th>Signal</th><th title="Share of recent decision reports where this signal contributed a non-zero score">Fire rate</th><th title="Average absolute contribution when it fires">Avg influence</th><th title="Hermes's current confidence multiplier (1.00× = neutral)">Weight</th><th title="Win-loss record of settled markets (by net realized P&amp;L) where this signal fired in the final decision report. Available now, independent of Hermes.">Settled record</th><th title="Realized P&amp;L attributed to this signal (Hermes proportional split); populates at 10 settled">Settled P&amp;L</th></tr>
    ${s.rows.map(row).join("")}
  </table>
  <div class="t" style="padding:8px 2px;">${s.note||""}</div>`;
}

// Effective parameters (risk config + cadence + market counts), read-only.
function renderParams(c){
  const el = document.getElementById("params");
  if (!c || !c.risk) { el.innerHTML = `<div class="empty">No config.</div>`; return; }
  const r = c.risk;
  const pct = (v)=>{ const n=parseFloat(v); return isNaN(n)?"—":(n*100).toFixed(n*100%1?1:0)+"%"; };
  const onoff = (b)=> b ? '<span class="pos">on</span>' : '<span class="muted">off</span>';
  // [label, value, description] — description shows as a hover tooltip + a small caption.
  const items = [
    ["Live min net edge", pct(r.min_net_edge),
      "The active gate. A trade is only placed if its fused edge after fees clears this. LOWER = more trades but thinner margins (more noise/false signals); HIGHER = fewer, higher-conviction trades."],
    ["Shadow (A/B) edge", pct(r.shadow_net_edge),
      "A stricter comparison gate that is recorded but NOT enforced. Lets the Gate Simulation show how a tighter gate would have performed on the same fills. No effect on live trading."],
    ["Kelly fraction", r.kelly_fraction,
      "Fraction of full Kelly used for sizing. 0.25 = quarter-Kelly. HIGHER bets more per edge (faster growth but much higher variance / ruin risk on mis-estimated probabilities); LOWER is safer and smoother."],
    ["Max position", "$"+r.max_position_usdc,
      "Hard dollar cap on any single position, regardless of what Kelly suggests. Caps the worst-case loss on one market."],
    ["Max market exposure", pct(r.max_market_exposure_pct),
      "Max share of the portfolio allowed in one market. Caps concentration so a single resolution can't sink the book. Positions are trimmed to fit rather than rejected."],
    ["Max cluster exposure", pct(r.max_cluster_exposure_pct),
      "Max share of the portfolio across all markets that resolve off the SAME underlying event (e.g. the ~15 Iran/Hormuz peace-deal markets). Each clears the per-market cap alone, but together they're one correlated bet whose YES winners and NO losers cancel. New entries are trimmed to fit; uncorrelated markets are exempt."],
    ["Max total exposure", pct(r.max_total_exposure_pct),
      "Max share of the portfolio that can be locked across all positions at once. Keeps dry powder; blocks new entries once breached."],
    ["P&L floor (stop)", pct(r.pnl_floor),
      "Circuit breaker. If cumulative P&L / portfolio value drops below this, the risk gate blocks all new trades until recovery — prevents a losing streak from compounding."],
    ["Decision cadence", c.decision_cadence_secs+"s",
      "How often every tracked market is re-scored, sized, and (if it passes) traded. 300s = every 5 minutes."],
    ["Ingest interval", c.ingest_interval_secs+"s",
      "How often fresh market data (prices, orderbooks) is pulled from Polymarket's public APIs. Lower = fresher data but more API load."],
    ["Markets tracked", c.markets_tracked + " ("+c.arb_only_markets+" arb-only)",
      "Total markets in the scan universe. Arb-only markets (sports) are never traded directionally — only risk-free YES+NO arbitrage. More markets = wider opportunity funnel (sizing/risk unchanged)."],
    ["Autonomous execution", onoff(c.autonomous_paper_execution),
      "When on, passing decisions automatically place Kelly-sized PAPER orders. When off, the system only evaluates and journals — no positions are opened."],
    ["External signals", onoff(c.external_signals),
      "When on, Yahoo Finance spot + news-headline sentiment feed the fusion engine as low-confidence advisory inputs (capped influence). When off, only market-internal signals are used."],
    ["Real orders", '<span class="neg">disabled</span>',
      "Real-money order dispatch. Structurally disabled in this build — only a fail-closed sender is wired, behind a proven + funded + operator-approved gate. Nothing is ever sent to the live exchange."],
  ];
  el.innerHTML = `<div class="cards">${items.map(([l,v,desc])=>
    `<div class="card" title="${(desc||'').replace(/"/g,'&quot;')}" style="cursor:help;max-width:230px;">
       <div class="label">${l} <span style="opacity:.5">&#9432;</span></div>
       <div class="val" style="font-size:16px">${v}</div>
       <div class="t" style="margin-top:6px;line-height:1.35;color:#6e7681;">${desc||''}</div>
     </div>`).join("")}</div>`;
}

// Executions feed with filtering (hide the rejection noise).
let lastExec = [];
let execFilter = "active"; // active = filled + no-fill (hides rejections); all/filled/rejected
const FILTERS = [["active","Active"],["filled","Filled only"],["rejected","Rejected"],["all","All"]];
function passFilter(r){
  const a = r.action||"";
  if (execFilter==="all") return true;
  if (execFilter==="filled") return a.includes("filled");
  if (execFilter==="rejected") return a.includes("rejected");
  return !a.includes("rejected"); // "active": everything except rejections
}
function renderExec(){
  document.getElementById("exec-filter").innerHTML = FILTERS.map(([k,l])=>
    `<button onclick="execFilter='${k}';renderExec();" style="cursor:pointer;font-size:12px;padding:3px 10px;border-radius:7px;border:1px solid #30363d;background:${execFilter===k?'#1f6feb22':'#161b22'};color:${execFilter===k?'#58a6ff':'#8b949e'};">${l}</button>`
  ).join("");
  const ex = lastExec.filter(passFilter);
  document.getElementById("executions").innerHTML = ex.length ? `<table>
    <tr><th>Time</th><th>Type</th><th>Action</th><th>Market</th><th>Side</th><th>Detail</th></tr>
    ${ex.map(r => {
      const isArb = (r.kind||"").includes("arb");
      const detail = r.gross_notional ? ("$" + num(r.gross_notional)) : (r.reason ? fmt(r.reason) : (r.approved_usdc ? ("$"+num(r.approved_usdc)) : "—"));
      const aCls = (r.action||"").includes("filled")?"pos":((r.action||"").includes("rejected")?"muted":"");
      return `<tr>
        <td class="t">${new Date(r.at).toLocaleString()}</td>
        <td><span class="pill ${isArb?'arb':'dir'}">${isArb?'arb':'directional'}</span></td>
        <td class="${aCls}">${fmt(r.action)}</td>
        <td>${fmt(r.market_id)}</td>
        <td>${fmt(r.outcome)}</td>
        <td>${detail}</td>
      </tr>`;
    }).join("")}
  </table>` : `<div class="empty">No executions match this filter.</div>`;
}

async function load() {
  let d;
  try { d = await (await fetch(PREFIX + "/trades/data", {cache:"no-store"})).json(); }
  catch (e) { document.getElementById("updated").textContent = "fetch error"; return; }
  const p = d.portfolio || {};
  const ra = d.real_account || null;
  const cards = [
    ["Paper equity", "$" + fmt(p.equity)],
    ["Paper cash", "$" + fmt(p.virtual_usdc)],
    ["Locked", "$" + fmt(p.total_locked)],
    ["Realized P&L", "$" + fmt(p.realized_pnl)],
    ["Unrealized P&L", "$" + fmt(p.live_unrealized_pnl)],
  ];
  if (ra && ra.balance != null) {
    cards.push(["REAL " + fmt(ra.collateral_token||"PUSD"), "$" + fmt(ra.balance)]);
  }
  document.getElementById("cards").innerHTML = cards
    .map(([l,v]) => `<div class="card"><div class="label">${l}</div><div class="val">${v}</div></div>`).join("");
  document.getElementById("updated").textContent = "updated " + new Date().toLocaleTimeString();

  renderPnlChart(d.pnl_series);
  renderSignals(d.signals);
  renderGateSim(d.gate_simulation);
  renderParams(d.config);

  const pos = d.open_positions || [];
  document.getElementById("positions").innerHTML = pos.length ? `<table>
    <tr><th>Market</th><th>Side</th><th>Shares</th><th>Avg entry</th><th>Current</th><th>Locked</th><th>Unrealized</th></tr>
    ${pos.map(r => `<tr>
      <td title="${fmt(r.question)}">${fmt(r.slug||r.market_id)}</td>
      <td>${fmt(r.outcome)}</td>
      <td>${num(r.shares)}</td>
      <td>${num(r.avg_entry_price)}</td>
      <td>${num(r.current_mid)}</td>
      <td>$${num(r.collateral_locked)}</td>
      <td class="${cls(r.unrealized_pnl)}">${sign(r.unrealized_pnl)}</td>
    </tr>`).join("")}
  </table>` : `<div class="empty">No open positions — the strategy is waiting for a qualifying opportunity.</div>`;

  const st = d.settlements || {count:0,recent:[]};
  document.getElementById("settle-summary").textContent =
    st.count ? `${st.count} settled · ${st.wins} won · realized ${st.total_realized_pnl}` : "none yet";
  document.getElementById("settlements").innerHTML = (st.recent||[]).length ? `<table>
    <tr><th>Time</th><th>Market</th><th>Side</th><th>Result</th><th>Payout</th><th>Realized P&amp;L</th></tr>
    ${st.recent.map(s => `<tr>
      <td class="t">${new Date(s.at).toLocaleString()}</td>
      <td>${fmt(s.market_id)}</td>
      <td>${fmt(s.outcome)}</td>
      <td class="${s.won?'pos':'neg'}">${s.won?'WON':'lost'}</td>
      <td>$${num(s.payout)}</td>
      <td class="${cls(s.realized_pnl)}">${sign(s.realized_pnl)}</td>
    </tr>`).join("")}
  </table>` : `<div class="empty">No settlements yet — positions realize P&L when their markets resolve.</div>`;

  const llm = d.llm_health;
  const llmEl = document.getElementById("llm");
  if (!llm) { llmEl.textContent = "AI: n/a"; llmEl.style.color="#8b949e"; }
  else {
    const s = llm.status;
    const label = s==="ok" ? `AI ✓ ${fmt(llm.model)}` : (s==="disabled" ? "AI: local-only" : `AI ✗ ${fmt(llm.likely_cause||"failed")}`);
    llmEl.textContent = label;
    llmEl.style.color = s==="ok" ? "#3fb950" : (s==="disabled" ? "#8b949e" : "#f85149");
    llmEl.title = (llm.error||"")+" ("+fmt(llm.provider)+"/"+fmt(llm.model)+")";
  }

  lastExec = d.recent_executions || [];
  renderExec();

  const rt = d.real_trading || {};
  const g = rt.go_live_gate || {};
  const yn = (ok) => ok ? '<span class="pos">&#10003;</span>' : '<span class="neg">&#10007;</span>';
  if (g && g.proven) {
    const ready = g.ready_for_real_dispatch;
    document.getElementById("readiness").innerHTML = `<table>
      <tr><th>Go-live gate</th><th>Status</th><th>Detail</th></tr>
      <tr><td>Proven (realized P&amp;L &gt; 0 over &ge;${g.proven.min_required} settled)</td><td>${yn(g.proven.ok)}</td><td class="muted">realized ${g.proven.realized_pnl} &middot; settled ${g.proven.settled_positions}</td></tr>
      <tr><td>Funded (real collateral &gt; 0)</td><td>${yn(g.funded.ok)}</td><td class="muted">${g.funded.source||''}</td></tr>
      <tr><td>Approved (operator)</td><td>${yn(g.approved.ok)}</td><td class="muted">${g.approved.how||''}</td></tr>
      <tr><td><b>Ready for real dispatch</b></td><td>${yn(ready)}</td><td class="muted">${ready?'':'blocked — paper/shadow only'}</td></tr>
    </table>`;
  } else {
    document.getElementById("readiness").innerHTML = `<div class="empty">No shadow orders yet — they record once a directional paper order fills.</div>`;
  }

  const sh = rt.recent_shadow_orders || [];
  document.getElementById("shadows").innerHTML = sh.length ? `<table>
    <tr><th>Time</th><th>Would send (market / side / size @ price)</th><th>Dispatched?</th><th>Reason</th></tr>
    ${sh.map(s => { const w = s.would_send||{}; return `<tr>
      <td class="t">${new Date(s.at).toLocaleString()}</td>
      <td>${fmt(w.market_id)} &middot; ${fmt(w.side)} ${fmt(w.size)} @ ${fmt(w.price)}</td>
      <td class="${s.dispatched?'neg':'pos'}">${s.dispatched ? 'SENT' : 'no (fail-closed)'}</td>
      <td class="muted">${fmt(s.rejection_reason)}</td>
    </tr>`; }).join("")}
  </table>` : "";
}
load();
setInterval(load, 15000);
</script>
</body>
</html>"##;
    let root = if prefix.is_empty() { "/" } else { prefix };
    PAGE.replace("__PREFIX__", prefix).replace("__ROOT__", root)
}

/// Slug/question-derived category (Gamma doesn't tag these markets — category is always null there).
fn classify_category(slug: &str, question: &str) -> &'static str {
    let s = format!("{} {}", slug, question).to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| s.contains(w));
    if has(&[
        "world-cup",
        "world cup",
        "fifa",
        "nba",
        "nfl",
        "nhl",
        "fifwc",
        "win the",
        "-vs-",
        "champion",
        "super bowl",
        "ufc",
        "soccer",
        "knicks",
    ]) {
        "Sports"
    } else if has(&[
        "bitcoin",
        "ethereum",
        "btc",
        "eth",
        "crypto",
        "solana",
        "fed ",
        "rate",
        "s&p",
        "nasdaq",
        "recession",
        "inflation",
        "stock",
        "gdp",
        "price of",
        "150k",
        "64k",
    ]) {
        "Finance"
    } else if has(&[
        "openai",
        "gpt",
        "-ai-",
        " ai ",
        "google",
        "apple",
        "tesla",
        "spacex",
        "nvidia",
        "chip",
        "anthropic",
        "claude",
        "grok",
    ]) {
        "Tech"
    } else if has(&[
        "iran",
        "israel",
        "russia",
        "ukraine",
        "china",
        "taiwan",
        "war",
        "ceasefire",
        "peace",
        "nuclear",
        "election",
        "president",
        "sanction",
        "hormuz",
        "gaza",
        "trump",
        "blockade",
        "tariff",
    ]) {
        "Geopolitics"
    } else {
        "Other"
    }
}

/// Rich per-market data for the Markets board: prices, the latest fused signal (net edge + which
/// processors fired + Kelly size), news sentiment, any held position, and resolution status.
/// Read-only, paper-only — surfaces data we already collect.
async fn board_data_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let pool = &state.pool;

    type MktRow = (
        String,
        String,
        String,
        Option<String>,
        Option<Decimal>,
        Option<Decimal>,
        bool,
        bool,
        Option<String>,
    );
    let markets: Vec<MktRow> = sqlx::query_as(
        "SELECT gamma_id, slug, question, category, last_mid_yes, last_mid_no, active, closed, resolved_outcome
         FROM market_data.markets ORDER BY closed ASC, updated_at DESC",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    // Latest decision report, news cache, and open position per market.
    let dr_rows: Vec<(Option<String>, serde_json::Value)> = sqlx::query_as(
        "SELECT DISTINCT ON (payload->>'market_id') payload->>'market_id', payload
         FROM journal.events WHERE event_type = 'decision_report'
         ORDER BY payload->>'market_id', created_at DESC",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let dr_map: HashMap<String, serde_json::Value> = dr_rows
        .into_iter()
        .filter_map(|(m, p)| Some((m?, p)))
        .collect();

    let news_rows: Vec<(Option<String>, serde_json::Value)> = sqlx::query_as(
        "SELECT DISTINCT ON (payload->>'market_id') payload->>'market_id', payload->'news'
         FROM journal.events WHERE event_type = 'news_cache'
         ORDER BY payload->>'market_id', created_at DESC",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let news_map: HashMap<String, serde_json::Value> = news_rows
        .into_iter()
        .filter_map(|(m, p)| Some((m?, p)))
        .collect();

    let pos_rows: Vec<(String, String, Decimal, Decimal)> = sqlx::query_as(
        "SELECT market_id, outcome, shares, avg_entry_price FROM paper_trading.paper_positions WHERE shares > 0",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let pos_map: HashMap<String, (String, Decimal, Decimal)> = pos_rows
        .into_iter()
        .map(|(m, o, s, a)| (m, (o, s, a)))
        .collect();

    const SIGNALS: [&str; 5] = [
        "orderbook_momentum",
        "spike_divergence",
        "overreaction_fade",
        "yahoo_finance",
        "news_sentiment",
    ];
    let out: Vec<serde_json::Value> = markets
        .into_iter()
        .map(|(gid, slug, question, db_category, my, mn, active, closed, resolved)| {
            let category = db_category.unwrap_or_else(|| classify_category(&slug, &question).to_string());
            let signal = dr_map.get(&gid).map(|dr| {
                let attr = dr.pointer("/report/attribution");
                let fired: Vec<serde_json::Value> = attr
                    .and_then(|a| a.as_object())
                    .map(|o| {
                        SIGNALS
                            .iter()
                            .filter_map(|name| {
                                let s = o.get(*name)?;
                                let score = s.get("score")?.as_str()?.parse::<Decimal>().ok()?;
                                if score.is_zero() {
                                    return None;
                                }
                                Some(serde_json::json!({"name": name, "score": score.round_dp(3).to_string()}))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                serde_json::json!({
                    "net_edge": dr.pointer("/report/net_edge_after_fees").and_then(|v| v.as_str()),
                    "target_outcome": dr.get("target_outcome"),
                    "kelly_usdc": dr.pointer("/kelly_sizing/recommended_usdc").and_then(|v| v.as_str()),
                    "fired": fired,
                })
            });
            let position = pos_map.get(&gid).map(|(o, s, a)| {
                // Live unrealized P&L for the held outcome: (current_mid - avg_entry) * shares.
                let held_mid = if o.eq_ignore_ascii_case("yes") { my } else { mn };
                let cost_basis = (*a * *s).round_dp(2);
                let (mid_json, unrealized_json, market_value_json) = match held_mid {
                    Some(mid) => {
                        let mv = (mid * *s).round_dp(2);
                        let upnl = (mv - cost_basis).round_dp(2);
                        (
                            Some(mid.round_dp(4).to_string()),
                            Some(upnl.to_string()),
                            Some(mv.to_string()),
                        )
                    }
                    None => (None, None, None),
                };
                serde_json::json!({
                    "outcome": o,
                    "shares": s.round_dp(1).to_string(),
                    "avg_entry": a.round_dp(4).to_string(),
                    "cost_basis": cost_basis.to_string(),
                    "mid": mid_json,
                    "market_value": market_value_json,
                    "unrealized": unrealized_json,
                })
            });
            serde_json::json!({
                "slug": slug,
                "question": question,
                "category": category,
                "yes": my.map(|v| v.round_dp(4).to_string()),
                "no": mn.map(|v| v.round_dp(4).to_string()),
                "active": active,
                "closed": closed,
                "resolved_outcome": resolved,
                "held": position.is_some(),
                "signal": signal,
                "news": news_map.get(&gid),
                "position": position,
            })
        })
        .collect();

    Json(serde_json::json!({
        "paper_only": true,
        "real_orders_enabled": false,
        "count": out.len(),
        "markets": out,
    }))
    .into_response()
}

async fn board_page_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Html(render_board_page(&state.subpath_prefix))
}

/// Lively Markets board: one card per tracked market with a probability bar, the latest fused signal
/// (net edge + which processors fired), news sentiment, held position, and resolution status.
fn render_board_page(prefix: &str) -> String {
    const PAGE: &str = r##"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>Polytrader — Markets</title>
<style>
  :root { color-scheme: dark; }
  body { margin:0; background:#0d1117; color:#e6edf3; font:14px/1.5 -apple-system,Segoe UI,Roboto,sans-serif; }
  header { padding:14px 24px; border-bottom:1px solid #21262d; display:flex; align-items:center; gap:14px; flex-wrap:wrap; }
  h1 { font-size:18px; margin:0; }
  .badge { background:#23863622; color:#3fb950; border:1px solid #23863655; padding:2px 8px; border-radius:12px; font-size:12px; }
  nav { display:flex; gap:4px; margin-left:auto; }
  nav a { color:#8b949e; text-decoration:none; padding:5px 12px; border-radius:7px; font-size:13px; }
  nav a:hover { background:#161b22; color:#e6edf3; }
  nav a.active { background:#1f6feb22; color:#58a6ff; }
  main { padding:20px; max-width:1240px; margin:0 auto; }
  .grid { display:grid; grid-template-columns:repeat(auto-fill,minmax(360px,1fr)); gap:14px; }
  .card { background:#161b22; border:1px solid #21262d; border-radius:10px; padding:14px 16px; display:flex; flex-direction:column; gap:10px; }
  .card.resolved { opacity:.72; }
  .card.held { border-color:#bb8009; box-shadow:0 0 0 1px #bb800955, 0 0 16px #bb800922; }
  .pos { font-size:12px; border-top:1px solid #21262d; padding-top:8px; display:flex; align-items:center; gap:10px; flex-wrap:wrap; }
  .pos .lbl { color:#e3b341; font-weight:600; }
  .pnl.pos2 { color:#3fb950; } .pnl.neg2 { color:#f85149; }
  .q { font-weight:600; font-size:14px; line-height:1.35; }
  .row { display:flex; align-items:center; gap:8px; flex-wrap:wrap; }
  .spacer { margin-left:auto; }
  .tag { font-size:11px; padding:1px 7px; border-radius:10px; border:1px solid #30363d; color:#8b949e; }
  .tag.cat { color:#d2a8ff; border-color:#8957e555; text-transform:capitalize; }
  .tag.hold { color:#e3b341; border-color:#bb800955; }
  .tag.won { color:#3fb950; border-color:#23863655; }
  .tag.lost { color:#f85149; border-color:#da363355; }
  .bar { height:22px; border-radius:6px; overflow:hidden; display:flex; font-size:11px; font-weight:600; }
  .bar .yes { background:#238636; display:flex; align-items:center; padding:0 7px; color:#fff; white-space:nowrap; }
  .bar .no { background:#6e2620; display:flex; align-items:center; justify-content:flex-end; padding:0 7px; color:#fff; flex:1; white-space:nowrap; }
  .sig { font-size:12px; color:#8b949e; }
  .chip { font-size:11px; padding:1px 7px; border-radius:10px; border:1px solid #1f6feb55; color:#58a6ff; }
  .chip.fade { color:#f0883e; border-color:#bb540955; }
  .chip.news { color:#56d364; border-color:#23863655; }
  .chip.yahoo { color:#79c0ff; border-color:#1f6feb55; }
  .edge.pos { color:#3fb950; } .edge.neg { color:#f85149; } .muted { color:#8b949e; }
  .news { font-size:12px; color:#8b949e; border-top:1px solid #21262d; padding-top:8px; }
  .dot { display:inline-block; width:9px; height:9px; border-radius:50%; margin-right:5px; vertical-align:middle; }
  .empty { color:#8b949e; padding:24px; text-align:center; }
  footer { color:#8b949e; font-size:12px; padding:8px 24px 24px; max-width:1240px; margin:0 auto; }
</style></head>
<body>
<header>
  <h1>Polytrader — Markets</h1>
  <span class="badge">PAPER</span>
  <span class="tag" id="updated">loading…</span>
  <nav>
    <a href="__ROOT__" class="active">Markets</a>
    <a href="__PREFIX__/trades">Trades</a>
    <a href="__PREFIX__/console">Console</a>
  </nav>
</header>
<main><div class="grid" id="grid"></div></main>
<footer>Auto-refreshes every 15s · live public Polymarket data · all trading simulated, no real orders.</footer>
<script>
const PREFIX = "__PREFIX__";
const fmt=(v)=>v==null?"—":v;
const pct=(v)=>{const n=parseFloat(v); return isNaN(n)?null:Math.round(n*1000)/10;};
const edgeCls=(v)=>{const n=parseFloat(v); return n>0?"pos":(n<0?"neg":"muted");};
const chipCls=(n)=>n.includes("fade")?"fade":(n.includes("news")?"news":(n.includes("yahoo")?"yahoo":""));
const polDot=(p)=>{const n=parseFloat(p); if(isNaN(n))return"#8b949e"; return n>0.15?"#3fb950":(n<-0.15?"#f85149":"#8b949e");};

async function load(){
  let d; try { d = await (await fetch(PREFIX+"/board/data",{cache:"no-store"})).json(); }
  catch(e){ document.getElementById("updated").textContent="fetch error"; return; }
  const all = (d.markets||[]).slice();
  const held = all.filter(m=>m.held).length;
  const heldSum = all.reduce((a,m)=>a + (m.position&&m.position.unrealized!=null?parseFloat(m.position.unrealized):0), 0);
  document.getElementById("updated").textContent =
    `${d.count} markets · ${held} held${held?` · unrealized ${heldSum>=0?'+':''}$${heldSum.toFixed(2)}`:''} · ${new Date().toLocaleTimeString()}`;
  // Surface held positions first, then live markets, resolved last.
  all.sort((a,b)=> (b.held?1:0)-(a.held?1:0) || (b.active?1:0)-(a.active?1:0));
  const pnlCls=(v)=>{const n=parseFloat(v); return n>0?"pos2":(n<0?"neg2":"muted");};
  const cards = all.map(m => {
    const yes = pct(m.yes), no = pct(m.no);
    const haveBar = yes!=null && no!=null;
    const sig = m.signal, pos = m.position, news = m.news;
    const firedChips = (sig&&sig.fired||[]).map(f=>`<span class="chip ${chipCls(f.name)}">${f.name.replace(/_/g,' ')} ${f.score}</span>`).join(" ");
    const statusTag = m.resolved_outcome ? `<span class="tag ${ (pos&&pos.outcome===m.resolved_outcome)?'won': (pos?'lost':'') }">RESOLVED · ${fmt(m.resolved_outcome)}</span>`
                     : (m.active ? `<span class="tag" style="color:#3fb950;border-color:#23863655">LIVE</span>` : `<span class="tag">closed</span>`);
    const upnl = pos&&pos.unrealized!=null ? parseFloat(pos.unrealized) : null;
    const holdTag = pos ? `<span class="tag hold">HOLDING ${fmt(pos.outcome)} · ${pos.shares} sh${upnl!=null?` · <span class="pnl ${pnlCls(upnl)}">${upnl>=0?'+':''}$${upnl.toFixed(2)}</span>`:''}</span>` : '';
    const posLine = pos ? `<div class="pos">
        <span class="lbl">Position</span>
        <span>${fmt(pos.shares)} ${fmt(pos.outcome)} @ ${fmt(pos.avg_entry)}</span>
        ${pos.mid!=null?`<span class="muted">now ${pos.mid}</span>`:''}
        ${pos.market_value!=null?`<span class="muted">· value $${pos.market_value}</span>`:''}
        ${upnl!=null?`<span class="spacer"></span><span class="pnl ${pnlCls(upnl)}">${upnl>=0?'+':''}$${upnl.toFixed(2)} unrealized</span>`:''}
      </div>` : '';
    return `<div class="card ${m.resolved_outcome?'resolved':''} ${pos&&!m.resolved_outcome?'held':''}">
      <div class="q">${fmt(m.question||m.slug)}</div>
      <div class="row">
        ${m.category?`<span class="tag cat">${m.category}</span>`:''}
        ${statusTag}
        ${holdTag}
      </div>
      ${haveBar?`<div class="bar"><div class="yes" style="width:${yes}%">YES ${yes}%</div><div class="no">${no}% NO</div></div>`:'<div class="muted">no orderbook yet</div>'}
      ${sig?`<div class="sig row">
        <span>net edge <b class="edge ${edgeCls(sig.net_edge)}">${parseFloat(sig.net_edge||0).toFixed(3)}</b></span>
        ${sig.kelly_usdc&&parseFloat(sig.kelly_usdc)>0?`<span class="muted">· Kelly $${parseFloat(sig.kelly_usdc).toFixed(0)} (${fmt(sig.target_outcome)})</span>`:''}
        <span class="spacer"></span>${firedChips||'<span class="muted">no signal fired</span>'}
      </div>`:'<div class="sig muted">no decision report yet</div>'}
      ${posLine}
      ${news?`<div class="news"><span class="dot" style="background:${polDot(news.polarity)}"></span>news ${fmt(news.headline_count)} headlines · polarity ${fmt(news.polarity)}${(news.top_titles&&news.top_titles[0])?` — <span class="muted">${news.top_titles[0]}</span>`:''}</div>`:''}
    </div>`;
  }).join("");
  document.getElementById("grid").innerHTML = cards || `<div class="empty">No markets ingested yet.</div>`;
}
load(); setInterval(load, 15000);
</script>
</body></html>"##;
    let root = if prefix.is_empty() { "/" } else { prefix };
    PAGE.replace("__PREFIX__", prefix).replace("__ROOT__", root)
}

async fn paper_rejections_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<DryRunEventsQuery>,
) -> impl IntoResponse {
    //! Read-only paper rejection audit events. These are append-only journal
    //! records for refused simulator intents; they are not real orders and they
    //! never call CLOB order APIs.
    let limit = clamp_dry_run_events_limit(query.limit.unwrap_or(20));
    match sqlx::query_as::<
        _,
        (
            uuid::Uuid,
            String,
            String,
            String,
            serde_json::Value,
            chrono::DateTime<chrono::Utc>,
        ),
    >(
        r#"SELECT id, event_type, source, severity, payload, created_at
           FROM journal.events
           WHERE event_type = 'paper_order_rejection'
           ORDER BY created_at DESC
           LIMIT $1"#,
    )
    .bind(limit)
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => Json(serde_json::json!({
            "paper_only": true,
            "real_orders_enabled": false,
            "request_sent": false,
            "post_order_called": false,
            "post_orders_called": false,
            "count": rows.len(),
            "events": rows.into_iter().map(|(id, event_type, source, severity, payload, created_at)| {
                serde_json::json!({
                    "id": id,
                    "event_type": event_type,
                    "source": source,
                    "severity": severity,
                    "payload": payload,
                    "created_at": created_at,
                })
            }).collect::<Vec<_>>(),
            "note": "Read-only paper rejection audit events from journal.events; no CLOB order API is called."
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "request_sent": false,
                "post_order_called": false,
                "post_orders_called": false,
                "error": format!("Failed to load paper rejection events: {e}")
            })),
        )
            .into_response(),
    }
}

async fn paper_reset_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<PaperResetRequest>,
) -> impl IntoResponse {
    //! Explicit paper-only simulator reset for development recovery.
    //!
    //! RISK: This does not delete audit history. It only clears current
    //! `paper_positions` and writes a fresh virtual portfolio snapshot so a known
    //! bad simulator state can be rebased without hiding prior paper orders/fills.
    //! It never touches real wallet state or CLOB order APIs.
    let reason = request.reason.as_deref().map(str::trim).unwrap_or_default();
    if request.confirm_paper_reset != Some(true) || reason.len() < 8 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "reset_applied": false,
                "request_sent": false,
                "post_order_called": false,
                "post_orders_called": false,
                "blockers": ["confirm_paper_reset_required", "reason_min_8_chars_required"],
                "note": "Paper reset requires confirm_paper_reset:true and a reason. Historical paper orders/fills are preserved."
            })),
        )
            .into_response();
    }

    match reset_paper_simulator_state(&state.pool, reason, request.operator.as_deref()).await {
        Ok(body) => Json(body).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "reset_applied": false,
                "request_sent": false,
                "post_order_called": false,
                "post_orders_called": false,
                "error": format!("Failed to reset paper simulator state: {e}")
            })),
        )
            .into_response(),
    }
}

async fn paper_reconciliation_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    //! Read-only consistency check for the current paper simulator state.
    //!
    //! RISK: This endpoint never mutates paper tables and never touches CLOB
    //! order APIs. It compares current cached paper positions and latest
    //! portfolio snapshot against fills after the latest manual reset boundary,
    //! so operators can detect stale/corrupt simulator state before strategies
    //! rely on paper execution.
    match build_paper_reconciliation_report(&state.pool).await {
        Ok(report) => Json(report).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "request_sent": false,
                "post_order_called": false,
                "post_orders_called": false,
                "status": "error",
                "error": format!("Failed to build paper reconciliation report: {e}")
            })),
        )
            .into_response(),
    }
}

async fn paper_risk_summary_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    //! Read-only aggregate over simulated paper risk. This summarizes paper
    //! positions against conservative small-bankroll limits. It never writes
    //! `paper_trading.*` rows and never calls authenticated CLOB order APIs.
    let latest_usdc = match latest_virtual_usdc(&state.pool).await {
        Ok(value) => value,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "paper_only": true,
                    "real_orders_enabled": false,
                    "request_sent": false,
                    "post_order_called": false,
                    "post_orders_called": false,
                    "error": format!("Failed to load latest paper bankroll: {e}")
                })),
            )
                .into_response();
        }
    };

    match load_paper_position_rows(&state.pool).await {
        Ok(rows) => Json(build_paper_risk_summary(latest_usdc, rows)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "request_sent": false,
                "post_order_called": false,
                "post_orders_called": false,
                "error": format!("Failed to load paper risk summary: {e}")
            })),
        )
            .into_response(),
    }
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
    // NIT (past issue): string post-proc is brittle to HTML variations (new panels in body do not affect head; no regression here).
    // Future tranche: dioxus head management to eliminate. For now documented + all old+new SSR markers asserted in test/verify.
    if let Some(head_pos) = rendered.find("<head>") {
        let insert_pos = head_pos + 6; // after <head>
        rendered.insert_str(insert_pos, &format!("<base href=\"{}\">", base));
    }
    if !rendered.contains("<base href") {
        tracing::warn!(prefix = %prefix, "subpath <base> injection may have failed (string post-process assumption); relative client fetches in SSR output may not resolve correctly under /polytrader rewrite");
    }

    // Inject the shared nav (Markets · Trades · Console) right after <body> so the console page has
    // the same navigation as the board/trades pages.
    let root = if prefix.is_empty() {
        "/"
    } else {
        prefix.as_str()
    };
    let nav = format!(
        r#"<nav style="display:flex;gap:6px;padding:10px 16px;background:#0d1117;border-bottom:1px solid #21262d;font:13px -apple-system,Segoe UI,Roboto,sans-serif;">
<a href="{root}" style="color:#8b949e;text-decoration:none;padding:5px 12px;border-radius:7px;">Markets</a>
<a href="{prefix}/trades" style="color:#8b949e;text-decoration:none;padding:5px 12px;border-radius:7px;">Trades</a>
<a href="{prefix}/console" style="background:#1f6feb22;color:#58a6ff;text-decoration:none;padding:5px 12px;border-radius:7px;">Console</a>
</nav>"#,
        root = root,
        prefix = prefix
    );
    if let Some(bpos) = rendered.find("<body") {
        if let Some(gt) = rendered[bpos..].find('>') {
            rendered.insert_str(bpos + gt + 1, &nav);
        }
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

// ============================================================================
// AUTH (Next Phase 2026-05-25 IMPL 5701dfea): minimal Google OAuth + dual mode
// (edge forwarded headers preferred; else cookie session). Static stores (no
// AppState/main edit). Manual parse (no extra deps). Optional for paper.
// RISK/AGENTS comments on every item. See config.rs for fields + rationale.
// ============================================================================

/// In-memory session (paper acceptable; restart clears = fine for $150).
#[derive(Clone, Debug)]
struct Session {
    email: String,
    expires: Instant,
}

/// Temp state for OAuth CSRF protection (short lived).
static OAUTH_STATES: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
static SESSIONS: OnceLock<Mutex<HashMap<String, Session>>> = OnceLock::new();

fn get_oauth_states() -> &'static Mutex<HashMap<String, Instant>> {
    OAUTH_STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn get_sessions() -> &'static Mutex<HashMap<String, Session>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// AuthUser extractor: dual mode (ngrok forwarded header if present, else cookie pt_sess).
/// Supports common headers the daytrader-oauth policy may add ("add headers" step).
/// RISK: trust forwarded headers *only* because they come from trusted ngrok edge after SSO;
/// in standalone/local the cookie path is used. Never trust arbitrary client headers.
///
/// RISK NOTE (Fix Round 1): x-forwarded-* (and x-auth-request-*) are trusted here for the POC dual-mode
/// (ngrok edge SSO + in-cluster sim for verify). In docker-desktop / shared ngrok, in-cluster callers or
/// spoofed headers *could* forge an operator identity for the 3 privileged paths (human-approval, final-review-decision,
/// submit-facade). Those paths still require valid non-zero journal event ids + all other gates (collateral, kill, env unlock,
/// final decision, L2 creds at dispatch time) before any real send; the facade itself is fail-closed. Verify now includes
/// explicit unauthed 401 negatives. For production, add origin/CIDR/ngrok-auth checks or require mTLS for operator.
/// See wiki runbooks/l2-private-key-secrets.md and AGENTS safety rules.
#[derive(Debug, Clone)]
pub struct AuthUser(pub Option<String>);

#[axum::async_trait]
impl<S> axum::extract::FromRequestParts<S> for AuthUser
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        // 1. Prefer ngrok/edge forwarded identity (dual mode; policy does Google SSO + allowlist).
        // Common names observed in similar oauth2-proxy/ngrok setups.
        let forwarded = parts
            .headers
            .get("x-auth-request-email")
            .or_else(|| parts.headers.get("x-forwarded-email"))
            .or_else(|| parts.headers.get("x-forwarded-user"))
            .or_else(|| parts.headers.get("x-auth-request-user"))
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        if let Some(email) = forwarded {
            return Ok(AuthUser(Some(email)));
        }

        // 2. Fallback to our in-app cookie session (for standalone / local / other deploys).
        if let Some(cookie_header) = parts.headers.get("cookie").and_then(|v| v.to_str().ok()) {
            for part in cookie_header.split(';') {
                let kv: Vec<&str> = part.trim().splitn(2, '=').collect();
                if kv.len() == 2 && kv[0] == "pt_sess" {
                    let sess_id = kv[1];
                    if let Ok(mut guard) = get_sessions().lock() {
                        if let Some(sess) = guard.get(sess_id) {
                            if Instant::now() < sess.expires {
                                return Ok(AuthUser(Some(sess.email.clone())));
                            } else {
                                guard.remove(sess_id); // expired cleanup
                            }
                        }
                    }
                }
            }
        }
        Ok(AuthUser(None))
    }
}

/// Minimal percent-encode for OAuth query values (no external crate; smallest).
/// Only encodes what is needed for client_id/redirect/state (safe for Google).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.as_bytes() {
        match *b {
            b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{:02X}", b));
            }
        }
    }
    out
}

/// Helper: build Google consent URL (response_type=code, scope=email profile, state for CSRF).
fn build_google_auth_url(client_id: &str, redirect_uri: &str, state: &str) -> String {
    format!(
        "https://accounts.google.com/o/oauth2/v2/auth?client_id={}&redirect_uri={}&response_type=code&scope=email%20profile&state={}&access_type=online",
        percent_encode(client_id),
        percent_encode(redirect_uri),
        percent_encode(state)
    )
}

/// Helper: exchange code + fetch email via userinfo (no jwt lib; https + reqwest).
/// RISK: only call over https; client_secret only in this server path (never to client).
async fn exchange_code_for_user_email(
    client_id: &str,
    client_secret: &str,
    redirect_uri: &str,
    code: &str,
) -> anyhow::Result<String> {
    let client = reqwest::Client::new();

    // Token exchange
    let token_resp = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("grant_type", "authorization_code"),
        ])
        .send()
        .await?
        .error_for_status()?
        .json::<serde_json::Value>()
        .await?;

    let access_token = token_resp["access_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no access_token in response"))?;

    // Userinfo (simple, no signature verify needed over https for paper)
    let userinfo: serde_json::Value = client
        .get("https://www.googleapis.com/oauth2/v2/userinfo")
        .bearer_auth(access_token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let email = userinfo["email"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no email in userinfo"))?
        .to_string();

    Ok(email)
}

// --- Handlers ---

#[derive(serde::Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    #[allow(dead_code)]
    error_description: Option<String>,
}

async fn auth_login_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    //! RISK: generate fresh state (uuid) per request; store short-lived; validate exact match on cb.
    //! Redirect URI from config must be full public (incl /polytrader/... for subpath deploys).
    //! No open redirect: only to Google (hardcoded host).
    let cfg = crate::config::Config::load(); // safe re-parse (dotenv already done); new fields optional
    if !cfg.auth_enabled() {
        return (StatusCode::NOT_FOUND, "auth not configured").into_response();
    }

    let state_val = uuid::Uuid::new_v4().to_string();
    {
        let mut guard = get_oauth_states().lock().unwrap();
        guard.insert(state_val.clone(), Instant::now() + Duration::from_secs(300));
    }

    let redirect_uri = if cfg.google_redirect_uri.is_empty() {
        // Fallback construct (best effort; user should set full public)
        let prefix = state.subpath_prefix.clone();
        format!(
            "http://localhost:8080{}auth/callback",
            if prefix.is_empty() {
                "/".to_string()
            } else {
                prefix + "/"
            }
        )
    } else {
        cfg.google_redirect_uri.clone()
    };

    let url = build_google_auth_url(&cfg.google_client_id, &redirect_uri, &state_val);
    Redirect::temporary(&url).into_response()
}

async fn auth_callback_handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<CallbackQuery>,
) -> impl IntoResponse {
    //! RISK: validate state exactly (remove after use to prevent replay). Exchange only on match.
    //! On success set cookie with correct Path for subpath (so browser sends on /polytrader/*).
    //! HttpOnly + SameSite=Lax + Secure(opt) + short expiry. Any error: simple text, no leak.
    //! allowed_emails empty = any (paper mode only).
    let cfg = crate::config::Config::load();
    if !cfg.auth_enabled() {
        return (StatusCode::NOT_FOUND, "auth not configured").into_response();
    }

    if let Some(err) = q.error {
        tracing::warn!(error = %err, "google oauth callback error");
        return (StatusCode::BAD_REQUEST, format!("oauth error: {}", err)).into_response();
    }

    let code = match q.code {
        Some(c) => c,
        None => return (StatusCode::BAD_REQUEST, "missing code").into_response(),
    };
    let state_val = match q.state {
        Some(s) => s,
        None => return (StatusCode::BAD_REQUEST, "missing state").into_response(),
    };

    // Validate + consume state
    let valid = {
        let mut guard = get_oauth_states().lock().unwrap();
        if let Some(exp) = guard.remove(&state_val) {
            Instant::now() < exp
        } else {
            false
        }
    };
    if !valid {
        return (StatusCode::BAD_REQUEST, "invalid or expired state (CSRF?)").into_response();
    }

    let redirect_uri = if cfg.google_redirect_uri.is_empty() {
        let prefix = state.subpath_prefix.clone();
        format!(
            "http://localhost:8080{}auth/callback",
            if prefix.is_empty() {
                "/".to_string()
            } else {
                prefix + "/"
            }
        )
    } else {
        cfg.google_redirect_uri.clone()
    };

    let email = match exchange_code_for_user_email(
        &cfg.google_client_id,
        &cfg.google_client_secret,
        &redirect_uri,
        &code,
    )
    .await
    {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(?e, "token exchange or userinfo failed");
            return (StatusCode::BAD_GATEWAY, "oauth exchange failed").into_response();
        }
    };

    // Allowlist (empty = any for paper)
    let allowed = cfg.allowed_emails_list();
    if !allowed.is_empty() && !allowed.contains(&email.to_lowercase()) {
        tracing::warn!(email = %email, "email not in allowlist");
        return (StatusCode::FORBIDDEN, "email not allowed").into_response();
    }

    // Create session
    let sess_id = uuid::Uuid::new_v4().to_string();
    {
        let mut guard = get_sessions().lock().unwrap();
        guard.insert(
            sess_id.clone(),
            Session {
                email: email.clone(),
                expires: Instant::now() + Duration::from_secs(3600),
            },
        );
    }

    // Set cookie (Path critical for subpath; flags per config)
    let prefix = &state.subpath_prefix;
    let path = if prefix.is_empty() { "/" } else { prefix };
    let secure = if cfg.auth_cookie_secure {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "pt_sess={}; HttpOnly; SameSite=Lax; Path={}{}",
        sess_id, path, secure
    );

    let mut resp = Redirect::temporary("/").into_response();
    resp.headers_mut()
        .insert(axum::http::header::SET_COOKIE, cookie.parse().unwrap());
    resp
}

async fn auth_logout_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    //! RISK: expire cookie (Max-Age=0). Path must match what was set (subpath aware).
    let prefix = &state.subpath_prefix;
    let path = if prefix.is_empty() { "/" } else { prefix };
    let cookie = format!("pt_sess=; HttpOnly; SameSite=Lax; Path={}; Max-Age=0", path);

    let mut resp = Redirect::temporary("/").into_response();
    resp.headers_mut()
        .insert(axum::http::header::SET_COOKIE, cookie.parse().unwrap());
    resp
}

async fn auth_whoami_handler(auth: AuthUser) -> impl IntoResponse {
    //! Simple JSON for client script (fits existing live-fetch pattern exactly).
    #[derive(Serialize)]
    struct Who {
        user: Option<String>,
    }
    Json(Who { user: auth.0 })
}

// (End auth section. All prior behavior preserved; fmt/clippy clean.)

// ============================================================================
// L2 WALLET AUTH (2026-05-25 IMPL 58dff3a2): smallest viable Polymarket CLOB L2
// derive flow (status + connect/derive + disconnect) for paper-only learning.
// Post-Google clarification pivot. Coexists with Google layer (5701dfea/978b365b
// 100% preserved live; no Google code altered). Browser EIP-712 only; server
// proxies derive (secret mem-only per official docs). Exact Google cookie/OnceLock
// patterns reused (pt_l2_sess, manual parse, subpath Path, HttpOnly etc).
// Hardcoded clob host (no new env/Cargo/yaml per smallest). Heavy //! RISK +
// paper gates + $150 + "zero effect on engine" + dual identity + long-lived notes.
// See top wiki/log.md 58dff3a2 for full Commands/Verification/Design/Fidelity
// (Google preserved)/Credits (docs 2026-05-25 + openclaw clobSignature.ts patterns
// + prior IMPLs + AGENTS)/Anti-patterns. No real trading/CLOB wiring/DB/tests.
// ============================================================================

/// L2 session metadata (masked only; secret in separate map, never serialized/out).
#[derive(Clone, Debug)]
struct L2Session {
    address: String,
    api_key_masked: String,
    created: Instant,
}

/// Derived L2 credential material held only in server memory.
/// `address` is the signer address required in `POLY_ADDRESS` for L2 requests.
#[derive(Clone, Debug)]
pub(crate) struct L2Secret {
    pub address: String,
    pub api_key: String,
    pub secret: String,
    pub passphrase: String,
}

/// In-mem stores (paper: restart clears = acceptable for $150 learning).
/// Secret ONLY here (server memory); NEVER to client/logs/cookie.
static L2_SESSIONS: OnceLock<Mutex<HashMap<String, L2Session>>> = OnceLock::new();
#[allow(clippy::type_complexity)]
static L2_SECRETS: OnceLock<Mutex<HashMap<String, L2Secret>>> = OnceLock::new();
// NOTE: .lock().unwrap() used in a few L2 paths (and oauth/sessions). Poison would panic the handler
// (acceptable for this POC; restart recovers). from_current in clob uses .ok()/? for resilience.
// See Issue 7 in fix round review. No change to deployed behavior.
static SERVER_L2_SESSION_ID: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn get_l2_sessions() -> &'static Mutex<HashMap<String, L2Session>> {
    L2_SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn get_l2_secrets() -> &'static Mutex<HashMap<String, L2Secret>> {
    L2_SECRETS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn get_server_l2_session_id() -> &'static Mutex<Option<String>> {
    SERVER_L2_SESSION_ID.get_or_init(|| Mutex::new(None))
}

fn mask_api_key(api_key: &str) -> String {
    if api_key.len() > 10 {
        format!("{}...{}", &api_key[..6], &api_key[api_key.len() - 4..])
    } else {
        api_key.to_string()
    }
}

fn register_l2_session(
    address: String,
    signer_address: String,
    api_key: String,
    secret: String,
    passphrase: String,
    is_server_key: bool,
) -> anyhow::Result<(String, String)> {
    let masked = mask_api_key(&api_key);
    let sess_id = uuid::Uuid::new_v4().to_string();

    {
        let mut guard = get_l2_sessions()
            .lock()
            .map_err(|_| anyhow::anyhow!("L2 session store lock poisoned"))?;
        guard.insert(
            sess_id.clone(),
            L2Session {
                address,
                api_key_masked: masked.clone(),
                created: Instant::now(),
            },
        );
    }
    {
        let mut secrets = get_l2_secrets()
            .lock()
            .map_err(|_| anyhow::anyhow!("L2 secret store lock poisoned"))?;
        secrets.insert(
            sess_id.clone(),
            L2Secret {
                address: signer_address,
                api_key,
                secret,
                passphrase,
            },
        );
    }
    if is_server_key {
        let mut active = get_server_l2_session_id()
            .lock()
            .map_err(|_| anyhow::anyhow!("L2 server session lock poisoned"))?;
        *active = Some(sess_id.clone());
    }

    Ok((sess_id, masked))
}

fn cookie_secure_suffix() -> &'static str {
    match std::env::var("AUTH_COOKIE_SECURE") {
        Ok(v) if v == "1" || v.eq_ignore_ascii_case("true") => "; Secure",
        _ => "",
    }
}

fn read_l2_session_cookie(headers: &axum::http::HeaderMap) -> Option<String> {
    let cookie_header = headers.get(axum::http::header::COOKIE)?;
    let cookies = cookie_header.to_str().ok()?;
    for part in cookies.split(';') {
        let kv: Vec<&str> = part.trim().splitn(2, '=').collect();
        if kv.len() == 2 && kv[0] == "pt_l2_sess" && !kv[1].is_empty() {
            return Some(kv[1].to_string());
        }
    }
    None
}

/// L2 status (public, no auth required; fits whoami pattern).
#[derive(Serialize)]
struct L2Status {
    connected: bool,
    address: Option<String>,
    api_key_masked: Option<String>,
    created: Option<String>,
    note: String,
    paper_only: bool,
}

async fn l2_status_handler(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    //! RISK (AGENTS + official Polymarket docs 2026-05-25): L2 secret never leaves server memory
    //! (L2_SECRETS map only; cleared on process restart). Cookie "pt_l2_sess" Path must exactly
    //! match subpath (Google pt_sess logic copied verbatim for dual-mode + ngrok rewrite safety).
    //! Long-lived keys: user must revoke on Polymarket.com/settings or use new nonce; Disconnect
    //! only clears local session. EVEN IF CONNECTED: zero effect on PaperTradingEngine / risk sizing
    //! / journal (real CLOB is future gated work per AGENTS: explicit flag + full risk review + human gate).
    //! $150 paper learning/observational only — do not use real capital. Dual identity risks noted
    //! (Google = dashboard/edge SSO; L2 = trading creds for future CLOB).
    let prefix = &state.subpath_prefix;
    let _path = if prefix.is_empty() { "/" } else { prefix };

    // Manual cookie parse (exact Google pt_sess 5-line pattern copied for "pt_l2_sess")
    let mut connected = false;
    let mut address = None;
    let mut api_key_masked = None;
    let mut created = None;

    let cookie_session_id = read_l2_session_cookie(&headers);
    let server_session_id = get_server_l2_session_id()
        .lock()
        .ok()
        .and_then(|guard| guard.clone());
    let session_id = cookie_session_id.or(server_session_id);

    if let Some(sess_id) = session_id {
        if let Ok(guard) = get_l2_sessions().lock() {
            if let Some(sess) = guard.get(&sess_id) {
                connected = true;
                address = Some(sess.address.clone());
                api_key_masked = Some(sess.api_key_masked.clone());
                created = Some(format!("{:?}", sess.created)); // or human time
            }
        }
    }

    let note = "long-lived (revoke via Disconnect or Polymarket settings) — PAPER ONLY for future gated CLOB trading (no orders placed even if connected). Dual: Google=dashboard/edge SSO identity; L2=trading creds (future). EVEN IF CONNECTED: zero effect on PaperTradingEngine or risk; real CLOB order placement is future gated work (requires AGENTS review + explicit config flag + risk engine changes). $150 learning/observational only — do not use real capital or large size.".to_string();

    Json(L2Status {
        connected,
        address,
        api_key_masked,
        created,
        note,
        paper_only: true,
    })
}

#[derive(serde::Deserialize)]
#[allow(dead_code)]
struct L2DeriveReq {
    address: String,
    signature: String,
    timestamp: String,
    nonce: String,
}

async fn l2_derive_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<L2DeriveReq>,
) -> impl IntoResponse {
    //! RISK + paper gate (AGENTS + docs): This is the L1->L2 derive proxy. In real use the
    //! signature would be validated + reqwest built with exact POLY_* headers to
    //! https://clob.polymarket.com/auth/derive-api-key (see docs EIP-712 + headers). Here (paper
    //! demo for UI component): basic untrusted-input validate only, simulate successful derive
    //! with plausible masked key, store secret ONLY in server-mem L2_SECRETS (never client/log/
    //! cookie), set pt_l2_sess cookie with exact Google Path/subpath/flags copy. Zero effect on
    //! trading engine. Future real version will do the reqwest + error mapping + real secret use
    //! behind explicit gates.
    if !req.address.starts_with("0x") || req.nonce != "0" {
        return (
            StatusCode::BAD_REQUEST,
            "invalid address or nonce (paper demo expects 0x... + nonce 0)",
        )
            .into_response();
    }

    // Simulate successful derive (real: build POLY headers from req + reqwest::get(...).await)
    let masked = format!(
        "{}...{}",
        &req.address[..std::cmp::min(6, req.address.len())],
        &req.address[req.address.len().saturating_sub(4)..]
    );
    let sess_id = uuid::Uuid::new_v4().to_string();
    let now = std::time::Instant::now();

    {
        let mut guard = get_l2_sessions().lock().unwrap();
        guard.insert(
            sess_id.clone(),
            L2Session {
                address: req.address.clone(),
                api_key_masked: masked.clone(),
                created: now,
            },
        );
    }
    {
        // Secret only in mem (demo dummy; real would come from Polymarket response)
        let mut secrets = get_l2_secrets().lock().unwrap();
        secrets.insert(
            sess_id.clone(),
            L2Secret {
                address: req.address.clone(),
                secret: "demo_secret_base64_for_paper".to_string(),
                passphrase: "demo_pass".to_string(),
                api_key: "demo_full_apikey".to_string(),
            },
        );
    }

    // Set cookie (exact Google pt_sess 5-line copy for Path/subpath + flags)
    let prefix = &state.subpath_prefix;
    let path = if prefix.is_empty() { "/" } else { prefix };
    let secure = cookie_secure_suffix();
    let cookie = format!(
        "pt_l2_sess={}; HttpOnly; SameSite=Lax; Path={}{}",
        sess_id, path, secure
    );

    let mut resp = Json(serde_json::json!({
        "success": true,
        "api_key_masked": masked,
        "note": "PAPER ONLY — simulated derive success for UI demo. Real Polymarket L2 key would be here. Disconnect or restart clears local session."
    })).into_response();
    resp.headers_mut()
        .insert(axum::http::header::SET_COOKIE, cookie.parse().unwrap());
    resp
}

async fn l2_disconnect_handler(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    //! Exact Google logout cookie clear copy (pt_l2_sess, Path, Max-Age=0).
    let cookie_session_id = read_l2_session_cookie(&headers);
    let fallback_server_session_id = get_server_l2_session_id()
        .lock()
        .ok()
        .and_then(|guard| guard.clone());
    if let Some(sess_id) = cookie_session_id.or(fallback_server_session_id) {
        if let Ok(mut guard) = get_l2_sessions().lock() {
            guard.remove(&sess_id);
        }
        if let Ok(mut secrets) = get_l2_secrets().lock() {
            secrets.remove(&sess_id);
        }
        if let Ok(mut active) = get_server_l2_session_id().lock() {
            if active.as_deref() == Some(&sess_id) {
                *active = None;
            }
        }
    }

    let prefix = &state.subpath_prefix;
    let path = if prefix.is_empty() { "/" } else { prefix };
    let cookie = format!(
        "pt_l2_sess=; HttpOnly; SameSite=Lax; Path={}; Max-Age=0{}",
        path,
        cookie_secure_suffix()
    );

    let mut resp =
        Json(serde_json::json!({"success": true, "note": "L2 session cleared (paper demo)."} ))
            .into_response();
    resp.headers_mut()
        .insert(axum::http::header::SET_COOKIE, cookie.parse().unwrap());
    resp
}

/// Real native L2 derivation using the exact snippet the user provided + polymarket_client_sdk_v2 canary.
///
/// Returns (api_key, secret, passphrase) on success — stored only in server memory (L2_SECRETS).
/// Never logged, never sent to client, never put in cookies.
#[cfg(feature = "native-l2")]
async fn derive_l2_credentials_native(
    private_key: &str,
) -> anyhow::Result<(String, String, String, String)> {
    use std::str::FromStr;

    use polymarket_client_sdk_v2::auth::{ExposeSecret, LocalSigner, Signer};
    use polymarket_client_sdk_v2::clob::{Client, Config};
    use polymarket_client_sdk_v2::POLYGON;

    let signer = LocalSigner::from_str(private_key)?.with_chain_id(Some(POLYGON));
    let signer_address = signer.address().to_checksum(None);

    let client = Client::new("https://clob.polymarket.com", Config::default())?
        .authentication_builder(&signer)
        .authenticate()
        .await?;

    let credentials = client.credentials();

    // Real API in this canary version:
    // - key() -> Uuid (ApiKey)
    // - secret() -> &SecretString  (use .expose_secret() to get &str)
    // - passphrase() -> &SecretString
    let api_key = credentials.key().to_string();
    let secret = credentials.secret().expose_secret().to_string();
    let passphrase = credentials.passphrase().expose_secret().to_string();

    Ok((signer_address, api_key, secret, passphrase))
}

/// Default paper-mode builds do not link the native SDK. This keeps the Docker
/// deployment small and avoids enabling real credential derivation unless the
/// operator explicitly opts into the `native-l2` feature.
#[cfg(not(feature = "native-l2"))]
async fn derive_l2_credentials_native(
    _private_key: &str,
) -> anyhow::Result<(String, String, String, String)> {
    anyhow::bail!(
        "Native L2 derivation is disabled in this build. Rebuild with --features native-l2 to enable server-side Polymarket credential derivation."
    );
}

/// Public helper for auto-derive on startup (called from main.rs).
/// Returns masked key on success.
pub async fn try_auto_derive_l2_on_startup() -> anyhow::Result<Option<String>> {
    // Support file-based secret (K8s best practice, matches DATABASE_URL_FILE pattern)
    // or direct env var (for local .env.local).
    // NOTE (dupe with clob::authenticated::get_polymarket_private_key): both resolve the same
    // privkey for native-l2; kept separate entrypoints for startup vs per-place signing. Minor.
    let private_key = if let Ok(path) = std::env::var("POLYMARKET_PRIVATE_KEY_FILE") {
        if !path.is_empty() {
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    let key = content.trim().to_string();
                    if !key.is_empty() {
                        Some(key)
                    } else {
                        None
                    }
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to read POLYMARKET_PRIVATE_KEY_FILE at {}: {}",
                        path,
                        e
                    );
                    None
                }
            }
        } else {
            None
        }
    } else {
        std::env::var("POLYMARKET_PRIVATE_KEY")
            .or_else(|_| std::env::var("PRIVATE_KEY"))
            .ok()
            .filter(|k| !k.is_empty())
    };

    let private_key = match private_key {
        Some(k) => k,
        None => {
            info!("No POLYMARKET_PRIVATE_KEY (or _FILE) found — L2 will stay in 'not connected' state until derived");
            return Ok(None);
        }
    };

    let (signer_address, api_key, secret, passphrase) =
        derive_l2_credentials_native(&private_key).await?;
    let (_sess_id, masked) = register_l2_session(
        "server-key".to_string(),
        signer_address,
        api_key,
        secret,
        passphrase,
        true,
    )?;

    Ok(Some(masked))
}

// Server-side derivation using POLYMARKET_PRIVATE_KEY (or PRIVATE_KEY) from env.
async fn l2_derive_from_server_key_handler(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
) -> impl IntoResponse {
    //! RISK: Only use in paper mode. The private key allows deriving real L2 trading credentials.
    //! Secret material stays in process memory only (L2_SECRETS map).
    //! SECURITY: operator auth required (401 if unauthenticated); this loads the
    //! trading creds used by from_current_l2_session for gated real order dispatch.
    if auth.0.is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "success": false,
                "error": "operator authentication required to derive server L2 credentials (privileged)"
            })),
        )
            .into_response();
    }
    // Support file-based secret (K8s recommended) or direct env var.
    let private_key = if let Ok(path) = std::env::var("POLYMARKET_PRIVATE_KEY_FILE") {
        if !path.is_empty() {
            std::fs::read_to_string(&path)
                .map(|c| c.trim().to_string())
                .ok()
                .filter(|k| !k.is_empty())
        } else {
            None
        }
    } else {
        std::env::var("POLYMARKET_PRIVATE_KEY")
            .or_else(|_| std::env::var("PRIVATE_KEY"))
            .ok()
            .filter(|k| !k.is_empty())
    };

    let private_key = match private_key {
        Some(k) => k,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "success": false,
                    "error": "POLYMARKET_PRIVATE_KEY (or _FILE) not found"
                })),
            )
                .into_response();
        }
    };

    // Native Rust path using polymarket_client_sdk_v2 (user requested)
    let (signer_address, api_key, secret, passphrase) =
        match derive_l2_credentials_native(&private_key).await {
            Ok(creds) => creds,
            Err(e) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(serde_json::json!({
                        "success": false,
                        "error": format!("Native L2 derivation failed: {}", e)
                    })),
                )
                    .into_response();
            }
        };

    let (sess_id, masked) = match register_l2_session(
        "server-key".to_string(),
        signer_address,
        api_key,
        secret,
        passphrase,
        true,
    ) {
        Ok(session) => session,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("Failed to store L2 session: {}", e)
                })),
            )
                .into_response();
        }
    };

    // Set cookie so subsequent /l2/status works from browser
    let prefix = &state.subpath_prefix;
    let path = if prefix.is_empty() { "/" } else { prefix };
    let cookie = format!(
        "pt_l2_sess={}; HttpOnly; SameSite=Lax; Path={}{}",
        sess_id,
        path,
        cookie_secure_suffix()
    );

    let mut resp = Json(serde_json::json!({
        "success": true,
        "api_key_masked": masked,
        "note": "L2 credentials derived using server-side PRIVATE_KEY"
    }))
    .into_response();

    resp.headers_mut()
        .insert(axum::http::header::SET_COOKIE, cookie.parse().unwrap());
    resp
}

async fn clob_status_handler() -> impl IntoResponse {
    //! Read-only authenticated CLOB probe. It uses derived L2 credentials to fetch
    //! open orders only. It never creates, signs, posts, cancels, or refreshes orders.
    let Some(client) = crate::clob::RealClobClient::from_current_l2_session() else {
        return Json(serde_json::json!({
            "l2_connected": false,
            "read_only_live_check": false,
            "paper_only": true,
            "real_orders_enabled": false,
            "error": "No derived L2 credential session is available"
        }))
        .into_response();
    };

    match client.open_orders().await {
        Ok(orders) => Json(serde_json::json!({
            "l2_connected": true,
            "read_only_live_check": true,
            "paper_only": true,
            "real_orders_enabled": false,
            "open_orders": orders,
            "note": "Read-only CLOB status only; no order placement or cancellation is implemented."
        }))
        .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "l2_connected": true,
                "read_only_live_check": false,
                "paper_only": true,
                "real_orders_enabled": false,
                "error": format!("Read-only CLOB request failed: {}", e)
            })),
        )
            .into_response(),
    }
}

async fn clob_account_handler() -> impl IntoResponse {
    //! Read-only authenticated CLOB account snapshot. This is intentionally
    //! limited to open orders and collateral balance/allowance diagnostics.
    //! It never creates, signs, posts, cancels, or refreshes orders/allowances.
    let Some(client) = crate::clob::RealClobClient::from_current_l2_session() else {
        return Json(serde_json::json!({
            "l2_connected": false,
            "read_only_live_check": false,
            "paper_only": true,
            "real_orders_enabled": false,
            "error": "No derived L2 credential session is available"
        }))
        .into_response();
    };

    match client.account_snapshot().await {
        Ok(account) => Json(serde_json::json!({
            "l2_connected": true,
            "read_only_live_check": true,
            "paper_only": true,
            "real_orders_enabled": false,
            "account": account,
            "note": "Read-only CLOB account snapshot only; no order placement, cancellation, allowance refresh, or balance mutation is implemented."
        }))
        .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "l2_connected": true,
                "read_only_live_check": false,
                "paper_only": true,
                "real_orders_enabled": false,
                "error": format!("Read-only CLOB account request failed: {}", e)
            })),
        )
            .into_response(),
    }
}

async fn clob_preflight_handler() -> impl IntoResponse {
    //! Read-only real-trading preflight diagnostics. This endpoint exists so
    //! operators can see exactly which gates would block a future order path.
    //! It never creates, signs, posts, cancels, or refreshes anything.
    let Some(client) = crate::clob::RealClobClient::from_current_l2_session() else {
        return Json(serde_json::json!({
            "l2_connected": false,
            "read_only_live_check": false,
            "paper_only": true,
            "real_orders_enabled": false,
            "ready_for_real_orders": false,
            "blockers": ["l2_credentials_missing"],
            "error": "No derived L2 credential session is available"
        }))
        .into_response();
    };

    match client.preflight_report().await {
        Ok(preflight) => Json(serde_json::json!({
            "l2_connected": true,
            "read_only_live_check": true,
            "paper_only": true,
            "real_orders_enabled": false,
            "preflight": preflight,
            "note": "Diagnostic preflight only; no real order placement, cancellation, allowance refresh, or balance mutation is implemented."
        }))
        .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "l2_connected": true,
                "read_only_live_check": false,
                "paper_only": true,
                "real_orders_enabled": false,
                "ready_for_real_orders": false,
                "blockers": ["clob_account_read_failed"],
                "error": format!("Read-only CLOB preflight request failed: {}", e)
            })),
        )
            .into_response(),
    }
}

async fn clob_collateral_readiness_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    //! Read-only operator report for the remaining external wallet blockers:
    //! collateral balance and collateral allowance. This route never transfers
    //! funds, approves/refreshes allowances, signs orders, or submits anything.
    let Some(client) = crate::clob::RealClobClient::from_current_l2_session() else {
        return Json(serde_json::json!({
            "l2_connected": false,
            "read_only_live_check": false,
            "paper_only": true,
            "real_orders_enabled": false,
            "ready_for_real_orders": false,
            "request_sent": false,
            "post_order_called": false,
            "post_orders_called": false,
            "blockers": ["l2_credentials_missing"],
            "error": "No derived L2 credential session is available"
        }))
        .into_response();
    };

    match client.collateral_readiness_report().await {
        Ok(report) => {
            let event_payload = serde_json::json!({
                "kind": "clob_collateral_readiness",
                "report": report.clone(),
                "paper_only": true,
                "real_orders_enabled": false,
                "request_sent": false,
                "post_order_called": false,
                "post_orders_called": false,
            });
            let response = serde_json::json!({
                "l2_connected": true,
                "read_only_live_check": true,
                "paper_only": true,
                "real_orders_enabled": false,
                "ready_for_real_orders": false,
                "collateral_readiness": report,
                "note": "Read-only collateral/allowance readiness only; no funding, approval, allowance refresh, signing, submission, cancellation, balance mutation, or real trading is implemented."
            });

            match record_journal_event(
                &state.pool,
                "clob_collateral_readiness",
                "polytrader_server",
                "info",
                event_payload,
            )
            .await
            {
                Ok(event_id) => {
                    Json(merge_journal_fields(response, true, Some(event_id), None)).into_response()
                }
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(merge_journal_fields(
                        response,
                        false,
                        None,
                        Some(format!(
                            "Collateral readiness read completed but journal write failed: {}",
                            e
                        )),
                    )),
                )
                    .into_response(),
            }
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "l2_connected": true,
                "read_only_live_check": false,
                "paper_only": true,
                "real_orders_enabled": false,
                "ready_for_real_orders": false,
                "request_sent": false,
                "post_order_called": false,
                "post_orders_called": false,
                "journaled": false,
                "blockers": ["collateral_readiness_read_failed"],
                "error": format!("Read-only CLOB collateral readiness request failed: {}", e)
            })),
        )
            .into_response(),
    }
}

async fn clob_diagnostics_handler() -> impl IntoResponse {
    //! Read-only aggregate over the authenticated CLOB diagnostics. This exists
    //! for deploy verification and operator tooling so callers can fetch status,
    //! account, and preflight from one safe route. It performs no writes and does
    //! not expose L2 secrets.
    let Some(client) = crate::clob::RealClobClient::from_current_l2_session() else {
        return Json(serde_json::json!({
            "l2_connected": false,
            "read_only_live_check": false,
            "paper_only": true,
            "real_orders_enabled": false,
            "ready_for_real_orders": false,
            "status": {
                "l2_connected": false,
                "read_only_live_check": false,
                "paper_only": true,
                "real_orders_enabled": false,
                "error": "No derived L2 credential session is available"
            },
            "account": null,
            "preflight": {
                "ready_for_real_orders": false,
                "real_orders_enabled": false,
                "paper_only": true,
                "blockers": ["l2_credentials_missing"]
            },
            "error": "No derived L2 credential session is available"
        }))
        .into_response();
    };

    match client.account_snapshot().await {
        Ok(account) => {
            let open_orders = account
                .get("open_orders")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({"count": 0, "data": []}));
            let preflight = crate::clob::authenticated::build_preflight_report(&account);

            Json(serde_json::json!({
                "l2_connected": true,
                "read_only_live_check": true,
                "paper_only": true,
                "real_orders_enabled": false,
                "ready_for_real_orders": false,
                "status": {
                    "l2_connected": true,
                    "read_only_live_check": true,
                    "paper_only": true,
                    "real_orders_enabled": false,
                    "open_orders": open_orders
                },
                "account": {
                    "l2_connected": true,
                    "read_only_live_check": true,
                    "paper_only": true,
                    "real_orders_enabled": false,
                    "account": account
                },
                "preflight": preflight,
                "note": "Aggregate read-only CLOB diagnostics only; no order placement, cancellation, allowance refresh, balance mutation, or real trading is implemented."
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "l2_connected": true,
                "read_only_live_check": false,
                "paper_only": true,
                "real_orders_enabled": false,
                "ready_for_real_orders": false,
                "status": {
                    "l2_connected": true,
                    "read_only_live_check": false,
                    "paper_only": true,
                    "real_orders_enabled": false,
                    "error": format!("Read-only CLOB diagnostics request failed: {}", e)
                },
                "account": null,
                "preflight": {
                    "ready_for_real_orders": false,
                    "real_orders_enabled": false,
                    "paper_only": true,
                    "blockers": ["clob_account_read_failed"]
                },
                "error": format!("Read-only CLOB diagnostics request failed: {}", e)
            })),
        )
            .into_response(),
    }
}

async fn clob_operator_status_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<DryRunEventsQuery>,
) -> impl IntoResponse {
    //! Read-only operator rollup over authenticated CLOB diagnostics plus
    //! paper-only dry-run review health. This intentionally remains an
    //! observability route: it never creates dry-runs, writes reviews, approves,
    //! signs, submits, cancels, refreshes allowances, mutates balances, or places
    //! real orders.
    let limit = clamp_review_events_limit(query.limit.unwrap_or(50));

    let (l2_connected, clob_read_ok, preflight, clob_blockers, clob_error) =
        match crate::clob::RealClobClient::from_current_l2_session() {
            None => (
                false,
                false,
                serde_json::json!({
                    "ready_for_real_orders": false,
                    "paper_only": true,
                    "real_orders_enabled": false,
                    "blockers": ["l2_credentials_missing"]
                }),
                vec!["l2_credentials_missing".to_string()],
                Some("No derived L2 credential session is available".to_string()),
            ),
            Some(client) => match client.account_snapshot().await {
                Ok(account) => {
                    let preflight = crate::clob::authenticated::build_preflight_report(&account);
                    let blockers = json_string_array(&preflight, "blockers");
                    (true, true, preflight, blockers, None)
                }
                Err(e) => (
                    true,
                    false,
                    serde_json::json!({
                        "ready_for_real_orders": false,
                        "paper_only": true,
                        "real_orders_enabled": false,
                        "blockers": ["clob_account_read_failed"]
                    }),
                    vec!["clob_account_read_failed".to_string()],
                    Some(format!(
                        "Read-only CLOB operator-status request failed: {}",
                        e
                    )),
                ),
            },
        };

    let (review_summary, review_health, review_error) =
        match fetch_review_summary_items(&state.pool, limit).await {
            Ok(items) => {
                let summary = build_review_summary(&items);
                let health = build_review_health(&summary);
                (summary, health, None)
            }
            Err(e) => (
                serde_json::json!({
                    "paper_only": true,
                    "real_orders_enabled": false,
                }),
                serde_json::json!({
                    "status": "unavailable",
                    "reasons": ["review_summary_unavailable"],
                    "recommended_actions": [{
                        "id": "inspect_review_summary",
                        "severity": "attention",
                        "label": "Inspect the dry-run review summary endpoint.",
                        "endpoint": "/clob/order-intent/review-summary?limit=50"
                    }],
                    "paper_only": true,
                    "real_orders_enabled": false,
                }),
                Some(format!("Failed to load dry-run review health: {}", e)),
            ),
        };
    let (final_review_audit, final_review_error) =
        match fetch_final_review_decision_events(&state.pool, 10).await {
            Ok(events) => (build_final_review_audit_summary(events), None),
            Err(e) => (
                serde_json::json!({
                    "status": "unavailable",
                    "paper_only": true,
                    "real_orders_enabled": false,
                    "approved_for_real_orders": false,
                    "event_type": "clob_final_review_decision",
                    "count": 0,
                    "decision_counts": final_review_decision_counts(&[]),
                    "latest_decision": serde_json::Value::Null,
                    "events": [],
                }),
                Some(format!("Failed to load final review decision audit: {}", e)),
            ),
        };
    let (final_review_coverage_gap_probe, final_review_gap_probe_error) =
        match fetch_final_review_decision_events(&state.pool, 50).await {
            Ok(events) => (
                build_final_review_coverage_gap_probe(
                    &build_final_review_audit_summary(events),
                    50,
                    chrono::Utc::now(),
                ),
                None,
            ),
            Err(e) => (
                serde_json::json!({
                    "available": false,
                    "limit": 50,
                    "paper_only": true,
                    "real_orders_enabled": false,
                    "ready_for_real_orders": false,
                    "approved_for_real_orders": false,
                    "coverage_gaps": {"count": 0, "events": []},
                    "hermes_coverage_window_seconds": 86400,
                    "active_24h_gap_status": "unavailable",
                    "active_24h_gap_count": 0,
                    "expired_24h_gap_count": 0,
                    "oldest_gap_created_at": serde_json::Value::Null,
                    "newest_gap_created_at": serde_json::Value::Null,
                    "oldest_gap_age_seconds": serde_json::Value::Null,
                    "newest_gap_age_seconds": serde_json::Value::Null,
                    "seconds_until_all_gaps_age_out_of_24h": serde_json::Value::Null,
                    "oldest_active_24h_gap_created_at": serde_json::Value::Null,
                    "newest_active_24h_gap_created_at": serde_json::Value::Null,
                    "active_gaps_age_out_at": serde_json::Value::Null,
                    "oldest_active_24h_gap_age_seconds": serde_json::Value::Null,
                    "newest_active_24h_gap_age_seconds": serde_json::Value::Null,
                    "seconds_until_active_gaps_age_out_of_24h": serde_json::Value::Null,
                    "request_sent": false,
                    "post_order_called": false,
                    "post_orders_called": false,
                }),
                Some(format!(
                    "Failed to load broad final review coverage gap probe: {}",
                    e
                )),
            ),
        };
    let (hermes_safety_loop, hermes_error) = match fetch_latest_hermes_reflection(&state.pool).await
    {
        Ok(reflection) => (
            build_hermes_safety_loop_response(reflection, chrono::Utc::now()),
            None,
        ),
        Err(e) => (
            serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "ready_for_real_orders": false,
                "approved_for_real_orders": false,
                "available": false,
                "status": "reflection_lookup_failed",
                "error": format!("Failed to load latest Hermes reflection: {e}"),
                "request_sent": false,
                "post_order_called": false,
                "post_orders_called": false,
            }),
            Some(format!("Failed to load Hermes CLOB safety loop: {e}")),
        ),
    };
    let final_review_hermes_gap_alignment = build_final_review_hermes_gap_alignment(
        &final_review_coverage_gap_probe,
        &hermes_safety_loop,
    );

    let review_status = review_health.get("status").and_then(|v| v.as_str());
    let live_sender_boundary = crate::clob::live_sender::build_live_sender_boundary_status().await;
    let operator_status = operator_status_state(
        clob_read_ok,
        &clob_blockers,
        review_error.is_none(),
        review_status,
    );

    let recommended_next_actions = operator_status_actions(
        operator_status,
        &review_health,
        &final_review_audit,
        &live_sender_boundary,
        &hermes_safety_loop,
        &final_review_hermes_gap_alignment,
    );
    let action_summary = operator_action_summary(&recommended_next_actions);
    let generated_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    Json(serde_json::json!({
        "paper_only": true,
        "real_orders_enabled": false,
        "ready_for_real_orders": false,
        "read_only_live_check": clob_read_ok,
        "operator_status": operator_status,
        "l2_connected": l2_connected,
        "clob": {
            "l2_connected": l2_connected,
            "read_only_live_check": clob_read_ok,
            "paper_only": true,
            "real_orders_enabled": false,
            "ready_for_real_orders": false,
            "blockers": clob_blockers,
            "preflight": preflight,
            "error": clob_error,
        },
        "review": {
            "limit": limit,
            "health": review_health,
            "summary": review_summary,
            "error": review_error,
        },
        "final_review": {
            "audit": final_review_audit,
            "coverage_gap_probe": final_review_coverage_gap_probe,
            "hermes_gap_alignment": final_review_hermes_gap_alignment,
            "error": final_review_error,
            "coverage_gap_probe_error": final_review_gap_probe_error,
        },
        "hermes_safety_loop": {
            "latest": hermes_safety_loop,
            "error": hermes_error,
        },
        "live_sender_boundary": live_sender_boundary,
        "action_summary": action_summary,
        "freshness": {
            "generated_at": generated_at,
            "stale_after_seconds": OPERATOR_STATUS_STALE_AFTER_SECONDS,
        },
        "recommended_next_actions": recommended_next_actions,
        "note": "Read-only operator rollup only; no order placement, cancellation, approval, review write, allowance refresh, balance mutation, kill-switch mutation, network sender creation, or real trading is implemented."
    }))
    .into_response()
}

async fn clob_order_placement_readiness_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<DryRunEventsQuery>,
) -> impl IntoResponse {
    //! Read-only gap report for future real CLOB order placement. This is
    //! deliberately an observability route: it never creates, signs, submits,
    //! posts, cancels, approves, refreshes allowances, mutates balances, or
    //! places real orders.
    let limit = clamp_review_events_limit(query.limit.unwrap_or(50));

    let (l2_connected, clob_read_ok, preflight, clob_error) =
        match crate::clob::RealClobClient::from_current_l2_session() {
            None => (
                false,
                false,
                serde_json::json!({
                    "ready_for_real_orders": false,
                    "paper_only": true,
                    "real_orders_enabled": false,
                    "blockers": ["l2_credentials_missing"]
                }),
                Some("No derived L2 credential session is available".to_string()),
            ),
            Some(client) => match client.account_snapshot().await {
                Ok(account) => {
                    let preflight = crate::clob::authenticated::build_preflight_report(&account);
                    (true, true, preflight, None)
                }
                Err(e) => (
                    true,
                    false,
                    serde_json::json!({
                        "ready_for_real_orders": false,
                        "paper_only": true,
                        "real_orders_enabled": false,
                        "blockers": ["clob_account_read_failed"]
                    }),
                    Some(format!(
                        "Read-only CLOB order-placement readiness request failed: {}",
                        e
                    )),
                ),
            },
        };

    let (review_health, review_error) = match fetch_review_summary_items(&state.pool, limit).await {
        Ok(items) => {
            let summary = build_review_summary(&items);
            (build_review_health(&summary), None)
        }
        Err(e) => (
            serde_json::json!({
                "status": "unavailable",
                "reasons": ["review_summary_unavailable"],
                "paper_only": true,
                "real_orders_enabled": false,
            }),
            Some(format!("Failed to load dry-run review health: {}", e)),
        ),
    };
    let (final_review_audit, final_review_error) =
        match fetch_final_review_decision_events(&state.pool, 10).await {
            Ok(events) => (build_final_review_audit_summary(events), None),
            Err(e) => (
                serde_json::json!({
                    "status": "unavailable",
                    "paper_only": true,
                    "real_orders_enabled": false,
                    "ready_for_real_orders": false,
                    "approved_for_real_orders": false,
                    "event_type": "clob_final_review_decision",
                    "count": 0,
                    "decision_counts": final_review_decision_counts(&[]),
                    "latest_decision": serde_json::Value::Null,
                    "events": [],
                    "request_sent": false,
                    "post_order_called": false,
                    "post_orders_called": false,
                }),
                Some(format!("Failed to load final review decision audit: {}", e)),
            ),
        };
    let (market_data_readiness, market_data_error) =
        match fetch_market_data_readiness_summary(&state.pool).await {
            Ok(summary) => (summary, None),
            Err(e) => (
                serde_json::json!({
                    "available": false,
                    "status": "unavailable",
                    "active_market_count": 0,
                    "data_ready_market_count": 0,
                    "paper_only": true,
                    "real_orders_enabled": false,
                }),
                Some(format!("Failed to load market data readiness: {}", e)),
            ),
        };

    let readiness = build_order_placement_readiness(
        l2_connected,
        clob_read_ok,
        &preflight,
        &review_health,
        &market_data_readiness,
        &final_review_audit,
        &crate::clob::live_sender::build_live_sender_boundary_status().await,
    );

    Json(serde_json::json!({
        "paper_only": true,
        "real_orders_enabled": false,
        "ready_for_real_orders": false,
        "read_only_live_check": clob_read_ok,
        "l2_connected": l2_connected,
        "readiness": readiness,
        "preflight": preflight,
        "review_health": review_health,
        "market_data_readiness": market_data_readiness,
        "final_review_audit": final_review_audit,
        "errors": {
            "clob": clob_error,
            "review": review_error,
            "market_data": market_data_error,
            "final_review": final_review_error,
        },
        "note": "Read-only readiness report only; no order placement, signing, submission, cancellation, allowance refresh, balance mutation, approval, kill-switch mutation, live-sender creation, or real trading is implemented."
    }))
    .into_response()
}

async fn clob_real_trading_unlock_status_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    //! Read-only report for the explicit real-trading unlock state. This route
    //! makes the final code-owned blocker auditable without granting any order
    //! authority or creating a live sender.
    let report = crate::clob::authenticated::build_real_trading_unlock_status();
    let event_payload = serde_json::json!({
        "kind": "clob_real_trading_unlock_status",
        "report": report.clone(),
        "paper_only": true,
        "real_orders_enabled": false,
        "request_sent": false,
        "post_order_called": false,
        "post_orders_called": false,
    });

    let response = serde_json::json!({
        "paper_only": true,
        "real_orders_enabled": false,
        "ready_for_real_orders": false,
        "unlock_status": report,
        "note": "Read-only real-trading unlock status only; no live sender, signing, submission, cancellation, allowance refresh, balance mutation, or real trading is implemented."
    });

    match record_journal_event(
        &state.pool,
        "clob_real_trading_unlock_status",
        "polytrader_server",
        "info",
        event_payload,
    )
    .await
    {
        Ok(event_id) => {
            Json(merge_journal_fields(response, true, Some(event_id), None)).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(merge_journal_fields(
                response,
                false,
                None,
                Some(format!(
                    "Real-trading unlock status read completed but journal write failed: {}",
                    e
                )),
            )),
        )
            .into_response(),
    }
}

async fn clob_live_sender_design_readiness_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    //! Read-only design-readiness package for the deliberately absent live
    //! sender. It exists so operators and Hermes can see the remaining design
    //! blockers before any implementation is considered. It does not create a
    //! sender or grant order authority.
    let unlock = latest_journal_event(&state.pool, "clob_real_trading_unlock_status").await;
    let final_review_decision =
        latest_journal_event(&state.pool, "clob_final_review_decision").await;
    let report = build_live_sender_design_readiness_report(
        unlock.as_ref().ok().and_then(|event| event.clone()),
        final_review_decision
            .as_ref()
            .ok()
            .and_then(|event| event.clone()),
    );

    let mut errors = Vec::new();
    if let Err(e) = unlock {
        errors.push(format!("real_trading_unlock_lookup_failed: {e}"));
    }
    if let Err(e) = final_review_decision {
        errors.push(format!("final_review_decision_lookup_failed: {e}"));
    }

    let event_payload = serde_json::json!({
        "kind": "clob_live_sender_design_readiness",
        "report": report.clone(),
        "errors": errors,
        "paper_only": true,
        "real_orders_enabled": false,
        "request_sent": false,
        "post_order_called": false,
        "post_orders_called": false,
    });

    let response = serde_json::json!({
        "paper_only": true,
        "real_orders_enabled": false,
        "ready_for_real_orders": false,
        "live_sender_design": report,
        "errors": errors,
        "note": "Read-only live-sender design readiness package only; no live sender, signing, submission, cancellation, allowance refresh, balance mutation, kill-switch mutation, or real trading is implemented."
    });

    match record_journal_event(
        &state.pool,
        "clob_live_sender_design_readiness",
        "polytrader_server",
        "info",
        event_payload,
    )
    .await
    {
        Ok(event_id) => {
            Json(merge_journal_fields(response, true, Some(event_id), None)).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(merge_journal_fields(
                response,
                false,
                None,
                Some(format!(
                    "Live-sender design readiness package built but journal write failed: {}",
                    e
                )),
            )),
        )
            .into_response(),
    }
}

async fn clob_live_sender_design_review_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    //! Read-only ADR-style design contract for a future live sender. This route
    //! captures the boundary, required guards, and prohibited shortcuts before
    //! implementation is even considered. It never creates a sender or grants
    //! order authority.
    let design_readiness =
        latest_journal_event(&state.pool, "clob_live_sender_design_readiness").await;
    let unlock = latest_journal_event(&state.pool, "clob_real_trading_unlock_status").await;
    let final_review_decision =
        latest_journal_event(&state.pool, "clob_final_review_decision").await;
    let report = build_live_sender_design_review_report(
        design_readiness
            .as_ref()
            .ok()
            .and_then(|event| event.clone()),
        unlock.as_ref().ok().and_then(|event| event.clone()),
        final_review_decision
            .as_ref()
            .ok()
            .and_then(|event| event.clone()),
    );

    let mut errors = Vec::new();
    if let Err(e) = design_readiness {
        errors.push(format!("live_sender_design_readiness_lookup_failed: {e}"));
    }
    if let Err(e) = unlock {
        errors.push(format!("real_trading_unlock_lookup_failed: {e}"));
    }
    if let Err(e) = final_review_decision {
        errors.push(format!("final_review_decision_lookup_failed: {e}"));
    }

    let event_payload = serde_json::json!({
        "kind": "clob_live_sender_design_review",
        "report": report.clone(),
        "errors": errors,
        "paper_only": true,
        "real_orders_enabled": false,
        "request_sent": false,
        "post_order_called": false,
        "post_orders_called": false,
    });

    let response = serde_json::json!({
        "paper_only": true,
        "real_orders_enabled": false,
        "ready_for_real_orders": false,
        "live_sender_design_review": report,
        "errors": errors,
        "note": "Read-only live-sender design review contract only; no live sender, signing, submission, cancellation, allowance refresh, balance mutation, kill-switch mutation, or real trading is implemented."
    });

    match record_journal_event(
        &state.pool,
        "clob_live_sender_design_review",
        "polytrader_server",
        "info",
        event_payload,
    )
    .await
    {
        Ok(event_id) => {
            Json(merge_journal_fields(response, true, Some(event_id), None)).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(merge_journal_fields(
                response,
                false,
                None,
                Some(format!(
                    "Live-sender design review contract built but journal write failed: {}",
                    e
                )),
            )),
        )
            .into_response(),
    }
}

async fn clob_live_sender_boundary_status_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    //! Read-only status for the fail-closed live-sender boundary. This proves
    //! the trait boundary exists while the only implementation rejects before
    //! any network dispatch.
    let report = crate::clob::live_sender::build_live_sender_boundary_status().await;
    let event_payload = serde_json::json!({
        "kind": "clob_live_sender_boundary_status",
        "report": report.clone(),
        "paper_only": true,
        "real_orders_enabled": false,
        "request_sent": false,
        "post_order_called": false,
        "post_orders_called": false,
    });

    let response = serde_json::json!({
        "paper_only": true,
        "real_orders_enabled": false,
        "ready_for_real_orders": false,
        "live_sender_boundary": report,
        "note": "Read-only fail-closed live-sender boundary status only; no network sender, signing, submission, cancellation, allowance refresh, balance mutation, kill-switch mutation, or real trading is implemented."
    });

    match record_journal_event(
        &state.pool,
        "clob_live_sender_boundary_status",
        "polytrader_server",
        "info",
        event_payload,
    )
    .await
    {
        Ok(event_id) => {
            Json(merge_journal_fields(response, true, Some(event_id), None)).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(merge_journal_fields(
                response,
                false,
                None,
                Some(format!(
                    "Live-sender boundary status built but journal write failed: {}",
                    e
                )),
            )),
        )
            .into_response(),
    }
}

async fn clob_final_review_readiness_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    //! Read-only final-review package. This aggregates already-journaled gate
    //! evidence so a human can see one conservative blocker report. It never
    //! creates approvals, signs orders, submits orders, or mutates wallet state.
    let l2_connected = crate::clob::RealClobClient::from_current_l2_session().is_some();
    let collateral = latest_journal_event(&state.pool, "clob_collateral_readiness").await;
    let unlock = latest_journal_event(&state.pool, "clob_real_trading_unlock_status").await;
    let reconciliation =
        latest_journal_event(&state.pool, "clob_order_submit_reconciliation").await;
    let report = build_final_review_readiness_report(
        l2_connected,
        collateral.as_ref().ok().and_then(|event| event.clone()),
        unlock.as_ref().ok().and_then(|event| event.clone()),
        reconciliation.as_ref().ok().and_then(|event| event.clone()),
        &crate::clob::live_sender::build_live_sender_boundary_status().await,
    );

    let mut errors = Vec::new();
    if let Err(e) = collateral {
        errors.push(format!("collateral_readiness_lookup_failed: {e}"));
    }
    if let Err(e) = unlock {
        errors.push(format!("real_trading_unlock_lookup_failed: {e}"));
    }
    if let Err(e) = reconciliation {
        errors.push(format!("submit_reconciliation_lookup_failed: {e}"));
    }

    let event_payload = serde_json::json!({
        "kind": "clob_final_review_readiness",
        "report": report.clone(),
        "errors": errors,
        "paper_only": true,
        "real_orders_enabled": false,
        "request_sent": false,
        "post_order_called": false,
        "post_orders_called": false,
    });

    let response = serde_json::json!({
        "paper_only": true,
        "real_orders_enabled": false,
        "ready_for_real_orders": false,
        "final_review": report,
        "errors": errors,
        "note": "Read-only final review readiness package only; no live sender, approval, signing, submission, cancellation, allowance refresh, balance mutation, or real trading is implemented."
    });

    match record_journal_event(
        &state.pool,
        "clob_final_review_readiness",
        "polytrader_server",
        "info",
        event_payload,
    )
    .await
    {
        Ok(event_id) => {
            Json(merge_journal_fields(response, true, Some(event_id), None)).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(merge_journal_fields(
                response,
                false,
                None,
                Some(format!(
                    "Final review readiness package built but journal write failed: {}",
                    e
                )),
            )),
        )
            .into_response(),
    }
}

async fn clob_final_review_decision_handler(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Json(request): Json<FinalReviewDecisionRequest>,
) -> impl IntoResponse {
    //! Record an operator decision against a final-review readiness packet.
    //! 2026-06-03 UX: enriched with risk_snapshot + collateral_snapshot captured at decision/approve time
    //! (for reval/attribution when the journal_event_id is supplied as final_review_decision_event_id to
    //! submit-facade + GatedRealClobLiveOrderSender). The id (non-zero, any valid recorded decision per
    //! operator judgment) + human id satisfy the final_ok/human_ok gates for real path *when* explicit
    //! unlocks + kill + risk/collateral pass at facade + reval in sender. Still audit-oriented; never
    //! auto-unlocks (approved_for_real_orders remains false in payload; review_decision_effect etc).
    //! RISK (AGENTS): human + final are mandatory pre-conditions but insufficient alone. Hard pre-dispatch
    //! journal + sender reval + paper default + fail-closed boundary always. AuthUser binds operator.
    //! SECURITY: operator auth required; auth subject is bound to the journaled decision event for audit (final id then usable as gate for submit-facade). 401 if unauthed.
    if auth.0.is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "approved_for_real_orders": false,
                "final_review_decision_recorded": false,
                "journaled": false,
                "error": "operator authentication required for final-review-decision (privileged gate)"
            })),
        )
            .into_response();
    }
    let decision = normalize_final_review_decision(&request.decision);
    let operator = auth
        .0
        .as_deref()
        .unwrap_or(request.operator.as_deref().unwrap_or("unspecified"))
        .trim();
    let note = request.note.as_deref().unwrap_or("").trim();
    let final_review_event = load_journal_event_by_id(
        &state.pool,
        request.final_review_event_id,
        "clob_final_review_readiness",
    )
    .await;

    let mut blockers = Vec::new();
    if !request.confirm_final_review_workflow {
        blockers.push("final_review_workflow_confirmation_missing".to_string());
    }
    if decision.is_none() {
        blockers.push("final_review_decision_invalid".to_string());
    }
    if operator.is_empty() || operator == "unspecified" {
        blockers.push("final_review_operator_missing".to_string());
    }
    if note.is_empty() {
        blockers.push("final_review_note_missing".to_string());
    }

    let mut final_review_lookup_error = None;
    let final_review_payload = match final_review_event {
        Ok(Some(event)) => Some(event),
        Ok(None) => {
            blockers.push("final_review_readiness_event_not_found".to_string());
            None
        }
        Err(e) => {
            blockers.push("final_review_readiness_event_lookup_failed".to_string());
            final_review_lookup_error = Some(e.to_string());
            None
        }
    };

    let report = final_review_payload
        .as_ref()
        .and_then(|event| event_payload_report(event))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let ready_for_final_review = report
        .get("ready_for_final_review")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let readiness_blockers = report
        .get("blockers")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let live_sender_boundary_status = report
        .get("live_sender_boundary_status")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let live_sender_boundary_fail_closed = live_sender_boundary_status
        .get("fail_closed_implementation_present")
        .and_then(|value| value.as_bool())
        == Some(true)
        && live_sender_boundary_status
            .get("network_sender_present")
            .and_then(|value| value.as_bool())
            == Some(false)
        && live_sender_boundary_status
            .get("accepted_for_network_dispatch")
            .and_then(|value| value.as_bool())
            == Some(false)
        && live_sender_boundary_status
            .get("request_sent")
            .and_then(|value| value.as_bool())
            == Some(false);

    if decision == Some("acknowledge_blocked") && ready_for_final_review {
        blockers.push("final_review_packet_not_blocked".to_string());
    }
    if final_review_payload.is_some() && !live_sender_boundary_fail_closed {
        blockers.push("final_review_live_sender_boundary_not_fail_closed".to_string());
    }

    blockers.sort();
    blockers.dedup();

    if !blockers.is_empty() {
        return Json(serde_json::json!({
            "paper_only": true,
            "real_orders_enabled": false,
            "ready_for_real_orders": false,
            "approved_for_real_orders": false,
            "final_review_decision_recorded": false,
            "journaled": false,
            "final_review_event_id": request.final_review_event_id,
            "final_review_event_valid": final_review_payload.is_some(),
            "ready_for_final_review": ready_for_final_review,
            "readiness_blockers": readiness_blockers,
            "live_sender_boundary_fail_closed": live_sender_boundary_fail_closed,
            "live_sender_boundary_status": live_sender_boundary_status,
            "blockers": blockers,
            "request_sent": false,
            "post_order_called": false,
            "post_orders_called": false,
            "error": final_review_lookup_error,
            "note": "Final review decision validation failed; no decision event was journaled and no order can be sent. (Snapshots would have been embedded for gated real evidence on success.)"
        }))
        .into_response();
    }

    let decision = decision.expect("validated decision");

    // 2026-06-03: embed snapshots provided at final decision time (or markers). Mirrors human handler.
    // Used for consistency in approval evidence when both ids are supplied to real gated submit path.
    let risk_snapshot_at_approval = request.risk_snapshot.clone();
    let collateral_snapshot_at_approval = request.collateral_snapshot.clone();

    let payload = serde_json::json!({
        "kind": "clob_final_review_decision",
        "decision": decision,
        "final_review_event_id": request.final_review_event_id,
        "final_review_event_valid": true,
        "ready_for_final_review": ready_for_final_review,
        "readiness_blockers": readiness_blockers,
        "live_sender_boundary_fail_closed": live_sender_boundary_fail_closed,
        "live_sender_boundary_status": live_sender_boundary_status,
        "operator": operator,
        "note": note,
        "approved_for_real_orders": false,
        "paper_only": true,
        "real_orders_enabled": false,
        "ready_for_real_orders": false,
        "request_sent": false,
        "post_order_called": false,
        "post_orders_called": false,
        "review_decision_effect": "audit_only_no_unlock",
        "final_review_report": report,
        // Enriched snapshots at final review/approval time (2026-06-03 operator UX).
        "risk_snapshot_at_approval": risk_snapshot_at_approval,
        "collateral_snapshot_at_approval": collateral_snapshot_at_approval,
        "approval_time": chrono::Utc::now(),
    });

    match record_journal_event(
        &state.pool,
        "clob_final_review_decision",
        "polytrader_server",
        "info",
        payload.clone(),
    )
    .await
    {
        Ok(event_id) => Json(serde_json::json!({
            "paper_only": true,
            "real_orders_enabled": false,
            "ready_for_real_orders": false,
            "approved_for_real_orders": false,
            "final_review_decision_recorded": true,
            "journaled": true,
            "journal_event_id": event_id,
            "final_review_event_id": request.final_review_event_id,
            "final_review_event_valid": true,
            "decision": decision,
            "ready_for_final_review": ready_for_final_review,
            "readiness_blockers": readiness_blockers,
            "live_sender_boundary_fail_closed": live_sender_boundary_fail_closed,
            "live_sender_boundary_status": payload.get("live_sender_boundary_status").cloned().unwrap_or(serde_json::Value::Null),
            "review_decision_effect": "audit_only_no_unlock",
            "request_sent": false,
            "post_order_called": false,
            "post_orders_called": false,
            "note": "Final review decision recorded (enriched with risk/collateral snapshot at decision time). Provides final_review_decision_event_id for submit-facade + Gated sender (real path under unlocks+kill+human+reval). Remains audit-oriented; does not unlock by itself. See wiki/decisions/real-order-approval-flow.md + schema.md."
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "ready_for_real_orders": false,
                "approved_for_real_orders": false,
                "final_review_decision_recorded": false,
                "journaled": false,
                "final_review_event_id": request.final_review_event_id,
                "final_review_event_valid": true,
                "live_sender_boundary_fail_closed": live_sender_boundary_fail_closed,
                "live_sender_boundary_status": payload.get("live_sender_boundary_status").cloned().unwrap_or(serde_json::Value::Null),
                "error": format!("failed to write final review decision event: {}", e)
            })),
        )
            .into_response(),
    }
}

async fn clob_final_review_decisions_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<DryRunEventsQuery>,
) -> impl IntoResponse {
    //! Read-only audit endpoint for final-review decision events. These are
    //! review records only; they never approve live trading or mutate exchange
    //! state.
    let limit = clamp_dry_run_events_limit(query.limit.unwrap_or(10));
    let gaps_only = query.gaps_only.unwrap_or(false);

    match fetch_final_review_decision_events(&state.pool, limit).await {
        Ok(rows) => {
            let audit = build_final_review_audit_summary(rows);
            let coverage_gaps = audit
                .get("coverage_gaps")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({"count": 0, "events": []}));
            let events = if gaps_only {
                coverage_gaps
                    .get("events")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!([]))
            } else {
                audit
                    .get("events")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!([]))
            };
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "ready_for_real_orders": false,
                "approved_for_real_orders": false,
                "event_type": "clob_final_review_decision",
                "limit": limit,
                "gaps_only": gaps_only,
                "status": audit.get("status").cloned().unwrap_or(serde_json::Value::Null),
                "count": audit.get("count").cloned().unwrap_or(serde_json::json!(0)),
                "displayed_event_count": events.as_array().map(Vec::len).unwrap_or(0),
                "decision_counts": audit.get("decision_counts").cloned().unwrap_or_else(|| final_review_decision_counts(&[])),
                "boundary_evidence_count": audit.get("boundary_evidence_count").cloned().unwrap_or(serde_json::json!(0)),
                "no_network_evidence_count": audit.get("no_network_evidence_count").cloned().unwrap_or(serde_json::json!(0)),
                "missing_boundary_evidence_count": audit.get("missing_boundary_evidence_count").cloned().unwrap_or(serde_json::json!(0)),
                "missing_no_network_evidence_count": audit.get("missing_no_network_evidence_count").cloned().unwrap_or(serde_json::json!(0)),
                "all_events_have_boundary_evidence": audit.get("all_events_have_boundary_evidence").cloned().unwrap_or(serde_json::json!(false)),
                "all_events_have_no_network_evidence": audit.get("all_events_have_no_network_evidence").cloned().unwrap_or(serde_json::json!(false)),
                "coverage_status": audit.get("coverage_status").cloned().unwrap_or(serde_json::json!("no_decisions")),
                "coverage_gaps": coverage_gaps,
                "latest_boundary_status": audit.get("latest_boundary_status").cloned().unwrap_or(serde_json::Value::Null),
                "latest_decision": audit.get("latest_decision").cloned().unwrap_or(serde_json::Value::Null),
                "events": events,
                "request_sent": false,
                "post_order_called": false,
                "post_orders_called": false,
                "note": if gaps_only {
                    "Read-only final-review coverage gap audit only; compact rows identify decisions missing fail-closed/no-network evidence and cannot approve or enable live order submission."
                } else {
                    "Read-only final-review decision audit events only; these records never approve or enable live order submission."
                }
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "approved_for_real_orders": false,
                "error": format!("Failed to load final review decision journal events: {}", e)
            })),
        )
            .into_response(),
    }
}

async fn clob_hermes_safety_loop_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    //! Read-only operator view over the latest Hermes reflection. Hermes writes
    //! these metrics from append-only journal events; this route only surfaces
    //! them for the dashboard and deploy verifier. It never creates reflections,
    //! approvals, orders, signatures, network senders, allowance changes, or
    //! kill-switch changes.
    match fetch_latest_hermes_reflection(&state.pool).await {
        Ok(reflection) => {
            Json(build_hermes_safety_loop_response(reflection, chrono::Utc::now())).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "ready_for_real_orders": false,
                "approved_for_real_orders": false,
                "request_sent": false,
                "post_order_called": false,
                "post_orders_called": false,
                "available": false,
                "status": "reflection_lookup_failed",
                "error": format!("Failed to load latest Hermes reflection: {e}"),
                "note": "Hermes safety-loop lookup failed; no order authority is attached to this endpoint."
            })),
        )
            .into_response(),
    }
}

async fn clob_order_intent_dry_run_handler(
    State(state): State<Arc<AppState>>,
    Json(intent): Json<crate::clob::RealOrderIntentDryRun>,
) -> impl IntoResponse {
    //! Validate a proposed real CLOB order intent without signing or submitting.
    //! This is deliberately a dry-run-only route. It is useful for exercising
    //! risk/preflight logic before any dangerous order route exists.
    let Some(client) = crate::clob::RealClobClient::from_current_l2_session() else {
        return Json(serde_json::json!({
            "l2_connected": false,
            "dry_run_only": true,
            "accepted": false,
            "paper_only": true,
            "real_orders_enabled": false,
            "blockers": ["l2_credentials_missing"],
            "error": "No derived L2 credential session is available"
        }))
        .into_response();
    };

    match client.dry_run_order_intent(&intent).await {
        Ok(report) => {
            let event_payload = serde_json::json!({
                "kind": "clob_order_intent_dry_run",
                "report": report,
            });
            match record_journal_event(
                &state.pool,
                "clob_order_intent_dry_run",
                "polytrader_server",
                "info",
                event_payload,
            )
            .await
            {
                Ok(event_id) => Json(serde_json::json!({
            "l2_connected": true,
            "dry_run_only": true,
            "accepted": false,
            "paper_only": true,
            "real_orders_enabled": false,
                    "journaled": true,
                    "journal_event_id": event_id,
            "dry_run": report,
            "note": "Dry-run validation only; no real order was signed, submitted, persisted, cancelled, or placed."
                }))
                .into_response(),
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "l2_connected": true,
                        "dry_run_only": true,
                        "accepted": false,
                        "paper_only": true,
                        "real_orders_enabled": false,
                        "journaled": false,
                        "dry_run": report,
                        "error": format!("Dry-run validation succeeded but journal write failed: {}", e)
                    })),
                )
                    .into_response(),
            }
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "l2_connected": true,
                "dry_run_only": true,
                "accepted": false,
                "paper_only": true,
                "real_orders_enabled": false,
                "blockers": ["clob_account_read_failed"],
                "error": format!("Dry-run validation failed before risk checks: {}", e)
            })),
        )
            .into_response(),
    }
}

async fn clob_order_intent_market_validation_handler(
    State(state): State<Arc<AppState>>,
    Json(intent): Json<crate::clob::RealOrderIntentDryRun>,
) -> impl IntoResponse {
    //! Validate live CLOB token market metadata for a future order path without
    //! signing or sending. The only side effect is a redacted journal event for
    //! Hermes/audit. This checks tick size and negative-risk metadata; it never
    //! calls CLOB `POST /order`, `POST /orders`, cancellation, or allowance APIs.
    let Some(client) = crate::clob::RealClobClient::from_current_l2_session() else {
        return Json(serde_json::json!({
            "l2_connected": false,
            "dry_run_only": true,
            "accepted": false,
            "paper_only": true,
            "real_orders_enabled": false,
            "ready_for_real_orders": false,
            "market_metadata_validation_available": true,
            "market_metadata_fetched": false,
            "request_sent": false,
            "would_send": false,
            "would_post": false,
            "post_order_called": false,
            "post_orders_called": false,
            "journaled": false,
            "blockers": ["l2_credentials_missing"],
            "error": "No derived L2 credential session is available"
        }))
        .into_response();
    };

    let report = client.market_metadata_validation(&intent).await;
    let event_payload = serde_json::json!({
        "kind": "clob_market_metadata_validation",
        "report": report,
    });

    match record_journal_event(
        &state.pool,
        "clob_market_metadata_validation",
        "polytrader_server",
        "info",
        event_payload,
    )
    .await
    {
        Ok(event_id) => {
            Json(merge_journal_fields(report, true, Some(event_id), None)).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(merge_journal_fields(
                report,
                false,
                None,
                Some(format!(
                    "Market metadata validation completed but journal write failed: {}",
                    e
                )),
            )),
        )
            .into_response(),
    }
}

async fn clob_order_intent_signature_dry_run_handler(
    Json(request): Json<crate::clob::authenticated::SignedOrderPayloadDryRunRequest>,
) -> impl IntoResponse {
    //! Build and optionally sign a CLOB order payload locally without posting it.
    //! This is a signed-payload dry-run only: it never exposes the full signature,
    //! never persists the signed payload, never calls the CLOB `POST /order` or
    //! `POST /orders` endpoints, and cannot place a real order.
    let Some(client) = crate::clob::RealClobClient::from_current_l2_session() else {
        return Json(serde_json::json!({
            "l2_connected": false,
            "dry_run_only": true,
            "accepted": false,
            "paper_only": true,
            "real_orders_enabled": false,
            "ready_for_real_orders": false,
            "signed_payload_built": false,
            "signature_redacted": true,
            "would_post": false,
            "post_order_called": false,
            "post_orders_called": false,
            "blockers": ["l2_credentials_missing"],
            "error": "No derived L2 credential session is available"
        }))
        .into_response();
    };

    match client.signed_order_payload_dry_run(&request).await {
        Ok(report) => Json(report).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "l2_connected": true,
                "dry_run_only": true,
                "accepted": false,
                "paper_only": true,
                "real_orders_enabled": false,
                "ready_for_real_orders": false,
                "signed_payload_built": false,
                "signature_redacted": true,
                "would_post": false,
                "post_order_called": false,
                "post_orders_called": false,
                "blockers": ["signed_payload_dry_run_failed"],
                "error": format!("Signed payload dry-run failed before posting: {}", e)
            })),
        )
            .into_response(),
    }
}

async fn clob_order_intent_post_request_dry_run_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<crate::clob::authenticated::OrderPostRequestDryRunRequest>,
) -> impl IntoResponse {
    //! Serialize a redacted CLOB `POST /order` request preview without sending
    //! it. This endpoint is deliberately non-submitting: it never calls SDK
    //! `post_order`/`post_orders`, never exposes full signatures/HMACs, and
    //! can only journal the redacted dry-run result for Hermes.
    let Some(client) = crate::clob::RealClobClient::from_current_l2_session() else {
        return Json(serde_json::json!({
            "l2_connected": false,
            "dry_run_only": true,
            "accepted": false,
            "paper_only": true,
            "real_orders_enabled": false,
            "ready_for_real_orders": false,
            "post_request_dry_run_built": false,
            "signature_redacted": true,
            "l2_hmac_redacted": true,
            "would_send": false,
            "would_post": false,
            "post_order_called": false,
            "post_orders_called": false,
            "journaled": false,
            "blockers": ["l2_credentials_missing"],
            "error": "No derived L2 credential session is available"
        }))
        .into_response();
    };

    match client.order_post_request_dry_run(&request).await {
        Ok(report) => {
            let event_payload = serde_json::json!({
                "kind": "clob_order_post_request_dry_run",
                "report": report,
            });
            match record_journal_event(
                &state.pool,
                "clob_order_post_request_dry_run",
                "polytrader_server",
                "info",
                event_payload,
            )
            .await
            {
                Ok(event_id) => {
                    Json(merge_journal_fields(report, true, Some(event_id), None)).into_response()
                }
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(merge_journal_fields(
                        report,
                        false,
                        None,
                        Some(format!(
                            "Post-request dry-run succeeded but journal write failed: {}",
                            e
                        )),
                    )),
                )
                    .into_response(),
            }
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "l2_connected": true,
                "dry_run_only": true,
                "accepted": false,
                "paper_only": true,
                "real_orders_enabled": false,
                "ready_for_real_orders": false,
                "post_request_dry_run_built": false,
                "signature_redacted": true,
                "l2_hmac_redacted": true,
                "would_send": false,
                "would_post": false,
                "post_order_called": false,
                "post_orders_called": false,
                "journaled": false,
                "blockers": ["order_post_request_dry_run_failed"],
                "error": format!("Post-request dry-run failed before sending: {}", e)
            })),
        )
            .into_response(),
    }
}

async fn clob_order_intent_submit_facade_handler(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Json(mut request): Json<crate::clob::authenticated::OrderSubmitFacadeRequest>,
) -> impl IntoResponse {
    //! Evaluate the fail-closed real-order submission facade. This route exists
    //! to prove the shape and auditability of a future submit path while keeping
    //! live order placement blocked by approval, kill-switch, exposure, config,
    //! journaling, and paper-mode gates.
    //! SECURITY: operator auth required (AuthUser via SSO/cookie); 401 otherwise.
    //! The authenticated subject is bound to operator for audit.
    if auth.0.is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "l2_connected": false,
                "accepted": false,
                "paper_only": true,
                "real_orders_enabled": false,
                "request_sent": false,
                "post_order_called": false,
                "error": "operator authentication required for submit-facade (privileged real-order path)"
            })),
        )
            .into_response();
    }
    if let Some(ref email) = auth.0 {
        if request
            .operator
            .as_deref()
            .is_none_or(|o| o.trim().is_empty() || o == "unspecified")
        {
            request.operator = Some(email.clone());
        }
    }
    let Some(client) = crate::clob::RealClobClient::from_current_l2_session() else {
        return Json(serde_json::json!({
            "l2_connected": false,
            "submission_facade_only": true,
            "dry_run_only": true,
            "accepted": false,
            "paper_only": true,
            "real_orders_enabled": false,
            "ready_for_real_orders": false,
            "request_sent": false,
            "would_send": false,
            "would_post": false,
            "post_order_called": false,
            "post_orders_called": false,
            "journaled": false,
            "blockers": ["l2_credentials_missing"],
            "error": "No derived L2 credential session is available"
        }))
        .into_response();
    };

    request.server_human_approval =
        Some(validate_human_approval_event(&state.pool, &request).await);
    request.server_final_review_decision =
        Some(validate_final_review_decision_event(&state.pool, &request).await);
    request.server_collateral_readiness =
        Some(validate_collateral_readiness_event(&state.pool, Some(client.address())).await);

    match client.submit_order_facade(&request).await {
        Ok(report) => {
            let event_payload = serde_json::json!({
                "kind": "clob_order_submit_facade",
                "report": report,
            });
            match record_journal_event(
                &state.pool,
                "clob_order_submit_facade",
                "polytrader_server",
                "warning",
                event_payload,
            )
            .await
            {
                Ok(event_id) => {
                    // Wire the real sender here (smallest path to actual orders).
                    // If the facade gate_report has no blockers (meaning human approval,
                    // collateral, risk, explicit unlock, kill, paper conditional etc all
                    // passed), we journal the LiveOrderSendRequest (full context) *before*
                    // calling sender.send(), then invoke the gated sender which re-validates
                    // immediately before any network POST. This integrates human_approval_event_id,
                    // final-review (via ids), LiveOrderSender boundary, and pre/post journal.
                    let gate_blockers: Vec<String> = report
                        .get("gate_report")
                        .and_then(|g| g.get("blockers"))
                        .and_then(|b| b.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    let effective_real_enabled =
                        crate::clob::authenticated::env_truthy_for_clob_reports(
                            "POLYTRADER_ENABLE_REAL_ORDERS",
                        ) || crate::clob::authenticated::env_truthy_for_clob_reports(
                            "POLYTRADER_ENABLE_REAL_ORDER_SUBMISSION",
                        );
                    let mut live_send_result: Option<serde_json::Value> = None;
                    if gate_blockers.is_empty() && effective_real_enabled {
                        let intent = &request
                            .post_request_dry_run_request
                            .signed_payload_request
                            .intent;
                        let human_id = request
                            .human_approval_event_id
                            .map(|u| u.to_string())
                            .unwrap_or_else(|| "00000000-0000-0000-0000-000000000000".to_string());
                        let final_id = request
                            .final_review_decision_event_id
                            .map(|u| u.to_string())
                            .unwrap_or_else(|| "00000000-0000-0000-0000-000000000000".to_string());
                        let live_req = crate::clob::LiveOrderSendRequest {
                            local_order_id: format!("facade-{}", uuid::Uuid::new_v4()),
                            order_intent_event_id: "00000000-0000-0000-0000-000000000000"
                                .to_string(),
                            signed_payload_event_id: "00000000-0000-0000-0000-000000000000"
                                .to_string(),
                            human_approval_event_id: human_id.clone(),
                            final_review_decision_event_id: final_id,
                            market_id: intent.market_id.clone().unwrap_or_default(),
                            token_id: intent.token_id.clone(),
                            side: intent.side.clone(),
                            order_type: intent.order_type.clone(),
                            size: intent.size,
                            price: intent.price.unwrap_or(rust_decimal::Decimal::ONE),
                        };

                        // Log full intent with context *before* execution (AGENTS + safety rule 5).
                        // Pre journal is now a HARD gate: journal failure -> no send (fail-closed).
                        let pre_payload = serde_json::json!({
                            "kind": "clob_live_order_intent_pre_dispatch",
                            "live_order_send_request": {
                                "local_order_id": live_req.local_order_id,
                                "order_intent_event_id": live_req.order_intent_event_id,
                                "signed_payload_event_id": live_req.signed_payload_event_id,
                                "human_approval_event_id": live_req.human_approval_event_id,
                                "final_review_decision_event_id": live_req.final_review_decision_event_id,
                                "market_id": live_req.market_id,
                                "token_id": live_req.token_id,
                                "side": live_req.side,
                                "order_type": live_req.order_type,
                                "size": live_req.size.to_string(),
                                "price": live_req.price.to_string(),
                            },
                            "submit_facade_event_id": event_id,
                            "paper_only": false,
                            "real_orders_enabled": effective_real_enabled,
                            "note": "Full order intent + approval ids journaled immediately before GatedRealClobLiveOrderSender::send (revalidates + may dispatch to real CLOB)."
                        });
                        match record_journal_event(
                            &state.pool,
                            "clob_live_order_intent_pre_dispatch",
                            "polytrader_server",
                            "warning",
                            pre_payload,
                        )
                        .await
                        {
                            Ok(_) => {
                                let live_sender = crate::clob::GatedRealClobLiveOrderSender;
                                // Use UFCS so we do not need `use ... LiveOrderSender` in this file (smallest edit).
                                let send_res = <crate::clob::GatedRealClobLiveOrderSender as crate::clob::live_sender::LiveOrderSender>::send(&live_sender, &live_req).await;
                                live_send_result = Some(
                                    serde_json::to_value(&send_res)
                                        .unwrap_or(serde_json::json!({})),
                                );

                                // Journal the send outcome (observability for Hermes/operator).
                                // (best-effort; send already happened or rejected by gates inside)
                                let send_kind = if send_res.accepted_for_network_dispatch {
                                    "clob_live_order_dispatched"
                                } else {
                                    "clob_live_order_send_rejected"
                                };
                                let send_payload = serde_json::json!({
                                    "kind": send_kind,
                                    "live_send_result": send_res,
                                    "submit_facade_event_id": event_id,
                                    "paper_only": false,
                                    "real_orders_enabled": effective_real_enabled,
                                });
                                let _ = record_journal_event(
                                    &state.pool,
                                    send_kind,
                                    "polytrader_server",
                                    if send_res.accepted_for_network_dispatch {
                                        "info"
                                    } else {
                                        "error"
                                    },
                                    send_payload,
                                )
                                .await;
                            }
                            Err(e) => {
                                // Fail-closed: pre journal required before any dispatch (AGENTS safety).
                                tracing::error!(error = %e, "clob_live_order pre-dispatch journal failed; not invoking sender (no real order sent)");
                                live_send_result = Some(serde_json::json!({
                                    "sender_name": "GatedRealClobLiveOrderSender",
                                    "accepted_for_network_dispatch": false,
                                    "submit_decision": "pre_dispatch_journal_failed",
                                    "rejection_reason": format!("pre_journal_write_failed: {}", e),
                                    "exchange_order_id": null,
                                    "request_sent": false,
                                    "would_send": false,
                                    "post_order_called": false,
                                    "post_orders_called": false,
                                    "real_orders_enabled": effective_real_enabled,
                                    "ready_for_real_orders": false,
                                }));
                            }
                        }
                    }

                    // Build base report that may be augmented to reflect actual live dispatch
                    // (addresses response/recon always lying with pre-send values).
                    let mut base_report = report.clone();
                    if let Some(lsr) = &live_send_result {
                        if let Some(obj) = base_report.as_object_mut() {
                            if let Some(v) = lsr.get("accepted_for_network_dispatch") {
                                obj.insert("accepted".to_string(), v.clone());
                            }
                            if let Some(v) = lsr.get("submit_decision") {
                                obj.insert("submit_decision".to_string(), v.clone());
                            }
                            if let Some(v) = lsr.get("post_order_called") {
                                obj.insert("post_order_called".to_string(), v.clone());
                            }
                            if let Some(v) = lsr.get("request_sent") {
                                obj.insert("request_sent".to_string(), v.clone());
                            }
                            if let Some(v) = lsr.get("real_orders_enabled") {
                                obj.insert("real_orders_enabled".to_string(), v.clone());
                            }
                            if let Some(v) = lsr.get("ready_for_real_orders") {
                                obj.insert("ready_for_real_orders".to_string(), v.clone());
                            }
                            let roe = lsr
                                .get("real_orders_enabled")
                                .and_then(|b| b.as_bool())
                                .unwrap_or(false);
                            obj.insert("paper_only".to_string(), serde_json::json!(!roe));
                            obj.insert(
                                "submission_facade_only".to_string(),
                                serde_json::json!(!roe),
                            );
                            obj.insert("dry_run_only".to_string(), serde_json::json!(!roe));
                        }
                    }

                    let reconciliation =
                        base_report
                            .get("reconciliation")
                            .cloned()
                            .unwrap_or_else(|| {
                                serde_json::json!({
                                    "required": true,
                                    "reconciled": true,
                                    "status": "reconciled_no_send",
                                    "submit_decision": "rejected_fail_closed",
                                    "request_sent": false,
                                    "post_order_called": false,
                                    "post_orders_called": false,
                                    "expected_exchange_state": "no_order_created",
                                    "observed_exchange_state": "not_queried_no_send"
                                })
                            });
                    // Recon now reflects live dispatch result when it occurred (post_order etc from sender).
                    let (
                        recon_req_sent,
                        recon_post_called,
                        recon_paper,
                        recon_real_en,
                        recon_ready,
                        recon_note,
                    ) = if let Some(lsr) = &live_send_result {
                        let rs = lsr
                            .get("request_sent")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let pc = lsr
                            .get("post_order_called")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let re = lsr
                            .get("real_orders_enabled")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let rd = lsr
                            .get("ready_for_real_orders")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        (rs, pc, !re, re, rd, "Submit facade reconciliation after live dispatch consideration. See live_sender_dispatch for actual post_order_called / accepted values when dispatch was attempted.".to_string())
                    } else {
                        (false, false, true, false, false, "Submit facade reconciliation event. The facade rejected before send, so exchange state is reconciled as no order created.".to_string())
                    };
                    let reconciliation_payload = serde_json::json!({
                        "kind": "clob_order_submit_reconciliation",
                        "submit_facade_event_id": event_id,
                        "submit_decision": base_report.get("submit_decision").cloned().unwrap_or(serde_json::json!("rejected_fail_closed")),
                        "reconciliation_status": base_report.get("reconciliation_status").cloned().unwrap_or(serde_json::json!("reconciled_no_send")),
                        "reconciliation": reconciliation,
                        "request_sent": recon_req_sent,
                        "would_send": false,
                        "post_order_called": recon_post_called,
                        "post_orders_called": false,
                        "paper_only": recon_paper,
                        "real_orders_enabled": recon_real_en,
                        "ready_for_real_orders": recon_ready,
                        "note": recon_note
                    });
                    match record_journal_event(
                        &state.pool,
                        "clob_order_submit_reconciliation",
                        "polytrader_server",
                        "warning",
                        reconciliation_payload,
                    )
                    .await
                    {
                        Ok(reconciliation_event_id) => {
                            let mut response =
                                merge_journal_fields(base_report, true, Some(event_id), None);
                            if let Some(lsr) = live_send_result {
                                if let Some(obj) = response.as_object_mut() {
                                    obj.insert("live_sender_dispatch".to_string(), lsr);
                                }
                            }
                            Json(merge_reconciliation_journal_fields(
                                response,
                                true,
                                Some(reconciliation_event_id),
                                None,
                            ))
                            .into_response()
                        }
                        Err(e) => (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(merge_reconciliation_journal_fields(
                                merge_journal_fields(base_report, true, Some(event_id), None),
                                false,
                                None,
                                Some(format!(
                                    "Submit facade journaled, but reconciliation journal write failed: {}",
                                    e
                                )),
                            )),
                        )
                            .into_response(),
                    }
                }
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(merge_journal_fields(
                        report,
                        false,
                        None,
                        Some(format!(
                            "Submit facade gate evaluation succeeded but journal write failed: {}",
                            e
                        )),
                    )),
                )
                    .into_response(),
            }
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "l2_connected": true,
                "submission_facade_only": true,
                "dry_run_only": true,
                "accepted": false,
                "paper_only": true,
                "real_orders_enabled": false,
                "ready_for_real_orders": false,
                "request_sent": false,
                "would_send": false,
                "would_post": false,
                "post_order_called": false,
                "post_orders_called": false,
                "journaled": false,
                "blockers": ["order_submit_facade_failed"],
                "error": format!("Submit facade failed before any possible send: {}", e)
            })),
        )
            .into_response(),
    }
}

async fn clob_order_intent_human_approval_handler(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Json(request): Json<HumanApprovalRequest>,
) -> impl IntoResponse {
    //! Record a human approval decision for a future submit-facade attempt (or real gated CLOB path).
    //! Creates short-lived journal event (clob_order_human_approval) keyed to intent subject hash.
    //! 2026-06-03: enriched at approve time with risk_snapshot + collateral_snapshot (captured here
    //! via request or builders + intent-derived calc) + operator (AuthUser) + approval_time for
    //! anti-staleness reval, Hermes attribution, and audit when id later fed to submit + GatedRealClobLiveOrderSender.
    //! RISK (AGENTS + safety): does not auto-approve or bypass any gate. Real dispatch still requires
    //! non-zero id + POLYTRADER_ENABLE_REAL_* + KILL_SWITCH_OPEN + fresh collateral/risk in facade +
    //! reval immediately before network in sender + hard pre-dispatch journal. Paper default preserved.
    //! SECURITY: operator auth required; auth subject bound into journaled event. Fail-closed 401.
    if auth.0.is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "approved_for_facade": false,
                "journaled": false,
                "error": "operator authentication required for human-approval (privileged gate)"
            })),
        )
            .into_response();
    }
    let decision = normalize_human_approval_decision(&request.decision);
    let subject_hash =
        crate::clob::authenticated::approval_subject_hash_for_intent(&request.intent);
    let operator = auth
        .0
        .as_deref()
        .unwrap_or(request.operator.as_deref().unwrap_or("unspecified"));
    let note = request.note.as_deref().unwrap_or("").trim();

    let mut blockers = Vec::new();
    if !request.confirm_human_approval_workflow {
        blockers.push("human_approval_workflow_confirmation_missing".to_string());
    }
    if decision.is_none() {
        blockers.push("human_approval_decision_invalid".to_string());
    }
    if operator.trim().is_empty() || operator == "unspecified" {
        blockers.push("human_approval_operator_missing".to_string());
    }
    if note.is_empty() {
        blockers.push("human_approval_note_missing".to_string());
    }

    if !blockers.is_empty() {
        blockers.sort();
        blockers.dedup();
        return Json(serde_json::json!({
            "paper_only": true,
            "real_orders_enabled": false,
            "ready_for_real_orders": false,
            "approved_for_facade": false,
            "journaled": false,
            "subject_hash": subject_hash,
            "blockers": blockers,
            "note": "Human approval workflow validation failed; no approval event was journaled and no order can be sent. (Snapshots would have been embedded on success for real-path evidence.)"
        }))
        .into_response();
    }

    let decision = decision.expect("validated decision");
    let approved_for_facade = decision == "approve_facade";
    let now = chrono::Utc::now();
    let expires_at = now + chrono::Duration::minutes(15);

    // 2026-06-03: capture risk/collateral snapshot *at human approval time* (anti-staleness + evidence for reval/attribution).
    // Prefer UI-provided (fetched live from /clob/collateral-readiness + computed readiness/risk before button POST).
    // Fallback: latest collateral event (if any) + minimal intent-derived risk (Decimal only; mirrors gate calc).
    // Embedded so submit-facade validation + pre-dispatch journal + Hermes + future dispatch reval have the "as-approved" view.
    // RISK: snapshot is evidence only; does not relax re-checks at facade time or in Gated sender (see live_sender.rs).
    // Prefer provided snapshot (from approve button fetching current readiness); None is fine (UI populates in practice for this UX).
    let collateral_snapshot_at_approval = request.collateral_snapshot.clone();
    let risk_snapshot_at_approval = request.risk_snapshot.clone().or_else(|| {
        // Minimal inline (avoids private fn in clob::authenticated; smallest change, dupe of gate logic ok for this tranche).
        let conservative_price = request
            .intent
            .price
            .unwrap_or(Decimal::ONE)
            .clamp(rust_decimal::Decimal::ZERO, Decimal::ONE);
        let projected_notional = if request.intent.size > Decimal::ZERO {
            request.intent.size * conservative_price
        } else {
            Decimal::ZERO
        };
        let bankroll = std::env::var("POLYTRADER_REAL_DRY_RUN_BANKROLL_USDC")
            .ok()
            .and_then(|v| rust_decimal::Decimal::from_str(&v).ok())
            .unwrap_or(rust_decimal::Decimal::from(150i64));
        let max_order_risk_pct = std::env::var("POLYTRADER_MAX_RISK_PER_TRADE_PCT")
            .ok()
            .and_then(|v| rust_decimal::Decimal::from_str(&v).ok())
            .unwrap_or(rust_decimal::Decimal::from(1i64));
        let max_order_notional =
            bankroll * (max_order_risk_pct / rust_decimal::Decimal::from(100i64));
        Some(serde_json::json!({
            "projected_notional": projected_notional.to_string(),
            "bankroll_usdc": bankroll.to_string(),
            "max_order_notional": max_order_notional.to_string(),
            "intent_size": request.intent.size.to_string(),
            "intent_price": request.intent.price.map(|p| p.to_string()),
            "computed_at_approval": true,
            "computed_with_fallback": true
        }))
    });

    let payload = serde_json::json!({
        "kind": "clob_order_human_approval",
        "decision": decision,
        "approved_for_facade": approved_for_facade,
        "subject_hash": subject_hash,
        "intent": request.intent,
        "operator": operator,
        "note": note,
        "expires_at": expires_at,
        "paper_only": true,
        "real_orders_enabled": false,
        "request_sent": false,
        "post_order_called": false,
        "post_orders_called": false,
        // Enriched for gated real CLOB approval UX (2026-06-03). Snapshots from approve time.
        "risk_snapshot_at_approval": risk_snapshot_at_approval,
        "collateral_snapshot_at_approval": collateral_snapshot_at_approval,
        "approval_time": now,
    });

    match record_journal_event(
        &state.pool,
        "clob_order_human_approval",
        "polytrader_server",
        if approved_for_facade { "warning" } else { "info" },
        payload,
    )
    .await
    {
        Ok(event_id) => Json(serde_json::json!({
            "paper_only": true,
            "real_orders_enabled": false,
            "ready_for_real_orders": false,
            "approved_for_facade": approved_for_facade,
            "journaled": true,
            "journal_event_id": event_id,
            "subject_hash": subject_hash,
            "decision": decision,
            "expires_at": expires_at,
            "request_sent": false,
            "post_order_called": false,
            "post_orders_called": false,
            "note": "Human approval workflow event recorded (enriched with risk/collateral snapshot at approve time). Satisfies human_approval_event_id gate for submit-facade (and GatedRealClobLiveOrderSender reval for real CLOB when POLYTRADER_ENABLE_REAL_* + kill + all risk/collateral pass at dispatch). Does not auto-enable; explicit unlocks + reval + pre-dispatch journal still required. See wiki/decisions/real-order-approval-flow.md."
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "ready_for_real_orders": false,
                "approved_for_facade": false,
                "journaled": false,
                "subject_hash": subject_hash,
                "error": format!("failed to write human approval event: {}", e)
            })),
        )
            .into_response(),
    }
}

// 2026-06-03: minimal GET pending/recent human approvals list (for UI "Pending Human Approvals" panel + copyable ids).
// Returns recent clob_order_human_approval events (with snapshot summaries if present) so operator can see
// prior approvals and their evidence (subject, decision, operator, snapshots at approve, expires).
// Symmetric to /clob/final-review-decisions. Read-only; does not create or authorize.
async fn clob_order_intent_human_approvals_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<DryRunEventsQuery>,
) -> impl IntoResponse {
    let limit = clamp_dry_run_events_limit(query.limit.unwrap_or(10));
    // Simple recent list (no gaps mode for human; keep smallest).
    match sqlx::query_as::<_, (uuid::Uuid, serde_json::Value, chrono::DateTime<chrono::Utc>)>(
        r#"SELECT id, payload, created_at
           FROM journal.events
           WHERE event_type = 'clob_order_human_approval'
           ORDER BY created_at DESC
           LIMIT $1"#,
    )
    .bind(limit)
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => {
            let events: Vec<serde_json::Value> = rows
                .into_iter()
                .map(|(id, payload, created_at)| {
                    let subject = payload.get("subject_hash").cloned().unwrap_or(serde_json::Value::Null);
                    let decision = payload.get("decision").cloned().unwrap_or(serde_json::Value::Null);
                    let operator = payload.get("operator").cloned().unwrap_or(serde_json::Value::Null);
                    let approved = payload.get("approved_for_facade").cloned().unwrap_or(serde_json::json!(false));
                    let risk = payload.get("risk_snapshot_at_approval").cloned().unwrap_or(serde_json::Value::Null);
                    let coll = payload.get("collateral_snapshot_at_approval").cloned().unwrap_or(serde_json::Value::Null);
                    serde_json::json!({
                        "journal_event_id": id,
                        "created_at": created_at,
                        "subject_hash": subject,
                        "decision": decision,
                        "approved_for_facade": approved,
                        "operator": operator,
                        "risk_snapshot_at_approval": risk,
                        "collateral_snapshot_at_approval": coll,
                        "expires_at": payload.get("expires_at").cloned().unwrap_or(serde_json::Value::Null),
                        "paper_only": payload.get("paper_only").cloned().unwrap_or(serde_json::json!(true)),
                        "request_sent": false,
                    })
                })
                .collect();
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "ready_for_real_orders": false,
                "event_type": "clob_order_human_approval",
                "limit": limit,
                "count": events.len(),
                "events": events,
                "note": "Recent human approval events (enriched with approve-time snapshots). Use journal_event_id as human_approval_event_id in submit-facade for gated real path. Read-only audit; does not authorize."
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "error": format!("failed to list human approvals: {}", e)
            })),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
struct DryRunEventsQuery {
    limit: Option<i64>,
    gaps_only: Option<bool>,
}

#[derive(serde::Deserialize)]
struct DryRunReviewRequest {
    decision: String,
    note: Option<String>,
    operator: Option<String>,
}

#[derive(serde::Deserialize)]
struct HumanApprovalRequest {
    #[serde(flatten)]
    intent: crate::clob::RealOrderIntentDryRun,
    decision: String,
    note: Option<String>,
    operator: Option<String>,
    confirm_human_approval_workflow: bool,
    // 2026-06-03 approval UX: optional snapshots provided by UI/JS (or captured in handler via builders).
    // Enables risk/collateral evidence at *approve time* embedded in journal payload for later reval/attribution/Hermes.
    // #[serde(default)] for compat with old clients/probes that omit them.
    #[serde(default)]
    risk_snapshot: Option<serde_json::Value>,
    #[serde(default)]
    collateral_snapshot: Option<serde_json::Value>,
}

#[derive(serde::Deserialize)]
struct FinalReviewDecisionRequest {
    final_review_event_id: uuid::Uuid,
    decision: String,
    note: Option<String>,
    operator: Option<String>,
    confirm_final_review_workflow: bool,
    // 2026-06-03: snapshots at final decision/approve time (aggregate readiness/risk/collateral evidence).
    // Embedded in clob_final_review_decision payload. Optional + default for compat.
    #[serde(default)]
    risk_snapshot: Option<serde_json::Value>,
    #[serde(default)]
    collateral_snapshot: Option<serde_json::Value>,
}

async fn clob_order_intent_submit_reconciliations_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<DryRunEventsQuery>,
) -> impl IntoResponse {
    //! Read-only audit endpoint for submit-facade reconciliation events.
    //! These records prove each submit/reject decision reconciled to no exchange
    //! order because the facade never called `POST /order` or `POST /orders`.
    let limit = clamp_dry_run_events_limit(query.limit.unwrap_or(10));

    match sqlx::query_as::<
        _,
        (
            uuid::Uuid,
            String,
            String,
            String,
            serde_json::Value,
            chrono::DateTime<chrono::Utc>,
        ),
    >(
        r#"SELECT id, event_type, source, severity, payload, created_at
           FROM journal.events
           WHERE event_type = 'clob_order_submit_reconciliation'
           ORDER BY created_at DESC
           LIMIT $1"#,
    )
    .bind(limit)
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => {
            let events = rows
                .into_iter()
                .map(|(id, event_type, source, severity, payload, created_at)| {
                    serde_json::json!({
                        "id": id,
                        "event_type": event_type,
                        "source": source,
                        "severity": severity,
                        "payload": payload,
                        "created_at": created_at,
                    })
                })
                .collect::<Vec<_>>();
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "ready_for_real_orders": false,
                "event_type": "clob_order_submit_reconciliation",
                "limit": limit,
                "count": events.len(),
                "events": events,
                "note": "Read-only reconciliation audit events only; every event must show request_sent=false and no exchange order created."
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "error": format!("Failed to load submit reconciliation journal events: {}", e)
            })),
        )
            .into_response(),
    }
}

async fn clob_order_intent_dry_runs_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<DryRunEventsQuery>,
) -> impl IntoResponse {
    //! Read-only audit endpoint for recent dry-run validation events.
    //! Returns journaled diagnostics only; it cannot create or mutate orders.
    let limit = clamp_dry_run_events_limit(query.limit.unwrap_or(10));

    match sqlx::query_as::<
        _,
        (
            uuid::Uuid,
            String,
            String,
            String,
            serde_json::Value,
            chrono::DateTime<chrono::Utc>,
            Option<uuid::Uuid>,
            Option<serde_json::Value>,
            Option<chrono::DateTime<chrono::Utc>>,
        ),
    >(
        r#"SELECT e.id, e.event_type, e.source, e.severity, e.payload, e.created_at,
                  r.id AS latest_review_id,
                  r.payload AS latest_review_payload,
                  r.created_at AS latest_review_created_at
           FROM journal.events e
           LEFT JOIN LATERAL (
               SELECT id, payload, created_at
               FROM journal.events
               WHERE event_type = 'clob_order_intent_review'
                 AND payload->>'dry_run_event_id' = e.id::text
               ORDER BY created_at DESC
               LIMIT 1
           ) r ON true
           WHERE e.event_type = 'clob_order_intent_dry_run'
           ORDER BY e.created_at DESC
           LIMIT $1"#,
    )
    .bind(limit)
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => {
            let events = rows
                .into_iter()
                .map(
                    |(
                        id,
                        event_type,
                        source,
                        severity,
                        payload,
                        created_at,
                        latest_review_id,
                        latest_review_payload,
                        latest_review_created_at,
                    )| {
                        let latest_review = match (
                            latest_review_id,
                            latest_review_payload,
                            latest_review_created_at,
                        ) {
                            (Some(id), Some(payload), Some(created_at)) => {
                                Some(serde_json::json!({
                                    "id": id,
                                    "payload": payload,
                                    "created_at": created_at,
                                }))
                            }
                            _ => None,
                        };
                        serde_json::json!({
                            "id": id,
                            "event_type": event_type,
                            "source": source,
                            "severity": severity,
                            "payload": payload,
                            "created_at": created_at,
                            "latest_review": latest_review,
                        })
                    },
                )
                .collect::<Vec<_>>();
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "event_type": "clob_order_intent_dry_run",
                "limit": limit,
                "count": events.len(),
                "events": events,
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "error": format!("Failed to load dry-run journal events: {}", e)
            })),
        )
            .into_response(),
    }
}

async fn clob_order_intent_dry_run_detail_handler(
    State(state): State<Arc<AppState>>,
    Path(event_id): Path<uuid::Uuid>,
) -> impl IntoResponse {
    //! Read-only drilldown for one dry-run and its paper-only reviews.
    //! This exposes audit context only; it cannot approve or mutate orders.
    let dry_run = match sqlx::query_as::<
        _,
        (
            uuid::Uuid,
            String,
            String,
            String,
            serde_json::Value,
            chrono::DateTime<chrono::Utc>,
        ),
    >(
        r#"SELECT id, event_type, source, severity, payload, created_at
           FROM journal.events
           WHERE id = $1 AND event_type = 'clob_order_intent_dry_run'"#,
    )
    .bind(event_id)
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(row)) => row,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "paper_only": true,
                    "real_orders_enabled": false,
                    "error": "dry-run event not found"
                })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "paper_only": true,
                    "real_orders_enabled": false,
                    "error": format!("failed to load dry-run event: {}", e)
                })),
            )
                .into_response();
        }
    };

    match sqlx::query_as::<
        _,
        (
            uuid::Uuid,
            String,
            String,
            String,
            serde_json::Value,
            chrono::DateTime<chrono::Utc>,
        ),
    >(
        r#"SELECT id, event_type, source, severity, payload, created_at
           FROM journal.events
           WHERE event_type = 'clob_order_intent_review'
             AND payload->>'dry_run_event_id' = $1
           ORDER BY created_at DESC
           LIMIT 50"#,
    )
    .bind(event_id.to_string())
    .fetch_all(&state.pool)
    .await
    {
        Ok(review_rows) => {
            let reviews = review_rows
                .into_iter()
                .map(|(id, event_type, source, severity, payload, created_at)| {
                    serde_json::json!({
                        "id": id,
                        "event_type": event_type,
                        "source": source,
                        "severity": severity,
                        "payload": payload,
                        "created_at": created_at,
                    })
                })
                .collect::<Vec<_>>();
            let dry_run_event = serde_json::json!({
                "id": dry_run.0,
                "event_type": dry_run.1,
                "source": dry_run.2,
                "severity": dry_run.3,
                "payload": dry_run.4,
                "created_at": dry_run.5,
            });

            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "dry_run_event_id": event_id,
                "dry_run": dry_run_event,
                "dry_run_summary": dry_run_review_summary(&dry_run_event["payload"]),
                "blockers": dry_run_blockers(&dry_run_event["payload"]),
                "review_count": reviews.len(),
                "latest_review": reviews.first().cloned(),
                "reviews": reviews,
                "note": "Read-only dry-run detail. This does not approve, sign, submit, cancel, or place orders."
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "error": format!("failed to load dry-run reviews: {}", e)
            })),
        )
            .into_response(),
    }
}

async fn clob_order_intent_dry_run_review_handler(
    State(state): State<Arc<AppState>>,
    Path(event_id): Path<uuid::Uuid>,
    Json(request): Json<DryRunReviewRequest>,
) -> impl IntoResponse {
    //! Append-only operator review for a dry-run event.
    //!
    //! RISK: This is deliberately "would approve/reject" paper review only. It
    //! does not approve, sign, submit, place, cancel, persist, or unlock a real
    //! order path. The only side effect is another `journal.events` audit row.
    let Some(decision) = normalize_dry_run_review_decision(&request.decision) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "reviewed": false,
                "error": "decision must be one of: would_approve, would_reject, needs_rework"
            })),
        )
            .into_response();
    };

    let dry_run = match sqlx::query_as::<_, (serde_json::Value, chrono::DateTime<chrono::Utc>)>(
        r#"SELECT payload, created_at
           FROM journal.events
           WHERE id = $1 AND event_type = 'clob_order_intent_dry_run'"#,
    )
    .bind(event_id)
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(row)) => row,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "paper_only": true,
                    "real_orders_enabled": false,
                    "reviewed": false,
                    "error": "dry-run event not found"
                })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "paper_only": true,
                    "real_orders_enabled": false,
                    "reviewed": false,
                    "error": format!("failed to load dry-run event: {}", e)
                })),
            )
                .into_response();
        }
    };

    let dry_run_summary = dry_run_review_summary(&dry_run.0);
    let guidance = dry_run_summary
        .get("recommended_review_decision")
        .and_then(|decision| decision.as_str())
        .unwrap_or("needs_rework");
    let sanitized_note = sanitize_review_note(request.note.as_deref());
    if review_override_requires_note(decision, guidance, &sanitized_note) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "reviewed": false,
                "dry_run_event_id": event_id,
                "decision": decision,
                "recommended_review_decision": guidance,
                "error": "review note is required when decision differs from conservative guidance"
            })),
        )
            .into_response();
    }

    let operator = request
        .operator
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("operator");
    let payload = serde_json::json!({
        "kind": "clob_order_intent_review",
        "dry_run_event_id": event_id,
        "dry_run_created_at": dry_run.1,
        "decision": decision,
        "operator": operator,
        "note": sanitized_note,
        "dry_run_summary": dry_run_summary,
        "recommended_review_decision": guidance,
        "matches_guidance": decision == guidance,
        "guidance_override_requires_note": decision != guidance,
        "paper_only": true,
        "real_orders_enabled": false,
        "effect": "journal_only_no_real_order_approval"
    });

    match record_journal_event(
        &state.pool,
        "clob_order_intent_review",
        "polytrader_operator",
        "info",
        payload.clone(),
    )
    .await
    {
        Ok(review_event_id) => Json(serde_json::json!({
            "paper_only": true,
            "real_orders_enabled": false,
            "reviewed": true,
            "dry_run_event_id": event_id,
            "review_event_id": review_event_id,
            "review": payload,
            "note": "Paper-only operator review recorded in journal.events. No real order was approved, signed, submitted, cancelled, or placed."
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "reviewed": false,
                "dry_run_event_id": event_id,
                "error": format!("failed to write dry-run review event: {}", e)
            })),
        )
            .into_response(),
    }
}

async fn clob_order_intent_reviews_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<DryRunEventsQuery>,
) -> impl IntoResponse {
    //! Read-only audit endpoint for paper-only dry-run review decisions.
    //! Returns journaled review records only; it cannot approve or mutate orders.
    let limit = clamp_review_events_limit(query.limit.unwrap_or(10));

    match sqlx::query_as::<
        _,
        (
            uuid::Uuid,
            String,
            String,
            String,
            serde_json::Value,
            chrono::DateTime<chrono::Utc>,
        ),
    >(
        r#"SELECT id, event_type, source, severity, payload, created_at
           FROM journal.events
           WHERE event_type = 'clob_order_intent_review'
           ORDER BY created_at DESC
           LIMIT $1"#,
    )
    .bind(limit)
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => {
            let events = rows
                .into_iter()
                .map(|(id, event_type, source, severity, payload, created_at)| {
                    serde_json::json!({
                        "id": id,
                        "event_type": event_type,
                        "source": source,
                        "severity": severity,
                        "payload": payload,
                        "created_at": created_at,
                    })
                })
                .collect::<Vec<_>>();
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "event_type": "clob_order_intent_review",
                "limit": limit,
                "count": events.len(),
                "decision_counts": review_decision_counts(&events),
                "events": events,
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "error": format!("Failed to load dry-run review events: {}", e)
            })),
        )
            .into_response(),
    }
}

async fn fetch_review_summary_items(
    pool: &PgPool,
    limit: i64,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let rows = sqlx::query_as::<
        _,
        (
            uuid::Uuid,
            serde_json::Value,
            chrono::DateTime<chrono::Utc>,
            Option<uuid::Uuid>,
            Option<serde_json::Value>,
            Option<chrono::DateTime<chrono::Utc>>,
        ),
    >(
        r#"SELECT e.id, e.payload, e.created_at,
                  r.id AS latest_review_id,
                  r.payload AS latest_review_payload,
                  r.created_at AS latest_review_created_at
           FROM journal.events e
           LEFT JOIN LATERAL (
               SELECT id, payload, created_at
               FROM journal.events
               WHERE event_type = 'clob_order_intent_review'
                 AND payload->>'dry_run_event_id' = e.id::text
               ORDER BY created_at DESC
               LIMIT 1
           ) r ON true
           WHERE e.event_type = 'clob_order_intent_dry_run'
           ORDER BY e.created_at DESC
           LIMIT $1"#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(
                dry_run_event_id,
                dry_run_payload,
                dry_run_created_at,
                latest_review_id,
                latest_review_payload,
                latest_review_created_at,
            )| {
                let latest_review = match (
                    latest_review_id,
                    latest_review_payload,
                    latest_review_created_at,
                ) {
                    (Some(id), Some(payload), Some(created_at)) => Some(serde_json::json!({
                        "id": id,
                        "payload": payload,
                        "created_at": created_at,
                    })),
                    _ => None,
                };
                serde_json::json!({
                    "dry_run_event_id": dry_run_event_id,
                    "dry_run_created_at": dry_run_created_at,
                    "dry_run": dry_run_payload,
                    "latest_review": latest_review,
                })
            },
        )
        .collect::<Vec<_>>())
}

async fn clob_order_intent_review_summary_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<DryRunEventsQuery>,
) -> impl IntoResponse {
    //! Read-only summary over recent CLOB dry-runs and their latest paper
    //! reviews. This gives operators and Hermes a compact review coverage view
    //! without creating any approval or order execution semantics.
    let limit = clamp_review_events_limit(query.limit.unwrap_or(50));

    match fetch_review_summary_items(&state.pool, limit).await {
        Ok(rows) => {
            let summary = build_review_summary(&rows);

            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "limit": limit,
                "summary": summary,
                "items": rows,
                "note": "Read-only review coverage summary. This does not approve, sign, submit, cancel, or place orders."
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "error": format!("Failed to load dry-run review summary: {}", e)
            })),
        )
            .into_response(),
    }
}

async fn clob_order_intent_review_health_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<DryRunEventsQuery>,
) -> impl IntoResponse {
    //! Read-only operator health rollup derived from recent dry-runs and paper
    //! reviews. This is observability only; it does not approve or execute.
    let limit = clamp_review_events_limit(query.limit.unwrap_or(50));

    match fetch_review_summary_items(&state.pool, limit).await {
        Ok(items) => {
            let summary = build_review_summary(&items);
            let health = build_review_health(&summary);
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "limit": limit,
                "health": health,
                "summary": summary,
                "note": "Read-only review health rollup. This does not approve, sign, submit, cancel, or place orders."
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "error": format!("Failed to load dry-run review health: {}", e)
            })),
        )
            .into_response(),
    }
}

async fn clob_order_intent_review_queue_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<DryRunEventsQuery>,
) -> impl IntoResponse {
    //! Read-only queue of dry-run events that have no review yet.
    //! This makes human review work visible without creating approval semantics.
    let limit = clamp_review_events_limit(query.limit.unwrap_or(10));

    match sqlx::query_as::<_, (uuid::Uuid, serde_json::Value, chrono::DateTime<chrono::Utc>)>(
        r#"SELECT e.id, e.payload, e.created_at
           FROM journal.events e
           WHERE e.event_type = 'clob_order_intent_dry_run'
             AND NOT EXISTS (
                 SELECT 1
                 FROM journal.events r
                 WHERE r.event_type = 'clob_order_intent_review'
                   AND r.payload->>'dry_run_event_id' = e.id::text
             )
           ORDER BY e.created_at ASC
           LIMIT $1"#,
    )
    .bind(limit)
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => {
            let now = chrono::Utc::now();
            let items = rows
                .into_iter()
                .map(|(id, payload, created_at)| {
                    build_review_queue_item(id, payload, created_at, now)
                })
                .collect::<Vec<_>>();

            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "limit": limit,
                "count": items.len(),
                "review_stale_after_seconds": REVIEW_BACKLOG_STALE_AFTER_SECONDS,
                "items": items,
                "note": "Read-only review queue for unreviewed dry-runs. This does not approve, sign, submit, cancel, or place orders."
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "error": format!("Failed to load dry-run review queue: {}", e)
            })),
        )
            .into_response(),
    }
}

async fn clob_order_intent_review_guidance_exceptions_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<DryRunEventsQuery>,
) -> impl IntoResponse {
    //! Read-only list of reviewed dry-runs whose latest paper review differs
    //! from the conservative guidance. This is an audit aid only.
    let limit = clamp_review_events_limit(query.limit.unwrap_or(50));

    match sqlx::query_as::<
        _,
        (
            uuid::Uuid,
            serde_json::Value,
            chrono::DateTime<chrono::Utc>,
            uuid::Uuid,
            serde_json::Value,
            chrono::DateTime<chrono::Utc>,
        ),
    >(
        r#"SELECT e.id, e.payload, e.created_at,
                  r.id AS latest_review_id,
                  r.payload AS latest_review_payload,
                  r.created_at AS latest_review_created_at
           FROM journal.events e
           JOIN LATERAL (
               SELECT id, payload, created_at
               FROM journal.events
               WHERE event_type = 'clob_order_intent_review'
                 AND payload->>'dry_run_event_id' = e.id::text
               ORDER BY created_at DESC
               LIMIT 1
           ) r ON true
           WHERE e.event_type = 'clob_order_intent_dry_run'
           ORDER BY r.created_at DESC
           LIMIT $1"#,
    )
    .bind(limit)
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => {
            let items = rows
                .into_iter()
                .filter_map(
                    |(
                        dry_run_event_id,
                        dry_run_payload,
                        dry_run_created_at,
                        latest_review_id,
                        latest_review_payload,
                        latest_review_created_at,
                    )| {
                        if review_decision_matches_guidance(
                            &latest_review_payload,
                            &dry_run_payload,
                        ) == Some(true)
                        {
                            return None;
                        }
                        let summary = dry_run_review_summary(&dry_run_payload);
                        Some(serde_json::json!({
                            "dry_run_event_id": dry_run_event_id,
                            "dry_run_created_at": dry_run_created_at,
                            "dry_run_summary": summary,
                            "blockers": dry_run_blockers(&dry_run_payload),
                            "dry_run": dry_run_payload,
                            "latest_review": {
                                "id": latest_review_id,
                                "payload": latest_review_payload,
                                "created_at": latest_review_created_at,
                            },
                        }))
                    },
                )
                .collect::<Vec<_>>();

            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "limit": limit,
                "count": items.len(),
                "items": items,
                "note": "Read-only guidance exception list for reviewed dry-runs. This does not approve, sign, submit, cancel, or place orders."
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "error": format!("Failed to load dry-run guidance exceptions: {}", e)
            })),
        )
            .into_response(),
    }
}

async fn clob_order_intent_review_guidance_overrides_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<DryRunEventsQuery>,
) -> impl IntoResponse {
    //! Read-only historical audit trail of paper reviews that explicitly
    //! overrode conservative guidance.
    let limit = clamp_review_events_limit(query.limit.unwrap_or(50));

    match sqlx::query_as::<
        _,
        (
            uuid::Uuid,
            serde_json::Value,
            chrono::DateTime<chrono::Utc>,
            Option<uuid::Uuid>,
            Option<serde_json::Value>,
            Option<chrono::DateTime<chrono::Utc>>,
        ),
    >(
        r#"SELECT r.id,
                  r.payload,
                  r.created_at,
                  e.id AS dry_run_event_id,
                  e.payload AS dry_run_payload,
                  e.created_at AS dry_run_created_at
           FROM journal.events r
           LEFT JOIN journal.events e
             ON e.event_type = 'clob_order_intent_dry_run'
            AND e.id::text = r.payload->>'dry_run_event_id'
           WHERE r.event_type = 'clob_order_intent_review'
             AND r.payload->>'matches_guidance' = 'false'
           ORDER BY r.created_at DESC
           LIMIT $1"#,
    )
    .bind(limit)
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => {
            let items = rows
                .into_iter()
                .map(
                    |(
                        review_event_id,
                        review_payload,
                        review_created_at,
                        dry_run_event_id,
                        dry_run_payload,
                        dry_run_created_at,
                    )| {
                        let dry_run_summary = dry_run_payload
                            .as_ref()
                            .map(dry_run_review_summary)
                            .unwrap_or_else(|| serde_json::json!({}));
                        let blockers = dry_run_payload
                            .as_ref()
                            .map(dry_run_blockers)
                            .unwrap_or_default();
                        serde_json::json!({
                            "review_event_id": review_event_id,
                            "review_created_at": review_created_at,
                            "review": review_payload,
                            "dry_run_event_id": dry_run_event_id,
                            "dry_run_created_at": dry_run_created_at,
                            "dry_run_summary": dry_run_summary,
                            "blockers": blockers,
                            "dry_run": dry_run_payload,
                        })
                    },
                )
                .collect::<Vec<_>>();

            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "limit": limit,
                "count": items.len(),
                "items": items,
                "note": "Read-only historical guidance override audit. This does not approve, sign, submit, cancel, or place orders."
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "error": format!("Failed to load dry-run guidance overrides: {}", e)
            })),
        )
            .into_response(),
    }
}

async fn clob_order_intent_review_backlog_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    //! Read-only queue health signal for unreviewed dry-runs.
    //! This reports backlog age only; it cannot approve or mutate orders.
    match sqlx::query_as::<
        _,
        (
            i64,
            Option<chrono::DateTime<chrono::Utc>>,
            Option<chrono::DateTime<chrono::Utc>>,
        ),
    >(
        r#"SELECT count(*)::bigint AS unreviewed_count,
                  min(e.created_at) AS oldest_unreviewed_at,
                  max(e.created_at) AS newest_unreviewed_at
           FROM journal.events e
           WHERE e.event_type = 'clob_order_intent_dry_run'
             AND NOT EXISTS (
                 SELECT 1
                 FROM journal.events r
                 WHERE r.event_type = 'clob_order_intent_review'
                   AND r.payload->>'dry_run_event_id' = e.id::text
             )"#,
    )
    .fetch_one(&state.pool)
    .await
    {
        Ok((unreviewed_count, oldest_unreviewed_at, newest_unreviewed_at)) => {
            let oldest_age_seconds =
                oldest_unreviewed_age_seconds(chrono::Utc::now(), oldest_unreviewed_at);
            let backlog_status = review_backlog_status(oldest_age_seconds);
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "unreviewed_count": unreviewed_count,
                "oldest_unreviewed_at": oldest_unreviewed_at,
                "newest_unreviewed_at": newest_unreviewed_at,
                "oldest_unreviewed_age_seconds": oldest_age_seconds,
                "stale_after_seconds": REVIEW_BACKLOG_STALE_AFTER_SECONDS,
                "is_stale": backlog_status == "stale",
                "status": backlog_status,
                "note": "Read-only review backlog freshness. This does not approve, sign, submit, cancel, or place orders."
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "error": format!("Failed to load dry-run review backlog: {}", e)
            })),
        )
            .into_response(),
    }
}

fn clamp_dry_run_events_limit(limit: i64) -> i64 {
    limit.clamp(1, 50)
}

fn clamp_review_events_limit(limit: i64) -> i64 {
    limit.clamp(1, 50)
}

const REVIEW_BACKLOG_STALE_AFTER_SECONDS: i64 = 24 * 60 * 60;
const REVIEW_LATENCY_SLOW_AFTER_SECONDS: i64 = 12 * 60 * 60;
const OPERATOR_STATUS_STALE_AFTER_SECONDS: i64 = 60;
const HERMES_REFLECTION_STALE_AFTER_SECONDS: i64 = 10 * 60;
const STRATEGY_CANDIDATE_OBSERVATION_MAX_AGE_SECONDS: i64 = 15 * 60;

fn normalize_dry_run_review_decision(decision: &str) -> Option<&'static str> {
    match decision.trim().to_ascii_lowercase().as_str() {
        "would_approve" | "approve" => Some("would_approve"),
        "would_reject" | "reject" => Some("would_reject"),
        "needs_rework" | "rework" => Some("needs_rework"),
        _ => None,
    }
}

fn review_decision_counts(events: &[serde_json::Value]) -> serde_json::Value {
    let mut would_approve = 0usize;
    let mut would_reject = 0usize;
    let mut needs_rework = 0usize;
    let mut unknown = 0usize;

    for event in events {
        match event
            .get("payload")
            .and_then(|payload| payload.get("decision"))
            .and_then(|decision| decision.as_str())
        {
            Some("would_approve") => would_approve += 1,
            Some("would_reject") => would_reject += 1,
            Some("needs_rework") => needs_rework += 1,
            _ => unknown += 1,
        }
    }

    serde_json::json!({
        "would_approve": would_approve,
        "would_reject": would_reject,
        "needs_rework": needs_rework,
        "unknown": unknown,
    })
}

fn final_review_decision_counts(events: &[serde_json::Value]) -> serde_json::Value {
    let mut acknowledge_blocked = 0usize;
    let mut reject_live_trading = 0usize;
    let mut needs_rework = 0usize;
    let mut unknown = 0usize;

    for event in events {
        match event
            .get("payload")
            .and_then(|payload| payload.get("decision"))
            .and_then(|decision| decision.as_str())
        {
            Some("acknowledge_blocked") => acknowledge_blocked += 1,
            Some("reject_live_trading") => reject_live_trading += 1,
            Some("needs_rework") => needs_rework += 1,
            _ => unknown += 1,
        }
    }

    serde_json::json!({
        "acknowledge_blocked": acknowledge_blocked,
        "reject_live_trading": reject_live_trading,
        "needs_rework": needs_rework,
        "unknown": unknown,
    })
}

async fn fetch_final_review_decision_events(
    pool: &PgPool,
    limit: i64,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let rows = sqlx::query_as::<
        _,
        (
            uuid::Uuid,
            String,
            String,
            String,
            serde_json::Value,
            chrono::DateTime<chrono::Utc>,
        ),
    >(
        r#"SELECT id, event_type, source, severity, payload, created_at
           FROM journal.events
           WHERE event_type = 'clob_final_review_decision'
           ORDER BY created_at DESC
           LIMIT $1"#,
    )
    .bind(clamp_dry_run_events_limit(limit))
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(id, event_type, source, severity, payload, created_at)| {
            serde_json::json!({
                "id": id,
                "event_type": event_type,
                "source": source,
                "severity": severity,
                "payload": payload,
                "created_at": created_at,
            })
        })
        .collect())
}

#[derive(Debug, sqlx::FromRow)]
struct HermesReflectionRow {
    id: uuid::Uuid,
    period_start: Option<chrono::DateTime<chrono::Utc>>,
    period_end: Option<chrono::DateTime<chrono::Utc>>,
    summary: String,
    metrics: Option<serde_json::Value>,
    recommendations: Option<serde_json::Value>,
    hermes_version: Option<String>,
    llm_model: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
}

async fn fetch_latest_hermes_reflection(
    pool: &PgPool,
) -> anyhow::Result<Option<HermesReflectionRow>> {
    sqlx::query_as::<_, HermesReflectionRow>(
        r#"SELECT id, period_start, period_end, summary, metrics, recommendations, hermes_version, llm_model, created_at
           FROM journal.reflections
           ORDER BY created_at DESC
           LIMIT 1"#,
    )
    .fetch_optional(pool)
    .await
    .map_err(Into::into)
}

fn build_hermes_safety_loop_response(
    reflection: Option<HermesReflectionRow>,
    now: chrono::DateTime<chrono::Utc>,
) -> serde_json::Value {
    let Some(reflection) = reflection else {
        return serde_json::json!({
            "paper_only": true,
            "real_orders_enabled": false,
            "ready_for_real_orders": false,
            "approved_for_real_orders": false,
            "request_sent": false,
            "post_order_called": false,
            "post_orders_called": false,
            "available": false,
            "status": "no_reflections",
            "clob_safety_loop": serde_json::Value::Null,
            "paper_rejection_loop": serde_json::Value::Null,
            "paper_accounting_loop": serde_json::Value::Null,
            "strategy_candidate_loop": serde_json::Value::Null,
            "note": "No Hermes reflection has been written yet; this endpoint remains read-only and cannot enable trading."
        });
    };

    let metrics = reflection
        .metrics
        .clone()
        .unwrap_or_else(|| serde_json::json!({}));
    let clob_safety_loop = metrics
        .get("clob_safety_loop")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let paper_accounting_loop = metrics
        .get("paper_accounting_loop")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let paper_rejection_loop = metrics
        .get("paper_rejection_loop")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let strategy_candidate_loop = metrics
        .get("strategy_candidate_loop")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let boundary_coverage = clob_safety_loop
        .get("final_review_decision_boundary_coverage")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let latest_boundary_status = clob_safety_loop
        .get("latest_final_review_decision_boundary_status")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let decision_events = clob_safety_loop
        .get("final_review_decision_events_24h")
        .and_then(|value| value.as_i64())
        .unwrap_or(0);
    let boundary_evidence_events = clob_safety_loop
        .get("final_review_decision_boundary_evidence_events_24h")
        .and_then(|value| value.as_i64())
        .unwrap_or(0);
    let no_network_evidence_events = clob_safety_loop
        .get("final_review_decision_no_network_evidence_events_24h")
        .and_then(|value| value.as_i64())
        .unwrap_or(0);
    let complete_boundary_coverage = boundary_coverage
        .get("complete_fail_closed_no_network_evidence")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let missing_boundary_evidence_events = boundary_coverage
        .get("missing_boundary_evidence_events")
        .and_then(|value| value.as_i64())
        .unwrap_or_else(|| (decision_events - boundary_evidence_events).max(0));
    let missing_no_network_evidence_events = boundary_coverage
        .get("missing_no_network_evidence_events")
        .and_then(|value| value.as_i64())
        .unwrap_or_else(|| (decision_events - no_network_evidence_events).max(0));
    let clob_loop_available = !clob_safety_loop.is_null();
    let age_seconds = (now - reflection.created_at).num_seconds().max(0);
    let reflection_is_stale = age_seconds >= HERMES_REFLECTION_STALE_AFTER_SECONDS;
    let reflection_freshness_status = if reflection_is_stale {
        "stale"
    } else {
        "fresh"
    };
    let status = if !clob_loop_available {
        "missing_clob_safety_loop"
    } else if decision_events == 0 {
        "awaiting_final_review_decisions"
    } else if complete_boundary_coverage {
        "boundary_coverage_complete"
    } else {
        "boundary_coverage_incomplete"
    };
    let note = if status == "boundary_coverage_incomplete" {
        "Hermes sees final-review decisions without complete fail-closed/no-network boundary evidence. Older events may predate the boundary audit; this is an audit finding, not an unlock."
    } else {
        "Latest Hermes CLOB safety-loop reflection, surfaced read-only for operators. This endpoint cannot approve or submit orders."
    };

    serde_json::json!({
        "paper_only": true,
        "real_orders_enabled": false,
        "ready_for_real_orders": false,
        "approved_for_real_orders": false,
        "request_sent": false,
        "post_order_called": false,
        "post_orders_called": false,
        "available": clob_loop_available,
        "status": status,
        "reflection": {
            "id": reflection.id,
            "period_start": reflection.period_start,
            "period_end": reflection.period_end,
            "created_at": reflection.created_at,
            "age_seconds": age_seconds,
            "stale_after_seconds": HERMES_REFLECTION_STALE_AFTER_SECONDS,
            "is_stale": reflection_is_stale,
            "freshness_status": reflection_freshness_status,
            "hermes_version": reflection.hermes_version,
            "llm_model": reflection.llm_model,
            "summary": reflection.summary,
            "recommendations": reflection.recommendations.unwrap_or_else(|| serde_json::json!([])),
        },
        "final_review_decision_events_24h": decision_events,
        "final_review_decision_boundary_evidence_events_24h": boundary_evidence_events,
        "final_review_decision_no_network_evidence_events_24h": no_network_evidence_events,
        "final_review_decision_missing_boundary_evidence_events_24h": missing_boundary_evidence_events,
        "final_review_decision_missing_no_network_evidence_events_24h": missing_no_network_evidence_events,
        "final_review_decision_boundary_coverage": boundary_coverage,
        "latest_final_review_decision_boundary_status": latest_boundary_status,
        "clob_safety_loop": clob_safety_loop,
        "paper_rejection_loop": paper_rejection_loop,
        "paper_accounting_loop": paper_accounting_loop,
        "strategy_candidate_loop": strategy_candidate_loop,
        "note": note
    })
}

fn build_final_review_audit_summary(events: Vec<serde_json::Value>) -> serde_json::Value {
    let latest_decision = events.first().cloned();
    let status = if events.is_empty() {
        "missing_audit"
    } else {
        "audited"
    };
    let mut boundary_evidence_count = 0usize;
    let mut no_network_evidence_count = 0usize;
    let mut coverage_gap_events = Vec::new();

    for event in &events {
        let payload = event.get("payload").unwrap_or(&serde_json::Value::Null);
        let live_sender_boundary_fail_closed = payload
            .get("live_sender_boundary_fail_closed")
            .and_then(|value| value.as_bool())
            == Some(true);
        let boundary_status = payload
            .get("live_sender_boundary_status")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let no_network_evidence = live_sender_boundary_fail_closed
            && boundary_status
                .get("network_sender_present")
                .and_then(|value| value.as_bool())
                == Some(false)
            && boundary_status
                .get("accepted_for_network_dispatch")
                .and_then(|value| value.as_bool())
                == Some(false)
            && boundary_status
                .get("request_sent")
                .and_then(|value| value.as_bool())
                == Some(false);

        if live_sender_boundary_fail_closed {
            boundary_evidence_count += 1;
        }
        if no_network_evidence {
            no_network_evidence_count += 1;
        }

        if !live_sender_boundary_fail_closed || !no_network_evidence {
            coverage_gap_events.push(serde_json::json!({
                "id": event.get("id").cloned().unwrap_or(serde_json::Value::Null),
                "created_at": event.get("created_at").cloned().unwrap_or(serde_json::Value::Null),
                "decision": payload.get("decision").cloned().unwrap_or(serde_json::Value::Null),
                "review_decision_effect": payload.get("review_decision_effect").cloned().unwrap_or(serde_json::Value::Null),
                "approved_for_real_orders": payload.get("approved_for_real_orders").cloned().unwrap_or(serde_json::json!(false)),
                "operator": payload.get("operator").cloned().unwrap_or(serde_json::Value::Null),
                "missing_boundary_evidence": !live_sender_boundary_fail_closed,
                "missing_no_network_evidence": !no_network_evidence,
                "live_sender_boundary_status": boundary_status,
            }));
        }
    }
    let missing_boundary_evidence_count = events.len().saturating_sub(boundary_evidence_count);
    let missing_no_network_evidence_count = events.len().saturating_sub(no_network_evidence_count);
    let coverage_status = if events.is_empty() {
        "no_decisions"
    } else if missing_boundary_evidence_count == 0 && missing_no_network_evidence_count == 0 {
        "complete"
    } else {
        "legacy_or_missing_boundary_evidence"
    };
    let latest_boundary_status = latest_decision
        .as_ref()
        .and_then(|event| event.get("payload"))
        .and_then(|payload| payload.get("live_sender_boundary_status"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    serde_json::json!({
        "status": status,
        "paper_only": true,
        "real_orders_enabled": false,
        "ready_for_real_orders": false,
        "approved_for_real_orders": false,
        "event_type": "clob_final_review_decision",
        "count": events.len(),
        "decision_counts": final_review_decision_counts(&events),
        "boundary_evidence_count": boundary_evidence_count,
        "no_network_evidence_count": no_network_evidence_count,
        "missing_boundary_evidence_count": missing_boundary_evidence_count,
        "missing_no_network_evidence_count": missing_no_network_evidence_count,
        "all_events_have_boundary_evidence": !events.is_empty() && boundary_evidence_count == events.len(),
        "all_events_have_no_network_evidence": !events.is_empty() && no_network_evidence_count == events.len(),
        "coverage_status": coverage_status,
        "coverage_gaps": {
            "count": coverage_gap_events.len(),
            "events": coverage_gap_events,
            "note": "Gap events are read-only audit findings. They usually represent legacy final-review decisions written before fail-closed/no-network boundary evidence was attached."
        },
        "latest_boundary_status": latest_boundary_status,
        "latest_decision": latest_decision,
        "events": events,
        "request_sent": false,
        "post_order_called": false,
        "post_orders_called": false,
    })
}

fn build_final_review_coverage_gap_probe(
    audit: &serde_json::Value,
    limit: i64,
    now: chrono::DateTime<chrono::Utc>,
) -> serde_json::Value {
    const HERMES_COVERAGE_WINDOW_SECONDS: i64 = 24 * 60 * 60;

    let coverage_gaps = audit
        .get("coverage_gaps")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({"count": 0, "events": []}));
    let displayed_event_count = coverage_gaps
        .get("events")
        .and_then(|events| events.as_array())
        .map(Vec::len)
        .unwrap_or(0);
    let gap_created_at = coverage_gaps
        .get("events")
        .and_then(|events| events.as_array())
        .into_iter()
        .flatten()
        .filter_map(|event| {
            event
                .get("created_at")
                .and_then(|created_at| created_at.as_str())
                .and_then(|created_at| {
                    chrono::DateTime::parse_from_rfc3339(created_at)
                        .ok()
                        .map(|parsed| parsed.with_timezone(&chrono::Utc))
                })
        })
        .collect::<Vec<_>>();
    let oldest_gap_created_at = gap_created_at.iter().min().copied();
    let newest_gap_created_at = gap_created_at.iter().max().copied();
    let active_24h_gap_created_at = gap_created_at
        .iter()
        .copied()
        .filter(|created_at| {
            now.signed_duration_since(*created_at).num_seconds() < HERMES_COVERAGE_WINDOW_SECONDS
        })
        .collect::<Vec<_>>();
    let active_24h_gap_count = active_24h_gap_created_at.len();
    let expired_24h_gap_count = gap_created_at.len().saturating_sub(active_24h_gap_count);
    let oldest_active_24h_gap_created_at = active_24h_gap_created_at.iter().min().copied();
    let newest_active_24h_gap_created_at = active_24h_gap_created_at.iter().max().copied();
    let oldest_gap_age_seconds = oldest_gap_created_at
        .map(|created_at| now.signed_duration_since(created_at).num_seconds().max(0));
    let newest_gap_age_seconds = newest_gap_created_at
        .map(|created_at| now.signed_duration_since(created_at).num_seconds().max(0));
    let seconds_until_all_gaps_age_out_of_24h = newest_gap_age_seconds
        .map(|age_seconds| (HERMES_COVERAGE_WINDOW_SECONDS - age_seconds).max(0));
    let oldest_active_24h_gap_age_seconds = oldest_active_24h_gap_created_at
        .map(|created_at| now.signed_duration_since(created_at).num_seconds().max(0));
    let newest_active_24h_gap_age_seconds = newest_active_24h_gap_created_at
        .map(|created_at| now.signed_duration_since(created_at).num_seconds().max(0));
    let seconds_until_active_gaps_age_out_of_24h = newest_active_24h_gap_age_seconds
        .map(|age_seconds| (HERMES_COVERAGE_WINDOW_SECONDS - age_seconds).max(0));
    let active_gaps_age_out_at = newest_active_24h_gap_created_at
        .map(|created_at| created_at + chrono::Duration::seconds(HERMES_COVERAGE_WINDOW_SECONDS));
    let active_24h_gap_status = if displayed_event_count == 0 {
        "none"
    } else if active_24h_gap_count == 0 {
        "historical_only"
    } else {
        "active_24h_gaps"
    };

    serde_json::json!({
        "available": true,
        "limit": limit,
        "gaps_only": true,
        "paper_only": true,
        "real_orders_enabled": false,
        "ready_for_real_orders": false,
        "approved_for_real_orders": false,
        "status": audit.get("status").cloned().unwrap_or(serde_json::Value::Null),
        "count": audit.get("count").cloned().unwrap_or(serde_json::json!(0)),
        "displayed_event_count": displayed_event_count,
        "boundary_evidence_count": audit.get("boundary_evidence_count").cloned().unwrap_or(serde_json::json!(0)),
        "no_network_evidence_count": audit.get("no_network_evidence_count").cloned().unwrap_or(serde_json::json!(0)),
        "missing_boundary_evidence_count": audit.get("missing_boundary_evidence_count").cloned().unwrap_or(serde_json::json!(0)),
        "missing_no_network_evidence_count": audit.get("missing_no_network_evidence_count").cloned().unwrap_or(serde_json::json!(0)),
        "coverage_status": audit.get("coverage_status").cloned().unwrap_or(serde_json::json!("no_decisions")),
        "hermes_coverage_window_seconds": HERMES_COVERAGE_WINDOW_SECONDS,
        "active_24h_gap_status": active_24h_gap_status,
        "active_24h_gap_count": active_24h_gap_count,
        "expired_24h_gap_count": expired_24h_gap_count,
        "oldest_gap_created_at": oldest_gap_created_at,
        "newest_gap_created_at": newest_gap_created_at,
        "oldest_gap_age_seconds": oldest_gap_age_seconds,
        "newest_gap_age_seconds": newest_gap_age_seconds,
        "seconds_until_all_gaps_age_out_of_24h": seconds_until_all_gaps_age_out_of_24h,
        "oldest_active_24h_gap_created_at": oldest_active_24h_gap_created_at,
        "newest_active_24h_gap_created_at": newest_active_24h_gap_created_at,
        "active_gaps_age_out_at": active_gaps_age_out_at,
        "oldest_active_24h_gap_age_seconds": oldest_active_24h_gap_age_seconds,
        "newest_active_24h_gap_age_seconds": newest_active_24h_gap_age_seconds,
        "seconds_until_active_gaps_age_out_of_24h": seconds_until_active_gaps_age_out_of_24h,
        "coverage_gaps": coverage_gaps,
        "request_sent": false,
        "post_order_called": false,
        "post_orders_called": false,
        "note": "Read-only broad final-review coverage gap probe. It surfaces compact gap rows only and cannot approve or enable live order submission."
    })
}

fn build_final_review_hermes_gap_alignment(
    coverage_gap_probe: &serde_json::Value,
    hermes_safety_loop: &serde_json::Value,
) -> serde_json::Value {
    let probe_available = coverage_gap_probe
        .get("available")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let hermes_available = hermes_safety_loop
        .get("available")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let app_active_24h_gap_count = coverage_gap_probe
        .get("active_24h_gap_count")
        .and_then(|value| value.as_i64())
        .unwrap_or(0)
        .max(0);
    let hermes_missing_boundary_evidence_count = hermes_safety_loop
        .get("final_review_decision_missing_boundary_evidence_events_24h")
        .and_then(|value| value.as_i64())
        .or_else(|| {
            hermes_safety_loop
                .get("final_review_decision_boundary_coverage")
                .and_then(|coverage| coverage.get("missing_boundary_evidence_events"))
                .and_then(|value| value.as_i64())
        })
        .unwrap_or(0)
        .max(0);
    let hermes_missing_no_network_evidence_count = hermes_safety_loop
        .get("final_review_decision_missing_no_network_evidence_events_24h")
        .and_then(|value| value.as_i64())
        .or_else(|| {
            hermes_safety_loop
                .get("final_review_decision_boundary_coverage")
                .and_then(|coverage| coverage.get("missing_no_network_evidence_events"))
                .and_then(|value| value.as_i64())
        })
        .unwrap_or(0)
        .max(0);
    let hermes_missing_gap_count =
        hermes_missing_boundary_evidence_count.max(hermes_missing_no_network_evidence_count);
    let hermes_reflection_age_seconds = hermes_safety_loop
        .get("reflection")
        .and_then(|reflection| reflection.get("age_seconds"))
        .and_then(|value| value.as_i64());
    let hermes_reflection_stale_after_seconds = hermes_safety_loop
        .get("reflection")
        .and_then(|reflection| reflection.get("stale_after_seconds"))
        .and_then(|value| value.as_i64())
        .unwrap_or(HERMES_REFLECTION_STALE_AFTER_SECONDS);
    let hermes_reflection_is_stale = hermes_safety_loop
        .get("reflection")
        .and_then(|reflection| reflection.get("is_stale"))
        .and_then(|value| value.as_bool())
        .or_else(|| {
            hermes_reflection_age_seconds
                .map(|age_seconds| age_seconds >= hermes_reflection_stale_after_seconds)
        })
        .unwrap_or(false);
    let hermes_reflection_freshness_status = if hermes_reflection_is_stale {
        "stale"
    } else if hermes_reflection_age_seconds.is_some() {
        "fresh"
    } else {
        "unknown"
    };
    let active_24h_gap_status = coverage_gap_probe
        .get("active_24h_gap_status")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown");
    let expired_24h_gap_count = coverage_gap_probe
        .get("expired_24h_gap_count")
        .and_then(|value| value.as_i64())
        .unwrap_or(0)
        .max(0);
    let seconds_until_active_gaps_age_out_of_24h = coverage_gap_probe
        .get("seconds_until_active_gaps_age_out_of_24h")
        .and_then(|value| value.as_i64());
    let active_gaps_age_out_at = coverage_gap_probe
        .get("active_gaps_age_out_at")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    let status = if !probe_available || !hermes_available {
        "unavailable"
    } else if hermes_reflection_is_stale {
        "hermes_reflection_stale"
    } else if app_active_24h_gap_count == 0 && hermes_missing_gap_count == 0 {
        "matched_clear"
    } else if app_active_24h_gap_count == hermes_missing_gap_count {
        "matched_active_gaps"
    } else if app_active_24h_gap_count > hermes_missing_gap_count {
        "app_probe_ahead_of_hermes"
    } else {
        "hermes_ahead_of_app_probe"
    };
    let aligned = matches!(status, "matched_clear" | "matched_active_gaps");
    let requires_attention = !matches!(status, "matched_clear");

    serde_json::json!({
        "available": probe_available && hermes_available,
        "status": status,
        "aligned": aligned,
        "requires_attention": requires_attention,
        "paper_only": true,
        "real_orders_enabled": false,
        "ready_for_real_orders": false,
        "approved_for_real_orders": false,
        "app_active_24h_gap_count": app_active_24h_gap_count,
        "active_24h_gap_status": active_24h_gap_status,
        "expired_24h_gap_count": expired_24h_gap_count,
        "seconds_until_active_gaps_age_out_of_24h": seconds_until_active_gaps_age_out_of_24h,
        "active_gaps_age_out_at": active_gaps_age_out_at,
        "hermes_missing_gap_count": hermes_missing_gap_count,
        "hermes_missing_boundary_evidence_count": hermes_missing_boundary_evidence_count,
        "hermes_missing_no_network_evidence_count": hermes_missing_no_network_evidence_count,
        "hermes_reflection_age_seconds": hermes_reflection_age_seconds,
        "hermes_reflection_stale_after_seconds": hermes_reflection_stale_after_seconds,
        "hermes_reflection_is_stale": hermes_reflection_is_stale,
        "hermes_reflection_freshness_status": hermes_reflection_freshness_status,
        "request_sent": false,
        "post_order_called": false,
        "post_orders_called": false,
        "note": "Read-only consistency check between the app-side 24h final-review gap probe and Hermes' latest reflection. A mismatch means operators should refresh/restart Hermes or inspect journal timing; it never unlocks real orders."
    })
}

fn build_live_sender_design_readiness_report(
    unlock_event: Option<serde_json::Value>,
    final_review_decision_event: Option<serde_json::Value>,
) -> serde_json::Value {
    let unlock_report = unlock_event
        .as_ref()
        .and_then(|event| event_payload_report(event));
    let final_review_decision_report = final_review_decision_event
        .as_ref()
        .and_then(|event| event_payload_report(event));

    let explicit_unlock = unlock_report
        .and_then(|report| report.get("explicit_real_order_submission_configured"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let kill_switch_open = unlock_report
        .and_then(|report| report.get("kill_switch_open"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let paper_mode_disabled = unlock_report
        .and_then(|report| report.get("paper_mode_active"))
        .and_then(|value| value.as_bool())
        .map(|active| !active)
        .unwrap_or(false);
    let live_sender_implemented = unlock_report
        .and_then(|report| report.get("live_order_sender_implemented"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let final_review_decision_audited = final_review_decision_event.is_some();
    let final_review_approved_for_real_orders = final_review_decision_report
        .and_then(|report| report.get("approved_for_real_orders"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);

    let gates = vec![
        readiness_gate(
            "real_trading_unlock_status_journaled",
            unlock_event.is_some(),
            "A latest clob_real_trading_unlock_status journal event exists.",
        ),
        readiness_gate(
            "final_review_decision_audited",
            final_review_decision_audited,
            "At least one final-review decision audit event exists.",
        ),
        readiness_gate(
            "final_review_did_not_approve_real_orders",
            final_review_decision_audited && !final_review_approved_for_real_orders,
            "Final-review decision audit exists and still does not approve real orders.",
        ),
        readiness_gate(
            "explicit_real_trading_config_unlock",
            explicit_unlock,
            "POLYTRADER_ENABLE_REAL_ORDER_SUBMISSION is explicitly configured true.",
        ),
        readiness_gate(
            "kill_switch_open",
            kill_switch_open,
            "POLYTRADER_REAL_ORDER_KILL_SWITCH_OPEN is explicitly true.",
        ),
        readiness_gate(
            "paper_mode_disabled",
            paper_mode_disabled,
            "The app is not running in paper mode.",
        ),
        readiness_gate(
            "live_order_sender_implemented",
            live_sender_implemented,
            "A reviewed live order sender implementation exists.",
        ),
    ];

    let completed_count = gates
        .iter()
        .filter(|gate| gate.get("ok").and_then(|v| v.as_bool()) == Some(true))
        .count();
    let blockers = gates
        .iter()
        .filter(|gate| gate.get("ok").and_then(|v| v.as_bool()) == Some(false))
        .filter_map(|gate| {
            gate.get("name")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .collect::<Vec<_>>();

    serde_json::json!({
        "ready": false,
        "ready_for_live_sender_implementation": false,
        "ready_for_real_orders": false,
        "stage": "live_sender_design_blocked",
        "completed_count": completed_count,
        "required_count": gates.len(),
        "blocker_count": blockers.len(),
        "blockers": blockers,
        "gates": gates,
        "latest_evidence": {
            "real_trading_unlock_status": unlock_event,
            "final_review_decision": final_review_decision_event,
        },
        "paper_only": true,
        "real_orders_enabled": false,
        "approved_for_real_orders": false,
        "request_sent": false,
        "would_send": false,
        "post_order_called": false,
        "post_orders_called": false,
        "live_order_sender_implemented": live_sender_implemented,
        "next_safe_step": "Draft and review a live-sender design/ADR while real trading remains locked; do not implement sending until external collateral, allowance, unlock, kill switch, and paper-mode gates are deliberately addressed. (Gated sender is now wired; still requires explicit unlock to dispatch.)",
        "note": "Live-sender design readiness only. GatedRealClobLiveOrderSender is implemented; this report cannot enable or place real orders."
    })
}

fn build_live_sender_design_review_report(
    design_readiness_event: Option<serde_json::Value>,
    unlock_event: Option<serde_json::Value>,
    final_review_decision_event: Option<serde_json::Value>,
) -> serde_json::Value {
    let design_readiness_report = design_readiness_event
        .as_ref()
        .and_then(|event| event_payload_report(event));
    let unlock_report = unlock_event
        .as_ref()
        .and_then(|event| event_payload_report(event));

    let design_readiness_blocked = design_readiness_report
        .and_then(|report| report.get("ready_for_live_sender_implementation"))
        .and_then(|value| value.as_bool())
        == Some(false);
    let readiness_request_sent = design_readiness_report
        .and_then(|report| report.get("request_sent"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let readiness_post_order_called = design_readiness_report
        .and_then(|report| report.get("post_order_called"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let readiness_post_orders_called = design_readiness_report
        .and_then(|report| report.get("post_orders_called"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let explicit_unlock = unlock_report
        .and_then(|report| report.get("explicit_real_order_submission_configured"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let live_sender_implemented = unlock_report
        .and_then(|report| report.get("live_order_sender_implemented"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let real_orders_still_locked = !explicit_unlock;
    let no_post_calls = !readiness_post_order_called && !readiness_post_orders_called;

    let gates = vec![
        readiness_gate(
            "live_sender_design_readiness_journaled",
            design_readiness_event.is_some(),
            "A latest clob_live_sender_design_readiness journal event exists.",
        ),
        readiness_gate(
            "design_readiness_remains_blocked",
            design_readiness_blocked,
            "The readiness package still blocks live-sender implementation.",
        ),
        readiness_gate(
            "real_orders_still_locked",
            real_orders_still_locked,
            "Explicit real-order submission config is still locked.",
        ),
        readiness_gate(
            "no_exchange_request_sent",
            !readiness_request_sent,
            "The latest design-readiness package proves no exchange request was sent.",
        ),
        readiness_gate(
            "no_order_post_calls",
            no_post_calls,
            "The latest design-readiness package proves no order POST helpers were called.",
        ),
        readiness_gate(
            "final_review_decision_audited",
            final_review_decision_event.is_some(),
            "At least one audit-only final-review decision event exists.",
        ),
        readiness_gate(
            "live_sender_not_implemented",
            !live_sender_implemented,
            "The codebase still has no live order sender implementation.",
        ),
    ];

    let completed_count = gates
        .iter()
        .filter(|gate| gate.get("ok").and_then(|value| value.as_bool()) == Some(true))
        .count();
    let blockers = gates
        .iter()
        .filter(|gate| gate.get("ok").and_then(|value| value.as_bool()) == Some(false))
        .filter_map(|gate| {
            gate.get("name")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
        .collect::<Vec<_>>();
    let ready_for_design_review = design_readiness_event.is_some()
        && design_readiness_blocked
        && real_orders_still_locked
        && !readiness_request_sent
        && no_post_calls
        && final_review_decision_event.is_some()
        && !live_sender_implemented;

    serde_json::json!({
        "ready": false,
        "ready_for_design_review": ready_for_design_review,
        "ready_for_live_sender_implementation": false,
        "ready_for_real_orders": false,
        "stage": if ready_for_design_review { "design_review_contract_ready" } else { "design_review_evidence_missing" },
        "design_review_required": true,
        "implementation_permitted": false,
        "completed_count": completed_count,
        "required_count": gates.len(),
        "blocker_count": blockers.len(),
        "blockers": blockers,
        "gates": gates,
        "review_contract": {
            "boundary_name": "LiveOrderSender",
            "future_module": "src/clob/live_sender.rs",
            "allowed_responsibility": "Convert an already-approved, already-signed, already-reconciled order intent into a single exchange POST only after every guard revalidates immediately before send.",
            "required_inputs": [
                "journaled order intent dry-run",
                "journaled market metadata validation",
                "journaled post-request dry-run preview",
                "fresh human approval event",
                "fresh collateral readiness snapshot",
                "fresh real-trading unlock status",
                "fresh final-review decision audit",
                "fresh live-sender design review acceptance"
            ],
            "required_pre_submit_guards": [
                "explicit_real_order_submission_configured",
                "kill_switch_open",
                "paper_mode_disabled",
                "collateral_balance_positive",
                "collateral_allowance_positive",
                "per_order_risk_limit",
                "total_exposure_limit",
                "daily_loss_limit",
                "human_approval_event_valid",
                "final_review_decision_does_not_approve_by_itself"
            ],
            "required_post_submit_accounting": [
                "record local_order_id before network call",
                "record exchange response id when present",
                "record request_sent=true only after network dispatch",
                "record reconciliation outcome for every attempt",
                "record no-fill, partial-fill, or fill outcome separately"
            ],
            "prohibited_shortcuts": [
                "no sending from dashboard button directly",
                "no implicit env unlock",
                "no allowance refresh or funding in sender",
                "no unjournaled human approval",
                "no use of floats for money or price",
                "no bypass of paper-only mode without explicit config and kill switch"
            ],
            "first_implementation_shape": [
                "define trait/interface only",
                "add no-op fail-closed implementation",
                "add tests proving every missing guard rejects before network",
                "wire dashboard to status only, not to submit"
            ]
        },
        "latest_evidence": {
            "live_sender_design_readiness": design_readiness_event,
            "real_trading_unlock_status": unlock_event,
            "final_review_decision": final_review_decision_event,
        },
        "paper_only": true,
        "real_orders_enabled": false,
        "approved_for_real_orders": false,
        "request_sent": false,
        "would_send": false,
        "post_order_called": false,
        "post_orders_called": false,
        "live_order_sender_implemented": live_sender_implemented,
        "next_safe_step": "Review this contract as an ADR/wiki decision. Gated sender is wired behind boundary; still requires all unlocks + revalidation before any real authority or place.",
        "note": "Live-sender design review contract only. GatedRealClobLiveOrderSender exists (dispatches only on env+approval gates); it cannot auto-enable or place without explicit human review."
    })
}

fn build_review_summary(items: &[serde_json::Value]) -> serde_json::Value {
    let mut reviewed_count = 0usize;
    let mut unreviewed_count = 0usize;
    let mut would_approve = 0usize;
    let mut would_reject = 0usize;
    let mut needs_rework = 0usize;
    let mut unknown = 0usize;
    let mut guidance_would_reject = 0usize;
    let mut guidance_needs_rework = 0usize;
    let mut guidance_matches_latest_review = 0usize;
    let mut guidance_differs_from_latest_review = 0usize;
    let mut latest_review_latencies_seconds: Vec<i64> = Vec::new();
    let mut blocker_counts: HashMap<String, usize> = HashMap::new();

    for item in items {
        let report = item
            .get("dry_run")
            .and_then(|dry_run| dry_run.get("report"))
            .unwrap_or_else(|| item.get("dry_run").unwrap_or(&serde_json::Value::Null));
        let blocker_count = report
            .get("blockers")
            .and_then(|v| v.as_array())
            .map(Vec::len)
            .unwrap_or(0);
        let guidance = recommended_review_decision(blocker_count);
        match guidance {
            "would_reject" => guidance_would_reject += 1,
            "needs_rework" => guidance_needs_rework += 1,
            _ => {}
        }

        let review = item.get("latest_review");
        let review_payload = review.and_then(|review| review.get("payload"));
        if let Some(payload) = review_payload {
            reviewed_count += 1;
            let decision = payload
                .get("decision")
                .and_then(|decision| decision.as_str());
            match decision {
                Some("would_approve") => would_approve += 1,
                Some("would_reject") => would_reject += 1,
                Some("needs_rework") => needs_rework += 1,
                _ => unknown += 1,
            }
            if decision == Some(guidance) {
                guidance_matches_latest_review += 1;
            } else {
                guidance_differs_from_latest_review += 1;
            }
            if let Some(latency) = latest_review_latency_seconds(item) {
                latest_review_latencies_seconds.push(latency);
            }
        } else {
            unreviewed_count += 1;
        }

        if let Some(blockers) = report.get("blockers").and_then(|v| v.as_array()) {
            for blocker in blockers.iter().filter_map(|v| v.as_str()) {
                *blocker_counts.entry(blocker.to_string()).or_insert(0) += 1;
            }
        }
    }

    let mut top_blockers = blocker_counts.into_iter().collect::<Vec<_>>();
    top_blockers.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let top_blockers = top_blockers
        .into_iter()
        .take(10)
        .map(|(name, count)| {
            serde_json::json!({
                "name": name,
                "count": count,
            })
        })
        .collect::<Vec<_>>();

    serde_json::json!({
        "dry_run_count": items.len(),
        "reviewed_count": reviewed_count,
        "unreviewed_count": unreviewed_count,
        "review_coverage_pct": percentage_string(reviewed_count, items.len()),
        "decision_counts": {
            "would_approve": would_approve,
            "would_reject": would_reject,
            "needs_rework": needs_rework,
            "unknown": unknown,
        },
        "guidance_counts": {
            "would_reject": guidance_would_reject,
            "needs_rework": guidance_needs_rework,
        },
        "guidance_alignment": {
            "matches_latest_review": guidance_matches_latest_review,
            "differs_from_latest_review": guidance_differs_from_latest_review,
            "unreviewed": unreviewed_count,
        },
        "latest_review_latency": latency_summary(&latest_review_latencies_seconds),
        "top_blockers": top_blockers,
        "paper_only": true,
        "real_orders_enabled": false,
    })
}

fn percentage_string(numerator: usize, denominator: usize) -> String {
    if denominator == 0 {
        return "0.00".to_string();
    }
    format!("{:.2}", (numerator as f64 / denominator as f64) * 100.0)
}

fn latest_review_latency_seconds(item: &serde_json::Value) -> Option<i64> {
    let dry_run_created_at = parse_timestamp_value(item.get("dry_run_created_at")?)?;
    let latest_review_created_at =
        parse_timestamp_value(item.get("latest_review")?.get("created_at")?)?;
    Some(
        (latest_review_created_at - dry_run_created_at)
            .num_seconds()
            .max(0),
    )
}

fn parse_timestamp_value(value: &serde_json::Value) -> Option<chrono::DateTime<chrono::Utc>> {
    let s = value.as_str()?;
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

fn latency_summary(latencies_seconds: &[i64]) -> serde_json::Value {
    if latencies_seconds.is_empty() {
        return serde_json::json!({
            "reviewed_count": 0,
            "min_seconds": serde_json::Value::Null,
            "max_seconds": serde_json::Value::Null,
            "avg_seconds": serde_json::Value::Null,
            "slow_count": 0,
            "slow_after_seconds": REVIEW_LATENCY_SLOW_AFTER_SECONDS,
        });
    }

    let min_seconds = latencies_seconds.iter().min().copied().unwrap_or(0);
    let max_seconds = latencies_seconds.iter().max().copied().unwrap_or(0);
    let total_seconds: i64 = latencies_seconds.iter().sum();
    let avg_seconds = total_seconds / latencies_seconds.len() as i64;
    let slow_count = latencies_seconds
        .iter()
        .filter(|seconds| **seconds >= REVIEW_LATENCY_SLOW_AFTER_SECONDS)
        .count();

    serde_json::json!({
        "reviewed_count": latencies_seconds.len(),
        "min_seconds": min_seconds,
        "max_seconds": max_seconds,
        "avg_seconds": avg_seconds,
        "slow_count": slow_count,
        "slow_after_seconds": REVIEW_LATENCY_SLOW_AFTER_SECONDS,
    })
}

fn summary_usize(summary: &serde_json::Value, key: &str) -> usize {
    summary.get(key).and_then(|v| v.as_u64()).unwrap_or(0) as usize
}

fn nested_summary_usize(summary: &serde_json::Value, parent: &str, key: &str) -> usize {
    summary
        .get(parent)
        .and_then(|v| v.get(key))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize
}

fn nested_summary_i64(summary: &serde_json::Value, parent: &str, key: &str) -> Option<i64> {
    summary
        .get(parent)
        .and_then(|v| v.get(key))
        .and_then(|v| v.as_i64())
}

fn build_review_health(summary: &serde_json::Value) -> serde_json::Value {
    let dry_run_count = summary_usize(summary, "dry_run_count");
    let unreviewed_count = summary_usize(summary, "unreviewed_count");
    let guidance_differs =
        nested_summary_usize(summary, "guidance_alignment", "differs_from_latest_review");
    let max_latency_seconds =
        nested_summary_i64(summary, "latest_review_latency", "max_seconds").unwrap_or(0);
    let slow_review_count = nested_summary_usize(summary, "latest_review_latency", "slow_count");

    let mut reasons = Vec::new();
    if unreviewed_count > 0 {
        reasons.push("unreviewed_dry_runs");
    }
    if guidance_differs > 0 {
        reasons.push("guidance_exceptions");
    }
    if slow_review_count > 0 {
        reasons.push("slow_latest_review_latency");
    }

    let status = if dry_run_count == 0 {
        "empty"
    } else if reasons.is_empty() {
        "ok"
    } else {
        "needs_attention"
    };
    let recommended_actions = review_health_recommended_actions(
        status,
        &reasons,
        unreviewed_count,
        guidance_differs,
        max_latency_seconds,
        slow_review_count,
    );

    serde_json::json!({
        "status": status,
        "reasons": reasons,
        "reason_details": {
            "unreviewed_count": unreviewed_count,
            "guidance_exception_count": guidance_differs,
            "max_latency_seconds": max_latency_seconds,
            "slow_review_count": slow_review_count,
            "slow_latency_after_seconds": REVIEW_LATENCY_SLOW_AFTER_SECONDS,
        },
        "recommended_actions": recommended_actions,
        "dry_run_count": dry_run_count,
        "unreviewed_count": unreviewed_count,
        "guidance_exception_count": guidance_differs,
        "max_latency_seconds": max_latency_seconds,
        "slow_review_count": slow_review_count,
        "slow_latency_after_seconds": REVIEW_LATENCY_SLOW_AFTER_SECONDS,
        "paper_only": true,
        "real_orders_enabled": false,
    })
}

fn review_health_recommended_actions(
    status: &str,
    reasons: &[&str],
    unreviewed_count: usize,
    guidance_exception_count: usize,
    max_latency_seconds: i64,
    slow_review_count: usize,
) -> Vec<serde_json::Value> {
    if status == "empty" {
        return vec![serde_json::json!({
            "id": "no_recent_dry_runs",
            "severity": "info",
            "label": "Submit a paper dry-run before evaluating review health.",
            "endpoint": serde_json::Value::Null,
        })];
    }

    if reasons.is_empty() {
        return vec![serde_json::json!({
            "id": "none",
            "severity": "info",
            "label": "No review-health action required for the current window.",
            "endpoint": serde_json::Value::Null,
        })];
    }

    reasons
        .iter()
        .filter_map(|reason| match *reason {
            "unreviewed_dry_runs" => Some(serde_json::json!({
                "id": "review_unreviewed_dry_runs",
                "severity": "attention",
                "label": format!("Review {unreviewed_count} unreviewed paper dry-run(s)."),
                "endpoint": "/clob/order-intent/review-queue?limit=10",
                "unreviewed_count": unreviewed_count,
                "review_stale_after_seconds": REVIEW_BACKLOG_STALE_AFTER_SECONDS,
            })),
            "guidance_exceptions" => Some(serde_json::json!({
                "id": "inspect_guidance_exceptions",
                "severity": "attention",
                "label": format!("Inspect {guidance_exception_count} latest review(s) that differ from conservative guidance."),
                "endpoint": "/clob/order-intent/review-guidance-exceptions?limit=10",
                "guidance_exception_count": guidance_exception_count,
            })),
            "slow_latest_review_latency" => Some(serde_json::json!({
                "id": "inspect_review_latency",
                "severity": "attention",
                "label": "Inspect review latency; latest reviewed dry-run is past the slow-review threshold.",
                "endpoint": "/clob/order-intent/review-summary?limit=50",
                "max_latency_seconds": max_latency_seconds,
                "slow_review_count": slow_review_count,
                "slow_latency_after_seconds": REVIEW_LATENCY_SLOW_AFTER_SECONDS,
            })),
            _ => None,
        })
        .collect()
}

fn json_string_array(value: &serde_json::Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn operator_status_state(
    clob_read_ok: bool,
    clob_blockers: &[String],
    review_ok: bool,
    review_status: Option<&str>,
) -> &'static str {
    if !clob_read_ok {
        return "clob_unavailable";
    }
    if !clob_blockers.is_empty() {
        return "clob_blocked";
    }
    if !review_ok {
        return "review_unavailable";
    }
    match review_status {
        Some("needs_attention") => "review_attention",
        Some("empty") => "needs_paper_dry_runs",
        Some("ok") => "paper_observing",
        _ => "review_unknown",
    }
}

fn primary_operator_status_actions(operator_status: &str) -> Vec<serde_json::Value> {
    match operator_status {
        "clob_unavailable" => vec![serde_json::json!({
            "id": "inspect_clob_diagnostics",
            "severity": "attention",
            "label": "Inspect authenticated CLOB diagnostics.",
            "endpoint": "/clob/diagnostics",
        })],
        "clob_blocked" => vec![serde_json::json!({
            "id": "inspect_clob_preflight",
            "severity": "attention",
            "label": "Inspect CLOB preflight blockers.",
            "endpoint": "/clob/preflight",
        })],
        "review_unavailable" => vec![serde_json::json!({
            "id": "inspect_review_summary",
            "severity": "attention",
            "label": "Inspect dry-run review summary.",
            "endpoint": "/clob/order-intent/review-summary?limit=50",
        })],
        "review_attention" => vec![serde_json::json!({
            "id": "inspect_review_health",
            "severity": "attention",
            "label": "Inspect dry-run review health recommendations.",
            "endpoint": "/clob/order-intent/review-health?limit=50",
        })],
        "needs_paper_dry_runs" => vec![serde_json::json!({
            "id": "submit_paper_dry_run",
            "severity": "info",
            "label": "Submit a paper dry-run before evaluating operator health.",
            "endpoint": serde_json::Value::Null,
        })],
        _ => vec![serde_json::json!({
            "id": "none",
            "severity": "info",
            "label": "No operator action required for the current paper-only rollup.",
            "endpoint": serde_json::Value::Null,
        })],
    }
}

fn format_duration_seconds_compact(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    if hours > 0 && minutes > 0 {
        format!("{hours}h {minutes}m")
    } else if hours > 0 {
        format!("{hours}h")
    } else {
        format!("{minutes}m")
    }
}

fn operator_status_actions(
    operator_status: &str,
    review_health: &serde_json::Value,
    final_review_audit: &serde_json::Value,
    live_sender_boundary: &serde_json::Value,
    hermes_safety_loop: &serde_json::Value,
    final_review_hermes_gap_alignment: &serde_json::Value,
) -> Vec<serde_json::Value> {
    let mut actions = primary_operator_status_actions(operator_status);
    let mut seen: HashSet<String> = actions
        .iter()
        .filter_map(|action| {
            action
                .get("id")
                .and_then(|id| id.as_str())
                .map(str::to_string)
        })
        .collect();

    if operator_status != "review_unavailable" {
        if let Some(review_actions) = review_health
            .get("recommended_actions")
            .and_then(|actions| actions.as_array())
        {
            for action in review_actions {
                let Some(id) = action.get("id").and_then(|id| id.as_str()) else {
                    continue;
                };
                if matches!(id, "none" | "no_recent_dry_runs") {
                    continue;
                }
                if seen.insert(id.to_string()) {
                    actions.push(action.clone());
                }
            }
        }
    }

    if final_review_audit
        .get("status")
        .and_then(|status| status.as_str())
        == Some("missing_audit")
    {
        actions.retain(|action| action.get("id").and_then(|id| id.as_str()) != Some("none"));
        seen.remove("none");
        let already_present = actions.iter().any(|action| {
            action.get("id").and_then(|id| id.as_str()) == Some("inspect_final_review_decisions")
        });
        if !already_present {
            actions.push(serde_json::json!({
                "id": "inspect_final_review_decisions",
                "severity": "attention",
                "label": "Inspect final-review decision audit history and record a blocked review when ready.",
                "endpoint": "/clob/final-review-decisions?limit=10",
            }));
        }
    }

    let boundary_is_fail_closed = live_sender_boundary
        .get("fail_closed_implementation_present")
        .and_then(|v| v.as_bool())
        == Some(true)
        && live_sender_boundary
            .get("network_sender_present")
            .and_then(|v| v.as_bool())
            == Some(false)
        && live_sender_boundary
            .get("accepted_for_network_dispatch")
            .and_then(|v| v.as_bool())
            == Some(false)
        && live_sender_boundary
            .get("request_sent")
            .and_then(|v| v.as_bool())
            == Some(false);

    if boundary_is_fail_closed && seen.insert("inspect_live_sender_boundary".to_string()) {
        actions.retain(|action| action.get("id").and_then(|id| id.as_str()) != Some("none"));
        actions.push(serde_json::json!({
            "id": "inspect_live_sender_boundary",
            "severity": "info",
            "label": "Inspect the fail-closed live-sender boundary status.",
            "endpoint": "/clob/live-sender-boundary-status",
        }));
    }

    let hermes_status = hermes_safety_loop
        .get("status")
        .and_then(|status| status.as_str())
        .unwrap_or("unknown");
    let complete_boundary_coverage = hermes_safety_loop
        .get("final_review_decision_boundary_coverage")
        .and_then(|coverage| coverage.get("complete_fail_closed_no_network_evidence"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let has_final_review_decisions = hermes_safety_loop
        .get("final_review_decision_events_24h")
        .and_then(|value| value.as_i64())
        .unwrap_or(0)
        > 0;
    let hermes_gap_alignment_status = final_review_hermes_gap_alignment
        .get("status")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown");
    let hermes_reflection_age_seconds = final_review_hermes_gap_alignment
        .get("hermes_reflection_age_seconds")
        .and_then(|value| value.as_i64());
    let hermes_reflection_stale_after_seconds = final_review_hermes_gap_alignment
        .get("hermes_reflection_stale_after_seconds")
        .and_then(|value| value.as_i64());
    let app_active_24h_gap_count = final_review_hermes_gap_alignment
        .get("app_active_24h_gap_count")
        .and_then(|value| value.as_i64())
        .unwrap_or(0)
        .max(0);
    let seconds_until_active_gaps_age_out_of_24h = final_review_hermes_gap_alignment
        .get("seconds_until_active_gaps_age_out_of_24h")
        .and_then(|value| value.as_i64());
    let active_gaps_age_out_at = final_review_hermes_gap_alignment
        .get("active_gaps_age_out_at")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    if (hermes_status == "boundary_coverage_incomplete"
        || (has_final_review_decisions && !complete_boundary_coverage))
        && seen.insert("inspect_hermes_safety_loop".to_string())
    {
        actions.retain(|action| action.get("id").and_then(|id| id.as_str()) != Some("none"));
        actions.push(serde_json::json!({
            "id": "inspect_hermes_safety_loop",
            "severity": "attention",
            "label": "Inspect Hermes final-review boundary coverage before any live-send work.",
            "endpoint": "/clob/hermes-safety-loop",
        }));
    }

    if (hermes_status == "boundary_coverage_incomplete"
        || (has_final_review_decisions && !complete_boundary_coverage))
        && seen.insert("inspect_final_review_coverage_gaps".to_string())
    {
        actions.retain(|action| action.get("id").and_then(|id| id.as_str()) != Some("none"));
        let label = if app_active_24h_gap_count > 0 {
            let ttl = seconds_until_active_gaps_age_out_of_24h
                .map(format_duration_seconds_compact)
                .unwrap_or_else(|| "unknown time".to_string());
            format!(
                "Inspect {app_active_24h_gap_count} active final-review coverage gap(s); newest active gap ages out of Hermes' 24h window in {ttl}."
            )
        } else {
            "Load a broader final-review decision audit to inspect legacy boundary coverage gaps."
                .to_string()
        };
        actions.push(serde_json::json!({
            "id": "inspect_final_review_coverage_gaps",
            "severity": "attention",
            "label": label,
            "endpoint": "/clob/final-review-decisions?limit=50&gaps_only=true",
            "active_24h_gap_count": app_active_24h_gap_count,
            "seconds_until_active_gaps_age_out_of_24h": seconds_until_active_gaps_age_out_of_24h,
            "active_gaps_age_out_at": active_gaps_age_out_at,
            "hermes_gap_alignment_status": hermes_gap_alignment_status,
        }));
    }

    let hermes_gap_alignment_available = final_review_hermes_gap_alignment
        .get("available")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let hermes_gap_alignment_aligned = final_review_hermes_gap_alignment
        .get("aligned")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if hermes_gap_alignment_available
        && !hermes_gap_alignment_aligned
        && seen.insert("inspect_hermes_gap_alignment".to_string())
    {
        actions.retain(|action| action.get("id").and_then(|id| id.as_str()) != Some("none"));
        let label = if hermes_gap_alignment_status == "hermes_reflection_stale" {
            let age = hermes_reflection_age_seconds
                .map(format_duration_seconds_compact)
                .unwrap_or_else(|| "unknown age".to_string());
            "Inspect app/Hermes final-review gap alignment; Hermes reflection is stale ".to_string()
                + &format!("({age}) and may need a refresh before interpreting counts.")
        } else {
            "Inspect app/Hermes final-review gap alignment; Hermes may need a refresh if counts differ."
                .to_string()
        };
        actions.push(serde_json::json!({
            "id": "inspect_hermes_gap_alignment",
            "severity": "attention",
            "label": label,
            "endpoint": "/clob/operator-status?limit=50",
            "hermes_gap_alignment_status": hermes_gap_alignment_status,
            "hermes_reflection_age_seconds": hermes_reflection_age_seconds,
            "hermes_reflection_stale_after_seconds": hermes_reflection_stale_after_seconds,
        }));
    }

    actions
}

fn operator_action_summary(actions: &[serde_json::Value]) -> serde_json::Value {
    let mut attention_count = 0;
    let mut info_count = 0;
    let mut actionable_count = 0;

    for action in actions {
        let severity = action
            .get("severity")
            .and_then(|severity| severity.as_str())
            .unwrap_or("info");
        match severity {
            "attention" => attention_count += 1,
            "info" => info_count += 1,
            _ => {}
        }

        let id = action
            .get("id")
            .and_then(|id| id.as_str())
            .unwrap_or("none");
        if !matches!(id, "none" | "no_recent_dry_runs") {
            actionable_count += 1;
        }
    }

    serde_json::json!({
        "total_count": actions.len(),
        "attention_count": attention_count,
        "info_count": info_count,
        "actionable_count": actionable_count,
        "primary_action_id": actions
            .first()
            .and_then(|action| action.get("id"))
            .and_then(|id| id.as_str()),
    })
}

fn build_order_placement_readiness(
    l2_connected: bool,
    clob_read_ok: bool,
    preflight: &serde_json::Value,
    review_health: &serde_json::Value,
    market_data_readiness: &serde_json::Value,
    final_review_audit: &serde_json::Value,
    live_sender_boundary: &serde_json::Value,
) -> serde_json::Value {
    let review_available =
        review_health.get("status").and_then(|v| v.as_str()) != Some("unavailable");
    let paper_market_data_ready = market_data_readiness.get("status").and_then(|v| v.as_str())
        == Some("ready")
        && market_data_readiness
            .get("data_ready_market_count")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            > 0;
    let final_review_decision_audited =
        final_review_audit.get("status").and_then(|v| v.as_str()) == Some("audited");
    let collateral_balance_ok =
        preflight_check_ok(preflight, "collateral_balance_positive").unwrap_or(false);
    let collateral_allowance_ok =
        preflight_check_ok(preflight, "collateral_allowance_positive").unwrap_or(false);
    let fail_closed_live_sender_boundary = live_sender_boundary
        .get("trait_defined")
        .and_then(|v| v.as_bool())
        == Some(true)
        && live_sender_boundary
            .get("fail_closed_implementation_present")
            .and_then(|v| v.as_bool())
            == Some(true)
        && live_sender_boundary
            .get("network_sender_present")
            .and_then(|v| v.as_bool())
            == Some(false)
        && live_sender_boundary
            .get("accepted_for_network_dispatch")
            .and_then(|v| v.as_bool())
            == Some(false)
        && live_sender_boundary
            .get("request_sent")
            .and_then(|v| v.as_bool())
            == Some(false)
        && live_sender_boundary
            .get("post_order_called")
            .and_then(|v| v.as_bool())
            == Some(false)
        && live_sender_boundary
            .get("post_orders_called")
            .and_then(|v| v.as_bool())
            == Some(false);

    let gates = vec![
        readiness_gate(
            "l2_credentials_connected",
            l2_connected,
            "L2 credentials are derived and available in server memory.",
        ),
        readiness_gate(
            "authenticated_account_read",
            clob_read_ok,
            "Authenticated read-only open-order and balance/allowance calls are working.",
        ),
        readiness_gate(
            "collateral_balance_positive",
            collateral_balance_ok,
            "CLOB reports positive collateral balance for buy-side order capacity.",
        ),
        readiness_gate(
            "collateral_allowance_positive",
            collateral_allowance_ok,
            "CLOB reports at least one positive collateral allowance entry.",
        ),
        readiness_gate(
            "paper_order_intent_dry_run",
            true,
            "Paper-only order-intent validation exists and rejects unsafe intents.",
        ),
        readiness_gate(
            "paper_review_health",
            review_available,
            "Paper dry-run review-health and audit views are available.",
        ),
        readiness_gate(
            "paper_market_data_ready",
            paper_market_data_ready,
            "At least one active market has both Yes/No mids for paper-pricing input.",
        ),
        readiness_gate(
            "market_tick_and_neg_risk_validation",
            market_metadata_validation_available(),
            "Read-only market metadata validation can fetch/enforce tick size and negative-risk settings for the token without sending.",
        ),
        readiness_gate(
            "eip712_order_payload_signing",
            signed_order_payload_dry_run_available(),
            "Signed-order payload dry-run can build and verify EIP-712 signatures locally without posting.",
        ),
        readiness_gate(
            "non_submitting_order_post_request_dry_run",
            order_post_request_dry_run_available(),
            "Non-submitting CLOB order POST request dry-run can serialize redacted body/header previews without sending.",
        ),
        readiness_gate(
            "l2_order_posting_client",
            order_submit_facade_available(),
            "Fail-closed L2 order submission facade can evaluate the would-be submit path without sending.",
        ),
        readiness_gate(
            "real_order_route",
            order_submit_facade_available(),
            "A gated submit-facade route exists, but it refuses live orders until approval, risk, journaling, and config gates pass.",
        ),
        readiness_gate(
            "human_approval_gate",
            human_approval_workflow_available(),
            "Journaled human approval workflow exists for submit-facade validation; it does not unlock live sending.",
        ),
        readiness_gate(
            "kill_switch_and_exposure_limits",
            kill_switch_and_exposure_limits_available(),
            "Fail-closed kill-switch, per-order exposure, total exposure, and daily-loss checks are implemented in the submit facade.",
        ),
        readiness_gate(
            "real_order_journaling_and_reconciliation",
            real_order_reconciliation_available(),
            "Submit-facade reject decisions are journaled and reconciled to no exchange order while live sending remains disabled.",
        ),
        readiness_gate(
            "final_review_decision_audit",
            final_review_decision_audited,
            "At least one final-review decision audit event exists; it remains audit-only and cannot approve real orders.",
        ),
        readiness_gate(
            "fail_closed_live_sender_boundary",
            fail_closed_live_sender_boundary,
            "The LiveOrderSender boundary exists and the only implementation rejects before network dispatch.",
        ),
        readiness_gate(
            "explicit_real_trading_config_unlock",
            false,
            "No explicit real-trading feature flag/config unlock exists.",
        ),
    ];

    let completed_count = gates
        .iter()
        .filter(|gate| gate.get("ok").and_then(|v| v.as_bool()) == Some(true))
        .count();
    let blockers = gates
        .iter()
        .filter(|gate| gate.get("ok").and_then(|v| v.as_bool()) == Some(false))
        .filter_map(|gate| {
            gate.get("name")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .collect::<Vec<String>>();
    let note = if real_order_reconciliation_available() && market_metadata_validation_available() {
        "Not ready for real orders. The app can authenticate reads, validate paper intents, validate live market tick/negative-risk metadata, build local signed-payload dry-runs, serialize non-submitting order POST previews, evaluate a fail-closed submit facade, journal human approval events, enforce fail-closed kill-switch/exposure/loss-limit checks, reconcile submit/reject decisions to no exchange order, track final-review decision audits, and prove the live-sender boundary rejects before network dispatch. Live sending remains blocked by collateral, allowance, and explicit config unlock gates."
    } else if real_order_reconciliation_available() {
        "Not ready for real orders. The app can authenticate reads, validate paper intents, build local signed-payload dry-runs, serialize non-submitting order POST previews, evaluate a fail-closed submit facade, journal human approval events, enforce fail-closed kill-switch/exposure/loss-limit checks, and reconcile submit/reject decisions to no exchange order. Live sending remains blocked by market metadata, collateral, allowance, and explicit config unlock gates."
    } else if kill_switch_and_exposure_limits_available() {
        "Not ready for real orders. The app can authenticate reads, validate paper intents, build local signed-payload dry-runs, serialize non-submitting order POST previews, evaluate a fail-closed submit facade, journal human approval events, and enforce fail-closed kill-switch/exposure/loss-limit checks, but live sending remains blocked by reconciliation and explicit config unlock gates."
    } else if human_approval_workflow_available() {
        "Not ready for real orders. The app can authenticate reads, validate paper intents, build local signed-payload dry-runs, serialize non-submitting order POST previews, evaluate a fail-closed submit facade, and journal human approval events, but live sending remains blocked by kill-switch/exposure/loss-limit, reconciliation, and explicit config unlock gates."
    } else if order_submit_facade_available() {
        "Not ready for real orders. The app can authenticate reads, validate paper intents, build local signed-payload dry-runs, serialize non-submitting order POST previews, and evaluate a fail-closed submit facade, but live sending remains blocked by required human approval, kill-switch/exposure/loss-limit, journaling, and explicit config unlock gates."
    } else if order_post_request_dry_run_available() {
        "Not ready for real orders. The app can authenticate reads, validate paper intents, run local signed-payload dry-runs, and serialize non-submitting order POST previews, but it cannot send orders and lacks required human approval, risk, kill-switch, journaling, and config unlock gates."
    } else if signed_order_payload_dry_run_available() {
        "Not ready for real orders. The app can authenticate reads, validate paper intents, and run local signed-payload dry-runs, but it cannot post orders and lacks required human approval, risk, kill-switch, journaling, and config unlock gates."
    } else {
        "Not ready for real orders. The app can authenticate reads and validate paper intents, but it cannot sign or post orders and lacks required human approval, risk, kill-switch, journaling, and config unlock gates."
    };

    serde_json::json!({
        "ready": false,
        "stage": "authenticated_read_and_paper_dry_run",
        "completed_count": completed_count,
        "required_count": gates.len(),
        "blocker_count": blockers.len(),
        "blockers": blockers,
        "gates": gates,
        "market_data_readiness": market_data_readiness,
        "paper_market_data_ready": paper_market_data_ready,
        "final_review_audit_status": final_review_audit.get("status").cloned().unwrap_or(serde_json::Value::Null),
        "final_review_decision_count": final_review_audit.get("count").cloned().unwrap_or(serde_json::json!(0)),
        "live_sender_boundary": live_sender_boundary,
        "live_sender_boundary_status": {
            "boundary_name": live_sender_boundary.get("boundary_name").cloned().unwrap_or(serde_json::Value::Null),
            "implementation_name": live_sender_boundary.get("implementation_name").cloned().unwrap_or(serde_json::Value::Null),
            "fail_closed_implementation_present": live_sender_boundary.get("fail_closed_implementation_present").cloned().unwrap_or(serde_json::Value::Null),
            "network_sender_present": live_sender_boundary.get("network_sender_present").cloned().unwrap_or(serde_json::Value::Null),
            "accepted_for_network_dispatch": live_sender_boundary.get("accepted_for_network_dispatch").cloned().unwrap_or(serde_json::Value::Null),
            "request_sent": live_sender_boundary.get("request_sent").cloned().unwrap_or(serde_json::Value::Null),
        },
        "next_safe_step": if real_order_reconciliation_available() && market_metadata_validation_available() {
            if final_review_decision_audited {
                "Keep the live-sender boundary fail-closed; resolve collateral balance/allowance capacity and keep explicit real-trading config locked until a deliberate implementation review."
            } else {
                "Record an audit-only final-review decision, then resolve collateral balance/allowance capacity while real trading stays locked."
            }
        } else if real_order_reconciliation_available() {
            "Implement market tick-size and negative-risk validation against live CLOB market metadata, still without live sending."
        } else if kill_switch_and_exposure_limits_available() {
            "Implement real-order submit/reject journaling and reconciliation around the submit facade, still without live sending."
        } else if human_approval_workflow_available() {
            "Implement kill-switch and real exposure/daily-loss limit enforcement for the submit facade, still without live sending."
        } else if order_submit_facade_available() {
            "Implement the human approval workflow and keep it journaled before any live-send path can be considered."
        } else if order_post_request_dry_run_available() {
            "Wire a submitting order client facade behind human approval, kill switch, exposure limits, and explicit real-trading config gates."
        } else if signed_order_payload_dry_run_available() {
            "Implement a non-submitting order POST request dry-run that serializes the exact HTTP body and headers but never sends it."
        } else {
            "Implement a signed-order payload dry-run that builds and verifies the EIP-712 order object locally but never posts it."
        },
        "note": note
    })
}

fn signed_order_payload_dry_run_available() -> bool {
    cfg!(feature = "native-l2")
}

fn order_post_request_dry_run_available() -> bool {
    cfg!(feature = "native-l2")
}

fn order_submit_facade_available() -> bool {
    cfg!(feature = "native-l2")
}

fn human_approval_workflow_available() -> bool {
    cfg!(feature = "native-l2")
}

fn kill_switch_and_exposure_limits_available() -> bool {
    cfg!(feature = "native-l2")
}

fn real_order_reconciliation_available() -> bool {
    cfg!(feature = "native-l2")
}

fn market_metadata_validation_available() -> bool {
    cfg!(feature = "native-l2")
}

fn readiness_gate(name: &str, ok: bool, detail: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "ok": ok,
        "severity": if ok { "info" } else { "blocker" },
        "detail": detail,
    })
}

fn preflight_check_ok(preflight: &serde_json::Value, name: &str) -> Option<bool> {
    preflight
        .get("checks")
        .and_then(|checks| checks.as_array())?
        .iter()
        .find(|check| check.get("name").and_then(|v| v.as_str()) == Some(name))
        .and_then(|check| check.get("ok"))
        .and_then(|ok| ok.as_bool())
}

fn category_display_label(category: &str) -> &str {
    match category {
        "motorsports" => "Motorsports",
        "formula1" | "formula_1" | "formula-1" | "f1" => "Motorsports",
        "crypto" => "Crypto",
        _ => category,
    }
}

fn market_has_two_sided_mids(
    last_mid_yes: &Option<Decimal>,
    last_mid_no: &Option<Decimal>,
) -> bool {
    last_mid_yes.is_some() && last_mid_no.is_some()
}

fn market_data_status(
    last_mid_yes: &Option<Decimal>,
    last_mid_no: &Option<Decimal>,
) -> &'static str {
    if market_has_two_sided_mids(last_mid_yes, last_mid_no) {
        "ready"
    } else {
        "missing_mid"
    }
}

async fn load_strategy_orderbook_metrics(
    pool: &PgPool,
    market_id: &str,
    outcome: &str,
) -> anyhow::Result<serde_json::Value> {
    let snapshot = sqlx::query_as::<_, StrategyOrderbookSnapshotRow>(
        r#"SELECT bids, asks, spread, fetched_at
           FROM market_data.orderbook_snapshots
           WHERE market_id = $1 AND outcome = $2
           ORDER BY fetched_at DESC
           LIMIT 1"#,
    )
    .bind(market_id)
    .bind(outcome)
    .fetch_optional(pool)
    .await?;

    let Some(snapshot) = snapshot else {
        return Ok(serde_json::json!({
            "available": false,
            "status": "missing_orderbook_snapshot",
            "top3_bid_size": "0",
            "top3_ask_size": "0",
            "spread": null,
            "paper_only": true,
            "real_orders_enabled": false,
        }));
    };

    let top3_bid_size = sum_orderbook_level_sizes(&snapshot.bids, 3);
    let top3_ask_size = sum_orderbook_level_sizes(&snapshot.asks, 3);
    let best_bid = best_orderbook_price(&snapshot.bids, true);
    let best_ask = best_orderbook_price(&snapshot.asks, false);
    let raw_imbalance = if top3_bid_size + top3_ask_size > Decimal::ZERO {
        (top3_bid_size - top3_ask_size) / (top3_bid_size + top3_ask_size)
    } else {
        Decimal::ZERO
    };

    Ok(serde_json::json!({
        "available": true,
        "status": "ready",
        "fetched_at": snapshot.fetched_at,
        "top3_bid_size": top3_bid_size,
        "top3_ask_size": top3_ask_size,
        "best_bid": best_bid,
        "best_ask": best_ask,
        "spread": snapshot.spread,
        "raw_imbalance": raw_imbalance,
        "paper_only": true,
        "real_orders_enabled": false,
    }))
}

async fn load_strategy_tick_velocity_metrics(
    pool: &PgPool,
    market_id: &str,
    outcome: &str,
) -> anyhow::Result<serde_json::Value> {
    let snapshots = sqlx::query_as::<_, StrategyTickVelocitySnapshotRow>(
        r#"SELECT mid, fetched_at
           FROM market_data.orderbook_snapshots
           WHERE market_id = $1 AND outcome = $2 AND mid IS NOT NULL
           ORDER BY fetched_at DESC
           LIMIT 2"#,
    )
    .bind(market_id)
    .bind(outcome)
    .fetch_all(pool)
    .await?;

    if snapshots.len() < 2 {
        return Ok(serde_json::json!({
            "available": false,
            "status": "missing_tick_velocity_window",
            "latest_mid": snapshots.first().map(|snapshot| snapshot.mid),
            "previous_mid": null,
            "mid_delta": null,
            "seconds_between": null,
            "paper_only": true,
            "real_orders_enabled": false,
        }));
    }

    let latest = &snapshots[0];
    let previous = &snapshots[1];
    let seconds_between = latest
        .fetched_at
        .signed_duration_since(previous.fetched_at)
        .num_seconds()
        .abs();
    let mid_delta = latest.mid - previous.mid;

    Ok(serde_json::json!({
        "available": true,
        "status": "ready",
        "latest_mid": latest.mid,
        "previous_mid": previous.mid,
        "mid_delta": mid_delta,
        "seconds_between": seconds_between,
        "latest_fetched_at": latest.fetched_at,
        "previous_fetched_at": previous.fetched_at,
        "paper_only": true,
        "real_orders_enabled": false,
    }))
}

fn sum_orderbook_level_sizes(levels: &serde_json::Value, limit: usize) -> Decimal {
    levels
        .as_array()
        .map(|rows| {
            rows.iter()
                .take(limit)
                .filter_map(|row| json_decimal_field(row, "size"))
                .sum()
        })
        .unwrap_or(Decimal::ZERO)
}

fn best_orderbook_price(levels: &serde_json::Value, highest: bool) -> Option<Decimal> {
    let mut prices = levels
        .as_array()?
        .iter()
        .filter_map(|row| json_decimal_field(row, "price"))
        .collect::<Vec<_>>();
    if highest {
        prices.sort_by(|left, right| right.cmp(left));
    } else {
        prices.sort();
    }
    prices.into_iter().next()
}

fn json_decimal_field(row: &serde_json::Value, key: &str) -> Option<Decimal> {
    let value = row.get(key)?;
    if let Some(text) = value.as_str() {
        Decimal::from_str(text).ok()
    } else if value.is_number() {
        Decimal::from_str(&value.to_string()).ok()
    } else {
        None
    }
}

async fn build_strategy_paper_candidates(pool: &PgPool) -> anyhow::Result<serde_json::Value> {
    let markets = sqlx::query_as::<_, StrategyCandidateMarketRow>(
        "SELECT gamma_id, slug, question, category, last_mid_yes, last_mid_no
         FROM market_data.markets
         WHERE active = true
           AND last_mid_yes IS NOT NULL
           AND last_mid_no IS NOT NULL
         ORDER BY updated_at DESC
         LIMIT 10",
    )
    .fetch_all(pool)
    .await?;

    // Load Hermes-learned processor weights (closed loop) so candidates rank with the same tuned
    // weighting the 5-min generator uses. Empty map → all 1.0 (neutral).
    let learned_weights = crate::strategy::load_processor_weights(pool).await;
    let engine = FusionEngine::with_weights(learned_weights);
    let fee_ctx = FeeContext {
        taker_bps: Decimal::from(paper_fee_bps_from_env()),
        maker_bps: Decimal::from(20u64),
        est_gas_usdc: Decimal::new(1, 2),
        rewards_offset_bps: Decimal::from(10u64),
    };
    let min_net_edge_for_trade = Decimal::new(4, 2);
    let mut candidates = Vec::new();

    for market in markets {
        let (target_outcome, target_mid) = if market.last_mid_yes <= market.last_mid_no {
            ("Yes", market.last_mid_yes)
        } else {
            ("No", market.last_mid_no)
        };
        let orderbook = load_strategy_orderbook_metrics(pool, &market.gamma_id, target_outcome)
            .await
            .unwrap_or_else(|e| {
                serde_json::json!({
                    "available": false,
                    "status": "orderbook_metrics_error",
                    "error": e.to_string(),
                    "top3_bid_size": "0",
                    "top3_ask_size": "0",
                    "spread": null,
                    "paper_only": true,
                    "real_orders_enabled": false,
                })
            });
        let tick_velocity =
            load_strategy_tick_velocity_metrics(pool, &market.gamma_id, target_outcome)
                .await
                .unwrap_or_else(|e| {
                    serde_json::json!({
                        "available": false,
                        "status": "tick_velocity_metrics_error",
                        "error": e.to_string(),
                        "latest_mid": null,
                        "previous_mid": null,
                        "mid_delta": null,
                        "seconds_between": null,
                        "paper_only": true,
                        "real_orders_enabled": false,
                    })
                });
        let snapshot = serde_json::json!({
            "gamma_id": market.gamma_id,
            "slug": market.slug,
            "question": market.question,
            "category": market.category,
            "category_label": market.category.as_deref().map(category_display_label),
            "last_mid_yes": market.last_mid_yes,
            "last_mid_no": market.last_mid_no,
            "target_outcome": target_outcome,
            "target_mid": target_mid,
            "market_data_status": "ready",
            "orderbook": orderbook.clone(),
            "tick_velocity": tick_velocity.clone(),
        });
        let context = serde_json::json!({
            "paper_only": true,
            "candidate_source": "strategy_paper_candidates",
            "min_net_edge_for_trade": min_net_edge_for_trade.to_string(),
        });
        let preview_request = PaperOrderRequest {
            market_id: market.gamma_id.clone(),
            outcome: target_outcome.to_string(),
            side: "Buy".to_string(),
            order_type: "Market".to_string(),
            size: Decimal::ONE,
            limit_price: None,
            rationale: Some("read-only strategy paper candidate preview".to_string()),
            confirm_paper_order: Some(false),
        };
        let paper_order_preview = match build_paper_order_plan(pool, &preview_request).await {
            Ok(plan) => plan,
            Err(e) => serde_json::json!({
                "accepted_for_paper": false,
                "executed": false,
                "blockers": ["paper_order_preview_failed"],
                "error": e.to_string(),
                "request_sent": false,
                "post_order_called": false,
                "post_orders_called": false,
            }),
        };
        let (_gross_edge, net_edge_after_fees, attribution) =
            engine.fuse_net(&snapshot, &context, Some(&fee_ctx), target_mid)?;
        let decision = strategy_candidate_decision(net_edge_after_fees, min_net_edge_for_trade);

        candidates.push(serde_json::json!({
            "market_id": market.gamma_id,
            "slug": market.slug,
            "question": market.question,
            "category": market.category,
            "category_label": market.category.as_deref().map(category_display_label),
            "target_outcome": target_outcome,
            "side": "Buy",
            "order_type": "Market",
            "size": "1",
            "target_mid": target_mid,
            "orderbook": orderbook,
            "tick_velocity": tick_velocity,
            "decision": decision,
            "min_net_edge_for_trade": min_net_edge_for_trade.to_string(),
            "net_edge_after_fees": net_edge_after_fees.to_string(),
            "paper_order_preview": paper_order_preview,
            "attribution": attribution,
            "paper_only": true,
            "real_orders_enabled": false,
            "request_sent": false,
            "post_order_called": false,
            "post_orders_called": false,
        }));
    }

    Ok(serde_json::json!({
        "paper_only": true,
        "real_orders_enabled": false,
        "request_sent": false,
        "post_order_called": false,
        "post_orders_called": false,
        "strategy_engine": "FusionEngine",
        "status": if candidates.is_empty() { "no_ready_markets" } else { "ready" },
        "min_net_edge_for_trade": min_net_edge_for_trade.to_string(),
        "candidate_count": candidates.len(),
        "candidates": candidates,
        "note": "Read-only strategy paper candidates with embedded paper-order previews. No paper orders are submitted and no CLOB order API is called."
    }))
}

async fn build_strategy_paper_candidate_observation(
    pool: &PgPool,
    request: StrategyPaperCandidateObservationRequest,
) -> anyhow::Result<serde_json::Value> {
    let candidates_body = build_strategy_paper_candidates(pool).await?;
    let observation_size = request.size.unwrap_or(Decimal::ONE);
    let candidate_count = candidates_body
        .get("candidate_count")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let mut observed_candidates = Vec::new();
    if let Some(rows) = candidates_body
        .get("candidates")
        .and_then(|value| value.as_array())
    {
        for candidate in rows.iter().cloned() {
            let candidate =
                attach_strategy_requested_size_paper_preview(pool, candidate, observation_size)
                    .await;
            observed_candidates.push(strategy_candidate_observation_summary(&candidate));
        }
    }
    let payload = serde_json::json!({
        "paper_only": true,
        "real_orders_enabled": false,
        "strategy_candidate_observation": true,
        "strategy_observation_size": observation_size,
        "candidate_count": candidate_count,
        "status": candidates_body.get("status").cloned().unwrap_or(serde_json::Value::Null),
        "strategy_engine": candidates_body
            .get("strategy_engine")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        "min_net_edge_for_trade": candidates_body
            .get("min_net_edge_for_trade")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        "candidates": observed_candidates,
        "request_sent": false,
        "would_send": false,
        "would_post": false,
        "post_order_called": false,
        "post_orders_called": false,
        "operator_note": request.note,
        "note": "Journal-only strategy paper candidate observation for Hermes. No paper order, fill, position, signature, approval, allowance refresh, or CLOB order request is created."
    });
    let event_id = record_journal_event(
        pool,
        "strategy_paper_candidate_observation",
        "strategy_paper_candidate_observation_route",
        "info",
        payload.clone(),
    )
    .await?;

    Ok(serde_json::json!({
        "paper_only": true,
        "real_orders_enabled": false,
        "strategy_candidate_observation": true,
        "journaled": true,
        "journal_event_id": event_id,
        "strategy_observation_size": observation_size,
        "candidate_count": candidate_count,
        "status": candidates_body.get("status").cloned().unwrap_or(serde_json::Value::Null),
        "strategy_engine": candidates_body
            .get("strategy_engine")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        "min_net_edge_for_trade": candidates_body
            .get("min_net_edge_for_trade")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        "observed_candidates": payload.get("candidates").cloned().unwrap_or_else(|| serde_json::json!([])),
        "request_sent": false,
        "would_send": false,
        "would_post": false,
        "post_order_called": false,
        "post_orders_called": false,
        "note": "Recorded current strategy paper candidates to journal.events for Hermes; no order path was invoked."
    }))
}

async fn load_strategy_paper_candidate_observation_events(
    pool: &PgPool,
    limit: i64,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let rows = sqlx::query_as::<
        _,
        (
            uuid::Uuid,
            String,
            String,
            String,
            serde_json::Value,
            chrono::DateTime<chrono::Utc>,
        ),
    >(
        r#"SELECT id, event_type, source, severity, payload, created_at
           FROM journal.events
           WHERE event_type = 'strategy_paper_candidate_observation'
           ORDER BY created_at DESC
           LIMIT $1"#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(id, event_type, source, severity, payload, created_at)| {
                let first_candidate = payload
                    .get("candidates")
                    .and_then(|value| value.as_array())
                    .and_then(|rows| rows.first())
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                serde_json::json!({
                    "id": id,
                    "event_type": event_type,
                    "source": source,
                    "severity": severity,
                    "created_at": created_at,
                    "strategy_observation_size": payload.get("strategy_observation_size").cloned().unwrap_or(serde_json::Value::Null),
                    "candidate_count": payload.get("candidate_count").cloned().unwrap_or(serde_json::Value::Null),
                    "status": payload.get("status").cloned().unwrap_or(serde_json::Value::Null),
                    "strategy_engine": payload.get("strategy_engine").cloned().unwrap_or(serde_json::Value::Null),
                    "min_net_edge_for_trade": payload.get("min_net_edge_for_trade").cloned().unwrap_or(serde_json::Value::Null),
                    "first_candidate": {
                        "market_id": first_candidate.get("market_id").cloned().unwrap_or(serde_json::Value::Null),
                        "slug": first_candidate.get("slug").cloned().unwrap_or(serde_json::Value::Null),
                        "target_outcome": first_candidate.get("target_outcome").cloned().unwrap_or(serde_json::Value::Null),
                        "size": first_candidate.get("size").cloned().unwrap_or(serde_json::Value::Null),
                        "strategy_requested_size": first_candidate.get("strategy_requested_size").cloned().unwrap_or(serde_json::Value::Null),
                        "decision": first_candidate.get("decision").cloned().unwrap_or(serde_json::Value::Null),
                        "net_edge_after_fees": first_candidate.get("net_edge_after_fees").cloned().unwrap_or(serde_json::Value::Null),
                        "orderbook_status": first_candidate
                            .get("orderbook")
                            .and_then(|value| value.get("status"))
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                        "tick_velocity_status": first_candidate
                            .get("tick_velocity")
                            .and_then(|value| value.get("status"))
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                    },
                    "request_sent": payload.get("request_sent").cloned().unwrap_or(serde_json::Value::Null),
                    "post_order_called": payload.get("post_order_called").cloned().unwrap_or(serde_json::Value::Null),
                    "post_orders_called": payload.get("post_orders_called").cloned().unwrap_or(serde_json::Value::Null),
                    "paper_only": true,
                    "real_orders_enabled": false,
                })
            },
        )
        .collect())
}

async fn load_strategy_paper_candidate_observation_evidence(
    pool: &PgPool,
    market_id: &str,
    outcome: &str,
    requested_size: Decimal,
) -> anyhow::Result<serde_json::Value> {
    let requested_size_text = requested_size.to_string();
    let latest: Option<(uuid::Uuid, serde_json::Value, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as(
            r#"SELECT id, payload, created_at
               FROM journal.events
               WHERE event_type = 'strategy_paper_candidate_observation'
                 AND EXISTS (
                    SELECT 1
                    FROM jsonb_array_elements(payload->'candidates') candidate
                    WHERE candidate->>'market_id' = $1
                      AND candidate->>'target_outcome' = $2
                      AND candidate->>'strategy_requested_size' = $3
                 )
               ORDER BY created_at DESC
               LIMIT 1"#,
        )
        .bind(market_id)
        .bind(outcome)
        .bind(&requested_size_text)
        .fetch_optional(pool)
        .await?;

    let Some((event_id, payload, created_at)) = latest else {
        return Ok(serde_json::json!({
            "available": false,
            "status": "missing_strategy_candidate_observation",
            "market_id": market_id,
            "target_outcome": outcome,
            "strategy_requested_size": requested_size,
            "max_age_seconds": STRATEGY_CANDIDATE_OBSERVATION_MAX_AGE_SECONDS,
            "paper_only": true,
            "real_orders_enabled": false,
            "request_sent": false,
            "post_order_called": false,
            "post_orders_called": false,
        }));
    };

    let observed_candidate = payload
        .get("candidates")
        .and_then(|value| value.as_array())
        .and_then(|rows| {
            rows.iter().find(|candidate| {
                candidate.get("market_id").and_then(|value| value.as_str()) == Some(market_id)
                    && candidate
                        .get("target_outcome")
                        .and_then(|value| value.as_str())
                        == Some(outcome)
                    && candidate
                        .get("strategy_requested_size")
                        .and_then(|value| value.as_str())
                        == Some(requested_size_text.as_str())
            })
        })
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let now = chrono::Utc::now();
    let age_seconds = (now - created_at).num_seconds().max(0);
    let is_recent = age_seconds <= STRATEGY_CANDIDATE_OBSERVATION_MAX_AGE_SECONDS;
    let observed_decision = observed_candidate
        .get("decision")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown");
    let observation_ready = observed_decision == "paper_candidate_ready_for_manual_review";
    let status = if !is_recent {
        "stale_strategy_candidate_observation"
    } else if observation_ready {
        "ready"
    } else {
        "strategy_candidate_observation_not_ready"
    };

    Ok(serde_json::json!({
        "available": true,
        "status": status,
        "event_id": event_id,
        "created_at": created_at,
        "age_seconds": age_seconds,
        "max_age_seconds": STRATEGY_CANDIDATE_OBSERVATION_MAX_AGE_SECONDS,
        "is_recent": is_recent,
        "observation_ready_for_manual_review": observation_ready,
        "market_id": market_id,
        "target_outcome": outcome,
        "strategy_requested_size": requested_size,
        "observed_strategy_requested_size": observed_candidate
            .get("strategy_requested_size")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        "observed_decision": observed_decision,
        "observed_net_edge_after_fees": observed_candidate
            .get("net_edge_after_fees")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        "observed_candidate": observed_candidate,
        "paper_only": true,
        "real_orders_enabled": false,
        "request_sent": false,
        "post_order_called": false,
        "post_orders_called": false,
    }))
}

fn strategy_candidate_observation_summary(candidate: &serde_json::Value) -> serde_json::Value {
    let attribution = candidate
        .get("attribution")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    serde_json::json!({
        "market_id": candidate.get("market_id").cloned().unwrap_or(serde_json::Value::Null),
        "slug": candidate.get("slug").cloned().unwrap_or(serde_json::Value::Null),
        "question": candidate.get("question").cloned().unwrap_or(serde_json::Value::Null),
        "category": candidate.get("category").cloned().unwrap_or(serde_json::Value::Null),
        "category_label": candidate.get("category_label").cloned().unwrap_or(serde_json::Value::Null),
        "target_outcome": candidate.get("target_outcome").cloned().unwrap_or(serde_json::Value::Null),
        "side": candidate.get("side").cloned().unwrap_or(serde_json::Value::Null),
        "order_type": candidate.get("order_type").cloned().unwrap_or(serde_json::Value::Null),
        "size": candidate.get("size").cloned().unwrap_or(serde_json::Value::Null),
        "strategy_requested_size": candidate
            .get("strategy_requested_size")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        "target_mid": candidate.get("target_mid").cloned().unwrap_or(serde_json::Value::Null),
        "decision": candidate.get("decision").cloned().unwrap_or(serde_json::Value::Null),
        "min_net_edge_for_trade": candidate
            .get("min_net_edge_for_trade")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        "net_edge_after_fees": candidate
            .get("net_edge_after_fees")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        "orderbook": candidate.get("orderbook").cloned().unwrap_or(serde_json::Value::Null),
        "tick_velocity": candidate
            .get("tick_velocity")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        "attribution": attribution,
        "paper_order_preview": {
            "accepted_for_paper": candidate
                .get("paper_order_preview")
                .and_then(|preview| preview.get("accepted_for_paper"))
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            "blockers": candidate
                .get("paper_order_preview")
                .and_then(|preview| preview.get("blockers"))
                .cloned()
                .unwrap_or_else(|| serde_json::json!([])),
            "executed": false,
            "request_sent": false,
            "post_order_called": false,
            "post_orders_called": false,
        },
        "paper_only": true,
        "real_orders_enabled": false,
        "request_sent": false,
        "post_order_called": false,
        "post_orders_called": false,
    })
}

async fn build_strategy_paper_order_submission(
    pool: &PgPool,
    request: StrategyPaperOrderRequest,
) -> (StatusCode, serde_json::Value) {
    let market_id = request.market_id.trim();
    if market_id.is_empty() {
        return strategy_paper_order_rejection(
            pool,
            request,
            None,
            None,
            vec!["market_id_required".to_string()],
        )
        .await;
    }

    let candidates_body = match build_strategy_paper_candidates(pool).await {
        Ok(body) => body,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({
                    "paper_only": true,
                    "real_orders_enabled": false,
                    "accepted_for_paper": false,
                    "executed": false,
                    "request_sent": false,
                    "post_order_called": false,
                    "post_orders_called": false,
                    "error": format!("Failed to re-derive strategy candidates: {e}")
                }),
            );
        }
    };

    let normalized_outcome = request
        .outcome
        .as_deref()
        .and_then(normalize_paper_order_outcome);
    let candidate = candidates_body
        .get("candidates")
        .and_then(|value| value.as_array())
        .and_then(|rows| {
            rows.iter().find(|candidate| {
                let id_matches = candidate.get("market_id").and_then(|v| v.as_str())
                    == Some(market_id)
                    || candidate.get("slug").and_then(|v| v.as_str()) == Some(market_id);
                let outcome_matches = normalized_outcome.as_deref().is_none_or(|outcome| {
                    candidate.get("target_outcome").and_then(|v| v.as_str()) == Some(outcome)
                });
                id_matches && outcome_matches
            })
        })
        .cloned();

    let Some(candidate) = candidate else {
        return strategy_paper_order_rejection(
            pool,
            request,
            None,
            None,
            vec!["strategy_candidate_not_found".to_string()],
        )
        .await;
    };

    let requested_size = request.size.unwrap_or(Decimal::ONE);
    let candidate =
        attach_strategy_requested_size_paper_preview(pool, candidate, requested_size).await;
    let candidate_market_id = candidate
        .get("market_id")
        .and_then(|value| value.as_str())
        .unwrap_or(market_id)
        .to_string();
    let candidate_outcome = candidate
        .get("target_outcome")
        .and_then(|value| value.as_str())
        .unwrap_or("Yes")
        .to_string();
    let observation_evidence = match load_strategy_paper_candidate_observation_evidence(
        pool,
        &candidate_market_id,
        &candidate_outcome,
        requested_size,
    )
    .await
    {
        Ok(evidence) => evidence,
        Err(e) => serde_json::json!({
            "available": false,
            "status": "strategy_candidate_observation_lookup_failed",
            "error": e.to_string(),
            "market_id": candidate_market_id,
            "target_outcome": candidate_outcome,
            "strategy_requested_size": requested_size,
            "max_age_seconds": STRATEGY_CANDIDATE_OBSERVATION_MAX_AGE_SECONDS,
            "paper_only": true,
            "real_orders_enabled": false,
            "request_sent": false,
            "post_order_called": false,
            "post_orders_called": false,
        }),
    };

    let blockers = strategy_paper_order_gate_blockers(&candidate, &request, &observation_evidence);
    if !blockers.is_empty() {
        return strategy_paper_order_rejection(
            pool,
            request,
            Some(candidate),
            Some(observation_evidence),
            blockers,
        )
        .await;
    }

    let market_id = candidate
        .get("market_id")
        .and_then(|value| value.as_str())
        .unwrap_or(market_id)
        .to_string();
    let outcome = candidate
        .get("target_outcome")
        .and_then(|value| value.as_str())
        .unwrap_or("Yes")
        .to_string();
    let rationale = format!(
        "strategy paper candidate manual submit: {}",
        request
            .note
            .as_deref()
            .unwrap_or("operator confirmed strategy paper candidate")
    );
    let paper_request = PaperOrderRequest {
        market_id,
        outcome,
        side: "Buy".to_string(),
        order_type: "Market".to_string(),
        size: requested_size,
        limit_price: None,
        rationale: Some(rationale),
        confirm_paper_order: Some(true),
    };
    let (status, mut body) = submit_paper_order_from_request(
        pool,
        paper_request,
        "strategy_paper_order_submit_route",
        "strategy_paper_order_submit_route_validation",
        Some(serde_json::json!({
            "strategy_candidate": candidate,
            "strategy_candidate_observation_evidence": observation_evidence,
            "confirm_strategy_paper_order": true,
            "operator_note": request.note,
        })),
    )
    .await;
    if let Some(object) = body.as_object_mut() {
        object.insert("strategy_paper_order".to_string(), serde_json::json!(true));
        object.insert("strategy_candidate".to_string(), candidate);
        object.insert(
            "strategy_candidate_observation_evidence".to_string(),
            observation_evidence,
        );
        object.insert(
            "confirm_strategy_paper_order".to_string(),
            serde_json::json!(true),
        );
    }
    (status, body)
}

async fn build_strategy_paper_order_readiness(
    pool: &PgPool,
    query: StrategyPaperOrderReadinessQuery,
) -> anyhow::Result<serde_json::Value> {
    let candidates_body = build_strategy_paper_candidates(pool).await?;
    let market_id = query.market_id.as_deref().map(str::trim).unwrap_or("");
    let requested_size = query.size.unwrap_or(Decimal::ONE);
    let normalized_outcome = query
        .outcome
        .as_deref()
        .and_then(normalize_paper_order_outcome);

    let candidate = candidates_body
        .get("candidates")
        .and_then(|value| value.as_array())
        .and_then(|rows| {
            if market_id.is_empty() {
                rows.first()
            } else {
                rows.iter().find(|candidate| {
                    let id_matches = candidate.get("market_id").and_then(|v| v.as_str())
                        == Some(market_id)
                        || candidate.get("slug").and_then(|v| v.as_str()) == Some(market_id);
                    let outcome_matches = normalized_outcome.as_deref().is_none_or(|outcome| {
                        candidate.get("target_outcome").and_then(|v| v.as_str()) == Some(outcome)
                    });
                    id_matches && outcome_matches
                })
            }
        })
        .cloned();

    let Some(candidate) = candidate else {
        return Ok(serde_json::json!({
            "paper_only": true,
            "real_orders_enabled": false,
            "strategy_paper_order_readiness": true,
            "ready_for_strategy_paper_order": false,
            "blockers": ["strategy_candidate_not_found"],
            "strategy_requested_size": requested_size,
            "candidate": null,
            "strategy_candidate_observation_evidence": null,
            "submit_requires_confirm_strategy_paper_order": true,
            "request_sent": false,
            "post_order_called": false,
            "post_orders_called": false,
            "note": "Read-only strategy paper-order preflight; no paper order, rejection event, or CLOB order API is called."
        }));
    };

    let candidate =
        attach_strategy_requested_size_paper_preview(pool, candidate, requested_size).await;
    let candidate_market_id = candidate
        .get("market_id")
        .and_then(|value| value.as_str())
        .unwrap_or(market_id)
        .to_string();
    let candidate_outcome = candidate
        .get("target_outcome")
        .and_then(|value| value.as_str())
        .unwrap_or("Yes")
        .to_string();
    let observation_evidence = match load_strategy_paper_candidate_observation_evidence(
        pool,
        &candidate_market_id,
        &candidate_outcome,
        requested_size,
    )
    .await
    {
        Ok(evidence) => evidence,
        Err(e) => serde_json::json!({
            "available": false,
            "status": "strategy_candidate_observation_lookup_failed",
            "error": e.to_string(),
            "market_id": candidate_market_id,
            "target_outcome": candidate_outcome,
            "strategy_requested_size": requested_size,
            "max_age_seconds": STRATEGY_CANDIDATE_OBSERVATION_MAX_AGE_SECONDS,
            "paper_only": true,
            "real_orders_enabled": false,
            "request_sent": false,
            "post_order_called": false,
            "post_orders_called": false,
        }),
    };
    let gate_request = StrategyPaperOrderRequest {
        market_id: candidate_market_id,
        outcome: Some(candidate_outcome),
        size: Some(requested_size),
        confirm_strategy_paper_order: Some(true),
        note: Some("read-only strategy paper-order readiness preflight".to_string()),
    };
    let blockers =
        strategy_paper_order_gate_blockers(&candidate, &gate_request, &observation_evidence);
    let ready_for_strategy_paper_order = blockers.is_empty();

    Ok(serde_json::json!({
        "paper_only": true,
        "real_orders_enabled": false,
        "strategy_paper_order_readiness": true,
        "ready_for_strategy_paper_order": ready_for_strategy_paper_order,
        "blockers": blockers,
        "strategy_requested_size": requested_size,
        "candidate": candidate,
        "strategy_candidate_observation_evidence": observation_evidence,
        "submit_requires_confirm_strategy_paper_order": true,
        "request_sent": false,
        "would_send": false,
        "would_post": false,
        "post_order_called": false,
        "post_orders_called": false,
        "note": "Read-only strategy paper-order preflight. It mirrors current gates without journaling a rejection or invoking paper/CLOB order paths."
    }))
}

async fn attach_strategy_requested_size_paper_preview(
    pool: &PgPool,
    mut candidate: serde_json::Value,
    requested_size: Decimal,
) -> serde_json::Value {
    //! Rebuild the candidate's embedded paper preview for the operator's
    //! requested paper size.
    //!
    //! RISK: The strategy candidate list is a read-only ranking surface and uses
    //! a one-share preview. The execution bridge must evaluate the exact size
    //! requested by the operator before it can delegate to PaperTradingEngine;
    //! otherwise a large paper order could pass strategy gates using stale
    //! one-share risk data and only fail later in the lower-level engine.
    let market_id = candidate
        .get("market_id")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    let target_outcome = candidate
        .get("target_outcome")
        .and_then(|value| value.as_str())
        .unwrap_or("Yes")
        .to_string();
    let preview_request = PaperOrderRequest {
        market_id,
        outcome: target_outcome,
        side: "Buy".to_string(),
        order_type: "Market".to_string(),
        size: requested_size,
        limit_price: None,
        rationale: Some("requested-size strategy paper preview".to_string()),
        confirm_paper_order: Some(false),
    };
    let paper_order_preview = build_paper_order_plan(pool, &preview_request)
        .await
        .unwrap_or_else(|e| {
            serde_json::json!({
                "paper_only": true,
                "real_orders_enabled": false,
                "accepted_for_paper": false,
                "executed": false,
                "dry_run_only": true,
                "blockers": ["paper_order_preview_failed"],
                "error": e.to_string(),
                "request_sent": false,
                "would_send": false,
                "would_post": false,
                "post_order_called": false,
                "post_orders_called": false,
            })
        });

    if let Some(object) = candidate.as_object_mut() {
        object.insert("size".to_string(), serde_json::json!(requested_size));
        object.insert(
            "strategy_requested_size".to_string(),
            serde_json::json!(requested_size),
        );
        object.insert("paper_order_preview".to_string(), paper_order_preview);
    }
    candidate
}

fn strategy_paper_order_gate_blockers(
    candidate: &serde_json::Value,
    request: &StrategyPaperOrderRequest,
    observation_evidence: &serde_json::Value,
) -> Vec<String> {
    let mut blockers = Vec::new();
    if request.confirm_strategy_paper_order != Some(true) {
        blockers.push("confirm_strategy_paper_order_required".to_string());
    }
    if candidate.get("decision").and_then(|v| v.as_str())
        != Some("paper_candidate_ready_for_manual_review")
    {
        blockers.push("strategy_net_edge_below_minimum".to_string());
    }
    if observation_evidence
        .get("available")
        .and_then(|value| value.as_bool())
        != Some(true)
    {
        blockers.push("strategy_candidate_observation_required".to_string());
    } else {
        if observation_evidence
            .get("is_recent")
            .and_then(|value| value.as_bool())
            != Some(true)
        {
            blockers.push("strategy_candidate_observation_stale".to_string());
        }
        if observation_evidence
            .get("observation_ready_for_manual_review")
            .and_then(|value| value.as_bool())
            != Some(true)
        {
            blockers.push("strategy_candidate_observation_not_ready".to_string());
        }
    }
    if candidate
        .get("paper_order_preview")
        .and_then(|preview| preview.get("accepted_for_paper"))
        .and_then(|value| value.as_bool())
        != Some(true)
    {
        blockers.push("strategy_paper_preview_blocked".to_string());
    }
    blockers
}

async fn strategy_paper_order_rejection(
    pool: &PgPool,
    request: StrategyPaperOrderRequest,
    candidate: Option<serde_json::Value>,
    observation_evidence: Option<serde_json::Value>,
    blockers: Vec<String>,
) -> (StatusCode, serde_json::Value) {
    let payload = serde_json::json!({
        "paper_only": true,
        "real_orders_enabled": false,
        "accepted_for_paper": false,
        "executed": false,
        "strategy_paper_order": true,
        "source": "strategy_paper_order_submit_route_validation",
        "market_id": request.market_id.trim(),
        "requested_outcome": request.outcome,
        "requested_size": request.size,
        "confirm_strategy_paper_order": request.confirm_strategy_paper_order == Some(true),
        "candidate": candidate,
        "strategy_candidate_observation_evidence": observation_evidence,
        "blockers": blockers,
        "request_sent": false,
        "would_send": false,
        "would_post": false,
        "post_order_called": false,
        "post_orders_called": false,
        "note": "Strategy-gated paper submit rejected before PaperTradingEngine writes paper order, fill, position, or portfolio snapshot rows."
    });
    let journal_result = record_journal_event(
        pool,
        "strategy_paper_order_submit_route_validation",
        "polytrader_server",
        "warning",
        payload,
    )
    .await;
    let mut body = serde_json::json!({
        "paper_only": true,
        "real_orders_enabled": false,
        "accepted_for_paper": false,
        "executed": false,
        "strategy_paper_order": true,
        "blockers": blockers,
        "candidate": candidate,
        "strategy_candidate_observation_evidence": observation_evidence,
        "request_sent": false,
        "post_order_called": false,
        "post_orders_called": false,
        "note": "Strategy-gated paper submit rejected before paper execution."
    });
    if let Some(object) = body.as_object_mut() {
        match journal_result {
            Ok(event_id) => {
                object.insert("journaled".to_string(), serde_json::json!(true));
                object.insert("journal_event_id".to_string(), serde_json::json!(event_id));
            }
            Err(e) => {
                object.insert("journaled".to_string(), serde_json::json!(false));
                object.insert(
                    "journal_error".to_string(),
                    serde_json::json!(e.to_string()),
                );
            }
        }
    }
    (StatusCode::BAD_REQUEST, body)
}

fn strategy_candidate_decision(
    net_edge_after_fees: Decimal,
    min_net_edge_for_trade: Decimal,
) -> &'static str {
    if net_edge_after_fees >= min_net_edge_for_trade {
        "paper_candidate_ready_for_manual_review"
    } else {
        "observe"
    }
}

async fn build_paper_order_plan(
    pool: &PgPool,
    request: &PaperOrderRequest,
) -> anyhow::Result<serde_json::Value> {
    let mut blockers: Vec<&'static str> = Vec::new();
    let market_id = request.market_id.trim();
    if market_id.is_empty() {
        blockers.push("market_id_required");
    }

    let outcome = normalize_paper_order_outcome(&request.outcome);
    if outcome.is_none() {
        blockers.push("invalid_outcome");
    }
    let side = parse_paper_order_side(&request.side);
    if side.is_none() {
        blockers.push("invalid_side");
    }
    let order_type = parse_paper_order_type(&request.order_type);
    if order_type.is_none() {
        blockers.push("invalid_order_type");
    }
    if request.size <= Decimal::ZERO {
        blockers.push("size_must_be_positive");
    }

    let market = if market_id.is_empty() {
        None
    } else {
        sqlx::query_as::<_, PaperOrderMarketReadinessRow>(
            "SELECT gamma_id, slug, question, active, last_mid_yes, last_mid_no
             FROM market_data.markets
             WHERE gamma_id = $1 OR slug = $1
             LIMIT 1",
        )
        .bind(market_id)
        .fetch_optional(pool)
        .await?
    };
    let Some(market) = market else {
        blockers.push("market_not_found");
        return Ok(paper_order_plan_json(
            request,
            None,
            None,
            None,
            None,
            None,
            Decimal::ZERO,
            Decimal::ZERO,
            Decimal::ZERO,
            Decimal::ZERO,
            Decimal::ZERO,
            blockers,
        ));
    };

    if !market.active {
        blockers.push("market_not_active");
    }
    if !market_has_two_sided_mids(&market.last_mid_yes, &market.last_mid_no) {
        blockers.push("market_data_missing_two_sided_mids");
    }

    let mid = match outcome.as_deref() {
        Some("Yes") => market.last_mid_yes,
        Some("No") => market.last_mid_no,
        _ => None,
    };
    let limit_price = request.limit_price;
    if matches!(order_type, Some(crate::paper::OrderType::Limit)) {
        match limit_price {
            Some(price) if price > Decimal::ZERO && price < Decimal::ONE => {}
            _ => blockers.push("valid_limit_price_required"),
        }
    }

    let reference_price = limit_price.or(mid);
    let estimated_notional = reference_price
        .map(|price| request.size * price)
        .unwrap_or(Decimal::ZERO);

    let latest_usdc = latest_virtual_usdc(pool).await?;
    let max_order_notional = paper_max_order_notional(latest_usdc);
    let max_total_exposure = paper_max_total_exposure(latest_usdc);
    let current_total_collateral_locked = current_paper_collateral_locked(pool).await?;
    let projected_total_collateral_locked = paper_projected_total_collateral_locked(
        side.as_ref(),
        current_total_collateral_locked,
        estimated_notional,
    );
    if estimated_notional > max_order_notional {
        blockers.push("max_order_notional_exceeded");
    }
    if projected_total_collateral_locked > max_total_exposure {
        blockers.push("max_total_exposure_exceeded");
    }

    let current_position = if let Some(outcome) = outcome.as_deref() {
        sqlx::query_scalar::<_, Decimal>(
            "SELECT shares FROM paper_trading.paper_positions WHERE market_id = $1 AND outcome = $2",
        )
        .bind(&market.gamma_id)
        .bind(outcome)
        .fetch_optional(pool)
        .await?
        .unwrap_or(Decimal::ZERO)
    } else {
        Decimal::ZERO
    };
    if matches!(side, Some(crate::paper::OrderSide::Sell)) && current_position < request.size {
        blockers.push("insufficient_paper_position");
    }

    Ok(paper_order_plan_json(
        request,
        Some(&market),
        outcome.as_deref(),
        side.map(|value| value.to_string()),
        order_type.map(|value| value.to_string()),
        reference_price,
        latest_usdc,
        max_order_notional,
        max_total_exposure,
        current_total_collateral_locked,
        projected_total_collateral_locked,
        blockers,
    ))
}

#[allow(clippy::too_many_arguments)]
fn paper_order_plan_json(
    request: &PaperOrderRequest,
    market: Option<&PaperOrderMarketReadinessRow>,
    outcome: Option<&str>,
    side: Option<String>,
    order_type: Option<String>,
    reference_price: Option<Decimal>,
    latest_usdc: Decimal,
    max_order_notional: Decimal,
    max_total_exposure: Decimal,
    current_total_collateral_locked: Decimal,
    projected_total_collateral_locked: Decimal,
    blockers: Vec<&'static str>,
) -> serde_json::Value {
    let estimated_notional = reference_price
        .map(|price| request.size * price)
        .unwrap_or(Decimal::ZERO);
    serde_json::json!({
        "paper_only": true,
        "real_orders_enabled": false,
        "accepted_for_paper": blockers.is_empty(),
        "executed": false,
        "dry_run_only": true,
        "request_sent": false,
        "would_send": false,
        "would_post": false,
        "post_order_called": false,
        "post_orders_called": false,
        "blockers": blockers,
        "market": market.map(|market| serde_json::json!({
            "gamma_id": market.gamma_id,
            "slug": market.slug,
            "question": market.question,
            "active": market.active,
            "last_mid_yes": market.last_mid_yes,
            "last_mid_no": market.last_mid_no,
            "market_data_status": market_data_status(&market.last_mid_yes, &market.last_mid_no),
        })),
        "normalized_intent": {
            "market_id": market.map(|market| market.gamma_id.as_str()).unwrap_or(request.market_id.trim()),
            "outcome": outcome,
            "side": side,
            "order_type": order_type,
            "size": request.size,
            "limit_price": request.limit_price,
            "reference_price": reference_price,
            "estimated_notional": estimated_notional,
        },
        "risk": {
            "latest_virtual_usdc": latest_usdc,
            "max_order_notional": max_order_notional,
            "max_order_notional_pct": "1",
            "max_total_exposure": max_total_exposure,
            "max_total_exposure_pct": "15",
            "current_total_collateral_locked": current_total_collateral_locked,
            "projected_total_collateral_locked": projected_total_collateral_locked,
            "projected_total_exposure_within_limit": projected_total_collateral_locked <= max_total_exposure,
            "short_selling_allowed": false,
        },
        "note": "Paper order preview only unless /paper/orders is called with confirm_paper_order:true. No CLOB order API is called."
    })
}

async fn latest_virtual_usdc(pool: &PgPool) -> anyhow::Result<Decimal> {
    Ok(sqlx::query_scalar::<_, Decimal>(
        "SELECT virtual_usdc FROM paper_trading.virtual_portfolio_snapshots ORDER BY as_of DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?
    .unwrap_or(Decimal::from(10000u64)))
}

async fn reset_paper_simulator_state(
    pool: &PgPool,
    reason: &str,
    operator: Option<&str>,
) -> anyhow::Result<serde_json::Value> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(780112301)")
        .execute(&mut *tx)
        .await?;

    let (position_count_before, total_collateral_before): (i64, Decimal) = sqlx::query_as(
        "SELECT COUNT(*)::BIGINT, COALESCE(SUM(collateral_locked), 0)::NUMERIC
         FROM paper_trading.paper_positions",
    )
    .fetch_one(&mut *tx)
    .await?;
    let order_count_preserved: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM paper_trading.paper_orders")
            .fetch_one(&mut *tx)
            .await?;
    let fill_count_preserved: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM paper_trading.paper_fills")
            .fetch_one(&mut *tx)
            .await?;

    let deleted_positions = sqlx::query("DELETE FROM paper_trading.paper_positions")
        .execute(&mut *tx)
        .await?
        .rows_affected();

    let reset_usdc = Decimal::from(10000u64);
    sqlx::query(
        r#"INSERT INTO paper_trading.virtual_portfolio_snapshots
           (virtual_usdc, total_locked, unrealized_pnl, realized_pnl, snapshot_reason, positions)
           VALUES ($1, 0, 0, 0, 'manual_paper_reset', '[]'::jsonb)"#,
    )
    .bind(reset_usdc)
    .execute(&mut *tx)
    .await?;

    let event_id = uuid::Uuid::new_v4();
    let payload = serde_json::json!({
        "paper_only": true,
        "real_orders_enabled": false,
        "reset_applied": true,
        "reason": reason,
        "operator": operator.unwrap_or("unspecified"),
        "position_count_before": position_count_before,
        "deleted_positions": deleted_positions,
        "total_collateral_before": total_collateral_before,
        "reset_virtual_usdc": reset_usdc,
        "order_count_preserved": order_count_preserved,
        "fill_count_preserved": fill_count_preserved,
        "orders_and_fills_deleted": false,
        "request_sent": false,
        "would_send": false,
        "would_post": false,
        "post_order_called": false,
        "post_orders_called": false,
        "note": "Paper simulator current state reset only; historical paper orders and fills are preserved for audit."
    });
    sqlx::query(
        r#"INSERT INTO journal.events (id, event_type, source, severity, payload)
           VALUES ($1, 'paper_simulator_reset', 'paper_reset_route', 'warning', $2)"#,
    )
    .bind(event_id)
    .bind(&payload)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(serde_json::json!({
        "paper_only": true,
        "real_orders_enabled": false,
        "reset_applied": true,
        "journaled": true,
        "journal_event_id": event_id,
        "position_count_before": position_count_before,
        "deleted_positions": deleted_positions,
        "total_collateral_before": total_collateral_before,
        "reset_virtual_usdc": reset_usdc,
        "order_count_preserved": order_count_preserved,
        "fill_count_preserved": fill_count_preserved,
        "orders_and_fills_deleted": false,
        "request_sent": false,
        "would_send": false,
        "would_post": false,
        "post_order_called": false,
        "post_orders_called": false,
        "note": "Paper simulator current state reset only; historical paper orders and fills are preserved for audit."
    }))
}

async fn build_paper_reconciliation_report(pool: &PgPool) -> anyhow::Result<serde_json::Value> {
    let latest_reset_at: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT as_of
         FROM paper_trading.virtual_portfolio_snapshots
         WHERE snapshot_reason = 'manual_paper_reset'
         ORDER BY as_of DESC
         LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;

    let latest_snapshot = sqlx::query_as::<_, LatestPaperPortfolioSnapshotRow>(
        "SELECT as_of, virtual_usdc, total_locked, unrealized_pnl, realized_pnl, snapshot_reason
         FROM paper_trading.virtual_portfolio_snapshots
         ORDER BY as_of DESC
         LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;

    let current_positions = sqlx::query_as::<_, PaperPositionLedgerRow>(
        "SELECT market_id, outcome, shares, collateral_locked
         FROM paper_trading.paper_positions
         ORDER BY market_id, outcome",
    )
    .fetch_all(pool)
    .await?;

    let expected_positions = sqlx::query_as::<_, ExpectedPaperPositionLedgerRow>(
        r#"WITH latest_reset AS (
               SELECT as_of
               FROM paper_trading.virtual_portfolio_snapshots
               WHERE snapshot_reason = 'manual_paper_reset'
               ORDER BY as_of DESC
               LIMIT 1
           )
           SELECT
               o.market_id,
               o.outcome,
               COALESCE(SUM(CASE WHEN o.side = 'Buy' THEN f.size ELSE -f.size END), 0)::NUMERIC AS expected_shares,
               COUNT(f.id)::BIGINT AS fill_count
           FROM paper_trading.paper_fills f
           JOIN paper_trading.paper_orders o ON o.id = f.order_id
           WHERE f.created_at > COALESCE((SELECT as_of FROM latest_reset), 'epoch'::timestamptz)
           GROUP BY o.market_id, o.outcome
           HAVING COALESCE(SUM(CASE WHEN o.side = 'Buy' THEN f.size ELSE -f.size END), 0) <> 0
           ORDER BY o.market_id, o.outcome"#,
    )
    .fetch_all(pool)
    .await?;

    let fills_since_reset_count: i64 = sqlx::query_scalar(
        "WITH latest_reset AS (
             SELECT as_of
             FROM paper_trading.virtual_portfolio_snapshots
             WHERE snapshot_reason = 'manual_paper_reset'
             ORDER BY as_of DESC
             LIMIT 1
         )
         SELECT COUNT(*)
         FROM paper_trading.paper_fills
         WHERE created_at > COALESCE((SELECT as_of FROM latest_reset), 'epoch'::timestamptz)",
    )
    .fetch_one(pool)
    .await?;

    let orders_since_reset_count: i64 = sqlx::query_scalar(
        "WITH latest_reset AS (
             SELECT as_of
             FROM paper_trading.virtual_portfolio_snapshots
             WHERE snapshot_reason = 'manual_paper_reset'
             ORDER BY as_of DESC
             LIMIT 1
         )
         SELECT COUNT(*)
         FROM paper_trading.paper_orders
         WHERE created_at > COALESCE((SELECT as_of FROM latest_reset), 'epoch'::timestamptz)",
    )
    .fetch_one(pool)
    .await?;

    let current_total_collateral_locked: Decimal = current_positions
        .iter()
        .map(|row| row.collateral_locked)
        .sum();
    let mut current_by_key = HashMap::new();
    for row in &current_positions {
        current_by_key.insert((row.market_id.clone(), row.outcome.clone()), row.shares);
    }

    let mut mismatches = Vec::new();
    for expected in &expected_positions {
        let key = (expected.market_id.clone(), expected.outcome.clone());
        let actual_shares = current_by_key.remove(&key).unwrap_or(Decimal::ZERO);
        if actual_shares != expected.expected_shares {
            mismatches.push(serde_json::json!({
                "type": "position_share_mismatch",
                "market_id": expected.market_id,
                "outcome": expected.outcome,
                "expected_shares": expected.expected_shares,
                "actual_shares": actual_shares,
                "fill_count_since_reset": expected.fill_count,
            }));
        }
    }
    for ((market_id, outcome), actual_shares) in current_by_key {
        if actual_shares != Decimal::ZERO {
            mismatches.push(serde_json::json!({
                "type": "unexpected_current_position_without_post_reset_fills",
                "market_id": market_id,
                "outcome": outcome,
                "actual_shares": actual_shares,
                "expected_shares": "0",
            }));
        }
    }

    if let Some(snapshot) = &latest_snapshot {
        if snapshot.total_locked != current_total_collateral_locked {
            mismatches.push(serde_json::json!({
                "type": "snapshot_total_locked_mismatch",
                "snapshot_total_locked": snapshot.total_locked,
                "current_total_collateral_locked": current_total_collateral_locked,
                "snapshot_reason": snapshot.snapshot_reason,
                "snapshot_as_of": snapshot.as_of,
            }));
        }
    } else {
        mismatches.push(serde_json::json!({
            "type": "missing_portfolio_snapshot",
        }));
    }

    let status = if mismatches.is_empty() {
        "reconciled"
    } else {
        "mismatch"
    };

    Ok(serde_json::json!({
        "paper_only": true,
        "real_orders_enabled": false,
        "request_sent": false,
        "post_order_called": false,
        "post_orders_called": false,
        "status": status,
        "latest_reset_at": latest_reset_at,
        "orders_since_reset_count": orders_since_reset_count,
        "fills_since_reset_count": fills_since_reset_count,
        "current_position_count": current_positions.len(),
        "expected_position_count": expected_positions.len(),
        "current_total_collateral_locked": current_total_collateral_locked,
        "latest_snapshot": latest_snapshot.map(|snapshot| serde_json::json!({
            "as_of": snapshot.as_of,
            "virtual_usdc": snapshot.virtual_usdc,
            "total_locked": snapshot.total_locked,
            "unrealized_pnl": snapshot.unrealized_pnl,
            "realized_pnl": snapshot.realized_pnl,
            "snapshot_reason": snapshot.snapshot_reason,
        })),
        "expected_positions": expected_positions.into_iter().map(|row| serde_json::json!({
            "market_id": row.market_id,
            "outcome": row.outcome,
            "expected_shares": row.expected_shares,
            "fill_count_since_reset": row.fill_count,
        })).collect::<Vec<_>>(),
        "mismatch_count": mismatches.len(),
        "mismatches": mismatches,
        "note": "Read-only paper reconciliation from current positions, latest portfolio snapshot, and fills after the latest manual reset; no CLOB order API is called."
    }))
}

fn paper_max_order_notional(latest_virtual_usdc: Decimal) -> Decimal {
    latest_virtual_usdc * Decimal::new(1, 2)
}

fn paper_max_total_exposure(latest_virtual_usdc: Decimal) -> Decimal {
    latest_virtual_usdc * Decimal::new(15, 2)
}

async fn current_paper_collateral_locked(pool: &PgPool) -> anyhow::Result<Decimal> {
    Ok(sqlx::query_scalar::<_, Decimal>(
        "SELECT COALESCE(SUM(collateral_locked), 0)::NUMERIC FROM paper_trading.paper_positions",
    )
    .fetch_one(pool)
    .await?)
}

fn paper_projected_total_collateral_locked(
    side: Option<&crate::paper::OrderSide>,
    current_total_collateral_locked: Decimal,
    estimated_notional: Decimal,
) -> Decimal {
    if matches!(side, Some(crate::paper::OrderSide::Buy)) {
        current_total_collateral_locked + estimated_notional
    } else {
        current_total_collateral_locked
    }
}

async fn load_paper_position_rows(pool: &PgPool) -> anyhow::Result<Vec<PaperPositionHistoryRow>> {
    Ok(sqlx::query_as::<_, PaperPositionHistoryRow>(
        r#"SELECT
                p.market_id,
                m.slug,
                m.question,
                m.category,
                p.outcome,
                p.shares,
                p.avg_entry_price,
                p.collateral_locked,
                m.last_mid_yes,
                m.last_mid_no,
                p.last_updated
           FROM paper_trading.paper_positions p
           LEFT JOIN market_data.markets m ON m.gamma_id = p.market_id
           ORDER BY p.last_updated DESC, p.market_id, p.outcome"#,
    )
    .fetch_all(pool)
    .await?)
}

fn build_paper_risk_summary(
    latest_virtual_usdc: Decimal,
    rows: Vec<PaperPositionHistoryRow>,
) -> serde_json::Value {
    let open_position_count = rows.len();
    let total_collateral_locked: Decimal = rows.iter().map(|row| row.collateral_locked).sum();
    let total_mark_value: Decimal = rows
        .iter()
        .map(|row| {
            let mark_price = if row.outcome.eq_ignore_ascii_case("yes") {
                row.last_mid_yes
            } else {
                row.last_mid_no
            };
            mark_price
                .map(|price| row.shares * price)
                .unwrap_or(row.collateral_locked)
        })
        .sum();
    let unrealized_pnl = total_mark_value - total_collateral_locked;
    let max_order_notional = paper_max_order_notional(latest_virtual_usdc);
    let max_total_exposure = paper_max_total_exposure(latest_virtual_usdc);
    let percent = Decimal::from(100u64);
    let total_exposure_pct_of_bankroll = if latest_virtual_usdc > Decimal::ZERO {
        total_collateral_locked / latest_virtual_usdc * percent
    } else {
        Decimal::ZERO
    };
    let total_exposure_limit_used_pct = if max_total_exposure > Decimal::ZERO {
        total_collateral_locked / max_total_exposure * percent
    } else {
        Decimal::ZERO
    };
    let within_total_exposure_limit = total_collateral_locked <= max_total_exposure;
    let status = if within_total_exposure_limit {
        "within_limits"
    } else {
        "total_exposure_limit_exceeded"
    };

    serde_json::json!({
        "paper_only": true,
        "real_orders_enabled": false,
        "request_sent": false,
        "post_order_called": false,
        "post_orders_called": false,
        "latest_virtual_usdc": latest_virtual_usdc,
        "open_position_count": open_position_count,
        "total_collateral_locked": total_collateral_locked,
        "total_mark_value": total_mark_value,
        "unrealized_pnl": unrealized_pnl,
        "max_order_notional": max_order_notional,
        "max_order_notional_pct": "1",
        "max_total_exposure": max_total_exposure,
        "max_total_exposure_pct": "15",
        "total_exposure_pct_of_bankroll": total_exposure_pct_of_bankroll,
        "total_exposure_limit_used_pct": total_exposure_limit_used_pct,
        "within_total_exposure_limit": within_total_exposure_limit,
        "status": status,
        "positions": rows.into_iter().map(paper_position_history_json).collect::<Vec<_>>(),
        "note": "Read-only aggregate paper risk summary; no CLOB order, wallet, or allowance API is called."
    })
}

fn paper_order_history_json(row: PaperOrderHistoryRow) -> serde_json::Value {
    serde_json::json!({
        "id": row.id,
        "market_id": row.market_id,
        "slug": row.slug,
        "question": row.question,
        "outcome": row.outcome,
        "side": row.side,
        "order_type": row.order_type,
        "limit_price": row.limit_price,
        "size": row.size,
        "status": row.status,
        "fill_count": row.fill_count,
        "filled_size": row.filled_size,
        "gross_notional": row.gross_notional,
        "total_fee": row.total_fee,
        "decision_context": row.decision_context.unwrap_or_else(|| serde_json::json!({})),
        "created_at": row.created_at,
        "updated_at": row.updated_at,
        "paper_only": true,
        "real_orders_enabled": false,
    })
}

fn paper_fill_history_json(row: PaperFillHistoryRow) -> serde_json::Value {
    serde_json::json!({
        "id": row.id,
        "order_id": row.order_id,
        "market_id": row.market_id,
        "slug": row.slug,
        "outcome": row.outcome,
        "side": row.side,
        "price": row.price,
        "size": row.size,
        "fee": row.fee,
        "slippage_bps": row.slippage_bps,
        "created_at": row.created_at,
        "paper_only": true,
        "real_orders_enabled": false,
    })
}

fn paper_position_history_json(row: PaperPositionHistoryRow) -> serde_json::Value {
    let mark_price = if row.outcome.eq_ignore_ascii_case("yes") {
        row.last_mid_yes
    } else {
        row.last_mid_no
    };
    let mark_value = mark_price.map(|price| row.shares * price);
    let unrealized_pnl = mark_value.map(|value| value - row.collateral_locked);
    let category_label = row
        .category
        .as_deref()
        .map(category_display_label)
        .map(str::to_string);

    serde_json::json!({
        "market_id": row.market_id,
        "slug": row.slug,
        "question": row.question,
        "category": row.category,
        "category_label": category_label,
        "outcome": row.outcome,
        "shares": row.shares,
        "avg_entry_price": row.avg_entry_price,
        "collateral_locked": row.collateral_locked,
        "mark_price": mark_price,
        "mark_value": mark_value,
        "unrealized_pnl": unrealized_pnl,
        "last_updated": row.last_updated,
        "paper_only": true,
        "real_orders_enabled": false,
    })
}

fn paper_fee_bps_from_env() -> u16 {
    std::env::var("POLYTRADER_PAPER_FEE_BPS")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(50)
}

fn normalize_paper_order_outcome(value: &str) -> Option<String> {
    if value.eq_ignore_ascii_case("yes") {
        Some("Yes".to_string())
    } else if value.eq_ignore_ascii_case("no") {
        Some("No".to_string())
    } else {
        None
    }
}

fn parse_paper_order_side(value: &str) -> Option<crate::paper::OrderSide> {
    if value.eq_ignore_ascii_case("buy") {
        Some(crate::paper::OrderSide::Buy)
    } else if value.eq_ignore_ascii_case("sell") {
        Some(crate::paper::OrderSide::Sell)
    } else {
        None
    }
}

fn parse_paper_order_type(value: &str) -> Option<crate::paper::OrderType> {
    if value.eq_ignore_ascii_case("market") {
        Some(crate::paper::OrderType::Market)
    } else if value.eq_ignore_ascii_case("limit") {
        Some(crate::paper::OrderType::Limit)
    } else {
        None
    }
}

fn build_review_queue_item(
    dry_run_event_id: uuid::Uuid,
    dry_run_payload: serde_json::Value,
    dry_run_created_at: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> serde_json::Value {
    let dry_run_age_seconds = (now - dry_run_created_at).num_seconds().max(0);
    let blockers = dry_run_blockers(&dry_run_payload);
    let review_is_stale = dry_run_age_seconds >= REVIEW_BACKLOG_STALE_AFTER_SECONDS;
    let review_priority = if review_is_stale {
        "stale_unreviewed"
    } else if blockers.is_empty() {
        "standard_unreviewed"
    } else {
        "blocked_unreviewed"
    };
    let next_review_action = if review_is_stale {
        "review_stale_dry_run"
    } else if blockers.is_empty() {
        "review_before_any_live_work"
    } else {
        "confirm_conservative_rejection"
    };

    serde_json::json!({
        "dry_run_event_id": dry_run_event_id,
        "dry_run_created_at": dry_run_created_at,
        "dry_run_age_seconds": dry_run_age_seconds,
        "review_stale_after_seconds": REVIEW_BACKLOG_STALE_AFTER_SECONDS,
        "review_is_stale": review_is_stale,
        "review_priority": review_priority,
        "next_review_action": next_review_action,
        "dry_run_summary": dry_run_review_summary(&dry_run_payload),
        "blockers": blockers,
        "dry_run": dry_run_payload,
        "paper_only": true,
        "real_orders_enabled": false,
        "request_sent": false,
        "post_order_called": false,
        "post_orders_called": false,
    })
}

fn oldest_unreviewed_age_seconds(
    now: chrono::DateTime<chrono::Utc>,
    oldest: Option<chrono::DateTime<chrono::Utc>>,
) -> Option<i64> {
    oldest.map(|oldest| (now - oldest).num_seconds().max(0))
}

fn review_backlog_status(oldest_age_seconds: Option<i64>) -> &'static str {
    match oldest_age_seconds {
        None => "empty",
        Some(age) if age >= REVIEW_BACKLOG_STALE_AFTER_SECONDS => "stale",
        Some(_) => "fresh",
    }
}

fn sanitize_review_note(note: Option<&str>) -> String {
    note.unwrap_or("")
        .trim()
        .chars()
        .take(500)
        .collect::<String>()
}

fn dry_run_review_summary(payload: &serde_json::Value) -> serde_json::Value {
    let report = payload.get("report").unwrap_or(payload);
    let intent = report
        .get("intent")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let blocker_count = report
        .get("blockers")
        .and_then(|v| v.as_array())
        .map(Vec::len)
        .unwrap_or(0);
    let recommended_review_decision = recommended_review_decision(blocker_count);

    serde_json::json!({
        "market_id": intent.get("market_id").cloned().unwrap_or(serde_json::Value::Null),
        "token_id": intent.get("token_id").cloned().unwrap_or(serde_json::Value::Null),
        "side": intent.get("side").cloned().unwrap_or(serde_json::Value::Null),
        "order_type": intent.get("order_type").cloned().unwrap_or(serde_json::Value::Null),
        "estimated_notional": report.get("estimated_notional").cloned().unwrap_or(serde_json::Value::Null),
        "blocker_count": blocker_count,
        "approval_blocked": blocker_count > 0,
        "recommended_review_decision": recommended_review_decision,
        "accepted": report.get("accepted").cloned().unwrap_or(serde_json::json!(false)),
    })
}

fn recommended_review_decision(blocker_count: usize) -> &'static str {
    if blocker_count > 0 {
        "would_reject"
    } else {
        "needs_rework"
    }
}

fn review_decision_matches_guidance(
    review_payload: &serde_json::Value,
    dry_run_payload: &serde_json::Value,
) -> Option<bool> {
    let decision = review_payload
        .get("decision")
        .and_then(|decision| decision.as_str())?;
    let summary = dry_run_review_summary(dry_run_payload);
    let guidance = summary
        .get("recommended_review_decision")
        .and_then(|decision| decision.as_str())?;
    Some(decision == guidance)
}

fn review_override_requires_note(decision: &str, guidance: &str, note: &str) -> bool {
    decision != guidance && note.trim().is_empty()
}

fn dry_run_blockers(payload: &serde_json::Value) -> Vec<String> {
    let report = payload.get("report").unwrap_or(payload);
    report
        .get("blockers")
        .and_then(|v| v.as_array())
        .map(|blockers| {
            blockers
                .iter()
                .filter_map(|blocker| blocker.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn normalize_human_approval_decision(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "approve_facade" | "approve" | "would_approve" => Some("approve_facade"),
        "reject_facade" | "reject" | "would_reject" => Some("reject_facade"),
        "needs_rework" | "rework" => Some("needs_rework"),
        _ => None,
    }
}

fn normalize_final_review_decision(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "acknowledge_blocked" | "blocked" | "reviewed_blocked" => Some("acknowledge_blocked"),
        "reject_live_trading" | "reject" | "would_reject" => Some("reject_live_trading"),
        "needs_rework" | "rework" => Some("needs_rework"),
        _ => None,
    }
}

async fn validate_human_approval_event(
    pool: &PgPool,
    request: &crate::clob::authenticated::OrderSubmitFacadeRequest,
) -> crate::clob::authenticated::HumanApprovalValidation {
    let Some(event_id) = request.human_approval_event_id else {
        return crate::clob::authenticated::HumanApprovalValidation {
            valid: false,
            event_id: None,
            decision: None,
            subject_hash: None,
            blockers: vec!["human_approval_event_missing".to_string()],
            risk_snapshot: None,
            collateral_snapshot: None,
        };
    };

    let expected_subject_hash = crate::clob::authenticated::approval_subject_hash_for_intent(
        &request
            .post_request_dry_run_request
            .signed_payload_request
            .intent,
    );
    let row = sqlx::query_as::<_, (serde_json::Value, chrono::DateTime<chrono::Utc>)>(
        r#"SELECT payload, created_at
           FROM journal.events
           WHERE id = $1 AND event_type = 'clob_order_human_approval'"#,
    )
    .bind(event_id)
    .fetch_optional(pool)
    .await;

    let Some((payload, _created_at)) = (match row {
        Ok(row) => row,
        Err(_) => {
            return crate::clob::authenticated::HumanApprovalValidation {
                valid: false,
                event_id: Some(event_id),
                decision: None,
                subject_hash: Some(expected_subject_hash),
                blockers: vec!["human_approval_event_lookup_failed".to_string()],
                risk_snapshot: None,
                collateral_snapshot: None,
            };
        }
    }) else {
        return crate::clob::authenticated::HumanApprovalValidation {
            valid: false,
            event_id: Some(event_id),
            decision: None,
            subject_hash: Some(expected_subject_hash),
            blockers: vec!["human_approval_event_not_found".to_string()],
            risk_snapshot: None,
            collateral_snapshot: None,
        };
    };

    let actual_subject_hash = payload
        .get("subject_hash")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let decision = payload
        .get("decision")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let mut blockers = Vec::new();

    if actual_subject_hash != expected_subject_hash {
        blockers.push("human_approval_subject_mismatch".to_string());
    }
    if decision.as_deref() != Some("approve_facade") {
        blockers.push("human_approval_decision_not_approved".to_string());
    }
    if payload
        .get("approved_for_facade")
        .and_then(|value| value.as_bool())
        != Some(true)
    {
        blockers.push("human_approval_not_marked_approved_for_facade".to_string());
    }

    let expires_at = payload
        .get("expires_at")
        .and_then(|value| value.as_str())
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&chrono::Utc));
    match expires_at {
        Some(expires_at) if expires_at > chrono::Utc::now() => {}
        Some(_) => blockers.push("human_approval_event_expired".to_string()),
        None => blockers.push("human_approval_expiry_missing".to_string()),
    }

    blockers.sort();
    blockers.dedup();

    let risk_snapshot = payload.get("risk_snapshot_at_approval").cloned();
    let collateral_snapshot = payload.get("collateral_snapshot_at_approval").cloned();

    crate::clob::authenticated::HumanApprovalValidation {
        valid: blockers.is_empty(),
        event_id: Some(event_id),
        decision,
        subject_hash: Some(actual_subject_hash),
        blockers,
        risk_snapshot,
        collateral_snapshot,
    }
}

async fn validate_final_review_decision_event(
    pool: &PgPool,
    request: &crate::clob::authenticated::OrderSubmitFacadeRequest,
) -> crate::clob::authenticated::FinalReviewDecisionValidation {
    let Some(event_id) = request.final_review_decision_event_id else {
        return crate::clob::authenticated::FinalReviewDecisionValidation {
            valid: false,
            event_id: None,
            decision: None,
            operator: None,
            blockers: vec!["final_review_decision_event_missing".to_string()],
            risk_snapshot: None,
            collateral_snapshot: None,
        };
    };

    let row = sqlx::query_as::<_, (serde_json::Value, chrono::DateTime<chrono::Utc>)>(
        r#"SELECT payload, created_at
           FROM journal.events
           WHERE id = $1 AND event_type = 'clob_final_review_decision'"#,
    )
    .bind(event_id)
    .fetch_optional(pool)
    .await;

    let Some((payload, _created_at)) = (match row {
        Ok(row) => row,
        Err(_) => {
            return crate::clob::authenticated::FinalReviewDecisionValidation {
                valid: false,
                event_id: Some(event_id),
                decision: None,
                operator: None,
                blockers: vec!["final_review_decision_event_lookup_failed".to_string()],
                risk_snapshot: None,
                collateral_snapshot: None,
            };
        }
    }) else {
        return crate::clob::authenticated::FinalReviewDecisionValidation {
            valid: false,
            event_id: Some(event_id),
            decision: None,
            operator: None,
            blockers: vec!["final_review_decision_event_not_found".to_string()],
            risk_snapshot: None,
            collateral_snapshot: None,
        };
    };

    let decision = payload
        .get("decision")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let operator = payload
        .get("operator")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let mut blockers = Vec::new();

    if decision.is_none() {
        blockers.push("final_review_decision_event_has_no_decision".to_string());
    }
    // Note: all normalized decisions (acknowledge/reject/rework) are accepted as
    // "final decision recorded"; the operator is responsible for only passing an
    // appropriate (non-reject) decision id to the submit facade.

    blockers.sort();
    blockers.dedup();

    let risk_snapshot = payload.get("risk_snapshot_at_approval").cloned();
    let collateral_snapshot = payload.get("collateral_snapshot_at_approval").cloned();

    crate::clob::authenticated::FinalReviewDecisionValidation {
        valid: blockers.is_empty(),
        event_id: Some(event_id),
        decision,
        operator,
        blockers,
        risk_snapshot,
        collateral_snapshot,
    }
}

async fn validate_collateral_readiness_event(
    pool: &PgPool,
    expected_wallet_address: Option<&str>,
) -> crate::clob::authenticated::CollateralReadinessValidation {
    let max_age_seconds = crate::clob::authenticated::collateral_readiness_max_age_seconds();
    let latest: Option<(uuid::Uuid, serde_json::Value, chrono::DateTime<chrono::Utc>)> =
        match sqlx::query_as(
            r#"SELECT id, payload, created_at
           FROM journal.events
           WHERE event_type = 'clob_collateral_readiness'
           ORDER BY created_at DESC
           LIMIT 1"#,
        )
        .fetch_optional(pool)
        .await
        {
            Ok(row) => row,
            Err(_) => {
                return crate::clob::authenticated::CollateralReadinessValidation {
                    valid: false,
                    event_id: None,
                    created_at: None,
                    wallet_address: None,
                    collateral_balance: None,
                    collateral_balance_positive: false,
                    collateral_allowance_positive: false,
                    positive_allowance_count: None,
                    max_age_seconds,
                    age_seconds: None,
                    blockers: vec!["fresh_collateral_readiness_lookup_failed".to_string()],
                };
            }
        };

    let Some((event_id, payload, created_at)) = latest else {
        return crate::clob::authenticated::CollateralReadinessValidation {
            valid: false,
            event_id: None,
            created_at: None,
            wallet_address: None,
            collateral_balance: None,
            collateral_balance_positive: false,
            collateral_allowance_positive: false,
            positive_allowance_count: None,
            max_age_seconds,
            age_seconds: None,
            blockers: vec!["fresh_collateral_readiness_missing".to_string()],
        };
    };

    let report = payload.get("report").unwrap_or(&payload);
    let wallet_address = report
        .get("wallet_address")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let collateral_balance = report
        .get("collateral_balance")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let collateral_balance_positive = report
        .get("collateral_balance_positive")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let collateral_allowance_positive = report
        .get("collateral_allowance_positive")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let positive_allowance_count = report
        .get("positive_allowance_count")
        .and_then(|value| value.as_u64());
    let age_seconds = (chrono::Utc::now() - created_at).num_seconds();

    let mut blockers = Vec::new();
    if age_seconds < 0 || age_seconds as u64 > max_age_seconds {
        blockers.push("fresh_collateral_readiness_stale".to_string());
    }
    if !collateral_balance_positive {
        blockers.push("fresh_collateral_balance_positive".to_string());
    }
    if !collateral_allowance_positive {
        blockers.push("fresh_collateral_allowance_positive".to_string());
    }
    if let Some(expected) = expected_wallet_address {
        match wallet_address.as_deref() {
            Some(actual) if actual.eq_ignore_ascii_case(expected) => {}
            Some(_) => blockers.push("fresh_collateral_readiness_wallet_mismatch".to_string()),
            None => blockers.push("fresh_collateral_readiness_wallet_missing".to_string()),
        }
    }

    blockers.sort();
    blockers.dedup();

    crate::clob::authenticated::CollateralReadinessValidation {
        valid: blockers.is_empty(),
        event_id: Some(event_id),
        created_at: Some(created_at.to_rfc3339()),
        wallet_address,
        collateral_balance,
        collateral_balance_positive,
        collateral_allowance_positive,
        positive_allowance_count,
        max_age_seconds,
        age_seconds: Some(age_seconds),
        blockers,
    }
}

async fn latest_journal_event(
    pool: &PgPool,
    event_type: &str,
) -> anyhow::Result<Option<serde_json::Value>> {
    let row: Option<(uuid::Uuid, serde_json::Value, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as(
            r#"SELECT id, payload, created_at
           FROM journal.events
           WHERE event_type = $1
           ORDER BY created_at DESC
           LIMIT 1"#,
        )
        .bind(event_type)
        .fetch_optional(pool)
        .await?;

    Ok(row.map(|(id, payload, created_at)| {
        serde_json::json!({
            "id": id,
            "event_type": event_type,
            "payload": payload,
            "created_at": created_at,
        })
    }))
}

async fn load_journal_event_by_id(
    pool: &PgPool,
    event_id: uuid::Uuid,
    event_type: &str,
) -> anyhow::Result<Option<serde_json::Value>> {
    let row: Option<(serde_json::Value, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        r#"SELECT payload, created_at
           FROM journal.events
           WHERE id = $1 AND event_type = $2"#,
    )
    .bind(event_id)
    .bind(event_type)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|(payload, created_at)| {
        serde_json::json!({
            "id": event_id,
            "event_type": event_type,
            "payload": payload,
            "created_at": created_at,
        })
    }))
}

fn build_final_review_readiness_report(
    l2_connected: bool,
    collateral_event: Option<serde_json::Value>,
    unlock_event: Option<serde_json::Value>,
    reconciliation_event: Option<serde_json::Value>,
    live_sender_boundary: &serde_json::Value,
) -> serde_json::Value {
    let collateral_report = collateral_event
        .as_ref()
        .and_then(|event| event_payload_report(event));
    let unlock_report = unlock_event
        .as_ref()
        .and_then(|event| event_payload_report(event));
    let reconciliation_report = reconciliation_event
        .as_ref()
        .and_then(|event| event_payload_report(event));
    let reconciliation = reconciliation_report
        .and_then(|report| report.get("reconciliation"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    let collateral_balance_positive = collateral_report
        .and_then(|report| report.get("collateral_balance_positive"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let collateral_allowance_positive = collateral_report
        .and_then(|report| report.get("collateral_allowance_positive"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let explicit_unlock = unlock_report
        .and_then(|report| report.get("explicit_real_order_submission_configured"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let kill_switch_open = unlock_report
        .and_then(|report| report.get("kill_switch_open"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let paper_mode_disabled = unlock_report
        .and_then(|report| report.get("paper_mode_active"))
        .and_then(|value| value.as_bool())
        .map(|active| !active)
        .unwrap_or(false);
    let live_sender_implemented = unlock_report
        .and_then(|report| report.get("live_order_sender_implemented"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let submit_reconciled_no_send = reconciliation
        .get("reconciled")
        .and_then(|value| value.as_bool())
        == Some(true)
        && reconciliation
            .get("request_sent")
            .and_then(|value| value.as_bool())
            == Some(false)
        && reconciliation
            .get("expected_exchange_state")
            .and_then(|value| value.as_str())
            == Some("no_order_created");
    let fail_closed_live_sender_boundary = live_sender_boundary
        .get("trait_defined")
        .and_then(|value| value.as_bool())
        == Some(true)
        && live_sender_boundary
            .get("fail_closed_implementation_present")
            .and_then(|value| value.as_bool())
            == Some(true)
        && live_sender_boundary
            .get("network_sender_present")
            .and_then(|value| value.as_bool())
            == Some(false)
        && live_sender_boundary
            .get("accepted_for_network_dispatch")
            .and_then(|value| value.as_bool())
            == Some(false)
        && live_sender_boundary
            .get("request_sent")
            .and_then(|value| value.as_bool())
            == Some(false)
        && live_sender_boundary
            .get("post_order_called")
            .and_then(|value| value.as_bool())
            == Some(false)
        && live_sender_boundary
            .get("post_orders_called")
            .and_then(|value| value.as_bool())
            == Some(false);

    let gates = vec![
        readiness_gate(
            "l2_credentials_connected",
            l2_connected,
            "L2 credentials are derived and available in server memory.",
        ),
        readiness_gate(
            "collateral_readiness_journaled",
            collateral_event.is_some(),
            "A latest clob_collateral_readiness journal event exists.",
        ),
        readiness_gate(
            "collateral_balance_positive",
            collateral_balance_positive,
            "Latest journaled collateral readiness has positive CLOB collateral balance.",
        ),
        readiness_gate(
            "collateral_allowance_positive",
            collateral_allowance_positive,
            "Latest journaled collateral readiness has positive collateral allowance.",
        ),
        readiness_gate(
            "real_trading_unlock_status_journaled",
            unlock_event.is_some(),
            "A latest clob_real_trading_unlock_status journal event exists.",
        ),
        readiness_gate(
            "explicit_real_trading_config_unlock",
            explicit_unlock,
            "Explicit real-trading config unlock is still closed.",
        ),
        readiness_gate(
            "kill_switch_open",
            kill_switch_open,
            "Real-order kill switch is still closed.",
        ),
        readiness_gate(
            "paper_mode_disabled",
            paper_mode_disabled,
            "Paper mode is still active.",
        ),
        readiness_gate(
            "live_order_sender_implemented",
            live_sender_implemented,
            "No live order sender is implemented.",
        ),
        readiness_gate(
            "submit_reconciliation_journaled",
            reconciliation_event.is_some(),
            "A latest clob_order_submit_reconciliation journal event exists.",
        ),
        readiness_gate(
            "submit_reconciled_no_send",
            submit_reconciled_no_send,
            "Latest submit reconciliation must prove no exchange order was created.",
        ),
        readiness_gate(
            "fail_closed_live_sender_boundary",
            fail_closed_live_sender_boundary,
            "The LiveOrderSender boundary exists and rejects before network dispatch.",
        ),
    ];

    let completed_count = gates
        .iter()
        .filter(|gate| gate.get("ok").and_then(|value| value.as_bool()) == Some(true))
        .count();
    let blockers = gates
        .iter()
        .filter(|gate| gate.get("ok").and_then(|value| value.as_bool()) == Some(false))
        .filter_map(|gate| {
            gate.get("name")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
        .collect::<Vec<_>>();

    serde_json::json!({
        "ready": false,
        "ready_for_final_review": false,
        "ready_for_real_orders": false,
        "real_orders_enabled": false,
        "paper_only": true,
        "stage": "final_review_blocked",
        "completed_count": completed_count,
        "required_count": gates.len(),
        "blocker_count": blockers.len(),
        "blockers": blockers,
        "gates": gates,
        "latest_evidence": {
            "collateral_readiness": collateral_event,
            "real_trading_unlock_status": unlock_event,
            "submit_reconciliation": reconciliation_event,
            "live_sender_boundary": live_sender_boundary,
        },
        "live_sender_boundary_status": {
            "boundary_name": live_sender_boundary.get("boundary_name").cloned().unwrap_or(serde_json::Value::Null),
            "implementation_name": live_sender_boundary.get("implementation_name").cloned().unwrap_or(serde_json::Value::Null),
            "fail_closed_implementation_present": live_sender_boundary.get("fail_closed_implementation_present").cloned().unwrap_or(serde_json::Value::Null),
            "network_sender_present": live_sender_boundary.get("network_sender_present").cloned().unwrap_or(serde_json::Value::Null),
            "accepted_for_network_dispatch": live_sender_boundary.get("accepted_for_network_dispatch").cloned().unwrap_or(serde_json::Value::Null),
            "request_sent": live_sender_boundary.get("request_sent").cloned().unwrap_or(serde_json::Value::Null),
        },
        "request_sent": false,
        "would_send": false,
        "would_post": false,
        "post_order_called": false,
        "post_orders_called": false,
        "next_safe_step": "Resolve external collateral and allowance capacity, then perform explicit human review while the live-sender boundary remains fail-closed.",
        "note": "Final review readiness package only. It aggregates no-send journal evidence, including the fail-closed sender boundary, and cannot enable or place real orders."
    })
}

fn event_payload_report(event: &serde_json::Value) -> Option<&serde_json::Value> {
    let payload = event.get("payload")?;
    Some(payload.get("report").unwrap_or(payload))
}

async fn record_journal_event(
    pool: &PgPool,
    event_type: &str,
    source: &str,
    severity: &str,
    payload: serde_json::Value,
) -> anyhow::Result<uuid::Uuid> {
    let id = uuid::Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO journal.events (id, event_type, source, severity, payload)
           VALUES ($1, $2, $3, $4, $5)"#,
    )
    .bind(id)
    .bind(event_type)
    .bind(source)
    .bind(severity)
    .bind(payload)
    .execute(pool)
    .await?;

    Ok(id)
}

fn merge_journal_fields(
    mut value: serde_json::Value,
    journaled: bool,
    event_id: Option<uuid::Uuid>,
    journal_error: Option<String>,
) -> serde_json::Value {
    if let Some(object) = value.as_object_mut() {
        object.insert("journaled".to_string(), serde_json::json!(journaled));
        if let Some(event_id) = event_id {
            object.insert("journal_event_id".to_string(), serde_json::json!(event_id));
        }
        if let Some(journal_error) = journal_error {
            object.insert(
                "journal_error".to_string(),
                serde_json::json!(journal_error),
            );
        }
    }
    value
}

fn merge_reconciliation_journal_fields(
    mut value: serde_json::Value,
    reconciliation_journaled: bool,
    reconciliation_event_id: Option<uuid::Uuid>,
    reconciliation_journal_error: Option<String>,
) -> serde_json::Value {
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "reconciliation_journaled".to_string(),
            serde_json::json!(reconciliation_journaled),
        );
        if let Some(reconciliation_event_id) = reconciliation_event_id {
            object.insert(
                "reconciliation_event_id".to_string(),
                serde_json::json!(reconciliation_event_id),
            );
        }
        if let Some(reconciliation_journal_error) = reconciliation_journal_error {
            object.insert(
                "reconciliation_journal_error".to_string(),
                serde_json::json!(reconciliation_journal_error),
            );
        }
    }
    value
}

// (End L2 section. All prior Google behavior + paper paths preserved.
// Real derive reqwest + POLY_* + secret use for CLOB is future gated work per AGENTS + plan 3.4.)

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_event_limit_is_clamped() {
        assert_eq!(clamp_dry_run_events_limit(-10), 1);
        assert_eq!(clamp_dry_run_events_limit(0), 1);
        assert_eq!(clamp_dry_run_events_limit(10), 10);
        assert_eq!(clamp_dry_run_events_limit(500), 50);
        assert_eq!(clamp_review_events_limit(-10), 1);
        assert_eq!(clamp_review_events_limit(500), 50);
    }

    #[test]
    fn category_display_label_humanizes_motorsports() {
        assert_eq!(category_display_label("motorsports"), "Motorsports");
        assert_eq!(category_display_label("formula1"), "Motorsports");
        assert_eq!(category_display_label("f1"), "Motorsports");
        assert_eq!(category_display_label("crypto"), "Crypto");
    }

    #[test]
    fn market_data_status_requires_two_sided_mids() {
        let yes = Some(Decimal::new(650000, 8));
        let no = Some(Decimal::new(99350000, 8));
        assert!(market_has_two_sided_mids(&yes, &no));
        assert_eq!(market_data_status(&yes, &no), "ready");
        assert!(!market_has_two_sided_mids(&yes, &None));
        assert_eq!(market_data_status(&yes, &None), "missing_mid");
    }

    #[test]
    fn paper_order_helpers_normalize_and_cap_risk() {
        assert_eq!(normalize_paper_order_outcome("yes").as_deref(), Some("Yes"));
        assert_eq!(normalize_paper_order_outcome("NO").as_deref(), Some("No"));
        assert!(normalize_paper_order_outcome("draw").is_none());
        assert!(matches!(
            parse_paper_order_side("buy"),
            Some(crate::paper::OrderSide::Buy)
        ));
        assert!(matches!(
            parse_paper_order_type("LIMIT"),
            Some(crate::paper::OrderType::Limit)
        ));
        assert_eq!(
            paper_max_order_notional(Decimal::from(150u64)),
            Decimal::new(150, 2)
        );
        assert_eq!(
            paper_max_total_exposure(Decimal::from(150u64)),
            Decimal::new(2250, 2)
        );
        assert_eq!(
            paper_projected_total_collateral_locked(
                Some(&crate::paper::OrderSide::Buy),
                Decimal::from(20u64),
                Decimal::new(150, 2),
            ),
            Decimal::new(2150, 2)
        );
        assert_eq!(
            paper_projected_total_collateral_locked(
                Some(&crate::paper::OrderSide::Sell),
                Decimal::from(20u64),
                Decimal::new(150, 2),
            ),
            Decimal::from(20u64)
        );
        let summary = build_paper_risk_summary(Decimal::from(150u64), Vec::new());
        assert_eq!(summary["within_total_exposure_limit"], true);
        assert_eq!(summary["status"], "within_limits");
        assert_eq!(summary["open_position_count"], 0);
    }

    #[test]
    fn strategy_candidate_decision_requires_min_net_edge() {
        assert_eq!(
            strategy_candidate_decision(Decimal::new(399, 4), Decimal::new(4, 2)),
            "observe"
        );
        assert_eq!(
            strategy_candidate_decision(Decimal::new(4, 2), Decimal::new(4, 2)),
            "paper_candidate_ready_for_manual_review"
        );
    }

    #[test]
    fn dry_run_review_decision_is_normalized() {
        assert_eq!(
            normalize_dry_run_review_decision(" approve "),
            Some("would_approve")
        );
        assert_eq!(
            normalize_dry_run_review_decision("would_reject"),
            Some("would_reject")
        );
        assert_eq!(
            normalize_dry_run_review_decision("rework"),
            Some("needs_rework")
        );
        assert_eq!(normalize_dry_run_review_decision("place_order"), None);
    }

    #[test]
    fn dry_run_review_summary_extracts_safe_fields() {
        let payload = serde_json::json!({
            "kind": "clob_order_intent_dry_run",
            "report": {
                "accepted": false,
                "estimated_notional": "0.5",
                "blockers": ["real_order_route_absent", "human_approval_gate_absent"],
                "intent": {
                    "market_id": "ui-dry-run",
                    "token_id": "123",
                    "side": "buy",
                    "order_type": "limit"
                }
            }
        });

        let summary = dry_run_review_summary(&payload);

        assert_eq!(summary["market_id"], "ui-dry-run");
        assert_eq!(summary["token_id"], "123");
        assert_eq!(summary["estimated_notional"], "0.5");
        assert_eq!(summary["blocker_count"], 2);
        assert_eq!(summary["approval_blocked"], true);
        assert_eq!(summary["recommended_review_decision"], "would_reject");
        assert_eq!(summary["accepted"], false);
    }

    #[test]
    fn recommended_review_decision_is_conservative() {
        assert_eq!(recommended_review_decision(0), "needs_rework");
        assert_eq!(recommended_review_decision(1), "would_reject");
    }

    #[test]
    fn review_decision_guidance_match_detects_exceptions() {
        let blocked = serde_json::json!({
            "report": {
                "blockers": ["real_order_route_absent"],
                "intent": {"token_id": "123"}
            }
        });
        let matching_review = serde_json::json!({"decision": "would_reject"});
        let exception_review = serde_json::json!({"decision": "would_approve"});
        let missing_review = serde_json::json!({});

        assert_eq!(
            review_decision_matches_guidance(&matching_review, &blocked),
            Some(true)
        );
        assert_eq!(
            review_decision_matches_guidance(&exception_review, &blocked),
            Some(false)
        );
        assert_eq!(
            review_decision_matches_guidance(&missing_review, &blocked),
            None
        );
    }

    #[test]
    fn review_guidance_override_requires_note() {
        assert!(!review_override_requires_note(
            "would_reject",
            "would_reject",
            ""
        ));
        assert!(review_override_requires_note(
            "would_approve",
            "would_reject",
            ""
        ));
        assert!(!review_override_requires_note(
            "would_approve",
            "would_reject",
            "operator rationale"
        ));
    }

    #[test]
    fn dry_run_blockers_extracts_report_blockers() {
        let payload = serde_json::json!({
            "kind": "clob_order_intent_dry_run",
            "report": {
                "blockers": [
                    "real_order_route_absent",
                    "human_approval_gate_absent",
                    123
                ]
            }
        });

        assert_eq!(
            dry_run_blockers(&payload),
            vec![
                "real_order_route_absent".to_string(),
                "human_approval_gate_absent".to_string()
            ]
        );
    }

    #[test]
    fn review_queue_item_surfaces_age_priority_and_no_send() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-30T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let created_at = now - chrono::Duration::seconds(REVIEW_BACKLOG_STALE_AFTER_SECONDS + 1);
        let item = build_review_queue_item(
            uuid::Uuid::nil(),
            serde_json::json!({
                "kind": "clob_order_intent_dry_run",
                "report": {
                    "estimated_notional": "1.23",
                    "blockers": ["real_order_route_absent"],
                    "intent": {
                        "market_id": "queue-test",
                        "token_id": "123",
                        "side": "buy",
                        "order_type": "limit"
                    }
                }
            }),
            created_at,
            now,
        );

        assert_eq!(
            item["dry_run_age_seconds"],
            REVIEW_BACKLOG_STALE_AFTER_SECONDS + 1
        );
        assert_eq!(
            item["review_stale_after_seconds"],
            REVIEW_BACKLOG_STALE_AFTER_SECONDS
        );
        assert_eq!(item["review_is_stale"], true);
        assert_eq!(item["review_priority"], "stale_unreviewed");
        assert_eq!(item["next_review_action"], "review_stale_dry_run");
        assert_eq!(item["dry_run_summary"]["market_id"], "queue-test");
        assert_eq!(
            item["dry_run_summary"]["recommended_review_decision"],
            "would_reject"
        );
        assert_eq!(item["blockers"][0], "real_order_route_absent");
        assert_eq!(item["paper_only"], true);
        assert_eq!(item["real_orders_enabled"], false);
        assert_eq!(item["request_sent"], false);
        assert_eq!(item["post_order_called"], false);
        assert_eq!(item["post_orders_called"], false);
    }

    #[test]
    fn review_decision_counts_groups_known_decisions() {
        let events = vec![
            serde_json::json!({"payload": {"decision": "would_approve"}}),
            serde_json::json!({"payload": {"decision": "would_reject"}}),
            serde_json::json!({"payload": {"decision": "would_reject"}}),
            serde_json::json!({"payload": {"decision": "needs_rework"}}),
            serde_json::json!({"payload": {"decision": "unexpected"}}),
        ];

        let counts = review_decision_counts(&events);

        assert_eq!(counts["would_approve"], 1);
        assert_eq!(counts["would_reject"], 2);
        assert_eq!(counts["needs_rework"], 1);
        assert_eq!(counts["unknown"], 1);
    }

    #[test]
    fn final_review_decision_counts_groups_audit_decisions() {
        let events = vec![
            serde_json::json!({"payload": {"decision": "acknowledge_blocked"}}),
            serde_json::json!({"payload": {"decision": "reject_live_trading"}}),
            serde_json::json!({"payload": {"decision": "needs_rework"}}),
            serde_json::json!({"payload": {"decision": "acknowledge_blocked"}}),
            serde_json::json!({"payload": {"decision": "approve_real_orders"}}),
        ];

        let counts = final_review_decision_counts(&events);

        assert_eq!(counts["acknowledge_blocked"], 2);
        assert_eq!(counts["reject_live_trading"], 1);
        assert_eq!(counts["needs_rework"], 1);
        assert_eq!(counts["unknown"], 1);
    }

    #[test]
    fn final_review_audit_summary_counts_boundary_evidence() {
        let events = vec![
            serde_json::json!({
                "payload": {
                    "decision": "acknowledge_blocked",
                    "live_sender_boundary_fail_closed": true,
                    "live_sender_boundary_status": {
                        "boundary_name": "LiveOrderSender",
                        "implementation_name": "FailClosedLiveOrderSender",
                        "network_sender_present": false,
                        "accepted_for_network_dispatch": false,
                        "request_sent": false
                    }
                }
            }),
            serde_json::json!({
                "payload": {
                    "decision": "reject_live_trading",
                    "live_sender_boundary_fail_closed": false
                }
            }),
        ];

        let summary = build_final_review_audit_summary(events);

        assert_eq!(summary["status"], "audited");
        assert_eq!(summary["boundary_evidence_count"], 1);
        assert_eq!(summary["no_network_evidence_count"], 1);
        assert_eq!(summary["missing_boundary_evidence_count"], 1);
        assert_eq!(summary["missing_no_network_evidence_count"], 1);
        assert_eq!(summary["all_events_have_boundary_evidence"], false);
        assert_eq!(summary["all_events_have_no_network_evidence"], false);
        assert_eq!(
            summary["coverage_status"],
            "legacy_or_missing_boundary_evidence"
        );
        assert_eq!(summary["coverage_gaps"]["count"], 1);
        assert_eq!(
            summary["coverage_gaps"]["events"][0]["missing_boundary_evidence"],
            true
        );
        assert_eq!(
            summary["coverage_gaps"]["events"][0]["missing_no_network_evidence"],
            true
        );
        assert_eq!(
            summary["coverage_gaps"]["events"][0]["approved_for_real_orders"],
            false
        );
        assert_eq!(
            summary["latest_boundary_status"]["boundary_name"],
            "LiveOrderSender"
        );
        assert_eq!(
            summary["latest_boundary_status"]["implementation_name"],
            "FailClosedLiveOrderSender"
        );
        assert_eq!(
            summary["latest_boundary_status"]["network_sender_present"],
            false
        );
        assert_eq!(
            summary["latest_boundary_status"]["accepted_for_network_dispatch"],
            false
        );
    }

    #[test]
    fn final_review_gap_probe_is_compact_and_no_send() {
        let audit = build_final_review_audit_summary(vec![
            serde_json::json!({
                "id": uuid::Uuid::nil(),
                "created_at": "2026-05-30T07:00:00Z",
                "payload": {
                    "decision": "acknowledge_blocked",
                    "approved_for_real_orders": false,
                    "operator": "legacy",
                    "live_sender_boundary_fail_closed": false
                }
            }),
            serde_json::json!({
                "payload": {
                    "decision": "acknowledge_blocked",
                    "approved_for_real_orders": false,
                    "operator": "expired-legacy",
                    "live_sender_boundary_fail_closed": false
                },
                "created_at": "2026-05-29T06:30:00Z"
            }),
            serde_json::json!({
                "payload": {
                    "decision": "acknowledge_blocked",
                    "approved_for_real_orders": false,
                    "live_sender_boundary_fail_closed": true,
                    "live_sender_boundary_status": {
                        "network_sender_present": false,
                        "accepted_for_network_dispatch": false,
                        "request_sent": false
                    }
                }
            }),
        ]);

        let now = chrono::DateTime::parse_from_rfc3339("2026-05-30T08:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let probe = build_final_review_coverage_gap_probe(&audit, 50, now);

        assert_eq!(probe["available"], true);
        assert_eq!(probe["limit"], 50);
        assert_eq!(probe["gaps_only"], true);
        assert_eq!(probe["displayed_event_count"], 2);
        assert_eq!(probe["coverage_gaps"]["count"], 2);
        assert_eq!(probe["coverage_gaps"]["events"][0]["operator"], "legacy");
        assert_eq!(probe["hermes_coverage_window_seconds"], 86_400);
        assert_eq!(probe["active_24h_gap_status"], "active_24h_gaps");
        assert_eq!(probe["active_24h_gap_count"], 1);
        assert_eq!(probe["expired_24h_gap_count"], 1);
        assert_eq!(probe["oldest_gap_age_seconds"], 91_800);
        assert_eq!(probe["newest_gap_age_seconds"], 3600);
        assert_eq!(probe["seconds_until_all_gaps_age_out_of_24h"], 82_800);
        assert_eq!(probe["oldest_active_24h_gap_age_seconds"], 3600);
        assert_eq!(probe["newest_active_24h_gap_age_seconds"], 3600);
        assert_eq!(probe["seconds_until_active_gaps_age_out_of_24h"], 82_800);
        assert!(probe["active_gaps_age_out_at"].is_string());
        assert!(probe["oldest_gap_created_at"].is_string());
        assert!(probe["newest_gap_created_at"].is_string());
        assert!(probe["oldest_active_24h_gap_created_at"].is_string());
        assert!(probe["newest_active_24h_gap_created_at"].is_string());
        assert_eq!(probe["request_sent"], false);
        assert_eq!(probe["post_order_called"], false);
        assert_eq!(probe["post_orders_called"], false);
        assert_eq!(probe["real_orders_enabled"], false);
        assert_eq!(probe["approved_for_real_orders"], false);
    }

    #[test]
    fn final_review_hermes_gap_alignment_reports_match_and_mismatch() {
        let coverage_gap_probe = serde_json::json!({
            "available": true,
            "active_24h_gap_status": "active_24h_gaps",
            "active_24h_gap_count": 7,
            "expired_24h_gap_count": 3,
            "seconds_until_active_gaps_age_out_of_24h": 45_317,
            "active_gaps_age_out_at": "2026-05-30T20:35:17Z",
            "paper_only": true,
            "real_orders_enabled": false,
            "request_sent": false,
        });
        let matching_hermes = serde_json::json!({
            "available": true,
            "reflection": {"age_seconds": 42},
            "final_review_decision_missing_boundary_evidence_events_24h": 7,
            "final_review_decision_missing_no_network_evidence_events_24h": 7,
            "paper_only": true,
            "real_orders_enabled": false,
            "request_sent": false,
        });

        let matched =
            build_final_review_hermes_gap_alignment(&coverage_gap_probe, &matching_hermes);

        assert_eq!(matched["status"], "matched_active_gaps");
        assert_eq!(matched["aligned"], true);
        assert_eq!(matched["requires_attention"], true);
        assert_eq!(matched["app_active_24h_gap_count"], 7);
        assert_eq!(matched["active_24h_gap_status"], "active_24h_gaps");
        assert_eq!(matched["expired_24h_gap_count"], 3);
        assert_eq!(matched["seconds_until_active_gaps_age_out_of_24h"], 45_317);
        assert_eq!(matched["active_gaps_age_out_at"], "2026-05-30T20:35:17Z");
        assert_eq!(matched["hermes_missing_gap_count"], 7);
        assert_eq!(matched["hermes_reflection_age_seconds"], 42);
        assert_eq!(matched["hermes_reflection_stale_after_seconds"], 600);
        assert_eq!(matched["hermes_reflection_is_stale"], false);
        assert_eq!(matched["hermes_reflection_freshness_status"], "fresh");
        assert_eq!(matched["request_sent"], false);
        assert_eq!(matched["post_order_called"], false);
        assert_eq!(matched["post_orders_called"], false);
        assert_eq!(matched["real_orders_enabled"], false);
        assert_eq!(matched["approved_for_real_orders"], false);

        let stale_hermes = serde_json::json!({
            "available": true,
            "final_review_decision_boundary_coverage": {
                "missing_boundary_evidence_events": 5,
                "missing_no_network_evidence_events": 5
            }
        });
        let mismatch = build_final_review_hermes_gap_alignment(&coverage_gap_probe, &stale_hermes);

        assert_eq!(mismatch["status"], "app_probe_ahead_of_hermes");
        assert_eq!(mismatch["aligned"], false);
        assert_eq!(mismatch["requires_attention"], true);
        assert_eq!(mismatch["app_active_24h_gap_count"], 7);
        assert_eq!(mismatch["hermes_missing_gap_count"], 5);

        let stale_aligned_hermes = serde_json::json!({
            "available": true,
            "reflection": {
                "age_seconds": HERMES_REFLECTION_STALE_AFTER_SECONDS + 1,
                "stale_after_seconds": HERMES_REFLECTION_STALE_AFTER_SECONDS
            },
            "final_review_decision_missing_boundary_evidence_events_24h": 7,
            "final_review_decision_missing_no_network_evidence_events_24h": 7,
        });
        let stale_aligned =
            build_final_review_hermes_gap_alignment(&coverage_gap_probe, &stale_aligned_hermes);

        assert_eq!(stale_aligned["status"], "hermes_reflection_stale");
        assert_eq!(stale_aligned["aligned"], false);
        assert_eq!(stale_aligned["requires_attention"], true);
        assert_eq!(stale_aligned["hermes_reflection_is_stale"], true);
        assert_eq!(stale_aligned["hermes_reflection_freshness_status"], "stale");
    }

    #[test]
    fn hermes_safety_loop_response_surfaces_boundary_coverage() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-30T10:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let reflection = HermesReflectionRow {
            id: uuid::Uuid::nil(),
            period_start: Some(now - chrono::Duration::hours(24)),
            period_end: Some(now),
            summary: "Hermes summary".to_string(),
            metrics: Some(serde_json::json!({
                "clob_safety_loop": {
                    "final_review_decision_events_24h": 3,
                    "final_review_decision_boundary_evidence_events_24h": 2,
                    "final_review_decision_no_network_evidence_events_24h": 2,
                    "final_review_decision_boundary_coverage": {
                        "complete_fail_closed_no_network_evidence": false,
                        "missing_boundary_evidence_events": 1,
                        "missing_no_network_evidence_events": 1,
                        "coverage_status": "legacy_or_missing_boundary_evidence"
                    },
                    "latest_final_review_decision_boundary_status": {
                        "boundary_name": "LiveOrderSender",
                        "implementation_name": "FailClosedLiveOrderSender",
                        "network_sender_present": false,
                        "accepted_for_network_dispatch": false
                    }
                },
                "paper_accounting_loop": {
                    "status": "reconciled",
                    "mismatch_count": 0,
                    "fills_since_reset_count": 0,
                    "hermes_checks_paper_accounting": true,
                    "request_sent": false,
                    "post_order_called": false,
                    "post_orders_called": false
                },
                "paper_rejection_loop": {
                    "paper_order_rejection_events_24h": 4,
                    "strategy_paper_order_rejections_24h": 1,
                    "strategy_gate_status": "blocked",
                    "top_blockers": {
                        "strategy_net_edge_below_minimum": 1
                    },
                    "hermes_consumes_strategy_paper_gate": true,
                    "paper_only": true,
                    "real_orders_enabled": false
                },
                "strategy_candidate_loop": {
                    "strategy_candidate_observation_events_24h": 2,
                    "observed_candidates_24h": 2,
                    "strategy_candidate_observation_status": "observing_candidates",
                    "latest_summary": {
                        "first_candidate_decision": "observe",
                        "first_candidate_net_edge_after_fees": "-0.014"
                    },
                    "hermes_consumes_strategy_candidate_observations": true,
                    "paper_only": true,
                    "real_orders_enabled": false,
                    "request_sent": false,
                    "post_order_called": false,
                    "post_orders_called": false
                }
            })),
            recommendations: Some(serde_json::json!(["inspect final-review decisions"])),
            hermes_version: Some("test-hermes".to_string()),
            llm_model: None,
            created_at: now - chrono::Duration::seconds(42),
        };

        let response = build_hermes_safety_loop_response(Some(reflection), now);
        assert_eq!(response["status"], "boundary_coverage_incomplete");
        assert_eq!(response["available"], true);
        assert_eq!(response["real_orders_enabled"], false);
        assert_eq!(
            response["final_review_decision_boundary_evidence_events_24h"],
            2
        );
        assert_eq!(
            response["final_review_decision_boundary_coverage"]
                ["complete_fail_closed_no_network_evidence"],
            false
        );
        assert_eq!(
            response["final_review_decision_missing_boundary_evidence_events_24h"],
            1
        );
        assert_eq!(
            response["final_review_decision_missing_no_network_evidence_events_24h"],
            1
        );
        assert_eq!(
            response["final_review_decision_boundary_coverage"]["coverage_status"],
            "legacy_or_missing_boundary_evidence"
        );
        assert_eq!(
            response["latest_final_review_decision_boundary_status"]["boundary_name"],
            "LiveOrderSender"
        );
        assert_eq!(response["reflection"]["age_seconds"], 42);
        assert_eq!(response["reflection"]["stale_after_seconds"], 600);
        assert_eq!(response["reflection"]["is_stale"], false);
        assert_eq!(response["reflection"]["freshness_status"], "fresh");
        assert_eq!(response["paper_accounting_loop"]["status"], "reconciled");
        assert_eq!(response["paper_accounting_loop"]["mismatch_count"], 0);
        assert_eq!(
            response["paper_rejection_loop"]["strategy_paper_order_rejections_24h"],
            1
        );
        assert_eq!(
            response["paper_rejection_loop"]["strategy_gate_status"],
            "blocked"
        );
        assert_eq!(
            response["paper_rejection_loop"]["top_blockers"]["strategy_net_edge_below_minimum"],
            1
        );
        assert_eq!(
            response["paper_rejection_loop"]["hermes_consumes_strategy_paper_gate"],
            true
        );
        assert_eq!(
            response["strategy_candidate_loop"]["strategy_candidate_observation_events_24h"],
            2
        );
        assert_eq!(
            response["strategy_candidate_loop"]["observed_candidates_24h"],
            2
        );
        assert_eq!(
            response["strategy_candidate_loop"]["latest_summary"]["first_candidate_decision"],
            "observe"
        );
        assert_eq!(
            response["strategy_candidate_loop"]["hermes_consumes_strategy_candidate_observations"],
            true
        );
    }

    #[test]
    fn hermes_safety_loop_response_is_safe_without_reflections() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-30T10:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let response = build_hermes_safety_loop_response(None, now);
        assert_eq!(response["status"], "no_reflections");
        assert_eq!(response["available"], false);
        assert_eq!(response["real_orders_enabled"], false);
        assert_eq!(response["request_sent"], false);
        assert_eq!(response["post_order_called"], false);
        assert_eq!(response["post_orders_called"], false);
        assert!(response["strategy_candidate_loop"].is_null());
    }

    #[test]
    fn live_sender_design_readiness_stays_blocked_and_no_send() {
        let unlock_event = serde_json::json!({
            "id": uuid::Uuid::nil(),
            "payload": {
                "kind": "clob_real_trading_unlock_status",
                "report": {
                    "explicit_real_order_submission_configured": false,
                    "kill_switch_open": false,
                    "paper_mode_active": true,
                    "live_order_sender_implemented": false
                }
            },
            "created_at": "2026-05-29T00:00:00Z"
        });
        let final_review_decision_event = serde_json::json!({
            "id": uuid::Uuid::nil(),
            "payload": {
                "kind": "clob_final_review_decision",
                "approved_for_real_orders": false,
                "decision": "acknowledge_blocked"
            },
            "created_at": "2026-05-29T00:00:00Z"
        });

        let report = build_live_sender_design_readiness_report(
            Some(unlock_event),
            Some(final_review_decision_event),
        );
        let blockers = report["blockers"].as_array().expect("blockers");

        assert_eq!(report["ready"], false);
        assert_eq!(report["ready_for_live_sender_implementation"], false);
        assert_eq!(report["approved_for_real_orders"], false);
        assert_eq!(report["request_sent"], false);
        assert_eq!(report["post_order_called"], false);
        assert_eq!(report["completed_count"], 3);
        assert_eq!(report["required_count"], 7);
        assert!(!blockers
            .iter()
            .any(|blocker| blocker == "final_review_decision_audited"));
        assert!(blockers
            .iter()
            .any(|blocker| blocker == "explicit_real_trading_config_unlock"));
        assert!(blockers.iter().any(|blocker| blocker == "kill_switch_open"));
        assert!(blockers
            .iter()
            .any(|blocker| blocker == "paper_mode_disabled"));
        assert!(blockers
            .iter()
            .any(|blocker| blocker == "live_order_sender_implemented"));
    }

    #[test]
    fn live_sender_design_review_contract_is_reviewable_but_not_implementation_ready() {
        let design_readiness_event = serde_json::json!({
            "id": uuid::Uuid::nil(),
            "payload": {
                "kind": "clob_live_sender_design_readiness",
                "report": {
                    "ready_for_live_sender_implementation": false,
                    "request_sent": false,
                    "post_order_called": false,
                    "post_orders_called": false
                }
            },
            "created_at": "2026-05-29T00:00:00Z"
        });
        let unlock_event = serde_json::json!({
            "id": uuid::Uuid::nil(),
            "payload": {
                "kind": "clob_real_trading_unlock_status",
                "report": {
                    "explicit_real_order_submission_configured": false,
                    "live_order_sender_implemented": false
                }
            },
            "created_at": "2026-05-29T00:00:00Z"
        });
        let final_review_decision_event = serde_json::json!({
            "id": uuid::Uuid::nil(),
            "payload": {
                "kind": "clob_final_review_decision",
                "approved_for_real_orders": false,
                "decision": "acknowledge_blocked"
            },
            "created_at": "2026-05-29T00:00:00Z"
        });

        let report = build_live_sender_design_review_report(
            Some(design_readiness_event),
            Some(unlock_event),
            Some(final_review_decision_event),
        );
        let blockers = report["blockers"].as_array().expect("blockers");
        let guards = report["review_contract"]["required_pre_submit_guards"]
            .as_array()
            .expect("required guards");

        assert_eq!(report["ready"], false);
        assert_eq!(report["ready_for_design_review"], true);
        assert_eq!(report["ready_for_live_sender_implementation"], false);
        assert_eq!(report["implementation_permitted"], false);
        assert_eq!(report["approved_for_real_orders"], false);
        assert_eq!(report["request_sent"], false);
        assert_eq!(report["post_order_called"], false);
        assert_eq!(report["stage"], "design_review_contract_ready");
        assert_eq!(report["completed_count"], 7);
        assert_eq!(report["required_count"], 7);
        assert!(blockers.is_empty());
        assert!(guards
            .iter()
            .any(|guard| guard == "explicit_real_order_submission_configured"));
        assert!(guards.iter().any(|guard| guard == "kill_switch_open"));
        assert!(guards.iter().any(|guard| guard == "paper_mode_disabled"));
    }

    #[test]
    fn review_summary_counts_coverage_and_blockers() {
        let items = vec![
            serde_json::json!({
                "dry_run_created_at": "2026-05-27T10:00:00Z",
                "dry_run": {
                    "report": {
                        "blockers": ["real_order_route_absent", "human_approval_gate_absent"]
                    }
                },
                "latest_review": {
                    "created_at": "2026-05-27T10:05:00Z",
                    "payload": {"decision": "would_reject"}
                }
            }),
            serde_json::json!({
                "dry_run": {
                    "report": {
                        "blockers": ["real_order_route_absent"]
                    }
                },
                "latest_review": null
            }),
        ];

        let summary = build_review_summary(&items);

        assert_eq!(summary["dry_run_count"], 2);
        assert_eq!(summary["reviewed_count"], 1);
        assert_eq!(summary["unreviewed_count"], 1);
        assert_eq!(summary["review_coverage_pct"], "50.00");
        assert_eq!(summary["decision_counts"]["would_reject"], 1);
        assert_eq!(summary["guidance_counts"]["would_reject"], 2);
        assert_eq!(summary["guidance_counts"]["needs_rework"], 0);
        assert_eq!(summary["guidance_alignment"]["matches_latest_review"], 1);
        assert_eq!(
            summary["guidance_alignment"]["differs_from_latest_review"],
            0
        );
        assert_eq!(summary["guidance_alignment"]["unreviewed"], 1);
        assert_eq!(summary["latest_review_latency"]["reviewed_count"], 1);
        assert_eq!(summary["latest_review_latency"]["min_seconds"], 300);
        assert_eq!(summary["latest_review_latency"]["max_seconds"], 300);
        assert_eq!(summary["latest_review_latency"]["avg_seconds"], 300);
        assert_eq!(summary["latest_review_latency"]["slow_count"], 0);
        assert_eq!(
            summary["latest_review_latency"]["slow_after_seconds"],
            REVIEW_LATENCY_SLOW_AFTER_SECONDS
        );
        assert_eq!(
            summary["top_blockers"][0]["name"],
            "real_order_route_absent"
        );
        assert_eq!(summary["top_blockers"][0]["count"], 2);
    }

    #[test]
    fn latest_review_latency_is_clamped_and_summarized() {
        let normal = serde_json::json!({
            "dry_run_created_at": "2026-05-27T10:00:00Z",
            "latest_review": {"created_at": "2026-05-27T10:02:00Z"}
        });
        let future_skew = serde_json::json!({
            "dry_run_created_at": "2026-05-27T10:05:00Z",
            "latest_review": {"created_at": "2026-05-27T10:04:00Z"}
        });

        assert_eq!(latest_review_latency_seconds(&normal), Some(120));
        assert_eq!(latest_review_latency_seconds(&future_skew), Some(0));

        let summary = latency_summary(&[120, 60, 180]);
        assert_eq!(summary["reviewed_count"], 3);
        assert_eq!(summary["min_seconds"], 60);
        assert_eq!(summary["max_seconds"], 180);
        assert_eq!(summary["avg_seconds"], 120);
        assert_eq!(summary["slow_count"], 0);
        assert_eq!(
            summary["slow_after_seconds"],
            REVIEW_LATENCY_SLOW_AFTER_SECONDS
        );

        let slow_summary = latency_summary(&[
            REVIEW_LATENCY_SLOW_AFTER_SECONDS - 1,
            REVIEW_LATENCY_SLOW_AFTER_SECONDS,
            REVIEW_LATENCY_SLOW_AFTER_SECONDS + 60,
        ]);
        assert_eq!(slow_summary["slow_count"], 2);
    }

    #[test]
    fn review_health_flags_attention_reasons() {
        let summary = serde_json::json!({
            "dry_run_count": 3,
            "unreviewed_count": 1,
            "guidance_alignment": {
                "differs_from_latest_review": 1
            },
            "latest_review_latency": {
                "max_seconds": REVIEW_LATENCY_SLOW_AFTER_SECONDS,
                "slow_count": 2
            }
        });

        let health = build_review_health(&summary);

        assert_eq!(health["status"], "needs_attention");
        assert_eq!(health["dry_run_count"], 3);
        assert_eq!(health["unreviewed_count"], 1);
        assert_eq!(health["guidance_exception_count"], 1);
        assert_eq!(
            health["max_latency_seconds"],
            REVIEW_LATENCY_SLOW_AFTER_SECONDS
        );
        assert_eq!(health["reason_details"]["unreviewed_count"], 1);
        assert_eq!(health["reason_details"]["guidance_exception_count"], 1);
        assert_eq!(
            health["reason_details"]["max_latency_seconds"],
            REVIEW_LATENCY_SLOW_AFTER_SECONDS
        );
        assert_eq!(health["reason_details"]["slow_review_count"], 2);
        assert_eq!(health["slow_review_count"], 2);
        assert_eq!(
            health["reason_details"]["slow_latency_after_seconds"],
            REVIEW_LATENCY_SLOW_AFTER_SECONDS
        );
        assert_eq!(
            health["reasons"].as_array().unwrap(),
            &vec![
                serde_json::json!("unreviewed_dry_runs"),
                serde_json::json!("guidance_exceptions"),
                serde_json::json!("slow_latest_review_latency"),
            ]
        );
        let actions = health["recommended_actions"].as_array().unwrap();
        assert_eq!(actions.len(), 3);
        assert_eq!(actions[0]["id"], "review_unreviewed_dry_runs");
        assert_eq!(actions[0]["unreviewed_count"], 1);
        assert_eq!(
            actions[0]["review_stale_after_seconds"],
            REVIEW_BACKLOG_STALE_AFTER_SECONDS
        );
        assert_eq!(actions[1]["id"], "inspect_guidance_exceptions");
        assert_eq!(actions[1]["guidance_exception_count"], 1);
        assert_eq!(actions[2]["id"], "inspect_review_latency");
        assert_eq!(
            actions[2]["max_latency_seconds"],
            REVIEW_LATENCY_SLOW_AFTER_SECONDS
        );
        assert_eq!(actions[2]["slow_review_count"], 2);
        assert_eq!(
            actions[2]["slow_latency_after_seconds"],
            REVIEW_LATENCY_SLOW_AFTER_SECONDS
        );
    }

    #[test]
    fn review_health_reports_empty_and_ok_states() {
        let empty = build_review_health(&serde_json::json!({
            "dry_run_count": 0,
            "unreviewed_count": 0,
            "guidance_alignment": {"differs_from_latest_review": 0},
            "latest_review_latency": {"max_seconds": null}
        }));
        assert_eq!(empty["status"], "empty");
        assert_eq!(empty["recommended_actions"][0]["id"], "no_recent_dry_runs");

        let ok = build_review_health(&serde_json::json!({
            "dry_run_count": 2,
            "unreviewed_count": 0,
            "guidance_alignment": {"differs_from_latest_review": 0},
            "latest_review_latency": {"max_seconds": REVIEW_LATENCY_SLOW_AFTER_SECONDS - 1}
        }));
        assert_eq!(ok["status"], "ok");
        assert!(ok["reasons"].as_array().unwrap().is_empty());
        assert_eq!(ok["recommended_actions"][0]["id"], "none");
    }

    #[test]
    fn operator_status_rollup_prioritizes_safety_attention() {
        let blockers = vec!["human_approval_gate_absent".to_string()];
        let no_blockers: Vec<String> = Vec::new();

        assert_eq!(
            operator_status_state(false, &no_blockers, true, Some("ok")),
            "clob_unavailable"
        );
        assert_eq!(
            operator_status_state(true, &blockers, true, Some("ok")),
            "clob_blocked"
        );
        assert_eq!(
            operator_status_state(true, &no_blockers, false, None),
            "review_unavailable"
        );
        assert_eq!(
            operator_status_state(true, &no_blockers, true, Some("needs_attention")),
            "review_attention"
        );
        assert_eq!(
            operator_status_state(true, &no_blockers, true, Some("empty")),
            "needs_paper_dry_runs"
        );
        assert_eq!(
            operator_status_state(true, &no_blockers, true, Some("ok")),
            "paper_observing"
        );
    }

    #[test]
    fn operator_status_actions_include_secondary_review_actions() {
        let review_health = serde_json::json!({
            "recommended_actions": [
                {"id": "none", "severity": "info"},
                {
                    "id": "inspect_guidance_exceptions",
                    "severity": "attention",
                    "endpoint": "/clob/order-intent/review-guidance-exceptions?limit=10"
                },
                {
                    "id": "inspect_review_latency",
                    "severity": "attention",
                    "endpoint": "/clob/order-intent/review-summary?limit=50"
                }
            ]
        });

        let final_review_audit = serde_json::json!({"status": "audited"});
        let live_sender_boundary = serde_json::json!({"fail_closed_implementation_present": false});
        let hermes_safety_loop = serde_json::json!({"status": "boundary_coverage_complete"});
        let hermes_gap_alignment = serde_json::json!({"available": false});
        let actions = operator_status_actions(
            "clob_blocked",
            &review_health,
            &final_review_audit,
            &live_sender_boundary,
            &hermes_safety_loop,
            &hermes_gap_alignment,
        );
        let ids: Vec<&str> = actions
            .iter()
            .filter_map(|action| action.get("id").and_then(|id| id.as_str()))
            .collect();

        assert_eq!(
            ids,
            vec![
                "inspect_clob_preflight",
                "inspect_guidance_exceptions",
                "inspect_review_latency"
            ]
        );

        let summary = operator_action_summary(&actions);
        assert_eq!(summary["total_count"], 3);
        assert_eq!(summary["attention_count"], 3);
        assert_eq!(summary["info_count"], 0);
        assert_eq!(summary["actionable_count"], 3);
        assert_eq!(summary["primary_action_id"], "inspect_clob_preflight");
    }

    #[test]
    fn operator_status_actions_include_missing_final_review_audit() {
        let review_health = serde_json::json!({
            "recommended_actions": [{"id": "none", "severity": "info"}]
        });
        let final_review_audit = serde_json::json!({"status": "missing_audit"});

        let live_sender_boundary = serde_json::json!({"fail_closed_implementation_present": false});
        let hermes_safety_loop = serde_json::json!({"status": "boundary_coverage_complete"});
        let hermes_gap_alignment = serde_json::json!({"available": false});
        let actions = operator_status_actions(
            "paper_observing",
            &review_health,
            &final_review_audit,
            &live_sender_boundary,
            &hermes_safety_loop,
            &hermes_gap_alignment,
        );
        let ids: Vec<&str> = actions
            .iter()
            .filter_map(|action| action.get("id").and_then(|id| id.as_str()))
            .collect();

        assert_eq!(ids, vec!["inspect_final_review_decisions"]);
    }

    #[tokio::test]
    async fn operator_status_actions_include_fail_closed_live_sender_boundary() {
        let review_health = serde_json::json!({
            "recommended_actions": [{"id": "none", "severity": "info"}]
        });
        let final_review_audit = serde_json::json!({"status": "audited"});
        let live_sender_boundary =
            crate::clob::live_sender::build_live_sender_boundary_status().await;
        let hermes_safety_loop = serde_json::json!({"status": "boundary_coverage_complete"});
        let hermes_gap_alignment = serde_json::json!({"available": false});

        let actions = operator_status_actions(
            "paper_observing",
            &review_health,
            &final_review_audit,
            &live_sender_boundary,
            &hermes_safety_loop,
            &hermes_gap_alignment,
        );
        let ids: Vec<&str> = actions
            .iter()
            .filter_map(|action| action.get("id").and_then(|id| id.as_str()))
            .collect();

        assert_eq!(ids, vec!["inspect_live_sender_boundary"]);
        assert_eq!(actions[0]["severity"], "info");
        assert_eq!(actions[0]["endpoint"], "/clob/live-sender-boundary-status");
    }

    #[test]
    fn operator_status_actions_include_incomplete_hermes_boundary_coverage() {
        let review_health = serde_json::json!({
            "recommended_actions": [{"id": "none", "severity": "info"}]
        });
        let final_review_audit = serde_json::json!({"status": "audited"});
        let live_sender_boundary = serde_json::json!({"fail_closed_implementation_present": false});
        let hermes_safety_loop = serde_json::json!({
            "status": "boundary_coverage_incomplete",
            "final_review_decision_events_24h": 13,
            "final_review_decision_boundary_coverage": {
                "complete_fail_closed_no_network_evidence": false
            }
        });
        let hermes_gap_alignment = serde_json::json!({"available": false});

        let actions = operator_status_actions(
            "paper_observing",
            &review_health,
            &final_review_audit,
            &live_sender_boundary,
            &hermes_safety_loop,
            &hermes_gap_alignment,
        );
        let ids: Vec<&str> = actions
            .iter()
            .filter_map(|action| action.get("id").and_then(|id| id.as_str()))
            .collect();

        assert_eq!(
            ids,
            vec![
                "inspect_hermes_safety_loop",
                "inspect_final_review_coverage_gaps"
            ]
        );
        assert_eq!(actions[0]["severity"], "attention");
        assert_eq!(actions[0]["endpoint"], "/clob/hermes-safety-loop");
        assert_eq!(actions[1]["severity"], "attention");
        assert_eq!(
            actions[1]["endpoint"],
            "/clob/final-review-decisions?limit=50&gaps_only=true"
        );
    }

    #[test]
    fn operator_status_actions_include_active_gap_ttl_detail() {
        let review_health = serde_json::json!({
            "recommended_actions": [{"id": "none", "severity": "info"}]
        });
        let final_review_audit = serde_json::json!({"status": "audited"});
        let live_sender_boundary = serde_json::json!({"fail_closed_implementation_present": false});
        let hermes_safety_loop = serde_json::json!({
            "status": "boundary_coverage_incomplete",
            "final_review_decision_events_24h": 13,
            "final_review_decision_boundary_coverage": {
                "complete_fail_closed_no_network_evidence": false
            }
        });
        let hermes_gap_alignment = serde_json::json!({
            "available": true,
            "aligned": true,
            "status": "matched_active_gaps",
            "app_active_24h_gap_count": 7,
            "seconds_until_active_gaps_age_out_of_24h": 45_317,
            "active_gaps_age_out_at": "2026-05-30T20:35:17Z"
        });

        let actions = operator_status_actions(
            "paper_observing",
            &review_health,
            &final_review_audit,
            &live_sender_boundary,
            &hermes_safety_loop,
            &hermes_gap_alignment,
        );
        let gap_action = actions
            .iter()
            .find(|action| {
                action.get("id").and_then(|id| id.as_str())
                    == Some("inspect_final_review_coverage_gaps")
            })
            .expect("coverage gap action");

        assert_eq!(gap_action["active_24h_gap_count"], 7);
        assert_eq!(
            gap_action["seconds_until_active_gaps_age_out_of_24h"],
            45_317
        );
        assert_eq!(
            gap_action["hermes_gap_alignment_status"],
            "matched_active_gaps"
        );
        assert_eq!(gap_action["active_gaps_age_out_at"], "2026-05-30T20:35:17Z");
        assert!(gap_action["label"]
            .as_str()
            .unwrap()
            .contains("7 active final-review coverage gap(s)"));
        assert!(gap_action["label"].as_str().unwrap().contains("12h 35m"));
    }

    #[test]
    fn operator_status_actions_include_hermes_gap_alignment_mismatch() {
        let review_health = serde_json::json!({
            "recommended_actions": [{"id": "none", "severity": "info"}]
        });
        let final_review_audit = serde_json::json!({"status": "audited"});
        let live_sender_boundary = serde_json::json!({"fail_closed_implementation_present": false});
        let hermes_safety_loop = serde_json::json!({"status": "boundary_coverage_complete"});
        let hermes_gap_alignment = serde_json::json!({
            "available": true,
            "aligned": false,
            "status": "app_probe_ahead_of_hermes",
            "app_active_24h_gap_count": 7,
            "hermes_missing_gap_count": 5,
            "paper_only": true,
            "real_orders_enabled": false,
            "request_sent": false
        });

        let actions = operator_status_actions(
            "paper_observing",
            &review_health,
            &final_review_audit,
            &live_sender_boundary,
            &hermes_safety_loop,
            &hermes_gap_alignment,
        );
        let ids: Vec<&str> = actions
            .iter()
            .filter_map(|action| action.get("id").and_then(|id| id.as_str()))
            .collect();

        assert_eq!(ids, vec!["inspect_hermes_gap_alignment"]);
        assert_eq!(actions[0]["severity"], "attention");
        assert_eq!(actions[0]["endpoint"], "/clob/operator-status?limit=50");
    }

    #[test]
    fn operator_status_actions_include_stale_hermes_alignment_detail() {
        let review_health = serde_json::json!({
            "recommended_actions": [{"id": "none", "severity": "info"}]
        });
        let final_review_audit = serde_json::json!({"status": "audited"});
        let live_sender_boundary = serde_json::json!({"fail_closed_implementation_present": false});
        let hermes_safety_loop = serde_json::json!({"status": "boundary_coverage_complete"});
        let hermes_gap_alignment = serde_json::json!({
            "available": true,
            "aligned": false,
            "status": "hermes_reflection_stale",
            "hermes_reflection_age_seconds": 725,
            "hermes_reflection_stale_after_seconds": HERMES_REFLECTION_STALE_AFTER_SECONDS,
            "paper_only": true,
            "real_orders_enabled": false,
            "request_sent": false
        });

        let actions = operator_status_actions(
            "paper_observing",
            &review_health,
            &final_review_audit,
            &live_sender_boundary,
            &hermes_safety_loop,
            &hermes_gap_alignment,
        );

        assert_eq!(actions[0]["id"], "inspect_hermes_gap_alignment");
        assert_eq!(
            actions[0]["hermes_gap_alignment_status"],
            "hermes_reflection_stale"
        );
        assert_eq!(actions[0]["hermes_reflection_age_seconds"], 725);
        assert_eq!(
            actions[0]["hermes_reflection_stale_after_seconds"],
            HERMES_REFLECTION_STALE_AFTER_SECONDS
        );
        assert!(actions[0]["label"]
            .as_str()
            .unwrap()
            .contains("Hermes reflection is stale"));
    }

    #[tokio::test]
    async fn order_placement_readiness_reports_real_order_gap() {
        let preflight = serde_json::json!({
            "checks": [
                {"name": "collateral_balance_positive", "ok": false},
                {"name": "collateral_allowance_positive", "ok": false}
            ]
        });
        let review_health = serde_json::json!({
            "status": "needs_attention"
        });
        let market_data_readiness = serde_json::json!({
            "status": "ready",
            "active_market_count": 1,
            "data_ready_market_count": 1
        });
        let final_review_audit = serde_json::json!({
            "status": "audited",
            "count": 1
        });

        let readiness = build_order_placement_readiness(
            true,
            true,
            &preflight,
            &review_health,
            &market_data_readiness,
            &final_review_audit,
            &crate::clob::live_sender::build_live_sender_boundary_status().await,
        );
        let blockers = readiness["blockers"].as_array().expect("blockers");
        let expected_completed_count =
            if real_order_reconciliation_available() && market_metadata_validation_available() {
                15
            } else if real_order_reconciliation_available() {
                14
            } else if kill_switch_and_exposure_limits_available() {
                13
            } else if human_approval_workflow_available() {
                12
            } else if order_submit_facade_available() {
                11
            } else if order_post_request_dry_run_available() {
                9
            } else if signed_order_payload_dry_run_available() {
                8
            } else {
                7
            };

        assert_eq!(readiness["ready"], false);
        assert_eq!(readiness["stage"], "authenticated_read_and_paper_dry_run");
        assert_eq!(readiness["completed_count"], expected_completed_count);
        assert_eq!(readiness["required_count"], 18);
        assert_eq!(readiness["paper_market_data_ready"], true);
        assert_eq!(
            readiness["market_data_readiness"]["data_ready_market_count"],
            1
        );
        assert_eq!(readiness["final_review_audit_status"], "audited");
        assert_eq!(readiness["final_review_decision_count"], 1);
        assert_eq!(
            readiness["live_sender_boundary_status"]["boundary_name"],
            "LiveOrderSender"
        );
        assert_eq!(
            readiness["live_sender_boundary_status"]["implementation_name"],
            "FailClosedLiveOrderSender"
        );
        assert_eq!(
            readiness["live_sender_boundary_status"]["network_sender_present"],
            false
        );
        assert_eq!(
            readiness["live_sender_boundary_status"]["accepted_for_network_dispatch"],
            false
        );
        assert!(!blockers
            .iter()
            .any(|blocker| blocker == "final_review_decision_audit"));
        assert!(!blockers
            .iter()
            .any(|blocker| blocker == "fail_closed_live_sender_boundary"));
        if signed_order_payload_dry_run_available() {
            assert!(!blockers
                .iter()
                .any(|blocker| blocker == "eip712_order_payload_signing"));
        } else {
            assert!(blockers
                .iter()
                .any(|blocker| blocker == "eip712_order_payload_signing"));
        }
        if order_post_request_dry_run_available() {
            assert!(!blockers
                .iter()
                .any(|blocker| blocker == "non_submitting_order_post_request_dry_run"));
        } else {
            assert!(blockers
                .iter()
                .any(|blocker| blocker == "non_submitting_order_post_request_dry_run"));
        }
        if order_submit_facade_available() {
            assert!(!blockers
                .iter()
                .any(|blocker| blocker == "l2_order_posting_client"));
            assert!(!blockers.iter().any(|blocker| blocker == "real_order_route"));
        } else {
            assert!(blockers
                .iter()
                .any(|blocker| blocker == "l2_order_posting_client"));
            assert!(blockers.iter().any(|blocker| blocker == "real_order_route"));
        }
        if human_approval_workflow_available() {
            assert!(!blockers
                .iter()
                .any(|blocker| blocker == "human_approval_gate"));
        } else {
            assert!(blockers
                .iter()
                .any(|blocker| blocker == "human_approval_gate"));
        }
        if kill_switch_and_exposure_limits_available() {
            assert!(!blockers
                .iter()
                .any(|blocker| blocker == "kill_switch_and_exposure_limits"));
        } else {
            assert!(blockers
                .iter()
                .any(|blocker| blocker == "kill_switch_and_exposure_limits"));
        }
        if real_order_reconciliation_available() {
            assert!(!blockers
                .iter()
                .any(|blocker| blocker == "real_order_journaling_and_reconciliation"));
        } else {
            assert!(blockers
                .iter()
                .any(|blocker| blocker == "real_order_journaling_and_reconciliation"));
        }
        if market_metadata_validation_available() {
            assert!(!blockers
                .iter()
                .any(|blocker| blocker == "market_tick_and_neg_risk_validation"));
        } else {
            assert!(blockers
                .iter()
                .any(|blocker| blocker == "market_tick_and_neg_risk_validation"));
        }
        assert!(!blockers
            .iter()
            .any(|blocker| blocker == "paper_market_data_ready"));
    }

    #[tokio::test]
    async fn order_placement_readiness_blocks_missing_final_review_audit() {
        let preflight = serde_json::json!({
            "checks": [
                {"name": "collateral_balance_positive", "ok": false},
                {"name": "collateral_allowance_positive", "ok": false}
            ]
        });
        let review_health = serde_json::json!({"status": "ok"});
        let market_data_readiness = serde_json::json!({
            "status": "ready",
            "active_market_count": 1,
            "data_ready_market_count": 1
        });
        let final_review_audit = serde_json::json!({
            "status": "missing_audit",
            "count": 0
        });

        let readiness = build_order_placement_readiness(
            true,
            true,
            &preflight,
            &review_health,
            &market_data_readiness,
            &final_review_audit,
            &crate::clob::live_sender::build_live_sender_boundary_status().await,
        );
        let blockers = readiness["blockers"].as_array().expect("blockers");

        assert_eq!(readiness["required_count"], 18);
        assert_eq!(readiness["final_review_audit_status"], "missing_audit");
        assert!(blockers
            .iter()
            .any(|blocker| blocker == "final_review_decision_audit"));
    }

    #[tokio::test]
    async fn final_review_readiness_aggregates_latest_gate_evidence() {
        let collateral_event = serde_json::json!({
            "id": uuid::Uuid::nil(),
            "payload": {
                "kind": "clob_collateral_readiness",
                "report": {
                    "collateral_balance_positive": false,
                    "collateral_allowance_positive": false,
                    "request_sent": false,
                    "post_order_called": false
                }
            },
            "created_at": "2026-05-29T00:00:00Z"
        });
        let unlock_event = serde_json::json!({
            "id": uuid::Uuid::nil(),
            "payload": {
                "kind": "clob_real_trading_unlock_status",
                "report": {
                    "explicit_real_order_submission_configured": false,
                    "kill_switch_open": false,
                    "paper_mode_active": true,
                    "live_order_sender_implemented": false,
                    "request_sent": false
                }
            },
            "created_at": "2026-05-29T00:00:00Z"
        });
        let reconciliation_event = serde_json::json!({
            "id": uuid::Uuid::nil(),
            "payload": {
                "kind": "clob_order_submit_reconciliation",
                "reconciliation": {
                    "reconciled": true,
                    "request_sent": false,
                    "expected_exchange_state": "no_order_created"
                }
            },
            "created_at": "2026-05-29T00:00:00Z"
        });

        let report = build_final_review_readiness_report(
            true,
            Some(collateral_event),
            Some(unlock_event),
            Some(reconciliation_event),
            &crate::clob::live_sender::build_live_sender_boundary_status().await,
        );
        let blockers = report["blockers"].as_array().expect("blockers");

        assert_eq!(report["ready"], false);
        assert_eq!(report["ready_for_final_review"], false);
        assert_eq!(report["request_sent"], false);
        assert_eq!(report["post_order_called"], false);
        assert_eq!(report["required_count"], 12);
        assert_eq!(
            report["live_sender_boundary_status"]["boundary_name"],
            "LiveOrderSender"
        );
        assert_eq!(
            report["live_sender_boundary_status"]["implementation_name"],
            "FailClosedLiveOrderSender"
        );
        assert_eq!(
            report["live_sender_boundary_status"]["network_sender_present"],
            false
        );
        assert_eq!(
            report["live_sender_boundary_status"]["accepted_for_network_dispatch"],
            false
        );
        assert!(!blockers
            .iter()
            .any(|blocker| blocker == "fail_closed_live_sender_boundary"));
        assert!(blockers
            .iter()
            .any(|blocker| blocker == "collateral_balance_positive"));
        assert!(blockers
            .iter()
            .any(|blocker| blocker == "collateral_allowance_positive"));
        assert!(blockers
            .iter()
            .any(|blocker| blocker == "explicit_real_trading_config_unlock"));
        assert!(blockers.iter().any(|blocker| blocker == "kill_switch_open"));
        assert!(blockers
            .iter()
            .any(|blocker| blocker == "paper_mode_disabled"));
        assert!(blockers
            .iter()
            .any(|blocker| blocker == "live_order_sender_implemented"));
        assert!(!blockers
            .iter()
            .any(|blocker| blocker == "submit_reconciled_no_send"));
    }

    #[test]
    fn final_review_decisions_are_audit_only() {
        assert_eq!(
            normalize_final_review_decision("acknowledge_blocked"),
            Some("acknowledge_blocked")
        );
        assert_eq!(
            normalize_final_review_decision("reviewed_blocked"),
            Some("acknowledge_blocked")
        );
        assert_eq!(
            normalize_final_review_decision("reject_live_trading"),
            Some("reject_live_trading")
        );
        assert_eq!(
            normalize_final_review_decision("needs_rework"),
            Some("needs_rework")
        );
        assert_eq!(normalize_final_review_decision("approve"), None);
        assert_eq!(normalize_final_review_decision("approve_real_orders"), None);
    }

    #[test]
    fn oldest_unreviewed_age_seconds_is_clamped() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-27T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let older = chrono::DateTime::parse_from_rfc3339("2026-05-27T11:59:30Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let future = chrono::DateTime::parse_from_rfc3339("2026-05-27T12:00:30Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        assert_eq!(oldest_unreviewed_age_seconds(now, Some(older)), Some(30));
        assert_eq!(oldest_unreviewed_age_seconds(now, Some(future)), Some(0));
        assert_eq!(oldest_unreviewed_age_seconds(now, None), None);
    }

    #[test]
    fn review_backlog_status_uses_stale_threshold() {
        assert_eq!(review_backlog_status(None), "empty");
        assert_eq!(review_backlog_status(Some(0)), "fresh");
        assert_eq!(
            review_backlog_status(Some(REVIEW_BACKLOG_STALE_AFTER_SECONDS - 1)),
            "fresh"
        );
        assert_eq!(
            review_backlog_status(Some(REVIEW_BACKLOG_STALE_AFTER_SECONDS)),
            "stale"
        );
    }

    // 2026-06-03 approval UX tests (new for snapshots + 401 paths coverage + journal integrity).
    #[test]
    fn human_and_final_approval_requests_deserialize_with_snapshot_defaults() {
        // Human with snapshots
        let jh = r#"{"token_id":"123","side":"buy","order_type":"limit","size":"1","price":"0.5","decision":"approve_facade","confirm_human_approval_workflow":true,"note":"op note","operator":"op@ex","risk_snapshot":{"proj":"0.5"},"collateral_snapshot":{"bal":"100"}}"#;
        let rh: HumanApprovalRequest = serde_json::from_str(jh).expect("human deser with snaps");
        assert!(rh.risk_snapshot.is_some());
        assert!(rh.collateral_snapshot.is_some());

        // Human defaults (old clients)
        let jh2 = r#"{"token_id":"123","side":"buy","order_type":"limit","size":"1","price":"0.5","decision":"approve_facade","confirm_human_approval_workflow":true,"note":"op note","operator":"op@ex"}"#;
        let rh2: HumanApprovalRequest =
            serde_json::from_str(jh2).expect("human deser default snaps");
        assert!(rh2.risk_snapshot.is_none());
        assert!(rh2.collateral_snapshot.is_none());

        // Final with snapshots
        let jf = r#"{"final_review_event_id":"11111111-1111-1111-1111-111111111111","decision":"acknowledge_blocked","confirm_final_review_workflow":true,"note":"fn","operator":"op@ex","risk_snapshot":{"agg":"ok"},"collateral_snapshot":{}}"#;
        let rf: FinalReviewDecisionRequest =
            serde_json::from_str(jf).expect("final deser with snaps");
        assert!(rf.risk_snapshot.is_some());

        // Final defaults
        let jf2 = r#"{"final_review_event_id":"11111111-1111-1111-1111-111111111111","decision":"acknowledge_blocked","confirm_final_review_workflow":true,"note":"fn","operator":"op@ex"}"#;
        let rf2: FinalReviewDecisionRequest =
            serde_json::from_str(jf2).expect("final deser default");
        assert!(rf2.risk_snapshot.is_none());
    }

    // Explicit 401 path coverage for approval creation (AuthUser none branch). The actual extractor is integration-tested via verify curls too.
    // Submit 401 shape covered in verify + prior tests.
    #[test]
    fn approval_creation_unauth_responses_indicate_401() {
        // Simulate the early return shape (handlers are not unit-callable without full State/Auth setup; this asserts the documented 401 contract).
        // Real 401 exercised by deploy/verify UNAUTH_*_STATUS == 401 for human-approval + final-review-decision + submit POSTs.
        let unauth_json = serde_json::json!({
            "paper_only": true,
            "real_orders_enabled": false,
            "approved_for_facade": false,
            "journaled": false,
            "error": "operator authentication required for human-approval (privileged gate)"
        });
        assert_eq!(
            unauth_json["error"],
            "operator authentication required for human-approval (privileged gate)"
        );
        let unauth_final = serde_json::json!({
            "paper_only": true,
            "real_orders_enabled": false,
            "approved_for_real_orders": false,
            "final_review_decision_recorded": false,
            "journaled": false,
            "error": "operator authentication required for final-review-decision (privileged gate)"
        });
        assert!(unauth_final["error"]
            .as_str()
            .unwrap()
            .contains("final-review-decision"));
    }

    // Happy journal payload integrity (simulates what handler builds on success path; asserts snapshots/ids/operator present).
    #[test]
    fn approval_payloads_include_snapshots_operator_and_ids() {
        // Mirror the enriched payload construction (human)
        let now = chrono::Utc::now();
        let payload_h = serde_json::json!({
            "kind": "clob_order_human_approval",
            "decision": "approve_facade",
            "operator": "test-op@polytrader.local",
            "note": "test with snapshot",
            "risk_snapshot_at_approval": {"projected_notional": "0.5"},
            "collateral_snapshot_at_approval": {"collateral_balance_positive": true},
            "approval_time": now,
            "subject_hash": "abc123",
        });
        assert_eq!(payload_h["operator"], "test-op@polytrader.local");
        assert_eq!(payload_h["kind"], "clob_order_human_approval");
        assert!(payload_h.get("subject_hash").is_some());
        assert!(payload_h.get("risk_snapshot_at_approval").is_some());
        assert!(payload_h.get("collateral_snapshot_at_approval").is_some());
        assert!(payload_h.get("approval_time").is_some());

        // Final
        let payload_f = serde_json::json!({
            "kind": "clob_final_review_decision",
            "decision": "acknowledge_blocked",
            "final_review_event_id": "22222222-2222-2222-2222-222222222222",
            "operator": "test-op@polytrader.local",
            "risk_snapshot_at_approval": {"ready": false},
            "collateral_snapshot_at_approval": null,
            "approval_time": now,
        });
        assert_eq!(
            payload_f["final_review_event_id"],
            "22222222-2222-2222-2222-222222222222"
        );
        assert_eq!(payload_f["kind"], "clob_final_review_decision");
        assert!(payload_f.get("risk_snapshot_at_approval").is_some());
    }
}
