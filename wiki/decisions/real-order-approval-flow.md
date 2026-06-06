# Real-Order Approval Flow (Human + Final Review for Gated CLOB)

**Date**: 2026-06-03 (wiki-first per AGENTS.md; implemented as operator UX tranche after gated sender hygiene)

**Status**: Implemented (smallest viable; existing event kinds enriched; no new kinds)

## Context / Problem
After the gated real CLOB sender + submit-facade (GatedRealClobLiveOrderSender + pre-dispatch hard journal + reval of non-zero human_approval_event_id + final_review_decision_event_id + env unlocks + kill), the gated path for real orders required valid journaled approval events. However, creation of those events was only via raw SQL, probe curls in verify, or the existing audit-oriented handlers that were documented as "facade validation only / audit only / does not authorize live".

Operators (with L2 secret + explicit unlocks + kill) had no user-friendly (UI or simple curl) way to create the required journaled events with evidence, risk snapshots, and operator binding. This made "we can start placing actual orders" not actually usable without bypassing the spirit of human-in-the-loop journaled gates.

The two-role model (human approval for specific intent + final review decision on aggregate readiness) was already wired for the facade/gated reval, but UX gap remained.

## Decision
Enhance the *existing* approval creation surfaces (POST /clob/order-intent/human-approval and /clob/final-review-decision) + add minimal symmetric GET lists + minimal Dioxus panels + JS approve buttons, to allow an operator (AuthUser bound) to:

- Capture current risk/collateral snapshots (by calling/embedding existing builders + intent-derived projected risk at approve time) into the journal payload of the approval events.
- Record human approval (per-intent, subject hash, 15m expiry, "approve_facade" decision) and final review decision (linked to readiness, any non-reject decision per prior contract) with full evidence.
- Receive the journal_event_id (UUID) prominently (copyable in UI, returned in JSON for curls).
- Use those UUIDs (non-zero) in submit-facade POST (along with confirm_real...) ; facade validates + hard pre-journals "clob_live_order_intent_pre_dispatch" carrying the ids; Gated sender revals the ids + envs + kill immediately before any place_limit_order (native-l2 sign+POST /order).
- Risk re-snapshot at approve for later attribution/reval (anti-staleness note at dispatch time if practical); snapshots carried in approval payloads and referenced in pre-dispatch journal.
- All under AuthUser (operator email bound into payloads); 401 fail-closed on priv paths.
- Defaults remain 100% paper/fail-closed (real still requires explicit POLYTRADER_ENABLE_REAL_ORDERS + KILL + valid ids + L2 + risk/collateral positive at facade time).
- No auto real, no change to paper engine, L2 derive, SSR subpath <base href="/polytrader/">, existing verified markers/ids/JS hooks, Decimal everywhere, heavy RISK comments, TEST_ENV_LOCK for env tests, pre-deploy native-l2 + --threads=1, no ||true.

The human approval event kind remains `clob_order_human_approval` (enriched payload); final remains `clob_final_review_decision` (enriched). Submit still requires both ids for the real gated branch. Hermes already counts the kinds + live pre/dispatch; snapshots feed future P&L/attribution/wiki proposals (gated).

## Alternatives Considered
- New routes (/clob/human-approval etc): more duplication; rejected for smallest (evolve existing compatibly).
- New journal kinds for "real" approvals: unnecessary churn (existing suffice and already consumed by hermes safety + submit validate); enrichment is backward compat.
- Full pending queue with auto subject matching etc: larger; kept minimal (recent lists + UI buttons that snapshot current + manual copy id to submit form).
- Snapshot only at facade time (no at-approve): loses the "at human review time" evidence for Hermes attribution and staleness detection.
- Auto-approve or relaxing id requirement: violates AGENTS + prior gated design + fail-closed.

## Rationale
- Wiki-first + AGENTS: documented before src; updates to log/schema/decisions/runbooks/plan.
- Usable by operator: UI panels (add "Pending Human Approvals" + enhance "Final Review Queue" + copyable ids) or curls; no raw INSERT.
- Safety preserved: snapshots for reval/audit; pre-dispatch hard journal; sender reval; AuthUser; env+kills+L2; paper default; boundary exercised as FailClosed in default reports.
- Observability: snapshots + operator + approval time in journal events; pre + post dispatch kinds.
- Fidelity: extends the exact post-hygiene state (ids as String in LiveOrderSendRequest, #[serde(default)], facade mapping, Gated checks only non-zero+envs+kill, no change to default behavior).

## How UUIDs Flow
1. Operator loads current readiness (collateral, final-review-readiness, order-placement-readiness for risk/evidence).
2. POST human-approval (with intent + decision + note + confirm + optional/current snapshots) → journal `clob_order_human_approval` (payload now includes risk_snapshot_at_approval, collateral_snapshot_at_approval, operator from AuthUser, expires_at, subject_hash, ...). Returns journal_event_id.
3. POST final-review-decision (with final_review_event_id from readiness + decision + note + confirm + snapshots) → journal `clob_final_review_decision` (enriched with snapshots at decision time, operator, linked readiness, boundary evidence). Returns journal_event_id.
4. POST submit-facade (with human_approval_event_id + final_review_decision_event_id + confirm_real + post dry-run etc) → server validates events (subject for human, existence for final), builds gate_report (now can surface approval-time snapshots for comparison), if no blockers + real envs: hard journal `clob_live_order_intent_pre_dispatch` (with the ids + intent), invoke Gated::send (rechecks !zero ids + envs + kill), which does last-minute rebuild + place_limit_order (sign+POST) if all pass.
5. Results journaled (dispatched or rejected); hermes consumes.

## Journal Event Shapes (Enriched)
See wiki/schema.md updates. Payloads for approvals now include at-approve-time:
- "risk_snapshot": {projected_notional, limits from envs/bankroll, intent details, ...}
- "collateral_snapshot_at_approval": latest clob_collateral_readiness report or fresh
- "operator": from AuthUser (bound)
- "approval_time": iso
- (human still has subject_hash, expires, approved_for_facade; final has readiness_blockers, live_sender_boundary_fail_closed etc.)

Never contain secrets.

## UI / Endpoint Contract (Minimal)
- Existing POSTs enhanced (accept optional *_snapshot fields via serde default; always capture/embed even if not sent).
- New/enhanced GETs: /clob/order-intent/human-approvals (recent list with ids, subject, decision, operator, snapshots summary); existing /clob/final-review-decisions already serves.
- Dioxus: new/added cards "Pending Human Approvals" + "Final Review Queue" (or integrated) using fetch + tables of recent; approve buttons fetch current /clob/collateral-readiness + /clob/order-placement-readiness (or final-readiness), POST with snapshots + operator_comment, on success show/copy "Use this UUID: <id> for human_approval_event_id / final_review_decision_event_id in submit-facade".
- Submit UI JS enhanced to also wire latest final id (if recorded) + human.
- All SSR subpath, existing ids ("clob-final-review-*-panel" etc), "Record Facade Approval" button, hooks preserved (new markers added + verify updated).
- Curls example (with auth sim header for cluster):
  curl -H 'x-forwarded-user: operator@polytrader.local' ... POST human-approval with snapshots embedded.

## Safety Invariants (Non-Negotiable)
- Paper default, real_orders_enabled:false , ready:false , network_present:false (boundary) unless explicit unlocks+kill+valid ids.
- Journal *before* any network (hard in facade for pre_dispatch; reval in sender).
- Human + final ids required + re-validated (subject/expiry for human; existence for final) for real dispatch.
- Snapshots at *approve* time for anti-staleness + Hermes attribution when real fills later.
- AuthUser on all priv (401 unauthed negatives in verify + tests).
- No auto, no default enable, L2 + native-l2 for sign, Decimal, heavy comments.
- Fail-closed everywhere (NoOp boundary exercised in defaults).
- Wiki updated first + reconciled; tests + observability added; runbooks/plan updated.

## Consequences / Follow-ups
- Hermes will see enriched payloads in existing clob_safety_loop counts for human_approval + final + live_* ; future reflections can attribute real P&L back to specific approval snapshots + operator.
- Later: staleness reval using snapshot vs dispatch-time collateral in facade/gated + journal note; full pending intent queue; per-user auth binding stronger.
- If real fills: new journal kinds for fills would be added then (with cross-ref to approval ids).
- Deploy/verify extended for new UI markers + 401s + positive probes using snapshots (no relax of prior).
- All per AGENTS: safety first, self-improving (wiki), observable/journaled, primarily Rust, Decimal, paper until explicit human gates + review.

## Verification (post-impl)
- make pre-deploy-check (fmt --check, clippy -D, test --threads=1, native-l2 gated tests)
- cargo test ... clob/server filters
- Manual/UI curls on pod or local: create approvals with snapshots, copy UUIDs, submit-facade with unlocks+kill (real path reaches dispatch or expected err), observe pre journal + snapshots in payloads, 401 unauthed, SSR markers, verify greps pass.
- Hermes /clob/hermes-safety-loop sees counts.
- Surfaces preserved: paper, L2, subpath, fail-closed boundary, etc.

Cross-refs: wiki/log.md (this tranche + 2026-06-06 hermes extension), wiki/schema.md, docs/project-plan.md, runbooks/l2-private-key-secrets.md (approval section), AGENTS.md, prior gated log entry 2026-06-02.

(Implementation details in the corresponding log entry; code changes minimal per plan.)

## Hermes Closed-Loop Attribution Extension (2026-06-06)

**Wiki-first (per AGENTS)**: Appended to this existing decision doc (no new file) after reads; plan/prepend to log.md preceded src change to hermes only.

Richer Hermes consumption (in src/bin/hermes.rs do_reflection + load_clob_safety_loop_snapshot) of the enriched approval events (clob_order_human_approval + clob_final_review_decision now with risk_snapshot_at_approval, collateral_snapshot_at_approval, operator, approval_time from 2026-06-03 UX) + correlation to subsequent clob_live_order_intent_pre_dispatch / clob_live_order_dispatched (via ids carried in pre-dispatch live_order_send_request.human_approval_event_id etc) + proxy for real fills/P&L when exercised under gates. Adds counts (approvals_with_snapshots_24h etc), safety metrics (approval_to_pre_dispatch_rate, dispatches_from_approved_24h, hermes_approval_gap), attribution in metrics/summary/recs (approved_edge_net_fees net-of-fees using paper proxy + risk_snapshot for edge; approval drag from approval_time to dispatch; outcome_vs_approval_decision stub pending resolution data), and specific low-risk wiki proposals (when HERMES_AUTONOMOUS_WIKI_PROPOSALS=lowrisk) derived from the approval attribution data. Smallest: reuse existing kinds/queries/paths (robust or(0) fallbacks, no crash on legacy); heavy RISK comments; tests added; no impact to trading paths or prior surfaces. See 2026-06-06 log entry for plan/evidence/executed + schema for event shapes. Hermes (first-class per AGENTS) now self-improves on approval quality + real P&L attribution (net fees/decision drag) for gated real path.

Consequences: better proposals for approval flow/fee/strategy tuning; when real fills + resolutions journaled, attribution will link directly to approval snapshots for "edge after fees realized vs approved". Still paper default; no auto; human review for high-impact.

## UI Polish for Approval Queue Practicality (2026-06-06, post-Hermes attribution)

**Wiki-first (per AGENTS)**: Appended to this existing decision doc (no new file) after multiple reads/greps on it + log "Ready for next" + project-plan + src/ui/app.rs (exact panels/JS/SSR asserts) + server/hermes/clob shapes; plan/prepend to log.md + this append + README/plan updates preceded *all* src edits (only ui/app.rs touched, additive).

Smallest additive polish (no new files/ids/hooks/markers/routes/queries; all old "Pending / Recent Human Approvals (for Gated Real CLOB)", clob-human-approvals-*-ids, "Copy/Use ID for Submit", recordHumanApprovalIntent, updateHumanApprovalsList, clob-final-review-*-panel, use*ForSubmit, hermes panel hooks, SSR asserts, <base>, paper/fail-closed/L2 etc preserved 100%) to make the now-Hermes-attributing approval lists more practical/ergonomic for operators per log.md "Ready for next (e.g. UI polish...)" + real-order-flow Consequences ("Later: full pending...") + "ties self-imp + usability":

- Better evidence display in rows: human list (using already-returned full risk_snapshot_at_approval + collateral_snapshot_at_approval from /human-approvals handler) now renders richer "Risk/Coll Snapshot Summary (enriched)" hints (projected + coll positive flag + [snap] marker); created_at serves as approval time proxy (enriched approval_time is in journal payload). Final list rows lightly annotate snap presence from payload (parity with human enrichment 2026-06-03).
- Existing "Refresh Human Approvals List" button (and setTimeout calls) preserved + leveraged; no auto added (keeps fidelity).
- Tighter "Copy/Use ID for Submit" integration: improved guidance text written to existing dry-run-result + notes (explicitly references "Submit Facade Check" button in "CLOB Dry-Run Intent card" + confirm_real under unlocks + window.latest* pairing with final/human).
- Light Hermes approval attribution hints (without new queries): extended the existing updateHermesSafetyLoop (which already fetches /clob/hermes-safety-loop, now carrying approvals_with_snapshots_24h / pre_dispatches... / hermes_approval_gap / approval_attribution from 2026-06-06 hermes tranche) to surface the new keys in the Hermes panel lines + append a concise " | Hermes attr: snaps=.. gap=.." hint to the *existing* clob-human-approvals-note el under the approvals card (reuses the query that was already scheduled on load; no new fetch, no new el/id). Ties the Hermes closed-loop directly to the approval queue UX.

No change to server (snaps already exposed), clob shapes, hermes.rs (keys already emitted), submit facade, or any risk paths. Advances operator usability of the gated real path (human+final+submit under unlocks) now that Hermes attributes the snaps/ids/pre-dispatches for P&L/drag (stubs until real fills). All per AGENTS (wiki-first, observable, paper-only, no auto, self-improving via Hermes data surfaced, smallest).

See wiki/log.md (this tranche + 2026-06-06 hermes + hygiene) for plan/reads/evidence/executed (local fmt/clippy/test green; SSR fidelity preserved). Cross-refs unchanged.

## Hermes 5-min Decision Report Cadence Extension (2026-06-06 continuation)

**Wiki-first (per AGENTS)**: Appended to this existing decision doc (no new file) after multiple reads/greps on it + log "Ready for next (e.g. ... or backtest per wiki follow-ups)" + project-plan + goals (DR details) + src/hermes (stubs for 5min/DecisionReport) + strategy (full DecisionReport + fuse_net); plan/prepend to log.md + this append + README/plan updates preceded *all* src edits (only hermes.rs touched, additive stub only).

Smallest additive extension of Hermes closed-loop (in src/bin/hermes.rs load_clob_safety_loop_snapshot + do_reflection + metrics/approval_attribution + tests) to surface the wiki-tracked 5-min Decision Reports cadence (heavily detailed in goals-and-operational-cadence.md as the "Trader" / opportunity layer using FusionEngine + DecisionReport net_edge_after_fees as PRIMARY signal for deliberate tier; "Future Dioxus Live Opportunities / Decision Report panel"; Hermes to read DRs + attribute per-signal; approval queue orthogonal per goals). Adds "decision_reports_considered_24h" stub (paper proxy 0, with comment "pending full 5min generator/journal from strategy; see DecisionReport + fuse_net already producing net edge + decision_report_summary; net DR edge will inform future approval quality in self-imp loop") to clob_safety_loop (for visibility in existing hermes panel + reflections) + dr_cadence sub in metrics/attribution context (reusing exact robust .unwrap_or(0), "paper proxy only, append-only, evidence-only, no new privileged, reuse existing", "2026-06-06", "see wiki/..." patterns from prior Hermes/approval tranches). No new event kinds (DRs will use jsonb in existing events when wired), no UI/SSR change (preserves 100% of polish markers like "Risk/Coll Snapshot Summary (enriched)", "Hermes attr: snaps=...", hasSnap, all SSR contains, update* etc + prior surfaces), no files created. Advances self-improving system (Hermes first-class + wiki) by making the 5-min DR cadence (per log "Ready for next / backtest follow-ups", project-plan, goals) part of the closed-loop data used for P&L/strategy/wiki proposals (gated), lightly tied to approval flow (shared self-imp observability for gated real path edge quality). All per AGENTS (wiki-first, observable/journaled, primarily Rust, paper-only, Decimal, no auto real, heavy RISK comments, update wiki as part, smallest).

See new top entry in wiki/log.md (2026-06-06 DR cadence) for plan/reads/evidence/executed + fidelity recon (reads preceded edits; wiki M before src). Cross-refs to goals-and-operational-cadence.md + strategy/mod.rs + real-order-approval-flow consequences for Hermes on approvals.

## 5-min Decision Report Generator Wiring + Real Hermes Consumption (2026-06-06 continuation)

**Wiki-first (per AGENTS)**: Appended to this *existing* decision doc (no new .md) after multiple read_file + grep (on log top DR stub entry + "Ready for next (e.g. ... or backtest per wiki follow-ups)" + "Current State" + plan/evidence/recon, project-plan DR stub follow-up note, this file's prior DR stub section + Hermes/UI polish, decisions/README, goals-and-operational-cadence.md (5-min details + Extend do_reflection + Future Dioxus + approval orthogonal), wiki/schema.md, src/bin/hermes.rs (stub + test + comments), src/strategy/mod.rs (DecisionReport + FusionEngine + fuse_net + "PRIMARY signal for 5-min tier"), src/main.rs (ingest spawn, journal, strategy mod), src/journal/* (writer for extension), src/server.rs (record_journal_event pattern + hermes-safety from reflections + existing fuse_net in strategy candidates + clob_safety from metrics), src/ui/app.rs (all old+polish+DR stub markers for 100% preserve: "Risk/Coll Snapshot Summary (enriched)", "Hermes attr: snaps=...", hasSnap, clob-hermes-safety-loop-panel, update*, "Pending / Recent Human Approvals...", paper_only, real_orders_enabled, l2-chip, <base>, SSR contains etc), src/clob/* (gated ids/pre-dispatch/LiveOrderSendRequest preserved), src/ingester/* (data feed for future), before *any* search_replace. Record of reads (multiple interleaved) in agent thinking. Only existing files edited (wiki + src/journal/writer.rs + src/main.rs + src/bin/hermes.rs). Smallest additive tranche.

Smallest viable that makes self-imp data (DR cadence stub + enriched approvals) *actionable* and advances the wiki-tracked 5-min DR cadence (heavily in goals "Trader" layer, "PRIMARY signal net_edge_after_fees", "Extend do_reflection to also read recent decision reports", "Future Dioxus..." note; per plan post-stub follow-up, log "Ready for next", strategy skeleton): wire actual (minimal) 5-min Decision Report generator (additive spawn timer in existing main.rs after ingest spawn, using strategy::FusionEngine + DecisionReport + fuse_net on recent market_data snapshots from DB (piggy patterns from server's build_strategy_paper_candidates), conservative FeeContext, journal via extended JournalWriter using exact server record_journal_event pattern to 'decision_report' reuse of journal.events jsonb payload with report/net/attr); extend hermes load_clob_safety_loop_snapshot (replace hardcoded 0 stub with real COUNT query on event_type='decision_report' >= period, robust unwrap_or per approval patterns) + lightly update notes/recs/decision_report_cadence/metrics/summary in do_reflection + existing dedicated test (mock still passes); no new files/kinds/migs/UI/SSR/deploy (hermes clob_safety now carries real count -> surfaces in /clob/hermes-safety-loop via existing reflection metrics path + hermes panel, without touching ui/app.rs or server -> 100% polish/old markers/SSR contains preserved exactly); no auto submit (per goals "optional behind flag"); heavy RISK/AGENTS comments. Advances self-imp (Hermes + wiki first-class now has real DR cadence data for attribution/proposals) + 5-min tier per current log top + goals/plan/strategy "skeleton" + "PRIMARY"; orthogonal to approval queue but shared observability for gated path quality. All per AGENTS (wiki-first, safety first/paper-only, observable/journaled, primarily Rust, update wiki as part, Decimal, no auto real, trading code RISK commented, smallest, preserve all prior verified incl gated fail-closed "gated_real_sender_present":true but network_present:false/"rejected_*", L2, pre-dispatch, 401s, TEST_ENV_LOCK+threads=1+native, SSR subpath+<base>+*every* marker, no migs/secrets/new priv).

See new top wiki/log.md entry (this tranche) for plan/reads/evidence/executed + fidelity recon (wiki M first, reads preceded src edits). Cross-refs goals-and-operational-cadence.md + strategy/mod.rs + schema (jsonb for decision reports) + real-order-approval-flow (Hermes ext).