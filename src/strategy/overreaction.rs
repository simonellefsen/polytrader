//! "Nothing Ever Happens" — overreaction fade signal processor.
//!
//! ## Thesis
//! Prediction markets frequently overprice dramatic outcomes driven by news, social media
//! sentiment, and recency bias. The crowd over-reacts; prices overshoot; and when the
//! dramatic event fails to materialise (which is most of the time), prices mean-revert.
//!
//! This processor fades extreme prices: if YES is priced above ~72 % it is likely
//! overpriced; below ~28 % it is likely underpriced. Orderbook imbalance (ask-heavy vs
//! bid-heavy) is used as a secondary confirmation signal.
//!
//! ## Signal conventions
//! - score > 0 → buy the underdog (the cheaper, faded side)
//! - score = 0 → no overreaction detected (market near fair value)
//! - Confidence: 0.30–0.65 (never high; heuristic only)
//!
//! ## RISK
//! Mean-reversion fails when the catalyst IS real (e.g. breaking news, legal ruling).
//! Always combine with the fusion gate (min net_edge_after_fees) and fractional Kelly
//! sizing from `risk::RiskManager`. Paper-only.

use crate::strategy::{Signal, SignalProcessor};
use anyhow::Result;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde_json::json;
use tracing::warn;

const HIGH_THRESHOLD: Decimal = dec!(0.72); // YES priced here or above → potential over-hype
const LOW_THRESHOLD: Decimal = dec!(0.28); // YES priced here or below → potential over-fear
const IMBALANCE_CONFIRM_RATIO: Decimal = dec!(1.5); // ask/bid ratio to call an imbalance

pub struct OverreactionProcessor;

impl SignalProcessor for OverreactionProcessor {
    fn name(&self) -> &'static str {
        "overreaction_fade"
    }

    fn compute_signal(
        &self,
        snapshot: &serde_json::Value,
        _ctx: &serde_json::Value,
    ) -> Result<Signal> {
        // Accept either "target_mid" (from 5-min DR) or "mid" (from server candidates).
        let mid = parse_mid(snapshot)?;

        if mid <= Decimal::ZERO || mid >= Decimal::ONE {
            warn!(mid = %mid, processor = "overreaction_fade", "mid out of (0,1); skipping");
            return Ok(zero_signal(
                json!({"reason": "invalid_mid", "mid": mid.to_string()}),
            ));
        }

        let distance = (mid - dec!(0.5)).abs(); // 0..0.5

        let is_high = mid >= HIGH_THRESHOLD;
        let is_low = mid <= LOW_THRESHOLD;

        if !is_high && !is_low {
            return Ok(zero_signal(json!({
                "reason": "near_neutral",
                "mid": mid.to_string(),
                "distance": distance.to_string()
            })));
        }

        // VOLATILITY GUARD: only fade an extreme price if it is NEW — i.e. the result of a recent
        // sharp move (genuine crowd overreaction). A long-standing extreme (e.g. a longshot that has
        // sat at 0.003 for weeks) is almost certainly *correctly* priced, not an overreaction, so
        // fading it is a losing bet. recent_move (|Δmid| over ~3h) is supplied by the DR generator;
        // when present and below threshold we suppress the signal. (Absent ⇒ no guard, e.g. the
        // observational server candidates path.)
        const MOVE_THRESHOLD: Decimal = dec!(0.07);
        if let Some(rm) = snapshot
            .get("recent_move")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<Decimal>().ok())
        {
            if rm < MOVE_THRESHOLD {
                return Ok(zero_signal(json!({
                    "reason": "no_recent_move_extreme_likely_correct",
                    "mid": mid.to_string(),
                    "recent_move": rm.to_string(),
                    "move_threshold": MOVE_THRESHOLD.to_string()
                })));
            }
        }

        // Orderbook imbalance (optional — not always in snapshot).
        let (bid_depth, ask_depth) = book_depths(snapshot);
        let imbalance_ratio = if bid_depth > Decimal::ZERO {
            ask_depth / bid_depth
        } else if ask_depth > Decimal::ZERO {
            dec!(999) // one-sided book; all sellers
        } else {
            Decimal::ONE // no book data; treat as balanced
        };

        // Score: base from price extremeness + bonus if book confirms the fade.
        // Max realistic score ≈ 0.55 (distance 0.3 × 0.8 + 0.15 bonus)
        let (score, direction) = if is_high {
            // YES overpriced → buy NO
            let bonus = if imbalance_ratio >= IMBALANCE_CONFIRM_RATIO {
                dec!(0.15) // ask-heavy confirms expensive YES
            } else {
                Decimal::ZERO
            };
            (distance * dec!(0.8) + bonus, "buy_no_fade_yes")
        } else {
            // YES underpriced → buy YES
            let bid_dominant_ratio = if ask_depth > Decimal::ZERO {
                bid_depth / ask_depth
            } else {
                Decimal::ONE
            };
            let bonus = if bid_dominant_ratio >= IMBALANCE_CONFIRM_RATIO {
                dec!(0.15)
            } else {
                Decimal::ZERO
            };
            (distance * dec!(0.8) + bonus, "buy_yes_fade_no")
        };

        // Confidence: 0.30 base + up to 0.20 for extreme price + 0.15 for imbalance.
        let imbalance_strength =
            ((imbalance_ratio - Decimal::ONE).abs()).min(dec!(3)) / dec!(3) * dec!(0.15);
        let confidence = (dec!(0.30) + distance * dec!(0.40) + imbalance_strength).min(dec!(0.65));

        Ok(Signal {
            processor_name: self.name(),
            score,
            confidence,
            edge: Some(score * confidence),
            metadata: json!({
                "thesis": "nothing_ever_happens_overreaction_fade",
                "mid": mid.to_string(),
                "distance_from_neutral": distance.to_string(),
                "direction": direction,
                "bid_depth": bid_depth.to_string(),
                "ask_depth": ask_depth.to_string(),
                "imbalance_ratio": imbalance_ratio.to_string(),
                "high_threshold": HIGH_THRESHOLD.to_string(),
                "low_threshold": LOW_THRESHOLD.to_string(),
            }),
        })
    }
}

fn parse_mid(snapshot: &serde_json::Value) -> Result<Decimal> {
    // Try field names used by different callers
    for key in &["target_mid", "mid"] {
        if let Some(s) = snapshot[key].as_str() {
            return s
                .parse::<Decimal>()
                .map_err(|e| anyhow::anyhow!("parse '{key}': {e}"));
        }
    }
    Err(anyhow::anyhow!("snapshot missing 'target_mid'/'mid' field"))
}

fn book_depths(snapshot: &serde_json::Value) -> (Decimal, Decimal) {
    let sum = |arr: Option<&Vec<serde_json::Value>>| {
        arr.unwrap_or(&vec![])
            .iter()
            .filter_map(|l| l["size"].as_str()?.parse::<Decimal>().ok())
            .fold(Decimal::ZERO, |acc, x| acc + x)
    };
    (
        sum(snapshot["bids"].as_array()),
        sum(snapshot["asks"].as_array()),
    )
}

fn zero_signal(metadata: serde_json::Value) -> Signal {
    Signal {
        processor_name: "overreaction_fade",
        score: Decimal::ZERO,
        confidence: Decimal::ZERO,
        edge: Some(Decimal::ZERO),
        metadata,
    }
}
