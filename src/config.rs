//! Configuration loading (env, clap, files).
//! All trading modes, risk limits, API endpoints, LLM settings live here.
//!
//! PAPER-ONLY SAFETY: load() asserts mode == "paper". Never remove this gate.

use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(
    author,
    version,
    about = "polytrader - Polymarket paper-first agent (paper mode only)"
)]
pub struct Config {
    /// Trading mode. ONLY "paper" supported in Phase 0/1/2. Real mode behind future gates.
    #[arg(long, env = "POLYTRADER_MODE", default_value = "paper")]
    pub mode: String,

    /// Postgres DSN (required via env or DATABASE_URL_FILE fallback after parse)
    #[arg(long, env = "DATABASE_URL", default_value = "")]
    pub database_url: String,

    /// Log level filter
    #[arg(long, env = "RUST_LOG", default_value = "info,polytrader=debug")]
    pub log_level: String,

    /// Taker fee in basis points for paper market orders (default 50 = 0.5%, matches typical Polymarket taker).
    #[arg(long, env = "POLYTRADER_PAPER_FEE_BPS", default_value_t = 50)]
    pub paper_fee_bps: u16,

    /// Comma-separated list of market slugs (or ids) for focused ingestion + paper trading in Phase 0.
    #[arg(
        long,
        env = "POLYTRADER_BOOTSTRAP_MARKETS",
        default_value = "will-bitcoin-hit-150k-by-june-30-2026"
    )]
    pub bootstrap_markets: String,

    /// Ingestion poll interval in seconds (conservative to respect public rate limits).
    #[arg(long, env = "POLYTRADER_INGEST_INTERVAL_SECS", default_value_t = 300)]
    pub ingest_interval_secs: u64,

    /// Starting virtual USDC balance for paper portfolio (whole dollars).
    #[arg(long, env = "POLYTRADER_INITIAL_PAPER_USDC", default_value_t = 10000)]
    pub initial_paper_usdc: u64,

    /// Subpath prefix when the app is deployed behind a path-based reverse proxy / ingress
    /// (e.g. "/polytrader" when exposed at https://.../polytrader).
    /// Leave empty for root deployment.
    /// The app will serve its UI/API under this prefix while still exposing /health at root for probes.
    #[arg(long, env = "SUBPATH_PREFIX", default_value = "")]
    pub subpath_prefix: String,

    // === AUTH (Next Phase 2026-05-25, IMPL 5701dfea; dual edge+app Google OAuth for UI) ===
    // RISK (AGENTS + security reviewer): Secrets only via env (never code, never logged).
    // GOOGLE_REDIRECT_URI *must* be the full public URL including subpath (e.g.
    // https://unground-...ngrok-free.dev/polytrader/auth/callback) for subpath deploys
    // behind rewrite; mismatch = open redirect or Google reject. ALLOWED_EMAILS or empty=any
    // (paper only; $150 personal data exposure risk even in sim for future attribution).
    // Cookie flags (HttpOnly/SameSite/Path=prefix/Secure opt) critical to mitigate hijack/CSRF
    // under /polytrader subpath + ngrok edge. Dual trusts ngrok forwarded headers (if present)
    // else in-app cookie. No new migs/deps. See server.rs for full handlers + extractor.
    // Credits: AGENTS.md, prior ngrok deploy entries (edge SSO context), no UI auth from 5 repos.
    #[arg(long, env = "GOOGLE_CLIENT_ID", default_value = "")]
    pub google_client_id: String,

    #[arg(long, env = "GOOGLE_CLIENT_SECRET", default_value = "")]
    pub google_client_secret: String,

    #[arg(long, env = "GOOGLE_REDIRECT_URI", default_value = "")]
    pub google_redirect_uri: String,

    /// Comma-separated allowlist (empty = any email for paper mode only).
    #[arg(long, env = "AUTH_ALLOWED_EMAILS", default_value = "")]
    pub allowed_emails: String,

    /// Whether auth cookies should be marked Secure (true for https prod; false ok for local http paper dev).
    #[arg(long, env = "AUTH_COOKIE_SECURE", default_value_t = false)]
    pub auth_cookie_secure: bool,
}

impl Config {
    pub fn load() -> Self {
        dotenvy::dotenv().ok();
        let mut cfg = Self::parse();

        // Robust credential loading (best practice for Kubernetes + CNPG)
        if cfg.database_url.is_empty() {
            // 1. Explicit env var (highest priority)
            if let Ok(v) = std::env::var("DATABASE_URL") {
                if !v.is_empty() {
                    cfg.database_url = v;
                }
            }
            // 2. File mounted from secret (very reliable pattern)
            if cfg.database_url.is_empty() {
                if let Ok(path) = std::env::var("DATABASE_URL_FILE") {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        let trimmed = content.trim().to_string();
                        if !trimmed.is_empty() {
                            cfg.database_url = trimmed;
                        }
                    }
                }
            }
            // 3. Legacy CNPG key name
            if cfg.database_url.is_empty() {
                if let Ok(v) = std::env::var("uri") {
                    if !v.is_empty() {
                        cfg.database_url = v;
                    }
                }
            }
        }

        // NON-NEGOTIABLE PAPER SAFETY GATE (AGENTS.md)
        let mode = cfg.mode.to_lowercase();
        assert!(
            mode == "paper",
            "FATAL: POLYTRADER_MODE must be exactly 'paper' (got '{}'). Real trading is disabled.",
            cfg.mode
        );

        if cfg.database_url.is_empty() {
            panic!(
                "FATAL: No database connection string found. \
                 Provide DATABASE_URL env, DATABASE_URL_FILE pointing to a secret file, or secret key 'uri'."
            );
        }

        cfg
    }

    pub fn bootstrap_market_list(&self) -> Vec<String> {
        self.bootstrap_markets
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Returns the normalized subpath prefix (always starts with / if non-empty, no trailing slash).
    pub fn normalized_subpath_prefix(&self) -> String {
        let prefix = self.subpath_prefix.trim();
        if prefix.is_empty() {
            return String::new();
        }
        let mut p = if prefix.starts_with('/') {
            prefix.to_string()
        } else {
            format!("/{}", prefix)
        };
        if p.ends_with('/') && p.len() > 1 {
            p.pop();
        }
        p
    }

    /// True if Google OAuth creds appear configured (for login route availability).
    pub fn auth_enabled(&self) -> bool {
        !self.google_client_id.is_empty() && !self.google_client_secret.is_empty()
    }

    /// Parse allowed emails to vec (empty vec = any for paper).
    pub fn allowed_emails_list(&self) -> Vec<String> {
        if self.allowed_emails.trim().is_empty() {
            return vec![];
        }
        self.allowed_emails
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect()
    }
}
