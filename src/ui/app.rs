//! Dioxus App for Phase 2 hydration (real SSR render source + client reactivity).
//! rsx! (with use_signal for initial + structure) is now the actual rendered source of truth
//! (via dioxus SSR in server.rs dashboard_handler). Client live updates via included script +
//! real fetch to preserved JSON endpoints (relative URLs + <base> for subpath compat).
//! Follows exact Dioxus 0.7 + existing patterns; no WASM bundle yet (smallest, deploy preserved).
//!
//! AUTH (2026-05-25 Next Phase IMPL 5701dfea): smallest static rsx links + chip placeholder
//! + enhancement to *existing* client script for /auth/whoami fetch (fits live-fetch pattern
//!   exactly). No App sig change, no new signals, no SSR string hacks (avoids past brittle issue).
//!
//! Links relative so <base> resolves under /polytrader subpath. Foundation for future per-user
//! personalization of paper bankroll/journal. Heavy comments. Credits to AGENTS + deploy history.

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
            style { "body {{ font-family: system-ui, sans-serif; margin: 2rem; background: #0b0c10; color: #eee; }} .banner {{ background: #c00; color: white; padding: 1rem; font-weight: bold; border-radius: 4px; }} .card {{ background: #16181f; padding: 1rem; margin: 1rem 0; border-radius: 6px; }} pre {{ background:#111; padding:0.5rem; }} button {{ padding: 0.5rem 1rem; }} table {{ border-collapse: collapse; }} td,th {{ border:1px solid #333; padding:4px; }} .auth {{ float: right; font-size: 0.9em; }} .auth a {{ color: #66b3ff; text-decoration: underline; }} .auth a:hover {{ color: #99ccff; }} .auth .l2 {{ margin-top: 4px; font-size: 0.85em; }}" }
        }
        body {
            h1 { "polytrader — Phase 2 (Dioxus SSR + Gated Hermes)" }
            div { class: "banner",
                "⚠️ PAPER TRADING ONLY — REAL MONEY TRADING DISABLED. Simulation using live public Polymarket data. Dioxus skeleton active."
            }

            // Primary auth is now Polymarket L2 for trading.
            // Google is legacy (dashboard identity only).
            div { class: "auth card",
                // Polymarket L2 (primary for trading)
                span { class: "l2", style: "font-weight: bold;", "Polymarket L2: " }
                span { id: "l2-chip", class: "l2", style: "margin-left: 0.5rem; font-family: monospace;", "Not connected (paper)" }
                span { id: "clob-chip", class: "l2", style: "margin-left: 0.5rem; font-family: monospace; opacity: 0.75;", "CLOB account: idle" }
                span { id: "preflight-chip", class: "l2", style: "margin-left: 0.5rem; font-family: monospace; opacity: 0.75;", "Preflight: idle" }
                button { "onclick": "deriveL2FromServerKey()", class: "l2", style: "margin-left: 0.5rem; font-size: 0.85em;", "Derive from Server Key" }
                button { "onclick": "disconnectL2()", class: "l2", style: "margin-left: 0.25rem; font-size: 0.85em;", "Disconnect" }
                small { class: "l2", style: "margin-left: 6px; opacity: 0.6;", "(auto on startup when key present)" }

                // Google kept only for edge SSO status (very small, no direct login link)
                small { style: "opacity: 0.5; font-size: 0.7em; margin-left: 8px;", id: "google-status", "" }
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
                h2 { "CLOB Readiness" }
                pre { id: "clob-readiness-panel", "No CLOB readiness loaded" }
                small { "Read-only authenticated CLOB diagnostic. Paper-only: no order placement, cancellation, or allowance mutation." }
            }

            div { class: "card",
                h2 { "CLOB Diagnostics" }
                pre { id: "clob-diagnostics-panel", "No CLOB diagnostics loaded" }
                small { "Aggregate read-only CLOB diagnostic: status, account, and preflight from one safe call." }
            }

            div { class: "card",
                h2 { "CLOB Operator Status" }
                pre { id: "clob-operator-status-panel", "No CLOB operator status loaded" }
                div { id: "clob-operator-status-actions" }
                small { "Read-only rollup of authenticated CLOB diagnostics plus paper dry-run review health." }
            }

            div { class: "card",
                h2 { "CLOB Order Placement Readiness" }
                pre { id: "clob-order-placement-readiness-panel", "No order placement readiness loaded" }
                small { "Read-only gap report for future real-order placement. This does not enable, sign, submit, or place orders." }
            }

            div { class: "card",
                h2 { "Real Trading Unlock Status" }
                pre { id: "clob-real-trading-unlock-panel", "No real trading unlock status loaded" }
                small { "Read-only explicit unlock report. This never enables live trading or creates a live sender." }
            }

            div { class: "card",
                h2 { "Live Sender Design Readiness" }
                pre { id: "clob-live-sender-design-panel", "No live sender design readiness loaded" }
                small { "Read-only design readiness package. This does not create a live sender or enable trading." }
            }

            div { class: "card",
                h2 { "Live Sender Design Review" }
                pre { id: "clob-live-sender-design-review-panel", "No live sender design review loaded" }
                small { "Read-only ADR-style contract for a future live sender. This does not implement or enable one." }
            }

            div { class: "card",
                h2 { "Live Sender Boundary Status" }
                pre { id: "clob-live-sender-boundary-panel", "No live sender boundary status loaded" }
                small { "Read-only fail-closed sender boundary. The only implementation rejects before network dispatch." }
            }

            div { class: "card",
                h2 { "Final Review Readiness" }
                pre { id: "clob-final-review-readiness-panel", "No final review readiness loaded" }
                button { "onclick": "recordFinalReviewDecision()", "Record Blocked Review" }
                pre { id: "clob-final-review-decision-panel", "No final review decision recorded" }
                small { "Read-only aggregate of latest journaled CLOB gate evidence. This does not approve or place orders." }
            }

            div { class: "card",
                h2 { "Final Review Decisions" }
                pre { id: "clob-final-review-decisions-summary", "No final review decisions loaded" }
                table {
                    thead {
                        tr {
                            th { "Time" }
                            th { "Decision" }
                            th { "Effect" }
                            th { "Approved" }
                            th { "Boundary" }
                            th { "Dispatch" }
                            th { "Operator" }
                            th { "Use ID" }
                        }
                    }
                    tbody { id: "clob-final-review-decisions-list",
                        tr {
                            td { colspan: "8", "No final review decisions loaded" }
                        }
                    }
                }
                small { "Read-only audit history for final-review decisions (enriched with snapshots). Use journal_event_id as final_review_decision_event_id (with human id) for submit-facade gated real path under unlocks. These do not auto-approve." }
            }

            // 2026-06-03: Pending human approvals list (for operator to inspect recent approvals + copy UUIDs for submit-facade / real gated).
            // Evidence includes snapshots at approve time. Approve action itself is via "Record Facade Approval" (which now captures snapshots).
            // "Copy ID" populates window var used by submit button and shows guidance.
            // 2026-06-06 UI polish (additive): th + note text enhanced for richer evidence + Hermes attribution cross-ref (see Hermes panel); all old ids/hooks/markers ("clob-human-approvals-*", "Pending / Recent...", "Refresh...", "Copy/Use ID...", onclick updateHuman.../recordHuman...) preserved exactly.
            div { class: "card",
                h2 { "Pending / Recent Human Approvals (for Gated Real CLOB)" }
                pre { id: "clob-human-approvals-summary", "No human approvals loaded" }
                table {
                    thead {
                        tr {
                            th { "Time" }
                            th { "Decision" }
                            th { "Approved" }
                            th { "Operator" }
                            th { "Risk/Coll Snapshot Summary (enriched)" }
                            th { "Action" }
                        }
                    }
                    tbody { id: "clob-human-approvals-list",
                        tr {
                            td { colspan: "6", "No human approvals loaded" }
                        }
                    }
                }
                button { "onclick": "updateHumanApprovalsList()", "Refresh Human Approvals List" }
                small { id: "clob-human-approvals-note", "Recent clob_order_human_approval events (enriched 2026-06-03 with approve-time risk/collateral snapshots). Copy journal_event_id as human_approval_event_id for submit-facade. Use with final id + unlocks for real gated path. Read-only. Hermes approval attribution hints (approvals_with_snapshots_24h/hermes_approval_gap etc from 2026-06-06 closed-loop) in adjacent Hermes CLOB Safety Loop panel + appended here on load." }
            }

            div { class: "card",
                h2 { "Hermes CLOB Safety Loop" }
                pre { id: "clob-hermes-safety-loop-panel", "No Hermes CLOB safety loop loaded" }
                small { "Read-only latest Hermes reflection over CLOB safety-loop journal events. This does not approve or place orders." }
                // 2026-06-07 additive (inside existing card only; reuses fetch + updateHermesSafetyLoop + panel id; no new panel/route/id/hook): small static text for SSR test coverage of new "Recent Decision Reports..." strings + disclaimers (skeleton + static pre lines for DR/tax (siblings in stored reflection.metrics from hermes do_reflection; server build_hermes_safety_loop_response currently promotes only clob_safety_loop scalars + limited reflection sub/summary so d. fallbacks; full top-level for "live" is future when endpoint extended per goals 'backtest harness on DRs vs paper fills + tax-adjusted with real join/attr'); ties to "Risk/Coll Snapshot Summary (enriched)" style; all old markers/ids/hooks/SSR contains for "Hermes CLOB Safety Loop" + id + update + "PAPER TRADING ONLY" + paper_only + real_orders_enabled===false + <base> + "Risk/Coll..." + "Hermes attr: snaps=" + hasSnap + clob-*-panel + update*/record* + "Pending..." + "Copy/Use..." + l2-chip etc preserved *exact*; "skeleton vs production" per hermes current + plan "local cargo sufficient").
                small { "Recent Decision Reports (5-min DR cadence) + tax + provenance to approvals/DRs (skeleton + static pre lines (DR/tax siblings in reflection.metrics; current server build promotes clob_safety_loop scalars + reflection sub only; future live when extended per goals); DR net_edge_after_fees (PRIMARY) + generated_by + ids for approvals tie + dr_vs_paper_fills_compare proxy lens; paper proxy only / skeleton vs production / observe pre-dispatch + DRs + tax + fills samples in next hermes reflection" }
            }

            div { class: "card",
                h2 { "CLOB Account" }
                pre { id: "clob-account-panel", "No CLOB account snapshot loaded" }
                small { "Read-only authenticated account snapshot. Shows visibility of open orders and collateral/allowance fields only." }
            }

            div { class: "card",
                h2 { "CLOB Preflight" }
                pre { id: "clob-preflight-panel", "No CLOB preflight loaded" }
                small { "Diagnostic-only blocker report for future real-order gates. This does not enable real trading." }
            }

            div { class: "card",
                h2 { "CLOB Collateral Readiness" }
                pre { id: "clob-collateral-readiness-panel", "No CLOB collateral readiness loaded" }
                small { "Read-only collateral and allowance blocker report. No funding, approval, or balance mutation is performed." }
            }

            div { class: "card",
                h2 { "CLOB Dry-Run Intent" }
                div {
                    label { "Token " input { id: "dry-token", placeholder: "CLOB token_id", value: "123" } }
                    label { style: "margin-left: 0.5rem;", "Side "
                        select { id: "dry-side",
                            option { value: "buy", "buy" }
                            option { value: "sell", "sell" }
                        }
                    }
                    label { style: "margin-left: 0.5rem;", "Type "
                        select { id: "dry-type",
                            option { value: "limit", "limit" }
                            option { value: "market", "market" }
                        }
                    }
                    label { style: "margin-left: 0.5rem;", "Size " input { id: "dry-size", value: "1" } }
                    label { style: "margin-left: 0.5rem;", "Price " input { id: "dry-price", value: "0.5" } }
                    label { style: "margin-left: 0.5rem;", "Edge bps " input { id: "dry-edge", value: "500" } }
                    button { "onclick": "submitDryRunIntent()", style: "margin-left: 0.5rem;", "Dry Run" }
                    button { "onclick": "validateMarketMetadataIntent()", style: "margin-left: 0.5rem;", "Validate Market Metadata" }
                    button { "onclick": "submitSignatureDryRunIntent()", style: "margin-left: 0.5rem;", "Signed Payload Dry Run" }
                    button { "onclick": "submitPostRequestDryRunIntent()", style: "margin-left: 0.5rem;", "POST Request Dry Run" }
                    button { "onclick": "recordHumanApprovalIntent()", style: "margin-left: 0.5rem;", "Record Facade Approval" }
                    button { "onclick": "submitOrderFacadeIntent()", style: "margin-left: 0.5rem;", "Submit Facade Check" }
                }
                pre { id: "dry-run-result", "No dry-run submitted from UI" }
            }

            div { class: "card",
                h2 { "Recent CLOB Dry-Runs" }
                table {
                    thead {
                        tr {
                            th { "Time" }
                            th { "Market" }
                            th { "Notional" }
                            th { "Blockers" }
                            th { "Review" }
                        }
                    }
                    tbody { id: "dry-runs-list",
                        tr {
                            td { colspan: "5", "No dry-runs loaded" }
                        }
                    }
                }
                small { "Read-only audit summary plus paper-only operator review decisions from journal.events" }
            }

            div { class: "card",
                h2 { "Recent CLOB Reviews" }
                table {
                    thead {
                        tr {
                            th { "Time" }
                            th { "Decision" }
                            th { "Market" }
                            th { "Operator" }
                            th { "Note" }
                        }
                    }
                    tbody { id: "reviews-list",
                        tr {
                            td { colspan: "5", "No reviews loaded" }
                        }
                    }
                }
                small { id: "reviews-summary", "Read-only review audit from journal.events" }
            }

            div { class: "card",
                h2 { "CLOB Review Summary" }
                pre { id: "review-summary-panel", "No review summary loaded" }
                small { "Read-only coverage and blocker summary for recent dry-runs" }
            }

            div { class: "card",
                h2 { "CLOB Review Health" }
                pre { id: "review-health-panel", "No review health loaded" }
                div { id: "review-health-actions" }
                small { "Read-only rollup of recent review coverage, guidance, and latency signals" }
            }

            div { class: "card",
                h2 { "CLOB Review Backlog" }
                pre { id: "review-backlog-panel", "No review backlog loaded" }
                small { "Read-only freshness signal for unreviewed dry-runs" }
            }

            div { class: "card",
                h2 { "CLOB Guidance Exceptions" }
                table {
                    thead {
                        tr {
                            th { "Time" }
                            th { "Decision" }
                            th { "Guidance" }
                            th { "Market" }
                            th { "Detail" }
                        }
                    }
                    tbody { id: "guidance-exceptions-list",
                        tr {
                            td { colspan: "5", "No guidance exceptions loaded" }
                        }
                    }
                }
                small { "Reviewed dry-runs whose latest paper review differs from conservative guidance" }
            }

            div { class: "card",
                h2 { "CLOB Guidance Overrides" }
                table {
                    thead {
                        tr {
                            th { "Time" }
                            th { "Decision" }
                            th { "Guidance" }
                            th { "Operator" }
                            th { "Note" }
                            th { "Detail" }
                        }
                    }
                    tbody { id: "guidance-overrides-list",
                        tr {
                            td { colspan: "6", "No guidance overrides loaded" }
                        }
                    }
                }
                small { "Historical paper reviews that overrode conservative guidance" }
            }

            div { class: "card",
                h2 { "CLOB Review Queue" }
                table {
                    thead {
                        tr {
                            th { "Time" }
                            th { "Age" }
                            th { "Market" }
                            th { "Notional" }
                            th { "Blockers" }
                            th { "Review" }
                        }
                    }
                    tbody { id: "review-queue-list",
                        tr {
                            td { colspan: "6", "No review queue loaded" }
                        }
                    }
                }
                small { "Oldest unreviewed dry-runs from journal.events; actions record paper-only review events" }
            }

            div { class: "card",
                h2 { "CLOB Dry-Run Detail" }
                pre { id: "dry-run-detail-panel", "Select a dry-run to inspect its audit detail" }
                small { "Read-only drilldown for one dry-run event and its paper-only reviews" }
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
                function escapeHtml(unsafe) {{
                    if (unsafe == null) return '';
                    return String(unsafe)
                        .replace(/&/g, "&amp;")
                        .replace(/</g, "&lt;")
                        .replace(/>/g, "&gt;")
                        .replace(/"/g, "&quot;")
                        .replace(/'/g, "&#039;");
                }}
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
                // Google auth status is now very de-emphasized (edge SSO only)
                // setTimeout(updateAuthChip, 800);  // intentionally reduced
                setTimeout(updateL2Chip, 600);
                setTimeout(updateClobChip, 1100);
                setTimeout(updateClobReadiness, 1300);
                setTimeout(updateClobDiagnosticsPanel, 1350);
                setTimeout(updateClobOperatorStatus, 1375);
                setTimeout(updateOrderPlacementReadiness, 1385);
                setTimeout(updateRealTradingUnlockStatus, 1390);
                setTimeout(updateLiveSenderDesignReadiness, 1393);
                setTimeout(updateLiveSenderDesignReview, 1394);
                setTimeout(updateLiveSenderBoundaryStatus, 1394);
                setTimeout(updateFinalReviewReadiness, 1395);
                setTimeout(updateFinalReviewDecisions, 1397);
                setTimeout(updateHumanApprovalsList, 1398);  // 2026-06-03 pending human approvals list + copy ids
                setTimeout(updateHermesSafetyLoop, 1399);
                setTimeout(updateClobAccountPanel, 1400);
                setTimeout(updatePreflightChip, 1500);
                setTimeout(updatePreflightPanel, 1600);
                setTimeout(updateDryRunsList, 1700);
                setTimeout(updateReviewsList, 1900);
                setTimeout(updateReviewSummary, 2100);
                setTimeout(updateReviewHealth, 2200);
                setTimeout(updateReviewBacklog, 2300);
                setTimeout(updateGuidanceExceptions, 2400);
                setTimeout(updateGuidanceOverrides, 2500);
                setTimeout(updateReviewQueue, 2700);

                // L2 Polymarket status (uses existing live fetch pattern)
                function updateL2Chip() {{
                    const el = document.getElementById('l2-chip');
                    if (!el) return;
                    fetch('l2/status').then(r => r.json()).then(d => {{
                        if (d && d.connected) {{
                            const base = 'Connected ' + (d.api_key_masked || '');
                            // Detect server-key derivation (from auto on startup or manual server key button)
                            if (d.address === 'server-key' || (d.note && d.note.includes('server-side'))) {{
                                el.innerHTML = base + ' <span style="font-size:0.75em;opacity:0.7;">(server key • auto)</span>';
                            }} else {{
                                el.innerHTML = base;
                            }}
                            el.style.color = '#66b3ff';
                            updateClobChip();
                            updateClobReadiness();
                            updateClobDiagnosticsPanel();
                            updateClobOperatorStatus();
                            updateRealTradingUnlockStatus();
                            updateFinalReviewReadiness();
                            updateClobAccountPanel();
                            updatePreflightChip();
                            updatePreflightPanel();
                            updateCollateralReadinessPanel();
                        }} else {{
                            el.innerHTML = 'Not connected (paper)';
                            el.style.color = '#aaa';
                            updateClobReadiness();
                            updateClobDiagnosticsPanel();
                            updateClobOperatorStatus();
                            updateRealTradingUnlockStatus();
                            updateFinalReviewReadiness();
                            updateClobAccountPanel();
                            updatePreflightPanel();
                            updateCollateralReadinessPanel();
                        }}
                    }}).catch(() => {{
                        el.innerHTML = 'Not connected (paper)';
                        updateClobReadiness();
                        updateClobDiagnosticsPanel();
                        updateClobOperatorStatus();
                        updateRealTradingUnlockStatus();
                        updateFinalReviewReadiness();
                        updateClobAccountPanel();
                        updatePreflightPanel();
                        updateCollateralReadinessPanel();
                    }});
                }}

                function updateClobReadiness() {{
                    const panel = document.getElementById('clob-readiness-panel');
                    if (!panel) return;
                    fetch('clob/status').then(r => r.json()).then(d => {{
                        const orders = d && d.open_orders ? d.open_orders : {{}};
                        let openCount = '?';
                        if (Array.isArray(orders.data)) {{
                            openCount = orders.data.length;
                        }} else if (typeof orders.count === 'number') {{
                            openCount = orders.count;
                        }} else if (Array.isArray(orders)) {{
                            openCount = orders.length;
                        }}
                        const ready = !!(d && d.l2_connected && d.read_only_live_check && d.paper_only && d.real_orders_enabled === false);
                        const lines = [
                            'l2_connected: ' + !!(d && d.l2_connected),
                            'read_only_live_check: ' + !!(d && d.read_only_live_check),
                            'paper_only: ' + !!(d && d.paper_only),
                            'real_orders_enabled: ' + !!(d && d.real_orders_enabled),
                            'open_orders.count: ' + openCount
                        ];
                        if (d && d.error) lines.push('error: ' + d.error);
                        if (d && d.note) lines.push('note: ' + d.note);
                        panel.textContent = lines.join('\\n');
                        panel.style.color = ready ? '#66d98f' : '#ffb366';
                    }}).catch(() => {{
                        panel.textContent = 'CLOB readiness unavailable';
                        panel.style.color = '#ffb366';
                    }});
                }}

                function updateClobChip() {{
                    const el = document.getElementById('clob-chip');
                    if (!el) return;
                    fetch('clob/account').then(r => r.json()).then(d => {{
                        if (d && d.read_only_live_check) {{
                            const account = d.account || {{}};
                            const orders = account.open_orders || {{}};
                            const count = Array.isArray(orders && orders.data)
                                ? orders.data.length
                                : (Array.isArray(orders) ? orders.length : 0);
                            const collateralVisible = !!(account.collateral && Object.prototype.hasOwnProperty.call(account.collateral, 'balance'));
                            el.innerHTML = 'CLOB account: OK (' + count + ' open' + (collateralVisible ? ', collateral visible' : '') + ')';
                            el.style.color = '#66d98f';
                        }} else if (d && d.l2_connected) {{
                            el.innerHTML = 'CLOB account: failed';
                            el.style.color = '#ffb366';
                        }} else {{
                            el.innerHTML = 'CLOB account: waiting for L2';
                            el.style.color = '#aaa';
                        }}
                    }}).catch(() => {{
                        el.innerHTML = 'CLOB account: unavailable';
                        el.style.color = '#ffb366';
                    }});
                }}

                function updateClobDiagnosticsPanel() {{
                    const panel = document.getElementById('clob-diagnostics-panel');
                    if (!panel) return;
                    fetch('clob/diagnostics').then(r => r.json()).then(d => {{
                        const status = d && d.status ? d.status : {{}};
                        const accountSection = d && d.account ? d.account : {{}};
                        const account = accountSection.account || {{}};
                        const openOrders = status.open_orders || account.open_orders || {{}};
                        let openCount = '?';
                        if (Array.isArray(openOrders.data)) {{
                            openCount = openOrders.data.length;
                        }} else if (typeof openOrders.count === 'number') {{
                            openCount = openOrders.count;
                        }} else if (Array.isArray(openOrders)) {{
                            openCount = openOrders.length;
                        }}
                        const collateral = account.collateral || {{}};
                        const allowances = collateral.allowances || {{}};
                        const allowanceEntries = Object.entries(allowances);
                        const positiveAllowanceCount = allowanceEntries.filter(([, v]) => {{
                            const n = Number(v);
                            return Number.isFinite(n) && n > 0;
                        }}).length;
                        const preflight = d && d.preflight ? d.preflight : {{}};
                        const blockers = Array.isArray(preflight.blockers) ? preflight.blockers : [];
                        const lines = [
                            'l2_connected: ' + !!(d && d.l2_connected),
                            'read_only_live_check: ' + !!(d && d.read_only_live_check),
                            'paper_only: ' + !!(d && d.paper_only),
                            'real_orders_enabled: ' + !!(d && d.real_orders_enabled),
                            'ready_for_real_orders: ' + !!(d && d.ready_for_real_orders),
                            'open_orders.count: ' + openCount,
                            'allowance_entries: ' + allowanceEntries.length,
                            'positive_allowance_entries: ' + positiveAllowanceCount,
                            'blockers: ' + (blockers.length ? blockers.join(', ') : 'none')
                        ];
                        if (d && d.error) lines.push('error: ' + d.error);
                        if (d && d.note) lines.push('note: ' + d.note);
                        panel.textContent = lines.join('\\n');
                        panel.style.color = d && d.read_only_live_check && !d.ready_for_real_orders ? '#ffb366' : '#66d98f';
                    }}).catch(() => {{
                        panel.textContent = 'CLOB diagnostics unavailable';
                        panel.style.color = '#ffb366';
                    }});
                }}

                function updateClobOperatorStatus() {{
                    const panel = document.getElementById('clob-operator-status-panel');
                    const actionsEl = document.getElementById('clob-operator-status-actions');
                    if (!panel) return;
                    fetch('clob/operator-status?limit=50').then(r => r.json()).then(d => {{
                        if (!d || d.error) {{
                            panel.textContent = 'CLOB operator status unavailable: ' + ((d && d.error) || 'unknown error');
                            if (actionsEl) actionsEl.innerHTML = '';
                            return;
                        }}
                        const clob = d && d.clob ? d.clob : {{}};
                        const review = d && d.review ? d.review : {{}};
                        const finalReview = d && d.final_review ? d.final_review : {{}};
                        const finalReviewAudit = finalReview.audit || {{}};
                        const finalReviewGapProbe = finalReview.coverage_gap_probe || {{}};
                        const finalReviewHermesAlignment = finalReview.hermes_gap_alignment || {{}};
                        const hermes = d && d.hermes_safety_loop ? (d.hermes_safety_loop.latest || {{}}) : {{}};
                        const hermesCoverage = hermes.final_review_decision_boundary_coverage || {{}};
                        const liveSenderBoundary = d && d.live_sender_boundary ? d.live_sender_boundary : {{}};
                        const health = review.health || {{}};
                        const summary = review.summary || {{}};
                        const blockers = Array.isArray(clob.blockers) ? clob.blockers : [];
                        const reasons = Array.isArray(health.reasons) ? health.reasons : [];
                        const actions = Array.isArray(d && d.recommended_next_actions) ? d.recommended_next_actions : [];
                        const actionSummary = (d && d.action_summary) || {{}};
                        const freshness = (d && d.freshness) || {{}};
                        const actionText = actions.length ? actions.map(a => (a.id || 'action') + ': ' + (a.label || '')).join(', ') : 'none';
                        const lines = [
                            'operator_status: ' + ((d && d.operator_status) || 'unknown'),
                            'l2_connected: ' + !!(d && d.l2_connected),
                            'read_only_live_check: ' + !!(d && d.read_only_live_check),
                            'paper_only: ' + !!(d && d.paper_only),
                            'real_orders_enabled: ' + !!(d && d.real_orders_enabled),
                            'ready_for_real_orders: ' + !!(d && d.ready_for_real_orders),
                            'clob.blockers: ' + (blockers.length ? blockers.join(', ') : 'none'),
                            'review.status: ' + (health.status || 'unknown'),
                            'review.reasons: ' + (reasons.length ? reasons.join(', ') : 'none'),
                            'dry-runs: ' + (summary.dry_run_count || 0),
                            'unreviewed: ' + (summary.unreviewed_count || 0),
                            'guidance exceptions: ' + ((summary.guidance_alignment && summary.guidance_alignment.differs_from_latest_review) || 0),
                            'final_review.status: ' + (finalReviewAudit.status || 'unknown'),
                            'final_review.decisions: ' + (finalReviewAudit.count || 0),
                            'final_review.approved_for_real_orders: ' + !!finalReviewAudit.approved_for_real_orders,
                            'final_review.coverage_gap_probe.available: ' + !!finalReviewGapProbe.available,
                            'final_review.coverage_gap_probe.limit: ' + (finalReviewGapProbe.limit || 0),
                            'final_review.coverage_gap_probe.coverage_status: ' + (finalReviewGapProbe.coverage_status || 'unknown'),
                            'final_review.coverage_gap_probe.gap_count: ' + ((finalReviewGapProbe.coverage_gaps && finalReviewGapProbe.coverage_gaps.count) || 0),
                            'final_review.coverage_gap_probe.displayed_event_count: ' + (finalReviewGapProbe.displayed_event_count || 0),
                            'final_review.coverage_gap_probe.active_24h_gap_status: ' + (finalReviewGapProbe.active_24h_gap_status || 'unknown'),
                            'final_review.coverage_gap_probe.active_24h_gap_count: ' + (finalReviewGapProbe.active_24h_gap_count ?? 0),
                            'final_review.coverage_gap_probe.expired_24h_gap_count: ' + (finalReviewGapProbe.expired_24h_gap_count ?? 0),
                            'final_review.coverage_gap_probe.hermes_coverage_window_seconds: ' + (finalReviewGapProbe.hermes_coverage_window_seconds ?? 0),
                            'final_review.coverage_gap_probe.oldest_gap_created_at: ' + (finalReviewGapProbe.oldest_gap_created_at || 'none'),
                            'final_review.coverage_gap_probe.newest_gap_created_at: ' + (finalReviewGapProbe.newest_gap_created_at || 'none'),
                            'final_review.coverage_gap_probe.oldest_gap_age_seconds: ' + (finalReviewGapProbe.oldest_gap_age_seconds ?? 'none'),
                            'final_review.coverage_gap_probe.newest_gap_age_seconds: ' + (finalReviewGapProbe.newest_gap_age_seconds ?? 'none'),
                            'final_review.coverage_gap_probe.seconds_until_all_gaps_age_out_of_24h: ' + (finalReviewGapProbe.seconds_until_all_gaps_age_out_of_24h ?? 'none'),
                            'final_review.coverage_gap_probe.seconds_until_active_gaps_age_out_of_24h: ' + (finalReviewGapProbe.seconds_until_active_gaps_age_out_of_24h ?? 'none'),
                            'final_review.coverage_gap_probe.active_gaps_age_out_at: ' + (finalReviewGapProbe.active_gaps_age_out_at || 'none'),
                            'final_review.hermes_gap_alignment.status: ' + (finalReviewHermesAlignment.status || 'unknown'),
                            'final_review.hermes_gap_alignment.aligned: ' + !!finalReviewHermesAlignment.aligned,
                            'final_review.hermes_gap_alignment.requires_attention: ' + !!finalReviewHermesAlignment.requires_attention,
                            'final_review.hermes_gap_alignment.app_active_24h_gap_count: ' + (finalReviewHermesAlignment.app_active_24h_gap_count ?? 0),
                            'final_review.hermes_gap_alignment.hermes_missing_gap_count: ' + (finalReviewHermesAlignment.hermes_missing_gap_count ?? 0),
                            'final_review.hermes_gap_alignment.active_gaps_age_out_at: ' + (finalReviewHermesAlignment.active_gaps_age_out_at || 'none'),
                            'final_review.hermes_gap_alignment.hermes_reflection_age_seconds: ' + (finalReviewHermesAlignment.hermes_reflection_age_seconds ?? 'none'),
                            'final_review.hermes_gap_alignment.hermes_reflection_freshness_status: ' + (finalReviewHermesAlignment.hermes_reflection_freshness_status || 'unknown'),
                            'final_review.hermes_gap_alignment.hermes_reflection_stale_after_seconds: ' + (finalReviewHermesAlignment.hermes_reflection_stale_after_seconds ?? 'none'),
                            'hermes_safety_loop.status: ' + (hermes.status || 'unknown'),
                            'hermes_safety_loop.final_review_decision_events_24h: ' + (hermes.final_review_decision_events_24h || 0),
                            'hermes_safety_loop.complete_fail_closed_no_network_evidence: ' + !!hermesCoverage.complete_fail_closed_no_network_evidence,
                            'live_sender.boundary: ' + (liveSenderBoundary.boundary_name || 'unknown') + ' / ' + (liveSenderBoundary.implementation_name || 'unknown'),
                            'live_sender.fail_closed_implementation_present: ' + !!liveSenderBoundary.fail_closed_implementation_present,
                            'live_sender.network_sender_present: ' + !!liveSenderBoundary.network_sender_present,
                            'live_sender.accepted_for_network_dispatch: ' + !!liveSenderBoundary.accepted_for_network_dispatch,
                            'live_sender.request_sent: ' + !!liveSenderBoundary.request_sent,
                            'action_summary: total=' + (actionSummary.total_count || 0) + ', attention=' + (actionSummary.attention_count || 0) + ', info=' + (actionSummary.info_count || 0) + ', primary=' + (actionSummary.primary_action_id || 'none'),
                            'freshness: generated_at=' + (freshness.generated_at || 'unknown') + ', stale_after_seconds=' + (freshness.stale_after_seconds || 0),
                            'next_actions: ' + actionText
                        ];
                        if (clob.error) lines.push('clob.error: ' + clob.error);
                        if (review.error) lines.push('review.error: ' + review.error);
                        if (finalReview.error) lines.push('final_review.error: ' + finalReview.error);
                        if (finalReview.coverage_gap_probe_error) lines.push('final_review.coverage_gap_probe.error: ' + finalReview.coverage_gap_probe_error);
                        if (d.hermes_safety_loop && d.hermes_safety_loop.error) lines.push('hermes_safety_loop.error: ' + d.hermes_safety_loop.error);
                        if (d && d.note) lines.push('note: ' + d.note);
                        panel.textContent = lines.join('\\n');
                        panel.style.color = d && d.operator_status === 'paper_observing' ? '#66d98f' : '#ffb366';
                        renderClobOperatorActions(actions);
                    }}).catch(() => {{
                        panel.textContent = 'CLOB operator status unavailable';
                        panel.style.color = '#ffb366';
                        if (actionsEl) actionsEl.innerHTML = '';
                    }});
                }}

                function renderClobOperatorActions(actions) {{
                    const actionsEl = document.getElementById('clob-operator-status-actions');
                    if (!actionsEl) return;
                    actionsEl.innerHTML = '';
                    if (!Array.isArray(actions) || !actions.length) return;
                    actions.forEach(action => {{
                        const id = action && action.id ? action.id : 'none';
                        const label = action && action.label ? action.label : id;
                        const severity = action && action.severity ? action.severity : 'info';
                        const row = document.createElement('div');
                        row.style.marginTop = '0.35rem';
                        const btn = document.createElement('button');
                        btn.type = 'button';
                        btn.textContent = id;
                        btn.title = severity + ': ' + label;
                        if (severity === 'attention') {{
                            btn.style.borderColor = '#ffb366';
                            btn.style.color = '#ffcf99';
                        }}
                        btn.onclick = () => runClobOperatorAction(id);
                        row.appendChild(btn);
                        const text = document.createElement('small');
                        text.textContent = ' [' + severity + '] ' + label;
                        row.appendChild(text);
                        actionsEl.appendChild(row);
                    }});
                }}

                function runClobOperatorAction(actionId) {{
                    const actions = {{
                        inspect_clob_diagnostics: [updateClobDiagnosticsPanel, 'clob-diagnostics-panel'],
                        inspect_clob_preflight: [updatePreflightPanel, 'clob-preflight-panel'],
                        inspect_review_summary: [updateReviewSummary, 'review-summary-panel'],
                        inspect_review_health: [updateReviewHealth, 'review-health-panel'],
                        review_unreviewed_dry_runs: [updateReviewQueue, 'review-queue-list'],
                        inspect_guidance_exceptions: [updateGuidanceExceptions, 'guidance-exceptions-list'],
                        inspect_review_latency: [updateReviewSummary, 'review-summary-panel'],
                        inspect_final_review_decisions: [updateFinalReviewDecisions, 'clob-final-review-decisions-list'],
                        inspect_final_review_coverage_gaps: [updateFinalReviewCoverageGaps, 'clob-final-review-decisions-list'],
                        inspect_hermes_gap_alignment: [updateClobOperatorStatus, 'clob-operator-status-panel'],
                        inspect_hermes_safety_loop: [updateHermesSafetyLoop, 'clob-hermes-safety-loop-panel'],
                        inspect_live_sender_design: [updateLiveSenderDesignReadiness, 'clob-live-sender-design-panel'],
                        inspect_live_sender_design_review: [updateLiveSenderDesignReview, 'clob-live-sender-design-review-panel'],
                        inspect_live_sender_boundary: [updateLiveSenderBoundaryStatus, 'clob-live-sender-boundary-panel'],
                        submit_paper_dry_run: [null, 'dry-run-result']
                    }};
                    const entry = actions[actionId];
                    if (!entry) return;
                    if (entry[0]) entry[0]();
                    const target = document.getElementById(entry[1]);
                    const card = target && target.closest ? target.closest('.card') : target;
                    if (card && card.scrollIntoView) card.scrollIntoView({{ behavior: 'smooth', block: 'start' }});
                }}

                function updateOrderPlacementReadiness() {{
                    const panel = document.getElementById('clob-order-placement-readiness-panel');
                    if (!panel) return;
                    fetch('clob/order-placement-readiness?limit=50').then(r => r.json()).then(d => {{
                        if (!d || d.error) {{
                            panel.textContent = 'CLOB order placement readiness unavailable: ' + ((d && d.error) || 'unknown error');
                            panel.style.color = '#ffb366';
                            return;
                        }}
                        const readiness = (d && d.readiness) || {{}};
                        const blockers = Array.isArray(readiness.blockers) ? readiness.blockers : [];
                        const gates = Array.isArray(readiness.gates) ? readiness.gates : [];
                        const boundary = readiness.live_sender_boundary_status || {{}};
                        const lines = [
                            'ready: ' + !!readiness.ready,
                            'stage: ' + (readiness.stage || 'unknown'),
                            'completed_gates: ' + (readiness.completed_count || 0) + '/' + (readiness.required_count || gates.length || 0),
                            'blocker_count: ' + (readiness.blocker_count || blockers.length || 0),
                            'blockers: ' + (blockers.length ? blockers.join(', ') : 'none'),
                            'final_review_audit_status: ' + (readiness.final_review_audit_status || 'unknown'),
                            'final_review_decision_count: ' + (readiness.final_review_decision_count || 0),
                            'live_sender_boundary: ' + (boundary.boundary_name || 'unknown') + ' / ' + (boundary.implementation_name || 'unknown'),
                            'fail_closed_implementation_present: ' + !!boundary.fail_closed_implementation_present,
                            'network_sender_present: ' + !!boundary.network_sender_present,
                            'accepted_for_network_dispatch: ' + !!boundary.accepted_for_network_dispatch,
                            'boundary_request_sent: ' + !!boundary.request_sent,
                            'next_safe_step: ' + (readiness.next_safe_step || 'unknown'),
                            'real_orders_enabled: ' + !!(d && d.real_orders_enabled),
                            'ready_for_real_orders: ' + !!(d && d.ready_for_real_orders),
                        ];
                        if (readiness.note) lines.push('note: ' + readiness.note);
                        if (d.errors && d.errors.clob) lines.push('clob.error: ' + d.errors.clob);
                        if (d.errors && d.errors.review) lines.push('review.error: ' + d.errors.review);
                        if (d.errors && d.errors.final_review) lines.push('final_review.error: ' + d.errors.final_review);
                        panel.textContent = lines.join('\\n');
                        panel.style.color = readiness.ready ? '#66d98f' : '#ffb366';
                    }}).catch(() => {{
                        panel.textContent = 'CLOB order placement readiness unavailable';
                        panel.style.color = '#ffb366';
                    }});
                }}

                function updateRealTradingUnlockStatus() {{
                    const panel = document.getElementById('clob-real-trading-unlock-panel');
                    if (!panel) return;
                    fetch('clob/real-trading-unlock-status').then(r => r.json()).then(d => {{
                        const status = d && d.unlock_status ? d.unlock_status : d || {{}};
                        const blockers = Array.isArray(status.blockers) ? status.blockers : [];
                        const lines = [
                            'ready: ' + !!status.ready,
                            'ready_for_real_orders: ' + !!(d && d.ready_for_real_orders),
                            'real_orders_enabled: ' + !!(d && d.real_orders_enabled),
                            'journaled: ' + !!(d && d.journaled),
                            'journal_event_id: ' + ((d && d.journal_event_id) || 'n/a'),
                            'explicit_real_order_submission_configured: ' + !!status.explicit_real_order_submission_configured,
                            'kill_switch_open: ' + !!status.kill_switch_open,
                            'paper_mode_active: ' + !!status.paper_mode_active,
                            'live_order_sender_implemented: ' + !!status.live_order_sender_implemented,
                            'request_sent: ' + !!status.request_sent,
                            'post_order_called: ' + !!status.post_order_called,
                            'post_orders_called: ' + !!status.post_orders_called,
                            'blockers: ' + (blockers.length ? blockers.join(', ') : 'none')
                        ];
                        if (status.note || (d && d.note)) lines.push('note: ' + (status.note || d.note));
                        if (d && d.error) lines.push('error: ' + d.error);
                        panel.textContent = lines.join('\\n');
                        panel.style.color = blockers.length ? '#ffb366' : '#66d98f';
                    }}).catch(() => {{
                        panel.textContent = 'Real trading unlock status unavailable';
                        panel.style.color = '#ffb366';
                    }});
                }}

                function updateLiveSenderDesignReadiness() {{
                    const panel = document.getElementById('clob-live-sender-design-panel');
                    if (!panel) return;
                    fetch('clob/live-sender-design-readiness').then(r => r.json()).then(d => {{
                        const design = d && d.live_sender_design ? d.live_sender_design : d || {{}};
                        const blockers = Array.isArray(design.blockers) ? design.blockers : [];
                        const lines = [
                            'ready_for_live_sender_implementation: ' + !!design.ready_for_live_sender_implementation,
                            'ready_for_real_orders: ' + !!(d && d.ready_for_real_orders),
                            'real_orders_enabled: ' + !!(d && d.real_orders_enabled),
                            'approved_for_real_orders: ' + !!design.approved_for_real_orders,
                            'journaled: ' + !!(d && d.journaled),
                            'journal_event_id: ' + ((d && d.journal_event_id) || 'n/a'),
                            'stage: ' + (design.stage || 'unknown'),
                            'completed_gates: ' + (design.completed_count || 0) + '/' + (design.required_count || 0),
                            'blocker_count: ' + (design.blocker_count || blockers.length || 0),
                            'blockers: ' + (blockers.length ? blockers.join(', ') : 'none'),
                            'request_sent: ' + !!design.request_sent,
                            'post_order_called: ' + !!design.post_order_called,
                            'post_orders_called: ' + !!design.post_orders_called,
                            'live_order_sender_implemented: ' + !!design.live_order_sender_implemented,
                            'next_safe_step: ' + (design.next_safe_step || 'unknown')
                        ];
                        if (design.note || (d && d.note)) lines.push('note: ' + (design.note || d.note));
                        if (d && Array.isArray(d.errors) && d.errors.length) lines.push('errors: ' + d.errors.join(', '));
                        panel.textContent = lines.join('\\n');
                        panel.style.color = blockers.length ? '#ffb366' : '#66d98f';
                    }}).catch(() => {{
                        panel.textContent = 'Live sender design readiness unavailable';
                        panel.style.color = '#ffb366';
                    }});
                }}

                function updateLiveSenderDesignReview() {{
                    const panel = document.getElementById('clob-live-sender-design-review-panel');
                    if (!panel) return;
                    fetch('clob/live-sender-design-review').then(r => r.json()).then(d => {{
                        const review = d && d.live_sender_design_review ? d.live_sender_design_review : d || {{}};
                        const blockers = Array.isArray(review.blockers) ? review.blockers : [];
                        const contract = review.review_contract || {{}};
                        const guards = Array.isArray(contract.required_pre_submit_guards) ? contract.required_pre_submit_guards : [];
                        const shortcuts = Array.isArray(contract.prohibited_shortcuts) ? contract.prohibited_shortcuts : [];
                        const lines = [
                            'ready_for_design_review: ' + !!review.ready_for_design_review,
                            'ready_for_live_sender_implementation: ' + !!review.ready_for_live_sender_implementation,
                            'implementation_permitted: ' + !!review.implementation_permitted,
                            'ready_for_real_orders: ' + !!(d && d.ready_for_real_orders),
                            'real_orders_enabled: ' + !!(d && d.real_orders_enabled),
                            'approved_for_real_orders: ' + !!review.approved_for_real_orders,
                            'journaled: ' + !!(d && d.journaled),
                            'journal_event_id: ' + ((d && d.journal_event_id) || 'n/a'),
                            'stage: ' + (review.stage || 'unknown'),
                            'completed_gates: ' + (review.completed_count || 0) + '/' + (review.required_count || 0),
                            'blocker_count: ' + (review.blocker_count || blockers.length || 0),
                            'blockers: ' + (blockers.length ? blockers.join(', ') : 'none'),
                            'boundary: ' + (contract.boundary_name || 'unknown'),
                            'future_module: ' + (contract.future_module || 'unknown'),
                            'required_pre_submit_guards: ' + (guards.length ? guards.join(', ') : 'none'),
                            'prohibited_shortcuts: ' + (shortcuts.length ? shortcuts.join(', ') : 'none'),
                            'request_sent: ' + !!review.request_sent,
                            'post_order_called: ' + !!review.post_order_called,
                            'post_orders_called: ' + !!review.post_orders_called,
                            'live_order_sender_implemented: ' + !!review.live_order_sender_implemented,
                            'next_safe_step: ' + (review.next_safe_step || 'unknown')
                        ];
                        if (review.note || (d && d.note)) lines.push('note: ' + (review.note || d.note));
                        if (d && Array.isArray(d.errors) && d.errors.length) lines.push('errors: ' + d.errors.join(', '));
                        panel.textContent = lines.join('\\n');
                        panel.style.color = blockers.length ? '#ffb366' : '#66d98f';
                    }}).catch(() => {{
                        panel.textContent = 'Live sender design review unavailable';
                        panel.style.color = '#ffb366';
                    }});
                }}

                function updateLiveSenderBoundaryStatus() {{
                    const panel = document.getElementById('clob-live-sender-boundary-panel');
                    if (!panel) return;
                    fetch('clob/live-sender-boundary-status').then(r => r.json()).then(d => {{
                        const boundary = d && d.live_sender_boundary ? d.live_sender_boundary : d || {{}};
                        const lines = [
                            'boundary_name: ' + (boundary.boundary_name || 'unknown'),
                            'implementation_name: ' + (boundary.implementation_name || 'unknown'),
                            'trait_defined: ' + !!boundary.trait_defined,
                            'fail_closed_implementation_present: ' + !!boundary.fail_closed_implementation_present,
                            'network_sender_present: ' + !!boundary.network_sender_present,
                            'implementation_permitted: ' + !!boundary.implementation_permitted,
                            'accepted_for_network_dispatch: ' + !!boundary.accepted_for_network_dispatch,
                            'submit_decision: ' + (boundary.submit_decision || 'unknown'),
                            'rejection_reason: ' + (boundary.rejection_reason || 'unknown'),
                            'ready_for_live_sender_implementation: ' + !!boundary.ready_for_live_sender_implementation,
                            'ready_for_real_orders: ' + !!(d && d.ready_for_real_orders),
                            'real_orders_enabled: ' + !!(d && d.real_orders_enabled),
                            'journaled: ' + !!(d && d.journaled),
                            'journal_event_id: ' + ((d && d.journal_event_id) || 'n/a'),
                            'request_sent: ' + !!boundary.request_sent,
                            'would_send: ' + !!boundary.would_send,
                            'post_order_called: ' + !!boundary.post_order_called,
                            'post_orders_called: ' + !!boundary.post_orders_called,
                            'required_next_step: ' + (boundary.required_next_step || 'unknown')
                        ];
                        if (boundary.note || (d && d.note)) lines.push('note: ' + (boundary.note || d.note));
                        panel.textContent = lines.join('\\n');
                        panel.style.color = boundary.network_sender_present ? '#ffb366' : '#66d98f';
                    }}).catch(() => {{
                        panel.textContent = 'Live sender boundary status unavailable';
                        panel.style.color = '#ffb366';
                    }});
                }}

                function updateFinalReviewReadiness() {{
                    const panel = document.getElementById('clob-final-review-readiness-panel');
                    if (!panel) return;
                    fetch('clob/final-review-readiness').then(r => r.json()).then(d => {{
                        const review = d && d.final_review ? d.final_review : d || {{}};
                        const blockers = Array.isArray(review.blockers) ? review.blockers : [];
                        const boundary = review.live_sender_boundary_status || {{}};
                        if (d && d.journal_event_id) {{
                            window.latestClobFinalReviewEventId = d.journal_event_id;
                        }}
                        const lines = [
                            'ready_for_final_review: ' + !!review.ready_for_final_review,
                            'ready_for_real_orders: ' + !!(d && d.ready_for_real_orders),
                            'real_orders_enabled: ' + !!(d && d.real_orders_enabled),
                            'journaled: ' + !!(d && d.journaled),
                            'journal_event_id: ' + ((d && d.journal_event_id) || 'n/a'),
                            'stage: ' + (review.stage || 'unknown'),
                            'completed_gates: ' + (review.completed_count || 0) + '/' + (review.required_count || 0),
                            'blocker_count: ' + (review.blocker_count || blockers.length || 0),
                            'blockers: ' + (blockers.length ? blockers.join(', ') : 'none'),
                            'live_sender_boundary: ' + (boundary.boundary_name || 'unknown') + ' / ' + (boundary.implementation_name || 'unknown'),
                            'fail_closed_implementation_present: ' + !!boundary.fail_closed_implementation_present,
                            'network_sender_present: ' + !!boundary.network_sender_present,
                            'accepted_for_network_dispatch: ' + !!boundary.accepted_for_network_dispatch,
                            'boundary_request_sent: ' + !!boundary.request_sent,
                            'request_sent: ' + !!review.request_sent,
                            'post_order_called: ' + !!review.post_order_called,
                            'post_orders_called: ' + !!review.post_orders_called,
                            'next_safe_step: ' + (review.next_safe_step || 'unknown')
                        ];
                        if (review.note || (d && d.note)) lines.push('note: ' + (review.note || d.note));
                        if (d && Array.isArray(d.errors) && d.errors.length) lines.push('errors: ' + d.errors.join(', '));
                        panel.textContent = lines.join('\\n');
                        panel.style.color = blockers.length ? '#ffb366' : '#66d98f';
                    }}).catch(() => {{
                        panel.textContent = 'Final review readiness unavailable';
                        panel.style.color = '#ffb366';
                    }});
                }}

                function recordFinalReviewDecision() {{
                    const panel = document.getElementById('clob-final-review-decision-panel');
                    const finalReviewEventId = window.latestClobFinalReviewEventId || null;
                    if (!panel) return;
                    if (!finalReviewEventId) {{
                        panel.textContent = 'No final review readiness event available yet';
                        panel.style.color = '#ffb366';
                        return;
                    }}
                    if (panel) panel.textContent = 'Fetching current evidence for snapshot at decision...';
                    Promise.all([
                        fetch('clob/collateral-readiness').then(function(r) {{ return r.json(); }}).catch(function() {{ return {{}}; }}),
                        fetch('clob/final-review-readiness').then(function(r) {{ return r.json(); }}).catch(function() {{ return {{}}; }})
                    ]).then(function(arr) {{
                        var coll = arr[0] || {{}};
                        var fr = arr[1] || {{}};
                        var collSnap = coll || {{}};
                        var riskSnap = (fr && (fr.final_review || fr.readiness || fr)) || {{}};
                        var payload = {{
                            final_review_event_id: finalReviewEventId,
                            decision: 'acknowledge_blocked',
                            confirm_final_review_workflow: true,
                            note: 'Dashboard final review acknowledges current blockers; no live trading approval',
                            operator: 'dashboard',
                            risk_snapshot: riskSnap,
                            collateral_snapshot: collSnap
                        }};
                        panel.textContent = 'Recording audit-only final review decision...';
                        fetch('clob/final-review-decision', {{
                            method: 'POST',
                            headers: {{ 'Content-Type': 'application/json' }},
                            body: JSON.stringify(payload)
                        }}).then(function(r) {{ return r.json(); }}).then(function(d) {{
                            var blockers = Array.isArray(d && d.blockers) ? d.blockers : [];
                            var readinessBlockers = Array.isArray(d && d.readiness_blockers) ? d.readiness_blockers : [];
                            var boundary = d && d.live_sender_boundary_status ? d.live_sender_boundary_status : {{}};
                            if (d && d.journal_event_id) {{
                                window.latestClobFinalReviewEventId = d.journal_event_id;  // now the decision id for submit-facade final_review_decision_event_id
                            }}
                            var lines = [
                                'final_review_decision_recorded: ' + !!(d && d.final_review_decision_recorded),
                                'journaled: ' + !!(d && d.journaled),
                                'decision: ' + ((d && d.decision) || 'n/a'),
                                'approved_for_real_orders: ' + !!(d && d.approved_for_real_orders),
                                'ready_for_real_orders: ' + !!(d && d.ready_for_real_orders),
                                'review_decision_effect: ' + ((d && d.review_decision_effect) || 'n/a'),
                                'final_review_event_valid: ' + !!(d && d.final_review_event_valid),
                                'live_sender_boundary_fail_closed: ' + !!(d && d.live_sender_boundary_fail_closed),
                                'live_sender_boundary: ' + (boundary.boundary_name || 'unknown') + ' / ' + (boundary.implementation_name || 'unknown'),
                                'network_sender_present: ' + !!boundary.network_sender_present,
                                'accepted_for_network_dispatch: ' + !!boundary.accepted_for_network_dispatch,
                                'final_review_event_id: ' + ((d && d.final_review_event_id) || 'n/a'),
                                'journal_event_id: ' + ((d && d.journal_event_id) || 'n/a'),
                                'request_sent: ' + !!(d && d.request_sent),
                                'post_order_called: ' + !!(d && d.post_order_called),
                                'post_orders_called: ' + !!(d && d.post_orders_called),
                                'readiness_blockers: ' + (readinessBlockers.length ? readinessBlockers.join(', ') : 'none'),
                                'blockers: ' + (blockers.length ? blockers.join(', ') : 'none')
                            ];
                            if (d && d.error) lines.push('error: ' + d.error);
                            panel.textContent = lines.join('\\n');
                            panel.style.color = blockers.length ? '#ffb366' : '#66d98f';
                            updateFinalReviewDecisions();
                            updateHumanApprovalsList();  // refresh lists after approval actions
                        }}).catch(function(e) {{
                            panel.textContent = 'Final review decision failed: ' + e;
                            panel.style.color = '#ffb366';
                        }});
                    }}).catch(function(e) {{
                        if (panel) panel.textContent = 'Evidence fetch failed for final decision: ' + e;
                    }});
                }}

                function updateFinalReviewDecisions(limit, gapsOnly) {{
                    const summary = document.getElementById('clob-final-review-decisions-summary');
                    const body = document.getElementById('clob-final-review-decisions-list');
                    if (!summary || !body) return;
                    const auditLimit = limit || 5;
                    const gapParam = gapsOnly ? '&gaps_only=true' : '';
                    fetch('clob/final-review-decisions?limit=' + encodeURIComponent(auditLimit) + gapParam).then(r => r.json()).then(d => {{
                        const counts = d && d.decision_counts ? d.decision_counts : {{}};
                        const latestBoundary = d && d.latest_boundary_status ? d.latest_boundary_status : {{}};
                        summary.textContent = [
                            'audit_limit: ' + auditLimit,
                            'gaps_only: ' + !!(d && d.gaps_only),
                            'displayed_event_count: ' + ((d && d.displayed_event_count) || 0),
                            'count: ' + ((d && d.count) || 0),
                            'boundary_evidence_count: ' + ((d && d.boundary_evidence_count) || 0),
                            'no_network_evidence_count: ' + ((d && d.no_network_evidence_count) || 0),
                            'missing_boundary_evidence_count: ' + ((d && d.missing_boundary_evidence_count) || 0),
                            'missing_no_network_evidence_count: ' + ((d && d.missing_no_network_evidence_count) || 0),
                            'all_events_have_boundary_evidence: ' + !!(d && d.all_events_have_boundary_evidence),
                            'all_events_have_no_network_evidence: ' + !!(d && d.all_events_have_no_network_evidence),
                            'coverage_status: ' + ((d && d.coverage_status) || 'unknown'),
                            'coverage_gap_count: ' + ((d && d.coverage_gaps && d.coverage_gaps.count) || 0),
                            'latest_boundary: ' + (latestBoundary.boundary_name || 'unknown') + ' / ' + (latestBoundary.implementation_name || 'unknown'),
                            'latest_network_sender_present: ' + !!latestBoundary.network_sender_present,
                            'latest_accepted_for_network_dispatch: ' + !!latestBoundary.accepted_for_network_dispatch,
                            'approved_for_real_orders: ' + !!(d && d.approved_for_real_orders),
                            'ready_for_real_orders: ' + !!(d && d.ready_for_real_orders),
                            'acknowledge_blocked: ' + (counts.acknowledge_blocked || 0),
                            'reject_live_trading: ' + (counts.reject_live_trading || 0),
                            'needs_rework: ' + (counts.needs_rework || 0),
                            'unknown: ' + (counts.unknown || 0),
                            'request_sent: ' + !!(d && d.request_sent),
                            'post_order_called: ' + !!(d && d.post_order_called),
                            'post_orders_called: ' + !!(d && d.post_orders_called)
                        ].join('\\n');
                        const events = Array.isArray(d && d.events) ? d.events : [];
                        if (!events.length) {{
                            body.innerHTML = '<tr><td colspan="8">No final review decisions loaded</td></tr>';
                            return;
                        }}
                        body.innerHTML = events.map(event => {{
                            const payload = event.payload || event || {{}};
                            const boundary = payload.live_sender_boundary_status || {{}};
                            const approved = payload.approved_for_real_orders === true ? 'true' : 'false';
                            const boundaryState = payload.missing_boundary_evidence === true ? 'missing' : (payload.live_sender_boundary_fail_closed === true ? 'fail-closed' : 'missing');
                            const noNetworkEvidence = payload.live_sender_boundary_fail_closed === true &&
                                boundary.network_sender_present === false &&
                                boundary.accepted_for_network_dispatch === false &&
                                boundary.request_sent === false;
                            const dispatchState = payload.missing_no_network_evidence === true ? 'missing' : (noNetworkEvidence ? 'no-network' : (boundary.accepted_for_network_dispatch === true ? 'accepted' : 'missing'));
                            const decId = event.id || payload.journal_event_id || '';
                            // 2026-06-06 UI polish (additive only): light snap evidence hint for final (parity with human enrichment + Hermes attribution); uses existing payload (snaps present since 2026-06-03); appended to operator cell. All old cols/ids/hooks/SSR strings preserved exactly.
                            const hasSnap = !!(payload.risk_snapshot_at_approval || payload.collateral_snapshot_at_approval);
                            const opWithHint = escapeHtml(payload.operator || 'unspecified') + (hasSnap ? ' [w/ snap]' : '');
                            return '<tr>' +
                                '<td>' + escapeHtml(event.created_at || '') + '</td>' +
                                '<td>' + escapeHtml(payload.decision || 'unknown') + '</td>' +
                                '<td>' + escapeHtml(payload.review_decision_effect || 'audit_only_no_unlock') + '</td>' +
                                '<td>' + escapeHtml(approved) + '</td>' +
                                '<td>' + escapeHtml(boundaryState) + '</td>' +
                                '<td>' + escapeHtml(dispatchState) + '</td>' +
                                '<td>' + opWithHint + '</td>' +
                                '<td><button onclick="useFinalDecisionIdForSubmit(\'' + decId + '\')">Copy/Use Final ID for Submit</button></td>' +
                                '</tr>';
                        }}).join('');
                    }}).catch(e => {{
                        summary.textContent = 'Final review decisions unavailable: ' + e;
                    }});
                }}

                function updateFinalReviewCoverageGaps() {{
                    updateFinalReviewDecisions(50, true);
                }}

                // 2026-06-03: fetch /clob/order-intent/human-approvals (pending/recent list), render table with copyable ids + snapshot hints.
                // Buttons allow operator to grab a prior approval id for submit-facade form (pairs with final id).
                function updateHumanApprovalsList() {{
                    const summary = document.getElementById('clob-human-approvals-summary');
                    const body = document.getElementById('clob-human-approvals-list');
                    if (!summary || !body) return;
                    fetch('clob/order-intent/human-approvals?limit=5').then(r => r.json()).then(d => {{
                        const count = (d && d.count) || 0;
                        summary.textContent = [
                            'event_type: ' + ((d && d.event_type) || 'clob_order_human_approval'),
                            'count: ' + count,
                            'note: ' + ((d && d.note) || '')
                        ].join('\\n');
                        const events = Array.isArray(d && d.events) ? d.events : [];
                        if (!events.length) {{
                            body.innerHTML = '<tr><td colspan="6">No human approvals loaded</td></tr>';
                            return;
                        }}
                        body.innerHTML = events.map(function(ev) {{
                            const id = ev.journal_event_id || '';
                            // 2026-06-06 UI polish (additive, no new ids/queries/hooks): richer evidence hint from already-returned full snaps (risk+coll) for operator practicality now that Hermes attributes them; created_at as approval time proxy (enriched approval_time in journal payload). Old useHuman... + table structure preserved.
                            let riskHint = 'n/a';
                            const rs = ev.risk_snapshot_at_approval || {{}};
                            const cs = ev.collateral_snapshot_at_approval || {{}};
                            if (ev.risk_snapshot_at_approval || ev.collateral_snapshot_at_approval) {{
                                // 2026-06-06 fix round: safe coercion for anomalous snapshot shapes (noisy [object Object] avoided; inline to keep no extra named helpers beyond escapeHtml)
                                const p = rs.projected_notional;
                                const p2 = rs.projected;
                                const proj = (typeof p === 'string' || typeof p === 'number') ? p : ((typeof p2 === 'string' || typeof p2 === 'number') ? p2 : '?');
                                const c = cs.collateral_balance_positive;
                                const c2 = cs.collateral_allowance_positive;
                                const collOk = (c === true || c2 === true) ? 'ok' : (c === false ? 'low' : 'see');
                                riskHint = 'p:' + proj + ' coll:' + collOk + ' [snap]';
                            }}
                            return '<tr>' +
                                '<td>' + escapeHtml(ev.created_at || '') + '</td>' +
                                '<td>' + escapeHtml(ev.decision || 'n/a') + '</td>' +
                                '<td>' + escapeHtml(String(!!ev.approved_for_facade)) + '</td>' +
                                '<td>' + escapeHtml(ev.operator || 'unspecified') + '</td>' +
                                '<td>' + escapeHtml(String(riskHint)) + '</td>' +
                                '<td><button onclick="useHumanApprovalIdForSubmit(\'' + id + '\')">Copy/Use ID for Submit</button></td>' +
                                '</tr>';
                        }}).join('');
                    }}).catch(function(e) {{
                        if (summary) summary.textContent = 'Human approvals list unavailable: ' + e;
                    }});
                }}

                function useHumanApprovalIdForSubmit(id) {{
                    if (!id) return;
                    window.latestClobHumanApprovalEventId = id;
                    const note = document.getElementById('clob-human-approvals-note');
                    if (note) note.textContent = (note.textContent || '').split(' | Selected')[0] + ' | Selected human_approval_event_id for submit: ' + id + ' (also use in submit-facade JSON human_approval_event_id). Pair with final_review_decision_event_id from Final Review panel. With unlocks+kill this id enables gated real dispatch. (Hermes attr preserved)';
                    // Also update the dry-run result area if present for visibility
                    // 2026-06-06 polish: tighter guidance for submit facade panel integration (additive text only).
                    const res = document.getElementById('dry-run-result');
                    if (res) res.textContent = 'Selected human_approval_event_id for submit: ' + id + '\\n(Now click Submit Facade Check in the CLOB Dry-Run Intent card (or POST /clob/order-intent/submit-facade with this + final id + confirm_real_order_submission:true under unlocks+kill). Pairs with Hermes attribution in safety panel.)';
                }}

                function useFinalDecisionIdForSubmit(id) {{
                    if (!id) return;
                    window.latestClobFinalReviewEventId = id;
                    const note = document.getElementById('clob-final-review-decisions-summary');
                    if (note) note.textContent = (note.textContent || '').split(' | Selected')[0] + ' | Selected final_review_decision_event_id for submit: ' + id + ' (use in submit-facade JSON as final_review_decision_event_id). Pair with human_approval_event_id. With unlocks+kill this enables gated real dispatch reval. (note preserved)';
                    const res = document.getElementById('dry-run-result');
                    if (res) res.textContent = 'Selected final_review_decision_event_id for submit: ' + id + '\\n(Now click Submit Facade Check in the CLOB Dry-Run Intent card (or POST with human id + this + confirm_real:true). Hermes attr notes in approvals/Hermes panels.)';
                }}

                function updateHermesSafetyLoop() {{
                    const panel = document.getElementById('clob-hermes-safety-loop-panel');
                    if (!panel) return;
                    fetch('clob/hermes-safety-loop').then(r => r.json()).then(d => {{
                        if (!d || d.error) {{
                            panel.textContent = 'Hermes CLOB safety loop unavailable: ' + ((d && d.error) || 'unknown error');
                            panel.style.color = '#ffb366';
                            return;
                        }}
                        const coverage = d.final_review_decision_boundary_coverage || {{}};
                        const boundary = d.latest_final_review_decision_boundary_status || {{}};
                        const reflection = d.reflection || {{}};
                        const recs = Array.isArray(reflection.recommendations) ? reflection.recommendations : [];
                        const lines = [
                            'status: ' + (d.status || 'unknown'),
                            'available: ' + !!d.available,
                            'real_orders_enabled: ' + !!d.real_orders_enabled,
                            'ready_for_real_orders: ' + !!d.ready_for_real_orders,
                            'reflection_age_seconds: ' + (reflection.age_seconds || 0),
                            'final_review_decision_events_24h: ' + (d.final_review_decision_events_24h || 0),
                            'boundary_evidence_events_24h: ' + (d.final_review_decision_boundary_evidence_events_24h || 0),
                            'no_network_evidence_events_24h: ' + (d.final_review_decision_no_network_evidence_events_24h || 0),
                            'missing_boundary_evidence_events_24h: ' + (d.final_review_decision_missing_boundary_evidence_events_24h || 0),
                            'missing_no_network_evidence_events_24h: ' + (d.final_review_decision_missing_no_network_evidence_events_24h || 0),
                            'coverage_status: ' + (coverage.coverage_status || 'unknown'),
                            'complete_fail_closed_no_network_evidence: ' + !!coverage.complete_fail_closed_no_network_evidence,
                            'all_decisions_have_boundary_evidence: ' + !!coverage.all_decisions_have_boundary_evidence,
                            'all_decisions_have_no_network_evidence: ' + !!coverage.all_decisions_have_no_network_evidence,
                            'latest_boundary: ' + (boundary.boundary_name || 'unknown') + ' / ' + (boundary.implementation_name || 'unknown'),
                            'latest_network_sender_present: ' + !!boundary.network_sender_present,
                            'latest_accepted_for_network_dispatch: ' + !!boundary.accepted_for_network_dispatch,
                            // 2026-06-06 UI polish (additive, reuse of existing fetch to /clob/hermes-safety-loop which now carries the keys from hermes attribution): surface Hermes approval attribution hints lightly in this panel (and append to approvals card note below) for operator value tying self-imp to the queue UX; no new queries/ids.
                            'approvals_with_snapshots_24h: ' + (d.approvals_with_snapshots_24h || 0),
                            'final_review_decisions_with_snapshots_24h: ' + (d.final_review_decisions_with_snapshots_24h || 0),
                            'pre_dispatches_with_approval_ids_24h: ' + (d.pre_dispatches_with_approval_ids_24h || 0),
                            'dispatches_from_approved_24h: ' + (d.dispatches_from_approved_24h || 0),
                            'approval_to_pre_dispatch_rate: ' + (d.approval_to_pre_dispatch_rate || '0.00'),
                            'hermes_approval_gap: ' + (d.hermes_approval_gap || 0),
                            'recommendations: ' + (recs.length ? recs.slice(0, 2).join(' | ') : 'none')
                        ];
                        // 2026-06-07 additive inside existing updateHermesSafetyLoop (after recommendations, before summary/note ifs; reuses same fetch/panel id + no change to any old line/string/id/hook/calls/setTimeout/inspect map/approvalsNote block): smallest surfacing of skeleton + static DR + tax + proxy data (DR/tax siblings in stored reflection.metrics from hermes do_reflection; server build_hermes_safety_loop_response currently promotes only clob_safety_loop scalars + limited reflection sub/summary so dynamic lines use fallback; full top-level promotion for d. "live" is future when endpoint extended per goals 'backtest harness on DRs vs paper fills + tax-adjusted with real join/attr') per current hermes state + log "Ready for next (e.g. UI for live Decision Reports + provenance to approvals/DR cadence + tax ...)" + goals "Extend `do_reflection`" "Query recent fills + all decision reports" "Compare decision reports vs actual outcomes" "backtest harness on DRs vs paper fills + tax-adjusted with real join/attr" + fees-tax "treat every paper trade as if it will one day be real" + "journal should be capable..." + plan "Ready for next / backtest" + AGENTS "self-improving" "When Adding Features" (wiki first, no new features; qualified per smallest + "no new DB harness" + "local cargo sufficient" -- server edit avoided to keep precise tranche-only + avoid any surface risk).
                        // Renders small pre lines for sampled DR net_edge_after_fees (PRIMARY per strategy/goals), generated_by, ids (for provenance to approvals/DR cadence), tax/fill lens from dr_vs proxy; disclaimers "paper proxy only" "skeleton vs production" "limited (no full DR-fill/id-level join/attr yet... see goals-and-operational-cadence.md for fuller...)" "observe pre-dispatch + DRs + tax + fills samples in next hermes reflection" (non-overclaim per hermes + prior tranches); tie style to "Risk/Coll Snapshot Summary (enriched)".
                        // RISK (AGENTS.md + safety first + trading rules non-negotiable): read-only display only (no enable real / no submit / no auto); paper_only + real_orders_enabled===false + fail-closed + L2 + pre-dispatch + gated reval + 401s + TEST_ENV_LOCK + --threads=1 + explicit native-l2 (no ||) + heavy comments + Decimal (source) + "no new privileged/UI" + "0 new tests ok if documented" + "local cargo + unit sufficient" + "skeleton vs production" + "no new DB harness"; all prior surfaces (paper default, gated "rejected_fail_closed", SSR <base href="/polytrader/"> + *every* old marker + *all polish* + DR-stub/approval/"Risk/Coll..."/"Hermes attr: snaps="/hasSnap/tax.../recent.../dr_vs... + "observe..." + clob-*-panel + update*/record* + "Pending..." + "Copy/Use..." + l2-chip + clob-hermes... + updateHermes... etc in app.rs + SSR test contains exactly) preserved 100% ironclad (proven by post greps/reads/SSR && chains; no leakage of new text into old strings; no regression on "PAPER TRADING ONLY" + paper_only + real_orders_enabled===false + <base> + 39+ markers). "What did we learn?": the self-imp loop data (DRs + tax + fills samples + dr_vs proxy attr + approvals provenance + pre-dispatch) from hermes reflections is now skeleton+static visible in UI for operator + to inform future low-risk Hermes wiki proposals (self-imp + wiki first-class per AGENTS); advances usability without touching any trading/real paths or prior verified surfaces. See wiki/log.md (this tranche) + AGENTS.md.
                        const clobLoop = d.clob_safety_loop || {{}};
                        const drCad = d.decision_report_cadence || clobLoop.decision_report_cadence || {{}};
                        const taxSk = d.tax_journal_skeleton || clobLoop.tax_journal_skeleton || {{}};
                        lines.push('decision_report_cadence.recent_decision_reports_sampled: ' + (drCad.recent_decision_reports_sampled || clobLoop.decision_reports_considered_24h || 0));
                        lines.push('decision_report_cadence.decision_reports_considered_24h: ' + (drCad.decision_reports_considered_24h || clobLoop.decision_reports_considered_24h || 0));
                        if (drCad.note) lines.push('decision_report_cadence.note: ' + drCad.note);
                        const drs = Array.isArray(drCad.recent_decision_reports_sampled) ? drCad.recent_decision_reports_sampled : [];
                        for (let i = 0; i < Math.min(2, drs.length); i++) {{
                            const s = drs[i] || {{}};
                            lines.push('DR[' + i + '] net_edge_after_fees (PRIMARY): ' + (s.net_edge_after_fees || s['net_edge_after_fees'] || '?') + ' generated_by: ' + (s.generated_by || '?') + ' id: ' + (s.id || s.journal_event_id || '?') + ' (provenance to approvals/DR cadence)');
                        }}
                        lines.push('tax_journal_skeleton.fills_24h: ' + (taxSk.fills_24h || clobLoop.fills_24h || 0));
                        lines.push('tax_journal_skeleton.recent_paper_fills_sampled: ' + (taxSk.recent_paper_fills_sampled ? 'present (paper proxy)' : 'n/a'));
                        const drvs = taxSk.dr_vs_paper_fills_compare || clobLoop.dr_vs_paper_fills_compare || {{}};
                        lines.push('tax_journal_skeleton.dr_vs_paper_fills_compare: dr_net_preview=' + (drvs.dr_net_preview || '?') + ' fills_fee_proxy=' + (drvs.fills_fee_proxy || '?') + ' tax_snapshots_for_attr=' + (drvs.tax_snapshots_for_attr || '?'));
                        if (drvs.proxy_attr_note) lines.push('dr_vs proxy_attr_note: ' + drvs.proxy_attr_note);
                        if (taxSk.note) lines.push('tax_journal_skeleton.note: ' + taxSk.note);
                        lines.push('disclaimers: paper proxy only; skeleton vs production; limited (no full DR-fill/id-level join/attr yet... see goals-and-operational-cadence.md for fuller backtest harness on DRs vs paper fills + tax-adjusted with real join/attr); observe pre-dispatch + DRs + tax + fills samples in next hermes reflection');
                        if (reflection.summary) lines.push('summary: ' + reflection.summary);
                        if (d.note) lines.push('note: ' + d.note);
                        panel.textContent = lines.join('\\n');
                        panel.style.color = d.status === 'boundary_coverage_complete' ? '#66d98f' : '#ffb366';
                        // 2026-06-06 polish: reuse this fetch (no new query) to lightly surface Hermes attribution hint directly in the approvals card's *existing* note el (ties queue to self-imp data without new ids/hooks/markers).
                        // Fix round: guard + use* preserve (split on ' | Selected') mitigate coupling; append ensures attr even if prior overwrites (idempotent via includes).
                        const approvalsNote = document.getElementById('clob-human-approvals-note');
                        if (approvalsNote && (d.approvals_with_snapshots_24h !== undefined || d.hermes_approval_gap !== undefined)) {{
                            const extra = ' | Hermes attr: snaps=' + (d.approvals_with_snapshots_24h || 0) + ' gap=' + (d.hermes_approval_gap || 0) + ' (full in this panel)';
                            if (!approvalsNote.textContent.includes('Hermes attr:')) {{
                                approvalsNote.textContent = approvalsNote.textContent + extra;
                            }}
                        }}
                    }}).catch(e => {{
                        panel.textContent = 'Hermes CLOB safety loop unavailable: ' + e;
                        panel.style.color = '#ffb366';
                    }});
                }}

                function updateClobAccountPanel() {{
                    const panel = document.getElementById('clob-account-panel');
                    if (!panel) return;
                    fetch('clob/account').then(r => r.json()).then(d => {{
                        const account = d && d.account ? d.account : {{}};
                        const orders = account.open_orders || {{}};
                        let openCount = '?';
                        if (Array.isArray(orders.data)) {{
                            openCount = orders.data.length;
                        }} else if (typeof orders.count === 'number') {{
                            openCount = orders.count;
                        }} else if (Array.isArray(orders)) {{
                            openCount = orders.length;
                        }}
                        const collateral = account.collateral || {{}};
                        const allowances = collateral.allowances || {{}};
                        const allowanceEntries = Object.entries(allowances);
                        const positiveAllowanceCount = allowanceEntries.filter(([, v]) => {{
                            const n = Number(v);
                            return Number.isFinite(n) && n > 0;
                        }}).length;
                        const lines = [
                            'l2_connected: ' + !!(d && d.l2_connected),
                            'read_only_live_check: ' + !!(d && d.read_only_live_check),
                            'paper_only: ' + !!(d && d.paper_only),
                            'real_orders_enabled: ' + !!(d && d.real_orders_enabled),
                            'open_orders.count: ' + openCount,
                            'collateral.balance: ' + (Object.prototype.hasOwnProperty.call(collateral, 'balance') ? collateral.balance : '?'),
                            'allowance_entries: ' + allowanceEntries.length,
                            'positive_allowance_entries: ' + positiveAllowanceCount
                        ];
                        if (d && d.error) lines.push('error: ' + d.error);
                        if (d && d.note) lines.push('note: ' + d.note);
                        panel.textContent = lines.join('\\n');
                        panel.style.color = d && d.read_only_live_check ? '#66d98f' : '#ffb366';
                    }}).catch(() => {{
                        panel.textContent = 'CLOB account snapshot unavailable';
                        panel.style.color = '#ffb366';
                    }});
                }}

                function updatePreflightChip() {{
                    const el = document.getElementById('preflight-chip');
                    if (!el) return;
                    fetch('clob/preflight').then(r => r.json()).then(d => {{
                        if (d && d.read_only_live_check && d.preflight) {{
                            const blockers = Array.isArray(d.preflight.blockers) ? d.preflight.blockers.length : 0;
                            el.innerHTML = blockers > 0 ? ('Preflight: blocked (' + blockers + ')') : 'Preflight: clear';
                            el.style.color = blockers > 0 ? '#ffb366' : '#66d98f';
                        }} else if (d && d.l2_connected) {{
                            el.innerHTML = 'Preflight: failed';
                            el.style.color = '#ffb366';
                        }} else {{
                            el.innerHTML = 'Preflight: waiting for L2';
                            el.style.color = '#aaa';
                        }}
                    }}).catch(() => {{
                        el.innerHTML = 'Preflight: unavailable';
                        el.style.color = '#ffb366';
                    }});
                }}

                function updatePreflightPanel() {{
                    const panel = document.getElementById('clob-preflight-panel');
                    if (!panel) return;
                    fetch('clob/preflight').then(r => r.json()).then(d => {{
                        const preflight = d && d.preflight ? d.preflight : d || {{}};
                        const blockers = Array.isArray(preflight.blockers) ? preflight.blockers : [];
                        const checks = Array.isArray(preflight.checks) ? preflight.checks : [];
                        const failedChecks = checks
                            .filter(c => c && c.ok === false)
                            .map(c => (c.name || 'unnamed') + ' [' + (c.severity || 'unknown') + ']');
                        const lines = [
                            'l2_connected: ' + !!(d && d.l2_connected),
                            'read_only_live_check: ' + !!(d && d.read_only_live_check),
                            'ready_for_real_orders: ' + !!(preflight.ready_for_real_orders),
                            'paper_only: ' + !!(preflight.paper_only || (d && d.paper_only)),
                            'real_orders_enabled: ' + !!(preflight.real_orders_enabled || (d && d.real_orders_enabled)),
                            'open_order_count: ' + (preflight.open_order_count ?? '?'),
                            'collateral_balance: ' + (preflight.collateral_balance ?? '?'),
                            'positive_allowance_count: ' + (preflight.positive_allowance_count ?? '?'),
                            'blockers: ' + (blockers.length ? blockers.join(', ') : 'none')
                        ];
                        if (failedChecks.length) lines.push('failed_checks: ' + failedChecks.join(', '));
                        if (d && d.error) lines.push('error: ' + d.error);
                        if (preflight.note || (d && d.note)) lines.push('note: ' + (preflight.note || d.note));
                        panel.textContent = lines.join('\\n');
                        panel.style.color = blockers.length ? '#ffb366' : '#66d98f';
                    }}).catch(() => {{
                        panel.textContent = 'CLOB preflight unavailable';
                        panel.style.color = '#ffb366';
                    }});
                }}

                function updateCollateralReadinessPanel() {{
                    const panel = document.getElementById('clob-collateral-readiness-panel');
                    if (!panel) return;
                    fetch('clob/collateral-readiness').then(r => r.json()).then(d => {{
                        const report = d && d.collateral_readiness ? d.collateral_readiness : d || {{}};
                        const blockers = Array.isArray(report.blockers) ? report.blockers : [];
                        const actions = Array.isArray(report.operator_actions) ? report.operator_actions : [];
                        const requiredActions = actions
                            .filter(a => a && a.required)
                            .map(a => a.id || 'action');
                        const lines = [
                            'l2_connected: ' + !!(d && d.l2_connected),
                            'read_only_live_check: ' + !!(d && d.read_only_live_check),
                            'journaled: ' + !!(d && d.journaled),
                            'journal_event_id: ' + ((d && d.journal_event_id) || 'n/a'),
                            'wallet_address: ' + (report.wallet_address || 'n/a'),
                            'signature_type: ' + (report.signature_type ?? 'n/a'),
                            'collateral_balance: ' + (report.collateral_balance ?? 'n/a'),
                            'collateral_balance_positive: ' + !!report.collateral_balance_positive,
                            'allowance_count: ' + (report.allowance_count ?? 0),
                            'positive_allowance_count: ' + (report.positive_allowance_count ?? 0),
                            'collateral_allowance_positive: ' + !!report.collateral_allowance_positive,
                            'request_sent: ' + !!report.request_sent,
                            'post_order_called: ' + !!report.post_order_called,
                            'post_orders_called: ' + !!report.post_orders_called,
                            'blockers: ' + (blockers.length ? blockers.join(', ') : 'none'),
                            'required_actions: ' + (requiredActions.length ? requiredActions.join(', ') : 'none')
                        ];
                        if (d && d.error) lines.push('error: ' + d.error);
                        if (report.note || (d && d.note)) lines.push('note: ' + (report.note || d.note));
                        panel.textContent = lines.join('\\n');
                        panel.style.color = blockers.length ? '#ffb366' : '#66d98f';
                    }}).catch(() => {{
                        panel.textContent = 'CLOB collateral readiness unavailable';
                        panel.style.color = '#ffb366';
                    }});
                }}

                function updateDryRunsList() {{
                    const body = document.getElementById('dry-runs-list');
                    if (!body) return;
                    fetch('clob/order-intent/dry-runs?limit=5').then(r => r.json()).then(d => {{
                        const events = Array.isArray(d && d.events) ? d.events : [];
                        if (!events.length) {{
                            body.innerHTML = '<tr><td colspan="5">No journaled dry-runs yet</td></tr>';
                            return;
                        }}
                        body.innerHTML = events.map(ev => {{
                            const report = ev && ev.payload && ev.payload.report ? ev.payload.report : {{}};
                            const intent = report.intent || {{}};
                            const blockers = Array.isArray(report.blockers) ? report.blockers : [];
                            const review = ev && ev.latest_review && ev.latest_review.payload ? ev.latest_review.payload : null;
                            const reviewText = review && review.decision ? review.decision : 'unreviewed';
                            const reviewClass = reviewText === 'would_approve' ? '#66d98f' : (reviewText === 'would_reject' ? '#ffb366' : '#aaa');
                            const when = ev.created_at ? new Date(ev.created_at).toLocaleTimeString() : '';
                            const market = intent.market_id || intent.token_id || 'n/a';
                            const notional = report.estimated_notional || 'n/a';
                            const guidance = blockers.length > 0 ? 'would_reject' : 'needs_rework';
                            const actions = "<button onclick=\"reviewDryRun('" + ev.id + "', 'would_reject')\">Reject</button> " +
                                "<button onclick=\"reviewDryRun('" + ev.id + "', 'needs_rework')\">Rework</button> " +
                                "<button onclick=\"reviewDryRun('" + ev.id + "', 'would_approve')\">Would Approve</button> " +
                                "<button onclick=\"showDryRunDetail('" + ev.id + "')\">Details</button>";
                            return '<tr><td>' + when + '</td><td>' + market + '</td><td>' + notional + '</td><td>' + blockers.length + '<br><small>guidance: ' + guidance + '</small></td><td><span style="color:' + reviewClass + '">' + reviewText + '</span><br>' + actions + '</td></tr>';
                        }}).join('');
                    }}).catch(() => {{
                        body.innerHTML = '<tr><td colspan="5">Dry-run journal unavailable</td></tr>';
                    }});
                }}

                function updateReviewsList() {{
                    const body = document.getElementById('reviews-list');
                    const summaryEl = document.getElementById('reviews-summary');
                    if (!body) return;
                    fetch('clob/order-intent/reviews?limit=5').then(r => r.json()).then(d => {{
                        const events = Array.isArray(d && d.events) ? d.events : [];
                        const counts = d && d.decision_counts ? d.decision_counts : {{}};
                        if (summaryEl) {{
                            summaryEl.textContent = 'would_approve=' + (counts.would_approve || 0) +
                                ' · would_reject=' + (counts.would_reject || 0) +
                                ' · needs_rework=' + (counts.needs_rework || 0);
                        }}
                        if (!events.length) {{
                            body.innerHTML = '<tr><td colspan="5">No journaled reviews yet</td></tr>';
                            return;
                        }}
                        body.innerHTML = events.map(ev => {{
                            const review = ev && ev.payload ? ev.payload : {{}};
                            const summary = review.dry_run_summary || {{}};
                            const when = ev.created_at ? new Date(ev.created_at).toLocaleTimeString() : '';
                            const decision = review.decision || 'unknown';
                            const color = decision === 'would_approve' ? '#66d98f' : (decision === 'would_reject' ? '#ffb366' : '#aaa');
                            const market = summary.market_id || summary.token_id || 'n/a';
                            const operator = review.operator || 'operator';
                            const note = review.note || '';
                            return '<tr><td>' + when + '</td><td><span style="color:' + color + '">' + decision + '</span></td><td>' + market + '</td><td>' + operator + '</td><td>' + note + '</td></tr>';
                        }}).join('');
                    }}).catch(() => {{
                        body.innerHTML = '<tr><td colspan="5">Review journal unavailable</td></tr>';
                    }});
                }}

                function updateReviewSummary() {{
                    const panel = document.getElementById('review-summary-panel');
                    if (!panel) return;
                    fetch('clob/order-intent/review-summary?limit=50').then(r => r.json()).then(d => {{
                        const summary = d && d.summary ? d.summary : {{}};
                        const counts = summary.decision_counts || {{}};
                        const guidance = summary.guidance_counts || {{}};
                        const alignment = summary.guidance_alignment || {{}};
                        const latency = summary.latest_review_latency || {{}};
                        const top = Array.isArray(summary.top_blockers) ? summary.top_blockers : [];
                        const topText = top.length ? top.map(b => b.name + '=' + b.count).join(', ') : 'none';
                        panel.textContent = [
                            'dry-runs: ' + (summary.dry_run_count || 0),
                            'reviewed: ' + (summary.reviewed_count || 0) + ' (' + (summary.review_coverage_pct || '0.00') + '%)',
                            'unreviewed: ' + (summary.unreviewed_count || 0),
                            'would_approve: ' + (counts.would_approve || 0),
                            'would_reject: ' + (counts.would_reject || 0),
                            'needs_rework: ' + (counts.needs_rework || 0),
                            'guidance would_reject: ' + (guidance.would_reject || 0),
                            'guidance needs_rework: ' + (guidance.needs_rework || 0),
                            'guidance matches latest: ' + (alignment.matches_latest_review || 0),
                            'guidance differs latest: ' + (alignment.differs_from_latest_review || 0),
                            'latest review avg age: ' + formatAge(latency.avg_seconds),
                            'latest review max age: ' + formatAge(latency.max_seconds),
                            'top blockers: ' + topText
                        ].join('\\n');
                    }}).catch(() => {{
                        panel.textContent = 'Review summary unavailable';
                    }});
                }}

                function updateReviewHealth() {{
                    const panel = document.getElementById('review-health-panel');
                    const actionsEl = document.getElementById('review-health-actions');
                    if (!panel) return;
                    fetch('clob/order-intent/review-health?limit=50').then(r => r.json()).then(d => {{
                        if (!d || d.error) {{
                            panel.textContent = 'Review health unavailable: ' + ((d && d.error) || 'unknown error');
                            if (actionsEl) actionsEl.innerHTML = '';
                            return;
                        }}
                        const health = d.health || {{}};
                        const reasons = Array.isArray(health.reasons) ? health.reasons : [];
                        const actions = Array.isArray(health.recommended_actions) ? health.recommended_actions : [];
                        const actionText = actions.length ? actions.map(a => {{
                            const endpoint = a.endpoint ? ' -> ' + a.endpoint : '';
                            return (a.id || 'action') + ': ' + (a.label || '') + endpoint;
                        }}).join('\\n') : 'none';
                        panel.textContent = [
                            'status: ' + (health.status || 'unknown'),
                            'reasons: ' + (reasons.length ? reasons.join(', ') : 'none'),
                            'dry-runs: ' + (health.dry_run_count || 0),
                            'unreviewed: ' + (health.unreviewed_count || 0),
                            'guidance exceptions: ' + (health.guidance_exception_count || 0),
                            'max review age: ' + formatAge(health.max_latency_seconds),
                            'slow after: ' + formatAge(health.slow_latency_after_seconds),
                            'actions:\\n' + actionText
                        ].join('\\n');
                        renderReviewHealthActions(actions);
                    }}).catch(() => {{
                        panel.textContent = 'Review health unavailable';
                        if (actionsEl) actionsEl.innerHTML = '';
                    }});
                }}

                function renderReviewHealthActions(actions) {{
                    const actionsEl = document.getElementById('review-health-actions');
                    if (!actionsEl) return;
                    actionsEl.innerHTML = '';
                    if (!Array.isArray(actions) || !actions.length) return;
                    actions.forEach(action => {{
                        const id = action && action.id ? action.id : 'none';
                        const label = action && action.label ? action.label : id;
                        const row = document.createElement('div');
                        row.style.marginTop = '0.35rem';
                        const btn = document.createElement('button');
                        btn.type = 'button';
                        btn.textContent = id;
                        btn.onclick = () => runReviewHealthAction(id);
                        row.appendChild(btn);
                        const text = document.createElement('small');
                        text.textContent = ' ' + label;
                        row.appendChild(text);
                        actionsEl.appendChild(row);
                    }});
                }}

                function runReviewHealthAction(actionId) {{
                    const actions = {{
                        review_unreviewed_dry_runs: [updateReviewQueue, 'review-queue-list'],
                        inspect_guidance_exceptions: [updateGuidanceExceptions, 'guidance-exceptions-list'],
                        inspect_review_latency: [updateReviewSummary, 'review-summary-panel']
                    }};
                    const entry = actions[actionId];
                    if (!entry) return;
                    entry[0]();
                    const target = document.getElementById(entry[1]);
                    const card = target && target.closest ? target.closest('.card') : target;
                    if (card && card.scrollIntoView) card.scrollIntoView({{ behavior: 'smooth', block: 'start' }});
                }}

                function formatAge(seconds) {{
                    if (seconds === null || seconds === undefined) return 'n/a';
                    const total = Math.max(0, Number(seconds) || 0);
                    const days = Math.floor(total / 86400);
                    const hours = Math.floor((total % 86400) / 3600);
                    const minutes = Math.floor((total % 3600) / 60);
                    if (days > 0) return days + 'd ' + hours + 'h';
                    if (hours > 0) return hours + 'h ' + minutes + 'm';
                    return minutes + 'm';
                }}

                function updateReviewBacklog() {{
                    const panel = document.getElementById('review-backlog-panel');
                    if (!panel) return;
                    fetch('clob/order-intent/review-backlog').then(r => r.json()).then(d => {{
                        if (!d || d.error) {{
                            panel.textContent = 'Review backlog unavailable: ' + ((d && d.error) || 'unknown error');
                            return;
                        }}
                        panel.textContent = [
                            'status: ' + (d.status || 'unknown'),
                            'unreviewed: ' + (d.unreviewed_count || 0),
                            'oldest age: ' + formatAge(d.oldest_unreviewed_age_seconds),
                            'stale after: ' + formatAge(d.stale_after_seconds),
                            'oldest: ' + (d.oldest_unreviewed_at || 'n/a'),
                            'newest: ' + (d.newest_unreviewed_at || 'n/a')
                        ].join('\\n');
                    }}).catch(() => {{
                        panel.textContent = 'Review backlog unavailable';
                    }});
                }}

                function updateGuidanceExceptions() {{
                    const body = document.getElementById('guidance-exceptions-list');
                    if (!body) return;
                    fetch('clob/order-intent/review-guidance-exceptions?limit=10').then(r => r.json()).then(d => {{
                        const items = Array.isArray(d && d.items) ? d.items : [];
                        if (!items.length) {{
                            body.innerHTML = '<tr><td colspan="5">No guidance exceptions</td></tr>';
                            return;
                        }}
                        body.innerHTML = items.map(item => {{
                            const summary = item && item.dry_run_summary ? item.dry_run_summary : {{}};
                            const review = item && item.latest_review && item.latest_review.payload ? item.latest_review.payload : {{}};
                            const when = item.latest_review && item.latest_review.created_at ? new Date(item.latest_review.created_at).toLocaleTimeString() : '';
                            const decision = review.decision || 'unknown';
                            const guidance = summary.recommended_review_decision || 'unknown';
                            const market = summary.market_id || summary.token_id || 'n/a';
                            const eventId = item.dry_run_event_id || '';
                            return "<tr><td>" + when + "</td><td>" + decision + "</td><td>" + guidance + "</td><td>" + market + "</td><td><button onclick=\"showDryRunDetail('" + eventId + "')\">Details</button></td></tr>";
                        }}).join('');
                    }}).catch(() => {{
                        body.innerHTML = '<tr><td colspan="5">Guidance exceptions unavailable</td></tr>';
                    }});
                }}

                function updateGuidanceOverrides() {{
                    const body = document.getElementById('guidance-overrides-list');
                    if (!body) return;
                    fetch('clob/order-intent/review-guidance-overrides?limit=10').then(r => r.json()).then(d => {{
                        const items = Array.isArray(d && d.items) ? d.items : [];
                        if (!items.length) {{
                            body.innerHTML = '<tr><td colspan="6">No guidance overrides</td></tr>';
                            return;
                        }}
                        body.innerHTML = items.map(item => {{
                            const summary = item && item.dry_run_summary ? item.dry_run_summary : {{}};
                            const review = item && item.review ? item.review : {{}};
                            const when = item.review_created_at ? new Date(item.review_created_at).toLocaleTimeString() : '';
                            const decision = review.decision || 'unknown';
                            const guidance = review.recommended_review_decision || summary.recommended_review_decision || 'unknown';
                            const operator = review.operator || 'operator';
                            const note = review.note || '';
                            const eventId = item.dry_run_event_id || review.dry_run_event_id || '';
                            return "<tr><td>" + when + "</td><td>" + decision + "</td><td>" + guidance + "</td><td>" + operator + "</td><td>" + note + "</td><td><button onclick=\"showDryRunDetail('" + eventId + "')\">Details</button></td></tr>";
                        }}).join('');
                    }}).catch(() => {{
                        body.innerHTML = '<tr><td colspan="6">Guidance overrides unavailable</td></tr>';
                    }});
                }}

                function updateReviewQueue() {{
                    const body = document.getElementById('review-queue-list');
                    if (!body) return;
                    fetch('clob/order-intent/review-queue?limit=5').then(r => r.json()).then(d => {{
                        const items = Array.isArray(d && d.items) ? d.items : [];
                        if (!items.length) {{
                            body.innerHTML = '<tr><td colspan="6">No unreviewed dry-runs</td></tr>';
                            return;
                        }}
                        body.innerHTML = items.map(item => {{
                            const summary = item && item.dry_run_summary ? item.dry_run_summary : {{}};
                            const blockers = Array.isArray(item && item.blockers) ? item.blockers : [];
                            const when = item.dry_run_created_at ? new Date(item.dry_run_created_at).toLocaleTimeString() : '';
                            const age = formatAge(item.dry_run_age_seconds);
                            const priority = item.review_priority || 'standard_unreviewed';
                            const nextAction = item.next_review_action || 'review_before_any_live_work';
                            const market = summary.market_id || summary.token_id || 'n/a';
                            const notional = summary.estimated_notional || 'n/a';
                            const eventId = item.dry_run_event_id || '';
                            const guidance = summary.recommended_review_decision || (blockers.length > 0 ? 'would_reject' : 'needs_rework');
                            const actions = "<button onclick=\"reviewDryRun('" + eventId + "', 'would_reject')\">Reject</button> " +
                                "<button onclick=\"reviewDryRun('" + eventId + "', 'needs_rework')\">Rework</button> " +
                                "<button onclick=\"reviewDryRun('" + eventId + "', 'would_approve')\">Would Approve</button> " +
                                "<button onclick=\"showDryRunDetail('" + eventId + "')\">Details</button>";
                            return '<tr><td>' + when + '</td><td>' + age + '<br><small>' + priority + '</small></td><td>' + market + '</td><td>' + notional + '</td><td>' + blockers.length + '<br><small>guidance: ' + guidance + '</small><br><small>next: ' + nextAction + '</small></td><td>' + actions + '</td></tr>';
                        }}).join('');
                    }}).catch(() => {{
                        body.innerHTML = '<tr><td colspan="6">Review queue unavailable</td></tr>';
                    }});
                }}

                function showDryRunDetail(eventId) {{
                    const panel = document.getElementById('dry-run-detail-panel');
                    if (!panel) return;
                    if (!eventId) {{
                        panel.textContent = 'No dry-run selected';
                        return;
                    }}
                    panel.textContent = 'Loading dry-run detail...';
                    fetch('clob/order-intent/dry-runs/' + eventId).then(r => r.json()).then(d => {{
                        if (!d || d.error) {{
                            panel.textContent = 'Dry-run detail unavailable: ' + ((d && d.error) || 'unknown error');
                            return;
                        }}
                        const summary = d.dry_run_summary || {{}};
                        const blockers = Array.isArray(d.blockers) ? d.blockers : [];
                        const reviews = Array.isArray(d.reviews) ? d.reviews : [];
                        const latest = d.latest_review && d.latest_review.payload ? d.latest_review.payload : null;
                        const lines = [
                            'event: ' + (d.dry_run_event_id || eventId),
                            'market: ' + (summary.market_id || summary.token_id || 'n/a'),
                            'notional: ' + (summary.estimated_notional || 'n/a'),
                            'accepted: ' + (summary.accepted === true),
                            'blockers: ' + (blockers.length ? blockers.join(', ') : 'none'),
                            'approval blocked: ' + (summary.approval_blocked === true),
                            'recommended review: ' + (summary.recommended_review_decision || 'n/a'),
                            'reviews: ' + reviews.length,
                            'latest decision: ' + ((latest && latest.decision) || 'unreviewed')
                        ];
                        panel.textContent = lines.join('\\n') + '\\n\\n' + JSON.stringify(d, null, 2);
                    }}).catch(e => {{
                        panel.textContent = 'Dry-run detail failed: ' + e;
                    }});
                }}

                function reviewDryRun(eventId, decision) {{
                    const note = window.prompt('Paper-only review note for ' + decision + ' (required if overriding guidance):') || '';
                    fetch('clob/order-intent/dry-runs/' + eventId + '/review', {{
                        method: 'POST',
                        headers: {{ 'Content-Type': 'application/json' }},
                        body: JSON.stringify({{ decision, note, operator: 'dashboard' }})
                    }}).then(r => r.json()).then(d => {{
                        if (!d || !d.reviewed) {{
                            window.alert('Review failed: ' + ((d && d.error) || 'unknown error'));
                        }}
                        updateDryRunsList();
                        updateReviewsList();
                        updateReviewSummary();
                        updateReviewHealth();
                        updateReviewBacklog();
                        updateGuidanceExceptions();
                        updateGuidanceOverrides();
                        updateReviewQueue();
                        updateClobOperatorStatus();
                    }}).catch(e => {{
                        window.alert('Review failed: ' + e);
                    }});
                }}

                function dryRunIntentPayload() {{
                    const value = id => {{
                        const el = document.getElementById(id);
                        return el ? el.value : '';
                    }};
                    return {{
                        token_id: value('dry-token'),
                        side: value('dry-side'),
                        order_type: value('dry-type'),
                        size: value('dry-size'),
                        price: value('dry-price') || null,
                        expected_edge_bps: value('dry-edge') || null,
                        market_id: 'ui-dry-run',
                        outcome: 'Yes'
                    }};
                }}

                function submitDryRunIntent() {{
                    const result = document.getElementById('dry-run-result');
                    const payload = dryRunIntentPayload();
                    if (result) result.textContent = 'Submitting dry-run...';
                    fetch('clob/order-intent/dry-run', {{
                        method: 'POST',
                        headers: {{ 'Content-Type': 'application/json' }},
                        body: JSON.stringify(payload)
                    }}).then(r => r.json()).then(d => {{
                        const report = d && d.dry_run ? d.dry_run : {{}};
                        const blockers = Array.isArray(report.blockers) ? report.blockers : [];
                        const lines = [
                            'accepted: ' + (d && d.accepted),
                            'journaled: ' + (d && d.journaled),
                            'event: ' + ((d && d.journal_event_id) || 'n/a'),
                            'notional: ' + (report.estimated_notional || 'n/a'),
                            'blockers: ' + (blockers.length ? blockers.join(', ') : 'none')
                        ];
                        if (result) result.textContent = lines.join('\n');
                        updateDryRunsList();
                        updateReviewSummary();
                        updateReviewHealth();
                        updateReviewBacklog();
                        updateGuidanceExceptions();
                        updateGuidanceOverrides();
                        updateReviewQueue();
                        updateClobOperatorStatus();
                    }}).catch(e => {{
                        if (result) result.textContent = 'Dry-run failed: ' + e;
                    }});
                }}

                function validateMarketMetadataIntent() {{
                    const result = document.getElementById('dry-run-result');
                    const payload = dryRunIntentPayload();
                    if (result) result.textContent = 'Validating live market metadata...';
                    fetch('clob/order-intent/market-validation', {{
                        method: 'POST',
                        headers: {{ 'Content-Type': 'application/json' }},
                        body: JSON.stringify(payload)
                    }}).then(r => r.json()).then(d => {{
                        const blockers = Array.isArray(d && d.blockers) ? d.blockers : [];
                        const lines = [
                            'market_metadata_validation_available: ' + (d && d.market_metadata_validation_available),
                            'market_metadata_fetched: ' + (d && d.market_metadata_fetched),
                            'tick_size: ' + ((d && d.tick_size) || 'n/a'),
                            'neg_risk: ' + (d && d.neg_risk),
                            'negative_risk_adapter_required: ' + (d && d.negative_risk_adapter_required),
                            'price_tick_valid: ' + (d && d.price_tick_valid),
                            'price_within_tick_range: ' + (d && d.price_within_tick_range),
                            'request_sent: ' + (d && d.request_sent),
                            'post_order_called: ' + (d && d.post_order_called),
                            'post_orders_called: ' + (d && d.post_orders_called),
                            'journaled: ' + (d && d.journaled),
                            'event: ' + ((d && d.journal_event_id) || 'n/a'),
                            'blockers: ' + (blockers.length ? blockers.join(', ') : 'none'),
                            'error: ' + ((d && d.fetch_error) || (d && d.error) || 'none')
                        ];
                        if (result) result.textContent = lines.join('\n');
                        updateOrderPlacementReadiness();
                        updateClobOperatorStatus();
                    }}).catch(e => {{
                        if (result) result.textContent = 'Market metadata validation failed: ' + e;
                    }});
                }}

                function submitSignatureDryRunIntent() {{
                    const result = document.getElementById('dry-run-result');
                    const payload = Object.assign(dryRunIntentPayload(), {{
                        confirm_signed_payload_dry_run: true
                    }});
                    if (result) result.textContent = 'Building signed payload dry-run...';
                    fetch('clob/order-intent/signature-dry-run', {{
                        method: 'POST',
                        headers: {{ 'Content-Type': 'application/json' }},
                        body: JSON.stringify(payload)
                    }}).then(r => r.json()).then(d => {{
                        const blockers = Array.isArray(d && d.blockers) ? d.blockers : [];
                        const signed = d && d.signed_payload ? d.signed_payload : {{}};
                        const lines = [
                            'signed_payload_built: ' + (d && d.signed_payload_built),
                            'signed_payload_verified: ' + (d && d.signed_payload_verified),
                            'signature_redacted: ' + (d && d.signature_redacted),
                            'would_post: ' + (d && d.would_post),
                            'post_order_called: ' + (d && d.post_order_called),
                            'post_orders_called: ' + (d && d.post_orders_called),
                            'blockers: ' + (blockers.length ? blockers.join(', ') : 'none'),
                            'signature: ' + (signed.signature_masked || 'n/a'),
                            'payload_version: ' + (signed.payload_version || 'n/a'),
                            'error: ' + ((d && d.signing_error) || (d && d.error) || 'none')
                        ];
                        if (result) result.textContent = lines.join('\n');
                        updateOrderPlacementReadiness();
                        updateClobOperatorStatus();
                    }}).catch(e => {{
                        if (result) result.textContent = 'Signed payload dry-run failed: ' + e;
                    }});
                }}

                function submitPostRequestDryRunIntent() {{
                    const result = document.getElementById('dry-run-result');
                    const payload = Object.assign(dryRunIntentPayload(), {{
                        confirm_signed_payload_dry_run: true,
                        confirm_order_post_request_dry_run: true
                    }});
                    if (result) result.textContent = 'Building non-submitting POST request dry-run...';
                    fetch('clob/order-intent/post-request-dry-run', {{
                        method: 'POST',
                        headers: {{ 'Content-Type': 'application/json' }},
                        body: JSON.stringify(payload)
                    }}).then(r => r.json()).then(d => {{
                        const blockers = Array.isArray(d && d.blockers) ? d.blockers : [];
                        const post = d && d.post_request_dry_run ? d.post_request_dry_run : {{}};
                        const lines = [
                            'post_request_dry_run_built: ' + (d && d.post_request_dry_run_built),
                            'signature_redacted: ' + (d && d.signature_redacted),
                            'l2_hmac_redacted: ' + (d && d.l2_hmac_redacted),
                            'would_send: ' + (d && d.would_send),
                            'post_order_called: ' + (d && d.post_order_called),
                            'post_orders_called: ' + (d && d.post_orders_called),
                            'journaled: ' + (d && d.journaled),
                            'event: ' + ((d && d.journal_event_id) || 'n/a'),
                            'method: ' + (post.method || 'n/a'),
                            'path: ' + (post.path || 'n/a'),
                            'body_sha256: ' + (post.body_sha256 || 'n/a'),
                            'blockers: ' + (blockers.length ? blockers.join(', ') : 'none'),
                            'error: ' + ((d && d.error) || 'none')
                        ];
                        if (result) result.textContent = lines.join('\n');
                        updateOrderPlacementReadiness();
                        updateClobOperatorStatus();
                    }}).catch(e => {{
                        if (result) result.textContent = 'POST request dry-run failed: ' + e;
                    }});
                }}

                function recordHumanApprovalIntent() {{
                    const result = document.getElementById('dry-run-result');
                    if (result) result.textContent = 'Fetching current evidence for snapshot...';
                    Promise.all([
                        fetch('clob/collateral-readiness').then(function(r) {{ return r.json(); }}).catch(function() {{ return {{}}; }}),
                        fetch('clob/order-placement-readiness').then(function(r) {{ return r.json(); }}).catch(function() {{ return {{}}; }})
                    ]).then(function(arr) {{
                        var coll = arr[0] || {{}};
                        var place = arr[1] || {{}};
                        var collSnap = coll || {{}};
                        var riskSnap = (place && (place.readiness || place.order_placement_readiness || place)) || {{}};
                        var payload = Object.assign(dryRunIntentPayload(), {{
                            decision: 'approve_facade',
                            confirm_human_approval_workflow: true,
                            note: 'Dashboard operator approval for submit-facade validation only',
                            operator: 'dashboard',
                            risk_snapshot: riskSnap,
                            collateral_snapshot: collSnap
                        }});
                        if (result) result.textContent = 'Recording journaled human approval...';
                        fetch('clob/order-intent/human-approval', {{
                            method: 'POST',
                            headers: {{ 'Content-Type': 'application/json' }},
                            body: JSON.stringify(payload)
                        }}).then(function(r) {{ return r.json(); }}).then(function(d) {{
                            if (d && d.journal_event_id) {{
                                window.latestClobHumanApprovalEventId = d.journal_event_id;
                            }}
                            var blockers = Array.isArray(d && d.blockers) ? d.blockers : [];
                            var lines = [
                                'human_approval_journaled: ' + (d && d.journaled),
                                'approved_for_facade: ' + (d && d.approved_for_facade),
                                'approval_event: ' + ((d && d.journal_event_id) || 'n/a'),
                                'subject_hash: ' + ((d && d.subject_hash) || 'n/a'),
                                'request_sent: ' + (d && d.request_sent),
                                'post_order_called: ' + (d && d.post_order_called),
                                'post_orders_called: ' + (d && d.post_orders_called),
                                'blockers: ' + (blockers.length ? blockers.join(', ') : 'none'),
                                'error: ' + ((d && d.error) || 'none')
                            ];
                            if (result) result.textContent = lines.join('\n');
                            updateOrderPlacementReadiness();
                            updateClobOperatorStatus();
                            updateHumanApprovalsList();
                        }}).catch(function(e) {{
                            if (result) result.textContent = 'Human approval workflow failed: ' + e;
                        }});
                    }}).catch(function(e) {{
                        if (result) result.textContent = 'Evidence fetch failed for human approval: ' + e;
                    }});
                }}

                function submitOrderFacadeIntent() {{
                    const result = document.getElementById('dry-run-result');
                    const approvalEventId = window.latestClobHumanApprovalEventId || null;
                    const finalEventId = window.latestClobFinalReviewEventId || null;
                    const payload = Object.assign(dryRunIntentPayload(), {{
                        confirm_signed_payload_dry_run: false,
                        confirm_order_post_request_dry_run: false,
                        confirm_real_order_submission: !!(approvalEventId || finalEventId),
                        human_approval_event_id: approvalEventId,
                        final_review_decision_event_id: finalEventId,
                        human_approval_token: '',
                        human_approval_note: 'UI safety check only',
                        operator: 'dashboard'
                    }});
                    if (result) result.textContent = 'Evaluating fail-closed submit facade...';
                    fetch('clob/order-intent/submit-facade', {{
                        method: 'POST',
                        headers: {{ 'Content-Type': 'application/json' }},
                        body: JSON.stringify(payload)
                    }}).then(r => r.json()).then(d => {{
                        const blockers = Array.isArray(d && d.blockers) ? d.blockers : [];
                        const gate = d && d.gate_report ? d.gate_report : {{}};
                        const risk = gate && gate.risk_limits ? gate.risk_limits : {{}};
                        const collateral = gate && gate.collateral_readiness ? gate.collateral_readiness : {{}};
                        const reconciliation = d && d.reconciliation ? d.reconciliation : {{}};
                        const lines = [
                            'submission_facade_only: ' + (d && d.submission_facade_only),
                            'submit_decision: ' + ((d && d.submit_decision) || 'n/a'),
                            'reconciliation_status: ' + ((d && d.reconciliation_status) || 'n/a'),
                            'reconciled: ' + (d && d.reconciled),
                            'facade_available: ' + (d && d.facade_available),
                            'human_approval_event_valid: ' + (d && d.human_approval_event_valid),
                            'final_review_decision_event_id: ' + ((d && d.final_review_decision_event_id) || 'n/a'),
                            'final_review_decision_event_valid: ' + (d && d.final_review_decision_event_valid),
                            'fresh_collateral_readiness_valid: ' + (collateral && collateral.valid),
                            'fresh_collateral_readiness_event: ' + ((collateral && collateral.event_id) || 'n/a'),
                            'fresh_collateral_balance_positive: ' + (collateral && collateral.collateral_balance_positive),
                            'fresh_collateral_allowance_positive: ' + (collateral && collateral.collateral_allowance_positive),
                            'kill_switch_and_risk_limits_available: ' + (gate && gate.kill_switch_and_risk_limits_available),
                            'kill_switch_open: ' + (gate && gate.kill_switch_open),
                            'request_sent: ' + (d && d.request_sent),
                            'would_send: ' + (d && d.would_send),
                            'post_order_called: ' + (d && d.post_order_called),
                            'post_orders_called: ' + (d && d.post_orders_called),
                            'journaled: ' + (d && d.journaled),
                            'event: ' + ((d && d.journal_event_id) || 'n/a'),
                            'reconciliation_journaled: ' + (d && d.reconciliation_journaled),
                            'reconciliation_event: ' + ((d && d.reconciliation_event_id) || 'n/a'),
                            'expected_exchange_state: ' + (reconciliation.expected_exchange_state || 'n/a'),
                            'observed_exchange_state: ' + (reconciliation.observed_exchange_state || 'n/a'),
                            'gate_ready: ' + (gate.ready),
                            'projected_notional: ' + (risk.projected_notional || 'n/a'),
                            'max_order_notional: ' + (risk.max_order_notional || 'n/a'),
                            'max_total_exposure: ' + (risk.max_total_exposure || 'n/a'),
                            'max_daily_loss: ' + (risk.max_daily_loss || 'n/a'),
                            'blockers: ' + (blockers.length ? blockers.join(', ') : 'none'),
                            'error: ' + ((d && d.error) || 'none')
                        ];
                        if (result) result.textContent = lines.join('\n');
                        updateOrderPlacementReadiness();
                        updateClobOperatorStatus();
                    }}).catch(e => {{
                        if (result) result.textContent = 'Submit facade check failed: ' + e;
                    }});
                }}

                function deriveL2FromServerKey() {{
                    const chip = document.getElementById('l2-chip');
                    const btns = document.querySelectorAll('.auth .l2 button');
                    if (chip) chip.innerHTML = '⟳ Deriving...';
                    btns.forEach(b => b.disabled = true);

                    fetch('l2/derive-from-server-key', {{ method: 'POST' }})
                        .then(r => r.json())
                        .then(d => {{
                            if (d && d.success) {{
                                if (chip) {{
                                    chip.innerHTML = 'Connected (server key) ' + (d.api_key_masked || '') + ' <span style="font-size:0.75em;opacity:0.7;">(manual)</span>';
                                    chip.style.color = '#66b3ff';
                                }}
                                updateClobChip();
                                updateClobReadiness();
                                updateClobDiagnosticsPanel();
                                updateClobOperatorStatus();
                                updateClobAccountPanel();
                                updatePreflightChip();
                                updatePreflightPanel();
                                updateDryRunsList();
                                updateReviewSummary();
                                updateReviewHealth();
                                updateReviewBacklog();
                                updateGuidanceExceptions();
                                updateGuidanceOverrides();
                                updateReviewQueue();
                            }} else {{
                                if (chip) chip.innerHTML = 'Failed: ' + (d.error || 'unknown error');
                                if (chip) chip.style.color = '#ff6666';
                            }}
                            btns.forEach(b => b.disabled = false);
                        }})
                        .catch(e => {{
                            if (chip) {{
                                chip.innerHTML = 'Network error - see console';
                                chip.style.color = '#ff6666';
                            }}
                            console.error('L2 derive error:', e);
                            btns.forEach(b => b.disabled = false);
                        }});
                }}
                function disconnectL2() {{
                    const chip = document.getElementById('l2-chip');
                    const clob = document.getElementById('clob-chip');
                    const preflight = document.getElementById('preflight-chip');
                    fetch('l2/disconnect', {{ method: 'POST' }})
                        .then(r => r.json())
                        .then(() => {{
                            if (chip) {{
                                chip.innerHTML = 'Not connected (paper)';
                                chip.style.color = '#aaa';
                            }}
                            if (clob) {{
                                clob.innerHTML = 'CLOB account: waiting for L2';
                                clob.style.color = '#aaa';
                            }}
                            updateClobReadiness();
                            updateClobDiagnosticsPanel();
                            updateClobOperatorStatus();
                            updateOrderPlacementReadiness();
                            updateClobAccountPanel();
                            if (preflight) {{
                                preflight.innerHTML = 'Preflight: waiting for L2';
                                preflight.style.color = '#aaa';
                            }}
                            updatePreflightPanel();
                            updateDryRunsList();
                            updateReviewSummary();
                            updateReviewHealth();
                            updateReviewBacklog();
                            updateGuidanceExceptions();
                            updateGuidanceOverrides();
                            updateReviewQueue();
                        }})
                        .catch(() => {{
                            if (chip) {{
                                chip.innerHTML = 'Disconnect failed';
                                chip.style.color = '#ff6666';
                            }}
                        }});
                }}
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
            rendered.contains("Recent CLOB Dry-Runs")
                && rendered.contains("id=\"dry-runs-list\"")
                && rendered.contains("clob/order-intent/dry-runs")
                && rendered.contains("reviewDryRun"),
            "dry-run audit panel, fetch hook, and paper review hook must be rendered"
        );
        assert!(
            rendered.contains("Recent CLOB Reviews")
                && rendered.contains("id=\"reviews-list\"")
                && rendered.contains("clob/order-intent/reviews"),
            "dry-run review audit panel and fetch hook must be rendered"
        );
        assert!(
            rendered.contains("CLOB Review Summary")
                && rendered.contains("id=\"review-summary-panel\"")
                && rendered.contains("clob/order-intent/review-summary")
                && rendered.contains("guidance matches latest")
                && rendered.contains("latest review avg age"),
            "dry-run review summary panel and fetch hook must be rendered"
        );
        assert!(
            rendered.contains("CLOB Readiness")
                && rendered.contains("id=\"clob-readiness-panel\"")
                && rendered.contains("clob/status")
                && rendered.contains("updateClobReadiness")
                && rendered.contains("read_only_live_check")
                && rendered.contains("real_orders_enabled"),
            "read-only CLOB readiness panel and fetch hook must be rendered"
        );
        assert!(
            rendered.contains("CLOB Diagnostics")
                && rendered.contains("id=\"clob-diagnostics-panel\"")
                && rendered.contains("clob/diagnostics")
                && rendered.contains("updateClobDiagnosticsPanel")
                && rendered.contains("ready_for_real_orders")
                && rendered.contains("positive_allowance_entries"),
            "aggregate CLOB diagnostics panel and fetch hook must be rendered"
        );
        assert!(
            rendered.contains("CLOB Operator Status")
                && rendered.contains("id=\"clob-operator-status-panel\"")
                && rendered.contains("id=\"clob-operator-status-actions\"")
                && rendered.contains("clob/operator-status")
                && rendered.contains("updateClobOperatorStatus")
                && rendered.contains("renderClobOperatorActions")
                && rendered.contains("runClobOperatorAction")
                && rendered.contains("inspect_guidance_exceptions")
                && rendered.contains("inspect_review_latency")
                && rendered.contains("inspect_final_review_decisions")
                && rendered.contains("inspect_final_review_coverage_gaps")
                && rendered.contains("inspect_hermes_gap_alignment")
                && rendered.contains("inspect_hermes_safety_loop")
                && rendered.contains("inspect_live_sender_boundary")
                && rendered.contains("operator_status")
                && rendered.contains("action_summary")
                && rendered.contains("final_review.status")
                && rendered.contains("final_review.coverage_gap_probe.available")
                && rendered.contains("final_review.coverage_gap_probe.gap_count")
                && rendered.contains("final_review.coverage_gap_probe.displayed_event_count")
                && rendered.contains("final_review.coverage_gap_probe.active_24h_gap_status")
                && rendered.contains("final_review.coverage_gap_probe.active_24h_gap_count")
                && rendered.contains("final_review.coverage_gap_probe.expired_24h_gap_count")
                && rendered
                    .contains("final_review.coverage_gap_probe.hermes_coverage_window_seconds")
                && rendered.contains("final_review.coverage_gap_probe.oldest_gap_age_seconds")
                && rendered.contains("final_review.coverage_gap_probe.newest_gap_age_seconds")
                && rendered.contains(
                    "final_review.coverage_gap_probe.seconds_until_all_gaps_age_out_of_24h"
                )
                && rendered.contains(
                    "final_review.coverage_gap_probe.seconds_until_active_gaps_age_out_of_24h"
                )
                && rendered.contains("final_review.coverage_gap_probe.active_gaps_age_out_at")
                && rendered.contains("final_review.hermes_gap_alignment.status")
                && rendered.contains("final_review.hermes_gap_alignment.aligned")
                && rendered.contains("final_review.hermes_gap_alignment.requires_attention")
                && rendered.contains("final_review.hermes_gap_alignment.app_active_24h_gap_count")
                && rendered.contains("final_review.hermes_gap_alignment.hermes_missing_gap_count")
                && rendered.contains("final_review.hermes_gap_alignment.active_gaps_age_out_at")
                && rendered.contains(
                    "final_review.hermes_gap_alignment.hermes_reflection_freshness_status"
                )
                && rendered.contains(
                    "final_review.hermes_gap_alignment.hermes_reflection_stale_after_seconds"
                )
                && rendered.contains("hermes_safety_loop.status")
                && rendered.contains("live_sender.network_sender_present")
                && rendered.contains("freshness")
                && rendered.contains("recommended_next_actions"),
            "read-only CLOB operator status panel and fetch hook must be rendered"
        );
        assert!(
            rendered.contains("CLOB Order Placement Readiness")
                && rendered.contains("id=\"clob-order-placement-readiness-panel\"")
                && rendered.contains("clob/order-placement-readiness")
                && rendered.contains("updateOrderPlacementReadiness")
                && rendered.contains("next_safe_step")
                && rendered.contains("final_review_audit_status")
                && rendered.contains("ready_for_real_orders"),
            "read-only CLOB order placement readiness panel and fetch hook must be rendered"
        );
        assert!(
            rendered.contains("Real Trading Unlock Status")
                && rendered.contains("id=\"clob-real-trading-unlock-panel\"")
                && rendered.contains("clob/real-trading-unlock-status")
                && rendered.contains("updateRealTradingUnlockStatus")
                && rendered.contains("explicit_real_order_submission_configured")
                && rendered.contains("live_order_sender_implemented"),
            "read-only real trading unlock status panel and fetch hook must be rendered"
        );
        assert!(
            rendered.contains("Live Sender Design Readiness")
                && rendered.contains("id=\"clob-live-sender-design-panel\"")
                && rendered.contains("clob/live-sender-design-readiness")
                && rendered.contains("updateLiveSenderDesignReadiness")
                && rendered.contains("ready_for_live_sender_implementation"),
            "read-only live sender design readiness panel and fetch hook must be rendered"
        );
        assert!(
            rendered.contains("Live Sender Design Review")
                && rendered.contains("id=\"clob-live-sender-design-review-panel\"")
                && rendered.contains("clob/live-sender-design-review")
                && rendered.contains("updateLiveSenderDesignReview")
                && rendered.contains("ready_for_design_review")
                && rendered.contains("implementation_permitted"),
            "read-only live sender design review panel and fetch hook must be rendered"
        );
        assert!(
            rendered.contains("Live Sender Boundary Status")
                && rendered.contains("id=\"clob-live-sender-boundary-panel\"")
                && rendered.contains("clob/live-sender-boundary-status")
                && rendered.contains("updateLiveSenderBoundaryStatus")
                && rendered.contains("fail_closed_implementation_present")
                && rendered.contains("accepted_for_network_dispatch"),
            "read-only live sender boundary panel and fetch hook must be rendered"
        );
        assert!(
            rendered.contains("Final Review Readiness")
                && rendered.contains("id=\"clob-final-review-readiness-panel\"")
                && rendered.contains("Record Blocked Review")
                && rendered.contains("id=\"clob-final-review-decision-panel\"")
                && rendered.contains("clob/final-review-readiness")
                && rendered.contains("clob/final-review-decision")
                && rendered.contains("updateFinalReviewReadiness")
                && rendered.contains("recordFinalReviewDecision")
                && rendered.contains("ready_for_final_review")
                && rendered.contains("live_sender_boundary")
                && rendered.contains("accepted_for_network_dispatch"),
            "read-only final review readiness and decision panels/fetch hooks must be rendered"
        );
        assert!(
            rendered.contains("Final Review Decisions")
                && rendered.contains("id=\"clob-final-review-decisions-summary\"")
                && rendered.contains("id=\"clob-final-review-decisions-list\"")
                && rendered.contains("clob/final-review-decisions")
                && rendered.contains("updateFinalReviewDecisions")
                && rendered.contains("updateFinalReviewCoverageGaps")
                && rendered.contains("inspect_final_review_coverage_gaps")
                && rendered.contains("audit_limit")
                && rendered.contains("gaps_only")
                && rendered.contains("displayed_event_count")
                && rendered.contains("approved_for_real_orders")
                && rendered.contains("boundary_evidence_count")
                && rendered.contains("no_network_evidence_count")
                && rendered.contains("missing_boundary_evidence_count")
                && rendered.contains("missing_no_network_evidence_count")
                && rendered.contains("all_events_have_boundary_evidence")
                && rendered.contains("all_events_have_no_network_evidence")
                && rendered.contains("coverage_gap_count")
                && rendered.contains("Use ID")
                && rendered.contains("Copy/Use Final ID for Submit"),
            "read-only final review decision audit list and fetch hook must be rendered"
        );
        assert!(
            rendered.contains("Pending / Recent Human Approvals (for Gated Real CLOB)")
                && rendered.contains("id=\"clob-human-approvals-summary\"")
                && rendered.contains("id=\"clob-human-approvals-list\"")
                && rendered.contains("clob/order-intent/human-approvals")
                && rendered.contains("updateHumanApprovalsList")
                && rendered.contains("Copy/Use ID for Submit")
                && rendered.contains("useHumanApprovalIdForSubmit"),
            "read-only human approvals pending list panel and fetch hook must be rendered (2026-06-03 tranche)"
        );
        assert!(
            rendered.contains("Hermes CLOB Safety Loop")
                && rendered.contains("id=\"clob-hermes-safety-loop-panel\"")
                && rendered.contains("clob/hermes-safety-loop")
                && rendered.contains("updateHermesSafetyLoop")
                && rendered.contains("final_review_decision_boundary_coverage")
                && rendered.contains("missing_boundary_evidence_events_24h")
                && rendered.contains("coverage_status")
                && rendered.contains("complete_fail_closed_no_network_evidence")
                // 2026-06-07 additive for new UI DR surfacing (inside existing hermes panel only): SSR test now contains the new static strings/ids from the additive small in the card + (dynamic will be in live); while *every* prior old + polish + DR-stub/approval/"Risk/Coll Snapshot Summary (enriched)"/"Hermes attr: snaps="/hasSnap + clob-*-panel + update*/record* + "Pending / Recent Human Approvals" + "Copy/Use ID for Submit" + l2-chip + "PAPER TRADING ONLY" + paper_only + real_orders_enabled===false + <base href="/polytrader/"> + all 100+ markers remain *exact* in this && chain (proven by post-edit greps/reads on app.rs + test green; no regression, no removed, no leakage of new text into old strings).
                && rendered.contains("Recent Decision Reports (5-min DR cadence)")
                && rendered.contains("net_edge_after_fees (PRIMARY)")
                && rendered.contains("provenance to approvals")
                && rendered.contains("skeleton vs production")
                && rendered.contains("observe pre-dispatch + DRs + tax + fills samples in next hermes reflection"),
            "read-only Hermes CLOB safety-loop panel and fetch hook must be rendered"
        );
        assert!(
            rendered.contains("CLOB Account")
                && rendered.contains("id=\"clob-account-panel\"")
                && rendered.contains("clob/account")
                && rendered.contains("updateClobAccountPanel")
                && rendered.contains("allowance_entries")
                && rendered.contains("positive_allowance_entries"),
            "read-only CLOB account panel and fetch hook must be rendered"
        );
        assert!(
            rendered.contains("CLOB Preflight")
                && rendered.contains("id=\"clob-preflight-panel\"")
                && rendered.contains("clob/preflight")
                && rendered.contains("updatePreflightPanel")
                && rendered.contains("ready_for_real_orders")
                && rendered.contains("real_orders_enabled"),
            "diagnostic CLOB preflight panel and fetch hook must be rendered"
        );
        assert!(
            rendered.contains("CLOB Collateral Readiness")
                && rendered.contains("id=\"clob-collateral-readiness-panel\"")
                && rendered.contains("clob/collateral-readiness")
                && rendered.contains("updateCollateralReadinessPanel")
                && rendered.contains("collateral_balance_positive")
                && rendered.contains("collateral_allowance_positive"),
            "read-only collateral readiness panel and fetch hook must be rendered"
        );
        assert!(
            rendered.contains("CLOB Review Health")
                && rendered.contains("id=\"review-health-panel\"")
                && rendered.contains("id=\"review-health-actions\"")
                && rendered.contains("clob/order-intent/review-health")
                && rendered.contains("updateReviewHealth")
                && rendered.contains("recommended_actions")
                && rendered.contains("runReviewHealthAction"),
            "dry-run review health panel and fetch hook must be rendered"
        );
        assert!(
            rendered.contains("CLOB Review Backlog")
                && rendered.contains("id=\"review-backlog-panel\"")
                && rendered.contains("clob/order-intent/review-backlog")
                && rendered.contains("updateReviewBacklog"),
            "dry-run review backlog panel and fetch hook must be rendered"
        );
        assert!(
            rendered.contains("CLOB Guidance Exceptions")
                && rendered.contains("id=\"guidance-exceptions-list\"")
                && rendered.contains("clob/order-intent/review-guidance-exceptions")
                && rendered.contains("updateGuidanceExceptions"),
            "dry-run guidance exceptions panel and fetch hook must be rendered"
        );
        assert!(
            rendered.contains("CLOB Guidance Overrides")
                && rendered.contains("id=\"guidance-overrides-list\"")
                && rendered.contains("clob/order-intent/review-guidance-overrides")
                && rendered.contains("updateGuidanceOverrides"),
            "dry-run guidance overrides panel and fetch hook must be rendered"
        );
        assert!(
            rendered.contains("CLOB Review Queue")
                && rendered.contains("id=\"review-queue-list\"")
                && rendered.contains("clob/order-intent/review-queue")
                && rendered.contains("updateReviewQueue")
                && rendered.contains("review_priority")
                && rendered.contains("dry_run_age_seconds")
                && rendered.contains("next_review_action"),
            "dry-run review queue panel and fetch hook must be rendered"
        );
        assert!(
            rendered.contains("CLOB Dry-Run Detail")
                && rendered.contains("id=\"dry-run-detail-panel\"")
                && rendered.contains("showDryRunDetail")
                && rendered.contains("clob/order-intent/dry-runs/")
                && rendered.contains("recommended review"),
            "dry-run detail panel and fetch hook must be rendered"
        );
        assert!(
            rendered.contains("CLOB Dry-Run Intent")
                && rendered.contains("id=\"dry-run-result\"")
                && rendered.contains("submitDryRunIntent")
                && rendered.contains("Validate Market Metadata")
                && rendered.contains("validateMarketMetadataIntent")
                && rendered.contains("Signed Payload Dry Run")
                && rendered.contains("submitSignatureDryRunIntent")
                && rendered.contains("POST Request Dry Run")
                && rendered.contains("submitPostRequestDryRunIntent")
                && rendered.contains("Record Facade Approval")
                && rendered.contains("recordHumanApprovalIntent")
                && rendered.contains("Submit Facade Check")
                && rendered.contains("submitOrderFacadeIntent")
                && rendered.contains("clob/order-intent/dry-run")
                && rendered.contains("clob/order-intent/market-validation")
                && rendered.contains("clob/order-intent/signature-dry-run")
                && rendered.contains("clob/order-intent/post-request-dry-run")
                && rendered.contains("clob/order-intent/human-approval")
                && rendered.contains("clob/order-intent/submit-facade")
                && rendered.contains("submission_facade_only")
                && rendered.contains("human_approval_event_valid")
                && rendered.contains("kill_switch_and_risk_limits_available")
                && rendered.contains("max_order_notional")
                && rendered.contains("max_daily_loss")
                && rendered.contains("market_metadata_validation_available")
                && rendered.contains("signature_redacted")
                && rendered.contains("l2_hmac_redacted")
                && rendered.contains("post_order_called"),
            "dry-run submission form and fetch hook must be rendered"
        );
        assert!(
            rendered.to_lowercase().contains("autonomous"),
            "Hermes gated proposal context in safety card"
        );
        // Note: full <base> injection + wrapper tested via integration in server (string post-process); this covers core rsx SSR fidelity.
        // AUTH note: whoami/login strings are in static rsx + script (tested via presence in full e2e if needed).
    }
}
