//! Public (unauthenticated) CLOB endpoints for live orderbook & trade data.
//! Base: https://clob.polymarket.com/
//!
//! Paper-only reads. Used by ingester and PaperTradingEngine for realistic matching/slippage.

use anyhow::Result;
use reqwest::Client;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceSize {
    pub price: String,
    pub size: String,
}

#[derive(Debug, Clone)]
pub struct OrderbookSnapshot {
    #[allow(dead_code)]
    pub token_id: String,
    pub bids: Vec<PriceSize>,
    pub asks: Vec<PriceSize>,
    pub mid: Option<Decimal>,
    pub fetched_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone)]
pub struct ClobPublicClient {
    http: Client,
    base: String,
}

impl ClobPublicClient {
    pub fn new() -> Self {
        Self {
            http: Client::builder()
                .user_agent("polytrader/0.1 (paper-only)")
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("http client"),
            base: "https://clob.polymarket.com".to_string(),
        }
    }

    /// Fetch full orderbook for a specific outcome token (Yes or No share).
    /// Endpoint confirmed public: /book?token_id=...
    pub async fn get_orderbook(&self, token_id: &str) -> Result<OrderbookSnapshot> {
        let url = format!("{}/book?token_id={}", self.base, token_id);
        #[derive(Deserialize)]
        struct BookResp {
            bids: Vec<PriceSize>,
            asks: Vec<PriceSize>,
        }
        let resp: BookResp = self.http.get(&url).send().await?.json().await?;

        // Try to also fetch authoritative mid (best effort)
        let mid = self.get_midpoint(token_id).await.ok();

        Ok(OrderbookSnapshot {
            token_id: token_id.to_string(),
            bids: resp.bids,
            asks: resp.asks,
            mid,
            fetched_at: chrono::Utc::now(),
        })
    }

    /// Ticker-like: current mid price for token (string in response).
    pub async fn get_midpoint(&self, token_id: &str) -> Result<Decimal> {
        let url = format!("{}/midpoint?token_id={}", self.base, token_id);
        #[derive(Deserialize)]
        struct MidResp {
            mid: String,
        }
        let resp: MidResp = self.http.get(&url).send().await?.json().await?;
        Decimal::from_str(&resp.mid).map_err(|e| anyhow::anyhow!("bad decimal mid: {}", e))
    }

    /// Convenience: compute mid from book if midpoint endpoint unavailable (depth weighted simple).
    pub fn mid_from_book(book: &OrderbookSnapshot) -> Option<Decimal> {
        let best_bid = book
            .bids
            .first()
            .and_then(|p| Decimal::from_str(&p.price).ok());
        let best_ask = book
            .asks
            .first()
            .and_then(|p| Decimal::from_str(&p.price).ok());
        match (best_bid, best_ask) {
            (Some(b), Some(a)) if a > b => Some((b + a) / Decimal::from(2)),
            (Some(b), None) => Some(b),
            (None, Some(a)) => Some(a),
            _ => None,
        }
    }
}
