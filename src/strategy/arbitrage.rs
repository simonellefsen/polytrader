//! Arbitrage (missing-probability) scanner for binary prediction markets.
//!
//! ## Why arbitrage exists in prediction markets
//! In a binary market one of YES or NO must resolve to $1.00. Therefore the
//! fair combined price is exactly $1.00. Occasionally the sum of the best ask
//! on YES and the best ask on NO falls below $1.00: buying both legs locks in a
//! risk-free profit no matter how the market resolves.
//!
//! Example: YES asks 0.35, NO asks 0.63 → total cost 0.98 → gross profit $0.02 per
//! share pair. After two taker fills at 0.5 % each (≈ $0.0098 in fees) → net ≈ $0.01.
//! Small, but *truly* risk-free on the price dimension.
//!
//! ## Execution risk (not priced by this scanner)
//! - **Leg timing**: snapshots are from the last ingester cycle (≤5 min stale).
//!   Real arb requires simultaneous fills via WebSocket. Snapshot-based arb is for
//!   *identification and analysis* only in this phase.
//! - **Liquidity**: available depth at the best ask may be smaller than desired size.
//!   `max_size_usdc` is constrained to the minimum depth across both legs.
//! - **Resolution ambiguity**: some markets have a third outcome or can void.
//!   Only clearly binary markets (two outcomes, standard resolution) should be targeted.
//!
//! Paper-only. All opportunities are returned for journaling; none are auto-executed.

use anyhow::Result;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tracing::warn;

/// Minimum net profit per unit to include in results.
const MIN_NET_PROFIT: Decimal = dec!(0.005); // 0.5 % after fees

/// A risk-free arbitrage opportunity detected in the orderbook snapshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArbitrageOpportunity {
    pub market_id: String,
    pub question: String,
    /// Lowest ask price for the YES token (taker buy price).
    pub ask_yes: Decimal,
    /// Lowest ask price for the NO token.
    pub ask_no: Decimal,
    /// ask_yes + ask_no. Values below 1.00 mean gross arb exists.
    pub total_cost: Decimal,
    /// 1.00 − total_cost (gross profit per share-pair before fees).
    pub gross_profit_per_unit: Decimal,
    /// Gross profit minus combined taker fees for both legs.
    pub net_profit_per_unit: Decimal,
    /// Combined taker fee estimate per share-pair (both legs).
    pub combined_fee_per_unit: Decimal,
    /// Max USDC deployable, limited by the thinner side's best-ask depth.
    pub max_size_usdc: Decimal,
    /// Net profit if max_size_usdc is fully deployed.
    pub estimated_max_profit_usdc: Decimal,
}

pub struct ArbitrageScanner {
    taker_bps: Decimal,
}

impl ArbitrageScanner {
    pub fn new(taker_bps: Decimal) -> Self {
        Self { taker_bps }
    }

    /// Default: 0.5 % taker (typical Polymarket low-volume rate; conservative for net).
    pub fn with_default_fees() -> Self {
        Self::new(dec!(50))
    }

    /// Scan all active markets for YES + NO best-ask arb.
    ///
    /// Uses the latest stored orderbook snapshot for each outcome. Returns only
    /// opportunities where net profit per unit ≥ MIN_NET_PROFIT (0.5 %).
    /// Sorted best-first by net_profit_per_unit.
    ///
    /// RISK (paper-only): Snapshot delay means prices shown may no longer be
    /// achievable. Do not treat this as executable signal without live feeds.
    pub async fn scan(&self, pool: &PgPool) -> Result<Vec<ArbitrageOpportunity>> {
        // Fetch the latest YES and NO snapshots for every active market in one query.
        // LATERAL ensures we get exactly the newest row per (market, outcome) pair.
        let rows: Vec<(String, String, serde_json::Value, serde_json::Value)> = sqlx::query_as(
            r#"
                SELECT
                    m.gamma_id,
                    m.question,
                    yes_snap.asks  AS yes_asks,
                    no_snap.asks   AS no_asks
                FROM market_data.markets m
                JOIN LATERAL (
                    SELECT asks
                    FROM market_data.orderbook_snapshots
                    WHERE market_id = m.gamma_id AND outcome = 'Yes'
                    ORDER BY fetched_at DESC
                    LIMIT 1
                ) yes_snap ON true
                JOIN LATERAL (
                    SELECT asks
                    FROM market_data.orderbook_snapshots
                    WHERE market_id = m.gamma_id AND outcome = 'No'
                    ORDER BY fetched_at DESC
                    LIMIT 1
                ) no_snap ON true
                WHERE m.active = true AND NOT m.closed
                "#,
        )
        .fetch_all(pool)
        .await?;

        let fee_rate = self.taker_bps / dec!(10000);
        let mut opportunities = Vec::new();

        for (market_id, question, yes_asks, no_asks) in rows {
            let (ask_yes, depth_yes) = best_ask(&yes_asks);
            let (ask_no, depth_no) = best_ask(&no_asks);

            if ask_yes <= Decimal::ZERO || ask_no <= Decimal::ZERO {
                warn!(market = %market_id, "arb scan: zero ask on one leg; skipping");
                continue;
            }
            if ask_yes >= Decimal::ONE || ask_no >= Decimal::ONE {
                continue; // malformed
            }

            let total_cost = ask_yes + ask_no;
            let gross_profit = Decimal::ONE - total_cost;
            if gross_profit <= Decimal::ZERO {
                continue; // no raw arb (the normal, efficient-market case)
            }

            // Fee: taker fill on each leg; fee base is the price paid per leg.
            let combined_fee = (ask_yes + ask_no) * fee_rate * dec!(2);
            let net_profit = gross_profit - combined_fee;

            if net_profit < MIN_NET_PROFIT {
                continue;
            }

            // Max shares we can arb = depth at best ask on the thinner side.
            // Total USDC cost for that many pairs = max_shares × total_cost.
            let max_shares = depth_yes.min(depth_no);
            let max_size_usdc = max_shares * total_cost;
            let estimated_max_profit = max_shares * net_profit;

            opportunities.push(ArbitrageOpportunity {
                market_id,
                question,
                ask_yes,
                ask_no,
                total_cost,
                gross_profit_per_unit: gross_profit,
                net_profit_per_unit: net_profit,
                combined_fee_per_unit: combined_fee,
                max_size_usdc,
                estimated_max_profit_usdc: estimated_max_profit,
            });
        }

        // Best opportunities first
        opportunities.sort_by_key(|o| std::cmp::Reverse(o.net_profit_per_unit));

        Ok(opportunities)
    }
}

/// Extract the lowest (best) ask price and its available depth from a JSONB asks array.
/// Returns (price, depth). Both are zero if the book is empty or unparseable.
fn best_ask(asks: &serde_json::Value) -> (Decimal, Decimal) {
    let arr = match asks.as_array() {
        Some(a) if !a.is_empty() => a,
        _ => return (Decimal::ZERO, Decimal::ZERO),
    };

    let mut best_price: Option<Decimal> = None;
    let mut depth_at_best = Decimal::ZERO;

    for level in arr {
        let Some(price) = level["price"]
            .as_str()
            .and_then(|s| s.parse::<Decimal>().ok())
        else {
            continue;
        };
        let size = level["size"]
            .as_str()
            .and_then(|s| s.parse::<Decimal>().ok())
            .unwrap_or(Decimal::ZERO);

        match best_price {
            None => {
                best_price = Some(price);
                depth_at_best = size;
            }
            Some(bp) if price < bp => {
                best_price = Some(price);
                depth_at_best = size;
            }
            Some(bp) if price == bp => {
                depth_at_best += size;
            }
            _ => {}
        }
    }

    (best_price.unwrap_or(Decimal::ZERO), depth_at_best)
}
