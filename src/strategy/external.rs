//! External advisory signals: Yahoo Finance (asset spot) + newsdata.io (headline sentiment).
//!
//! ## Design
//! The `SignalProcessor` trait is synchronous, so external data is PRE-FETCHED in the async DR
//! generator (Yahoo via `fetch_yahoo_context`; news via `fetch_newsdata_news`, cached + budget-capped
//! in main.rs) and injected into the fusion snapshot under `"external"`. The two processors below
//! then read that data synchronously — the same pattern used for `recent_move`. No new dependencies.
//!
//! ## Advisory only (per product decision)
//! Both processors emit LOW confidence (≤0.30) so they nudge the fused edge but never dominate; the
//! RiskManager net-edge gate still governs execution, and Hermes' weight loop learns their value.
//! Yahoo's chart endpoint is free/unauthenticated; newsdata.io uses a free key (200 credits/day) so
//! news is cached per market with a daily budget. X/Bloomberg need paid API keys (out of scope).
//!
//! ## Honesty
//! These are deliberately crude heuristics (a linear spot-vs-threshold lean; a keyword-polarity
//! lexicon over headlines). They are a starting point for Hermes to evaluate, not a validated alpha
//! source. Everything is paper-only and fully journaled.

use crate::strategy::{Signal, SignalProcessor};
use anyhow::Result;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde_json::json;

// ===================== Pre-fetch (async, called from the DR generator) =====================
// Yahoo (free, unmetered) is fetched each cycle; news goes through newsdata.io which is metered
// (200 credits/day on the free plan) and is therefore CACHED with a daily budget cap in main.rs.

/// Yahoo Finance context for asset-price markets: {symbol, spot, threshold, ratio_to_threshold}.
/// Returns None for non-asset markets or on fetch failure (processor then emits a zero signal).
pub async fn fetch_yahoo_context(
    client: &reqwest::Client,
    slug: &str,
) -> Option<serde_json::Value> {
    let (symbol, threshold) = asset_market(slug)?;
    let spot = fetch_yahoo_spot(client, symbol).await?;
    let ratio = if threshold > Decimal::ZERO {
        (spot / threshold).round_dp(4)
    } else {
        Decimal::ZERO
    };
    Some(json!({
        "symbol": symbol,
        "spot": spot.to_string(),
        "threshold": threshold.to_string(),
        "ratio_to_threshold": ratio.to_string(),
    }))
}

/// Map an asset-price market slug to a Yahoo ticker + numeric threshold (e.g. "150k" → 150000).
/// Returns None for non-asset markets.
fn asset_market(slug: &str) -> Option<(&'static str, Decimal)> {
    let s = slug.to_lowercase();
    let symbol = if s.contains("bitcoin") || s.contains("btc") {
        "BTC-USD"
    } else if s.contains("ethereum") || s.contains("eth") {
        "ETH-USD"
    } else if s.contains("solana") || s.contains("sol-") {
        "SOL-USD"
    } else {
        return None;
    };
    let threshold = parse_threshold(&s)?;
    Some((symbol, threshold))
}

/// Parse a threshold like "150k" / "64k" / "3000" from a slug. Returns the first numeric token,
/// multiplying by 1000 when immediately suffixed with 'k'.
fn parse_threshold(s: &str) -> Option<Decimal> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let num: Decimal = s[start..i].parse().ok()?;
            // Skip trivially small numbers that are clearly dates/ids would need context; here we
            // only treat a number directly followed by 'k' OR ≥1000 as a price threshold.
            let has_k = i < bytes.len() && (bytes[i] == b'k' || bytes[i] == b'K');
            if has_k {
                return Some(num * dec!(1000));
            }
            if num >= dec!(1000) {
                return Some(num);
            }
            // otherwise keep scanning (e.g. skip "2026")
        } else {
            i += 1;
        }
    }
    None
}

/// Fetch the latest spot price for a ticker from Yahoo's public chart endpoint.
/// Reference data only (not P&L) — converted to Decimal immediately.
async fn fetch_yahoo_spot(client: &reqwest::Client, symbol: &str) -> Option<Decimal> {
    let url =
        format!("https://query1.finance.yahoo.com/v8/finance/chart/{symbol}?interval=1d&range=1d");
    let resp = client.get(&url).send().await.ok()?;
    let v: serde_json::Value = resp.json().await.ok()?;
    let price = v["chart"]["result"][0]["meta"]["regularMarketPrice"].as_f64()?;
    Decimal::from_f64_retain(price).map(|d| d.round_dp(2))
}

/// Build a concise newsdata.io `q` from the market slug: drop stopwords / bare year tokens, keep the
/// topic nouns (e.g. "will-bitcoin-hit-150k-by-june-30-2026" → "bitcoin hit 150k june"). A focused
/// query returns more on-topic results per credit than the full question sentence.
pub fn newsdata_query(slug: &str) -> String {
    const STOP: [&str; 16] = [
        "will", "the", "by", "be", "there", "a", "an", "of", "to", "is", "on", "in", "for", "and",
        "x", "us",
    ];
    let words: Vec<String> = slug
        .split('-')
        .filter(|w| !w.is_empty())
        .filter(|w| !w.chars().all(|c| c.is_ascii_digit())) // drop bare numbers like "2026", "30"
        .filter(|w| !STOP.contains(&w.to_lowercase().as_str()))
        .take(6)
        .map(|w| w.to_string())
        .collect();
    words.join(" ")
}

/// One newsdata.io `/api/1/latest` call (costs 1 API credit). Returns (headline_count, polarity,
/// top_titles). Sentiment is paid-only, so polarity is computed from title+description via the
/// keyword lexicon. `size=10` is credit-optimal (1 credit regardless of size, up to 10 free).
///
/// CALLER MUST METER THIS: the free plan is 200 credits/day. main.rs caches results per market and
/// enforces a daily budget — do not call this on every cycle.
pub async fn fetch_newsdata_news(
    client: &reqwest::Client,
    api_key: &str,
    query: &str,
) -> Option<(usize, Decimal, Vec<String>)> {
    let resp = client
        .get("https://newsdata.io/api/1/latest")
        .query(&[
            ("apikey", api_key),
            ("q", query),
            ("language", "en"),
            ("size", "10"),
        ])
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        tracing::warn!(status = %resp.status(), "newsdata.io non-200 (budget/rate?); skipping");
        return None;
    }
    let v: serde_json::Value = resp.json().await.ok()?;
    if v["status"].as_str() != Some("success") {
        return None;
    }
    let results = v["results"].as_array()?;
    let mut titles = Vec::new();
    let mut texts = Vec::new();
    for a in results.iter().take(10) {
        if let Some(t) = a["title"].as_str() {
            titles.push(t.to_string());
            texts.push(t.to_string());
        }
        if let Some(d) = a["description"].as_str() {
            texts.push(d.to_string());
        }
    }
    if titles.is_empty() {
        return None;
    }
    let polarity = keyword_polarity(&texts);
    Some((titles.len(), polarity, titles.into_iter().take(6).collect()))
}

/// Crude keyword-polarity over headlines → [-1, 1]. Deliberately simple; advisory only.
fn keyword_polarity(titles: &[String]) -> Decimal {
    const POS: [&str; 14] = [
        "deal",
        "agreement",
        "ceasefire",
        "peace",
        "truce",
        "approve",
        "approved",
        "win",
        "wins",
        "surge",
        "rally",
        "gain",
        "rise",
        "resolve",
    ];
    const NEG: [&str; 16] = [
        "strike",
        "strikes",
        "attack",
        "war",
        "collapse",
        "fail",
        "fails",
        "reject",
        "crash",
        "plunge",
        "fall",
        "sanction",
        "escalate",
        "escalation",
        "threat",
        "killed",
    ];
    let mut pos = 0i32;
    let mut neg = 0i32;
    for t in titles {
        let lt = t.to_lowercase();
        for w in POS {
            if lt.contains(w) {
                pos += 1;
            }
        }
        for w in NEG {
            if lt.contains(w) {
                neg += 1;
            }
        }
    }
    let total = pos + neg;
    if total == 0 {
        return Decimal::ZERO;
    }
    (Decimal::from(pos - neg) / Decimal::from(total)).round_dp(3)
}

// ===================== Processors (sync, read injected snapshot["external"]) =====================

/// Yahoo Finance advisory: for asset markets, leans YES when spot is at/above the market's implied
/// threshold and against YES when far below. Counterweights naive overreaction on asset extremes.
pub struct YahooFinanceProcessor;

impl SignalProcessor for YahooFinanceProcessor {
    fn name(&self) -> &'static str {
        "yahoo_finance"
    }

    fn compute_signal(
        &self,
        snapshot: &serde_json::Value,
        _ctx: &serde_json::Value,
    ) -> Result<Signal> {
        let y = &snapshot["external"]["yahoo"];
        if !y.is_object() {
            return Ok(zero("yahoo_finance", json!({"reason": "no_yahoo_data"})));
        }
        let ratio = dec_str(&y["ratio_to_threshold"]);
        let target = snapshot["target_outcome"].as_str().unwrap_or("Yes");
        // Lean toward YES grows as spot approaches/exceeds the threshold; bounded to [-1, 1].
        let lean_yes = (ratio - dec!(1)).clamp(dec!(-1), dec!(1));
        let score = if target.eq_ignore_ascii_case("No") {
            -lean_yes
        } else {
            lean_yes
        };
        Ok(Signal {
            processor_name: "yahoo_finance",
            score,
            confidence: dec!(0.30), // advisory
            edge: Some(score * dec!(0.30)),
            metadata: json!({
                "advisory": true,
                "source": "yahoo_finance",
                "ratio_to_threshold": ratio.to_string(),
                "spot": y["spot"],
                "threshold": y["threshold"],
                "target_outcome": target,
            }),
        })
    }
}

/// News sentiment advisory: orients crude headline polarity toward the target outcome, with
/// confidence scaling weakly with headline volume (capped low).
pub struct NewsSentimentProcessor;

impl SignalProcessor for NewsSentimentProcessor {
    fn name(&self) -> &'static str {
        "news_sentiment"
    }

    fn compute_signal(
        &self,
        snapshot: &serde_json::Value,
        _ctx: &serde_json::Value,
    ) -> Result<Signal> {
        let n = &snapshot["external"]["news"];
        if !n.is_object() {
            return Ok(zero("news_sentiment", json!({"reason": "no_news_data"})));
        }
        let count = n["headline_count"].as_i64().unwrap_or(0);
        if count == 0 {
            return Ok(zero("news_sentiment", json!({"reason": "no_headlines"})));
        }
        let polarity = dec_str(&n["polarity"]); // [-1, 1]
        let target = snapshot["target_outcome"].as_str().unwrap_or("Yes");
        let score = if target.eq_ignore_ascii_case("No") {
            -polarity
        } else {
            polarity
        };
        // Confidence: 0.10 base + up to ~0.20 for volume, capped at 0.30 (advisory).
        let vol_factor = (Decimal::from(count.min(8)) / dec!(8)) * dec!(0.20);
        let confidence = (dec!(0.10) + vol_factor).min(dec!(0.30));
        Ok(Signal {
            processor_name: "news_sentiment",
            score,
            confidence,
            edge: Some(score * confidence),
            metadata: json!({
                "advisory": true,
                "source": "newsdata.io",
                "basis": "crude_keyword_polarity",
                "headline_count": count,
                "polarity": polarity.to_string(),
                "target_outcome": target,
                "top_titles": n["top_titles"],
            }),
        })
    }
}

fn dec_str(v: &serde_json::Value) -> Decimal {
    v.as_str()
        .and_then(|s| s.parse::<Decimal>().ok())
        .unwrap_or(Decimal::ZERO)
}

fn zero(name: &'static str, metadata: serde_json::Value) -> Signal {
    Signal {
        processor_name: name,
        score: Decimal::ZERO,
        confidence: Decimal::ZERO,
        edge: Some(Decimal::ZERO),
        metadata,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::SignalProcessor;

    #[test]
    fn asset_market_parses_ticker_and_threshold() {
        assert_eq!(
            asset_market("will-bitcoin-hit-150k-by-june-30-2026"),
            Some(("BTC-USD", dec!(150000)))
        );
        assert_eq!(
            asset_market("bitcoin-above-64k-on-june-13-2026"),
            Some(("BTC-USD", dec!(64000)))
        );
        assert_eq!(
            asset_market("will-france-win-the-2026-fifa-world-cup"),
            None
        );
    }

    #[test]
    fn keyword_polarity_leans_with_headlines() {
        let pos = vec![
            "Ceasefire agreement reached in peace deal".to_string(),
            "Both sides approve truce".to_string(),
        ];
        assert!(keyword_polarity(&pos) > Decimal::ZERO);
        let neg = vec!["Strikes escalate as war threat grows".to_string()];
        assert!(keyword_polarity(&neg) < Decimal::ZERO);
        let neutral = vec!["A quiet day in the markets".to_string()];
        assert_eq!(keyword_polarity(&neutral), Decimal::ZERO);
    }

    #[test]
    fn newsdata_query_drops_stopwords_and_years() {
        assert_eq!(
            super::newsdata_query("will-bitcoin-hit-150k-by-june-30-2026"),
            "bitcoin hit 150k june"
        );
        // stopwords (us, x, by, the) and bare numbers dropped; topic nouns kept (max 6).
        let q = super::newsdata_query("us-x-iran-permanent-peace-deal-by-june-30-2026");
        assert!(q.contains("iran") && q.contains("peace") && q.contains("deal"));
        assert!(!q.split(' ').any(|w| w == "us" || w == "x" || w == "2026"));
    }

    #[test]
    fn yahoo_processor_orients_to_target_outcome() {
        // spot far below threshold (ratio 0.43) → lean against YES.
        let snap = json!({
            "target_outcome": "Yes",
            "external": {"yahoo": {"ratio_to_threshold": "0.43", "spot": "64000", "threshold": "150000"}}
        });
        let sig = YahooFinanceProcessor
            .compute_signal(&snap, &json!({}))
            .unwrap();
        assert!(sig.score < Decimal::ZERO); // against buying YES
        assert_eq!(sig.confidence, dec!(0.30));
        // Same data, target = No → score flips positive.
        let snap_no = json!({
            "target_outcome": "No",
            "external": {"yahoo": {"ratio_to_threshold": "0.43", "spot": "64000", "threshold": "150000"}}
        });
        let sig_no = YahooFinanceProcessor
            .compute_signal(&snap_no, &json!({}))
            .unwrap();
        assert!(sig_no.score > Decimal::ZERO);
    }

    #[test]
    fn news_processor_zero_without_data() {
        let sig = NewsSentimentProcessor
            .compute_signal(&json!({"target_outcome": "Yes"}), &json!({}))
            .unwrap();
        assert_eq!(sig.score, Decimal::ZERO);
        assert_eq!(sig.confidence, Decimal::ZERO);
    }
}
