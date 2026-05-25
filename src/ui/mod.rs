//! Dioxus UI module (Phase 2: rsx is now the live rendered source via SSR).
//! Provides App component (rsx + use_signal + embedded client script for real fetch reactivity).
//! Runtime: dioxus SSR in server.rs dashboard (smallest hydration preserving Phase 0/1 probes/JSON/subpath/<base> exactly; no WASM assets).
//! Data fetched client-side from existing axum JSON endpoints (live updates on cards).

pub mod app;
// Re-export kept minimal; App used via crate::ui::App in future router integration (allow for now to keep skeleton smallest)
#[allow(unused_imports)]
pub use app::App;
