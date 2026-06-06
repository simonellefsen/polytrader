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