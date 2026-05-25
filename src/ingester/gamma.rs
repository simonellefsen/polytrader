//! Gamma API client (markets, events, prices, resolutions).
//! Base URL: https://gamma-api.polymarket.com/
//!
//! Paper-only: only public read endpoints. Used for ingester to discover live markets.
//!
//! get_market is provided for future but unused in Phase 0 tick (list suffices).
#![allow(dead_code)]

use anyhow::Result;
use reqwest::Client;
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

    /// List active markets (public Gamma /markets, up to limit).
    /// Filtering to bootstrap allowlist is now done exclusively by caller (ingest_tick + Config)
    /// so the list is fully driven by POLYTRADER_BOOTSTRAP_MARKETS at runtime (no hard-coded const).
    pub async fn list_active_markets(&self) -> Result<Vec<Market>> {
        let url = format!("{}/markets?limit=20&active=true", self.base);
        let resp: Vec<Value> = self.http.get(&url).send().await?.json().await?;

        let mut out = Vec::new();
        for v in resp {
            let slug = v
                .get("slug")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();

            let outcomes_str = v.get("outcomes").and_then(|o| o.as_str()).unwrap_or("[]");
            let outcomes: Vec<String> = serde_json::from_str(outcomes_str).unwrap_or_default();

            let clob_str = v
                .get("clobTokenIds")
                .and_then(|c| c.as_str())
                .unwrap_or("[]");
            let clob_token_ids: Vec<String> = serde_json::from_str(clob_str).unwrap_or_default();

            out.push(Market {
                id: v
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_string(),
                slug,
                question: v
                    .get("question")
                    .and_then(|q| q.as_str())
                    .unwrap_or("")
                    .to_string(),
                outcomes,
                clob_token_ids,
                active: v.get("active").and_then(|a| a.as_bool()).unwrap_or(false),
                closed: v.get("closed").and_then(|c| c.as_bool()).unwrap_or(true),
            });
        }
        tracing::debug!(count = out.len(), "gamma listed bootstrap markets");
        Ok(out)
    }

    pub async fn get_market(&self, market_id: &str) -> Result<Option<Market>> {
        // For bootstrap, reuse list and find; or call /markets/{id} if exists.
        let markets = self.list_active_markets().await?;
        Ok(markets.into_iter().find(|m| m.id == market_id))
    }
}
