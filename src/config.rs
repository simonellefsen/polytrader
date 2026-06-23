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

    /// Comma-separated list of market slugs (or ids) for focused ingestion + paper trading.
    /// Spans Geopolitics / Breaking / Finance plus Sports (the sports ones are arbitrage-only —
    /// see arb_only_markets). Finance/Tech with genuinely-uncertain prices are scarce on Polymarket
    /// right now, so the directional set is geopolitics-heavy but deliberately diversified: US 2028 /
    /// 2026-midterm control markets, Brazilian / French / UK / Israeli elections, China-Taiwan, the
    /// Iran cluster across horizons, plus Fed-rate and crypto markets — so signals aren't all
    /// correlated to one binary outcome. Sports (World Cup / NBA) are arbitrage-only (arb_only_markets).
    /// Every market still passes the per-trade risk gate (min net edge, exposure/concentration caps)
    /// before any paper position is opened, so widening this list only widens the opportunity funnel.
    #[arg(
        long,
        env = "POLYTRADER_BOOTSTRAP_MARKETS",
        default_value = "us-x-iran-permanent-peace-deal-by-june-30-2026-837-641-896-877-363-892-537-597,strait-of-hormuz-traffic-returns-to-normal-by-end-of-june,us-iran-nuclear-deal-by-june-30,will-donald-trump-announce-that-the-united-states-blockade-of-the-strait-of-hormuz-has-been-lifted-by-june-30-2026-159-962,us-x-iran-permanent-peace-deal-by-june-15-2026-734-856-129,us-announces-new-iran-agreementceasefire-extension-by-june-14,will-bitcoin-hit-150k-by-june-30-2026,strait-of-hormuz-traffic-returns-to-normal-by-july-31,us-x-iran-permanent-peace-deal-by-july-31-2026-831-252,will-benjamin-netanyahu-be-the-next-prime-minister-of-israel,us-x-iran-diplomatic-meeting-by-june-30-2026-983-259-948-431-294-182-296-883-134-598,us-announces-new-iran-agreementceasefire-extension-by-june-15-2889-539,will-donald-trump-announce-that-the-united-states-blockade-of-the-strait-of-hormuz-has-been-lifted-by-june-15-2026,will-gavin-newsom-win-the-2028-democratic-presidential-nomination-568,us-announces-new-iran-agreementceasefire-extension-by-june-30-848-925-757-129-337-165,us-x-iran-permanent-peace-deal-by-december-31-2026-961-587-341-574-555-817,will-donald-trump-announce-that-the-united-states-blockade-of-the-strait-of-hormuz-has-been-lifted-by-july-31-2026-495,will-jd-vance-win-the-2028-us-presidential-election,will-jd-vance-win-the-2028-republican-presidential-nomination,will-marco-rubio-win-the-2028-republican-presidential-nomination,will-the-republicans-win-the-2028-us-presidential-election,will-the-democrats-win-the-2028-us-presidential-election,will-the-republican-party-control-the-house-after-the-2026-midterm-elections,will-the-democratic-party-control-the-senate-after-the-2026-midterm-elections,2026-balance-of-power-d-senate-d-house-949,will-luiz-incio-lula-da-silva-win-the-2026-brazilian-presidential-election,will-flvio-bolsonaro-win-the-2026-brazilian-presidential-election,will-jordan-bardella-win-the-2027-french-presidential-election,will-douard-philippe-win-the-2027-french-presidential-election,will-andy-burnham-win-the-2026-makerfield-by-election,starmer-out-by-june-30-2026-862-594-548-219-739,will-gadi-eizenkot-be-the-next-prime-minister-of-israel,will-china-invade-taiwan-by-december-31-2027,us-x-iran-permanent-peace-deal-by-august-31-2026,us-iran-nuclear-deal-by-july-31,iran-agrees-to-end-enrichment-of-uranium-by-june-30,will-no-fed-rate-cuts-happen-in-2026,fed-rate-hike-in-2026,will-bitcoin-reach-90000-by-december-31-2026-113-862-581-343,will-ethereum-dip-to-800-by-december-31-2026-568,will-france-win-the-2026-fifa-world-cup-924,will-spain-win-the-2026-fifa-world-cup-963,will-the-new-york-knicks-win-the-2026-nba-finals,will-usa-win-the-2026-fifa-world-cup-467,will-england-win-the-2026-fifa-world-cup-937,will-argentina-win-the-2026-fifa-world-cup-245,will-portugal-win-the-2026-fifa-world-cup-912,will-brazil-win-the-2026-fifa-world-cup-183,will-germany-win-the-2026-fifa-world-cup-467,will-netherlands-win-the-2026-fifa-world-cup-739"
    )]
    pub bootstrap_markets: String,

    /// Slugs that are ARBITRAGE-ONLY: the autonomous directional executor skips them; only risk-free
    /// YES+NO arbitrage may trade them. Sports / World Cup go here (we don't take directional sports bets).
    #[arg(
        long,
        env = "POLYTRADER_ARB_ONLY_MARKETS",
        default_value = "will-france-win-the-2026-fifa-world-cup-924,will-spain-win-the-2026-fifa-world-cup-963,will-the-new-york-knicks-win-the-2026-nba-finals,will-usa-win-the-2026-fifa-world-cup-467,will-england-win-the-2026-fifa-world-cup-937,will-argentina-win-the-2026-fifa-world-cup-245,will-portugal-win-the-2026-fifa-world-cup-912,will-brazil-win-the-2026-fifa-world-cup-183,will-germany-win-the-2026-fifa-world-cup-467,will-netherlands-win-the-2026-fifa-world-cup-739"
    )]
    pub arb_only_markets: String,

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

    // === Polymarket L2 Trading Auth (2026-05-25 L2 pivot) ===
    // The private key used for L1 EIP-712 signing to derive L2 apiKey/secret/passphrase.
    // This is the key from your Polymarket account / deposit wallet.
    // Loaded via dotenvy (supports .env.local etc).
    // RISK: This key can create real L2 trading credentials. Never commit it.
    // We support both POLYMARKET_PRIVATE_KEY (recommended) and PRIVATE_KEY for compatibility
    // with the polymarket_credentials.py helper.
    #[arg(long, env = "POLYMARKET_PRIVATE_KEY", default_value = "")]
    pub polymarket_private_key: String,
}

impl Config {
    pub fn load() -> Self {
        dotenvy::from_filename(".env.local").ok();
        dotenvy::dotenv().ok();
        // The `backtest` subcommand carries its own flags (--min-net-edge, --weights, --since) that the
        // main clap parser doesn't know about. Strip everything from `backtest` onward before parsing so
        // `polytrader backtest ...` doesn't error here; main() reads the subcommand from the raw args.
        let raw: Vec<String> = std::env::args().collect();
        let argv: Vec<String> = match raw.iter().position(|a| a == "backtest") {
            Some(pos) => raw[..pos].to_vec(),
            None => raw,
        };
        let mut cfg = Self::parse_from(argv);

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

    /// Slugs that may only be traded via arbitrage (sports/World Cup); directional executor skips these.
    pub fn arb_only_market_list(&self) -> Vec<String> {
        self.arb_only_markets
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
