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
//! ## Where the prices come from (P5 increment 2, 2026-08-01)
//! Each leg is priced from the **live WebSocket book** when one is available, fresh and in sync,
//! and falls back to the polled `orderbook_snapshots` row otherwise. That fallback is not a
//! degraded mode to be avoided — it is the correct answer whenever the feed cannot vouch for a
//! book, and `LiveBookStore::get_fresh` already refuses to hand back anything desynced or on a
//! dead connection, so this scanner never reasons about feed health itself.
//!
//! The staleness this removes was substantial: the snapshot query accepts books up to **30 minutes**
//! old, against an arb line that events cross for seconds. Every leg carries both readings so the
//! two can be scored side by side in the same pass (`best_line_shortfall` vs
//! `best_line_shortfall_snapshot`) — if they never diverge, the live feed bought nothing here, and
//! that is a result worth being able to see rather than assume away.
//!
//! Remaining execution-risk caveats as the single-market scanner (per-level depth, no simultaneous
//! multi-leg fill); paper-only, journaled for Hermes.

use anyhow::Result;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use super::arbitrage::best_ask;
use crate::ingester::clob_ws::LiveBookStore;

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
    /// Live (WebSocket) when a fresh in-sync book exists, otherwise the polled snapshot.
    pub ask_no: Decimal,
    /// Depth at that best ask (shares). Always read from the SAME source as `ask_no`: a live price
    /// against snapshot depth would size the basket on liquidity that may no longer exist.
    pub depth: Decimal,
    /// Estimated taker fee per share for this leg.
    pub fee_per_share: Decimal,
    /// What the polled snapshot said, kept even when `ask_no` came from the live feed. This is what
    /// makes the live-vs-stale comparison measurable in-process rather than only across deploys —
    /// see `NegRiskDiagnostics::best_line_shortfall_snapshot`.
    #[serde(default)]
    pub ask_no_snapshot: Decimal,
    #[serde(default)]
    pub fee_per_share_snapshot: Decimal,
    /// True when `ask_no`/`depth` came from the live WebSocket book.
    #[serde(default)]
    pub from_live: bool,
}

/// Which of a leg's two price readings to score a basket with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PriceSource {
    /// Live book where available, polled snapshot elsewhere — what the executor actually trades on.
    Best,
    /// Polled snapshot only, ignoring the live feed — the counterfactual, for diagnostics.
    Snapshot,
}

impl NegRiskLeg {
    fn ask(&self, src: PriceSource) -> Decimal {
        match src {
            PriceSource::Best => self.ask_no,
            PriceSource::Snapshot => self.ask_no_snapshot,
        }
    }

    fn fee(&self, src: PriceSource) -> Decimal {
        match src {
            PriceSource::Best => self.fee_per_share,
            PriceSource::Snapshot => self.fee_per_share_snapshot,
        }
    }

    /// A leg contributes this much to the guaranteed margin. Non-positive legs only dilute.
    fn contribution(&self, src: PriceSource) -> Decimal {
        Decimal::ONE - self.ask(src) - self.fee(src)
    }
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

    // === P5 increment 2: live-book sourcing (2026-08-01) ===
    /// Legs priced from the live WebSocket book.
    #[serde(default)]
    pub legs_from_live_book: usize,
    /// Legs that fell back to the polled snapshot — no live book, or one flagged desynced/offline.
    #[serde(default)]
    pub legs_from_snapshot: usize,
    /// The same best-shortfall metric computed on snapshot prices ALONE.
    ///
    /// This is the counterfactual, and it is the whole point of keeping both readings: comparing
    /// `best_line_shortfall` against a number from a previous deploy confounds the price source
    /// with whatever the market did in between. Computed in the same pass over the same events,
    /// the pair isolates what the live feed is actually worth. If they stay equal, the 5-minute
    /// snapshot was never the binding constraint and increment 2 bought nothing — a result worth
    /// being able to see.
    #[serde(default)]
    pub best_line_shortfall_snapshot: Option<String>,
}

/// One member row as the scan query returns it:
/// `(event_id, gamma_id, slug, question, taker_fee_rate, snapshot asks, No token_id)`.
type MemberRow = (
    String,
    String,
    String,
    String,
    Option<Decimal>,
    serde_json::Value,
    String,
);

/// Scan all active negRisk events over the books already ingested. Returns opportunities sorted
/// best-first plus diagnostics.
pub async fn scan_negrisk(
    pool: &PgPool,
    books: Option<&LiveBookStore>,
) -> Result<(Vec<NegRiskOpportunity>, NegRiskDiagnostics)> {
    // Latest fresh No-book per active member of every negRisk event with enough visible members.
    // `token_id` comes along because it is the live feed's key — the snapshot row already knows
    // which token it described, so no second lookup is needed to join the two sources.
    let rows: Vec<MemberRow> = sqlx::query_as(
        r#"
        SELECT m.event_id, m.gamma_id, COALESCE(m.slug, ''), m.question, m.taker_fee_rate,
               no_snap.asks, no_snap.token_id
        FROM market_data.markets m
        JOIN LATERAL (
            SELECT asks, token_id FROM market_data.orderbook_snapshots
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
    // Same, scored on snapshot prices only — the counterfactual.
    let mut best_snapshot_shortfall: Option<Decimal> = None;

    // Group rows by event_id (rows arrive sorted).
    let mut by_event: std::collections::BTreeMap<String, Vec<NegRiskLeg>> =
        std::collections::BTreeMap::new();
    for (event_id, market_id, slug, question, fee_rate, no_asks, token_id) in rows {
        let (snap_ask, snap_depth) = best_ask(&no_asks);
        if snap_ask <= Decimal::ZERO || snap_ask >= Decimal::ONE || snap_depth <= Decimal::ZERO {
            continue;
        }
        diag.member_books += 1;
        let rate = fee_rate.unwrap_or_else(|| crate::polymarket_taker_fee_rate(&slug));

        // Prefer the live book. `get_fresh` already withholds anything desynced or on a dead
        // connection, so "no live book" and "a live book we do not trust" collapse into the same
        // fallback — the scanner never has to reason about feed health.
        let live = books
            .and_then(|b| b.get_fresh(&token_id))
            .and_then(|b| b.best_ask_with_size())
            .filter(|(p, d)| *p > Decimal::ZERO && *p < Decimal::ONE && *d > Decimal::ZERO);

        let (ask_no, depth) = match live {
            Some(pair) => {
                diag.legs_from_live_book += 1;
                pair
            }
            None => {
                diag.legs_from_snapshot += 1;
                (snap_ask, snap_depth)
            }
        };

        by_event.entry(event_id).or_default().push(NegRiskLeg {
            market_id,
            question,
            ask_no,
            depth,
            fee_per_share: crate::polymarket_fee(rate, ask_no, Decimal::ONE),
            ask_no_snapshot: snap_ask,
            fee_per_share_snapshot: crate::polymarket_fee(rate, snap_ask, Decimal::ONE),
            from_live: live.is_some(),
        });
    }

    for (event_id, mut legs) in by_event {
        if legs.len() < MIN_MEMBERS {
            continue;
        }
        diag.events_scanned += 1;

        // Score the same event on snapshot prices alone before any live-priced filtering narrows
        // the leg set, so the counterfactual answers "what would the old scanner have seen here",
        // not "what would it have seen among the legs the new one kept".
        if let Some((_, _, snap_short)) = score_basket(&legs, PriceSource::Snapshot) {
            if best_snapshot_shortfall.is_none_or(|b| snap_short > b) {
                best_snapshot_shortfall = Some(snap_short);
            }
        }

        // Basket selection: a leg contributes (1 − ask_no − fee) to the guaranteed margin; keep
        // only positive contributors (others dilute — leaving a member out never hurts, at most
        // one Yes can occur regardless). Sort best-contributor-first for reporting clarity.
        legs.retain(|l| l.contribution(PriceSource::Best) > Decimal::ZERO);
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
    diag.best_line_shortfall_snapshot = best_snapshot_shortfall.map(|s| s.round_dp(4).to_string());
    Ok((opportunities, diag))
}

/// Score an event under one price source: `(implied_yes_sum, arb line, signed shortfall)`.
///
/// Applies the same positive-contributor selection and `MIN_MEMBERS` floor the real basket does,
/// so the snapshot counterfactual is scored by the identical rules and the two numbers are
/// comparable. `None` when too few legs survive to form a basket.
fn score_basket(legs: &[NegRiskLeg], src: PriceSource) -> Option<(Decimal, Decimal, Decimal)> {
    let kept: Vec<&NegRiskLeg> = legs
        .iter()
        .filter(|l| l.contribution(src) > Decimal::ZERO)
        .collect();
    if kept.len() < MIN_MEMBERS {
        return None;
    }
    let sum: Decimal = kept.iter().map(|l| Decimal::ONE - l.ask(src)).sum();
    let fees: Decimal = kept.iter().map(|l| l.fee(src)).sum();
    let (line, shortfall) = arb_line_and_shortfall(sum, fees);
    Some((sum, line, shortfall))
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

    /// k evenly-priced legs summing to `s` implied-Yes, at taker rate `rate`. Both price readings
    /// are identical, i.e. the live book agreed with the snapshot — vary them explicitly when the
    /// test is about the difference.
    fn even_legs(k: usize, s: Decimal, rate: Decimal) -> Vec<NegRiskLeg> {
        let q = s / Decimal::from(k as u64);
        let ask_no = Decimal::ONE - q;
        (0..k)
            .map(|i| leg(&format!("m{i}"), ask_no, ask_no, rate))
            .collect()
    }

    /// One leg with independently chosen live and snapshot asks.
    fn leg(id: &str, live_ask: Decimal, snap_ask: Decimal, rate: Decimal) -> NegRiskLeg {
        NegRiskLeg {
            market_id: id.to_string(),
            question: format!("leg {id}"),
            ask_no: live_ask,
            depth: dec!(1000),
            fee_per_share: crate::polymarket_fee(rate, live_ask, Decimal::ONE),
            ask_no_snapshot: snap_ask,
            fee_per_share_snapshot: crate::polymarket_fee(rate, snap_ask, Decimal::ONE),
            from_live: live_ask != snap_ask,
        }
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

    // === P5 increment 2: live-book sourcing ===

    #[test]
    fn a_book_that_moved_since_the_snapshot_turns_a_miss_into_an_arb() {
        // The case increment 2 exists for. The polled snapshot is accepted up to 30 minutes old;
        // an event can cross the arb line and come back well inside that. Fee-free event, so the
        // line is exactly 1.00 and the arithmetic is readable: snapshot 0.67 x 3 sums to 0.99
        // (a miss), live 0.66 x 3 sums to 1.02 (a 2% arb).
        let legs: Vec<NegRiskLeg> = (0..3)
            .map(|i| leg(&format!("m{i}"), dec!(0.66), dec!(0.67), dec!(0)))
            .collect();

        let (_, _, live_short) = score_basket(&legs, PriceSource::Best).expect("live basket");
        let (_, _, snap_short) =
            score_basket(&legs, PriceSource::Snapshot).expect("snapshot basket");

        assert_eq!(snap_short, dec!(-0.01), "stale prices miss it");
        assert_eq!(live_short, dec!(0.02), "live prices find it");
        assert!(snap_short < Decimal::ZERO && live_short > Decimal::ZERO);
    }

    #[test]
    fn agreeing_readings_score_identically() {
        // The null result, and worth pinning: if the live book agrees with the snapshot, the two
        // metrics must coincide exactly. Otherwise the A/B would show a phantom improvement and
        // credit the feed for arithmetic rather than for freshness.
        let legs = even_legs(5, dec!(1.01), dec!(0.05));
        assert_eq!(
            score_basket(&legs, PriceSource::Best),
            score_basket(&legs, PriceSource::Snapshot)
        );
    }

    #[test]
    fn the_live_reading_can_also_be_worse_than_the_snapshot() {
        // Freshness is not a synonym for opportunity. A book that moved AGAINST us since the
        // snapshot must score worse, not better — the counterfactual has to be able to say the
        // stale price was flattering, which is exactly the case where trading on it would have
        // mis-filled.
        let legs: Vec<NegRiskLeg> = (0..3)
            .map(|i| leg(&format!("m{i}"), dec!(0.68), dec!(0.66), dec!(0)))
            .collect();
        let (_, _, live_short) = score_basket(&legs, PriceSource::Best).unwrap();
        let (_, _, snap_short) = score_basket(&legs, PriceSource::Snapshot).unwrap();
        assert_eq!(snap_short, dec!(0.02), "the stale price looked tradeable");
        assert_eq!(live_short, dec!(-0.04), "live says it is not");
    }

    #[test]
    fn a_basket_below_the_member_floor_scores_nothing() {
        let legs = vec![leg("solo", dec!(0.4), dec!(0.4), dec!(0))];
        assert!(score_basket(&legs, PriceSource::Best).is_none());
        assert!(score_basket(&legs, PriceSource::Snapshot).is_none());
    }
}
