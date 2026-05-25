//! Core data models for the paper trading domain (decimals everywhere).

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperOrder {
    pub id: uuid::Uuid,
    pub market_id: String,
    pub outcome: String, // "Yes" | "No" — required for binary market shares
    pub side: OrderSide,
    pub order_type: OrderType,
    pub limit_price: Option<Decimal>,
    pub size: Decimal,
    pub status: OrderStatus,
    pub created_at: DateTime<Utc>,
    /// Free-form context (rationale, signals, strategy notes) for Hermes reflection & audit.
    /// Stored as JSONB in DB.
    pub decision_context: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum OrderSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum OrderType {
    Market,
    Limit,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum OrderStatus {
    Open,
    PartiallyFilled,
    Filled,
    Cancelled,
    Rejected,
}

impl std::fmt::Display for OrderSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrderSide::Buy => write!(f, "Buy"),
            OrderSide::Sell => write!(f, "Sell"),
        }
    }
}
impl std::fmt::Display for OrderType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrderType::Market => write!(f, "Market"),
            OrderType::Limit => write!(f, "Limit"),
        }
    }
}
impl std::fmt::Display for OrderStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrderStatus::Open => write!(f, "Open"),
            OrderStatus::PartiallyFilled => write!(f, "PartiallyFilled"),
            OrderStatus::Filled => write!(f, "Filled"),
            OrderStatus::Cancelled => write!(f, "Cancelled"),
            OrderStatus::Rejected => write!(f, "Rejected"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperFill {
    pub id: uuid::Uuid,
    pub order_id: uuid::Uuid,
    pub price: Decimal,
    pub size: Decimal,
    pub fee: Decimal,
    pub slippage_bps: i32,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PaperPosition {
    pub market_id: String,
    pub outcome: String, // "Yes" | "No"
    pub shares: Decimal,
    pub avg_entry_price: Decimal,
    pub collateral_locked: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualPortfolio {
    pub virtual_usdc: Decimal,
    pub total_locked: Decimal,
    pub unrealized_pnl: Decimal,
    pub realized_pnl: Decimal,
    pub as_of: DateTime<Utc>,
}

/// Fee model first-class citizen for realistic paper (and future real) execution.
///
/// All money/edge math uses rust_decimal::Decimal exclusively (per AGENTS.md + Cargo).
/// Supports configurable taker/maker (volume tier foundation), estimated gas, rewards offset.
/// Volume tiers / dynamic lookup stubbed for smallest increment (future: from journal history or config json).
///
/// ## RISK IMPLICATIONS (AGENTS.md Trading Safety + fees-tax-latency-and-execution-tiers.md)
/// - **Critical for ~$150 paper capital**: Fees/gas/slippage are first-order; a 30bps mis-model on a $5 notional trade can erase the entire edge or turn +EV into net loss.
/// - Always over-estimate fees in opportunity evaluation and paper sizing (pessimistic defaults here: 1.5% taker vs real low-volume ~0.5-1%).
/// - Net edge (after fees) is the *only* number that matters for the deliberate tier / min 4-6% gate in goals-and-operational-cadence.md.
/// - This model is paper-only simulation. Real trading (future gated) must re-derive live fees from SDK + on-chain + rewards API.
/// - No silent fallbacks or magic numbers in callers: every net calc is explicit and journaled (via existing paper_fills.fee + decision_context jsonb or new metrics).
/// - Gas modeled as USDC opportunity cost even on L2 (Polygon); real gas volatile.
/// - Journal *every* virtual fill and pre-trade opportunity using this model for later Hermes fee-drag attribution (3.3).
///
/// **Credits**: Design and requirements from `wiki/strategies/fees-tax-latency-and-execution-tiers.md` (and sister `goals-and-operational-cadence.md`), efe1660 commit. Patterns adapted from transferred 5-repo fee/slippage handling (poly-maker trading_utils + stats, BTC-bot execution/fee models, openclaw paper trader costing).
///
/// See PaperTradingEngine for integration + net calc exposure to FusionEngine/strategy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FeeModel {
    pub taker_bps: Decimal,
    pub maker_bps: Decimal,
    pub est_gas_usdc_per_tx: Decimal,
    pub rewards_offset_bps: Decimal,
    // TODO(future, post Tier1 proof): volume_tier: u32, dynamic lookup, per-market overrides. Not in this smallest increment.
}

impl Default for FeeModel {
    fn default() -> Self {
        // Conservative/pessimistic for small capital + low volume (per wiki + $150 reality).
        // Over-estimates taker to protect against over-trading on thin edge.
        Self {
            taker_bps: dec!(150), // 1.5% — higher than typical low-volume taker; safe bias
            maker_bps: dec!(20),
            est_gas_usdc_per_tx: dec!(0.01),
            rewards_offset_bps: dec!(10),
        }
    }
}

impl FeeModel {
    /// Construct from legacy flat taker bps (for bootstrap compat with existing paper_fee_bps).
    /// Other fields use conservative defaults (including Default's 150bps taker for non-taker fields).
    ///
    /// NOTE (pessimistic defaults for $150 per fees wiki + Issue 2 review reconcile):
    /// - The 150bps pessimistic taker (and other conservative fields) is in Default.
    /// - This from_flat overrides *only* taker_bps with the passed (often 50 from config default for "typical Polymarket" compat).
    /// - To enforce pessimistic 150+ for default `cargo run` paths: set POLYTRADER_PAPER_FEE_BPS=150 (or higher) explicitly.
    /// - Future wiring (post Tier1) can add max(150, bps) bias or dedicated pessimistic ctor if needed.
    /// - See engine new() and config.rs:28 for legacy default.
    pub fn from_flat_taker_bps(bps: u16) -> Self {
        Self {
            taker_bps: Decimal::from(bps),
            ..Self::default()
        }
    }

    /// Total estimated cost (fee + gas + rewards offset) for a given gross notional.
    /// is_maker: true for limit that provides liquidity (rare in paper market orders).
    pub fn fee_for_notional(&self, notional: Decimal, is_maker: bool) -> Decimal {
        let rate_bps = if is_maker {
            self.maker_bps
        } else {
            self.taker_bps
        };
        let rate = rate_bps / dec!(10000);
        let gross_fee = notional * rate;
        let reward_offset = notional * (self.rewards_offset_bps / dec!(10000));
        let fee_after_rewards = (gross_fee - reward_offset).max(Decimal::ZERO);
        fee_after_rewards + self.est_gas_usdc_per_tx
    }

    /// Compute net edge after fees, gas, and estimated slippage.
    /// gross_edge: expected $ value edge before costs (e.g. fusion-derived or prob*notional).
    /// notional: USD size of the intended position/fill.
    /// slippage_bps: from book impact or synthetic (see engine).
    ///
    /// Returns net (can be negative — do not take the trade).
    /// This becomes the primary signal for deliberate 5-min tier (see strategy + goals wiki).
    pub fn net_edge_after_costs(
        &self,
        gross_edge: Decimal,
        notional: Decimal,
        is_maker: bool,
        slippage_bps: Decimal,
    ) -> Decimal {
        let fees_and_gas = self.fee_for_notional(notional, is_maker);
        let slip_cost = notional * (slippage_bps / dec!(10000));
        gross_edge - fees_and_gas - slip_cost
    }
}
