//! Risk management: Kelly criterion position sizing and pre-trade risk gates.
//!
//! All math uses rust_decimal::Decimal — no floats (per AGENTS.md).
//! Paper-only in current phase; real trading would use the same interface behind human gates.
//!
//! ## Sizing model
//! Full Kelly is too aggressive for estimated probabilities — a single bad estimate
//! can blow the bankroll. Quarter-Kelly (kelly_fraction=0.25) is the default: it
//! cuts variance by ~75% while still compounding. The formula used is:
//!
//!   f* = (b·p − q) / b      where b = (1−price)/price, q = 1−p
//!   position_usdc = f* · kelly_fraction · portfolio_usdc
//!   then capped at min(max_position_usdc, portfolio · max_market_exposure_pct)
//!
//! ## RISK implications (per AGENTS.md)
//! - Kelly assumes the true win probability is known; in prediction markets it is
//!   only estimated. Always apply fractional Kelly and never exceed the per-position cap.
//! - Pre-trade checks are mandatory gates, not suggestions. Never bypass them.
//! - All sizing decisions are journaled via the caller's decision_context for Hermes audit.
//! - Paper-only: no real capital exposure in this phase.

// check_pre_trade + supporting types are ready for wiring to the paper order submit handler.
// Suppress dead_code until that increment (same pattern as strategy/mod.rs).
#![allow(dead_code)]

use anyhow::Result;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tracing::{info, warn};

/// Configuration for the risk manager.
/// Defaults are conservative for a ~$150 paper portfolio.
#[derive(Debug, Clone)]
pub struct RiskConfig {
    /// Fraction of full Kelly to apply (0.25 = quarter-Kelly).
    pub kelly_fraction: Decimal,
    /// Hard USDC cap per position, regardless of Kelly.
    pub max_position_usdc: Decimal,
    /// Max fraction of total portfolio value in any one market (e.g. 0.20 = 20 %).
    pub max_market_exposure_pct: Decimal,
    /// Max fraction of portfolio locked across any one CORRELATED CLUSTER of markets (e.g. 0.35).
    /// Many Polymarket questions are near-duplicates of the same event (e.g. the ~15 Iran / Strait
    /// of Hormuz peace-deal markets). Each clears the per-market cap individually, yet together they
    /// are one giant correlated bet whose YES winners and NO losers cancel. This caps the aggregate
    /// exposure to any one such cluster. Uncorrelated markets are exempt (each is its own cluster).
    pub max_cluster_exposure_pct: Decimal,
    /// Max fraction of portfolio that can be locked simultaneously (e.g. 0.80 = 80 %).
    pub max_total_exposure_pct: Decimal,
    /// Minimum net edge (after fees) to approve a trade — the LIVE gate (e.g. 0.02 = 2 %).
    pub min_net_edge: Decimal,
    /// A stricter comparison ("shadow") gate used only for A/B attribution, NOT enforcement.
    /// Every executed fill is tagged with whether it also clears this threshold, so we can compare
    /// how a stricter gate would have performed using the same live run (the stricter portfolio is a
    /// subset of the lenient one). Default 0.04 = 4 %.
    pub shadow_net_edge: Decimal,
    /// Stop trading if cumulative PnL / portfolio_value drops below this threshold.
    pub pnl_floor: Decimal,
    /// Roadmap "P1 — friction-aware entry gate" (2026-07-10): multiplier `k` such that a trade is
    /// only approved when `net_edge >= k * round_trip_cost_frac(price)` (see that function for the
    /// price-banded fee+slippage estimate). `min_net_edge` alone only ever charged the ENTRY fee —
    /// it never priced the EXIT leg or the realized slippage, both of which are severe on cheap
    /// shares (measured 2026-07-10: 760bps one-way total cost under $0.20 vs 107bps above $0.80).
    /// 0 disables the gate entirely (pure min_net_edge behavior, unchanged).
    pub round_trip_cost_multiplier: Decimal,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            kelly_fraction: dec!(0.25),
            max_position_usdc: dec!(20),
            max_market_exposure_pct: dec!(0.20),
            max_cluster_exposure_pct: dec!(0.35),
            max_total_exposure_pct: dec!(0.80),
            min_net_edge: dec!(0.02),
            shadow_net_edge: dec!(0.04),
            pnl_floor: dec!(-0.20),
            round_trip_cost_multiplier: dec!(1.5),
        }
    }
}

/// Price-banded round-trip cost estimate: fee + slippage, BOTH legs (entry + exit), as a fraction of
/// notional. `min_net_edge` alone only ever priced the entry fee; this is the actual cost of a full
/// round-trip on THIS venue, which is highly price-dependent (a thin, cheap-share book is expensive to
/// cross twice). Bands and bps are the measured post-reset average `paper_fills.slippage_bps` + real
/// per-fill fee rate, by entry price, as of 2026-07-10 (re-derive via the same query as fill history
/// grows — see wiki/roadmap "P1 — friction-aware entry gate"):
///   <0.20: 404 slip + 356 fee = 760bps one-way   0.20-0.40: 241+272=513   0.40-0.60: 230+127=357
///   0.60-0.80: 178+100=278bps one-way            >=0.80: 85+22=107bps one-way
/// Doubled for the round trip (a symmetric exit is assumed — the exits feature always closes at
/// market, i.e. also crosses the book).
pub fn round_trip_cost_frac(price: Decimal) -> Decimal {
    let one_way_bps = if price < dec!(0.20) {
        dec!(760)
    } else if price < dec!(0.40) {
        dec!(513)
    } else if price < dec!(0.60) {
        dec!(357)
    } else if price < dec!(0.80) {
        dec!(278)
    } else {
        dec!(107)
    };
    (one_way_bps * dec!(2)) / dec!(10000)
}

impl RiskConfig {
    /// Build from environment, falling back to the conservative defaults. Lets the operator tune the
    /// gate (e.g. the 2% live / 4% shadow split) and sizing without a code change; the same values are
    /// surfaced read-only in the UI parameters panel.
    pub fn from_env() -> Self {
        fn dec_env(key: &str, default: Decimal) -> Decimal {
            std::env::var(key)
                .ok()
                .and_then(|v| v.trim().parse::<Decimal>().ok())
                .unwrap_or(default)
        }
        let d = RiskConfig::default();
        RiskConfig {
            kelly_fraction: dec_env("POLYTRADER_KELLY_FRACTION", d.kelly_fraction),
            max_position_usdc: dec_env("POLYTRADER_MAX_POSITION_USDC", d.max_position_usdc),
            max_market_exposure_pct: dec_env(
                "POLYTRADER_MAX_MARKET_EXPOSURE_PCT",
                d.max_market_exposure_pct,
            ),
            max_cluster_exposure_pct: dec_env(
                "POLYTRADER_MAX_CLUSTER_EXPOSURE_PCT",
                d.max_cluster_exposure_pct,
            ),
            max_total_exposure_pct: dec_env(
                "POLYTRADER_MAX_TOTAL_EXPOSURE_PCT",
                d.max_total_exposure_pct,
            ),
            min_net_edge: dec_env("POLYTRADER_MIN_NET_EDGE", d.min_net_edge),
            shadow_net_edge: dec_env("POLYTRADER_SHADOW_NET_EDGE", d.shadow_net_edge),
            pnl_floor: dec_env("POLYTRADER_PNL_FLOOR", d.pnl_floor),
            round_trip_cost_multiplier: dec_env(
                "POLYTRADER_ROUNDTRIP_COST_MULTIPLIER",
                d.round_trip_cost_multiplier,
            ),
        }
    }

    /// Read-only JSON view of the effective risk parameters for the UI parameters panel.
    /// Decimals are emitted as strings to preserve exactness.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "kelly_fraction": self.kelly_fraction.to_string(),
            "max_position_usdc": self.max_position_usdc.to_string(),
            "max_market_exposure_pct": self.max_market_exposure_pct.to_string(),
            "max_cluster_exposure_pct": self.max_cluster_exposure_pct.to_string(),
            "max_total_exposure_pct": self.max_total_exposure_pct.to_string(),
            "min_net_edge": self.min_net_edge.to_string(),
            "shadow_net_edge": self.shadow_net_edge.to_string(),
            "pnl_floor": self.pnl_floor.to_string(),
            "round_trip_cost_multiplier": self.round_trip_cost_multiplier.to_string(),
        })
    }
}

/// Kelly sizing result (surfaced in decision_context for Hermes).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionSizeResult {
    /// Final recommended USDC after all caps applied.
    pub recommended_usdc: Decimal,
    /// Raw Kelly USDC before caps (for attribution).
    pub kelly_usdc: Decimal,
    /// Which cap was binding, if any ("max_position_usdc" | "max_market_exposure_pct" | "negative_kelly" | ...).
    pub capped_by: Option<String>,
    /// Human-readable rationale for audit trail.
    pub rationale: String,
}

/// Result of a pre-trade risk gate check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskCheck {
    pub approved: bool,
    pub reason: String,
    /// Recommended (possibly trimmed) size in USDC. None if rejected.
    pub recommended_size: Option<Decimal>,
}

/// Classify a market slug into a CORRELATED CLUSTER key. Markets in the same cluster resolve off the
/// same underlying event (or highly correlated events), so their outcomes are not independent. The
/// `"uncorrelated"` bucket means "treat as its own cluster" (the cluster cap does not apply).
///
/// Slug-based and deliberately coarse: it errs toward grouping (a false grouping only tightens risk).
/// Sports are checked first because World-Cup slugs embed country names (e.g. `...-france-win-...`).
pub fn cluster_key(slug: &str) -> &'static str {
    let s = slug.to_lowercase();
    if s.contains("world-cup")
        || s.contains("fifa")
        || s.contains("nba")
        || s.contains("knicks")
        || s.contains("super-bowl")
    {
        return "sports";
    }
    // The big one: US x Iran peace deal / Strait of Hormuz / nuclear-enrichment markets.
    if s.contains("iran")
        || s.contains("hormuz")
        || s.contains("uranium")
        || s.contains("enrichment")
    {
        return "iran_geopolitics";
    }
    if s.contains("netanyahu") || s.contains("eizenkot") || s.contains("prime-minister-of-israel") {
        return "israel_pm";
    }
    if s.contains("china") || s.contains("taiwan") {
        return "china_taiwan";
    }
    if s.contains("bitcoin") || s.contains("ethereum") || s.contains("crypto") {
        return "crypto";
    }
    if s.contains("fed-rate") || s.contains("rate-cut") || s.contains("rate-hike") {
        return "fed_rates";
    }
    if s.contains("2028") {
        return "us_2028";
    }
    if s.contains("midterm")
        || s.contains("balance-of-power")
        || s.contains("control-the-house")
        || s.contains("control-the-senate")
    {
        return "us_2026_midterms";
    }
    if s.contains("brazil") || s.contains("lula") || s.contains("bolsonaro") {
        return "brazil_2026";
    }
    if s.contains("french")
        || s.contains("france")
        || s.contains("bardella")
        || s.contains("philippe")
    {
        return "france_2027";
    }
    if s.contains("makerfield") || s.contains("starmer") || s.contains("burnham") {
        return "uk_politics";
    }
    "uncorrelated"
}

/// Portfolio state for risk calculations. In production this is loaded from Postgres by
/// `RiskManager::load_exposure`; the offline backtest harness constructs it from its simulated
/// portfolio and feeds it straight to the pure [`RiskManager::gate`].
#[derive(Debug, Clone)]
pub struct PortfolioExposure {
    pub virtual_usdc: Decimal,
    pub total_locked: Decimal,
    pub total_pnl: Decimal,
    pub market_locked: Decimal,
    /// Correlated-cluster key of the candidate market ("uncorrelated" = cap N/A).
    pub cluster_key: &'static str,
    /// Total collateral already locked across ALL open positions in the candidate's cluster.
    pub cluster_locked: Decimal,
}

pub struct RiskManager {
    config: RiskConfig,
}

impl RiskManager {
    pub fn new(config: RiskConfig) -> Self {
        Self { config }
    }

    pub fn with_defaults() -> Self {
        Self::new(RiskConfig::default())
    }

    /// Construct from environment overrides (see RiskConfig::from_env).
    pub fn from_env() -> Self {
        Self::new(RiskConfig::from_env())
    }

    /// Compute Kelly-criterion position size for a binary prediction market.
    ///
    /// `win_prob`: your estimated true probability of winning (0..1).
    /// `entry_price`: the price per share you would pay (0..1); equals the
    ///   market's ask price for a taker buy.
    /// `portfolio_usdc`: current total portfolio value (available + locked).
    ///
    /// Returns a PositionSizeResult with the recommended USDC to deploy.
    ///
    /// RISK: If win_prob is mis-estimated by even a few percent, Kelly sizing can
    /// still over-bet. Quarter-Kelly (default) provides an important safety margin.
    pub fn kelly_size(
        &self,
        win_prob: Decimal,
        entry_price: Decimal,
        portfolio_usdc: Decimal,
    ) -> PositionSizeResult {
        if entry_price <= Decimal::ZERO || entry_price >= Decimal::ONE {
            return PositionSizeResult {
                recommended_usdc: Decimal::ZERO,
                kelly_usdc: Decimal::ZERO,
                capped_by: Some("invalid_price".into()),
                rationale: format!("entry_price {entry_price} out of (0,1) range"),
            };
        }
        if win_prob <= Decimal::ZERO || portfolio_usdc <= Decimal::ZERO {
            return PositionSizeResult {
                recommended_usdc: Decimal::ZERO,
                kelly_usdc: Decimal::ZERO,
                capped_by: Some("no_edge_or_zero_portfolio".into()),
                rationale: format!("win_prob={win_prob} portfolio={portfolio_usdc}"),
            };
        }

        // b = net odds per unit staked if the bet wins
        let b = (Decimal::ONE - entry_price) / entry_price;
        let q = Decimal::ONE - win_prob;
        let f_star = (b * win_prob - q) / b;

        if f_star <= Decimal::ZERO {
            return PositionSizeResult {
                recommended_usdc: Decimal::ZERO,
                kelly_usdc: Decimal::ZERO,
                capped_by: Some("negative_kelly".into()),
                rationale: format!(
                    "Kelly f*={f_star:.4}; no positive edge at entry_price={entry_price}"
                ),
            };
        }

        let kelly_usdc = f_star * self.config.kelly_fraction * portfolio_usdc;

        // Two caps: absolute per-position and market-concentration
        let concentration_cap = portfolio_usdc * self.config.max_market_exposure_pct;
        let binding_cap = self.config.max_position_usdc.min(concentration_cap);
        let (recommended_usdc, capped_by) = if kelly_usdc > binding_cap {
            let cap_name = if concentration_cap < self.config.max_position_usdc {
                "max_market_exposure_pct"
            } else {
                "max_position_usdc"
            };
            (binding_cap, Some(cap_name.into()))
        } else {
            (kelly_usdc, None)
        };

        PositionSizeResult {
            recommended_usdc: recommended_usdc.max(Decimal::ZERO),
            kelly_usdc,
            capped_by,
            rationale: format!(
                "Kelly f*={f_star:.4}, fractional({})={kelly_usdc:.2} USDC; cap={binding_cap:.2} (pos_cap={:.2}, conc_cap={concentration_cap:.2})",
                self.config.kelly_fraction, self.config.max_position_usdc
            ),
        }
    }

    /// Pre-trade risk gate. Returns approved=true only when all checks pass.
    ///
    /// Checks (in order):
    ///   1. Net edge >= min threshold (PRIMARY gate — see goals wiki 4-6% min net)
    ///   2. Portfolio PnL above floor (stop-loss)
    ///   3. Total portfolio exposure within limit
    ///   4. Per-market concentration within limit (trims rather than rejects)
    ///
    /// RISK: This gate protects against over-trading and ruin. All rejections are
    /// logged so Hermes can detect persistent edge decay or position bloat.
    pub async fn check_pre_trade(
        &self,
        pool: &PgPool,
        market_id: &str,
        net_edge: Decimal,
        proposed_usdc: Decimal,
        price: Decimal,
    ) -> Result<RiskCheck> {
        // Gate 1 short-circuit: the common min-edge rejection skips the DB exposure load. gate()
        // applies the identical check (via min_edge_reject) so the offline backtest harness
        // reproduces this decision without a database.
        if net_edge < self.config.min_net_edge {
            return Ok(self.min_edge_reject(net_edge));
        }
        if let Some(r) = self.round_trip_reject(net_edge, price) {
            return Ok(r);
        }
        let exp = self.load_exposure(pool, market_id).await?;
        Ok(self.gate(market_id, net_edge, proposed_usdc, price, &exp))
    }

    /// The standard "below minimum net edge" rejection, shared by the live short-circuit in
    /// `check_pre_trade` and the pure `gate` so the message can't drift between them.
    fn min_edge_reject(&self, net_edge: Decimal) -> RiskCheck {
        RiskCheck {
            approved: false,
            reason: format!(
                "net_edge {net_edge:.4} < min {:.4} (goals wiki 4-6% min net)",
                self.config.min_net_edge
            ),
            recommended_size: None,
        }
    }

    /// Gate 1.5 (P1, 2026-07-10): reject when net edge doesn't clear `k *
    /// round_trip_cost_frac(price)`. `None` when the multiplier is 0 (gate disabled) or the trade
    /// clears it. Shared by the live short-circuit and the pure `gate` for the same no-drift reason
    /// as `min_edge_reject`.
    fn round_trip_reject(&self, net_edge: Decimal, price: Decimal) -> Option<RiskCheck> {
        if self.config.round_trip_cost_multiplier <= Decimal::ZERO {
            return None;
        }
        let required = round_trip_cost_frac(price) * self.config.round_trip_cost_multiplier;
        if net_edge >= required {
            return None;
        }
        Some(RiskCheck {
            approved: false,
            reason: format!(
                "net_edge {net_edge:.4} < round-trip friction floor {required:.4} \
                 ({}x round_trip_cost_frac({price:.2}))",
                self.config.round_trip_cost_multiplier.normalize()
            ),
            recommended_size: None,
        })
    }

    /// Pure pre-trade risk gate: given the candidate edge/size and a portfolio-exposure snapshot,
    /// produce the approve / trim / reject decision. No DB, no async — the single source of truth
    /// shared by the live [`RiskManager::check_pre_trade`] (which loads `exp` from Postgres) and the
    /// offline backtest harness (which feeds a simulated `exp`). Gate order: 1 min-edge, 1.5
    /// friction-aware round-trip cost floor, 2 PnL floor, 3 total exposure, 3.5 correlated-cluster
    /// cap (trim), 4 per-market concentration (trim/reject).
    pub fn gate(
        &self,
        market_id: &str,
        net_edge: Decimal,
        proposed_usdc: Decimal,
        price: Decimal,
        exp: &PortfolioExposure,
    ) -> RiskCheck {
        // Gate 1: minimum net edge
        if net_edge < self.config.min_net_edge {
            return self.min_edge_reject(net_edge);
        }
        // Gate 1.5: friction-aware round-trip cost floor (see round_trip_reject doc).
        if let Some(r) = self.round_trip_reject(net_edge, price) {
            return r;
        }

        let total_value = exp.virtual_usdc + exp.total_locked;
        if total_value <= Decimal::ZERO {
            return RiskCheck {
                approved: false,
                reason: "portfolio value is zero".into(),
                recommended_size: None,
            };
        }

        // Gate 2: PnL floor
        let pnl_pct = exp.total_pnl / total_value;
        if pnl_pct < self.config.pnl_floor {
            warn!(
                pnl_pct = %pnl_pct,
                floor = %self.config.pnl_floor,
                "risk gate: PnL floor breached; blocking trade (paper-only)"
            );
            return RiskCheck {
                approved: false,
                reason: format!(
                    "portfolio PnL {:.1}% below floor {:.1}%",
                    pnl_pct * dec!(100),
                    self.config.pnl_floor * dec!(100)
                ),
                recommended_size: None,
            };
        }

        // Gate 3: total exposure
        let post_locked = exp.total_locked + proposed_usdc;
        if post_locked / total_value > self.config.max_total_exposure_pct {
            return RiskCheck {
                approved: false,
                reason: format!(
                    "total exposure {:.1}% would exceed limit {:.1}%",
                    post_locked / total_value * dec!(100),
                    self.config.max_total_exposure_pct * dec!(100)
                ),
                recommended_size: None,
            };
        }

        // Gate 3.5: correlated-cluster concentration. Caps aggregate exposure across all markets that
        // resolve off the same underlying event (e.g. the Iran/Hormuz peace-deal cluster), which each
        // individually pass the per-market cap yet together form one giant correlated bet. Exempt for
        // the "uncorrelated" bucket (there cluster_locked == market_locked and the per-market cap, being
        // stricter, governs). Trims `proposed_usdc` down to the remaining cluster headroom, then lets
        // the per-market gate below trim further if needed; rejects outright only at zero headroom.
        let mut proposed_usdc = proposed_usdc;
        let mut cluster_trim_note: Option<String> = None;
        if exp.cluster_key != "uncorrelated"
            && (exp.cluster_locked + proposed_usdc) / total_value
                > self.config.max_cluster_exposure_pct
        {
            let headroom =
                (total_value * self.config.max_cluster_exposure_pct) - exp.cluster_locked;
            if headroom <= Decimal::ZERO {
                return RiskCheck {
                    approved: false,
                    reason: format!(
                        "correlated cluster '{}' already at {:.1}% exposure limit",
                        exp.cluster_key,
                        self.config.max_cluster_exposure_pct * dec!(100)
                    ),
                    recommended_size: None,
                };
            }
            warn!(
                cluster = exp.cluster_key,
                trimmed_to = %headroom,
                "position trimmed to respect correlated-cluster limit (paper-only)"
            );
            proposed_usdc = headroom.min(proposed_usdc);
            cluster_trim_note = Some(format!(
                "trimmed to {:.2} USDC (cluster '{}' limit {:.1}%)",
                proposed_usdc,
                exp.cluster_key,
                self.config.max_cluster_exposure_pct * dec!(100)
            ));
        }

        // Gate 4: market concentration (trim rather than reject outright)
        let post_market = exp.market_locked + proposed_usdc;
        let market_pct = post_market / total_value;
        if market_pct > self.config.max_market_exposure_pct {
            let headroom = (total_value * self.config.max_market_exposure_pct) - exp.market_locked;
            if headroom <= Decimal::ZERO {
                return RiskCheck {
                    approved: false,
                    reason: format!(
                        "market {market_id} already at {:.1}% concentration limit",
                        self.config.max_market_exposure_pct * dec!(100)
                    ),
                    recommended_size: None,
                };
            }
            warn!(
                market = %market_id,
                trimmed_to = %headroom,
                "position trimmed to respect concentration limit (paper-only)"
            );
            return RiskCheck {
                approved: true,
                reason: format!(
                    "trimmed to {headroom:.2} USDC (concentration limit {:.1}%)",
                    self.config.max_market_exposure_pct * dec!(100)
                ),
                recommended_size: Some(headroom),
            };
        }

        info!(
            market = %market_id,
            net_edge = %net_edge,
            proposed = %proposed_usdc,
            cluster = exp.cluster_key,
            "pre-trade risk check passed (paper-only)"
        );
        RiskCheck {
            approved: true,
            reason: cluster_trim_note.unwrap_or_else(|| "all gates passed".into()),
            recommended_size: Some(proposed_usdc),
        }
    }

    pub fn config(&self) -> &RiskConfig {
        &self.config
    }

    async fn load_exposure(&self, pool: &PgPool, market_id: &str) -> Result<PortfolioExposure> {
        let row: Option<(Decimal, Decimal, Decimal, Decimal)> = sqlx::query_as(
            "SELECT virtual_usdc, total_locked, unrealized_pnl, realized_pnl
             FROM paper_trading.virtual_portfolio_snapshots
             ORDER BY as_of DESC LIMIT 1",
        )
        .fetch_optional(pool)
        .await?;

        let (virtual_usdc, total_locked, unrealized_pnl, realized_pnl) =
            row.unwrap_or((dec!(150), Decimal::ZERO, Decimal::ZERO, Decimal::ZERO));

        let market_locked: Decimal = sqlx::query_scalar(
            "SELECT COALESCE(SUM(collateral_locked), 0)
             FROM paper_trading.paper_positions
             WHERE market_id = $1",
        )
        .bind(market_id)
        .fetch_one(pool)
        .await
        .unwrap_or(Decimal::ZERO);

        // Candidate market's slug → cluster key. If we can't resolve a slug, treat as uncorrelated
        // (no cluster cap) rather than guessing.
        let candidate_slug: Option<String> =
            sqlx::query_scalar("SELECT slug FROM market_data.markets WHERE gamma_id = $1")
                .bind(market_id)
                .fetch_optional(pool)
                .await
                .ok()
                .flatten();
        let cluster_key = candidate_slug
            .as_deref()
            .map(cluster_key)
            .unwrap_or("uncorrelated");

        // Sum collateral across every OPEN position whose market shares the candidate's cluster.
        // Done in Rust (the classifier is slug-based) over the open-position rows.
        let cluster_locked = if cluster_key == "uncorrelated" {
            // No cross-market aggregation for the catch-all bucket; per-market cap still applies.
            market_locked
        } else {
            let rows: Vec<(Option<String>, Decimal)> = sqlx::query_as(
                "SELECT m.slug, p.collateral_locked
                 FROM paper_trading.paper_positions p
                 JOIN market_data.markets m ON m.gamma_id = p.market_id
                 WHERE p.shares > 0",
            )
            .fetch_all(pool)
            .await
            .unwrap_or_default();
            rows.into_iter()
                .filter(|(slug, _)| {
                    slug.as_deref()
                        .map(|s| self::cluster_key(s) == cluster_key)
                        .unwrap_or(false)
                })
                .map(|(_, c)| c)
                .sum()
        };

        Ok(PortfolioExposure {
            virtual_usdc,
            total_locked,
            total_pnl: unrealized_pnl + realized_pnl,
            market_locked,
            cluster_key,
            cluster_locked,
        })
    }
}

#[cfg(test)]
mod gate_tests {
    use super::*;

    // Defaults: min_edge 0.02, market 20%, cluster 35%, total 80%, pnl_floor -20%.
    fn mgr() -> RiskManager {
        RiskManager::with_defaults()
    }

    #[allow(clippy::too_many_arguments)]
    fn exp(
        virtual_usdc: Decimal,
        total_locked: Decimal,
        total_pnl: Decimal,
        market_locked: Decimal,
        cluster_key: &'static str,
        cluster_locked: Decimal,
    ) -> PortfolioExposure {
        PortfolioExposure {
            virtual_usdc,
            total_locked,
            total_pnl,
            market_locked,
            cluster_key,
            cluster_locked,
        }
    }

    #[test]
    fn gate_rejects_below_min_edge() {
        let e = exp(
            dec!(150),
            dec!(0),
            dec!(0),
            dec!(0),
            "uncorrelated",
            dec!(0),
        );
        let r = mgr().gate("m", dec!(0.01), dec!(10), dec!(0.9), &e);
        assert!(!r.approved);
        assert!(r.reason.contains("min"), "{}", r.reason);
        assert!(r.recommended_size.is_none());
    }

    #[test]
    fn gate_approves_clean_trade_untrimmed() {
        let e = exp(
            dec!(150),
            dec!(0),
            dec!(0),
            dec!(0),
            "uncorrelated",
            dec!(0),
        );
        let r = mgr().gate("m", dec!(0.05), dec!(10), dec!(0.9), &e);
        assert!(r.approved);
        assert_eq!(r.recommended_size, Some(dec!(10)));
    }

    #[test]
    fn gate_rejects_on_pnl_floor() {
        // -40 / 150 = -26.7% < -20% floor.
        let e = exp(
            dec!(150),
            dec!(0),
            dec!(-40),
            dec!(0),
            "uncorrelated",
            dec!(0),
        );
        let r = mgr().gate("m", dec!(0.05), dec!(10), dec!(0.9), &e);
        assert!(!r.approved);
        assert!(r.reason.contains("floor"), "{}", r.reason);
    }

    #[test]
    fn gate_rejects_on_total_exposure() {
        // total_value 120; post_locked 110 -> 91.7% > 80%.
        let e = exp(
            dec!(20),
            dec!(100),
            dec!(0),
            dec!(0),
            "uncorrelated",
            dec!(0),
        );
        let r = mgr().gate("m", dec!(0.05), dec!(10), dec!(0.9), &e);
        assert!(!r.approved);
        assert!(r.reason.contains("total exposure"), "{}", r.reason);
    }

    #[test]
    fn gate_trims_to_cluster_headroom() {
        // cluster 'iran' at $50 of $200; cap 35% = $70 -> headroom $20; proposed $30 trimmed to $20.
        let e = exp(
            dec!(150),
            dec!(50),
            dec!(0),
            dec!(0),
            "iran_geopolitics",
            dec!(50),
        );
        let r = mgr().gate("m", dec!(0.05), dec!(30), dec!(0.9), &e);
        assert!(r.approved);
        assert_eq!(r.recommended_size, Some(dec!(20)));
        assert!(r.reason.contains("cluster"), "{}", r.reason);
    }

    #[test]
    fn gate_trims_to_market_concentration() {
        // uncorrelated (cluster gate skipped); market at $20 of $170, cap 20% = $34 -> headroom $14.
        let e = exp(
            dec!(150),
            dec!(20),
            dec!(0),
            dec!(20),
            "uncorrelated",
            dec!(20),
        );
        let r = mgr().gate("m", dec!(0.05), dec!(30), dec!(0.9), &e);
        assert!(r.approved);
        assert_eq!(r.recommended_size, Some(dec!(14)));
        assert!(r.reason.contains("concentration"), "{}", r.reason);
    }

    #[test]
    fn round_trip_cost_frac_is_price_banded_and_doubled() {
        // 760bps one-way under $0.20 -> 1520bps round trip.
        assert_eq!(round_trip_cost_frac(dec!(0.10)), dec!(0.152));
        // 107bps one-way >= $0.80 -> 214bps round trip.
        assert_eq!(round_trip_cost_frac(dec!(0.90)), dec!(0.0214));
    }

    #[test]
    fn gate_rejects_cheap_share_below_friction_floor() {
        // price 0.10 -> round_trip_cost_frac = 0.152; default multiplier 1.5 -> floor 0.228.
        // net_edge 0.05 clears Gate 1 (min_net_edge 0.02) but not the friction floor.
        let e = exp(
            dec!(150),
            dec!(0),
            dec!(0),
            dec!(0),
            "uncorrelated",
            dec!(0),
        );
        let r = mgr().gate("m", dec!(0.05), dec!(10), dec!(0.10), &e);
        assert!(!r.approved);
        assert!(r.reason.contains("round-trip"), "{}", r.reason);
        // The multiplier must print exactly ("1.5x"), not truncated to "1x" the way a `{:.0}`
        // format of Decimal 1.5 did.
        assert!(r.reason.contains("(1.5x"), "{}", r.reason);
    }

    #[test]
    fn gate_approves_cheap_share_when_edge_clears_friction_floor() {
        // Same 0.228 floor, but net_edge 0.25 clears it.
        let e = exp(
            dec!(150),
            dec!(0),
            dec!(0),
            dec!(0),
            "uncorrelated",
            dec!(0),
        );
        let r = mgr().gate("m", dec!(0.25), dec!(10), dec!(0.10), &e);
        assert!(r.approved);
    }

    #[test]
    fn gate_friction_floor_disabled_by_zero_multiplier() {
        let mut cfg = RiskConfig::default();
        cfg.round_trip_cost_multiplier = dec!(0);
        let rm = RiskManager::new(cfg);
        let e = exp(
            dec!(150),
            dec!(0),
            dec!(0),
            dec!(0),
            "uncorrelated",
            dec!(0),
        );
        // net_edge 0.05 would fail the friction floor at the default multiplier but the gate is off.
        let r = rm.gate("m", dec!(0.05), dec!(10), dec!(0.10), &e);
        assert!(r.approved);
    }
}
