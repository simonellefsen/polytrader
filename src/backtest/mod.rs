//! Offline backtest / replay harness (Phase 1 of the roadmap's first thread).
//!
//! Read-only by construction: this module never writes the journal, never touches the paper engine,
//! and never places an order. It only *reads* already-journaled events and replays them through an
//! in-memory portfolio.
//!
//! It reuses the Phase-0 pure cores so a replayed decision is computed by the SAME logic as
//! production — there is no second copy of the fusion or risk-gate math to drift:
//!   * [`crate::strategy::fuse_from_attribution`] — re-weight a stored decision_report's per-signal
//!     scores under a candidate weight vector (no processors re-run).
//!   * [`crate::risk::RiskManager::gate`] — the pure approve/trim/reject gate, fed a simulated
//!     [`crate::risk::PortfolioExposure`].
//!   * [`crate::settlement_payout_and_pnl`] — the exact production settlement formula, so
//!     [`SimPortfolio::settle`] cannot diverge from how live realizes P&L.
//!
//! Two simulators over already-loaded events:
//!   * [`replay_fills`] — reconstruct the live equity path from the ACTUAL journaled fills +
//!     market resolutions. Together with [`realized_from_settlements`] this is the FIDELITY ANCHOR:
//!     the accounting must reproduce the live realized P&L before any counterfactual is trusted.
//!   * [`simulate_counterfactual`] — the analysis engine. Re-derive each decision from the stored
//!     decision_report attribution under a CANDIDATE {weights, min_net_edge}, size with quarter-Kelly,
//!     gate, and fill at the report's `target_mid` (a Phase-1 approximation; Phase 3 re-walks the real
//!     book), settling as markets resolve. Phase 2's config sweep will call this.

#![allow(dead_code)]

use crate::risk::{cluster_key, PortfolioExposure, RiskConfig, RiskManager};
use crate::strategy::fuse_from_attribution;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::{BTreeMap, BTreeSet};

/// Live paper portfolio starts from this virtual USDC base (mirrors `settle_resolved_positions` +
/// the seed snapshot in main.rs). Equity identity in the sim: `equity = base + realized` because cash
/// = base − locked + realized and equity = cash + locked (we hold no fee model here; realized P&L is
/// fee-independent, which is exactly what the fidelity anchor checks).
pub const SIM_BASE_USDC: Decimal = dec!(10000);

/// A single executed buy (from `autonomous_paper_execution` action=filled, or any source that opens a
/// paper position). `cost` is the collateral locked = what was paid = shares × avg fill price.
#[derive(Clone, Debug)]
pub struct FillEvent {
    pub at: DateTime<Utc>,
    pub market_id: String,
    pub outcome: String,
    pub shares: Decimal,
    pub cost: Decimal,
}

/// A market resolution: the winning outcome and (best-known) resolution time. `at == None` ⇒ settle at
/// the end of the walk (we know it resolved but not exactly when).
#[derive(Clone, Debug)]
pub struct Resolution {
    pub market_id: String,
    pub winning_outcome: String,
    pub at: Option<DateTime<Utc>>,
}

/// A journaled `decision_report` reduced to what the counterfactual needs.
#[derive(Clone, Debug)]
pub struct ReportRow {
    pub at: DateTime<Utc>,
    pub market_id: String,
    pub target_outcome: String,
    pub target_mid: Decimal,
    /// The `report.attribution` object (per-signal score/confidence + fee_impact).
    pub attribution: serde_json::Value,
}

/// One settled paper position (from a `paper_position_settled` event) — for the fidelity anchor.
#[derive(Clone, Debug)]
pub struct SettlementRow {
    pub won: bool,
    pub shares: Decimal,
    pub cost_basis: Decimal,
    /// The realized P&L the live system recorded — what we must reproduce.
    pub recorded_pnl: Decimal,
}

/// A fill the counterfactual decided to make, captured so a Phase-3 pass can re-price it against the
/// real order book. `approved_usdc` and `target_mid` are the inputs the live executor used to size and
/// limit the order.
#[derive(Clone, Debug, PartialEq)]
pub struct FillDecision {
    pub at: DateTime<Utc>,
    pub market_id: String,
    pub outcome: String,
    pub approved_usdc: Decimal,
    pub target_mid: Decimal,
}

/// Outcome of a simulation run.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SimResult {
    pub realized: Decimal,
    /// Mark-to-market P&L of positions still open at the end (Σ shares·mark − cost), using each
    /// market's latest decision-report mid. Zero when no marks are supplied. This is where config
    /// differences on UNRESOLVED markets show up — realized alone can't rank configs because the
    /// resolved markets are entered under every config.
    pub unrealized: Decimal,
    /// Total taker fees paid on entries (Phase-3 realistic fills only; 0 for fill-at-mid).
    pub fees: Decimal,
    /// Positions that actually settled (we held them at resolution).
    pub settled_positions: usize,
    pub wins: usize,
    pub losses: usize,
    /// Positions opened during the run.
    pub fills: usize,
    /// Positions still open at the end (their market never resolved within the data).
    pub open_at_end: usize,
    /// Running realized P&L after each settlement batch (a coarse equity path).
    pub realized_curve: Vec<Decimal>,
    /// The fill decisions made (in order), for Phase-3 realistic re-pricing.
    pub fill_log: Vec<FillDecision>,
}

impl SimResult {
    /// Total P&L net of fees (realized + open marks − fees).
    pub fn total_pnl(&self) -> Decimal {
        self.realized + self.unrealized - self.fees
    }

    /// Final equity = base + realized + open marks − fees.
    pub fn final_equity(&self) -> Decimal {
        SIM_BASE_USDC + self.realized + self.unrealized - self.fees
    }
}

/// Per-market latest mid for marking open positions: market_id → (latest target_outcome, latest mid).
/// For a held outcome, the mark is `mid` if it matches the latest target side, else `1 − mid` (binary
/// complement). Built from the decision_report stream by [`build_marks`].
pub type Marks = BTreeMap<String, (String, Decimal)>;

#[derive(Clone, Debug, Default)]
struct Position {
    shares: Decimal,
    /// Collateral locked = cost basis (what was paid).
    cost: Decimal,
}

/// In-memory paper portfolio. Accounting mirrors `paper::PaperTradingEngine` +
/// `settle_resolved_positions` for the fields the gate and settlement care about.
#[derive(Clone, Debug, Default)]
pub struct SimPortfolio {
    positions: BTreeMap<(String, String), Position>,
    realized: Decimal,
}

impl SimPortfolio {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn realized(&self) -> Decimal {
        self.realized
    }

    /// Total collateral locked across all open positions.
    pub fn total_locked(&self) -> Decimal {
        self.positions
            .values()
            .map(|p| p.cost)
            .fold(dec!(0), |a, b| a + b)
    }

    pub fn open_count(&self) -> usize {
        self.positions
            .values()
            .filter(|p| p.shares > dec!(0))
            .count()
    }

    /// True if any open position exists in this market (either outcome) — mirrors the live executor's
    /// "one directional position per market" dedup guard.
    pub fn has_position_in_market(&self, market_id: &str) -> bool {
        self.positions
            .iter()
            .any(|((m, _), p)| m == market_id && p.shares > dec!(0))
    }

    /// Mark-to-market P&L of all open positions: Σ (shares·mark − cost). `marks` gives each market's
    /// latest (target_outcome, mid); a held outcome marks at `mid` if it is that target side, else
    /// `1 − mid` (binary complement). Positions in a market with no mark contribute 0 (held at cost).
    pub fn unrealized(&self, marks: &Marks) -> Decimal {
        let mut u = dec!(0);
        for ((market, outcome), p) in &self.positions {
            if p.shares <= dec!(0) {
                continue;
            }
            if let Some((target_outcome, mid)) = marks.get(market) {
                let mark = if outcome.eq_ignore_ascii_case(target_outcome) {
                    *mid
                } else {
                    dec!(1) - *mid
                };
                u += p.shares * mark - p.cost;
            }
        }
        u
    }

    /// Collateral locked in a single market (across outcomes).
    fn market_locked(&self, market_id: &str) -> Decimal {
        self.positions
            .iter()
            .filter(|((m, _), _)| m == market_id)
            .map(|(_, p)| p.cost)
            .fold(dec!(0), |a, b| a + b)
    }

    /// Open a (or add to a) position.
    pub fn fill(&mut self, market_id: &str, outcome: &str, shares: Decimal, cost: Decimal) {
        let pos = self
            .positions
            .entry((market_id.to_string(), outcome.to_string()))
            .or_default();
        pos.shares += shares;
        pos.cost += cost;
    }

    /// Settle every open position in `market_id` against `winning_outcome`. Uses the EXACT production
    /// formula ([`crate::settlement_payout_and_pnl`]). Returns the number of positions settled and how
    /// many won (so the caller can track W/L). Settled positions are removed (idempotent: a second
    /// settle of the same market is a no-op).
    pub fn settle(&mut self, market_id: &str, winning_outcome: &str) -> (usize, usize) {
        let keys: Vec<(String, String)> = self
            .positions
            .keys()
            .filter(|(m, _)| m == market_id)
            .cloned()
            .collect();
        let (mut settled, mut wins) = (0usize, 0usize);
        for key in keys {
            let pos = self.positions.remove(&key).expect("key just collected");
            if pos.shares <= dec!(0) {
                continue;
            }
            let won = key.1.eq_ignore_ascii_case(winning_outcome);
            let (_payout, pnl) = crate::settlement_payout_and_pnl(won, pos.shares, pos.cost);
            self.realized += pnl;
            settled += 1;
            if won {
                wins += 1;
            }
        }
        (settled, wins)
    }

    /// Build the [`PortfolioExposure`] the pure gate expects, mirroring `RiskManager::load_exposure`
    /// but from sim state. `slug_of` maps gamma_id → slug for cluster classification.
    fn exposure(&self, market_id: &str, slug_of: &BTreeMap<String, String>) -> PortfolioExposure {
        let total_locked = self.total_locked();
        // total_value (= virtual_usdc + total_locked) == base + realized, mirroring live's
        // (10000 − locked + realized) + locked.
        let virtual_usdc = SIM_BASE_USDC + self.realized - total_locked;
        let ckey = slug_of
            .get(market_id)
            .map(|s| cluster_key(s))
            .unwrap_or("uncorrelated");
        let market_locked = self.market_locked(market_id);
        let cluster_locked = if ckey == "uncorrelated" {
            market_locked
        } else {
            self.positions
                .iter()
                .filter(|((m, _), p)| {
                    p.shares > dec!(0)
                        && slug_of
                            .get(m)
                            .map(|s| cluster_key(s) == ckey)
                            .unwrap_or(false)
                })
                .map(|(_, p)| p.cost)
                .fold(dec!(0), |a, b| a + b)
        };
        PortfolioExposure {
            virtual_usdc,
            total_locked,
            total_pnl: self.realized, // sim carries no open marks ⇒ unrealized 0
            market_locked,
            cluster_key: ckey,
            cluster_locked,
        }
    }
}

/// FIDELITY ANCHOR (pure): the live realized P&L recomputed from the journaled settlements via the
/// production formula. Must equal the portfolio's recorded realized — proves [`SimPortfolio::settle`]
/// and the settlement formula reproduce every live settlement exactly. Returns (realized, settled,
/// wins, losses) and whether every per-record P&L matched what was recorded.
pub fn realized_from_settlements(rows: &[SettlementRow]) -> (Decimal, usize, usize, usize, bool) {
    let mut realized = dec!(0);
    let (mut wins, mut losses) = (0usize, 0usize);
    let mut all_match = true;
    for r in rows {
        let (_payout, pnl) = crate::settlement_payout_and_pnl(r.won, r.shares, r.cost_basis);
        if pnl != r.recorded_pnl {
            all_match = false;
        }
        realized += pnl;
        if r.won {
            wins += 1;
        } else {
            losses += 1;
        }
    }
    (realized, rows.len(), wins, losses, all_match)
}

/// Ground-truth accounting replay: apply the ACTUAL fills, then settle on each resolution. Validates
/// the fill→lock→settle pipeline end-to-end (not just the settlement formula).
pub fn replay_fills(mut fills: Vec<FillEvent>, resolutions: &[Resolution]) -> SimResult {
    fills.sort_by_key(|f| f.at);
    let mut sim = SimPortfolio::new();
    for f in &fills {
        sim.fill(&f.market_id, &f.outcome, f.shares, f.cost);
    }
    settle_in_time_order(&mut sim, resolutions, fills.len())
}

/// Counterfactual configuration for [`simulate_counterfactual`].
#[derive(Clone, Debug)]
pub struct CounterfactualConfig {
    /// Candidate per-processor weights (missing ⇒ 1.0, clamped like the live read path).
    pub weights: BTreeMap<String, Decimal>,
    /// Risk config to gate with (typically `RiskConfig::default()` with `min_net_edge` overridden).
    pub risk: RiskConfig,
}

/// The analysis engine: re-derive every decision from stored decision_reports under a candidate config
/// and replay it through the sim. For each report (in time order), first settle any market that has
/// resolved by then, then — unless we already hold the market — fuse the stored scores under the
/// candidate weights, subtract the report's own fee, quarter-Kelly size, run the pure gate, and (if
/// approved) fill at `target_mid`. Settles all remaining resolved markets at the end.
pub fn simulate_counterfactual(
    reports: &[ReportRow],
    resolutions: &[Resolution],
    slug_of: &BTreeMap<String, String>,
    marks: &Marks,
    cfg: &CounterfactualConfig,
) -> SimResult {
    // Borrow + sort references (not the data) so a config sweep can reuse one loaded report set across
    // many runs without cloning ~50k rows per config.
    let mut order: Vec<&ReportRow> = reports.iter().collect();
    order.sort_by_key(|r| r.at);
    let rm = RiskManager::new(cfg.risk.clone());

    // Resolutions with a known time, sorted; the rest are applied at the end.
    let mut timed: Vec<&Resolution> = resolutions.iter().filter(|r| r.at.is_some()).collect();
    timed.sort_by_key(|r| r.at.unwrap());

    let mut sim = SimPortfolio::new();
    let mut settled_markets: BTreeSet<String> = BTreeSet::new();
    let mut res = SimResult::default();
    let mut next_res = 0usize;

    for rep in &order {
        // Settle anything resolved at or before this report's timestamp.
        while next_res < timed.len() && timed[next_res].at.unwrap() <= rep.at {
            apply_resolution(&mut sim, timed[next_res], &mut settled_markets, &mut res);
            next_res += 1;
        }

        // Dedup: one position per market (either outcome), like the live executor.
        if sim.has_position_in_market(&rep.market_id) {
            continue;
        }
        if !(rep.target_mid > dec!(0) && rep.target_mid < dec!(1)) {
            continue;
        }

        let gross = fuse_from_attribution(&rep.attribution, &cfg.weights);
        let net = gross - report_fee(&rep.attribution);
        // win_prob ≈ target_mid + net (same crude estimate as the live DR generator).
        let win_prob = (rep.target_mid + net).min(dec!(0.99)).max(dec!(0.01));
        let total_value = SIM_BASE_USDC + sim.realized();
        let sizing = rm.kelly_size(win_prob, rep.target_mid, total_value);
        if sizing.recommended_usdc <= dec!(0) {
            continue;
        }
        let exp = sim.exposure(&rep.market_id, slug_of);
        let check = rm.gate(
            &rep.market_id,
            net,
            sizing.recommended_usdc,
            rep.target_mid,
            &exp,
        );
        if !check.approved {
            continue;
        }
        let approved_usdc = check.recommended_size.unwrap_or(sizing.recommended_usdc);
        if approved_usdc <= dec!(0) {
            continue;
        }
        // Phase-1 fill approximation: fill the whole size at target_mid (cost == approved_usdc). Phase 3
        // will re-walk the stored order book for a realistic fill price.
        let shares = (approved_usdc / rep.target_mid).round_dp(2);
        if shares <= dec!(0) {
            continue;
        }
        sim.fill(&rep.market_id, &rep.target_outcome, shares, approved_usdc);
        res.fills += 1;
        res.fill_log.push(FillDecision {
            at: rep.at,
            market_id: rep.market_id.clone(),
            outcome: rep.target_outcome.clone(),
            approved_usdc,
            target_mid: rep.target_mid,
        });
    }

    // Settle the rest: remaining timed resolutions, then untimed ones.
    while next_res < timed.len() {
        apply_resolution(&mut sim, timed[next_res], &mut settled_markets, &mut res);
        next_res += 1;
    }
    for r in resolutions.iter().filter(|r| r.at.is_none()) {
        apply_resolution(&mut sim, r, &mut settled_markets, &mut res);
    }

    res.realized = sim.realized();
    res.unrealized = sim.unrealized(marks);
    res.open_at_end = sim.open_count();
    res
}

/// Build the per-market marks (latest target_outcome + mid) from the decision_report stream — the
/// latest report per market wins. Used to mark open positions at the end of a counterfactual.
pub fn build_marks(reports: &[ReportRow]) -> Marks {
    let mut latest: BTreeMap<String, (DateTime<Utc>, String, Decimal)> = BTreeMap::new();
    for r in reports {
        match latest.get(&r.market_id) {
            Some((t, _, _)) if *t >= r.at => {}
            _ => {
                latest.insert(
                    r.market_id.clone(),
                    (r.at, r.target_outcome.clone(), r.target_mid),
                );
            }
        }
    }
    latest
        .into_iter()
        .map(|(k, (_, o, m))| (k, (o, m)))
        .collect()
}

/// A single order-book price level.
#[derive(Clone, Debug, PartialEq)]
pub struct Level {
    pub price: Decimal,
    pub size: Decimal,
}

/// Limit-order slippage ceiling and max price, mirroring the live executor
/// (`limit = min(mid × 1.20, 0.99)`, shares = approved_usdc / limit).
const SLIPPAGE_CAP: Decimal = dec!(1.20);
const MAX_LIMIT: Decimal = dec!(0.99);
/// Walk `asks` best-first (ascending) for a LIMIT buy: fill up to `target_shares`, only at prices
/// ≤ `limit_price`. Mirrors the production `match_against_book` buy/limit path, including its
/// REAL Polymarket taker fee per level: `shares × rate × p × (1−p)` (`fee_rate` is the per-market
/// RATE — geopolitics 0, crypto 0.07, … — not a flat % of notional; the old flat 0.5%-of-cost
/// both charged fee-free geopolitics and underpriced crypto). Returns
/// `(filled_shares, gross_cost, fee)`. A thin/expensive book (best ask > limit) yields a zero
/// fill — exactly the live "no_fill_at_limit" outcome.
pub fn walk_asks_limit_buy(
    asks: &[Level],
    limit_price: Decimal,
    target_shares: Decimal,
    fee_rate: Decimal,
) -> (Decimal, Decimal, Decimal) {
    let mut sorted: Vec<&Level> = asks.iter().collect();
    sorted.sort_by_key(|l| l.price); // best (cheapest) ask first
    let mut remaining = target_shares;
    let mut filled = dec!(0);
    let mut cost = dec!(0);
    let mut fee = dec!(0);
    for lvl in sorted {
        if remaining <= dec!(0) {
            break;
        }
        if lvl.price > limit_price {
            break; // ask above the ceiling — stop (book sorted ascending)
        }
        let take = lvl.size.min(remaining);
        if take <= dec!(0) {
            continue;
        }
        filled += take;
        cost += lvl.price * take;
        fee += crate::polymarket_fee(fee_rate, lvl.price, take);
        remaining -= take;
    }
    (filled, cost, fee.round_dp(6))
}

/// Phase-3 realistic re-pricing: take the fill DECISIONS from a counterfactual (which markets/outcomes
/// were entered, with what approved size) and re-price each against the actual order book at the
/// decision time, applying taker fees — then settle and mark exactly as the at-mid run does. This
/// turns the counterfactual from an optimistic fill-at-mid estimate into a credible backtest with real
/// entry costs. `books[i]` is the ask side for `fill_log[i]` (None ⇒ no snapshot ⇒ skip, like a live
/// no-fill). NOTE: the fill DECISIONS are taken from the at-mid pass; the gate sequence is not re-run
/// with realistic prices (a second-order effect), so this re-prices rather than re-decides.
pub fn reprice_realistic(
    fill_log: &[FillDecision],
    books: &[Option<Vec<Level>>],
    resolutions: &[Resolution],
    marks: &Marks,
    fee_rates: &BTreeMap<String, Decimal>,
) -> SimResult {
    let mut sim = SimPortfolio::new();
    let mut total_fees = dec!(0);
    let mut fills = 0usize;
    for (d, book) in fill_log.iter().zip(books.iter()) {
        let Some(asks) = book else { continue };
        let limit = (d.target_mid * SLIPPAGE_CAP).min(MAX_LIMIT);
        if limit <= dec!(0) {
            continue;
        }
        let target_shares = (d.approved_usdc / limit).round_dp(2);
        // Per-market Polymarket taker rate (geopolitics 0 …); unmapped markets use the Other rate.
        let rate = fee_rates.get(&d.market_id).copied().unwrap_or(dec!(0.05));
        let (filled, cost, fee) = walk_asks_limit_buy(asks, limit, target_shares, rate);
        if filled <= dec!(0) {
            continue; // book too thin/expensive — no fill (matches live)
        }
        sim.fill(&d.market_id, &d.outcome, filled, cost);
        total_fees += fee;
        fills += 1;
    }

    // Settle resolved markets, then mark the rest — same accounting as the at-mid path.
    let mut res = settle_in_time_order(&mut sim, resolutions, fills);
    res.unrealized = sim.unrealized(marks);
    res.fees = total_fees;
    res
}

/// One row of a config sweep: a config's label, its simulation result, and the max drawdown of its
/// realized equity path.
#[derive(Clone, Debug)]
pub struct SweepRow {
    pub label: String,
    pub result: SimResult,
    pub max_drawdown: Decimal,
}

/// Run [`simulate_counterfactual`] for each labelled config against one shared (borrowed) report set —
/// the Phase-2 config sweep. Borrowing means no per-config clone of the (large) report set.
pub fn run_sweep(
    reports: &[ReportRow],
    resolutions: &[Resolution],
    slug_of: &BTreeMap<String, String>,
    marks: &Marks,
    configs: &[(String, CounterfactualConfig)],
) -> Vec<SweepRow> {
    configs
        .iter()
        .map(|(label, cfg)| {
            let result = simulate_counterfactual(reports, resolutions, slug_of, marks, cfg);
            let mdd = max_drawdown(&result.realized_curve);
            SweepRow {
                label: label.clone(),
                result,
                max_drawdown: mdd,
            }
        })
        .collect()
}

/// Max peak-to-trough decline of a running-realized path (equity drawdown == realized drawdown since
/// the base is constant). The implicit pre-settlement peak is 0, so an all-losses path reports its
/// full depth.
pub fn max_drawdown(realized_curve: &[Decimal]) -> Decimal {
    let mut peak = dec!(0);
    let mut mdd = dec!(0);
    for &r in realized_curve {
        if r > peak {
            peak = r;
        }
        let dd = peak - r;
        if dd > mdd {
            mdd = dd;
        }
    }
    mdd
}

/// Settle every resolution in (best-effort) time order. Used by [`replay_fills`].
fn settle_in_time_order(
    sim: &mut SimPortfolio,
    resolutions: &[Resolution],
    fills: usize,
) -> SimResult {
    let mut res = SimResult {
        fills,
        ..Default::default()
    };
    let mut settled_markets: BTreeSet<String> = BTreeSet::new();
    let mut timed: Vec<&Resolution> = resolutions.iter().filter(|r| r.at.is_some()).collect();
    timed.sort_by_key(|r| r.at.unwrap());
    for r in timed {
        apply_resolution(sim, r, &mut settled_markets, &mut res);
    }
    for r in resolutions.iter().filter(|r| r.at.is_none()) {
        apply_resolution(sim, r, &mut settled_markets, &mut res);
    }
    res.realized = sim.realized();
    res.open_at_end = sim.open_count();
    res
}

fn apply_resolution(
    sim: &mut SimPortfolio,
    r: &Resolution,
    settled_markets: &mut BTreeSet<String>,
    res: &mut SimResult,
) {
    if !settled_markets.insert(r.market_id.clone()) {
        return; // already settled
    }
    let (settled, wins) = sim.settle(&r.market_id, &r.winning_outcome);
    if settled > 0 {
        res.settled_positions += settled;
        res.wins += wins;
        res.losses += settled - wins;
        res.realized_curve.push(sim.realized());
    }
}

/// The per-decision fee the live DR generator baked into `net_edge_after_fees`, read back from the
/// stored attribution (so the counterfactual subtracts the same fee the report did). Prefers
/// `fee_impact.fee_cost_frac` (the correctly-unit'd fraction-of-notional written since the
/// 2026-07-04 fee overhaul); falls back to the historical `est_fees_and_gas` key (which pre-overhaul
/// reports subtracted directly — replaying the same number preserves what the live gate actually
/// saw at the time). 0 when absent (then `gross == net`).
fn report_fee(attribution: &serde_json::Value) -> Decimal {
    let fee_impact = attribution.get("fee_impact");
    let parse = |v: &serde_json::Value| match v {
        serde_json::Value::String(s) => s.parse::<Decimal>().ok(),
        serde_json::Value::Number(n) => n.to_string().parse::<Decimal>().ok(),
        _ => None,
    };
    fee_impact
        .and_then(|f| f.get("fee_cost_frac"))
        .and_then(parse)
        .or_else(|| {
            fee_impact
                .and_then(|f| f.get("est_fees_and_gas"))
                .and_then(parse)
        })
        .unwrap_or(dec!(0))
}

// ---------------------------------------------------------------------------------------------
// CLI runner + DB loaders (the only impure part — everything above is pure & unit-tested).
// ---------------------------------------------------------------------------------------------

/// Value following a `--flag` in the arg list, if present.
fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

/// Parse a `name=val,name=val` weight override into a map.
fn parse_weights(s: &str) -> BTreeMap<String, Decimal> {
    s.split(',')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((k.trim().to_string(), v.trim().parse::<Decimal>().ok()?))
        })
        .collect()
}

fn parse_dec_str(s: Option<String>) -> Option<Decimal> {
    s.and_then(|v| v.parse::<Decimal>().ok())
}

/// Entry point for the `polytrader backtest` subcommand. Read-only: loads journal events and replays
/// them; never writes. Prints the fidelity anchor (settlement formula vs live realized) and a
/// counterfactual run under the chosen config.
pub async fn run(pool: &sqlx::PgPool, args: &[String]) -> anyhow::Result<()> {
    let mut since = flag_value(args, "--since").map(str::to_string);
    // OOM GUARD (2026-07-12, roadmap TODO): this harness runs via `kubectl exec` INSIDE the live
    // trading pod's 512Mi cgroup, and an unbounded report load once OOM-killed the live server
    // (2026-07-10). Without an explicit `--since`, bound the replay at the latest paper reset —
    // that's also the only window whose results are comparable to the live portfolio. A truly
    // unbounded run must be requested explicitly with `--full-history`.
    if since.is_none() && !args.iter().any(|a| a == "--full-history") {
        let boundary: Option<Option<String>> = sqlx::query_scalar(
            "SELECT max(as_of)::text FROM paper_trading.virtual_portfolio_snapshots
             WHERE snapshot_reason = 'manual_paper_reset'",
        )
        .fetch_optional(pool)
        .await?;
        since = boundary.flatten();
        match &since {
            Some(b) => println!(
                "note: no --since given; defaulting to the last paper reset ({b}). Pass --full-history for an unbounded replay (memory-heavy; runs inside the live pod)."
            ),
            None => println!("note: no --since and no paper reset found; replaying full history."),
        }
    }
    let min_net_edge = flag_value(args, "--min-net-edge").and_then(|s| s.parse::<Decimal>().ok());
    let rt_multiplier = flag_value(args, "--rt-multiplier").and_then(|s| s.parse::<Decimal>().ok());
    let weights_override = flag_value(args, "--weights").map(parse_weights);

    // The anchor validates accounting against the live CUMULATIVE realized, so it always loads the full
    // settlement history — `--since` only bounds the counterfactual's report replay, never the anchor.
    let settlements = load_settlements(pool).await?;
    let exit_realized = load_exit_realized(pool).await?;
    let manual_sell_realized = load_manual_sell_realized(pool).await?;
    let reports = load_reports(pool, since.as_deref()).await?;
    let resolutions = load_resolutions(pool).await?;
    let slug_of = load_slug_map(pool).await?;
    let live_realized = load_live_realized(pool).await?;
    let weights = match weights_override {
        Some(w) => w,
        None => load_latest_weights(pool).await?,
    };
    let mut risk = RiskConfig::from_env();
    if let Some(m) = min_net_edge {
        risk.min_net_edge = m;
    }
    if let Some(k) = rt_multiplier {
        risk.round_trip_cost_multiplier = k;
    }

    // --- Fidelity anchor: the production settlement formula recomputed over every live settlement,
    // PLUS every autonomous exit's realized delta (see load_exit_realized doc — exits have been the
    // dominant realization path since 2026-07-04, and the anchor read a false MISMATCH without them),
    // PLUS manual/operator sells (see load_manual_sell_realized — the 2026-07-05 T1-esports cleanup).
    let (settlement_realized, settled, wins, losses, all_match) =
        realized_from_settlements(&settlements);
    let anchor_realized = settlement_realized + exit_realized + manual_sell_realized;
    // 1-cent tolerance: historical `autonomous_paper_exit` events journaled realized_gross rounded
    // to 4dp (fixed 2026-07-12, but the written events are immutable), so exact equality is
    // unattainable by construction — the dust is ±$0.0003 today. Real drift classes (uncounted
    // exits, fee-semantics mismatch, missed manual sells) were $10–$90; a cent cleanly separates.
    let residual = anchor_realized - live_realized;
    let realized_matches = residual.abs() < dec!(0.01);
    println!("== Fidelity anchor (settlement formula vs live) ==");
    println!("  settlements: {settled}   W/L: {wins}/{losses}");
    println!(
        "  realized recomputed: {anchor_realized}  (settlements {settlement_realized} + exits {exit_realized} + manual sells {manual_sell_realized})"
    );
    println!("  realized live-recorded: {live_realized}");
    println!(
        "  residual: {} (PASS tolerance ±0.01 — historical exit events carry 4dp-rounded values)",
        residual.round_dp(6)
    );
    println!("  per-record formula match: {all_match}");
    println!(
        "  ANCHOR: {}",
        if all_match && realized_matches {
            "PASS"
        } else {
            "MISMATCH — investigate before trusting counterfactuals"
        }
    );

    // Marks for valuing positions left open at the end (where config differences on unresolved markets
    // actually show up). Built once from the report stream, shared across all configs.
    let marks = build_marks(&reports);

    // `backtest sweep` → run the config grids; otherwise a single counterfactual under the chosen config.
    if args.iter().any(|a| a == "sweep") {
        run_sweep_report(&reports, &resolutions, &slug_of, &marks, &weights, &risk);
        return Ok(());
    }

    // --- Single counterfactual under the chosen config ---
    let cfg = CounterfactualConfig { weights, risk };
    let res = simulate_counterfactual(&reports, &resolutions, &slug_of, &marks, &cfg);
    println!();
    println!(
        "== Counterfactual (min_net_edge={}, {} weights, {} reports) ==",
        cfg.risk.min_net_edge,
        cfg.weights.len(),
        reports.len()
    );
    println!(
        "  fills: {}   settled: {}   W/L: {}/{}   open_at_end: {}",
        res.fills, res.settled_positions, res.wins, res.losses, res.open_at_end
    );
    println!(
        "  [fill@mid]  realized: {}   unrealized: {}   total_pnl: {}",
        res.realized,
        res.unrealized.round_dp(2),
        res.total_pnl().round_dp(2),
    );

    // --- Phase 3: re-price those same fills against the real book + taker fees ---
    // Load the ask side nearest (≤) each fill's time, only for the markets actually entered (~50).
    let mut books: Vec<Option<Vec<Level>>> = Vec::with_capacity(res.fill_log.len());
    for d in &res.fill_log {
        books.push(load_book_at(pool, &d.market_id, &d.outcome, d.at).await);
    }
    let with_book = books.iter().filter(|b| b.is_some()).count();
    // Per-market Polymarket taker rates (stored Gamma rate, else category default) — mirrors the
    // live engine's per-fill fee exactly (geopolitics free, crypto 0.07, …).
    let fee_rates = load_fee_rates(pool).await.unwrap_or_default();
    let real = reprice_realistic(&res.fill_log, &books, &resolutions, &marks, &fee_rates);
    println!(
        "  [book-walk] realized: {}   unrealized: {}   fees: {}   total_pnl: {}   ({} of {} fills had a book snapshot)",
        real.realized.round_dp(2),
        real.unrealized.round_dp(2),
        real.fees.round_dp(2),
        real.total_pnl().round_dp(2),
        with_book,
        res.fill_log.len(),
    );
    println!(
        "  NOTE: book-walk re-prices the at-mid fill DECISIONS at the real ask + Polymarket's real"
    );
    println!("        per-market taker fee (shares × rate × p × (1−p); geopolitics fee-free)");
    println!(
        "        (a credible backtest of THIS strategy). It is still a different strategy than the"
    );
    println!("        live line ran (no arb/both-sides legs, fixed weights) — the anchor is the live check.");
    Ok(())
}

/// Render the Phase-2 config sweeps: gate-threshold (the strict-vs-lenient question) and a few weight
/// presets. Ranked by realized P&L; max-drawdown shown alongside. Absolute P&L is not live-comparable
/// (Phase-1 approximations), but RELATIVE ranking across configs is meaningful — they share the same
/// approximations, so the differences are real.
fn run_sweep_report(
    reports: &[ReportRow],
    resolutions: &[Resolution],
    slug_of: &BTreeMap<String, String>,
    marks: &Marks,
    base_weights: &BTreeMap<String, Decimal>,
    base_risk: &RiskConfig,
) {
    println!();
    println!("== Sweep A — gate threshold (weights fixed = loaded/baseline) ==");
    let gate_grid: Vec<Decimal> = vec![
        dec!(0.02),
        dec!(0.025),
        dec!(0.03),
        dec!(0.04),
        dec!(0.05),
        dec!(0.06),
    ];
    let configs_a: Vec<(String, CounterfactualConfig)> = gate_grid
        .iter()
        .map(|edge| {
            let mut risk = base_risk.clone();
            risk.min_net_edge = *edge;
            (
                format!("edge={edge}"),
                CounterfactualConfig {
                    weights: base_weights.clone(),
                    risk,
                },
            )
        })
        .collect();
    print_sweep_table(run_sweep(reports, resolutions, slug_of, marks, &configs_a));

    println!();
    println!(
        "== Sweep B — weight presets (min_net_edge fixed = {}) ==",
        base_risk.min_net_edge
    );
    let presets: Vec<(&str, Vec<(&str, Decimal)>)> = vec![
        ("neutral", vec![]),
        ("momentum+", vec![("orderbook_momentum", dec!(1.5))]),
        ("news+", vec![("news_sentiment", dec!(1.5))]),
        (
            "external+",
            vec![("yahoo_finance", dec!(1.5)), ("news_sentiment", dec!(1.5))],
        ),
        ("momentum-off", vec![("orderbook_momentum", dec!(0.25))]),
    ];
    let mut configs_b: Vec<(String, CounterfactualConfig)> = vec![(
        "hermes-latest".to_string(),
        CounterfactualConfig {
            weights: base_weights.clone(),
            risk: base_risk.clone(),
        },
    )];
    for (label, kvs) in presets {
        let weights: BTreeMap<String, Decimal> =
            kvs.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        configs_b.push((
            label.to_string(),
            CounterfactualConfig {
                weights,
                risk: base_risk.clone(),
            },
        ));
    }
    print_sweep_table(run_sweep(reports, resolutions, slug_of, marks, &configs_b));

    println!();
    println!("  Ranked by total_pnl (realized + open marks). Absolute P&L is NOT live-comparable");
    println!(
        "  (fill-at-mid, no fees/arb); only the RELATIVE ranking is meaningful. `realized` covers"
    );
    println!(
        "  the ~14 markets resolved in-window (entered under every config, so it barely moves);"
    );
    println!("  config differences live in `unreal` — the marked value of still-open positions.");
}

fn print_sweep_table(mut rows: Vec<SweepRow>) {
    rows.sort_by_key(|r| std::cmp::Reverse(r.result.total_pnl())); // realized + marks, descending
    println!(
        "  {:<16} {:>9} {:>8} {:>9} {:>7} {:>6} {:>6} {:>9}",
        "config", "total_pnl", "realized", "unreal", "settled", "fills", "open", "max_dd"
    );
    for r in &rows {
        let s = &r.result;
        println!(
            "  {:<16} {:>9} {:>8} {:>9} {:>7} {:>6} {:>6} {:>9}",
            r.label,
            s.total_pnl().round_dp(2),
            s.realized.round_dp(2),
            s.unrealized.round_dp(2),
            s.settled_positions,
            s.fills,
            s.open_at_end,
            r.max_drawdown.round_dp(2),
        );
    }
}

/// Raw row for the settlement query: (won, shares, cost_basis, realized_pnl) as JSON text.
type SettlementQueryRow = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);
/// Raw row for the decision_report query: (created_at, market_id, target_outcome, target_mid, attribution).
type ReportQueryRow = (
    DateTime<Utc>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<serde_json::Value>,
);

async fn load_settlements(pool: &sqlx::PgPool) -> anyhow::Result<Vec<SettlementRow>> {
    // RESET-BOUNDARY: the fidelity anchor must reproduce the LIVE portfolio realized, which a
    // `POST /paper/reset` zeroes (writing a `manual_paper_reset` snapshot) while PRESERVING the journal.
    // So only settlements at/after the latest reset count — otherwise pre-reset events (incl. the
    // 2026-06-24 re-settlement phantoms, +$5.41) are summed against a post-reset live realized of 0 and
    // the anchor reads a false MISMATCH. Mirrors the /trades settlements panel filter.
    let rows: Vec<SettlementQueryRow> = sqlx::query_as(
        "SELECT payload->>'won', payload->>'shares', payload->>'cost_basis', payload->>'realized_pnl'
         FROM journal.events
         WHERE event_type = 'paper_position_settled'
           AND created_at >= COALESCE(
             (SELECT max(as_of) FROM paper_trading.virtual_portfolio_snapshots
              WHERE snapshot_reason = 'manual_paper_reset'), '-infinity'::timestamptz)
         ORDER BY created_at ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(won, shares, cost, pnl)| {
            Some(SettlementRow {
                won: won?.eq_ignore_ascii_case("true"),
                shares: parse_dec_str(shares)?,
                cost_basis: parse_dec_str(cost)?,
                recorded_pnl: parse_dec_str(pnl)?,
            })
        })
        .collect())
}

/// Net realized P&L from autonomous exits (take-profit/stop-loss/time-stop/signal-flip) since the
/// last reset. Found 2026-07-10: the anchor summed ONLY `paper_position_settled` and compared
/// against `live_realized`, which also includes every exit's realized delta — since exits shipped
/// 2026-07-04 they're the DOMINANT realization path, so the anchor read a false MISMATCH (51
/// settlements recomputed to +$42.34 vs a live-recorded −$48.10; the ~$90 gap is exactly the
/// accumulated exit P&L the anchor never knew existed). Same fix shape as the 2026-07-08 Hermes
/// attribution bug, applied here to the OTHER consumer that summed settlements alone.
async fn load_exit_realized(pool: &sqlx::PgPool) -> anyhow::Result<Decimal> {
    // GROSS, deliberately (2026-07-12): the engine adds each sell's (price − avg_entry) × size to
    // the snapshot's `realized_pnl` WITHOUT fees — fees enter the cash identity separately via
    // `total_fees_agg` (see paper/engine.rs). The old `- fees` here compared apples to oranges and
    // under-read the anchor by the cumulative exit fees (+$11.03 of the "residual ~$10 gap", which
    // was mis-attributed to manual sells; the real manual-sell contribution was +$0.95).
    let r: Option<Decimal> = sqlx::query_scalar(
        "SELECT SUM((payload->>'realized_gross')::numeric)
         FROM journal.events
         WHERE event_type = 'autonomous_paper_exit'
           AND created_at >= COALESCE(
             (SELECT max(as_of) FROM paper_trading.virtual_portfolio_snapshots
              WHERE snapshot_reason = 'manual_paper_reset'), '-infinity'::timestamptz)",
    )
    .fetch_optional(pool)
    .await?;
    Ok(r.unwrap_or(dec!(0)))
}

/// Realized P&L from NON-autonomous sells (operator `POST /paper/orders`, e.g. the 2026-07-05
/// T1-esports cleanup). These go through the same engine path as an exit but journal no
/// `autonomous_paper_exit` event, so the anchor missed them. There is no per-sell realized record,
/// but every fill tx writes a `post_fill_tx` snapshot whose `realized_pnl` is the prior snapshot's
/// value plus exactly this sell's realized delta — so the snapshot DIFF recovers it. The order row,
/// its fills, and the snapshot are stamped milliseconds apart (different clock sources), hence the
/// 5-second window join on the FIRST post-fill snapshot at/after the order, not timestamp equality.
/// Buys never change `realized_pnl`, so only sells are summed. Sells are identified by
/// `decision_context->>'source' != 'autonomous_exit'` — the tag every executor path writes.
async fn load_manual_sell_realized(pool: &sqlx::PgPool) -> anyhow::Result<Decimal> {
    let r: Option<Decimal> = sqlx::query_scalar(
        "SELECT SUM(s_now.realized_pnl - s_prev.realized_pnl)
         FROM paper_trading.paper_orders o
         JOIN LATERAL (
           SELECT realized_pnl, as_of FROM paper_trading.virtual_portfolio_snapshots
           WHERE snapshot_reason = 'post_fill_tx'
             AND as_of >= o.created_at AND as_of < o.created_at + interval '5 seconds'
           ORDER BY as_of ASC LIMIT 1
         ) s_now ON true
         JOIN LATERAL (
           SELECT realized_pnl FROM paper_trading.virtual_portfolio_snapshots
           WHERE as_of < s_now.as_of ORDER BY as_of DESC LIMIT 1
         ) s_prev ON true
         WHERE lower(o.side) = 'sell'
           AND COALESCE(o.decision_context->>'source', '') <> 'autonomous_exit'
           AND o.created_at >= COALESCE(
             (SELECT max(as_of) FROM paper_trading.virtual_portfolio_snapshots
              WHERE snapshot_reason = 'manual_paper_reset'), '-infinity'::timestamptz)",
    )
    .fetch_optional(pool)
    .await?;
    Ok(r.unwrap_or(dec!(0)))
}

async fn load_reports(pool: &sqlx::PgPool, since: Option<&str>) -> anyhow::Result<Vec<ReportRow>> {
    // MEMORY: the full `report.attribution` carries per-signal metadata (news headlines, etc.) — several
    // KB per report × tens of thousands of reports would OOM the 512Mi pod (the exec'd backtest shares
    // the live server's cgroup). Project to ONLY what `fuse_from_attribution` reads — each signal's
    // score/confidence + the fee — server-side, so the DB returns ~200 bytes/row instead of the blob.
    let rows: Vec<ReportQueryRow> = sqlx::query_as(
        "SELECT created_at, market_id, target_outcome, target_mid,
                jsonb_build_object(
                    'orderbook_momentum', jsonb_build_object('score', a->'orderbook_momentum'->'score', 'confidence', a->'orderbook_momentum'->'confidence'),
                    'spike_divergence',   jsonb_build_object('score', a->'spike_divergence'->'score',   'confidence', a->'spike_divergence'->'confidence'),
                    'theta_convergence',  jsonb_build_object('score', a->'theta_convergence'->'score',  'confidence', a->'theta_convergence'->'confidence'),
                    'overreaction_fade',  jsonb_build_object('score', a->'overreaction_fade'->'score',  'confidence', a->'overreaction_fade'->'confidence'),
                    'yahoo_finance',      jsonb_build_object('score', a->'yahoo_finance'->'score',      'confidence', a->'yahoo_finance'->'confidence'),
                    'news_sentiment',     jsonb_build_object('score', a->'news_sentiment'->'score',     'confidence', a->'news_sentiment'->'confidence'),
                    'fee_impact',         jsonb_build_object('est_fees_and_gas', a->'fee_impact'->'est_fees_and_gas')
                ) AS slim
         FROM (
            SELECT created_at,
                   payload->>'market_id'      AS market_id,
                   payload->>'target_outcome' AS target_outcome,
                   payload->>'target_mid'     AS target_mid,
                   payload->'report'->'attribution' AS a
            FROM journal.events
            WHERE event_type = 'decision_report'
              AND ($1::timestamptz IS NULL OR created_at >= $1::timestamptz)
         ) s
         ORDER BY created_at ASC",
    )
    .bind(since)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(at, market_id, outcome, mid, attr)| {
            Some(ReportRow {
                at,
                market_id: market_id?,
                target_outcome: outcome?,
                target_mid: parse_dec_str(mid)?,
                attribution: attr.unwrap_or_else(|| serde_json::json!({})),
            })
        })
        .collect())
}

async fn load_resolutions(pool: &sqlx::PgPool) -> anyhow::Result<Vec<Resolution>> {
    // Resolution time proxy = the first time we settled a position in that market (best signal we have
    // of when it actually resolved). NULL for markets that resolved but we never held ⇒ settle at end.
    let rows: Vec<(String, String, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT m.gamma_id, m.resolved_outcome,
                (SELECT min(e.created_at) FROM journal.events e
                  WHERE e.event_type = 'paper_position_settled'
                    AND e.payload->>'market_id' = m.gamma_id)
         FROM market_data.markets m
         WHERE m.resolved_outcome IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(market_id, winning_outcome, at)| Resolution {
            market_id,
            winning_outcome,
            at,
        })
        .collect())
}

async fn load_slug_map(pool: &sqlx::PgPool) -> anyhow::Result<BTreeMap<String, String>> {
    let rows: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT gamma_id, slug FROM market_data.markets")
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(g, s)| Some((g, s?)))
        .collect())
}

/// Per-market Polymarket taker fee RATE map for realistic-fill repricing: the stored Gamma
/// `taker_fee_rate` when synced, else the category default from the slug — the same resolution
/// order the live paper engine uses per fill.
async fn load_fee_rates(pool: &sqlx::PgPool) -> anyhow::Result<BTreeMap<String, Decimal>> {
    let rows: Vec<(String, Option<String>, Option<Decimal>)> =
        sqlx::query_as("SELECT gamma_id, slug, taker_fee_rate FROM market_data.markets")
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|(g, slug, rate)| {
            let r = rate
                .unwrap_or_else(|| crate::polymarket_taker_fee_rate(slug.as_deref().unwrap_or("")));
            (g, r)
        })
        .collect())
}

/// Parse a jsonb book side (`[{"price":"0.45","size":"123"}, ...]`) into levels.
fn parse_levels(v: &serde_json::Value) -> Vec<Level> {
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|l| {
                    let price = l.get("price")?.as_str()?.parse::<Decimal>().ok()?;
                    let size = l.get("size")?.as_str()?.parse::<Decimal>().ok()?;
                    Some(Level { price, size })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The ask side of the orderbook snapshot nearest (≤) `at` for a (market, outcome) — what a buy at that
/// moment would have walked. Loaded lazily, only for the markets the counterfactual actually filled.
async fn load_book_at(
    pool: &sqlx::PgPool,
    market_id: &str,
    outcome: &str,
    at: DateTime<Utc>,
) -> Option<Vec<Level>> {
    let asks: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT asks FROM market_data.orderbook_snapshots
         WHERE market_id = $1 AND outcome = $2 AND fetched_at <= $3
         ORDER BY fetched_at DESC LIMIT 1",
    )
    .bind(market_id)
    .bind(outcome)
    .bind(at)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    asks.map(|v| parse_levels(&v))
}

async fn load_live_realized(pool: &sqlx::PgPool) -> anyhow::Result<Decimal> {
    let r: Option<Decimal> = sqlx::query_scalar(
        "SELECT realized_pnl FROM paper_trading.virtual_portfolio_snapshots
         ORDER BY as_of DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(r.unwrap_or(dec!(0)))
}

async fn load_latest_weights(pool: &sqlx::PgPool) -> anyhow::Result<BTreeMap<String, Decimal>> {
    let payload: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT payload->'weights' FROM journal.events
         WHERE event_type = 'strategy_weights' ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?
    .flatten();
    let mut out = BTreeMap::new();
    if let Some(serde_json::Value::Object(map)) = payload {
        for (k, v) in map {
            let d = match v {
                serde_json::Value::String(s) => s.parse::<Decimal>().ok(),
                serde_json::Value::Number(n) => n.to_string().parse::<Decimal>().ok(),
                _ => None,
            };
            if let Some(d) = d {
                out.insert(k, d);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ts(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).unwrap()
    }

    #[test]
    fn settle_pays_winners_and_zeroes_losers() {
        let mut sim = SimPortfolio::new();
        // Bought 10 shares of Yes for $4 (entry 0.40); 10 shares of No for $6.
        sim.fill("m1", "Yes", dec!(10), dec!(4));
        sim.fill("m1", "No", dec!(10), dec!(6));
        assert_eq!(sim.total_locked(), dec!(10));
        let (settled, wins) = sim.settle("m1", "Yes");
        assert_eq!((settled, wins), (2, 1));
        // Yes won: payout 10 − cost 4 = +6 ; No lost: 0 − 6 = −6 ; net 0.
        assert_eq!(sim.realized(), dec!(0));
        assert_eq!(sim.open_count(), 0);
    }

    #[test]
    fn realized_from_settlements_matches_recorded() {
        // Mirrors the live anchor shape: winners pay shares−cost, losers pay −cost.
        let rows = vec![
            SettlementRow {
                won: true,
                shares: dec!(15),
                cost_basis: dec!(6.90),
                recorded_pnl: dec!(8.10),
            },
            SettlementRow {
                won: false,
                shares: dec!(15),
                cost_basis: dec!(7.35),
                recorded_pnl: dec!(-7.35),
            },
        ];
        let (realized, settled, wins, losses, all_match) = realized_from_settlements(&rows);
        assert!(
            all_match,
            "per-record P&L must match the production formula"
        );
        assert_eq!((settled, wins, losses), (2, 1, 1));
        assert_eq!(realized, dec!(0.75));
    }

    #[test]
    fn realized_from_settlements_flags_mismatch() {
        // A recorded_pnl that the formula disagrees with must be flagged (guards against silent drift).
        let rows = vec![SettlementRow {
            won: true,
            shares: dec!(10),
            cost_basis: dec!(4),
            recorded_pnl: dec!(99), // wrong; formula says +6
        }];
        let (_r, _s, _w, _l, all_match) = realized_from_settlements(&rows);
        assert!(!all_match);
    }

    #[test]
    fn replay_fills_reconstructs_realized_and_winloss() {
        let fills = vec![
            FillEvent {
                at: ts(1),
                market_id: "m1".into(),
                outcome: "Yes".into(),
                shares: dec!(10),
                cost: dec!(4),
            },
            FillEvent {
                at: ts(2),
                market_id: "m2".into(),
                outcome: "No".into(),
                shares: dec!(20),
                cost: dec!(5),
            },
        ];
        let resolutions = vec![
            Resolution {
                market_id: "m1".into(),
                winning_outcome: "Yes".into(),
                at: Some(ts(10)),
            },
            Resolution {
                market_id: "m2".into(),
                winning_outcome: "Yes".into(),
                at: Some(ts(11)),
            }, // No loses
        ];
        let res = replay_fills(fills, &resolutions);
        // m1: +6 (10−4). m2: −5 (lost). realized +1.
        assert_eq!(res.realized, dec!(1));
        assert_eq!((res.settled_positions, res.wins, res.losses), (2, 1, 1));
        assert_eq!(res.fills, 2);
        assert_eq!(res.open_at_end, 0);
    }

    #[test]
    fn counterfactual_respects_dedup_and_min_edge_gate() {
        // Two reports on the same market; two agreeing signals with Σ(conf·weight) = 1.0 so the
        // fused gross is exactly 0.30 (the max(Σw,1) denominator floor makes a LONE 0.5-weight
        // signal fuse to half its score — see fuse_weighted_lone_weak_signal_cannot_saturate).
        // Only ONE position should open (dedup), and a higher min_net_edge suppresses it entirely.
        let attr = json!({
            "orderbook_momentum": {"score": "0.30", "confidence": "0.5"},
            "theta_convergence": {"score": "0.30", "confidence": "0.5"},
            "fee_impact": {"est_fees_and_gas": "0.05"},
        });
        let reports = vec![
            ReportRow {
                at: ts(1),
                market_id: "m1".into(),
                target_outcome: "Yes".into(),
                target_mid: dec!(0.50),
                attribution: attr.clone(),
            },
            ReportRow {
                at: ts(2),
                market_id: "m1".into(),
                target_outcome: "Yes".into(),
                target_mid: dec!(0.50),
                attribution: attr.clone(),
            },
        ];
        let resolutions = vec![Resolution {
            market_id: "m1".into(),
            winning_outcome: "Yes".into(),
            at: None,
        }];
        let slug_of: BTreeMap<String, String> = BTreeMap::new();

        // gross = 0.30*0.5 / 0.5 = 0.30 ; net = 0.30 − 0.05 = 0.25 ⇒ clears a 2% gate.
        let mut cfg = CounterfactualConfig {
            weights: BTreeMap::new(),
            risk: RiskConfig::default(),
        };
        let res = simulate_counterfactual(&reports, &resolutions, &slug_of, &Marks::new(), &cfg);
        assert_eq!(res.fills, 1, "dedup: only one position per market");
        assert_eq!(res.settled_positions, 1);
        assert!(res.wins == 1 && res.realized > dec!(0));

        // Raise the gate above the net edge ⇒ no trade.
        cfg.risk.min_net_edge = dec!(0.99);
        let res2 = simulate_counterfactual(&reports, &resolutions, &slug_of, &Marks::new(), &cfg);
        assert_eq!(res2.fills, 0);
        assert_eq!(res2.realized, dec!(0));
    }

    #[test]
    fn counterfactual_weights_change_outcome() {
        // A signal whose sign flips the bet: with the default weight it fires positive; zeroing its
        // weight removes its contribution. Confirms the candidate weight vector actually bites.
        // Scores sized so the lone surviving signal (w 0.5, halved by the max(Σw,1) denominator
        // floor) still clears the friction floor at price 0.50 (10.71%): 0.60·0.5 − 0.05 = 0.25.
        let attr = json!({
            "orderbook_momentum": {"score": "0.60", "confidence": "0.5"},
            "news_sentiment": {"score": "-0.60", "confidence": "0.5"},
            "fee_impact": {"est_fees_and_gas": "0.05"},
        });
        let reports = vec![ReportRow {
            at: ts(1),
            market_id: "m1".into(),
            target_outcome: "Yes".into(),
            target_mid: dec!(0.50),
            attribution: attr,
        }];
        let resolutions = vec![Resolution {
            market_id: "m1".into(),
            winning_outcome: "Yes".into(),
            at: None,
        }];
        let slug_of: BTreeMap<String, String> = BTreeMap::new();

        // Equal-and-opposite signals ⇒ gross 0 ⇒ net negative after fee ⇒ no trade.
        let cfg_balanced = CounterfactualConfig {
            weights: BTreeMap::new(),
            risk: RiskConfig::default(),
        };
        assert_eq!(
            simulate_counterfactual(
                &reports,
                &resolutions,
                &slug_of,
                &Marks::new(),
                &cfg_balanced
            )
            .fills,
            0
        );

        // Zero the news weight ⇒ only the positive momentum remains ⇒ trade fires.
        let mut weights = BTreeMap::new();
        weights.insert("news_sentiment".to_string(), dec!(0.0));
        let cfg_tilted = CounterfactualConfig {
            weights,
            risk: RiskConfig::default(),
        };
        assert_eq!(
            simulate_counterfactual(&reports, &resolutions, &slug_of, &Marks::new(), &cfg_tilted)
                .fills,
            1
        );
    }

    #[test]
    fn unrealized_marks_open_positions_with_binary_complement() {
        let mut sim = SimPortfolio::new();
        // Bought 10 Yes for $4 (entry 0.40). Latest report targets No @ 0.55 ⇒ Yes mark = 1−0.55 = 0.45.
        sim.fill("m1", "Yes", dec!(10), dec!(4));
        let mut marks: Marks = BTreeMap::new();
        marks.insert("m1".into(), ("No".into(), dec!(0.55)));
        // unrealized = 10·0.45 − 4 = 0.50
        assert_eq!(sim.unrealized(&marks), dec!(0.50));
        // No mark for the market ⇒ held at cost ⇒ 0.
        assert_eq!(sim.unrealized(&Marks::new()), dec!(0));
    }

    #[test]
    fn build_marks_keeps_latest_report_per_market() {
        let reports = vec![
            ReportRow {
                at: ts(1),
                market_id: "m1".into(),
                target_outcome: "Yes".into(),
                target_mid: dec!(0.30),
                attribution: json!({}),
            },
            ReportRow {
                at: ts(5),
                market_id: "m1".into(),
                target_outcome: "No".into(),
                target_mid: dec!(0.60),
                attribution: json!({}),
            },
        ];
        let marks = build_marks(&reports);
        assert_eq!(marks.get("m1"), Some(&("No".to_string(), dec!(0.60))));
    }

    fn lvl(price: &str, size: &str) -> Level {
        Level {
            price: price.parse().unwrap(),
            size: size.parse().unwrap(),
        }
    }

    #[test]
    fn walk_asks_fills_best_first_under_limit_with_fee() {
        // Asks deliberately out of order; best (cheapest) must fill first, stop above the limit.
        let asks = vec![lvl("0.52", "100"), lvl("0.50", "10"), lvl("0.51", "10")];
        // limit 0.55, want 25 shares: take 10@0.50 + 10@0.51 + 5@0.52 = 25 shares.
        // Fee = Polymarket real model per level at rate 0.04:
        //   10×.04×.50×.50 + 10×.04×.51×.49 + 5×.04×.52×.48 = 0.1 + 0.09996 + 0.04992 = 0.24988
        let (filled, cost, fee) = walk_asks_limit_buy(&asks, dec!(0.55), dec!(25), dec!(0.04));
        assert_eq!(filled, dec!(25));
        assert_eq!(cost, dec!(12.70));
        assert_eq!(fee, dec!(0.24988));
    }

    #[test]
    fn walk_asks_fee_free_geopolitics_rate_charges_nothing() {
        let asks = vec![lvl("0.50", "100")];
        let (filled, cost, fee) = walk_asks_limit_buy(&asks, dec!(0.55), dec!(10), dec!(0));
        assert_eq!((filled, cost, fee), (dec!(10), dec!(5.00), dec!(0)));
    }

    #[test]
    fn walk_asks_no_fill_when_book_above_limit() {
        let asks = vec![lvl("0.80", "100")];
        let (filled, cost, fee) = walk_asks_limit_buy(&asks, dec!(0.60), dec!(10), dec!(0.04));
        assert_eq!((filled, cost, fee), (dec!(0), dec!(0), dec!(0)));
    }

    #[test]
    fn reprice_realistic_uses_book_price_not_mid() {
        // One fill decision: $10 approved on a mid-0.50 market. Limit = 0.50*1.2 = 0.60, target shares
        // = 10/0.60 ≈ 16.67. Book has plenty at 0.55, so it fills 16.67 @ 0.55.
        let fill_log = vec![FillDecision {
            at: ts(1),
            market_id: "m1".into(),
            outcome: "Yes".into(),
            approved_usdc: dec!(10),
            target_mid: dec!(0.50),
        }];
        let books = vec![Some(vec![lvl("0.55", "1000")])];
        let resolutions = vec![Resolution {
            market_id: "m1".into(),
            winning_outcome: "Yes".into(),
            at: Some(ts(10)),
        }];
        let rates: BTreeMap<String, Decimal> = [("m1".to_string(), dec!(0.04))].into();
        let res = reprice_realistic(&fill_log, &books, &resolutions, &Marks::new(), &rates);
        // shares = 16.67 ; cost = 16.67*0.55 = 9.1685 ; won ⇒ payout 16.67, realized = (16.67 − 9.1685)
        // rounded to 2dp by the production settlement formula = 7.50.
        assert_eq!(res.fills, 1);
        assert_eq!(res.settled_positions, 1);
        assert_eq!(res.wins, 1);
        assert_eq!(res.realized, dec!(7.50));
        // fee = 16.67 × 0.04 × 0.55 × 0.45 (real model), rounded to 6dp.
        assert_eq!(
            res.fees,
            (dec!(16.67) * dec!(0.04) * dec!(0.55) * dec!(0.45)).round_dp(6)
        );
    }

    #[test]
    fn reprice_realistic_skips_missing_books() {
        let fill_log = vec![FillDecision {
            at: ts(1),
            market_id: "m1".into(),
            outcome: "Yes".into(),
            approved_usdc: dec!(10),
            target_mid: dec!(0.50),
        }];
        let books = vec![None]; // no snapshot for this fill
        let res = reprice_realistic(&fill_log, &books, &[], &Marks::new(), &BTreeMap::new());
        assert_eq!(res.fills, 0);
        assert_eq!(res.realized, dec!(0));
    }

    #[test]
    fn max_drawdown_tracks_peak_to_trough() {
        // path: +5, +2 (dd 3 from peak 5), +8 (new peak), +1 (dd 7) → max dd 7.
        assert_eq!(max_drawdown(&[dec!(5), dec!(2), dec!(8), dec!(1)]), dec!(7));
        // monotonic up ⇒ no drawdown.
        assert_eq!(max_drawdown(&[dec!(1), dec!(2), dec!(3)]), dec!(0));
        // all losses from the implicit 0 peak ⇒ depth = worst point.
        assert_eq!(max_drawdown(&[dec!(-4), dec!(-9), dec!(-6)]), dec!(9));
        assert_eq!(max_drawdown(&[]), dec!(0));
    }

    #[test]
    fn run_sweep_ranks_gate_thresholds() {
        // One market, strong +0.25 net edge. A loose gate (0.02) trades it; a gate above 0.25 doesn't.
        // Two agreeing signals (Σw = 1.0) keep the fused gross at 0.30 under the max(Σw,1)
        // denominator floor, so the loose 2% gate fills and the tight 50% gate doesn't.
        let attr = json!({
            "orderbook_momentum": {"score": "0.30", "confidence": "0.5"},
            "theta_convergence": {"score": "0.30", "confidence": "0.5"},
            "fee_impact": {"est_fees_and_gas": "0.05"},
        });
        let reports = vec![ReportRow {
            at: ts(1),
            market_id: "m1".into(),
            target_outcome: "Yes".into(),
            target_mid: dec!(0.50),
            attribution: attr,
        }];
        let resolutions = vec![Resolution {
            market_id: "m1".into(),
            winning_outcome: "Yes".into(),
            at: None,
        }];
        let slug_of: BTreeMap<String, String> = BTreeMap::new();

        let mk = |edge: Decimal| CounterfactualConfig {
            weights: BTreeMap::new(),
            risk: RiskConfig {
                min_net_edge: edge,
                ..RiskConfig::default()
            },
        };
        let configs = vec![
            ("loose".to_string(), mk(dec!(0.02))),
            ("tight".to_string(), mk(dec!(0.50))),
        ];
        let rows = run_sweep(&reports, &resolutions, &slug_of, &Marks::new(), &configs);
        let loose = rows.iter().find(|r| r.label == "loose").unwrap();
        let tight = rows.iter().find(|r| r.label == "tight").unwrap();
        assert_eq!(loose.result.fills, 1);
        assert_eq!(tight.result.fills, 0);
        assert!(loose.result.realized > tight.result.realized);
    }
}
