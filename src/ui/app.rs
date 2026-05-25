//! Dioxus App for Phase 2 hydration (real SSR render source + client reactivity).
//! rsx! (with use_signal for initial + structure) is now the actual rendered source of truth
//! (via dioxus SSR in server.rs dashboard_handler). Client live updates via included script +
//! real fetch to preserved JSON endpoints (relative URLs + <base> for subpath compat).
//! Follows exact Dioxus 0.7 + existing patterns; no WASM bundle yet (smallest, deploy preserved).
//!
//! AUTH (2026-05-25 Next Phase IMPL 5701dfea): smallest static rsx links + chip placeholder
//!   + enhancement to *existing* client script for /auth/whoami fetch (fits live-fetch pattern
//!   exactly). No App sig change, no new signals, no SSR string hacks (avoids past brittle issue).
//!   Links relative so <base> resolves under /polytrader subpath. Foundation for future per-user
//!   personalization of paper bankroll/journal. Heavy comments. Credits to AGENTS + deploy history.

use dioxus::prelude::*;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct SimpleMarket {
    slug: String,
    question: String,
    last_mid_yes: Option<String>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct SimplePortfolio {
    virtual_usdc: String,
    unrealized_pnl: String,
}

#[component]
pub fn App() -> Element {
    // Reactive signals (initial state for SSR render). Real client reactivity + live updates
    // now provided by the <script> below (real fetch to /markets etc under <base>); full
    // WASM signals hydration is future (features already enabled, no bloat here).
    let refresh_count = use_signal(|| 0u32);
    let last_refresh = use_signal(|| "initial SSR (Phase 2 rsx)".to_string());
    let markets = use_signal(|| {
        vec![SimpleMarket {
            slug: "demo-market".to_string(),
            question: "Demo: Will sample resolve Yes?".to_string(),
            last_mid_yes: Some("0.52".to_string()),
        }]
    });
    let portfolio = use_signal(|| SimplePortfolio {
        virtual_usdc: "10000.00000000".to_string(),
        unrealized_pnl: "0.00000000".to_string(),
    });

    // Signals provide initial SSR state only (client live updates driven by the <script> in rsx output).
    // (Dead closure removed for Phase 2 static onclick + script; keeps compile/clippy clean.)

    let portfolio_json =
        serde_json::to_string_pretty(&*portfolio.read()).unwrap_or_else(|_| "{}".to_string());

    rsx! {
        head {
            title { "polytrader — Dioxus Phase 2 (SSR + Gated Hermes)" }
            // base href injected by server layer for /polytrader subpath rewrite compat (preserves Phase 0 verified behavior)
            style { "body {{ font-family: system-ui, sans-serif; margin: 2rem; background: #0b0c10; color: #eee; }} .banner {{ background: #c00; color: white; padding: 1rem; font-weight: bold; border-radius: 4px; }} .card {{ background: #16181f; padding: 1rem; margin: 1rem 0; border-radius: 6px; }} pre {{ background:#111; padding:0.5rem; }} button {{ padding: 0.5rem 1rem; }} table {{ border-collapse: collapse; }} td,th {{ border:1px solid #333; padding:4px; }} .auth {{ float: right; font-size: 0.9em; }}" }
        }
        body {
            h1 { "polytrader — Phase 2 (Dioxus SSR + Gated Hermes)" }
            div { class: "banner",
                "⚠️ PAPER TRADING ONLY — REAL MONEY TRADING DISABLED. Simulation using live public Polymarket data. Dioxus skeleton active."
            }

            // AUTH (Next Phase): smallest static links + placeholder (populated by existing script pattern).
            // Relative URLs resolve correctly under <base href="/polytrader/">. Dual with edge SSO.
            // Future: per-user paper bankroll attribution / journal once identity wired.
            div { class: "auth card",
                a { href: "/auth/login", "Login with Google" }
                span { id: "user-chip", style: "margin-left: 1rem;", "" }
                small { " (in-app Google OAuth, dual with ngrok edge SSO)" }
            }

            div { class: "card",
                h2 { "Live Snapshot (Dioxus reactive - Phase 2 SSR)" }
                p { "Active markets tracked: (see /markets endpoint or list below)" }
                p { "Latest virtual USDC: " strong { id: "usdc-val", "{portfolio.read().virtual_usdc}" } }
                p { "Unrealized PnL demo: " strong { id: "pnl-val", "{portfolio.read().unrealized_pnl}" } }
                p {
                    "Last refresh: " strong { id: "last-refresh", "{last_refresh.read().clone()}" }
                    button { "onclick": "refreshDemo()", "Live Refresh (real fetch to endpoints + update)" }
                }
                p { "Refresh clicks: " strong { id: "click-count", "{refresh_count}" } " (client reactivity via script)" }
            }

            div { class: "card",
                h2 { "Markets (fetched demo / live via endpoint)" }
                ul { id: "markets-list",
                    for m in markets.read().iter() {
                        li {
                            "{m.slug}: {m.question} - mid Yes approx "
                            "{m.last_mid_yes.as_deref().unwrap_or(\"?\")}"
                        }
                    }
                }
                small { "Real data: curl /markets (live client fetch updates this)" }
            }

            div { class: "card",
                h2 { "Portfolio Snapshot" }
                pre { "{portfolio_json}" }
            }

            div { class: "card",
                h2 { "Safety & Status (Dioxus)" }
                ul {
                    li { "Mode: paper (enforced)" }
                    li { "All activity journaled (paper_trading + journal schemas)" }
                    li { "Dioxus 0.7 fullstack SSR (rsx in this file is the rendered source) + client fetch reactivity (Phase 2)" }
                    li { "Hermes richer + autonomous low-risk wiki proposals (gated) in separate deployment" }
                    li { "In-app Google OAuth (dual with ngrok edge SSO) — foundation for future per-user paper features" }
                }
            }

            p {
                small { "polytrader v" { env!("CARGO_PKG_VERSION") } " — Dioxus 0.7 SSR + axum hybrid (rsx source of truth; live client updates)" }
            }

            // Client-side real fetch + reactivity script (included in SSR output from rsx).
            // Does live updates to dashboard cards (targets ids) using the preserved JSON endpoints.
            // Relative URLs + <base> from server wrapper ensure subpath /polytrader/* compat.
            // This delivers "clean client fetch + reactivity" without WASM bundle (smallest viable).
            // AUTH: also fetches /auth/whoami (new endpoint) to populate #user-chip (fits exact pattern).
            script {
                r#"
                let count = 0;
                function refreshDemo() {{
                    count++;
                    const now = new Date().toISOString();
                    const lastEl = document.getElementById('last-refresh');
                    if (lastEl) lastEl.textContent = 'refreshed #' + count + ' at ' + now;
                    const clickEl = document.getElementById('click-count');
                    if (clickEl) clickEl.textContent = count;
                    // Real client fetch + update (live reactivity for Phase 2)
                    fetch('markets').then(r => r.json()).then(data => {{
                        const ul = document.getElementById('markets-list');
                        if (ul && data && data.length) {{
                            ul.innerHTML = data.slice(0,3).map(m => '<li>' + (m.slug || 'm') + ': live from /markets</li>').join('');
                        }}
                    }}).catch(() => {{ /* graceful */ }});
                    fetch('paper/portfolio').then(r => r.json()).then(p => {{
                        const usdcEl = document.getElementById('usdc-val');
                        if (usdcEl && p && p.virtual_usdc) usdcEl.textContent = p.virtual_usdc;
                        const pnlEl = document.getElementById('pnl-val');
                        if (pnlEl && p && p.unrealized_pnl) pnlEl.textContent = p.unrealized_pnl;
                    }}).catch(() => {{ /* graceful */ }});
                    // AUTH: populate user chip from whoami (smallest addition, same pattern)
                    updateAuthChip();
                }}
                function updateAuthChip() {{
                    const chip = document.getElementById('user-chip');
                    if (!chip) return;
                    fetch('auth/whoami').then(r => r.json()).then(d => {{
                        if (d && d.user) {{
                            chip.innerHTML = 'Signed in as <strong>' + d.user + '</strong> | <a href=\"auth/logout\">Logout</a>';
                        }} else {{
                            chip.innerHTML = '<a href=\"auth/login\">Login with Google</a>';
                        }}
                    }}).catch(() => {{ chip.innerHTML = '<a href=\"auth/login\">Login with Google</a>'; }});
                }}
                // Auto one refresh on load for live feel (non-blocking)
                setTimeout(refreshDemo, 1200);
                setTimeout(updateAuthChip, 800);
                "#
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_structs_serde_and_defaults() {
        // Initial real test coverage for Phase 2 UI data paths (replaces TODO scaffolding)
        let m = SimpleMarket {
            slug: "test-slug".to_string(),
            question: "Will X happen?".to_string(),
            last_mid_yes: Some("0.42".to_string()),
        };
        let j = serde_json::to_string(&m).expect("serde");
        assert!(j.contains("test-slug"));
        assert!(j.contains("0.42"));

        let p = SimplePortfolio {
            virtual_usdc: "12345.00000000".to_string(),
            unrealized_pnl: "-10.50000000".to_string(),
        };
        let jp = serde_json::to_string(&p).expect("serde");
        assert!(jp.contains("12345"));
    }

    #[test]
    fn test_ssr_render_hydration_fidelity_phase2() {
        // Meaningful coverage for Phase 2 SSR hydration (rsx as source, Phase 2 strings, ids for client reactivity, script presence).
        // Exercises VirtualDom + dioxus_ssr::render path used by server dashboard_handler.
        let mut vdom = VirtualDom::new(App);
        vdom.rebuild_in_place();
        let rendered = dioxus_ssr::render(&vdom);
        assert!(
            rendered.contains("Phase 2 (Dioxus SSR + Gated Hermes)"),
            "title/h1 must reflect Phase 2 branding"
        );
        assert!(
            rendered.contains("id=\"usdc-val\""),
            "ids for live client fetch/DOM update must be present"
        );
        assert!(
            rendered.contains("id=\"markets-list\""),
            "ids for live client fetch/DOM update must be present"
        );
        assert!(
            rendered.contains("refreshDemo"),
            "client reactivity script (real fetch) must be rendered in output"
        );
        assert!(
            rendered.to_lowercase().contains("autonomous"),
            "Hermes gated proposal context in safety card"
        );
        // Note: full <base> injection + wrapper tested via integration in server (string post-process); this covers core rsx SSR fidelity.
        // AUTH note: whoami/login strings are in static rsx + script (tested via presence in full e2e if needed).
    }
}
