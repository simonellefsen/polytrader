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
#[allow(unused_imports)]
pub use models::{
    OrderSide, OrderStatus, OrderType, PaperFill, PaperOrder, PaperPosition, VirtualPortfolio,
};
