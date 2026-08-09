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
    ///
    /// Returns `Ok(None)` for the CLOB's canonical "no live orderbook for this token" response
    /// (404 `{"error":"No orderbook exists for the requested token id"}`) — confirmed live
    /// 2026-07-22 while investigating a ~13/hour "CLOB orderbook fetch failed" WARN pattern: every
    /// affected token traced back to arb-discovery-pool candidates (5-min BTC updown rounds,
    /// scheduled-but-not-yet-started esports/tennis matches) that genuinely have zero resting
    /// orders yet — a real, frequent, and entirely expected state for that pool, not a fetch
    /// failure. The old code called `.json::<BookResp>()` unconditionally, so this 404's error body
    /// (which has neither `bids` nor `asks`) failed struct deserialization and surfaced as a WARN
    /// ("error decoding response body") indistinguishable from an actual problem. `Err` is now
    /// reserved for genuine failures (network errors, timeouts, unexpected non-JSON bodies) that
    /// still deserve a caller's attention.
    pub async fn get_orderbook(&self, token_id: &str) -> Result<Option<OrderbookSnapshot>> {
        let url = format!("{}/book?token_id={}", self.base, token_id);
        #[derive(Deserialize)]
        struct BookResp {
            bids: Vec<PriceSize>,
            asks: Vec<PriceSize>,
        }
        let resp = self.http.get(&url).send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body: BookResp = resp.json().await?;

        // Try to also fetch authoritative mid (best effort)
        let mid = self.get_midpoint(token_id).await.ok();

        Ok(Some(OrderbookSnapshot {
            token_id: token_id.to_string(),
            bids: body.bids,
            asks: body.asks,
            mid,
            fetched_at: chrono::Utc::now(),
        }))
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

    /// Midpoint from a book, used ONLY when the API omits `mid`.
    ///
    /// Takes the max bid and min ask rather than `bids.first()` / `asks.first()`, because **CLOB
    /// snapshots are not sorted best-first** — the same fact `match_against_book` documents and
    /// sorts for before walking levels. Measured 2026-08-09 across 566 live books: **566 of 566**
    /// had a non-best first bid AND a non-best first ask, and reading position 0 as "best" gave a
    /// midpoint off by **0.38 on average, up to 0.4975** — i.e. roughly the (worst_bid + worst_ask)/2
    /// ≈ 0.5 artifact previously blamed on empty books.
    ///
    /// This path is currently DEAD — 0 of 4,699 snapshots in an hour had a null `mid`, so the API
    /// always supplies one and this never runs. It is fixed anyway: the cost is four lines, and the
    /// failure mode if the API ever stops populating `mid` is silent, systematic, and lands directly
    /// in `last_mid_*` — which feeds unrealized P&L, the board, and the drawdown breaker's input.
    pub fn mid_from_book(book: &OrderbookSnapshot) -> Option<Decimal> {
        let best_bid = book
            .bids
            .iter()
            .filter_map(|p| Decimal::from_str(&p.price).ok())
            .max();
        let best_ask = book
            .asks
            .iter()
            .filter_map(|p| Decimal::from_str(&p.price).ok())
            .min();
        match (best_bid, best_ask) {
            (Some(b), Some(a)) if a > b => Some((b + a) / Decimal::from(2)),
            (Some(b), None) => Some(b),
            (None, Some(a)) => Some(a),
            _ => None,
        }
    }
}

// The gated `ClobWsClient` skeleton that used to sit here is now a real client in
// `super::clob_ws`, behind the same two gates (Cargo feature `clob-ws` + runtime
// `POLYTRADER_ENABLE_CLOB_WS`). Its job was to hold the shape of the thing until Path B of the P5
// criterion read GO; it has.

#[cfg(test)]
mod mid_tests {
    use super::*;
    use crate::ingester::PriceSize;

    fn book(bids: &[&str], asks: &[&str]) -> OrderbookSnapshot {
        let lv = |xs: &[&str]| {
            xs.iter()
                .map(|p| PriceSize {
                    price: p.to_string(),
                    size: "100".to_string(),
                })
                .collect::<Vec<_>>()
        };
        OrderbookSnapshot {
            token_id: "t".into(),
            bids: lv(bids),
            asks: lv(asks),
            mid: None,
            fetched_at: chrono::Utc::now(),
        }
    }

    /// The regression this guards: CLOB snapshots are NOT sorted best-first. Reading position 0 as
    /// the best price gave a midpoint off by 0.38 on average across all 566 live books measured
    /// 2026-08-09 -- roughly (worst_bid + worst_ask)/2, which lands near 0.5 and looks like a
    /// plausible price rather than an obvious error.
    #[test]
    fn the_midpoint_uses_best_prices_even_when_levels_arrive_worst_first() {
        // Worst-first ordering, as observed live: bids descending from the WORST, asks from the
        // WORST. Best bid 0.78, best ask 0.79 -> true mid 0.785.
        let b = book(&["0.01", "0.40", "0.78"], &["0.99", "0.85", "0.79"]);
        assert_eq!(
            ClobPublicClient::mid_from_book(&b),
            Some(Decimal::from_str("0.785").unwrap())
        );
        // Position-0 reading would have produced (0.01 + 0.99)/2 = 0.50 -- the artifact.
        assert_ne!(
            ClobPublicClient::mid_from_book(&b),
            Some(Decimal::from_str("0.5").unwrap())
        );
        // Already best-first is unaffected.
        let sorted = book(&["0.78", "0.40"], &["0.79", "0.99"]);
        assert_eq!(
            ClobPublicClient::mid_from_book(&sorted),
            Some(Decimal::from_str("0.785").unwrap())
        );
    }

    /// One-sided and empty books keep their previous meaning: quote the side that exists, and
    /// refuse rather than invent when neither does.
    #[test]
    fn one_sided_and_empty_books_are_unchanged() {
        assert_eq!(
            ClobPublicClient::mid_from_book(&book(&["0.10", "0.60"], &[])),
            Some(Decimal::from_str("0.60").unwrap())
        );
        assert_eq!(
            ClobPublicClient::mid_from_book(&book(&[], &["0.90", "0.30"])),
            Some(Decimal::from_str("0.30").unwrap())
        );
        assert_eq!(ClobPublicClient::mid_from_book(&book(&[], &[])), None);
    }
}
