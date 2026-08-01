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
/// Ignore events where we can see fewer than this many live member books.
///
/// Was 3, on the stated grounds that "2-member events are equivalent to one binary market (covered
/// by the single-market scanner)". That was **wrong**: the two members of a 2-outcome negRisk event
/// are two SEPARATE markets with their own gamma_ids and their own CLOB books, whereas
/// `arbitrage::scan` only ever compares the Yes and No books *of one market*. Those baskets were
/// therefore covered by nothing. They are also the cross-book case the module doc calls out as the
/// profitable one — one book is trivially kept efficient by its own makers; two books that must
/// stay mutually consistent are not. The payout algebra is unchanged and already correct at k=2:
/// at most one member resolves Yes, so buying No in both pays >= k−1 = 1 for Σask_no, an arb
/// whenever Σask_no + fees < 1.
const MIN_MEMBERS: usize = 2;

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
    /// Best (highest) implied-Yes sum seen across events. **This is the GROSS line only.** Read it
    /// against `best_arb_line` below, never against 1.00 — see that field.
    pub best_implied_yes_sum: Option<String>,
    pub best_event_id: Option<String>,
    /// The fee-adjusted break-even for `best_event_id`: the implied-Yes sum that event would need
    /// to actually be an arb.
    ///
    /// **1.00 is the wrong bar and reporting it alone was actively misleading** (fixed 2026-08-01,
    /// after "best_implied_yes_sum 1.031" looked like a live arb the executor was inexplicably
    /// declining). With per-leg taker fees `rate x p x (1-p)`, buying No across k legs costs
    /// `rate x (S - Σq²)` in fees where `q_i = 1 - ask_no_i` and `S = Σq_i`, so the real line is
    ///
    /// ```text
    /// S  >  1 + rate x (S - Σq²)
    /// ```
    ///
    /// Two consequences the raw sum hides. (1) On a 5%-fee event the bar is ~1.034 at 3 legs rising
    /// to ~1.049 at 30 — **more legs is a worse deal**, because Σq² shrinks as the basket spreads,
    /// which is why every profitable basket in our history ran 3-11 legs. (2) On a FEE-FREE event
    /// (geopolitics, rate 0) the bar really is 1.00, so the same 1.031 that loses money on a
    /// 5%-fee event is a +3.1% arb there.
    pub best_arb_line: Option<String>,
    /// `best_implied_yes_sum - best_arb_line`: how far the closest event is from tradeable, signed.
    /// Negative means the gross overround exists but fees eat it — the common case.
    pub best_line_shortfall: Option<String>,
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
    // (implied_yes_sum, event_id, fee-adjusted arb line, signed shortfall vs that line)
    let mut best_sum: Option<(Decimal, String, Decimal, Decimal)> = None;

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

        let total_cost: Decimal = legs.iter().map(|l| l.ask_no).sum();
        let total_fees: Decimal = legs.iter().map(|l| l.fee_per_share).sum();

        // Rank the "closest event" by SHORTFALL against its own fee-adjusted line, not by the raw
        // implied-Yes sum. Those orderings genuinely differ: a 30-leg 5%-fee event at 1.031 is 1.8
        // points UNDER water, while a fee-free event at 1.005 is already tradeable. Reporting the
        // former as "closest to the arb line" pointed every investigation at the wrong event.
        let (line, shortfall) = arb_line_and_shortfall(implied_yes_sum(&legs), total_fees);
        if best_sum
            .as_ref()
            .is_none_or(|(_, _, _, best_short)| shortfall > *best_short)
        {
            best_sum = Some((implied_yes_sum(&legs), event_id.clone(), line, shortfall));
        }

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
    if let Some((s, e, line, shortfall)) = best_sum {
        diag.best_implied_yes_sum = Some(s.round_dp(4).to_string());
        diag.best_event_id = Some(e);
        diag.best_arb_line = Some(line.round_dp(4).to_string());
        diag.best_line_shortfall = Some(shortfall.round_dp(4).to_string());
    }
    Ok((opportunities, diag))
}

/// Σ implied-Yes across the basket's legs (`Σ (1 − ask_no)`).
fn implied_yes_sum(legs: &[NegRiskLeg]) -> Decimal {
    legs.iter().map(|l| Decimal::ONE - l.ask_no).sum()
}

/// The fee-adjusted break-even implied-Yes sum, and how far a basket is from it (signed).
///
/// Gross arb needs `Σ(1 − ask_no) > 1`; net arb needs it to clear the summed per-leg taker fees too,
/// so the true line is `1 + total_fees`. See `NegRiskDiagnostics::best_arb_line` for why reporting
/// the sum against a flat 1.00 was misleading.
fn arb_line_and_shortfall(implied_yes_sum: Decimal, total_fees: Decimal) -> (Decimal, Decimal) {
    let line = Decimal::ONE + total_fees;
    (line, implied_yes_sum - line)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// k evenly-priced legs summing to `s` implied-Yes, at taker rate `rate`.
    fn even_legs(k: usize, s: Decimal, rate: Decimal) -> Vec<NegRiskLeg> {
        let q = s / Decimal::from(k as u64);
        let ask_no = Decimal::ONE - q;
        (0..k)
            .map(|i| NegRiskLeg {
                market_id: format!("m{i}"),
                question: format!("leg {i}"),
                ask_no,
                depth: dec!(1000),
                fee_per_share: crate::polymarket_fee(rate, ask_no, Decimal::ONE),
            })
            .collect()
    }

    #[test]
    fn a_gross_overround_is_not_an_arb_once_fees_are_charged() {
        // The live 2026-08-01 case that prompted this diagnostic: 20 legs, implied-Yes sum 1.031,
        // 5% taker rate. It reads as "3.1% over the 1.00 arb line" and is in fact underwater.
        let legs = even_legs(20, dec!(1.031), dec!(0.05));
        let fees: Decimal = legs.iter().map(|l| l.fee_per_share).sum();
        let (line, shortfall) = arb_line_and_shortfall(implied_yes_sum(&legs), fees);
        assert!(line > dec!(1.048) && line < dec!(1.049), "line {line}");
        assert!(shortfall < dec!(0), "should be underwater, got {shortfall}");
    }

    #[test]
    fn spreading_the_same_overround_over_more_legs_raises_the_bar() {
        // Why every profitable basket in our history ran 3-11 legs: total fees are
        // rate x (S - sum q^2), and sum q^2 shrinks as the basket spreads, so the bar RISES with k.
        let line_of = |k: usize| {
            let legs = even_legs(k, dec!(1.031), dec!(0.05));
            arb_line_and_shortfall(
                implied_yes_sum(&legs),
                legs.iter().map(|l| l.fee_per_share).sum(),
            )
            .0
        };
        let (three, eleven, thirty) = (line_of(3), line_of(11), line_of(30));
        assert!(
            three < eleven && eleven < thirty,
            "{three} {eleven} {thirty}"
        );
        // The spread between the shapes is worth real money: ~1.034 at 3 legs vs ~1.049 at 30, so
        // there is a band of overrounds a concentrated basket captures and a spread one cannot.
        // 1.04 sits inside that band.
        assert!(three < dec!(1.04), "3-leg line {three} should clear 1.04");
        assert!(thirty > dec!(1.04), "30-leg line {thirty} should not");
        // And at the live 2026-08-01 sum of 1.031 NONE of them clear — the shape advantage narrows
        // the gap, it does not manufacture an arb that isn't there.
        assert!(three > dec!(1.031));
    }

    #[test]
    fn fee_free_events_really_do_have_a_1_00_arb_line() {
        // Geopolitics markets carry rate 0, so there the raw sum IS the right comparison — the same
        // 1.031 that loses money on a 5%-fee event is a live +3.1% arb here, at any leg count.
        for k in [3usize, 20, 30] {
            let legs = even_legs(k, dec!(1.031), dec!(0));
            let (line, shortfall) = arb_line_and_shortfall(
                implied_yes_sum(&legs),
                legs.iter().map(|l| l.fee_per_share).sum(),
            );
            assert_eq!(line, dec!(1), "k={k}");
            assert!(shortfall > dec!(0.03), "k={k} shortfall {shortfall}");
        }
    }

    #[test]
    fn a_basket_exactly_on_its_line_has_zero_shortfall() {
        let (line, shortfall) = arb_line_and_shortfall(dec!(1.05), dec!(0.05));
        assert_eq!(line, dec!(1.05));
        assert_eq!(shortfall, dec!(0));
    }
}
