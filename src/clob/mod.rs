//! Real (authenticated) CLOB client support.
//!
//! See `authenticated.rs` for gated read-only calls and dry-run helpers.
//! See `live_sender.rs` for the fail-closed future sender boundary.

pub mod authenticated;
pub mod live_sender;

// Re-export the main client for convenience
pub use authenticated::{RealClobClient, RealOrderIntentDryRun};
// Re-export live sender types so call sites (server, tests) can name the gated impl.
pub use live_sender::{GatedRealClobLiveOrderSender, LiveOrderSendRequest};
