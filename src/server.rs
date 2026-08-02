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
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Json, Redirect},
    routing::{get, post},
    Router,
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use sqlx::Row as _;
use std::collections::HashMap;
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

#[derive(Debug, Deserialize, Clone)]
struct HermesConfigRequest {
    model: String,
    reasoning_effort: Option<String>,
}

/// Fixed allow-list of OpenRouter models operators may pick for Hermes reflection synthesis.
/// Hermes (src/bin/hermes.rs) re-reads the latest `hermes_config` journal event each cycle, so
/// changes here take effect within one reflection interval — no redeploy needed. Keep in sync
/// with the identical list in src/bin/hermes.rs (no shared lib crate between the two binaries).
const HERMES_ALLOWED_MODELS: &[&str] = &[
    "openai/gpt-5.6-luna",
    "openai/gpt-5.6-terra",
    "~x-ai/grok-latest",
    "~google/gemini-flash-latest",
    "~google/gemini-pro-latest",
    "~anthropic/claude-sonnet-latest",
];

/// OpenRouter's unified `reasoning.effort` field; "none" omits the field entirely (plain models).
const HERMES_ALLOWED_REASONING_LEVELS: &[&str] = &["none", "low", "medium", "high"];

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
    taker_fee_rate: Option<Decimal>,
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
        // Landing page = the lively Markets board.
        .route("/", get(board_page_handler))
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
        .route("/trades/pnl", get(trades_pnl_handler))
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
        .route("/trades/hermes-config", post(hermes_config_set_handler))
        // AUTH (Next Phase): login/callback/logout/whoami. Optional for paper (dual edge+app).
        // Relative links in UI + <base> ensure subpath compat. /health untouched (public).
        .route("/auth/login", get(auth_login_handler))
        .route("/auth/callback", get(auth_callback_handler))
        .route("/auth/logout", get(auth_logout_handler))
        .route("/auth/whoami", get(auth_whoami_handler))
        // The /console tab and its ~70 CLOB/L2 diagnostic endpoints were removed 2026-08-02:
        // an unused operator UI carrying 56% of this file. The fail-closed real-order gating it
        // reported on lives in src/clob/ and is unchanged — only the HTTP facades are gone.
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

    let engine = crate::paper::PaperTradingEngine::new(
        pool.clone(),
        Arc::new(crate::journal::JournalWriter::new(pool.clone())),
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

/// Classify a fusion signal's health by comparing a `recent` fire-rate against a `baseline`, so a
/// silently degrading signal (e.g. a stalled news feed, a processor that stopped firing) is flagged in
/// the scorecard instead of needing manual comparison across status checks. Window-agnostic: used both
/// for 3h-vs-24h (sudden shifts) and 24h-vs-7d (slow multi-day decay).
///
/// `"insufficient_data"` when too few recent reports to judge; `"elevated"` when the recent rate is
/// notably higher than baseline (incl. a previously-quiet signal waking up); `"dormant"` when an
/// active signal has gone silent; `"degraded"` when its fire-rate more than halved; else `"ok"`.
fn signal_health(baseline_pct: Decimal, recent_pct: Decimal, recent_n: usize) -> &'static str {
    if recent_n < 20 {
        return "insufficient_data";
    }
    // Baseline near-silent: only notable if it suddenly woke up.
    if baseline_pct < Decimal::from(5) {
        return if recent_pct > Decimal::from(15) {
            "elevated"
        } else {
            "ok"
        };
    }
    if recent_pct <= Decimal::from(1) {
        return "dormant"; // an active signal went silent
    }
    if recent_pct < baseline_pct / Decimal::from(2) {
        return "degraded"; // fire-rate more than halved
    }
    if recent_pct > baseline_pct * Decimal::from(2) {
        return "elevated"; // fire-rate doubled
    }
    "ok"
}

/// The signals shown on the dashboard scorecard, in display order.
///
/// **This array is the single source of truth for the scorecard's SQL too** — the 7-day baseline
/// query is generated from it (`signal_fired_count_sql`), not hand-written alongside it. That is a
/// direct response to a real bug: the hand-written query kept a column for `overreaction_fade`
/// after the signal was retired from this list on 2026-06-29, while the results were still indexed
/// positionally by the list. Every row from `theta_convergence` down silently read a *different
/// signal's* baseline — theta was compared against a dead signal (permanent bogus "elevated"),
/// yahoo against theta's baseline ("degraded"), news against yahoo's ("dormant"). All three badges
/// on the live dashboard were artifacts, and the one instrument built to catch a silently-dying
/// signal was itself lying. Generating the columns makes that class of drift unrepresentable.
pub(crate) const SCORECARD_SIGNALS: [&str; 5] = [
    "orderbook_momentum",
    "spike_divergence",
    "theta_convergence",
    "yahoo_finance",
    "news_sentiment",
];

/// One `count(*) FILTER (…)` column per scorecard signal, in `SCORECARD_SIGNALS` order.
///
/// "fired" = the score string contains a 1-9 digit (any nonzero decimal does; `"0"`/`"-0"`/`"0.00"`/
/// absent do not) — a cast-free, robust mirror of `!Decimal::is_zero()` that cannot throw on a stray
/// non-numeric score. Signal names are compile-time constants from this crate, never user input.
fn signal_fired_count_sql() -> String {
    SCORECARD_SIGNALS
        .iter()
        .map(|s| {
            format!(
                "count(*) FILTER (WHERE payload->'report'->'attribution'->'{s}'->>'score' ~ '[1-9]')::bigint"
            )
        })
        .collect::<Vec<_>>()
        .join(",\n                   ")
}

/// Process-wide 5-minute cache for the expensive 7-day signal-health baseline. That aggregate reads
/// ~21k decision_report payloads (~1.6s) and is polled on every /trades/data hit (dashboard, 15s); it's
/// a slow-moving multi-day trend, so serving a ≤5-min-old value is fine and cuts the DB load ~95%.
#[allow(clippy::type_complexity)]
fn health_7d_baseline_cache(
) -> &'static std::sync::Mutex<Option<(std::time::Instant, (i64, [i64; SCORECARD_SIGNALS.len()]))>>
{
    static C: std::sync::OnceLock<
        std::sync::Mutex<Option<(std::time::Instant, (i64, [i64; SCORECARD_SIGNALS.len()]))>>,
    > = std::sync::OnceLock::new();
    C.get_or_init(|| std::sync::Mutex::new(None))
}

/// Per-signal 24h/3h scorecard aggregate cache: (24h report total, 3h report total, per-attribution-
/// key rows of (name, fired_24h, avg_abs_score_24h, fired_3h)). Same rationale as the 7d baseline
/// cache — the full-day JSONB scan must not run on every 5-min dashboard poll. 300s TTL.
type ScorecardAgg = (i64, i64, Vec<(String, i64, Decimal, i64)>);
#[allow(clippy::type_complexity)]
fn scorecard_agg_cache() -> &'static std::sync::Mutex<Option<(std::time::Instant, ScorecardAgg)>> {
    static C: std::sync::OnceLock<std::sync::Mutex<Option<(std::time::Instant, ScorecardAgg)>>> =
        std::sync::OnceLock::new();
    C.get_or_init(|| std::sync::Mutex::new(None))
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

    // Outstanding-position + exposure counters for the top view. There is no hard COUNT cap on open
    // positions/arbs — the binding limit is the total-exposure cap (max_total_exposure_pct of
    // virtual_usdc+locked, the same denominator the risk gate uses). Surface the count plus how much of
    // that exposure budget is used, so the operator can see headroom at a glance.
    let open_positions = positions.len();
    let risk_cfg = crate::risk::RiskConfig::from_env();
    let portfolio_json = match portfolio {
        Some((usdc, locked, _unreal, realized, as_of)) => {
            let total_value = usdc + locked;
            let max_total_exposure = (total_value * risk_cfg.max_total_exposure_pct).round_dp(2);
            let exposure_pct = if total_value > Decimal::ZERO {
                (locked / total_value * Decimal::from(100)).round_dp(1)
            } else {
                Decimal::ZERO
            };
            serde_json::json!({
                "virtual_usdc": usdc.round_dp(2).to_string(),
                "total_locked": locked.round_dp(2).to_string(),
                "realized_pnl": realized.round_dp(2).to_string(),
                "live_unrealized_pnl": total_unrealized.round_dp(2).to_string(),
                "equity": (usdc + locked + total_unrealized).round_dp(2).to_string(),
                "open_positions": open_positions,
                "max_total_exposure": max_total_exposure.to_string(),
                "exposure_pct": exposure_pct.to_string(),
                "max_position_usdc": risk_cfg.max_position_usdc.to_string(),
                "as_of": as_of.to_rfc3339(),
            })
        }
        None => serde_json::json!({ "open_positions": open_positions }),
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
    // ALL settlements SINCE THE LAST PAPER RESET (not a LIMIT-25 window): the realized-P&L aggregates
    // below (the settlements card AND the dual-gate simulation) must reconcile with the AUTHORITATIVE
    // cumulative portfolio realized P&L in virtual_portfolio_snapshots, which is the running sum of every
    // paper_position_settled event. Summing only the most-recent 25 events under-/over-counted wildly —
    // e.g. when a resolved market was being re-settled every cycle, 21 of the last 25 events were phantom
    // duplicates, which crowded out the real (mostly losing) settlements and reported settled_realized ≈
    // −$84 against a true portfolio realized of +$5. The display list is capped to the 25 most recent
    // separately. RESET-BOUNDARY: `POST /paper/reset` zeroes the portfolio (writes a `manual_paper_reset`
    // snapshot) but PRESERVES the journal, so pre-reset settlements (incl. the 2026-06-24 re-settlement
    // phantoms) must be excluded here or the panel reports stale wins against a post-reset realized of 0.
    let settle_rows: Vec<(serde_json::Value, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT payload, created_at FROM journal.events
         WHERE event_type = 'paper_position_settled'
           AND created_at >= COALESCE(
             (SELECT max(as_of) FROM paper_trading.virtual_portfolio_snapshots
              WHERE snapshot_reason = 'manual_paper_reset'), '-infinity'::timestamptz)
         ORDER BY created_at DESC",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    // Recent settlements for the UI list only (capped); aggregates below use the full set.
    let settlements: Vec<serde_json::Value> = settle_rows
        .iter()
        .take(25)
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
    // Total settled count over ALL settlements (the recent list is capped at 25 for display).
    let settled_count = settle_rows.len();
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

    // Latest operator-set Hermes model/reasoning override (append-only, same idiom as llm_health/
    // strategy_weights above). None means Hermes falls back to its LLM_MODEL/no-reasoning env defaults.
    let hermes_config_row: Option<(serde_json::Value, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT payload, created_at FROM journal.events WHERE event_type = 'hermes_config' ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    let hermes_config_json = serde_json::json!({
        "models": HERMES_ALLOWED_MODELS,
        "reasoning_levels": HERMES_ALLOWED_REASONING_LEVELS,
        "current": hermes_config_row.map(|(payload, created_at)| serde_json::json!({
            "model": payload.get("model"),
            "reasoning_effort": payload.get("reasoning_effort"),
            "updated_at": created_at,
        })),
    });

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
    let ingest_interval_secs: i64 = env_flag("POLYTRADER_INGEST_INTERVAL_SECS")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(300);
    // Live "markets tracked" (fixes a stale stat — this used to be just the static
    // POLYTRADER_BOOTSTRAP_MARKETS/ARB_ONLY_MARKETS env list lengths, e.g. "29 (7 arb-only)",
    // ignoring rotation-promoted markets, the volume-ranked arb-discovery pool, and the ladder
    // watchlist entirely — the real scan universe runs ~170 markets, ~6x higher). `updated_at`
    // within 2x the ingest interval is a reliable live-universe proxy: ingest_tick upserts
    // (refreshing updated_at) EVERY market in its candidate list every tick, so anything not
    // recently refreshed has fallen out of bootstrap/rotation/discovery and is a stale historical
    // row from a market we no longer poll, not currently tracked. 2x (not 1x) gives headroom against
    // tick-processing-time jitter (a market processed early in a slow tick can be ~tick_duration
    // stale before the next cycle even starts). arb-only uses the real `is_arb_only_market`
    // classifier (main.rs) against the live set, not the incomplete static allowlist, so sports AND
    // the broader arb_category-routed markets (crypto/finance/geopolitics/etc.) both count.
    let tracked_slugs: Vec<String> = sqlx::query_scalar(
        "SELECT slug FROM market_data.markets WHERE updated_at > now() - ($1 || ' seconds')::interval",
    )
    .bind((ingest_interval_secs * 2).to_string())
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let markets_tracked = tracked_slugs.len();
    let arb_only_count = tracked_slugs
        .iter()
        .filter(|s| crate::is_arb_only_market(s))
        .count();
    let config_json = serde_json::json!({
        "risk": risk_cfg.to_json(),
        "autonomous_paper_execution": env_flag("POLYTRADER_AUTONOMOUS_PAPER_EXECUTION")
            .map(|v| v.to_lowercase() == "on").unwrap_or(false),
        "external_signals": env_flag("POLYTRADER_EXTERNAL_SIGNALS")
            .map(|v| v.to_lowercase() == "on").unwrap_or(false),
        "ingest_interval_secs": ingest_interval_secs.to_string(),
        "decision_cadence_secs": "300",
        "markets_tracked": markets_tracked,
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
    // RESET-BOUNDARY: same as settle_rows above — reset preserves the journal, so lifetime fills
    // would report pre-reset bands (e.g. 78 ghost fills / $9.4k notional against an empty portfolio).
    let fill_rows: Vec<FillBandRow> = sqlx::query_as(
        "SELECT payload->>'market_id', payload->>'outcome', payload->>'net_edge',
                    payload->>'gross_notional', (payload->>'clears_shadow_gate')::bool
             FROM journal.events
             WHERE event_type = 'autonomous_paper_execution' AND payload->>'action' = 'filled'
               AND created_at >= COALESCE(
                 (SELECT max(as_of) FROM paper_trading.virtual_portfolio_snapshots
                  WHERE snapshot_reason = 'manual_paper_reset'), '-infinity'::timestamptz)",
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
        // Only settlements that came from a DIRECTIONAL entry belong in the gate simulation — the
        // gate never governed anything else. A settlement with no entry in `band_map` is an arb /
        // negrisk basket leg (or a pre-execution-journaling legacy position), so SKIP it.
        // The old code instead fell back to `in_strict = realized >= 0`, which was circular:
        // it classified an unmapped settlement by its own OUTCOME and then measured that outcome.
        // Live impact (2026-07-10): 41 unmapped arb-leg settlements, 34 of them profitable, GIFTED
        // +$91.42 to the strict band while their losing siblings stayed lenient-only — manufacturing
        // the entire "strict +$103 vs lenient +$40" gap and making a tighter gate look great on
        // evidence that was really just arb P&L. (Matches the long-standing roadmap note that the
        // strict-beats-lenient signal is a subset-methodology artifact.)
        let Some(&in_strict) = band_map.get(&(m.to_string(), o.to_string())) else {
            continue;
        };
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
    // overreaction_fade retired 2026-06-29 (unwired from the fusion engine) — excluded from the
    // scorecard so the UI doesn't show a permanently-dead row. The list lives at module scope
    // because the 7-day baseline SQL is generated from it — see SCORECARD_SIGNALS.
    const SIGNALS: [&str; SCORECARD_SIGNALS.len()] = SCORECARD_SIGNALS;
    // 24h + 3h fire-rate/influence aggregates, computed SERVER-SIDE (2026-07-15). The old code
    // pulled the most recent 3,000 attribution blobs and looped in Rust — fine when a day held
    // <3,000 reports, but DR volume grew to ~11,000/day, silently shrinking the "LAST 24H"
    // scorecard to ~6.3 hours. That mislabeled window showed spike_divergence as "0% (0)" while it
    // had actually fired 40× in the true 24h — a healthy signal one diagnostic away from being
    // debugged as dead. SQL aggregation over the full window costs no Rust memory; a 300s cache
    // keeps the JSONB scan off the 5-min dashboard poll path (same pattern as the 7d baseline).
    let (reports_total_i64, recent_total_i64, per_signal_agg) = {
        const TTL: std::time::Duration = std::time::Duration::from_secs(300);
        let cache = scorecard_agg_cache();
        let cached = cache.lock().ok().and_then(|g| {
            g.as_ref()
                .filter(|(t, _)| t.elapsed() < TTL)
                .map(|(_, v)| v.clone())
        });
        if let Some(v) = cached {
            v
        } else {
            let totals: Option<(i64, i64)> = sqlx::query_as(
                "SELECT count(*)::bigint,
                        count(*) FILTER (WHERE created_at > now() - interval '3 hours')::bigint
                 FROM journal.events
                 WHERE event_type = 'decision_report' AND created_at > now() - interval '24 hours'",
            )
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
            // Per attribution KEY: fired count (nonzero score, digit-regex — mirrors the 7d
            // baseline's cast-free check), avg |score| when fired (numeric-shape guard before the
            // cast so a stray non-numeric string can't error the whole scan), and 3h fired count.
            // Non-signal keys (fee_impact, advisory_only_policy, …) have no 'score' → NULL → false.
            let rows: Vec<(String, i64, Decimal, i64)> = sqlx::query_as(
                r#"SELECT e.key,
                          count(*) FILTER (WHERE e.value->>'score' ~ '[1-9]')::bigint,
                          COALESCE(avg(abs((e.value->>'score')::numeric))
                                   FILTER (WHERE e.value->>'score' ~ '^-?[0-9]+(\.[0-9]+)?$'
                                             AND e.value->>'score' ~ '[1-9]'), 0),
                          count(*) FILTER (WHERE e.value->>'score' ~ '[1-9]'
                                             AND s.created_at > now() - interval '3 hours')::bigint
                   FROM (SELECT created_at, payload->'report'->'attribution' AS a
                         FROM journal.events
                         WHERE event_type = 'decision_report'
                           AND created_at > now() - interval '24 hours') s,
                        LATERAL jsonb_each(s.a) e
                   GROUP BY e.key"#,
            )
            .fetch_all(pool)
            .await
            .unwrap_or_default();
            let v = (
                totals.map(|(t, _)| t).unwrap_or(0),
                totals.map(|(_, r)| r).unwrap_or(0),
                rows,
            );
            // Only cache a real (successful) computation, not a DB-error fallback.
            if totals.is_some() {
                if let Ok(mut g) = cache.lock() {
                    *g = Some((std::time::Instant::now(), v.clone()));
                }
            }
            v
        }
    };
    let reports_total = reports_total_i64 as usize;
    let recent_total = recent_total_i64 as usize;
    // Long baseline (7d) per-signal fire-rate — a SLIM count-only aggregate (no payloads pulled into
    // memory) so multi-day GRADUAL decay is caught too. The 3h-vs-24h check only sees SUDDEN shifts: when
    // a signal erodes slowly the 24h baseline erodes with it, so the recent/baseline ratio stays ~1 and
    // it reads "ok" — exactly how news_sentiment's ~20%→~1.8% slide hid. Comparing the 24h fire-rate
    // against a 7d baseline surfaces the slow slide.
    //
    // The per-signal columns are GENERATED from SIGNALS rather than written out beside it. They used
    // to be hand-written, and drifted: a column for the retired `overreaction_fade` stayed behind
    // while the results were indexed positionally, so every row from theta down read another
    // signal's baseline (see SCORECARD_SIGNALS for the full post-mortem).
    let (baseline_7d_total, baseline_7d_fired): (i64, [i64; SCORECARD_SIGNALS.len()]) = {
        // 1h TTL: the 7d baseline moves glacially, but the scan behind it is a ~6.5s full read of
        // a week of decision_report JSONB. At the old 300s TTL every 5-min dashboard poll recomputed
        // it (169 slow-query alerts in 14h on 2026-07-11); 1h keeps the health check honest at 1/12
        // the DB load.
        const TTL: std::time::Duration = std::time::Duration::from_secs(3600);
        let cache = health_7d_baseline_cache();
        let cached = cache.lock().ok().and_then(|g| {
            g.as_ref()
                .filter(|(t, _)| t.elapsed() < TTL)
                .map(|(_, v)| *v)
        });
        if let Some(v) = cached {
            v
        } else {
            let sql = format!(
                "SELECT count(*)::bigint,
                   {}
                 FROM journal.events
                 WHERE event_type = 'decision_report' AND created_at > now() - interval '7 days'",
                signal_fired_count_sql()
            );
            let computed: Option<sqlx::postgres::PgRow> =
                sqlx::query(&sql).fetch_optional(pool).await.ok().flatten();
            let v = match computed.as_ref() {
                Some(row) => {
                    let mut fired = [0i64; SCORECARD_SIGNALS.len()];
                    // Column 0 is the total; the per-signal columns follow in SIGNALS order by
                    // construction, since the same array generated them.
                    for (i, slot) in fired.iter_mut().enumerate() {
                        *slot = row.try_get::<i64, _>(i + 1).unwrap_or(0);
                    }
                    (row.try_get::<i64, _>(0).unwrap_or(0), fired)
                }
                None => (0, [0; SCORECARD_SIGNALS.len()]),
            };
            // Only cache a real (successful) computation, not a DB-error fallback of zeros.
            if computed.is_some() {
                if let Ok(mut g) = cache.lock() {
                    *g = Some((std::time::Instant::now(), v));
                }
            }
            v
        }
    };
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
    // Reset-boundary filter (same rationale as the settlements card above): only settlements since the
    // last paper reset, so the per-signal hit-rate reflects the CURRENT run, not stale pre-reset history.
    let settled_rows: Vec<(Option<String>, Option<Decimal>)> = sqlx::query_as(
        "SELECT payload->>'market_id', (payload->>'realized_pnl')::numeric
         FROM journal.events WHERE event_type = 'paper_position_settled'
           AND created_at >= COALESCE(
             (SELECT max(as_of) FROM paper_trading.virtual_portfolio_snapshots
              WHERE snapshot_reason = 'manual_paper_reset'), '-infinity'::timestamptz)",
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
    // Decision-report attributions for the settled markets — the 20 MOST RECENT per market (mirrors
    // Hermes's own attribution lookup), NOT a time window. A signal is credited a market if it fired
    // at any point in that window. (A fixed time window like "last 7 days" wrongly shrinks the record
    // as settled markets age past it — their decision history is mostly older than a few days.) Using
    // the most-recent reports per market also avoids under-crediting signals whose inputs vanish at
    // resolution (e.g. orderbook_momentum: the book empties when a market closes).
    // Self-monitoring (roadmap "Per-market scorecard query just over slow threshold", 2026-07-13):
    // this query clocked ~1.04s (488 rows) when flagged — barely past the 1s alert, uncached, runs
    // every dashboard load. Re-measured 2026-07-21: 330ms, comfortably under. Rather than a one-off
    // manual recheck (which needs someone to remember to look), time it every call and WARN loudly
    // if it crosses 1s again — durable, in-app, visible via the same `kubectl logs` grep as every
    // other diagnostic WARN in this codebase, independent of any external session/cron surviving.
    const SETTLED_ATTR_SLOW_THRESHOLD: std::time::Duration = std::time::Duration::from_secs(1);
    let market_attrs: Vec<(Option<String>, Option<serde_json::Value>)> =
        if settled_market_ids.is_empty() {
            Vec::new()
        } else {
            let t0 = std::time::Instant::now();
            let rows: Vec<(Option<String>, Option<serde_json::Value>)> = sqlx::query_as(
                "SELECT market_id, attribution FROM (
                     SELECT payload->>'market_id' AS market_id,
                            payload->'report'->'attribution' AS attribution,
                            row_number() OVER (
                                PARTITION BY payload->>'market_id' ORDER BY created_at DESC
                            ) AS rn
                     FROM journal.events
                     WHERE event_type = 'decision_report'
                       AND payload->>'market_id' = ANY($1)
                 ) s WHERE rn <= 20",
            )
            .bind(&settled_market_ids)
            .fetch_all(pool)
            .await
            .unwrap_or_default();
            let elapsed = t0.elapsed();
            if elapsed > SETTLED_ATTR_SLOW_THRESHOLD {
                tracing::warn!(
                    elapsed_ms = elapsed.as_millis() as u64,
                    rows = rows.len(),
                    markets = settled_market_ids.len(),
                    "settled-market scorecard attribution query exceeded 1s (see wiki/roadmap \
                     'Per-market scorecard query just over slow threshold' for the cache/index fix)"
                );
            }
            rows
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
        .enumerate()
        .map(|(idx, name)| {
            // Per-signal numbers from the cached full-window SQL aggregate (true 24h/3h — see the
            // scorecard_agg_cache comment for the LIMIT-3000 window-shrink bug this replaced).
            let (fired, avg_abs_score, recent_fired) = per_signal_agg
                .iter()
                .find(|(k, _, _, _)| k == name)
                .map(|(_, f24, avg, f3)| (*f24 as usize, avg.round_dp(3), *f3 as usize))
                .unwrap_or((0, Decimal::ZERO, 0));
            let fire_rate = if reports_total > 0 {
                (Decimal::from(fired) / Decimal::from(reports_total) * Decimal::from(100))
                    .round_dp(1)
            } else {
                Decimal::ZERO
            };
            // Recent-window (3h) fire-rate + health classification vs the 24h baseline.
            let recent_fire_rate = if recent_total > 0 {
                (Decimal::from(recent_fired) / Decimal::from(recent_total) * Decimal::from(100))
                    .round_dp(1)
            } else {
                Decimal::ZERO
            };
            let health = signal_health(fire_rate, recent_fire_rate, recent_total);
            // Long-baseline (7d) health: compare the 24h fire-rate to the 7d baseline to catch slow,
            // multi-day decay the 3h-vs-24h check is blind to. Reuses the same classifier (baseline=7d,
            // recent=24h, n=24h report count).
            let fire_rate_7d = if baseline_7d_total > 0 {
                (Decimal::from(baseline_7d_fired[idx]) / Decimal::from(baseline_7d_total)
                    * Decimal::from(100))
                .round_dp(1)
            } else {
                Decimal::ZERO
            };
            let health_7d = signal_health(fire_rate_7d, fire_rate, reports_total);
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
                "recent_fire_rate_pct": recent_fire_rate.to_string(),
                "fire_rate_7d_pct": fire_rate_7d.to_string(),
                "health": health,
                "health_7d": health_7d,
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
        "recent_window_h": 3,
        "recent_total": recent_total,
        "baseline_window_h": 168,
        "baseline_7d_total": baseline_7d_total,
        "settled_markets": net_by_market.len(),
        "rows": signal_rows,
        "note": "Fire-rate = share of the last 24h decision reports where the signal contributed a \
                 non-zero score. health compares the recent 3h fire-rate to the 24h baseline (catches \
                 SUDDEN shifts); health_7d compares the 24h fire-rate to the 7d baseline (catches \
                 multi-day GRADUAL decay the 3h-vs-24h check is blind to — the 24h baseline erodes with \
                 the signal). Both: 'degraded' = fire-rate more than halved, 'dormant' = went silent, \
                 'elevated' = doubled, 'insufficient_data' = too few reports to judge. Raw score is the \
                 average |score| BEFORE the advisory domination cap (2026-07-13) — for news_sentiment/ \
                 yahoo_finance this overstates real sway on the fused decision, since fuse_named bounds \
                 their actual contribution to at most the market-internal numerator's magnitude; Weight \
                 (Hermes's learned trust) is the more honest read of real influence post-cap. Settled \
                 record = win/loss of settled markets where the signal fired in the final decision \
                 report (count-based, overlapping). Realized P&L populates at 10 settled.",
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
        "hermes_config": hermes_config_json,
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

/// P&L time series for the chart's interval selector. `?range=` is one of 1d/1w/1m/1y/all (default
/// 1d). Each range pairs a look-back window with a downsample bucket so the payload stays ~300-400
/// points regardless of horizon: the running total P&L (realized + unrealized) of the LAST snapshot in
/// each bucket. The default /trades/data still ships the 1d series so the page paints without a second
/// round-trip; this endpoint backs the other ranges.
async fn trades_pnl_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let pool = &state.pool;
    let range = query
        .get("range")
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_else(|| "1d".to_string());
    // (window_secs, bucket_secs) per range. window 0 = no lower bound ("all"). Buckets chosen to keep
    // ~300-360 points: 1-min (1h), 5-min (1d), 30-min (1w), 2-h (1m), 1-day (1y/all).
    let (window_secs, bucket_secs): (i64, i64) = match range.as_str() {
        "1h" => (3600, 60),
        "1w" => (7 * 86400, 1800),
        "1m" => (30 * 86400, 7200),
        "1y" => (365 * 86400, 86400),
        "all" => (0, 86400),
        _ => (86400, 300), // 1d default
    };
    // Last snapshot per time bucket within the window (DISTINCT ON bucket, newest first), re-sorted
    // ascending for a left-to-right plot.
    let rows: Vec<(chrono::DateTime<chrono::Utc>, Decimal, Decimal)> = sqlx::query_as(
        "SELECT bucket_ts, realized_pnl, unrealized_pnl FROM (
             SELECT DISTINCT ON (b)
                 to_timestamp(floor(extract(epoch FROM as_of) / $1::bigint) * $1::bigint) AS bucket_ts,
                 floor(extract(epoch FROM as_of) / $1::bigint) AS b,
                 realized_pnl, unrealized_pnl, as_of
             FROM paper_trading.virtual_portfolio_snapshots
             WHERE ($2::bigint = 0 OR as_of >= now() - ($2::bigint * interval '1 second'))
             ORDER BY b, as_of DESC
         ) x ORDER BY bucket_ts ASC",
    )
    .bind(bucket_secs)
    .bind(window_secs)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let points: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|(at, realized, unreal)| {
            serde_json::json!({
                "t": at.timestamp(),
                "pnl": (realized + unreal).round_dp(2).to_string(),
            })
        })
        .collect();
    Json(serde_json::json!({
        "range": range,
        "bucket_secs": bucket_secs,
        "points": points,
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
  </nav>
</header>
<main>
  <div class="cards" id="cards"></div>
  <h2>Profit &amp; Loss <span class="pill" id="pnl-now"></span>
    <span id="pnl-ranges" style="float:right;display:inline-flex;gap:4px;font-size:12px;font-weight:400;"></span>
  </h2>
  <div id="pnl-chart" class="chartbox"><div class="empty">loading P&amp;L history…</div></div>
  <h2>Signal Scorecard <span class="pill" id="signals-window"></span></h2>
  <div id="signals"></div>
  <h2>Gate Simulation <span class="pill dir" id="gatesim-edges"></span></h2>
  <div id="gatesim"></div>
  <h2>Parameters <span class="pill" id="params-mode">paper · read-only</span></h2>
  <div id="params"></div>
  <h2>Hermes AI <span class="pill" id="hermes-config-status"></span></h2>
  <div id="hermes-config"></div>
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

// Live P&L area chart: per-segment coloring — green above the zero line, red below (so an early
// underwater dip reads red even when the latest value is positive). Plots the running
// realized+unrealized series. Pure inline SVG (no chart lib) so it stays self-contained.
// Hover state for the P&L chart (set at the end of renderPnlChart each redraw). Kept as plain data
// + closures so the crosshair/tooltip can be drawn by mutating just the #pnl-hover-g group instead
// of re-rendering the whole SVG on every mousemove.
let pnlChartState = null;
function pnlHover(evt){
  if (!pnlChartState) return;
  const {pts, sx, sy, W, H, padL, padR, padT, padB} = pnlChartState;
  const svg = evt.currentTarget.ownerSVGElement;
  if (!svg) return;
  const rect = svg.getBoundingClientRect();
  if (!rect.width) return;
  const relX = (evt.clientX - rect.left) / rect.width * W;
  let best = 0, bestD = Infinity;
  for (let i=0; i<pts.length; i++){
    const d = Math.abs(sx(pts[i].t) - relX);
    if (d < bestD){ bestD = d; best = i; }
  }
  const p = pts[best];
  const px = sx(p.t), py = sy(p.v);
  const g = document.getElementById("pnl-hover-g");
  if (!g) return;
  const dateStr = new Date(p.t*1000).toLocaleString([], {month:"short",day:"numeric",hour:"2-digit",minute:"2-digit"});
  const valStr = (p.v>=0?"+":"") + p.v.toFixed(2);
  const color = p.v>=0 ? "#3fb950" : "#f85149";
  const boxW = 112, boxH = 36;
  let bx = px + 10; if (bx + boxW > W - padR) bx = px - boxW - 10;
  let by = py - boxH - 10; if (by < padT) by = py + 10;
  if (by + boxH > H - padB) by = H - padB - boxH;
  g.innerHTML = `
    <line x1="${px.toFixed(1)}" y1="${padT}" x2="${px.toFixed(1)}" y2="${(H-padB).toFixed(1)}" stroke="#8b949e" stroke-width="1" stroke-dasharray="3 3"/>
    <circle cx="${px.toFixed(1)}" cy="${py.toFixed(1)}" r="4" fill="${color}" stroke="#0d1117" stroke-width="1.5"/>
    <rect x="${bx.toFixed(1)}" y="${by.toFixed(1)}" width="${boxW}" height="${boxH}" rx="4" fill="#161b22" stroke="#30363d"/>
    <text x="${(bx+8).toFixed(1)}" y="${(by+15).toFixed(1)}" fill="#8b949e" font-size="10">${dateStr}</text>
    <text x="${(bx+8).toFixed(1)}" y="${(by+29).toFixed(1)}" fill="${color}" font-size="14" font-weight="600">${valStr}</text>
  `;
}
function pnlHoverEnd(){
  const g = document.getElementById("pnl-hover-g");
  if (g) g.innerHTML = "";
}
function renderPnlChart(series, meta){
  const box = document.getElementById("pnl-chart");
  const pts = (series||[]).map(s => ({t: s.t, v: parseFloat(s.pnl)})).filter(p => !isNaN(p.v));
  const nowEl = document.getElementById("pnl-now");
  if (pts.length < 2) { box.innerHTML = `<div class="empty">Not enough P&L history yet for this range — try a wider interval or wait for more snapshots.</div>`; nowEl.textContent=""; return; }
  const last = pts[pts.length-1].v;
  const up = last >= 0;
  const G_STROKE="#3fb950", R_STROKE="#f85149", G_FILL="rgba(63,185,80,0.16)", R_FILL="rgba(248,81,73,0.16)";
  const stroke = up ? G_STROKE : R_STROKE; // for the "now" pill + end dot (latest value)
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
  // Split coloring at the zero line: the portion above 0 is green, below 0 is red (so an early dip
  // underwater shows red even if the latest value is positive). Done with two clip rectangles split
  // at zeroY, each clipping a green/red copy of the same area + line paths.
  const posH = Math.max(0, zeroY - padT), negH = Math.max(0, (H-padB) - zeroY);
  box.innerHTML = `<svg viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" role="img" aria-label="Profit and loss over time">
    <defs>
      <clipPath id="pnlPos"><rect x="0" y="${padT}" width="${W}" height="${posH.toFixed(1)}"/></clipPath>
      <clipPath id="pnlNeg"><rect x="0" y="${zeroY.toFixed(1)}" width="${W}" height="${negH.toFixed(1)}"/></clipPath>
    </defs>
    <line x1="${padL}" y1="${padT}" x2="${padL}" y2="${H-padB}" stroke="#30363d" stroke-width="1"/>
    <line x1="${padL}" y1="${zeroY.toFixed(1)}" x2="${W-padR}" y2="${zeroY.toFixed(1)}" stroke="#484f58" stroke-width="1" stroke-dasharray="4 4"/>
    <text x="6" y="${(padT+8).toFixed(0)}" fill="#8b949e" font-size="12">${fmtAxis(maxV)}</text>
    <text x="6" y="${(zeroY+4).toFixed(0)}" fill="#8b949e" font-size="12">0</text>
    <text x="6" y="${(H-padB).toFixed(0)}" fill="#8b949e" font-size="12">${fmtAxis(minV)}</text>
    <path d="${area}" fill="${G_FILL}" stroke="none" clip-path="url(#pnlPos)"/>
    <path d="${area}" fill="${R_FILL}" stroke="none" clip-path="url(#pnlNeg)"/>
    <path d="${line}" fill="none" stroke="${G_STROKE}" stroke-width="2" stroke-linejoin="round" stroke-linecap="round" clip-path="url(#pnlPos)"/>
    <path d="${line}" fill="none" stroke="${R_STROKE}" stroke-width="2" stroke-linejoin="round" stroke-linecap="round" clip-path="url(#pnlNeg)"/>
    <circle cx="${sx(pts[pts.length-1].t).toFixed(1)}" cy="${sy(last).toFixed(1)}" r="3.5" fill="${stroke}"/>
    <rect x="${padL}" y="${padT}" width="${(W-padL-padR).toFixed(1)}" height="${(H-padT-padB).toFixed(1)}" fill="transparent" style="cursor:crosshair;" onmousemove="pnlHover(event)" onmouseleave="pnlHoverEnd()"/>
    <g id="pnl-hover-g"></g>
  </svg>`;
  // Hover crosshair/tooltip state — see pnlHover/pnlHoverEnd above.
  pnlChartState = {pts, sx, sy, W, H, padL, padR, padT, padB};
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
       X axis: time · ${pts.length} points over ~${spanTxt}${meta&&meta.bucketTxt?` · ~1 point / ${meta.bucketTxt}`:''} ·
       Y axis: running total P&amp;L = realized + unrealized
     </div>`);
}

// P&L interval selector. 1d is served inline by /trades/data (pnl_series); wider ranges fetch the
// downsampled /trades/pnl endpoint. Selection is remembered and re-applied on each 15s poll.
const PNL_RANGES = [["1h","1H","1-min bucket"],["1d","1D","5-min cycle"],["1w","1W","30-min bucket"],["1m","1M","2-hour bucket"],["1y","1Y","1-day bucket"],["all","ALL","1-day bucket"]];
let pnlRange = "1d";
function renderPnlRangeButtons(){
  const host = document.getElementById("pnl-ranges");
  if (!host || host.childElementCount) return;
  host.innerHTML = PNL_RANGES.map(([r,label])=>
    `<button data-r="${r}" onclick="setPnlRange('${r}')" style="cursor:pointer;border:1px solid #30363d;background:#161b22;color:#8b949e;border-radius:6px;padding:2px 9px;">${label}</button>`
  ).join("");
  updatePnlRangeStyles();
}
function updatePnlRangeStyles(){
  document.querySelectorAll("#pnl-ranges button").forEach(b=>{
    const on = b.dataset.r===pnlRange;
    b.style.background = on ? "#1f6feb22" : "#161b22";
    b.style.color = on ? "#58a6ff" : "#8b949e";
    b.style.borderColor = on ? "#1f6feb55" : "#30363d";
  });
}
function bucketTxtFor(r){ const m=PNL_RANGES.find(x=>x[0]===r); return m?m[2]:""; }
async function loadPnl(){
  try {
    const r = await (await fetch(PREFIX + "/trades/pnl?range=" + encodeURIComponent(pnlRange), {cache:"no-store"})).json();
    renderPnlChart(r.points, {bucketTxt: bucketTxtFor(pnlRange)});
  } catch(e){ /* keep last chart */ }
}
function setPnlRange(r){
  pnlRange = r; updatePnlRangeStyles();
  if (r === "1d" && window.__lastTrades) renderPnlChart(window.__lastTrades.pnl_series, {bucketTxt: bucketTxtFor("1d")});
  else loadPnl();
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
  const healthBadge = (h, title) => {
    if (!h || h === 'ok' || h === 'insufficient_data') return '';
    const col = (h === 'dormant' || h === 'degraded') ? '#f85149' : '#d29922';
    return ` <span title="${title}" style="font-size:10px;padding:1px 5px;border-radius:4px;border:1px solid ${col};color:${col}">${h}</span>`;
  };
  const row = (r) => {
    const rp = r.realized_pnl;
    const rpCell = (rp===null||rp===undefined) ? '<span class="muted">— pending</span>' : `<span class="${cls(rp)}">${sign(rp)}</span>`;
    return `<tr>
      <td>${pretty(r.name)}</td>
      <td>${r.fire_rate_pct}% <span class="muted">(${r.fired})</span>${healthBadge(r.health, `recent 3h fire-rate ${r.recent_fire_rate_pct}% vs 24h baseline (sudden shift)`)}${healthBadge(r.health_7d, `24h fire-rate ${r.fire_rate_pct}% vs 7d baseline ${r.fire_rate_7d_pct}% (multi-day trend)`)}</td>
      <td>${r.avg_abs_score}</td>
      <td class="${wcls(r.weight)}">${parseFloat(r.weight).toFixed(2)}×</td>
      <td>${recordCell(r)}</td>
      <td>${rpCell}</td>
    </tr>`;
  };
  el.innerHTML = `<table>
    <tr><th>Signal</th><th title="Share of recent decision reports where this signal contributed a non-zero score">Fire rate</th><th title="Average absolute RAW score when it fires, BEFORE the advisory domination cap (2026-07-13). For news_sentiment/yahoo_finance this OVERSTATES real influence on the fused decision — their raw score can run far above market-internal signals, but fuse_named bounds their actual contribution to at most the market-internal numerator's magnitude. Compare against Weight (Hermes's learned trust), not this column, to judge real sway.">Raw score</th><th title="Hermes's current confidence multiplier (1.00× = neutral)">Weight</th><th title="Win-loss record of settled markets (by net realized P&amp;L) where this signal fired in the final decision report. Available now, independent of Hermes.">Settled record</th><th title="Realized P&amp;L attributed to this signal (Hermes proportional split); populates at 10 settled">Settled P&amp;L</th></tr>
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

// Hermes AI model/reasoning picker — the only mutable (non-read-only) control on this dashboard.
// Writes go to POST /trades/hermes-config (validated server-side against the same fixed lists);
// hermes.rs re-reads the latest choice each reflection cycle (~10min), no redeploy needed.
let hermesConfigSaving = false;
let hermesConfigBuilt = false; // build the form once; polling only refreshes the status pill so an
                                // unsaved dropdown pick isn't clobbered by the next 15s refresh.
function renderHermesConfig(hc){
  const el = document.getElementById("hermes-config");
  const statusEl = document.getElementById("hermes-config-status");
  if (!hc) { el.innerHTML = `<div class="empty">No config.</div>`; statusEl.textContent=""; return; }
  const cur = hc.current || {};
  const curModel = cur.model || null;
  const curReasoning = cur.reasoning_effort || "none";
  statusEl.textContent = curModel ? `active: ${curModel}${curReasoning!=="none" ? " · "+curReasoning : ""}` : "default (env)";
  if (hermesConfigBuilt) return;
  hermesConfigBuilt = true;
  el.innerHTML = `<div class="card" style="max-width:420px;">
    <div class="label">Model <span style="opacity:.5">&#9432;</span></div>
    <select id="hermes-model-select" style="width:100%;margin:4px 0 10px;background:#0d1117;color:#c9d1d9;border:1px solid #30363d;border-radius:6px;padding:6px;">
      ${(hc.models||[]).map(m => `<option value="${m}" ${m===curModel?'selected':''}>${m}</option>`).join("")}
    </select>
    <div class="label">Reasoning effort <span style="opacity:.5">&#9432;</span></div>
    <select id="hermes-reasoning-select" style="width:100%;margin:4px 0 10px;background:#0d1117;color:#c9d1d9;border:1px solid #30363d;border-radius:6px;padding:6px;">
      ${(hc.reasoning_levels||[]).map(r => `<option value="${r}" ${r===curReasoning?'selected':''}>${r}</option>`).join("")}
    </select>
    <button id="hermes-config-save" style="cursor:pointer;padding:6px 14px;border-radius:6px;border:1px solid #30363d;background:#1f6feb22;color:#58a6ff;">Save</button>
    <span id="hermes-config-msg" style="margin-left:10px;font-size:12px;"></span>
    <div class="t" style="margin-top:8px;line-height:1.35;color:#6e7681;">
      Controls only the optional LLM narrative layer for Hermes's reflection loop (paper-only self-improvement agent) — never trading/risk decisions, which are market-internal signals only. "none" reasoning omits the field for models that don't support it.
    </div>
  </div>`;
  document.getElementById("hermes-config-save").onclick = async () => {
    if (hermesConfigSaving) return;
    hermesConfigSaving = true;
    const msgEl = document.getElementById("hermes-config-msg");
    msgEl.textContent = "saving…"; msgEl.style.color = "#8b949e";
    const model = document.getElementById("hermes-model-select").value;
    const reasoning_effort = document.getElementById("hermes-reasoning-select").value;
    try {
      const r = await fetch(PREFIX + "/trades/hermes-config", {
        method: "POST",
        headers: {"Content-Type": "application/json"},
        body: JSON.stringify({model, reasoning_effort}),
      });
      const j = await r.json();
      if (r.ok && j.ok) { msgEl.textContent = "saved"; msgEl.style.color = "#3fb950"; }
      else { msgEl.textContent = "error: " + (j.error||"save failed"); msgEl.style.color = "#f85149"; }
    } catch (e) {
      msgEl.textContent = "error: " + e; msgEl.style.color = "#f85149";
    } finally {
      hermesConfigSaving = false;
    }
  };
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
    ["Open positions", (p.open_positions ?? "—") + "", "≤ $" + fmt(p.max_position_usdc) + " each"],
    ["Exposure used", (p.exposure_pct ?? "0") + "%", "$" + fmt(p.total_locked) + " / $" + fmt(p.max_total_exposure) + " cap"],
  ];
  if (ra && ra.balance != null) {
    cards.push(["REAL " + fmt(ra.collateral_token||"PUSD"), "$" + fmt(ra.balance)]);
  }
  document.getElementById("cards").innerHTML = cards
    .map(([l,v,sub]) => `<div class="card"><div class="label">${l}</div><div class="val">${v}</div>${sub?`<div style="font-size:11px;color:#8b949e;margin-top:3px">${sub}</div>`:""}</div>`).join("");
  document.getElementById("updated").textContent = "updated " + new Date().toLocaleTimeString();

  window.__lastTrades = d;
  renderPnlRangeButtons();
  // 1d range is already in the polled payload (no extra fetch); wider ranges hit /trades/pnl.
  if (pnlRange === "1d") renderPnlChart(d.pnl_series, {bucketTxt: bucketTxtFor("1d")});
  else loadPnl();
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
  renderHermesConfig(d.hermes_config);

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
    // Politics first: an election market is never Sports, and several sports keywords ("champion",
    // "win the") otherwise claim them.
    if has(&[
        "president",
        "presidential",
        "election",
        "nomination",
        "prime minister",
        "senate",
        "congress",
        "parliament",
    ]) {
        "Politics"
    } else if has(&[
        "world-cup",
        "world cup",
        "fifa",
        "nba",
        "nfl",
        "nhl",
        "fifwc",
        // "win the" was here until 2026-08-01 and tagged EVERY "Will X win the Y election" market
        // as Sports — all four 2028 US-presidential markets, the 2027 French presidential, the
        // Democratic nomination. Display-only (trading routing uses `arb_category` in main.rs, which
        // is unaffected), but it made the board unreadable at a glance. Politics is checked first
        // below so the sports keywords can't claim it.
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
        Option<chrono::DateTime<chrono::Utc>>,
    );
    let markets: Vec<MktRow> = sqlx::query_as(
        // end_date drives the AWAITING RESOLUTION badge: a held market past its end date is not
        // "live", it is parked waiting on Polymarket/UMA, and the card said nothing to distinguish
        // the two. Cast is best-effort — Gamma's end_date is free-form text in raw_json.
        //
        // ORDERING: soonest-resolving first, `gamma_id` as a total tiebreak. It used to be
        // `updated_at DESC`, which made the board reshuffle on almost every refresh — the ingest
        // tick rewrites `updated_at = now()` market-by-market for ~235s of every 300s interval
        // while the board polls every 15s, so most polls landed mid-tick and saw a different
        // permutation of the same cards. `updated_at` is a property of OUR ingest schedule, not of
        // the market, so it was never a meaningful sort key; `end_date` is stable and is what an
        // operator actually scans for. The trailing `gamma_id` matters as much as the switch: even
        // a stable key needs a total order, or rows tied on it are free to swap between queries.
        //
        // WHERE: the board should show what we are actually tracking — the current ingest universe,
        // plus everything we hold regardless of whether it still ranks. `market_data.markets` is
        // never pruned (6,806 rows and growing daily), and filtering on `closed` does almost
        // nothing because Gamma leaves `closed=false` on markets that have long since resolved
        // (6,519 of 6,806 are "not closed"). Recency of ingest is the honest proxy: the tick walks
        // the whole universe inside its 300s interval, so a 2-hour window means "still tracked"
        // with slack for a few missed ticks. Measured: 6,806 rows → 588, a 91% cut of a payload
        // that is re-fetched every 15 seconds by every open tab.
        "SELECT m.gamma_id, m.slug, m.question, m.category, m.last_mid_yes, m.last_mid_no,
                m.active, m.closed, m.resolved_outcome,
                CASE WHEN m.raw_json->>'end_date' ~ '^\\d{4}-\\d{2}-\\d{2}T'
                     THEN (m.raw_json->>'end_date')::timestamptz END AS end_date
         FROM market_data.markets m
         WHERE m.updated_at > now() - interval '2 hours'
            OR EXISTS (SELECT 1 FROM paper_trading.paper_positions p
                        WHERE p.market_id = m.gamma_id AND p.shares > 0)
         ORDER BY m.closed ASC,
                  CASE WHEN m.raw_json->>'end_date' ~ '^\\d{4}-\\d{2}-\\d{2}T'
                       THEN (m.raw_json->>'end_date')::timestamptz END ASC NULLS LAST,
                  m.gamma_id ASC",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    // Latest decision report, news cache, and open position per market. A LATERAL LIMIT-1 lookup
    // (backed by idx_events_type_market_created) rather than a DISTINCT ON scan over all ~92k
    // decision_reports — ~1.3s external-merge sort → ~0.5ms.
    //
    // The comment here used to say "driven from the ~50-row markets table". That premise rotted:
    // `market_data.markets` is never pruned and is now 6,806 rows, so each render was firing 6,806
    // index seeks per fan-out for cards the board no longer shows. Both fan-outs now carry the same
    // tracked-universe filter as the card query above, which is also what keeps them in agreement —
    // a report for a market absent from `markets` was never renderable anyway.
    let dr_rows: Vec<(String, serde_json::Value)> = sqlx::query_as(
        "SELECT m.gamma_id, latest.payload
         FROM market_data.markets m
         CROSS JOIN LATERAL (
             SELECT payload FROM journal.events
             WHERE event_type = 'decision_report' AND payload->>'market_id' = m.gamma_id
             ORDER BY created_at DESC LIMIT 1
         ) latest
         WHERE m.updated_at > now() - interval '2 hours'
            OR EXISTS (SELECT 1 FROM paper_trading.paper_positions p
                        WHERE p.market_id = m.gamma_id AND p.shares > 0)",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let dr_map: HashMap<String, serde_json::Value> = dr_rows.into_iter().collect();

    let news_rows: Vec<(String, serde_json::Value)> = sqlx::query_as(
        "SELECT m.gamma_id, latest.news
         FROM market_data.markets m
         CROSS JOIN LATERAL (
             SELECT payload->'news' AS news FROM journal.events
             WHERE event_type = 'news_cache' AND payload->>'market_id' = m.gamma_id
             ORDER BY created_at DESC LIMIT 1
         ) latest
         WHERE m.updated_at > now() - interval '2 hours'
            OR EXISTS (SELECT 1 FROM paper_trading.paper_positions p
                        WHERE p.market_id = m.gamma_id AND p.shares > 0)",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let news_map: HashMap<String, serde_json::Value> = news_rows.into_iter().collect();

    let pos_rows: Vec<(String, String, Decimal, Decimal)> = sqlx::query_as(
        "SELECT market_id, outcome, shares, avg_entry_price FROM paper_trading.paper_positions WHERE shares > 0",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    // Keyed market_id -> ALL legs. This was a plain HashMap<market_id, one_leg> until 2026-08-01,
    // which SILENTLY DROPPED a leg whenever we held both sides: `.collect()` keeps the last row for
    // a duplicate key. Both-sides holdings are exactly the two-leg YES+NO arbs, so the board showed
    // one hedged leg in isolation — the Israel/Iran ceasefire arb (Yes 200 @ 0.89 + No 200 @ 0.06,
    // $190 cost against a guaranteed $200 payout = **+$10 locked in**) rendered as "HOLDING No ·
    // −$11.70". A guaranteed profit displayed as a loss is worse than no display at all.
    let mut pos_map: HashMap<String, Vec<(String, Decimal, Decimal)>> = HashMap::new();
    for (m, o, s, a) in pos_rows {
        pos_map.entry(m).or_default().push((o, s, a));
    }

    // overreaction_fade retired 2026-06-29 (unwired from the fusion engine) — excluded from the
    // scorecard so the UI doesn't show a permanently-dead row. The list lives at module scope
    // because the 7-day baseline SQL is generated from it — see SCORECARD_SIGNALS.
    const SIGNALS: [&str; SCORECARD_SIGNALS.len()] = SCORECARD_SIGNALS;
    let out: Vec<serde_json::Value> = markets
        .into_iter()
        .map(|(gid, slug, question, db_category, my, mn, active, closed, resolved, end_date)| {
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
            let position = pos_map.get(&gid).filter(|legs| !legs.is_empty()).map(|legs| {
                // Per-leg valuation: (current_mid − avg_entry) × shares for the side actually held.
                let leg_json: Vec<serde_json::Value> = legs
                    .iter()
                    .map(|(o, s, a)| {
                        let held_mid = if o.eq_ignore_ascii_case("yes") { my } else { mn };
                        let cost_basis = (*a * *s).round_dp(2);
                        let (mid_json, unrealized_json, market_value_json) = match held_mid {
                            Some(mid) => {
                                let mv = (mid * *s).round_dp(2);
                                (
                                    Some(mid.round_dp(4).to_string()),
                                    Some((mv - cost_basis).round_dp(2).to_string()),
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
                    })
                    .collect();
                // Totals across every leg — for a both-sides arb this is the only honest number,
                // since the legs are a hedged unit and either one alone misrepresents the trade.
                let total_cost: Decimal = legs.iter().map(|(_, s, a)| *a * *s).sum();
                let total_value: Decimal = legs
                    .iter()
                    .filter_map(|(o, s, _)| {
                        let mid = if o.eq_ignore_ascii_case("yes") { my } else { mn };
                        mid.map(|m| m * *s)
                    })
                    .sum();
                // Primary leg (largest cost basis) keeps the flat shape the card already renders.
                let primary = legs
                    .iter()
                    .max_by_key(|(_, s, a)| (*a * *s).round_dp(4))
                    .expect("non-empty");
                let (po, ps, pa) = primary;
                let primary_mid = if po.eq_ignore_ascii_case("yes") { my } else { mn };
                let primary_cost = (*pa * *ps).round_dp(2);
                serde_json::json!({
                    "outcome": po,
                    "shares": ps.round_dp(1).to_string(),
                    "avg_entry": pa.round_dp(4).to_string(),
                    "cost_basis": primary_cost.to_string(),
                    "mid": primary_mid.map(|m| m.round_dp(4).to_string()),
                    "market_value": primary_mid.map(|m| (m * *ps).round_dp(2).to_string()),
                    // Both-sides: report the HEDGED total, not the primary leg alone.
                    "unrealized": if legs.len() > 1 {
                        (total_value - total_cost).round_dp(2).to_string()
                    } else {
                        primary_mid.map(|m| ((m * *ps).round_dp(2) - primary_cost).round_dp(2).to_string()).unwrap_or_default()
                    },
                    "both_sides": legs.len() > 1,
                    "legs": leg_json,
                    "total_cost_basis": total_cost.round_dp(2).to_string(),
                    "total_market_value": total_value.round_dp(2).to_string(),
                })
            });
            // AWAITING RESOLUTION: held, past its own end date, and Polymarket still has it open.
            // Without this the board renders a parked position identically to a live one, which
            // reads as "stuck" when it is usually on schedule — the comparable PortWatch market
            // ("Strait of Hormuz traffic returns to normal by July 15") settled 160h past its end
            // date. Only shown for HELD markets: for everything else the state is not actionable.
            let awaiting_hours = end_date.filter(|_| position.is_some() && !closed).and_then(|ed| {
                let hrs = (chrono::Utc::now() - ed).num_minutes() as f64 / 60.0;
                (hrs > 0.0).then_some(hrs)
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
                "awaiting_hours": awaiting_hours.map(|h| format!("{h:.0}")),
                "end_date": end_date.map(|d| d.format("%Y-%m-%d %H:%MZ").to_string()),
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
  /* Parked, not live: past end date, waiting on Polymarket/UMA. Deliberately not red — this is a
     normal state that routinely runs days, not a fault. */
  .tag.await { color:#a371f7; border-color:#8957e555; }
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
    // A held market past its own end date is NOT live — it is parked waiting on Polymarket to close
    // it and on a UMA proposer to post an outcome. Rendering it as LIVE (which the board did until
    // 2026-08-01) makes an on-schedule wait look like a stuck position. Nothing on our side gates
    // this; settlement fires as soon as Gamma reports closed + a resolved outcome.
    const awaitingH = m.awaiting_hours!=null ? parseInt(m.awaiting_hours,10) : null;
    const awaitingLbl = awaitingH==null ? '' : (awaitingH < 48 ? `${awaitingH}h` : `${Math.floor(awaitingH/24)}d`);
    const statusTag = m.resolved_outcome ? `<span class="tag ${ (pos&&pos.outcome===m.resolved_outcome)?'won': (pos?'lost':'') }">RESOLVED · ${fmt(m.resolved_outcome)}</span>`
                     : (awaitingH!=null ? `<span class="tag await" title="Past its end date (${fmt(m.end_date)}). Polymarket has not closed this market yet and no UMA resolution has been proposed — nothing is blocked on our side; settlement fires automatically once it resolves. For reference a comparable IMF-PortWatch market settled 160h (6.7 days) after its end date.">AWAITING RESOLUTION · ${awaitingLbl}</span>`
                     : (m.active ? `<span class="tag" style="color:#3fb950;border-color:#23863655">LIVE</span>` : `<span class="tag">closed</span>`));
    const upnl = pos&&pos.unrealized!=null ? parseFloat(pos.unrealized) : null;
    // Both-sides (two-leg arb) shows the HEDGED pair, never one leg on its own — a leg in isolation
    // reads as a loss even when the pair has locked in a guaranteed profit.
    const holdLabel = pos ? (pos.both_sides ? `HEDGED Yes+No · ${pos.legs.map(l=>l.shares).join('/')} sh` : `HOLDING ${fmt(pos.outcome)} · ${pos.shares} sh`) : '';
    const holdTag = pos ? `<span class="tag hold">${holdLabel}${upnl!=null?` · <span class="pnl ${pnlCls(upnl)}">${upnl>=0?'+':''}$${upnl.toFixed(2)}</span>`:''}</span>` : '';
    const legLine = (l)=>`<span>${fmt(l.shares)} ${fmt(l.outcome)} @ ${fmt(l.avg_entry)}</span>${l.mid!=null?`<span class="muted">now ${l.mid}</span>`:''}${l.market_value!=null?`<span class="muted">· value $${l.market_value}</span>`:''}`;
    const posLine = !pos ? '' : (pos.both_sides ? `<div class="pos">
        <span class="lbl">Hedged pair</span>
        ${pos.legs.map(l=>`<div>${legLine(l)}</div>`).join("")}
        <div><span class="muted">cost $${pos.total_cost_basis} · value $${pos.total_market_value}</span>
        ${upnl!=null?`<span class="spacer"></span><span class="pnl ${pnlCls(upnl)}">${upnl>=0?'+':''}$${upnl.toFixed(2)} unrealized (pair)</span>`:''}</div>
      </div>` : `<div class="pos">
        <span class="lbl">Position</span>
        <span>${fmt(pos.shares)} ${fmt(pos.outcome)} @ ${fmt(pos.avg_entry)}</span>
        ${pos.mid!=null?`<span class="muted">now ${pos.mid}</span>`:''}
        ${pos.market_value!=null?`<span class="muted">· value $${pos.market_value}</span>`:''}
        ${upnl!=null?`<span class="spacer"></span><span class="pnl ${pnlCls(upnl)}">${upnl>=0?'+':''}$${upnl.toFixed(2)} unrealized</span>`:''}
      </div>`);
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
      </div>`:'<div class="sig muted" title="Decision reports are capped at 40 markets per 5-min cycle (DR_MARKET_LIMIT), prioritising the directional universe + bootstrap list. ~5.5k markets are DR-eligible, so most never get one — this is our own throughput cap, not a Polymarket delay. Arb-only markets never need a DR.">not in the decision-report pool</div>'}
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

/// Set which OpenRouter model (and reasoning effort) Hermes uses for reflection synthesis.
/// Writes an append-only `hermes_config` journal event; Hermes (src/bin/hermes.rs) re-reads the
/// latest one each reflection cycle (~10min), so this takes effect without a redeploy. Paper-only
/// dashboard control — never touches trading/risk config, only the optional LLM narrative layer.
async fn hermes_config_set_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<HermesConfigRequest>,
) -> impl IntoResponse {
    let model = request.model.trim();
    if !HERMES_ALLOWED_MODELS.contains(&model) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "ok": false,
                "error": "unknown model",
                "allowed_models": HERMES_ALLOWED_MODELS,
            })),
        )
            .into_response();
    }
    let reasoning_effort = request
        .reasoning_effort
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("none");
    if !HERMES_ALLOWED_REASONING_LEVELS.contains(&reasoning_effort) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "ok": false,
                "error": "unknown reasoning_effort",
                "allowed_reasoning_levels": HERMES_ALLOWED_REASONING_LEVELS,
            })),
        )
            .into_response();
    }

    let payload = serde_json::json!({
        "model": model,
        "reasoning_effort": reasoning_effort,
        "paper_only": true,
        "note": "Operator-set override for Hermes LLM synthesis (dashboard). Read by hermes each reflection cycle.",
    });
    let insert = sqlx::query(
        "INSERT INTO journal.events (id, event_type, source, severity, payload)
         VALUES ($1, 'hermes_config', 'dashboard_ui', 'info', $2)",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(&payload)
    .execute(&state.pool)
    .await;

    match insert {
        Ok(_) => Json(serde_json::json!({
            "ok": true,
            "model": model,
            "reasoning_effort": reasoning_effort,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "ok": false,
                "error": format!("Failed to write hermes_config event: {e}"),
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

#[derive(serde::Deserialize)]
#[allow(dead_code)]
struct L2DeriveReq {
    address: String,
    signature: String,
    timestamp: String,
    nonce: String,
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
            tracing::info!("No POLYMARKET_PRIVATE_KEY (or _FILE) found — L2 will stay in 'not connected' state until derived");
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

#[derive(serde::Deserialize)]
struct DryRunEventsQuery {
    limit: Option<i64>,
    gaps_only: Option<bool>,
}

fn clamp_dry_run_events_limit(limit: i64) -> i64 {
    limit.clamp(1, 50)
}

const STRATEGY_CANDIDATE_OBSERVATION_MAX_AGE_SECONDS: i64 = 15 * 60;

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
        "SELECT gamma_id, slug, question, category, last_mid_yes, last_mid_no, taker_fee_rate
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
    // Fee context is per-market/per-side (real Polymarket taker model: per-market rate × p × (1−p),
    // geopolitics free, makers never charged) — built inside the loop below.
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
        let fee_ctx = FeeContext {
            taker_fee_rate: market
                .taker_fee_rate
                .unwrap_or_else(|| crate::polymarket_taker_fee_rate(&market.slug)),
            price: target_mid,
            est_gas_usdc: Decimal::new(1, 2),
        };
        // Notional for costing: a realistic small position (the old code passed target_mid — a
        // PRICE in [0,1] — as the notional, making fee costing meaningless).
        let (_gross_edge, net_edge_after_fees, attribution) =
            engine.fuse_net(&snapshot, &context, Some(&fee_ctx), Decimal::from(10u64))?;
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

// (End L2 section. All prior Google behavior + paper paths preserved.
// Real derive reqwest + POLY_* + secret use for CLOB is future gated work per AGENTS + plan 3.4.)

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_seven_day_baseline_query_has_exactly_one_column_per_scorecard_signal() {
        // The bug this pins: the hand-written query kept a column for the retired
        // `overreaction_fade` while results were indexed positionally by SCORECARD_SIGNALS, so
        // theta read a dead signal's baseline, yahoo read theta's, and news read yahoo's. Three of
        // five live dashboard badges were artifacts. Columns are now generated from the same array
        // that indexes them, and this asserts the two can never diverge again.
        let sql = signal_fired_count_sql();
        let columns: Vec<&str> = sql.split("count(*) FILTER").skip(1).collect();
        assert_eq!(
            columns.len(),
            SCORECARD_SIGNALS.len(),
            "one FILTER column per signal, got {} for {} signals",
            columns.len(),
            SCORECARD_SIGNALS.len()
        );
        // ...and in order, so positional indexing of the result is correct by construction.
        for (col, name) in columns.iter().zip(SCORECARD_SIGNALS.iter()) {
            assert!(
                col.contains(&format!("'{name}'")),
                "column {col:?} should reference signal {name}"
            );
        }
        assert!(
            !sql.contains("overreaction_fade"),
            "the retired signal must not have a column"
        );
    }

    #[test]
    fn signal_health_flags_shifts() {
        let d = Decimal::from;
        // Too few recent reports to judge.
        assert_eq!(signal_health(d(20), d(0), 5), "insufficient_data");
        // The news-drop case: 24h baseline ~20%, recent 3h ~4% → more than halved.
        assert_eq!(signal_health(d(20), d(4), 200), "degraded");
        // Active signal gone fully silent.
        assert_eq!(signal_health(d(15), d(0), 200), "dormant");
        // Steady → ok.
        assert_eq!(signal_health(d(20), d(18), 200), "ok");
        // A previously-quiet signal waking up.
        assert_eq!(signal_health(d(2), d(25), 200), "elevated");
        // Quiet and still quiet → ok (don't cry wolf on dormant-by-design signals).
        assert_eq!(signal_health(d(0), d(0), 200), "ok");
        // Fire-rate doubled.
        assert_eq!(signal_health(d(10), d(25), 200), "elevated");
    }

    #[test]
    fn signal_health_long_baseline_catches_slow_decay() {
        let d = Decimal::from;
        // The blindspot the 7d baseline fixes: news_sentiment slow-decayed ~20% → ~1.8% over days.
        // The 3h-vs-24h check reads "ok" because the 24h baseline eroded along with the signal — recent
        // 3h (~1.8%) vs an already-eroded 24h baseline (~2%) is a steady ratio.
        let now_3h = Decimal::new(18, 1); // 1.8%
        let baseline_24h = d(2); // the 24h baseline has itself eroded to ~2%
        assert_eq!(
            signal_health(baseline_24h, now_3h, 200),
            "ok",
            "3h-vs-24h is blind to slow decay once the 24h baseline erodes"
        );
        // But comparing the 24h fire-rate (~2%) against the 7d baseline (~20%) flags the slide.
        let baseline_7d = d(20);
        let rate_24h = d(2);
        assert_eq!(
            signal_health(baseline_7d, rate_24h, 200),
            "degraded",
            "24h-vs-7d must flag the multi-day erosion"
        );
        // Fully silent over 24h vs a 7d-active baseline → dormant.
        assert_eq!(signal_health(d(20), d(0), 200), "dormant");
        // A genuinely dormant-by-design signal (quiet across BOTH windows) must NOT false-alarm.
        assert_eq!(signal_health(d(0), d(0), 200), "ok");
        // A signal steady over the week → ok (no trend alarm).
        assert_eq!(signal_health(d(20), d(19), 200), "ok");
    }

    #[test]
    fn dry_run_event_limit_is_clamped() {
        // clamp_review_events_limit was console-only (the review-queue handlers) and was removed
        // with them 2026-08-02; clamp_dry_run_events_limit stays — it bounds event-list limits for
        // strategy observations, paper orders, paper fills, and paper rejections (all still live).
        assert_eq!(clamp_dry_run_events_limit(-10), 1);
        assert_eq!(clamp_dry_run_events_limit(0), 1);
        assert_eq!(clamp_dry_run_events_limit(10), 10);
        assert_eq!(clamp_dry_run_events_limit(500), 50);
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
}
