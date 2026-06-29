//! Gamma API client (markets, events, prices, resolutions).
//! Base URL: https://gamma-api.polymarket.com/
//!
//! Paper-only: only public read endpoints. Used for ingester to discover live markets.
//!
//! get_market is provided for future but unused in Phase 0 tick (list suffices).
#![allow(dead_code)]

use anyhow::Result;
use reqwest::Client;
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
pub struct Market {
    pub id: String,
    pub slug: String,
    pub question: String,
    pub outcomes: Vec<String>,
    pub clob_token_ids: Vec<String>,
    pub active: bool,
    pub closed: bool,
    /// Gamma `outcomePrices`, aligned with `outcomes`. After resolution one is "1" and the rest "0".
    pub outcome_prices: Vec<String>,
    /// Gamma `endDate` (ISO 8601, e.g. "2026-06-30T12:00:00Z"), if present. Resolution time — feeds the
    /// theta/convergence signal's days-to-resolution. Serialized into `raw_json` as `end_date`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_date: Option<String>,
    /// Polymarket taker fee RATE from Gamma's `feeSchedule.rate` (0 when `feesEnabled` is false, e.g.
    /// geopolitics). The effective fee is `shares × rate × price × (1 − price)`. None when the fields
    /// are absent — callers fall back to the category default. Stored per-market so historical P&L
    /// reflects the fee schedule in force.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub taker_fee_rate: Option<Decimal>,
}

#[derive(Clone)]
pub struct GammaClient {
    http: Client,
    base: String,
}

impl GammaClient {
    pub fn new() -> Self {
        Self {
            http: Client::builder()
                .user_agent("polytrader/0.1 (paper-only)")
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("http client"),
            base: "https://gamma-api.polymarket.com".to_string(),
        }
    }

    /// Parse a single Gamma market JSON object into our Market struct.
    /// Gamma returns `outcomes` and `clobTokenIds` as JSON-encoded *strings*, hence the inner parse.
    fn parse_market(v: &Value) -> Market {
        let outcomes_str = v.get("outcomes").and_then(|o| o.as_str()).unwrap_or("[]");
        let outcomes: Vec<String> = serde_json::from_str(outcomes_str).unwrap_or_default();

        let clob_str = v
            .get("clobTokenIds")
            .and_then(|c| c.as_str())
            .unwrap_or("[]");
        let clob_token_ids: Vec<String> = serde_json::from_str(clob_str).unwrap_or_default();

        let prices_str = v
            .get("outcomePrices")
            .and_then(|c| c.as_str())
            .unwrap_or("[]");
        let outcome_prices: Vec<String> = serde_json::from_str(prices_str).unwrap_or_default();

        Market {
            id: v
                .get("id")
                .and_then(|i| i.as_str())
                .unwrap_or("")
                .to_string(),
            slug: v
                .get("slug")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
            question: v
                .get("question")
                .and_then(|q| q.as_str())
                .unwrap_or("")
                .to_string(),
            outcomes,
            clob_token_ids,
            active: v.get("active").and_then(|a| a.as_bool()).unwrap_or(false),
            closed: v.get("closed").and_then(|c| c.as_bool()).unwrap_or(true),
            outcome_prices,
            end_date: v
                .get("endDate")
                .and_then(|d| d.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string()),
            taker_fee_rate: {
                // feesEnabled=false ⇒ fee-free (0). Otherwise take feeSchedule.rate. Absent ⇒ None.
                let enabled = v.get("feesEnabled").and_then(|b| b.as_bool());
                let rate = v
                    .get("feeSchedule")
                    .and_then(|f| f.get("rate"))
                    .and_then(|r| r.as_f64())
                    .and_then(Decimal::from_f64_retain);
                match (enabled, rate) {
                    (Some(false), _) => Some(Decimal::ZERO),
                    (_, Some(r)) => Some(r),
                    _ => None,
                }
            },
        }
    }

    /// Fetch specific markets by slug (public Gamma /markets?slug=...).
    ///
    /// This is the bootstrap path: querying each configured slug directly *guarantees* the market
    /// is fetched, instead of hoping it appears in a generic top-N list (which previously caused
    /// `processed: 0` — the bootstrap slug was never in the first 20 active markets returned).
    /// Robust: a failed/empty slug query is logged and skipped, never aborts the whole tick.
    pub async fn fetch_markets_by_slugs(&self, slugs: &[String]) -> Result<Vec<Market>> {
        let mut out = Vec::new();
        for slug in slugs {
            // Default Gamma /markets?slug=X returns the market ONLY while it is open — a RESOLVED
            // market is dropped from this query. If we stop here, the moment a held market resolves
            // we go blind to it: `closed`/resolved prices are never ingested, `resolved_outcome`
            // stays NULL, and settle_resolved_positions never fires (collateral locked forever).
            let url = format!("{}/markets?slug={}", self.base, slug);
            let mut items = self.get_markets_json(&url, slug).await;

            // Fallback: a slug missing from the open query is almost always resolved/closed. Re-query
            // including closed markets so we capture the resolution (closed=true + final outcomePrices),
            // which is exactly what the settlement path needs. This is the fix for "0 settlements
            // despite markets resolving".
            if items.is_empty() {
                let closed_url = format!("{}/markets?slug={}&closed=true", self.base, slug);
                items = self.get_markets_json(&closed_url, slug).await;
                if !items.is_empty() {
                    tracing::info!(slug = %slug, "gamma slug resolved/closed; ingested via closed=true fallback (will settle)");
                }
            }

            if items.is_empty() {
                tracing::warn!(slug = %slug, "gamma returned no market for bootstrap slug even with closed=true (renamed/delisted?)");
            }
            out.extend(items.iter().map(Self::parse_market));
        }
        tracing::debug!(count = out.len(), "gamma fetched markets by slug");
        Ok(out)
    }

    /// GET a Gamma /markets URL and parse the JSON array, logging+swallowing errors (returns empty).
    /// Keeps `fetch_markets_by_slugs` robust: one bad slug/response never aborts the tick.
    async fn get_markets_json(&self, url: &str, slug: &str) -> Vec<Value> {
        match self.http.get(url).send().await {
            Ok(resp) => match resp.json::<Vec<Value>>().await {
                Ok(items) => items,
                Err(e) => {
                    tracing::warn!(slug = %slug, error = %e, "gamma slug response parse failed; skipping");
                    Vec::new()
                }
            },
            Err(e) => {
                tracing::warn!(slug = %slug, error = %e, "gamma slug request failed; skipping");
                Vec::new()
            }
        }
    }

    /// List active markets (public Gamma /markets, up to limit).
    /// Used when no bootstrap allowlist is configured (discovery mode).
    pub async fn list_active_markets(&self) -> Result<Vec<Market>> {
        let url = format!("{}/markets?limit=20&active=true", self.base);
        let resp: Vec<Value> = self.http.get(&url).send().await?.json().await?;
        let out: Vec<Market> = resp.iter().map(Self::parse_market).collect();
        tracing::debug!(count = out.len(), "gamma listed active markets");
        Ok(out)
    }

    pub async fn get_market(&self, market_id: &str) -> Result<Option<Market>> {
        // For bootstrap, reuse list and find; or call /markets/{id} if exists.
        let markets = self.list_active_markets().await?;
        Ok(markets.into_iter().find(|m| m.id == market_id))
    }
}
