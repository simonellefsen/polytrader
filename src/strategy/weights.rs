//! Per-processor weights for the FusionEngine — the substrate of the closed learning loop.
//!
//! Hermes measures each signal processor's behaviour (fire rate, edge quality) and writes a
//! `strategy_weights` journal event. The main app + server load the latest weights here and the
//! FusionEngine multiplies each processor's confidence-weight by its learned multiplier. This is
//! what lets Hermes' learning *change runtime behaviour* rather than just emit text.
//!
//! ## Safety / honesty
//! - Weights are clamped to [MIN_WEIGHT, MAX_WEIGHT]; a corrupt event can never zero out or
//!   explode a processor.
//! - Default (no event, or a processor missing from the event) is 1.0 — neutral, identical to the
//!   pre-learning behaviour. The system degrades safely to "no tuning".
//! - Paper-only; weights only affect simulated decision reports + candidate ranking.

use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use sqlx::PgPool;
use std::collections::BTreeMap;

/// Lower bound: a processor can be trimmed but never fully silenced (keeps it observable so Hermes
/// can later re-credit it if realized P&L shows it was right).
pub const MIN_WEIGHT: Decimal = dec!(0.25);
/// Upper bound: headroom for future realized-P&L-based boosting; the current heuristic never
/// targets above 1.0 (it can only trim, not amplify, until ground-truth outcomes exist).
pub const MAX_WEIGHT: Decimal = dec!(2.0);

/// Clamp a weight to the allowed band.
pub fn clamp_weight(w: Decimal) -> Decimal {
    w.max(MIN_WEIGHT).min(MAX_WEIGHT)
}

/// Load the latest Hermes-written processor weights from `journal.events`.
///
/// Returns an empty map if no event exists (FusionEngine then treats every processor as 1.0).
/// Robust: any parse/query failure degrades to empty (neutral), never aborts a decision cycle.
pub async fn load_processor_weights(pool: &PgPool) -> BTreeMap<String, Decimal> {
    let latest: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT payload FROM journal.events
         WHERE event_type = 'strategy_weights'
         ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    let mut out = BTreeMap::new();
    if let Some(payload) = latest {
        if let Some(obj) = payload.get("weights").and_then(|w| w.as_object()) {
            for (name, val) in obj {
                let w = val
                    .as_str()
                    .and_then(|s| s.parse::<Decimal>().ok())
                    .unwrap_or(dec!(1.0));
                out.insert(name.clone(), clamp_weight(w));
            }
        }
    }
    out
}
