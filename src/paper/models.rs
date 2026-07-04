//! Core data models for the paper trading domain (decimals everywhere).

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
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

// FeeModel (flat taker/maker bps + rewards offset) removed 2026-07-04: it was dead code that never
// matched Polymarket's real fee structure. The real model is `fee = shares × rate × p × (1−p)` with
// a per-market rate synced from Gamma (geopolitics fee-free; MAKERS ARE NEVER CHARGED — they earn
// rebates instead) — see `crate::polymarket_fee` and `strategy::FeeContext`. The paper engine costs
// every fill with it directly (we always cross the book, so every fill is a taker fill).
