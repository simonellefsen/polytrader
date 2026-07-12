//! Strategy module (Phase 3.2 start — smallest viable skeleton per approved transfer plan).
//!
//! Multi-signal fusion foundation: processors (trait) produce Decimal-based signals;
//! FusionEngine combines them with attribution for journal + Hermes closed-loop learning.
//!
//! **Wiki-first compliance**: All design/credits in wiki/strategies/multi-signal-fusion.md,
//! integrations/polymarket-apis-and-data-sources.md, decisions/2026-05-25-adopt-multi-signal-fusion-from-btc-bot.md,
//! concepts/hermes-self-improvement.md (new fusion section), docs/project-plan.md (3.1-3.5), log.md top entry (2026-05-25 fees impl prepend).
//! Explicit credits to Polymarket-BTC-15-Minute-Trading-Bot/core/strategy_brain/{fusion_engine/signal_fusion.py,
//! signal_processors/base_processor.py + siblings} + openclaw-ai-polymarket-trading-bot/src/engine/{features,predictor,llmScorer}.ts
//! + poly-maker/poly_data/* + Poly-Trader/polymarket_ai_*.py + agents/agents/polymarket/*.
//!
//! Net-of-fees + DecisionReport extension credits: wiki/strategies/fees-tax-latency-and-execution-tiers.md + goals-and-operational-cadence.md (efe1660).
//!
//! **Exact patterns followed (no deviations)**:
//! - rust_decimal::Decimal for *all* scores, edges, math (Cargo.toml + AGENTS.md: "never float for finance").
//! - anyhow::Result + tracing (info/warn) for errors (see src/ingester/mod.rs ingest_tick).
//! - sqlx/journal hooks commented for future attribution writes (see src/journal/{models.rs,writer.rs} Reflection + INSERT patterns; existing tables only, no migrations).
//! - Heavy risk comments on every trading-related item (AGENTS: "All trading-related code must be heavily commented with risk implications").
//! - Paper-only (no real paths, no SDK, no wallet). Preserves all verified (k8s-apply, hermes ts, subpath, probes, JSON, Phase 2 SSR, fmt/clippy).
//! - No new deps, no new DB tables, no behavior change to ingester/paper/ui/hermes.
//! - Smallest: 1-2 processor stubs + basic FusionEngine (no full impl, no tests here — expanded per follow-ups per anti-pattern #2 briefing). Net edge extension is the minimal addition for fees wiki requirement.
//!
//! **Risk note (repeated for audit)**: This is paper-only scaffolding. Real capital exposure requires explicit human gates (AGENTS safety #1-5). Fusion signals can correlate or overfit; attribution depends on complete journal (log warnings, never silent). All P&L/edges use Decimal exclusively.

// Allow dead_code for this minimal skeleton (unused until wired in follow-up increments per plan; full usage + tests in later phases to avoid anti-pattern #2).
// This is the smallest change delivering the artifact (trait + 2 processors + FusionEngine + attribution) while satisfying fmt/clippy -D warnings + AGENTS.
#![allow(dead_code)]

pub mod arbitrage;
pub mod external;
pub mod negrisk;
pub mod overreaction;
pub mod theta;
pub mod weights;

pub use arbitrage::ArbitrageScanner;
pub use external::{
    fetch_newsdata_news, fetch_yahoo_context, news_fetch_in_cooldown, newsdata_query,
    slug_market_direction, NewsSentimentProcessor, YahooFinanceProcessor,
};
// OverreactionProcessor retired 2026-06-29 (no longer wired into the fusion engine); the impl remains
// in overreaction.rs for reference. Re-export removed so the unused processor doesn't warn.
pub use theta::ThetaConvergenceProcessor;
pub use weights::{clamp_weight, load_processor_weights};

use anyhow::Result;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde_json::json;
use std::collections::BTreeMap;
use tracing::warn;

/// Normalized signal from a processor (inspired by BTC bot base_processor.py Signal + confidence/edge).
/// All values Decimal; metadata for attribution (e.g. which sub-features fired).
#[derive(Debug, Clone)]
pub struct Signal {
    pub processor_name: &'static str,
    pub score: Decimal,        // e.g. -1.0..1.0 edge direction
    pub confidence: Decimal,   // 0.0..1.0
    pub edge: Option<Decimal>, // expected value or similar (Decimal)
    pub metadata: serde_json::Value,
}

/// Fee context for net-of-fees calculations in fusion — Polymarket's REAL taker model
/// (docs.polymarket.com/trading/fees): `fee = shares × rate × p × (1−p)`, per-market rate,
/// geopolitics fee-free. **Makers are never charged on Polymarket** (they earn 20–25% rebates
/// instead), and every simulated fill crosses the book (taker), so taker is the only side costed.
/// The tiered taker-rebate program is deliberately ignored (conservative: at paper volumes the
/// tier is Bronze and the rebate ~nil).
/// See wiki/strategies/fees-tax-latency-and-execution-tiers.md for why net (not gross) is mandatory.
#[derive(Debug, Clone, Default)]
pub struct FeeContext {
    /// Per-market Polymarket taker fee RATE (e.g. 0.04; 0 = fee-free geopolitics). Prefer the
    /// stored `market_data.markets.taker_fee_rate` (synced from Gamma feeSchedule); fall back to
    /// `crate::polymarket_taker_fee_rate(slug)` (category default).
    pub taker_fee_rate: Decimal,
    /// Trade price `p` for this side — the fee shape `p × (1−p)` peaks at 0.50, zero at extremes.
    pub price: Decimal,
    /// Estimated gas per order in USDC (orders are gasless via the relayer; kept as a small
    /// conservative buffer).
    pub est_gas_usdc: Decimal,
}

/// Decision Report (for 5-min deliberate tier + journal).
/// Exposes **net edge after fees** as the primary signal (per approved tiers + cadence wiki pages).
/// Attribution includes per-signal + fee_impact breakdown for Hermes (3.3).
/// Smallest: no chrono timestamp (avoids dep); generated by caller if needed.
/// RISK: Consumers *must* gate on net_edge (min 4-6% per goals wiki); gross edge leads to negative expectancy at $150 scale after fees.
#[derive(Debug, Clone)]
pub struct DecisionReport {
    pub fused_gross_edge: Decimal,
    pub net_edge_after_fees: Decimal,
    pub confidence: Decimal,
    /// Full attribution (per processor + fee_impact + metadata). Store as jsonb in journal.decision_context or metrics.
    pub attribution: serde_json::Value,
    // future: opportunities: Vec<...>, risk_checks: ...
}

/// Trait for signal processors (port of base_processor.py + siblings).
/// Implementations must be pure or read-only on snapshots; no side effects.
/// Risk: Processor bugs can inject bad signals into fusion -> bad paper decisions.
/// Mitigation: paper-only, journal every output, Hermes post-resolution attribution + anomaly detection.
pub trait SignalProcessor: Send + Sync {
    fn name(&self) -> &'static str;

    /// Compute signal from current market snapshot + context.
    /// In real use: fed by ingester events (orderbook jsonb + mids as Decimal).
    fn compute_signal(
        &self,
        _market_snapshot: &serde_json::Value,
        _ctx: &serde_json::Value,
    ) -> Result<Signal>;
}

/// Sum the `size` field across a jsonb orderbook level array (`[{"price","size"}, ...]`). Shared by
/// the orderbook-aware processors. Missing/unparseable → 0 (treated as no depth on that side).
fn level_depth(levels: Option<&Vec<serde_json::Value>>) -> Decimal {
    levels
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l["size"].as_str()?.parse::<Decimal>().ok())
                .fold(Decimal::ZERO, |acc, x| acc + x)
        })
        .unwrap_or(Decimal::ZERO)
}

/// (bid_depth, ask_depth) for the target token from the snapshot's `bids`/`asks` arrays.
fn snapshot_book_depths(snapshot: &serde_json::Value) -> (Decimal, Decimal) {
    (
        level_depth(snapshot["bids"].as_array()),
        level_depth(snapshot["asks"].as_array()),
    )
}

/// A neutral (non-firing) signal: score 0 + confidence 0 so it contributes zero weight to fusion
/// (doesn't dilute the weighted average) and reads as "not fired" in the signal scorecard.
fn neutral_signal(name: &'static str, metadata: serde_json::Value) -> Signal {
    Signal {
        processor_name: name,
        score: Decimal::ZERO,
        confidence: Decimal::ZERO,
        edge: Some(Decimal::ZERO),
        metadata,
    }
}

/// Orderbook imbalance / momentum. Reads the target token's resting depth (bids vs asks) and reads
/// net buying/selling pressure: a bid-heavy book = demand to buy the target token = upward price
/// pressure (score > 0, supports the long); ask-heavy = selling pressure (score < 0). Near-balanced
/// books are ignored (noise gate). Score is in the shared frame "edge for buying the target outcome".
///
/// Risk: momentum reverses at resolution and thin books are noisy; confidence is therefore capped low
/// (≤0.35) and scaled by total depth, so the net-edge gate + fractional Kelly still govern. Paper-only.
pub struct OrderbookMomentumProcessor;

impl SignalProcessor for OrderbookMomentumProcessor {
    fn name(&self) -> &'static str {
        "orderbook_momentum"
    }

    fn compute_signal(
        &self,
        snapshot: &serde_json::Value,
        _ctx: &serde_json::Value,
    ) -> Result<Signal> {
        let (bid_depth, ask_depth) = snapshot_book_depths(snapshot);
        let total = bid_depth + ask_depth;
        if total <= Decimal::ZERO {
            // Data-starved (no book in snapshot) — neutral, not an error. Distinct from the old stub.
            return Ok(neutral_signal(
                self.name(),
                json!({"reason": "no_book_data"}),
            ));
        }

        // Imbalance in [-1, 1]; > 0 = bid-heavy (upward pressure), < 0 = ask-heavy (downward).
        let imbalance = (bid_depth - ask_depth) / total;
        const NOISE_GATE: Decimal = dec!(0.20); // ignore near-balanced books (pure noise)
        if imbalance.abs() < NOISE_GATE {
            return Ok(neutral_signal(
                self.name(),
                json!({"reason": "balanced_book", "imbalance": imbalance.to_string()}),
            ));
        }

        // Map imbalance beyond the gate to a modest edge (max |score| ≈ 0.12).
        const MAX_SCORE: Decimal = dec!(0.12);
        let effective = (imbalance.abs() - NOISE_GATE) / (Decimal::ONE - NOISE_GATE); // 0..1
        let magnitude = effective * MAX_SCORE;
        let score = if imbalance.is_sign_positive() {
            magnitude
        } else {
            -magnitude
        };

        // Confidence: 0.15 base + up to 0.20 scaled by total depth (thin books → low confidence),
        // capped at 0.35 (advisory only). 5000 shares ≈ "deep enough" soft cap.
        let depth_conf = (total / dec!(5000)).min(Decimal::ONE) * dec!(0.20);
        let confidence = (dec!(0.15) + depth_conf).min(dec!(0.35));

        Ok(Signal {
            processor_name: self.name(),
            score,
            confidence,
            edge: Some(score * confidence),
            metadata: json!({
                "thesis": "orderbook_imbalance_momentum",
                "bid_depth": bid_depth.to_string(),
                "ask_depth": ask_depth.to_string(),
                "imbalance": imbalance.to_string(),
                "direction": if score.is_sign_positive() { "buy_pressure_supports_long" } else { "sell_pressure_opposes_long" },
            }),
        })
    }
}

/// Spike + divergence: fade a recent sharp price move ONLY when the orderbook now leans against it
/// (a reversal setup). A spike down in the target token (it got cheaper fast) with a now bid-heavy
/// book (buyers stepping in) → expect a bounce → score > 0 (buy the cheap target). A spike up with a
/// now ask-heavy book → expect a pullback → score < 0. A spike with the book confirming its own
/// direction is momentum, NOT divergence, and is suppressed (that's orderbook_momentum's job).
///
/// Distinct from overreaction_fade: that fires on absolute price *extremeness*; this fires on the
/// *dynamics* (a recent spike + opposing book), independent of how extreme the level is.
///
/// Risk: spikes can be driven by real catalysts (the reversal never comes); confidence is capped low
/// (≤0.40) and the net-edge gate + Kelly still govern. Paper-only.
pub struct SpikeDivergenceProcessor;

impl SignalProcessor for SpikeDivergenceProcessor {
    fn name(&self) -> &'static str {
        "spike_divergence"
    }

    fn compute_signal(
        &self,
        snapshot: &serde_json::Value,
        _ctx: &serde_json::Value,
    ) -> Result<Signal> {
        // Signed ~3h move of the target token (positive = rose, negative = fell). Supplied by the DR
        // generator; absent (e.g. the observational server path) ⇒ no signal.
        let move_signed = snapshot
            .get("recent_move_signed")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<Decimal>().ok())
            .unwrap_or(Decimal::ZERO);

        const SPIKE_THRESHOLD: Decimal = dec!(0.07); // same scale as the overreaction volatility guard
        if move_signed.abs() < SPIKE_THRESHOLD {
            return Ok(neutral_signal(
                self.name(),
                json!({"reason": "no_spike", "recent_move_signed": move_signed.to_string()}),
            ));
        }

        let (bid_depth, ask_depth) = snapshot_book_depths(snapshot);
        let total = bid_depth + ask_depth;
        if total <= Decimal::ZERO {
            return Ok(neutral_signal(
                self.name(),
                json!({"reason": "no_book_for_divergence", "recent_move_signed": move_signed.to_string()}),
            ));
        }
        let imbalance = (bid_depth - ask_depth) / total;

        // Divergence = book leans AGAINST the spike direction, with a minimum strength to confirm.
        const CONFIRM: Decimal = dec!(0.15);
        let spike_down_book_up = move_signed.is_sign_negative() && imbalance > CONFIRM;
        let spike_up_book_down = move_signed.is_sign_positive() && imbalance < -CONFIRM;
        if !spike_down_book_up && !spike_up_book_down {
            return Ok(neutral_signal(
                self.name(),
                json!({
                    "reason": "spike_without_divergence",
                    "recent_move_signed": move_signed.to_string(),
                    "imbalance": imbalance.to_string(),
                }),
            ));
        }

        // Score: fade the spike (reversal opposes the move). Magnitude scales with spike size beyond
        // the threshold (cap ≈ 0.12). Reversal of a DOWN spike supports the long (score > 0).
        const SPIKE_SPAN: Decimal = dec!(0.23); // 0.07..0.30 maps to 0..1
        let spike_mag = ((move_signed.abs() - SPIKE_THRESHOLD) / SPIKE_SPAN).min(Decimal::ONE);
        let magnitude = spike_mag * dec!(0.12);
        let score = if move_signed.is_sign_negative() {
            magnitude // price fell → expect bounce up → buy target
        } else {
            -magnitude // price rose → expect pullback → oppose long
        };

        // Confidence: 0.20 base + up to 0.15 from divergence strength, capped 0.40.
        let confidence =
            (dec!(0.20) + imbalance.abs().min(Decimal::ONE) * dec!(0.15)).min(dec!(0.40));

        Ok(Signal {
            processor_name: self.name(),
            score,
            confidence,
            edge: Some(score * confidence),
            metadata: json!({
                "thesis": "spike_with_orderbook_divergence_fade",
                "recent_move_signed": move_signed.to_string(),
                "imbalance": imbalance.to_string(),
                "direction": if score.is_sign_positive() { "fade_down_spike_buy_target" } else { "fade_up_spike_oppose_long" },
            }),
        })
    }
}

/// Basic FusionEngine (port of BTC signal_fusion.py).
/// Owns processors; fuse() returns aggregated + per-signal attribution for journal/Hermes.
/// Current impl: simple weighted average (extensible to consensus/divergence rules).
/// Risk: Poor weights or unhandled correlation amplifies drawdowns in paper (virtual). Always journal full attribution.
/// Mitigation: paper-only; Hermes 3.3 closed-loop will measure real P&L per signal and propose weight changes (gated).
pub struct FusionEngine {
    processors: Vec<Box<dyn SignalProcessor>>,
    /// Per-processor learned multipliers (name -> weight). Empty/missing entries default to 1.0.
    /// Populated by Hermes' closed-loop weight tuning via load_processor_weights(). Clamped on read.
    weights: BTreeMap<String, Decimal>,
}

impl FusionEngine {
    pub fn new() -> Self {
        Self::with_weights(BTreeMap::new())
    }

    /// Construct with Hermes-learned processor weights (from strategy::load_processor_weights).
    /// Unknown/missing processors fall back to weight 1.0 (neutral), so this is always safe.
    pub fn with_weights(weights: BTreeMap<String, Decimal>) -> Self {
        Self {
            processors: vec![
                Box::new(OrderbookMomentumProcessor),
                Box::new(SpikeDivergenceProcessor),
                // overreaction_fade RETIRED 2026-06-29: edge-validation found it drove the directional
                // losses (it faded the real June-2026 Iran ceasefire — 7 of 8 single-side losers were it
                // buying "No" on markets that resolved Yes). Unwired from the fusion engine; the impl
                // (overreaction.rs) is kept for reference but no longer trades.
                // Time-decay convergence tilt near resolution (dormant far out; low confidence).
                Box::new(ThetaConvergenceProcessor),
                // External advisory signals (low confidence; risk gate still governs; Hermes learns weights).
                Box::new(YahooFinanceProcessor),
                Box::new(NewsSentimentProcessor),
            ],
            weights,
        }
    }

    /// Effective learned multiplier for a processor (default 1.0, always clamped to the safe band).
    fn weight_for(&self, name: &str) -> Decimal {
        clamp_weight(self.weights.get(name).copied().unwrap_or(dec!(1.0)))
    }

    /// Fuse signals for a market. Returns fused score + attribution map (for journal.metrics jsonb).
    /// In production: called from decision path after ingester tick; write to journal (see writer.rs pattern).
    pub fn fuse(
        &self,
        snapshot: &serde_json::Value,
        ctx: &serde_json::Value,
    ) -> Result<(Decimal, serde_json::Value)> {
        let mut attribution = serde_json::Map::new();
        // (score, confidence, learned_weight) per successful processor — fed to the shared fusion core
        // so the live path and the offline `fuse_from_attribution` replay use identical math.
        let mut contributions: Vec<(Decimal, Decimal, Decimal)> = Vec::new();

        for p in &self.processors {
            match p.compute_signal(snapshot, ctx) {
                Ok(sig) => {
                    // Effective weight = confidence × Hermes-learned multiplier (closed loop).
                    // Default multiplier 1.0 reproduces the original confidence-only weighting.
                    let learned = self.weight_for(sig.processor_name);
                    let w = sig.confidence * learned;
                    contributions.push((sig.score, sig.confidence, learned));
                    attribution.insert(
                        sig.processor_name.to_string(),
                        json!({
                            "score": sig.score,
                            "confidence": sig.confidence,
                            "learned_weight": learned,
                            "effective_weight": w,
                            "edge": sig.edge,
                            "metadata": sig.metadata
                        }),
                    );
                }
                Err(e) => {
                    warn!(processor = %p.name(), error = %e, "processor failed (paper-only; degraded fusion)");
                    // No silent fallback (anti-pattern #5): attribution notes the failure.
                    attribution.insert(p.name().to_string(), json!({"error": e.to_string()}));
                }
            }
        }

        let fused = fuse_weighted(contributions.iter().copied());

        let attr_json = serde_json::Value::Object(attribution);
        Ok((fused, attr_json))
    }

    /// Extended path exposing **net edge after fees/gas** (the primary signal for the deliberate
    /// 5-min Decision Report tier, per fees-tax-latency-and-execution-tiers.md + goals-and-operational-cadence.md).
    ///
    /// Builds on fuse() + adds fee costing + enriched attribution (no silent; errors noted).
    /// One path updated for smallest viable (this + example placeholder).
    ///
    /// fee_ctx: from PaperTradingEngine (or defaults). notional: approx USD size for costing.
    /// Returns (gross_fused, net_after_fees, enriched_attribution_json_for_journal).
    ///
    /// RISK (AGENTS + wiki): Net (not gross) is non-negotiable for $150 capital. Fees routinely
    /// destroy small edges. This must become the gate before any paper submit. Always journal the
    /// full attr (including fee_impact) so Hermes can do fee-adjusted per-signal attribution.
    /// Conservative bias in defaults. Paper-only.
    ///
    /// Credits: net-of-fees requirement + tier model from the two 2026-05-25 strategy wiki pages;
    /// fusion patterns from BTC-bot signal_fusion.py (extended for costs).
    pub fn fuse_net(
        &self,
        snapshot: &serde_json::Value,
        ctx: &serde_json::Value,
        fee_ctx: Option<&FeeContext>,
        notional: Decimal,
    ) -> Result<(Decimal, Decimal, serde_json::Value)> {
        let (gross, mut attribution) = self.fuse(snapshot, ctx)?;

        let (net, fee_note) = if let Some(f) = fee_ctx {
            // Polymarket's real taker fee, expressed as a FRACTION of notional so units match the
            // fractional `gross` edge. (The old code subtracted raw USDC from the fraction — on a
            // $10 notional that over-penalized every DR ~10×, and its flat-bps model ignored that
            // geopolitics is FREE while crypto at low p costs up to rate×(1−p) ≈ 7% of notional.)
            //   shares   = notional / p
            //   fee_usdc = shares × rate × p × (1−p) = notional × rate × (1−p)
            //   cost     = (fee_usdc + gas) / notional  — the fraction subtracted from the edge.
            let (fee_usdc, cost_frac) = if f.price > Decimal::ZERO && notional > Decimal::ZERO {
                let shares = notional / f.price;
                let fee = crate::polymarket_fee(f.taker_fee_rate, f.price, shares);
                (fee, (fee + f.est_gas_usdc) / notional)
            } else {
                (Decimal::ZERO, Decimal::ZERO)
            };
            let n = gross - cost_frac;

            // Enrich attribution (jsonb ready for journal; explicit, no silent).
            // `fee_cost_frac` is the value actually subtracted (fraction-of-notional; the backtest
            // harness prefers it); `est_fees_and_gas` stays as the USDC amount for audit and as the
            // historical-report fallback key.
            if let Some(map) = attribution.as_object_mut() {
                map.insert(
                    "fee_impact".to_string(),
                    json!({
                        "taker_fee_rate_used": f.taker_fee_rate.to_string(),
                        "price": f.price.to_string(),
                        "fee_cost_frac": cost_frac.to_string(),
                        "est_fees_and_gas": (fee_usdc + f.est_gas_usdc).to_string(),
                        "notional_for_cost": notional.to_string(),
                        "gross_edge": gross.to_string(),
                        "net_edge_after_fees": n.to_string(),
                        "note": "PRIMARY signal for deliberate 5-min tier (see fees wiki + 4-6% min net in goals). Real Polymarket taker model: shares × rate × p × (1−p); makers pay nothing (we always cross = taker)."
                    }),
                );
            }
            (n, "fee_ctx provided; net computed")
        } else {
            if let Some(map) = attribution.as_object_mut() {
                map.insert(
                    "fee_impact".to_string(),
                    json!({
                        "note": "no fee_ctx (degraded path); net == gross. Always provide real FeeContext from paper engine for production use.",
                        "gross_edge": gross.to_string()
                    }),
                );
            }
            (gross, "no fee_ctx; net=gross (degraded)")
        };

        // Also surface a DecisionReport-shaped view in attribution for downstream 5-min generators
        if let Some(map) = attribution.as_object_mut() {
            map.insert(
                "decision_report_summary".to_string(),
                json!({
                    "fused_gross_edge": gross.to_string(),
                    "net_edge_after_fees": net.to_string(),
                    "fee_note": fee_note,
                    "primary_for_deliberate_tier": true
                }),
            );
        }

        let attr_json = attribution; // already a Value (from inner fuse); mutated in place via as_object_mut when Object
        Ok((gross, net, attr_json))
    }
}

/// Pure weighted-average fusion — the single source of truth for how per-signal scores combine.
/// `fused = Σ(scoreᵢ · confidenceᵢ · learned_weightᵢ) / Σ(confidenceᵢ · learned_weightᵢ)`, and 0 when
/// the total effective weight is ≤ 0. Shared by the live [`FusionEngine::fuse`] (fresh signals) and
/// the offline backtest harness via [`fuse_from_attribution`] (stored decision-report scores replayed
/// under a candidate weight vector — no processors re-run).
pub fn fuse_weighted<I>(signals: I) -> Decimal
where
    I: IntoIterator<Item = (Decimal, Decimal, Decimal)>,
{
    let mut total_weighted = Decimal::ZERO;
    let mut total_weight = Decimal::ZERO;
    for (score, confidence, learned) in signals {
        let w = confidence * learned;
        total_weighted += score * w;
        total_weight += w;
    }
    if total_weight > Decimal::ZERO {
        total_weighted / total_weight
    } else {
        Decimal::ZERO
    }
}

/// Replay fusion from a stored `report.attribution` object under a candidate weight vector — the
/// backtest-harness primitive. For each per-signal entry that carries both `score` and `confidence`
/// (this skips the `fee_impact`, `decision_report_summary`, and `{"error": …}` entries that also live
/// in the attribution map), applies `clamp_weight(weights[name])` as the learned multiplier (default
/// 1.0 when the signal is absent from the vector), then [`fuse_weighted`]. The `clamp_weight` mirrors
/// the live read path ([`FusionEngine::weight_for`]) so a candidate weight outside [0.25, 2.0] is
/// bounded exactly as production would bound it. By construction this reproduces
/// [`FusionEngine::fuse`]'s gross edge without re-running any processor.
pub fn fuse_from_attribution(
    attribution: &serde_json::Value,
    weights: &BTreeMap<String, Decimal>,
) -> Decimal {
    // Attribution numbers may be stored as JSON strings (rust_decimal default) or numbers — accept both.
    fn parse_dec(v: &serde_json::Value) -> Option<Decimal> {
        match v {
            serde_json::Value::String(s) => s.parse::<Decimal>().ok(),
            serde_json::Value::Number(n) => n.to_string().parse::<Decimal>().ok(),
            _ => None,
        }
    }
    let mut contributions: Vec<(Decimal, Decimal, Decimal)> = Vec::new();
    if let Some(map) = attribution.as_object() {
        for (name, entry) in map {
            let (Some(score), Some(confidence)) = (
                entry.get("score").and_then(parse_dec),
                entry.get("confidence").and_then(parse_dec),
            ) else {
                continue; // fee_impact / decision_report_summary / error entry — not a signal
            };
            let learned = clamp_weight(weights.get(name).copied().unwrap_or(dec!(1.0)));
            contributions.push((score, confidence, learned));
        }
    }
    fuse_weighted(contributions)
}

impl Default for FusionEngine {
    fn default() -> Self {
        Self::new()
    }
}

// Allow dead code in skeleton (will be wired/used in follow-up increments per plan; module-level #![allow] at top is sufficient and documented).
pub fn example_fuse_usage_placeholder() {
    // Example (not called yet; demonstrates journal hook using existing writer.rs patterns + new net edge path).
    // Proper handling (never unwrap; see fuse_net explicit fee_impact + error attribution; anti-pattern #5).
    // let engine = FusionEngine::new();
    // let fee = Some(FeeContext { taker_bps: dec!(100), ..Default::default() });
    // match engine.fuse_net(&json!({}), &json!({}), fee.as_ref(), dec!(5.0)) {
    //     Ok((gross, net, attr)) => {
    //         // DecisionReport-shaped (ready for 5-min generator + jsonb journal)
    //         let report = DecisionReport {
    //             fused_gross_edge: gross,
    //             net_edge_after_fees: net,
    //             confidence: dec!(0.6),
    //             attribution: attr,
    //         };
    //         // Later: journal ... (metrics or decision_context = serde_json::to_value(&report) )
    //         tracing::debug!(gross=%report.fused_gross_edge, net=%report.net_edge_after_fees, "net edge (primary deliberate signal) ready for journal");
    //     }
    //     Err(e) => { /* log and degrade */ }
    // }
    tracing::debug!("FusionEngine skeleton ready (paper-only, Decimal, net edge after fees + DecisionReport for deliberate tier + Hermes)");
}

#[cfg(test)]
mod processor_tests {
    use super::*;
    use serde_json::json;

    fn book(bid_size: &str, ask_size: &str) -> serde_json::Value {
        json!({
            "bids": [{"price": "0.40", "size": bid_size}],
            "asks": [{"price": "0.41", "size": ask_size}],
        })
    }

    #[test]
    fn momentum_bid_heavy_book_supports_long() {
        let sig = OrderbookMomentumProcessor
            .compute_signal(&book("9000", "1000"), &json!({}))
            .unwrap();
        // Bid-heavy = upward pressure = positive score (supports buying the target).
        assert!(sig.score > Decimal::ZERO, "score={}", sig.score);
        assert!(sig.confidence > Decimal::ZERO);
    }

    #[test]
    fn momentum_ask_heavy_book_opposes_long() {
        let sig = OrderbookMomentumProcessor
            .compute_signal(&book("1000", "9000"), &json!({}))
            .unwrap();
        assert!(sig.score < Decimal::ZERO, "score={}", sig.score);
    }

    #[test]
    fn momentum_balanced_book_is_neutral() {
        let sig = OrderbookMomentumProcessor
            .compute_signal(&book("5000", "5000"), &json!({}))
            .unwrap();
        assert_eq!(sig.score, Decimal::ZERO);
        assert_eq!(sig.confidence, Decimal::ZERO);
    }

    #[test]
    fn momentum_no_book_is_neutral_not_error() {
        let sig = OrderbookMomentumProcessor
            .compute_signal(&json!({}), &json!({}))
            .unwrap();
        assert_eq!(sig.score, Decimal::ZERO);
    }

    #[test]
    fn divergence_down_spike_with_buyers_supports_long() {
        // Target fell sharply (move -0.10) but the book is now bid-heavy → expect a bounce → score > 0.
        let mut snap = book("9000", "1000");
        snap["recent_move_signed"] = json!("-0.10");
        let sig = SpikeDivergenceProcessor
            .compute_signal(&snap, &json!({}))
            .unwrap();
        assert!(sig.score > Decimal::ZERO, "score={}", sig.score);
    }

    #[test]
    fn divergence_up_spike_with_sellers_opposes_long() {
        let mut snap = book("1000", "9000");
        snap["recent_move_signed"] = json!("0.10");
        let sig = SpikeDivergenceProcessor
            .compute_signal(&snap, &json!({}))
            .unwrap();
        assert!(sig.score < Decimal::ZERO, "score={}", sig.score);
    }

    #[test]
    fn divergence_spike_without_opposing_book_is_neutral() {
        // Spike down but book ALSO sell-heavy (confirms the move, not a reversal) → no signal.
        let mut snap = book("1000", "9000");
        snap["recent_move_signed"] = json!("-0.10");
        let sig = SpikeDivergenceProcessor
            .compute_signal(&snap, &json!({}))
            .unwrap();
        assert_eq!(sig.score, Decimal::ZERO);
    }

    #[test]
    fn divergence_small_move_is_neutral() {
        let mut snap = book("9000", "1000");
        snap["recent_move_signed"] = json!("-0.02"); // below the spike threshold
        let sig = SpikeDivergenceProcessor
            .compute_signal(&snap, &json!({}))
            .unwrap();
        assert_eq!(sig.score, Decimal::ZERO);
    }

    // --- Pure fusion cores (Phase 0: shared by live fuse + the backtest harness) ---

    #[test]
    fn fuse_weighted_matches_manual_weighted_average() {
        // (score, confidence, learned): weighted avg = Σ(s·c·w) / Σ(c·w).
        let signals = vec![
            (dec!(0.10), dec!(0.5), dec!(1.0)),  // s·c·w = 0.05, w = 0.5
            (dec!(-0.20), dec!(0.4), dec!(2.0)), // s·c·w = -0.16, w = 0.8
        ];
        // numerator = 0.05 - 0.16 = -0.11 ; denominator = 0.5 + 0.8 = 1.3
        let expected = dec!(-0.11) / dec!(1.3);
        assert_eq!(fuse_weighted(signals), expected);
    }

    #[test]
    fn fuse_weighted_zero_total_weight_is_zero() {
        assert_eq!(fuse_weighted(std::iter::empty()), Decimal::ZERO);
        // All-zero confidence ⇒ zero total weight ⇒ 0 (no divide-by-zero).
        assert_eq!(
            fuse_weighted(vec![(dec!(0.9), dec!(0), dec!(1.5))]),
            Decimal::ZERO
        );
    }

    #[test]
    fn fuse_from_attribution_skips_nonsignals_and_applies_candidate_weights() {
        // A realistic attribution map: two signals plus the non-signal book-keeping entries that
        // fuse_net adds. Only the two signals should drive the result.
        let attr = json!({
            "orderbook_momentum": {"score": "0.10", "confidence": "0.5"},
            "news_sentiment": {"score": "-0.20", "confidence": "0.4"},
            "spike_divergence": {"error": "boom"},          // skipped (no score/confidence)
            "fee_impact": {"gross_edge": "0.01"},            // skipped
            "decision_report_summary": {"net_edge_after_fees": "0.0"}, // skipped
        });
        let mut weights = BTreeMap::new();
        weights.insert("orderbook_momentum".to_string(), dec!(1.0));
        weights.insert("news_sentiment".to_string(), dec!(2.0));
        // momentum: w=0.5 contrib 0.05 ; news: w=0.4·2=0.8 contrib -0.16 ; same as the manual case.
        let expected = dec!(-0.11) / dec!(1.3);
        assert_eq!(fuse_from_attribution(&attr, &weights), expected);

        // Changing a candidate weight changes the fused edge (the harness's whole point).
        weights.insert("news_sentiment".to_string(), dec!(0.5));
        assert_ne!(fuse_from_attribution(&attr, &weights), expected);
    }

    #[test]
    fn fuse_from_attribution_reproduces_live_fuse() {
        // The Phase-0 guarantee: replaying a stored attribution under the SAME weights the engine used
        // reproduces fuse()'s gross edge exactly — so the harness tests weights offline == live fusion.
        let mut weights = BTreeMap::new();
        weights.insert("orderbook_momentum".to_string(), dec!(1.3));
        weights.insert("news_sentiment".to_string(), dec!(0.7));
        let engine = FusionEngine::with_weights(weights.clone());
        // A bid-heavy book fires orderbook_momentum; the other processors return neutral (still present
        // in attribution), so this exercises a mixed fired/neutral fusion.
        let snapshot = json!({
            "bids": [{"price": "0.40", "size": "9000"}],
            "asks": [{"price": "0.41", "size": "1000"}],
        });
        let (gross, attr) = engine.fuse(&snapshot, &json!({})).unwrap();
        let replayed = fuse_from_attribution(&attr, &weights);
        assert_eq!(replayed, gross, "harness replay must equal live fuse");
    }
}
