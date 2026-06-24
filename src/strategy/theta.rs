//! Theta / convergence signal processor.
//!
//! ## Thesis
//! As a binary market approaches resolution, price should converge toward its outcome (0 or 1) and the
//! time-uncertainty premium decays ("theta"). Empirically prediction markets also exhibit a mild
//! favorite-longshot bias and slow convergence — favorites tend to drift up to 1, clear underdogs down
//! to 0. This processor leans toward the side the market already favors, scaled by BOTH the strength of
//! that lean and how near resolution is, and stays silent far from resolution or on a coin-flip (no
//! clear convergence direction). It is intentionally LOW confidence — an advisory tilt the fusion gate,
//! fractional Kelly, and Hermes's learned weight still govern.
//!
//! ## Signal conventions (shared frame: "edge for buying the TARGET outcome")
//! - The pipeline targets the cheaper side, so `target_mid` is usually ≤ 0.5 (an underdog). For an
//!   underdog the convergence lean is DOWNWARD → score < 0 (do NOT buy a longshot about to expire) —
//!   a useful brake on `overreaction_fade`, which buys cheap underdogs on mean-reversion. A favored
//!   target (mid > 0.5) leans upward → score > 0.
//! - score = lean × urgency × GAIN, where lean = mid − 0.5 and urgency ramps 0→1 as resolution nears.
//! - score = 0 (neutral) when: no resolution date, outside the [0, HORIZON]-day window, or |lean| tiny
//!   (a true coin-flip has no convergence direction). Neutral also covers markets not yet re-ingested
//!   with an end date — so the signal is dormant-by-design until the data is present.
//!
//! ## RISK
//! Convergence fails on genuine upsets, and a pure price+time tilt is a weak prior — hence the low
//! confidence cap and the net-edge gate + Kelly. Paper-only; Hermes measures realized P&L and tunes the
//! weight (trims it if it does not pay).

use crate::strategy::{Signal, SignalProcessor};
use anyhow::Result;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde_json::json;

/// Only fire within this many days of resolution — theta is meaningless far out.
const HORIZON_DAYS: Decimal = dec!(14);
/// Score gain on (lean × urgency). Keeps |score| modest (≤ 0.5 × 0.5 × 1 = 0.25).
const GAIN: Decimal = dec!(0.5);
/// Minimum |mid − 0.5| to call a convergence direction (below = a coin-flip, no edge).
const MIN_LEAN: Decimal = dec!(0.03);

pub struct ThetaConvergenceProcessor;

impl SignalProcessor for ThetaConvergenceProcessor {
    fn name(&self) -> &'static str {
        "theta_convergence"
    }

    fn compute_signal(
        &self,
        snapshot: &serde_json::Value,
        _ctx: &serde_json::Value,
    ) -> Result<Signal> {
        // Days to resolution is supplied by the DR generator (computed from the market's end date).
        // Absent (not re-ingested yet, or the observational server path) ⇒ dormant, not an error.
        let days = match snapshot
            .get("days_to_resolution")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<Decimal>().ok())
        {
            Some(d) => d,
            None => return Ok(neutral(json!({"reason": "no_resolution_date"}))),
        };
        // Window gate: only near resolution. Overdue/disputed (days < 0) markets don't converge
        // normally, so we stay out of them too.
        if days < Decimal::ZERO || days > HORIZON_DAYS {
            return Ok(neutral(json!({
                "reason": "outside_horizon",
                "days_to_resolution": days.to_string(),
                "horizon_days": HORIZON_DAYS.to_string(),
            })));
        }

        let mid = match parse_mid(snapshot) {
            Some(m) if m > Decimal::ZERO && m < Decimal::ONE => m,
            _ => return Ok(neutral(json!({"reason": "invalid_or_missing_mid"}))),
        };

        let lean = mid - dec!(0.5); // signed: >0 target favored, <0 underdog
        if lean.abs() < MIN_LEAN {
            return Ok(neutral(json!({
                "reason": "coin_flip_no_direction",
                "mid": mid.to_string(),
                "lean": lean.to_string(),
            })));
        }

        // urgency (theta): 0 at the horizon, 1 at resolution. days ∈ [0, HORIZON] ⇒ urgency ∈ [0, 1].
        let urgency = ((HORIZON_DAYS - days) / HORIZON_DAYS)
            .max(Decimal::ZERO)
            .min(Decimal::ONE);
        let score = (lean * urgency * GAIN).round_dp(4);
        // Low/moderate confidence, growing as resolution nears; capped well below the strong signals.
        let confidence = (dec!(0.20) + dec!(0.25) * urgency).min(dec!(0.45));

        Ok(Signal {
            processor_name: self.name(),
            score,
            confidence,
            edge: Some((score * confidence).round_dp(4)),
            metadata: json!({
                "thesis": "theta_convergence_to_resolution",
                "mid": mid.to_string(),
                "lean": lean.to_string(),
                "days_to_resolution": days.to_string(),
                "urgency": urgency.round_dp(4).to_string(),
                "direction": if score > Decimal::ZERO {
                    "favored_converges_up_buy_target"
                } else {
                    "underdog_converges_down_avoid_target"
                },
                "horizon_days": HORIZON_DAYS.to_string(),
            }),
        })
    }
}

/// Accept either "target_mid" (5-min DR) or "mid" (server candidates).
fn parse_mid(snapshot: &serde_json::Value) -> Option<Decimal> {
    for key in &["target_mid", "mid"] {
        if let Some(s) = snapshot[key].as_str() {
            return s.parse::<Decimal>().ok();
        }
    }
    None
}

fn neutral(metadata: serde_json::Value) -> Signal {
    Signal {
        processor_name: "theta_convergence",
        score: Decimal::ZERO,
        confidence: Decimal::ZERO,
        edge: Some(Decimal::ZERO),
        metadata,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(mid: &str, days: Option<&str>) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        m.insert("target_mid".into(), json!(mid));
        if let Some(d) = days {
            m.insert("days_to_resolution".into(), json!(d));
        }
        serde_json::Value::Object(m)
    }

    fn score(v: &Signal) -> Decimal {
        v.score
    }

    #[test]
    fn dormant_without_resolution_date() {
        let s = ThetaConvergenceProcessor
            .compute_signal(&snap("0.2", None), &json!({}))
            .unwrap();
        assert_eq!(score(&s), dec!(0));
        assert_eq!(s.confidence, dec!(0));
    }

    #[test]
    fn dormant_far_from_resolution() {
        // 40 days out (> 14 horizon) → neutral.
        let s = ThetaConvergenceProcessor
            .compute_signal(&snap("0.2", Some("40")), &json!({}))
            .unwrap();
        assert_eq!(score(&s), dec!(0));
    }

    #[test]
    fn underdog_near_resolution_scores_negative() {
        // mid 0.15, 2 days out: lean -0.35, urgency (14-2)/14 = 0.857, score = -0.35*0.857*0.5 ≈ -0.15.
        let s = ThetaConvergenceProcessor
            .compute_signal(&snap("0.15", Some("2")), &json!({}))
            .unwrap();
        assert!(
            score(&s) < dec!(0),
            "underdog should lean down (avoid), got {}",
            score(&s)
        );
        assert!(s.confidence > dec!(0) && s.confidence <= dec!(0.45));
    }

    #[test]
    fn favorite_near_resolution_scores_positive() {
        // mid 0.85, 2 days out: lean +0.35 → positive (buy the converging favorite).
        let s = ThetaConvergenceProcessor
            .compute_signal(&snap("0.85", Some("2")), &json!({}))
            .unwrap();
        assert!(score(&s) > dec!(0));
    }

    #[test]
    fn coin_flip_is_neutral() {
        // mid 0.51 (lean 0.01 < MIN_LEAN) → no direction.
        let s = ThetaConvergenceProcessor
            .compute_signal(&snap("0.51", Some("1")), &json!({}))
            .unwrap();
        assert_eq!(score(&s), dec!(0));
    }

    #[test]
    fn urgency_increases_as_resolution_nears() {
        // Same mid, closer resolution → larger |score| (stronger convergence pressure).
        let far = ThetaConvergenceProcessor
            .compute_signal(&snap("0.2", Some("10")), &json!({}))
            .unwrap();
        let near = ThetaConvergenceProcessor
            .compute_signal(&snap("0.2", Some("1")), &json!({}))
            .unwrap();
        assert!(score(&near).abs() > score(&far).abs());
    }
}
