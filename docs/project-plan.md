# polytrader Project Plan

**Version**: 0.1 (Bootstrap)  
**Date**: 2026-05 (initial)  
**Owner**: simonellefsen + agents

## Mission Summary

Automate participation in Polymarket prediction markets using a Rust/Dioxus 24/7 agentic system with strong self-improvement loops (Hermes) and an LLM-consumable wiki. Prioritize safety via realistic paper trading before any real capital.

See [wiki/index.md](../wiki/index.md) and [AGENTS.md](../AGENTS.md).

## Key Research Findings (Polymarket)

### Simulation / Paper Trading Support

**Conclusion**: Polymarket has **no official sandbox, testnet, or paper trading environment**.

- All trading occurs on Polygon mainnet (chain ID 137) against real USDC (or collateralized positions).
- The CLOB and Gamma APIs are live/prod only.
- Community has built several paper trading simulators (e.g. Python-based orderbook replay with local matching).

**Implication for polytrader**:
- We **must** implement our own high-fidelity paper trading engine as Phase 0/1 deliverable.
- The simulator will consume **live public data**:
  - Gamma API (markets, events, prices, volumes)
  - CLOB public endpoints (order books, recent trades, tickers, prices) — no auth required for reads.
- Simulation fidelity requirements:
  - Realistic taker fees (Polymarket ~0.5–2% depending on volume/rewards; confirm exact).
  - Slippage modeled from live orderbook depth + configurable impact function.
  - Latency simulation.
  - Partial fills, queue priority (simplified FIFO or pro-rata).
  - Resolution & payout simulation (using market outcome when resolved).
- Later: "shadow" mode where real orders are placed in parallel with paper for comparison (high risk, gated).
- Third-party paper traders (e.g. https://github.com/agent-next/polymarket-paper-trader) can be studied for techniques but we own our implementation for integration with our journal + Hermes.

### API & SDK Landscape (May 2026)

**Strong official Rust support** (preferred path):

- Primary maintained SDK: **polymarket_client_sdk_v2** (https://github.com/Polymarket/rs-clob-client-v2)
  - Built on alloy (Ethereum signing), reqwest, rust_decimal, tokio.
  - Covers: L1 wallet signature → L2 API key derivation, full CLOB authenticated trading (market/limit orders, cancellations, WS), Gamma markets client, data APIs.
  - Actively referenced in official docs.
- Older `rs-clob-client` is archived.
- Other community crates exist (polyfill-rs for perf, etc.) but we start with the official v2 SDK.

**Data / Read APIs (public, no key)**:
- Gamma: https://gamma-api.polymarket.com/ (markets, events, prices, search, etc.)
- CLOB public: https://clob.polymarket.com/ (orderbook, trades, ticker, prices, ws public)
- Data API (some require key): https://data-api.polymarket.com/

**Authentication (two layers)**:
1. **L1**: EIP-712 signature from a Polygon wallet (private key or hardware) to obtain/derive L2 API credentials (apiKey, secret, passphrase). One-time or refresh.
2. **L2**: For every trading request, 5 custom headers + HMAC-SHA256 signature over timestamp + method + path + body using the L2 secret.

**2026-05-25 L2 Auth Pivot (smallest viable UI component for paper-only learning, post-Google clarification)**: Per user verbatim clarification ("UI component to authenticate with Polymarket for the API https://docs.polymarket.com/api-reference/authentication (L2...)") + "/implement go with the plan above". Added smallest UI status/expiry + connect/derive flow (browser EIP-712 wallet sign + backend derive proxy + mem+cookie storage exact Google pt_sess pattern + dual coexist with live Google 5701dfea/978b365b dashboard layer; no Google code altered). Full paper gates + $150 + "zero effect on engine even if connected" + long-lived phrasing + heavy RISK. Wiki-first (log prepend + this integrations cross-ref + this note). See wiki/log.md top 2026-05-25 L2 58dff3a2 entry for complete details, Commands, verification, fidelity (Google preserved), credits (official docs 2026-05-25 + openclaw patterns), anti-patterns. No real trading/CLOB wiring/DB/Cargo change. Local gates (fmt/clippy/test 4/4) passed. (Future 3.4: gated real use behind AGENTS review.)

**Order Types** (CLOB):
- GTC, FOK, IOC, GTD.
- Market (immediate), Limit.
- Buy/sell Yes or No shares (binary outcome shares priced 0–1).

**Risks / Notes**:
- Real funds required even for small tests (gas + USDC on Polygon).
- Rate limits exist (document in wiki/sources as we discover).
- Rewards program for volume — may affect effective fees.

**Recommendation**:
- Depend on `polymarket_client_sdk_v2` for the **real trading path**.
- Build a **PaperTradingEngine** trait + impl that shares the same high-level interface (submit_order, cancel, get_positions, etc.) but executes against simulated book + local matching engine fed by live public feeds.
- This allows the rest of the agent (strategy, UI, journal, Hermes) to be mode-agnostic.

## Architecture Overview

### Services / Deployables

1. **polytrader** (main Rust binary + Dioxus)
   - Long-running Tokio runtime.
   - Components:
     - Market data ingester (Gamma polling + CLOB WS/public feeds).
     - PaperTradingEngine (core simulation).
     - (Future) RealTradingAdapter (wrapping the official SDK, behind kill switch).
     - Decision / strategy engine (initially simple rules + LLM-assisted; later more sophisticated).
     - Journal writer (to Postgres).
     - Dioxus web server (dashboard: markets browser, paper portfolio, open sim orders, P&L charts, Hermes reflections, config).
   - Exposes: HTTP (Dioxus), perhaps gRPC or internal for Hermes.
   - Config: trading mode (paper only initially), risk params, LLM endpoint, wallet (read-only for paper).

2. **hermes** (Rust binary, separate deployment)
   - Periodic + event-driven loops:
     - Post-resolution market reviews (outcome vs. pre-trade probabilities, edge quality).
     - Trade attribution & P&L explainability.
     - Anomaly / bug detection in journal.
     - Strategy experiment proposals.
     - Wiki updates / synthesis (LLM calls to propose patches to markdown).
     - Research tasks (e.g. "study historical resolution accuracy of certain categories").
   - Writes structured reflections + action items to DB + wiki (via PR or direct file update in dev).
   - Can be triggered manually from UI.

3. **postgres** (CloudNativePG, 2 replicas)
   - Primary + hot standby.

## 2026-06-03 Tranche: Operator-Facing Approval Workflow for Gated Real Orders (Approval UX)

**Goal**: Close the UX gap so an operator (L2 secret + POLYTRADER_ENABLE_REAL_ORDERS + KILL_SWITCH_OPEN) can create the required journaled human_approval_event_id + final_review_decision_event_id (with risk/collateral snapshots at approve time, operator binding) via UI or simple curls — no raw journal INSERTs — then feed the UUIDs into submit-facade to exercise the (already wired) gated real path end-to-end under all safety gates.

**Wiki-first (non-negotiable)**: Prepended log.md entry, created wiki/decisions/real-order-approval-flow.md, updated schema.md (enriched event payloads), runbooks (extended), this plan, decisions/README cross-ref. All before heavy src edits.

**Approach (smallest, fidelity-preserving)**:
- Enhance existing POST /clob/order-intent/human-approval and /clob/final-review-decision (compatibly; accept optional snapshot fields) to capture + embed current risk_snapshot (intent-derived + limits) + collateral_snapshot (via existing builders) + operator (AuthUser) into journal payloads at approve time. (Actual paths used; plan short names were informal.)
- Add/enhance minimal GET pending lists (e.g. /clob/order-intent/human-approvals symmetric to final-review-decisions) returning recent events with evidence/ids.
- Minimal Dioxus SSR panels (new or enhanced "Pending Human Approvals" + "Final Review Queue" cards) + vanilla JS: list recent candidates/audits, approve buttons that fetch current readiness/collateral/risk evidence, POST with snapshots, surface returned UUIDs copyably for submit form or curls. Wire final id into submit-facade JS too. Preserve all prior verified SSR ids/markers/hooks/base-href/subpath.
- In submit-facade + LiveOrderSendRequest path: ensure ids from these enriched approvals flow (already), optionally surface snapshots in gate_report for reval note at dispatch (smallest; hard pre-dispatch "clob_live_order_intent_pre_dispatch" journal preserved).
- Gated sender reval (non-zero ids + envs + kill) unchanged.
- Risk anti-staleness: snapshot at *approve* (for Hermes attribution + later reval); note in dispatch if practical.
- New tests: 401 unauthed on approval creation paths (AuthUser), happy journal write + payload contains snapshot/ids/operator, validation happy with fresh approval ids, extend gated positive under TEST_ENV_LOCK + unlocks (auth sim).
- Hermes: existing consumption of the kinds + live_* (snapshots available in payloads for future P&L/wiki); minimal update if needed for notes.
- Verify: extend for new UI markers + 401 negatives + positive probes (with snapshots) without relaxing any prior requires.
- No new event kinds (reuse/enrich), no migration, no auto-real, no default behavior change, no paper regression, Decimal, heavy RISK comments, native-l2 for sign, etc.

**Safety**: All real path still fail-closed by default; paper_only + real_orders_enabled:false + boundary network_present:false exercised; explicit human + final + unlocks + kill + L2 + reval + pre-journal + risk gates mandatory. Snapshots make the human approval evidence richer for audit/attribution.

**Deliverables**: Updated wiki (first), src (server handlers/requests/validations/UI, clob if gaps for snapshot reval, tests), deploy/verify, docs/plan, runbooks. 0 open review issues after implement-review-fix.

**Status in this plan**: The "place where we can start placing actual orders" tranche (gated sender) is complete+hygiene'd; this is the follow-on to make the approval prerequisite *operator usable*.

Follow-up (post 2026-06-06 Hermes richer attribution + hygiene per log "Ready for next (e.g. UI polish or backtest per wiki follow-ups)"): smallest additive UI polish tranche on the existing approval panels/lists (better enriched risk/coll snapshot evidence display + hints + approval_time proxy in rows, tighter Copy/Use ID integration with submit facade, light Hermes approval_attribution hints via reuse of existing safety-loop fetch + note el (no new queries/ids); see wiki/log.md 2026-06-06 UI polish entry + append to decisions/real-order-approval-flow.md. Pure UI (only src/ui/app.rs + wiki), local cargo/SSR test sufficient, 100% prior surfaces preserved. Advances usability of the Hermes-attributing gated path.

Post-UI-polish follow-up (2026-06-06 per log top "Current State ... Ready for next (e.g. UI polish or backtest per wiki follow-ups)" + goals wiki 5-min DR tracking): smallest Hermes 5-min Decision Report cadence integration (additive "decision_reports_considered_24h" stub + dr_cadence note/sub in clob_safety_loop + metrics/approval_attribution + dedicated test in existing hermes.rs only; reuses approval tranche patterns exactly ("paper proxy only", "stub", "pending...", robust unwraps, heavy RISK/AGENTS, "see wiki/..."); makes wiki-tracked DR (Fusion/DecisionReport net_edge skeleton in strategy/, "PRIMARY signal for deliberate 5-min tier", Hermes to extend for DRs) visible/usable in self-imp reflections (no UI/SSR change so 100% polish markers + all SSR contains preserved exactly; no new files/kinds/migs; no deploy impact). Advances self-imp (Hermes + wiki first-class) + lightly ties to gated approval usability (DR edge for future proposals/quality). See new top wiki/log.md entry, append to decisions/real-order-approval-flow.md, updated README index. Local fmt/clippy/test (hermes + native gated clob, --threads=1) green. All per AGENTS.

Post-DR-stub follow-up (2026-06-06 per log "Ready for next" + goals "5-min layer" + "Extend do_reflection..." + strategy "skeleton" + project plan): wire the actual minimal 5-min Decision Report generator (additive spawn in existing main.rs + extend JournalWriter for record_journal_event reuse of events jsonb following server pattern + call FusionEngine::fuse_net + journal 'decision_report' shaped with net_edge_after_fees as PRIMARY per goals/strategy; extend hermes to real COUNT query replacing 0 stub + update cadence/attribution/summary/recs; no UI/SSR/deploy; preserves 100% prior incl all polish/DR-stub markers/SSR contains + paper/fail-closed/gated/L2/SSR subpath etc). Makes self-imp data actionable + advances 5-min DR cadence (heavily tracked). Wiki-first prepend/append to existing (log + real-order-approval-flow + README index + this plan note). Local cargo fmt/clippy/test green (threads=1 + native gated). See new log top entry + append in decisions/real-order-approval-flow.md. All per AGENTS.

Post-generator follow-up (2026-06-06 per log top "Current State ... Ready for next (e.g. start tax journal or backtest harness per wiki-tracked list in goals-and-operational-cadence.md + decisions/real-order-approval-flow + project-plan 'Ready for next / backtest') + goals "Extend `do_reflection` to also read recent decision reports" + "Query recent ... and all decision reports" + "backtest" + decisions follow-ups): smallest extend of do_reflection (in existing src/bin/hermes.rs only) to read recent decision reports (direct query sample of 'decision_report' jsonb for net_edge_after_fees PRIMARY + generated_by; include in decision_report_cadence metrics + local_summary + recs; dedicated mock test for new attribution/read path per briefing; reuses robust patterns). Makes the now-producing 5-min DRs actionable in self-imp loop for attribution (DR net vs paper outcomes/approvals) + starts backtest harness per wiki-tracked (no new files/kinds/UI; additive only; 100% prior surfaces incl all polish/DR-stub/SSR contains + paper/fail-closed/gated/L2 etc preserved exactly; no deploy). Wiki-first (prepend/append to existing log + real-order-approval-flow + README index + this plan note). Local cargo fmt/clippy/test (hermes + native gated --threads=1) green. See new top wiki/log.md entry + append in decisions/real-order-approval-flow.md. All per AGENTS.

See wiki/log.md (2026-06-03 entry + 2026-06-06 UI polish + hermes + new DR cadence + generator wiring), wiki/decisions/real-order-approval-flow.md, wiki/schema.md for full details + invariants. All per AGENTS.md.
