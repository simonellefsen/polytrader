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

/// Outcome of a simulation run.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SimResult {
    pub realized: Decimal,
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
}

impl SimResult {
    /// Final equity under the sim identity (cash + locked == base + realized; open marks excluded).
    pub fn final_equity(&self) -> Decimal {
        SIM_BASE_USDC + self.realized
    }
}

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
    mut reports: Vec<ReportRow>,
    resolutions: &[Resolution],
    slug_of: &BTreeMap<String, String>,
    cfg: &CounterfactualConfig,
) -> SimResult {
    reports.sort_by_key(|r| r.at);
    let rm = RiskManager::new(cfg.risk.clone());

    // Resolutions with a known time, sorted; the rest are applied at the end.
    let mut timed: Vec<&Resolution> = resolutions.iter().filter(|r| r.at.is_some()).collect();
    timed.sort_by_key(|r| r.at.unwrap());

    let mut sim = SimPortfolio::new();
    let mut settled_markets: BTreeSet<String> = BTreeSet::new();
    let mut res = SimResult::default();
    let mut next_res = 0usize;

    for rep in &reports {
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
        let check = rm.gate(&rep.market_id, net, sizing.recommended_usdc, &exp);
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
    res.open_at_end = sim.open_count();
    res
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
/// stored `attribution.fee_impact.est_fees_and_gas` (so the counterfactual subtracts the same fee the
/// report did). Falls back to 0 when absent (then `gross == net`).
fn report_fee(attribution: &serde_json::Value) -> Decimal {
    attribution
        .get("fee_impact")
        .and_then(|f| f.get("est_fees_and_gas"))
        .and_then(|v| match v {
            serde_json::Value::String(s) => s.parse::<Decimal>().ok(),
            serde_json::Value::Number(n) => n.to_string().parse::<Decimal>().ok(),
            _ => None,
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
    let since = flag_value(args, "--since").map(str::to_string);
    let min_net_edge = flag_value(args, "--min-net-edge").and_then(|s| s.parse::<Decimal>().ok());
    let weights_override = flag_value(args, "--weights").map(parse_weights);

    let settlements = load_settlements(pool, since.as_deref()).await?;
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

    // --- Fidelity anchor: the production settlement formula recomputed over every live settlement ---
    let (anchor_realized, settled, wins, losses, all_match) =
        realized_from_settlements(&settlements);
    let realized_matches = anchor_realized == live_realized;
    println!("== Fidelity anchor (settlement formula vs live) ==");
    println!("  settlements: {settled}   W/L: {wins}/{losses}");
    println!("  realized recomputed: {anchor_realized}");
    println!("  realized live-recorded: {live_realized}");
    println!("  per-record formula match: {all_match}");
    println!(
        "  ANCHOR: {}",
        if all_match && realized_matches {
            "PASS"
        } else {
            "MISMATCH — investigate before trusting counterfactuals"
        }
    );

    // --- Counterfactual under the chosen config ---
    let cfg = CounterfactualConfig { weights, risk };
    let res = simulate_counterfactual(reports.clone(), &resolutions, &slug_of, &cfg);
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
        "  realized: {}   final_equity: {}",
        res.realized,
        res.final_equity()
    );
    println!(
        "  NOTE: Phase-1 fills at target_mid (no book-walk) and excludes arb legs, so this will not"
    );
    println!(
        "        match live fills exactly — that's Phase 3. The anchor above validates accounting."
    );
    Ok(())
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

async fn load_settlements(
    pool: &sqlx::PgPool,
    since: Option<&str>,
) -> anyhow::Result<Vec<SettlementRow>> {
    let rows: Vec<SettlementQueryRow> = sqlx::query_as(
        "SELECT payload->>'won', payload->>'shares', payload->>'cost_basis', payload->>'realized_pnl'
         FROM journal.events
         WHERE event_type = 'paper_position_settled'
           AND ($1::timestamptz IS NULL OR created_at >= $1::timestamptz)
         ORDER BY created_at ASC",
    )
    .bind(since)
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

async fn load_reports(pool: &sqlx::PgPool, since: Option<&str>) -> anyhow::Result<Vec<ReportRow>> {
    let rows: Vec<ReportQueryRow> = sqlx::query_as(
        "SELECT created_at, payload->>'market_id', payload->>'target_outcome',
                payload->>'target_mid', payload->'report'->'attribution'
         FROM journal.events
         WHERE event_type = 'decision_report'
           AND ($1::timestamptz IS NULL OR created_at >= $1::timestamptz)
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
        // Two reports on the same market; a strong positive signal. Only ONE position should open
        // (dedup), and a higher min_net_edge should be able to suppress it entirely.
        let attr = json!({
            "orderbook_momentum": {"score": "0.30", "confidence": "0.5"},
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
        let res = simulate_counterfactual(reports.clone(), &resolutions, &slug_of, &cfg);
        assert_eq!(res.fills, 1, "dedup: only one position per market");
        assert_eq!(res.settled_positions, 1);
        assert!(res.wins == 1 && res.realized > dec!(0));

        // Raise the gate above the net edge ⇒ no trade.
        cfg.risk.min_net_edge = dec!(0.99);
        let res2 = simulate_counterfactual(reports, &resolutions, &slug_of, &cfg);
        assert_eq!(res2.fills, 0);
        assert_eq!(res2.realized, dec!(0));
    }

    #[test]
    fn counterfactual_weights_change_outcome() {
        // A signal whose sign flips the bet: with the default weight it fires positive; zeroing its
        // weight removes its contribution. Confirms the candidate weight vector actually bites.
        let attr = json!({
            "orderbook_momentum": {"score": "0.30", "confidence": "0.5"},
            "news_sentiment": {"score": "-0.30", "confidence": "0.5"},
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
            simulate_counterfactual(reports.clone(), &resolutions, &slug_of, &cfg_balanced).fills,
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
            simulate_counterfactual(reports, &resolutions, &slug_of, &cfg_tilted).fills,
            1
        );
    }
}
