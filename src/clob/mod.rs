//! Real (authenticated) CLOB client support.
//!
//! See `authenticated.rs` for gated read-only calls and dry-run helpers.
//! See `live_sender.rs` for the fail-closed future sender boundary.

pub mod authenticated;
pub mod live_sender;

// The `pub use` re-exports that used to live here existed for the /console handlers in
// server.rs, removed 2026-08-02 (unused operator UI). The real call site, main.rs, always named
// these through the fully-qualified `crate::clob::authenticated::...` / `crate::clob::live_sender`
// path, never through this shortcut, so nothing else needs it back.
