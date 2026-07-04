//! Event-level (NegRisk) arbitrage scanner.
//!
//! ## The invariant
//! Polymarket groups mutually-exclusive outcomes into a **negRisk event** of N binary member
//! markets, of which AT MOST ONE resolves Yes. Buying 1 share of **No in each of k members**
//! therefore pays out at least $(k−1): an arb whenever
//!
//! ```text
//! Σ best_no_ask  <  k − 1        ⇔        Σ (1 − best_no_ask)  >  1
//! ```
//!
//! i.e. whenever the implied Yes probabilities sum above 100% (the classic overround). Crucially
//! this holds for ANY SUBSET of the event's members — partial book coverage still yields a
//! (smaller) risk-free profit — so the scanner works over whatever books the ingest universe
//! already has, no event-wide ingestion required. Members priced at (1−ask_no) ≤ 0 can simply be
//! left out of the basket (they only dilute).
//!
//! ## Why this scanner exists
//! Single-market Yes+No arb was measured structurally dead (430 scans 2026-07-03/04, best combined
//! cost pinned at $1.000–1.001, zero sub-dollar books): one binary book is trivially kept efficient
//! by market makers. Keeping N books of one event mutually consistent is much harder, which is why
//! real Polymarket dislocations concentrate at the event level.
//!
//! Same execution-risk caveats as the single-market scanner (snapshot staleness, per-level depth);
//! paper-only, journaled for Hermes.

use anyhow::Result;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use super::arbitrage::best_ask;

/// Minimum net profit per basket-unit (one No share in every chosen member) to report.
/// Matches the single-market scanner's MIN_NET_PROFIT.
const MIN_NET_PROFIT: Decimal = dec!(0.002);
/// Ignore events where we can see fewer than this many live member books: 2-member events are
/// equivalent to one binary market (covered by the single-market scanner).
const MIN_MEMBERS: usize = 3;

/// One member leg of a NegRisk basket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NegRiskLeg {
    pub market_id: String,
    pub question: String,
    /// Best (lowest) ask on the No token — the taker buy price for this leg.
    pub ask_no: Decimal,
    /// Depth at that best ask (shares).
    pub depth: Decimal,
    /// Estimated taker fee per share for this leg.
    pub fee_per_share: Decimal,
}

/// A buy-all-No event arbitrage: buy 1 No share in each leg; at most one member resolves Yes, so
/// the basket pays at least $(legs−1) per unit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NegRiskOpportunity {
    pub event_id: String,
    pub legs: Vec<NegRiskLeg>,
    /// Σ ask_no across the chosen legs — cost of one basket unit.
    pub total_cost: Decimal,
    /// Guaranteed minimum payout per unit: legs − 1.
    pub min_payout: Decimal,
    /// min_payout − total_cost (before fees).
    pub gross_profit_per_unit: Decimal,
    /// Gross minus the summed per-leg taker fees.
    pub net_profit_per_unit: Decimal,
    /// Max basket units executable = min depth across legs.
    pub max_units: Decimal,
    pub estimated_max_profit_usdc: Decimal,
}

/// Scan diagnostics (journaled every pass so a zero count is explainable, mirroring ArbDiagnostics).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NegRiskDiagnostics {
    /// Distinct negRisk events with >= MIN_MEMBERS fresh member books.
    pub events_scanned: usize,
    /// Member books inspected in total.
    pub member_books: usize,
    /// Best (highest) implied-Yes sum seen across events — how close the closest event came to
    /// the >1.00 arb line. The event-level analogue of best_total_cost.
    pub best_implied_yes_sum: Option<String>,
    pub best_event_id: Option<String>,
    /// Opportunities clearing the net-profit threshold.
    pub net_arb_events: usize,
}

/// Scan all active negRisk events over the books already ingested. Returns opportunities sorted
/// best-first plus diagnostics.
pub async fn scan_negrisk(pool: &PgPool) -> Result<(Vec<NegRiskOpportunity>, NegRiskDiagnostics)> {
    // Latest fresh No-book per active member of every negRisk event with enough visible members.
    let rows: Vec<(
        String,
        String,
        String,
        String,
        Option<Decimal>,
        serde_json::Value,
    )> = sqlx::query_as(
        r#"
        SELECT m.event_id, m.gamma_id, COALESCE(m.slug, ''), m.question, m.taker_fee_rate,
               no_snap.asks
        FROM market_data.markets m
        JOIN LATERAL (
            SELECT asks FROM market_data.orderbook_snapshots
            WHERE market_id = m.gamma_id AND outcome = 'No'
              AND fetched_at > now() - interval '30 minutes'
            ORDER BY fetched_at DESC LIMIT 1
        ) no_snap ON true
        WHERE m.active AND NOT m.closed AND m.neg_risk AND m.event_id IS NOT NULL
        ORDER BY m.event_id
        "#,
    )
    .fetch_all(pool)
    .await?;

    let mut diag = NegRiskDiagnostics::default();
    let mut opportunities = Vec::new();
    let mut best_sum: Option<(Decimal, String)> = None;

    // Group rows by event_id (rows arrive sorted).
    let mut by_event: std::collections::BTreeMap<String, Vec<NegRiskLeg>> =
        std::collections::BTreeMap::new();
    for (event_id, market_id, slug, question, fee_rate, no_asks) in rows {
        let (ask_no, depth) = best_ask(&no_asks);
        if ask_no <= Decimal::ZERO || ask_no >= Decimal::ONE || depth <= Decimal::ZERO {
            continue;
        }
        diag.member_books += 1;
        let rate = fee_rate.unwrap_or_else(|| crate::polymarket_taker_fee_rate(&slug));
        let fee_per_share = crate::polymarket_fee(rate, ask_no, Decimal::ONE);
        by_event.entry(event_id).or_default().push(NegRiskLeg {
            market_id,
            question,
            ask_no,
            depth,
            fee_per_share,
        });
    }

    for (event_id, mut legs) in by_event {
        if legs.len() < MIN_MEMBERS {
            continue;
        }
        diag.events_scanned += 1;
        // Basket selection: a leg contributes (1 − ask_no − fee) to the guaranteed margin; keep
        // only positive contributors (others dilute — leaving a member out never hurts, at most
        // one Yes can occur regardless). Sort best-contributor-first for reporting clarity.
        legs.retain(|l| Decimal::ONE - l.ask_no - l.fee_per_share > Decimal::ZERO);
        if legs.len() < MIN_MEMBERS {
            continue;
        }
        legs.sort_by_key(|l| l.ask_no);

        let implied_yes_sum: Decimal = legs.iter().map(|l| Decimal::ONE - l.ask_no).sum();
        if best_sum.as_ref().is_none_or(|(s, _)| implied_yes_sum > *s) {
            best_sum = Some((implied_yes_sum, event_id.clone()));
        }

        let total_cost: Decimal = legs.iter().map(|l| l.ask_no).sum();
        let total_fees: Decimal = legs.iter().map(|l| l.fee_per_share).sum();
        let min_payout = Decimal::from(legs.len() as u64 - 1);
        let gross = min_payout - total_cost;
        let net = gross - total_fees;
        if net < MIN_NET_PROFIT {
            continue;
        }

        let max_units = legs.iter().map(|l| l.depth).min().unwrap_or(Decimal::ZERO);
        opportunities.push(NegRiskOpportunity {
            event_id,
            total_cost,
            min_payout,
            gross_profit_per_unit: gross,
            net_profit_per_unit: net,
            estimated_max_profit_usdc: max_units * net,
            max_units,
            legs,
        });
    }

    opportunities.sort_by_key(|o| std::cmp::Reverse(o.net_profit_per_unit));
    diag.net_arb_events = opportunities.len();
    if let Some((s, e)) = best_sum {
        diag.best_implied_yes_sum = Some(s.round_dp(4).to_string());
        diag.best_event_id = Some(e);
    }
    Ok((opportunities, diag))
}
