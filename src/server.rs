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

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Json, Redirect},
    routing::{get, post},
    Router,
};
use dioxus::prelude::*;
use rust_decimal::Decimal;
use serde::Serialize;
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
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
        .route("/", get(dashboard_handler))
        .route("/markets", get(markets_handler))
        .route("/paper/portfolio", get(portfolio_handler))
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
static SERVER_L2_SESSION_ID: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn get_l2_sessions() -> &'static Mutex<HashMap<String, L2Session>> {
    L2_SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn get_l2_secrets() -> &'static Mutex<HashMap<String, L2Secret>> {
    L2_SECRETS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn get_server_l2_session_id() -> &'static Mutex<Option<String>> {
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
) -> impl IntoResponse {
    //! RISK: Only use in paper mode. The private key allows deriving real L2 trading credentials.
    //! Secret material stays in process memory only (L2_SECRETS map).
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
    let live_sender_boundary = crate::clob::live_sender::build_live_sender_boundary_status();
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

    let readiness = build_order_placement_readiness(
        l2_connected,
        clob_read_ok,
        &preflight,
        &review_health,
        &final_review_audit,
        &crate::clob::live_sender::build_live_sender_boundary_status(),
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
        "final_review_audit": final_review_audit,
        "errors": {
            "clob": clob_error,
            "review": review_error,
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
    let report = crate::clob::live_sender::build_live_sender_boundary_status();
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
        &crate::clob::live_sender::build_live_sender_boundary_status(),
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
    Json(request): Json<FinalReviewDecisionRequest>,
) -> impl IntoResponse {
    //! Record an operator decision against a final-review readiness packet.
    //! This is audit-only. It deliberately cannot approve real orders, cannot
    //! open the kill switch, and cannot create a live sender.
    let decision = normalize_final_review_decision(&request.decision);
    let operator = request.operator.as_deref().unwrap_or("unspecified").trim();
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
            "note": "Final review decision validation failed; no decision event was journaled and no order can be sent."
        }))
        .into_response();
    }

    let decision = decision.expect("validated decision");
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
            "note": "Final review decision recorded for audit only. It does not authorize or enable real order submission."
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
    Json(mut request): Json<crate::clob::authenticated::OrderSubmitFacadeRequest>,
) -> impl IntoResponse {
    //! Evaluate the fail-closed real-order submission facade. This route exists
    //! to prove the shape and auditability of a future submit path while keeping
    //! live order placement blocked by approval, kill-switch, exposure, config,
    //! journaling, and paper-mode gates.
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
                    let reconciliation =
                        report.get("reconciliation").cloned().unwrap_or_else(|| {
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
                    let reconciliation_payload = serde_json::json!({
                        "kind": "clob_order_submit_reconciliation",
                        "submit_facade_event_id": event_id,
                        "submit_decision": report.get("submit_decision").cloned().unwrap_or(serde_json::json!("rejected_fail_closed")),
                        "reconciliation_status": report.get("reconciliation_status").cloned().unwrap_or(serde_json::json!("reconciled_no_send")),
                        "reconciliation": reconciliation,
                        "request_sent": false,
                        "would_send": false,
                        "post_order_called": false,
                        "post_orders_called": false,
                        "paper_only": true,
                        "real_orders_enabled": false,
                        "ready_for_real_orders": false,
                        "note": "Submit facade reconciliation event. The facade rejected before send, so exchange state is reconciled as no order created."
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
                            let response =
                                merge_journal_fields(report, true, Some(event_id), None);
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
                                merge_journal_fields(report, true, Some(event_id), None),
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
    Json(request): Json<HumanApprovalRequest>,
) -> impl IntoResponse {
    //! Record a human approval decision for a future submit-facade attempt. This
    //! is an audit workflow only: it creates a short-lived journal event keyed
    //! to a deterministic intent subject hash. It does not approve live sending.
    let decision = normalize_human_approval_decision(&request.decision);
    let subject_hash =
        crate::clob::authenticated::approval_subject_hash_for_intent(&request.intent);
    let operator = request.operator.as_deref().unwrap_or("unspecified");
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
            "note": "Human approval workflow validation failed; no approval event was journaled and no order can be sent."
        }))
        .into_response();
    }

    let decision = decision.expect("validated decision");
    let approved_for_facade = decision == "approve_facade";
    let now = chrono::Utc::now();
    let expires_at = now + chrono::Duration::minutes(15);
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
            "note": "Human approval workflow event recorded for submit-facade validation only. It does not authorize live order submission."
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
}

#[derive(serde::Deserialize)]
struct FinalReviewDecisionRequest {
    final_review_event_id: uuid::Uuid,
    decision: String,
    note: Option<String>,
    operator: Option<String>,
    confirm_final_review_workflow: bool,
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
        "live_order_sender_implemented": false,
        "next_safe_step": "Draft and review a live-sender design/ADR while real trading remains locked; do not implement sending until external collateral, allowance, unlock, kill switch, and paper-mode gates are deliberately addressed.",
        "note": "Live-sender design readiness only. This reports blockers before any live sender implementation and cannot enable or place real orders."
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
        "live_order_sender_implemented": false,
        "next_safe_step": "Review this contract as an ADR/wiki decision. If accepted, the next code step is a fail-closed trait boundary only, still with no network sender and no real-order authority.",
        "note": "Live-sender design review contract only. It can make the design reviewable, but it cannot permit implementation, enable trading, or place orders."
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
        });
    }

    let min_seconds = latencies_seconds.iter().min().copied().unwrap_or(0);
    let max_seconds = latencies_seconds.iter().max().copied().unwrap_or(0);
    let total_seconds: i64 = latencies_seconds.iter().sum();
    let avg_seconds = total_seconds / latencies_seconds.len() as i64;

    serde_json::json!({
        "reviewed_count": latencies_seconds.len(),
        "min_seconds": min_seconds,
        "max_seconds": max_seconds,
        "avg_seconds": avg_seconds,
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

    let mut reasons = Vec::new();
    if unreviewed_count > 0 {
        reasons.push("unreviewed_dry_runs");
    }
    if guidance_differs > 0 {
        reasons.push("guidance_exceptions");
    }
    if max_latency_seconds >= REVIEW_LATENCY_SLOW_AFTER_SECONDS {
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
    );

    serde_json::json!({
        "status": status,
        "reasons": reasons,
        "reason_details": {
            "unreviewed_count": unreviewed_count,
            "guidance_exception_count": guidance_differs,
            "max_latency_seconds": max_latency_seconds,
            "slow_latency_after_seconds": REVIEW_LATENCY_SLOW_AFTER_SECONDS,
        },
        "recommended_actions": recommended_actions,
        "dry_run_count": dry_run_count,
        "unreviewed_count": unreviewed_count,
        "guidance_exception_count": guidance_differs,
        "max_latency_seconds": max_latency_seconds,
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
    final_review_audit: &serde_json::Value,
    live_sender_boundary: &serde_json::Value,
) -> serde_json::Value {
    let review_available =
        review_health.get("status").and_then(|v| v.as_str()) != Some("unavailable");
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
            };
        }
    }) else {
        return crate::clob::authenticated::HumanApprovalValidation {
            valid: false,
            event_id: Some(event_id),
            decision: None,
            subject_hash: Some(expected_subject_hash),
            blockers: vec!["human_approval_event_not_found".to_string()],
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

    crate::clob::authenticated::HumanApprovalValidation {
        valid: blockers.is_empty(),
        event_id: Some(event_id),
        decision,
        subject_hash: Some(actual_subject_hash),
        blockers,
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
                "max_seconds": REVIEW_LATENCY_SLOW_AFTER_SECONDS
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

    #[test]
    fn operator_status_actions_include_fail_closed_live_sender_boundary() {
        let review_health = serde_json::json!({
            "recommended_actions": [{"id": "none", "severity": "info"}]
        });
        let final_review_audit = serde_json::json!({"status": "audited"});
        let live_sender_boundary = crate::clob::live_sender::build_live_sender_boundary_status();
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

    #[test]
    fn order_placement_readiness_reports_real_order_gap() {
        let preflight = serde_json::json!({
            "checks": [
                {"name": "collateral_balance_positive", "ok": false},
                {"name": "collateral_allowance_positive", "ok": false}
            ]
        });
        let review_health = serde_json::json!({
            "status": "needs_attention"
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
            &final_review_audit,
            &crate::clob::live_sender::build_live_sender_boundary_status(),
        );
        let blockers = readiness["blockers"].as_array().expect("blockers");
        let expected_completed_count =
            if real_order_reconciliation_available() && market_metadata_validation_available() {
                14
            } else if real_order_reconciliation_available() {
                13
            } else if kill_switch_and_exposure_limits_available() {
                12
            } else if human_approval_workflow_available() {
                11
            } else if order_submit_facade_available() {
                10
            } else if order_post_request_dry_run_available() {
                8
            } else if signed_order_payload_dry_run_available() {
                7
            } else {
                6
            };

        assert_eq!(readiness["ready"], false);
        assert_eq!(readiness["stage"], "authenticated_read_and_paper_dry_run");
        assert_eq!(readiness["completed_count"], expected_completed_count);
        assert_eq!(readiness["required_count"], 17);
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
    }

    #[test]
    fn order_placement_readiness_blocks_missing_final_review_audit() {
        let preflight = serde_json::json!({
            "checks": [
                {"name": "collateral_balance_positive", "ok": false},
                {"name": "collateral_allowance_positive", "ok": false}
            ]
        });
        let review_health = serde_json::json!({"status": "ok"});
        let final_review_audit = serde_json::json!({
            "status": "missing_audit",
            "count": 0
        });

        let readiness = build_order_placement_readiness(
            true,
            true,
            &preflight,
            &review_health,
            &final_review_audit,
            &crate::clob::live_sender::build_live_sender_boundary_status(),
        );
        let blockers = readiness["blockers"].as_array().expect("blockers");

        assert_eq!(readiness["required_count"], 17);
        assert_eq!(readiness["final_review_audit_status"], "missing_audit");
        assert!(blockers
            .iter()
            .any(|blocker| blocker == "final_review_decision_audit"));
    }

    #[test]
    fn final_review_readiness_aggregates_latest_gate_evidence() {
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
            &crate::clob::live_sender::build_live_sender_boundary_status(),
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
}
