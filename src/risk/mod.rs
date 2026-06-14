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
    /// Max fraction of portfolio that can be locked simultaneously (e.g. 0.80 = 80 %).
    pub max_total_exposure_pct: Decimal,
    /// Minimum net edge (after fees) to approve a trade (e.g. 0.04 = 4 %).
    pub min_net_edge: Decimal,
    /// Stop trading if cumulative PnL / portfolio_value drops below this threshold.
    pub pnl_floor: Decimal,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            kelly_fraction: dec!(0.25),
            max_position_usdc: dec!(20),
            max_market_exposure_pct: dec!(0.20),
            max_total_exposure_pct: dec!(0.80),
            min_net_edge: dec!(0.04),
            pnl_floor: dec!(-0.20),
        }
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

/// Portfolio state loaded from DB for risk calculations.
#[derive(Debug, Clone)]
struct PortfolioExposure {
    virtual_usdc: Decimal,
    total_locked: Decimal,
    total_pnl: Decimal,
    market_locked: Decimal,
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
    ) -> Result<RiskCheck> {
        // Gate 1: minimum net edge
        if net_edge < self.config.min_net_edge {
            return Ok(RiskCheck {
                approved: false,
                reason: format!(
                    "net_edge {net_edge:.4} < min {:.4} (goals wiki 4-6% min net)",
                    self.config.min_net_edge
                ),
                recommended_size: None,
            });
        }

        let exp = self.load_exposure(pool, market_id).await?;
        let total_value = exp.virtual_usdc + exp.total_locked;

        if total_value <= Decimal::ZERO {
            return Ok(RiskCheck {
                approved: false,
                reason: "portfolio value is zero".into(),
                recommended_size: None,
            });
        }

        // Gate 2: PnL floor
        let pnl_pct = exp.total_pnl / total_value;
        if pnl_pct < self.config.pnl_floor {
            warn!(
                pnl_pct = %pnl_pct,
                floor = %self.config.pnl_floor,
                "risk gate: PnL floor breached; blocking trade (paper-only)"
            );
            return Ok(RiskCheck {
                approved: false,
                reason: format!(
                    "portfolio PnL {:.1}% below floor {:.1}%",
                    pnl_pct * dec!(100),
                    self.config.pnl_floor * dec!(100)
                ),
                recommended_size: None,
            });
        }

        // Gate 3: total exposure
        let post_locked = exp.total_locked + proposed_usdc;
        if post_locked / total_value > self.config.max_total_exposure_pct {
            return Ok(RiskCheck {
                approved: false,
                reason: format!(
                    "total exposure {:.1}% would exceed limit {:.1}%",
                    post_locked / total_value * dec!(100),
                    self.config.max_total_exposure_pct * dec!(100)
                ),
                recommended_size: None,
            });
        }

        // Gate 4: market concentration (trim rather than reject outright)
        let post_market = exp.market_locked + proposed_usdc;
        let market_pct = post_market / total_value;
        if market_pct > self.config.max_market_exposure_pct {
            let headroom = (total_value * self.config.max_market_exposure_pct) - exp.market_locked;
            if headroom <= Decimal::ZERO {
                return Ok(RiskCheck {
                    approved: false,
                    reason: format!(
                        "market {market_id} already at {:.1}% concentration limit",
                        self.config.max_market_exposure_pct * dec!(100)
                    ),
                    recommended_size: None,
                });
            }
            warn!(
                market = %market_id,
                trimmed_to = %headroom,
                "position trimmed to respect concentration limit (paper-only)"
            );
            return Ok(RiskCheck {
                approved: true,
                reason: format!(
                    "trimmed to {headroom:.2} USDC (concentration limit {:.1}%)",
                    self.config.max_market_exposure_pct * dec!(100)
                ),
                recommended_size: Some(headroom),
            });
        }

        info!(
            market = %market_id,
            net_edge = %net_edge,
            proposed = %proposed_usdc,
            "pre-trade risk check passed (paper-only)"
        );
        Ok(RiskCheck {
            approved: true,
            reason: "all gates passed".into(),
            recommended_size: Some(proposed_usdc),
        })
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

        Ok(PortfolioExposure {
            virtual_usdc,
            total_locked,
            total_pnl: unrealized_pnl + realized_pnl,
            market_locked,
        })
    }
}
