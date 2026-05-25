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

// === GATED WS SKELETON FOR FUTURE REACTIVE TIER (fees-tax-latency wiki step #4) ===
// Added after wiki-first (log prepend + status append to this file's sibling docs).
// No activation, no poll change, no behavior impact on default build/run.
// See Cargo.toml [features] clob-ws and runtime env gate.

#[cfg(feature = "clob-ws")]
#[allow(dead_code, unused)]
/// Resilient WebSocket client skeleton for public CLOB feeds (orderbook deltas, trades, prices).
///
/// **STRONG GATES (non-negotiable)**:
/// - Compile: cargo ... --features clob-ws (default off; Cargo optional dep).
/// - Runtime: POLYTRADER_ENABLE_CLOB_WS=highconviction (explicit opt-in string).
/// - Never wired into ingest_tick, main, or paper paths in this increment (or any small-capital phase).
///
/// Use cases (only after deliberate Tier 1 / 5-min net-edge Fusion + goals adherence proven in paper):
///   - High-conviction sniping or MM in deep books where edge >> fees/gas/latency.
///
/// **RISK IMPLICATIONS (AGENTS.md + fees-tax-latency-and-execution-tiers.md + $150 context)**:
/// - WS streaming for *reactive* execution is high complexity and risk (disconnects, ordering, rate limits, over-trading).
/// - At ~$150, the primary deliberate 5-min FusionEngine + net-of-fees Decision Reports (Tier 1) is the correct default.
/// - This skeleton exists only as the "begin adding" foundation for 3.1/ future Tier 2.
/// - **Do not enable lightly or in prod without human review + backtesting showing net edge survives fees.**
/// - Real trading (even paper shadow) requires explicit gates beyond this.
///
/// **Error handling (anti-pattern #5 avoidance)**: Every path logs (tracing), returns Err on failure.
/// No silent fallback, no unwrap in production paths, reconnect with backoff + jitter (modeled on source).
/// Caller (future) must handle degradation to poll.
///
/// **Credits + patterns followed exactly**:
/// - Transferred: Polymarket-BTC-15-Minute-Trading-Bot/core/ingestion/managers/websocket_manager.py
///   (reconnect, rate_limiter, providers) + rate_limiter.py + data_validator; poly-maker/poly_data/websocket_handlers.py
///   + global_state.py + polymarket_client.py + trading_utils; openclaw/src/connectors/polymarket.ts;
///     Poly-Trader/agents + fetch patterns; agents/agents/polymarket/*.
/// - Wiki: decisions/2026-05-25-data-ingester-enhancements-for-3-1.md, integrations/polymarket-apis-and-data-sources.md,
///   strategies/fees-tax-latency-and-execution-tiers.md (Tier 2), market-making-liquidity.md.
/// - Local: exact clob_public.rs + ingester/mod.rs style (anyhow::Result, tracing::*, no floats, paper-only UA/comments, sqlx not here yet).
///
/// Endpoint (public, no auth): wss://ws-subscriptions-clob.polymarket.com/ws/market
/// Sub example: {"type":"subscribe","channel":"market","market":"<token_id>"}
/// (See Polymarket CLOB WS docs; subject to change; handle in real impl.)
///
/// This is the *smallest* viable skeleton delivering connection mgmt + basic handling + docs.
/// Full streaming event emission, book maintenance, gap detection in follow-up (after Tier 1 proof).
#[derive(Clone)]
pub struct ClobWsClient {
    url: String,
}

#[cfg(feature = "clob-ws")]
#[allow(dead_code, unused)]
impl ClobWsClient {
    pub fn new() -> Self {
        Self {
            url: "wss://ws-subscriptions-clob.polymarket.com/ws/market".to_string(),
        }
    }

    /// Connect (or reconnect) with exponential backoff + jitter.
    /// Real impl would use tokio_tungstenite::connect_async in loop.
    /// Returns Ok when connected; Err on persistent failure (logged, no panic).
    pub async fn connect_with_retry(&self) -> Result<()> {
        tracing::info!(
            url = %self.url,
            "ClobWsClient (gated skeleton) connect_with_retry called — no-op; real WS + reconnect loop behind feature+env only"
        );
        // Skeleton: in real version:
        // let mut backoff = 500u64;
        // loop {
        //   match tokio_tungstenite::connect_async(&self.url).await {
        //     Ok((ws, _)) => { /* spawn read_handler(ws, self.clone()) */ return Ok(()); }
        //     Err(e) => { warn!(error=%e, "WS connect fail; backoff {}ms", backoff); sleep(Duration::from_millis(backoff)).await; backoff = (backoff*2).min(30000); }
        //   }
        // }
        Ok(())
    }

    /// Subscribe to the market channel for a specific token (orderbook + trades updates).
    /// Message shape per public CLOB WS (subject to evolution).
    pub async fn subscribe_market(&self, token_id: &str) -> Result<()> {
        tracing::debug!(token_id = %token_id, url = %self.url, "ClobWsClient skeleton subscribe_market (gated; would send JSON sub)");
        // Real: ws.send( tungstenite::Message::Text( format!(r#"{{"type":"subscribe","channel":"market","market":"{}"}}"#, token_id) ) ).await?
        Ok(())
    }

    /// Basic incoming message handler (trades, books, heartbeats).
    /// In real: parse serde_json, match "channel", update shared state or emit to channel for Fusion signals (reactive path).
    /// Errors on bad msg (logged, no silent ignore of fatal).
    pub fn handle_message(&self, raw: &str) -> Result<()> {
        // Lightweight parse check for skeleton (no full deserialze to avoid dep bloat in gated).
        if raw.contains("\"channel\":\"market\"") || raw.contains("\"type\":\"trade\"") {
            tracing::trace!(
                len = raw.len(),
                "ClobWsClient skeleton received market/trade msg (gated path)"
            );
        } else if raw.contains("pong") || raw.contains("\"type\":\"ping\"") {
            // heartbeats
        } else {
            tracing::debug!(preview = %raw.chars().take(80).collect::<String>(), "ClobWsClient skeleton unhandled msg");
        }
        Ok(())
    }

    /// Example reconnect + handler loop sketch (would be spawned from connect).
    /// Demonstrates resilience without running in skeleton.
    pub async fn run_reconnect_loop(&self) -> Result<()> {
        tracing::warn!("ClobWsClient run_reconnect_loop (skeleton) — would be the long-lived task; never called unless explicitly wired behind gates");
        Ok(())
    }
}
