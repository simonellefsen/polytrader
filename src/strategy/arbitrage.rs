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

/// Minimum net profit per share-pair to include in results. Lowered 2026-06-29 from 0.5% to 0.2%:
/// with the corrected per-category fee model (geopolitics is fee-free; see `polymarket_taker_fee`),
/// thin sub-$1 arbs are genuinely risk-free profit and worth capturing.
const MIN_NET_PROFIT: Decimal = dec!(0.002);

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

/// How close a market's YES+NO sum can sit above $1.00 and still count as a "near miss" — a market
/// the scanner correctly evaluated that was almost arbitrageable. Distinguishes "efficient market, no
/// arb" from "scanner starved/broken" when the opportunity count is zero.
const NEAR_MISS_BAND: Decimal = dec!(0.02); // total_cost in (1.00, 1.02]

/// Observability for a scan pass. A zero opportunity count is ambiguous on its own — these counters
/// disambiguate: `markets_scanned`/`usable_books` near zero means the scanner is starved (missing or
/// stale orderbook snapshots), whereas a healthy `usable_books` with `best_total_cost` comfortably
/// above $1.00 means the market is simply efficient (no arb to find). Journaled each pass.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArbDiagnostics {
    /// Markets that had a snapshot for BOTH legs (rows returned by the query).
    pub markets_scanned: usize,
    /// Of those, how many had a valid best ask in (0,1) on both legs.
    pub usable_books: usize,
    /// Skipped: a zero/empty ask on at least one leg (snapshot gap).
    pub skipped_zero_ask: usize,
    /// Skipped: an ask >= $1.00 on a leg (malformed/degenerate book).
    pub skipped_malformed: usize,
    /// Books where YES+NO < $1.00 (gross arb exists, before the fee/min-profit filter).
    pub sub_dollar_books: usize,
    /// Near misses: YES+NO in (1.00, 1.00 + NEAR_MISS_BAND] — almost arbitrageable.
    pub near_miss_books: usize,
    /// Opportunities that cleared the net-profit threshold (== returned Vec length).
    pub net_arb_books: usize,
    /// Lowest YES+NO sum seen across all usable books (closest approach to/under $1.00).
    pub best_total_cost: Option<String>,
    /// Market id holding `best_total_cost` (for a quick spot-check).
    pub best_total_cost_market: Option<String>,
}

/// Stateless: fees are looked up PER MARKET (Polymarket's per-category taker fee via
/// `crate::polymarket_taker_fee`), not a single rate, so the scanner carries no fee field.
#[derive(Default)]
pub struct ArbitrageScanner;

impl ArbitrageScanner {
    pub fn with_default_fees() -> Self {
        Self
    }

    /// Scan all active markets for YES + NO best-ask arb.
    ///
    /// Uses the latest stored orderbook snapshot for each outcome. Returns only opportunities where
    /// net profit per share-pair ≥ MIN_NET_PROFIT (0.2%), net of Polymarket's real per-category taker
    /// fee (geopolitics is free). Sorted best-first by net_profit_per_unit.
    ///
    /// RISK (paper-only): Snapshot delay means prices shown may no longer be
    /// achievable. Do not treat this as executable signal without live feeds.
    pub async fn scan(&self, pool: &PgPool) -> Result<Vec<ArbitrageOpportunity>> {
        Ok(self.scan_with_diagnostics(pool).await?.0)
    }

    /// Same scan, but also returns [`ArbDiagnostics`] so callers can tell an efficient market apart
    /// from a starved/broken scanner when no opportunities are found.
    pub async fn scan_with_diagnostics(
        &self,
        pool: &PgPool,
    ) -> Result<(Vec<ArbitrageOpportunity>, ArbDiagnostics)> {
        // Fetch the latest YES and NO snapshots for every active market in one query.
        // LATERAL ensures we get exactly the newest row per (market, outcome) pair.
        let rows: Vec<(
            String,
            String,
            String,
            Option<Decimal>,
            serde_json::Value,
            serde_json::Value,
        )> = sqlx::query_as(
            r#"
                SELECT
                    m.gamma_id,
                    COALESCE(m.slug, '') AS slug,
                    m.question,
                    m.taker_fee_rate,
                    yes_snap.asks  AS yes_asks,
                    no_snap.asks   AS no_asks
                FROM market_data.markets m
                JOIN LATERAL (
                    SELECT asks
                    FROM market_data.orderbook_snapshots
                    WHERE market_id = m.gamma_id AND outcome = 'Yes'
                      AND fetched_at > now() - interval '30 minutes'
                    ORDER BY fetched_at DESC
                    LIMIT 1
                ) yes_snap ON true
                JOIN LATERAL (
                    SELECT asks
                    FROM market_data.orderbook_snapshots
                    WHERE market_id = m.gamma_id AND outcome = 'No'
                      AND fetched_at > now() - interval '30 minutes'
                    ORDER BY fetched_at DESC
                    LIMIT 1
                ) no_snap ON true
                WHERE m.active = true AND NOT m.closed
                "#,
        )
        .fetch_all(pool)
        .await?;

        let mut opportunities = Vec::new();
        let mut diag = ArbDiagnostics {
            markets_scanned: rows.len(),
            ..Default::default()
        };
        let mut best_total_cost: Option<(Decimal, String)> = None;

        for (market_id, slug, question, stored_fee_rate, yes_asks, no_asks) in rows {
            let (ask_yes, depth_yes) = best_ask(&yes_asks);
            let (ask_no, depth_no) = best_ask(&no_asks);

            if ask_yes <= Decimal::ZERO || ask_no <= Decimal::ZERO {
                warn!(market = %market_id, "arb scan: zero ask on one leg; skipping");
                diag.skipped_zero_ask += 1;
                continue;
            }
            if ask_yes >= Decimal::ONE || ask_no >= Decimal::ONE {
                diag.skipped_malformed += 1;
                continue; // malformed
            }
            diag.usable_books += 1;

            let total_cost = ask_yes + ask_no;
            // Track the closest approach to $1.00 across all usable books.
            if best_total_cost
                .as_ref()
                .is_none_or(|(c, _)| total_cost < *c)
            {
                best_total_cost = Some((total_cost, market_id.clone()));
            }
            let gross_profit = Decimal::ONE - total_cost;
            if gross_profit <= Decimal::ZERO {
                // No raw arb (the normal, efficient-market case). Tally near misses so a zero
                // opportunity count is explainable rather than mysterious.
                if total_cost <= Decimal::ONE + NEAR_MISS_BAND {
                    diag.near_miss_books += 1;
                }
                continue;
            }
            diag.sub_dollar_books += 1;

            // Per-leg taker fee via Polymarket's real model (shares × rate × p × (1−p); geopolitics is
            // free). Prefer the per-market rate synced from Gamma; fall back to the category default.
            let rate = stored_fee_rate.unwrap_or_else(|| crate::polymarket_taker_fee_rate(&slug));
            let combined_fee = crate::polymarket_fee(rate, ask_yes, Decimal::ONE)
                + crate::polymarket_fee(rate, ask_no, Decimal::ONE);
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

        diag.net_arb_books = opportunities.len();
        if let Some((cost, market)) = best_total_cost {
            diag.best_total_cost = Some(cost.round_dp(4).to_string());
            diag.best_total_cost_market = Some(market);
        }

        Ok((opportunities, diag))
    }
}

/// Extract the lowest (best) ask price and its available depth from a JSONB asks array.
/// Returns (price, depth). Both are zero if the book is empty or unparseable.
/// (Shared with the NegRisk event-level scanner.)
pub(crate) fn best_ask(asks: &serde_json::Value) -> (Decimal, Decimal) {
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
