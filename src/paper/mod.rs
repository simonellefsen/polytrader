//! PaperTradingEngine — the heart of safe simulation.
//!
//! High-fidelity matching against live public orderbook snapshots.
//! Produces the same journal artifacts as the future real adapter.
//!
//! NOTE: many items appear dead_code in Phase 0 (no call sites for submit yet);
//! they are wired and will be exercised by dashboard/tests/strategy soon.
#![allow(dead_code)]

mod engine;
mod models;

pub use engine::PaperTradingEngine;
// Basket pre-flight types (P5 increment 3). Callers reach these through `plan_basket`'s return
// value rather than by name, so they are re-exported for the module's public surface, not for a
// current import site — same reason the models below carry the attribute.
#[allow(unused_imports)]
pub use engine::{BasketLegPlan, BasketPlan, BookSource};
#[allow(unused_imports)]
pub use models::{
    OrderSide, OrderStatus, OrderType, PaperFill, PaperOrder, PaperPosition, VirtualPortfolio,
};
