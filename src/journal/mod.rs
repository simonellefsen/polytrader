//! Journal: immutable(ish) record of decisions, fills, reflections, experiments.
//! Primary data source for Hermes self-improvement.
//!
//! NOTE: writer methods + Reflection appear dead in Phase 0 binary (no direct calls yet outside engine);
//! they are fully implemented and journal all paper actions.
#![allow(dead_code)]

mod models;
mod writer;

pub use writer::JournalWriter;
