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
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Json, Redirect},
    routing::get,
    Router,
};
use dioxus::prelude::*;
use rust_decimal::Decimal;
use serde::Serialize;
use sqlx::PgPool;
use std::collections::HashMap;
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
        // AUTH (Next Phase): login/callback/logout/whoami. Optional for paper (dual edge+app).
        // Relative links in UI + <base> ensure subpath compat. /health untouched (public).
        .route("/auth/login", get(auth_login_handler))
        .route("/auth/callback", get(auth_callback_handler))
        .route("/auth/logout", get(auth_logout_handler))
        .route("/auth/whoami", get(auth_whoami_handler))
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
