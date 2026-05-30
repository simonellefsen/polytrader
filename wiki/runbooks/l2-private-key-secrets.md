# L2 Private Key Secrets Management (Polymarket)

This runbook explains how to safely provide your `POLYMARKET_PRIVATE_KEY` (the L1 key used for EIP-712 signing to derive L2 CLOB credentials) to the running polytrader pods.

## Why this exists

- The app performs **auto L2 derivation on startup** when the key is present and the binary is built with `native-l2`.
- In local development we use `.env.local` + `dotenvy`.
- In Kubernetes we **never** want the raw key in the image, in git, or in plain environment variables visible in `kubectl describe`.
- We follow the same secure pattern already used for the database password (`DATABASE_URL_FILE` + mounted Secret).

## One-time / local setup

1. Put the key in your (git-ignored) `.env.local`:

   ```env
   POLYMARKET_PRIVATE_KEY=0xYourRealKeyHere
   ```

2. Local app startup now reads `.env.local` before `.env`, so `POLYMARKET_PRIVATE_KEY` works for local runs without exporting it into the shell.

3. When you run:

   ```bash
   make k8s-apply
   ```

   You will be prompted:

   ```
   ==> Populate/update POLYMARKET_PRIVATE_KEY from .env.local into the cluster now? [y/N]
   ```

   Answering `y` will:
   - Create or update the runtime-only `polytrader-l2-auth` Secret
   - Restart the `polytrader` Deployment (so the new pod picks up the key via the file mount)
   - If the image was built with `native-l2`, the pod will log something like:
     `L2 credentials successfully derived on startup using server key`

   The base kustomize manifests intentionally do **not** declare this Secret with a placeholder value. This prevents `make k8s-apply` / `make k8s-deploy` from overwriting the real cluster secret with `REPLACE_WITH...` during routine deploys.

## Manual / one-off update

```bash
make k8s-set-l2-key
```

This does the same thing without going through a full apply.

## How it works under the hood

### Kubernetes side (`deploy/k8s/base/`)

- `secrets.yaml` does **not** declare `polytrader-l2-auth`; it is created only by `make k8s-set-l2-key` or an equivalent secret-management flow.
- `polytrader.yaml` (Deployment):
  - Sets env var:
    ```yaml
    - name: POLYMARKET_PRIVATE_KEY_FILE
      value: /etc/secrets/l2-auth/private-key
    ```
  - Mounts the secret:
    ```yaml
    volumeMounts:
      - name: l2-auth-secret
        mountPath: /etc/secrets/l2-auth
        readOnly: true
    volumes:
      - name: l2-auth-secret
        secret:
          secretName: polytrader-l2-auth
          optional: true
          items:
            - key: private-key
              path: private-key
    ```

  The optional mount lets the app start safely without L2 credentials on a fresh dev cluster. In that state, authenticated CLOB diagnostics remain disconnected until the secret is created.

### Application side

- `src/server.rs` (`try_auto_derive_l2_on_startup` and the derive handler) checks:
  1. `POLYMARKET_PRIVATE_KEY_FILE` → reads the file
  2. Falls back to direct `POLYMARKET_PRIVATE_KEY` env var (useful locally)
- The key is **never** logged.
- On success the L2 session and derived secret tuple are populated in process memory. The UI chip updates to:
  - `Connected ... (server key • auto)` on startup, or
  - `Connected ... (manual)` when using the button.
- `/l2/status` falls back to the startup server-key session when there is no browser cookie yet.
- `/l2/disconnect` clears the active in-memory session and derived credentials.

## Read-only authenticated CLOB check

After L2 credentials are derived, the app performs a safe authenticated read through:

```bash
curl http://localhost:8080/clob/status
# or behind the public subpath:
curl http://localhost:8080/polytrader/clob/status
```

This endpoint signs `GET /data/orders` with the derived L2 credentials and reports whether Polymarket accepted the request. It always returns `paper_only=true` and `real_orders_enabled=false`; there is still no real order placement or cancellation route in the app.

For the fuller diagnostic snapshot:

```bash
curl http://localhost:8080/clob/account
# or behind the public subpath:
curl http://localhost:8080/polytrader/clob/account
```

This reads open orders plus collateral balance/allowance via `GET /balance-allowance?asset_type=COLLATERAL&signature_type=...`. `POLYMARKET_SIGNATURE_TYPE` defaults to `0` (EOA); set it to `1`, `2`, or `3` only after verifying the account's proxy/safe/deposit-wallet funder setup.

For a gate-oriented diagnostic:

```bash
curl http://localhost:8080/clob/preflight
# or behind the public subpath:
curl http://localhost:8080/polytrader/clob/preflight
```

This endpoint reports blockers that would prevent any future real-order path. As of this phase, `ready_for_real_orders` is always `false` because the binary intentionally has no real order route and no human approval workflow.

For a single aggregate read-only diagnostic:

```bash
curl http://localhost:8080/clob/diagnostics
# or behind the public subpath:
curl http://localhost:8080/polytrader/clob/diagnostics
```

This endpoint returns `status`, `account`, and `preflight` sections from one authenticated account snapshot. It is still read-only, always reports `paper_only=true` and `real_orders_enabled=false`, and never places, cancels, signs, submits, refreshes allowances, or mutates balances.

For the remaining external wallet blockers:

```bash
curl http://localhost:8080/clob/collateral-readiness
# or behind the public subpath:
curl http://localhost:8080/polytrader/clob/collateral-readiness
```

This endpoint returns the active wallet address, signature type, CLOB-reported collateral balance, allowance entries, positive allowance count, blockers, and operator actions. It is read-only and must always include `request_sent=false`, `post_order_called=false`, and `post_orders_called=false`. It does not fund collateral, approve allowances, refresh allowances, sign orders, submit orders, cancel orders, or mutate balances.

Successful reads are also written to `journal.events` with `event_type='clob_collateral_readiness'` so Hermes can track the external wallet blockers over time. The journal payload is a no-send audit snapshot only; it must not contain private keys, L2 secrets, full signatures, or HMACs.

For a single operator rollup that combines CLOB diagnostics with paper dry-run review health:

```bash
curl 'http://localhost:8080/clob/operator-status?limit=50'
# or behind the public subpath:
curl 'http://localhost:8080/polytrader/clob/operator-status?limit=50'
```

This endpoint returns `operator_status`, `clob`, `review`, `final_review`, and `recommended_next_actions` sections. `final_review.coverage_gap_probe` is a compact 50-decision gap-only probe that mirrors the focused gap audit without embedding full decision payloads. The probe also reports `oldest_gap_created_at`, `newest_gap_created_at`, `oldest_gap_age_seconds`, `newest_gap_age_seconds`, and `seconds_until_all_gaps_age_out_of_24h` so operators can see when legacy/malformed final-review evidence gaps will age out of the Hermes 24h coverage window. Use `active_24h_gap_count`, `expired_24h_gap_count`, `active_24h_gap_status`, `seconds_until_active_gaps_age_out_of_24h`, and `active_gaps_age_out_at` to distinguish gaps that still affect Hermes' current 24h reflection from older historical audit rows kept for traceability. `final_review.hermes_gap_alignment` compares that active app-side gap count with Hermes' latest missing-evidence count and reports `matched_clear`, `matched_active_gaps`, `hermes_reflection_stale`, or a mismatch/staleness status; it is operator consistency metadata only. It also reports `hermes_reflection_age_seconds`, `hermes_reflection_stale_after_seconds`, `hermes_reflection_is_stale`, and `hermes_reflection_freshness_status`; the default stale threshold is 600 seconds because Hermes' loop runs on a five-minute interval. When active gaps remain, the `inspect_final_review_coverage_gaps` action includes `active_24h_gap_count`, `seconds_until_active_gaps_age_out_of_24h`, `active_gaps_age_out_at`, and `hermes_gap_alignment_status` so the operator can see whether the warning is expected to age out. If alignment is available but not matched, `recommended_next_actions` includes `inspect_hermes_gap_alignment` so operators refresh the rollup and inspect Hermes timing before interpreting the warning. The endpoint is still read-only, always reports `paper_only=true`, `real_orders_enabled=false`, `ready_for_real_orders=false`, and `approved_for_real_orders=false` inside the final-review audit/probe/alignment, and never submits dry-runs, writes reviews, places, cancels, signs, submits, opens kill switches, creates live senders, refreshes allowances, or mutates balances.

To inspect how far the app is from any real order placement route:

```bash
curl 'http://localhost:8080/clob/order-placement-readiness?limit=50'
# or behind the public subpath:
curl 'http://localhost:8080/polytrader/clob/order-placement-readiness?limit=50'
```

This endpoint returns a read-only `readiness` gap report with a stage, completed gate count, blocker count, gate list, final-review audit status, live-sender boundary status, and `next_safe_step`. With the `native-l2` build, the app can build signed payload dry-runs, serialize non-submitting POST request previews, evaluate the fail-closed submit facade, record short-lived journaled human approval events for facade validation, enforce fail-closed kill-switch/per-order/total-exposure/daily-loss checks, journal submit/reject reconciliation events that prove no exchange order was created, validate token tick-size plus negative-risk metadata without sending, account for audit-only final-review decision evidence, and prove the `LiveOrderSender` boundary is still implemented only by `FailClosedLiveOrderSender` with `network_sender_present=false` and `accepted_for_network_dispatch=false`. The app still has no collateral capacity, no allowance capacity, and no explicit real-trading config unlock. The report remains `ready=false`, `ready_for_real_orders=false`, and `real_orders_enabled=false`.

To inspect the explicit live-trading unlock state:

```bash
curl http://localhost:8080/clob/real-trading-unlock-status
curl http://localhost:8080/polytrader/clob/real-trading-unlock-status
```

This endpoint reports `POLYTRADER_ENABLE_REAL_ORDER_SUBMISSION`, `POLYTRADER_REAL_ORDER_KILL_SWITCH_OPEN`, paper-mode state, and the deliberate absence of a live sender. It always returns `ready=false`, `ready_for_real_orders=false`, `real_orders_enabled=false`, `request_sent=false`, `post_order_called=false`, and `post_orders_called=false`. Successful reads are journaled as `clob_real_trading_unlock_status` for Hermes.

To generate the read-only live-sender design readiness package:

```bash
curl 'http://localhost:8080/clob/live-sender-design-readiness'
curl 'http://localhost:8080/polytrader/clob/live-sender-design-readiness'
```

This endpoint reports whether the app is even ready to consider implementing a live order sender. It always returns `paper_only=true`, `real_orders_enabled=false`, `ready_for_real_orders=false`, `ready_for_live_sender_implementation=false`, `approved_for_real_orders=false`, `request_sent=false`, `post_order_called=false`, and `post_orders_called=false`. Successful reads are journaled as `clob_live_sender_design_readiness` for Hermes. It does not create a live sender, open the kill switch, approve real trading, sign orders, submit orders, cancel orders, refresh allowances, or mutate balances.

To generate the read-only live-sender design review contract:

```bash
curl 'http://localhost:8080/clob/live-sender-design-review'
curl 'http://localhost:8080/polytrader/clob/live-sender-design-review'
```

This endpoint returns an ADR-style `live_sender_design_review` contract with the future boundary name, required pre-submit guards, post-submit accounting requirements, prohibited shortcuts, and first implementation shape. It may report `ready_for_design_review=true` when the prior readiness evidence is journaled and still blocked, but it always returns `implementation_permitted=false`, `ready_for_live_sender_implementation=false`, `ready_for_real_orders=false`, `real_orders_enabled=false`, `approved_for_real_orders=false`, `request_sent=false`, `post_order_called=false`, and `post_orders_called=false`. Successful reads are journaled as `clob_live_sender_design_review` for Hermes. It does not create a live sender, open the kill switch, approve real trading, sign orders, submit orders, cancel orders, refresh allowances, or mutate balances.

To inspect the fail-closed live-sender trait boundary:

```bash
curl 'http://localhost:8080/clob/live-sender-boundary-status'
curl 'http://localhost:8080/polytrader/clob/live-sender-boundary-status'
```

This endpoint returns `live_sender_boundary` status for the code-level `LiveOrderSender` trait. The only implementation is `FailClosedLiveOrderSender`, which returns `accepted_for_network_dispatch=false`, `submit_decision="rejected_before_network"`, `network_sender_present=false`, `request_sent=false`, `post_order_called=false`, and `post_orders_called=false`. Successful reads are journaled as `clob_live_sender_boundary_status` for Hermes. It does not create a network sender, open the kill switch, approve real trading, sign orders, submit orders, cancel orders, refresh allowances, or mutate balances.

To generate the read-only final review readiness package:

```bash
curl 'http://localhost:8080/clob/final-review-readiness'
curl 'http://localhost:8080/polytrader/clob/final-review-readiness'
```

This endpoint aggregates the latest journaled collateral-readiness, real-trading unlock-status, submit-reconciliation events, and current fail-closed `LiveOrderSender` boundary status into one blocker report for final human review. It always returns `paper_only=true`, `real_orders_enabled=false`, `ready_for_real_orders=false`, `network_sender_present=false`, `accepted_for_network_dispatch=false`, `request_sent=false`, `post_order_called=false`, and `post_orders_called=false`. Successful reads are journaled as `clob_final_review_readiness` for Hermes. It does not create approvals, send orders, mutate wallet state, or enable a live sender.

To record an audit-only final review decision against a readiness package:

```bash
curl -sS -X POST http://localhost:8080/clob/final-review-decision \
  -H 'Content-Type: application/json' \
  -d '{"final_review_event_id":"<clob_final_review_readiness journal_event_id>","decision":"acknowledge_blocked","confirm_final_review_workflow":true,"note":"operator reviewed blockers; no live trading approval","operator":"runbook"}'
```

Allowed decisions are `acknowledge_blocked`, `reject_live_trading`, and `needs_rework`. This writes `clob_final_review_decision` for Hermes only when the linked readiness packet still includes fail-closed sender boundary evidence. The response and journal payload include `live_sender_boundary_fail_closed=true`, `network_sender_present=false`, and `accepted_for_network_dispatch=false`, and always return `approved_for_real_orders=false`, `real_orders_enabled=false`, `ready_for_real_orders=false`, `request_sent=false`, `post_order_called=false`, and `post_orders_called=false`. It does not approve live trading, open the kill switch, create a live sender, or change any exchange-side state.

To inspect recent final review decisions:

```bash
curl 'http://localhost:8080/clob/final-review-decisions?limit=10'
curl 'http://localhost:8080/polytrader/clob/final-review-decisions?limit=10'
```

This endpoint is a read-only audit list over `clob_final_review_decision` events. It returns `decision_counts`, `boundary_evidence_count`, `no_network_evidence_count`, `missing_boundary_evidence_count`, `missing_no_network_evidence_count`, `all_events_have_boundary_evidence`, `all_events_have_no_network_evidence`, `coverage_status`, `coverage_gaps`, `latest_boundary_status`, `latest_decision`, and recent `events`, while preserving `approved_for_real_orders=false`, `real_orders_enabled=false`, `ready_for_real_orders=false`, `network_sender_present=false`, `accepted_for_network_dispatch=false`, `request_sent=false`, `post_order_called=false`, and `post_orders_called=false`. `coverage_gaps.events` identifies legacy or malformed audit events missing fail-closed boundary or no-network evidence; it is an audit finding only and does not unlock any sender.

For a compact gap-only view, append `gaps_only=true`:

```bash
curl 'http://localhost:8080/polytrader/clob/final-review-decisions?limit=50&gaps_only=true'
```

Gap-only mode preserves the aggregate counts but returns only compact gap rows in `events`, plus `displayed_event_count`, so operators can inspect legacy missing-boundary evidence without loading every full decision payload.

Hermes also tracks this final-review decision boundary coverage in `journal.reflections.metrics->'clob_safety_loop'` as `final_review_decision_boundary_evidence_events_24h`, `final_review_decision_no_network_evidence_events_24h`, `final_review_decision_boundary_coverage`, and `latest_final_review_decision_boundary_status`. The nested coverage object includes present and missing counts: `boundary_evidence_events`, `no_network_evidence_events`, `missing_boundary_evidence_events`, `missing_no_network_evidence_events`, and `coverage_status`. Older decisions created before boundary evidence existed may keep `complete_fail_closed_no_network_evidence=false`; that is an audit finding, not an unlock.

To surface the latest Hermes CLOB safety-loop reflection without querying Postgres directly:

```bash
curl 'http://localhost:8080/clob/hermes-safety-loop'
curl 'http://localhost:8080/polytrader/clob/hermes-safety-loop'
```

This endpoint returns the latest reflection summary, recommendations, reflection age, `clob_safety_loop`, `final_review_decision_boundary_coverage`, top-level missing-count fields, and `latest_final_review_decision_boundary_status`. It is read-only and always preserves `real_orders_enabled=false`, `ready_for_real_orders=false`, `request_sent=false`, `post_order_called=false`, and `post_orders_called=false`.

For a dry-run validation of a hypothetical real order intent:

```bash
curl -sS -X POST http://localhost:8080/clob/order-intent/dry-run \
  -H 'Content-Type: application/json' \
  -d '{"token_id":"123","side":"buy","order_type":"limit","size":"1","price":"0.5","expected_edge_bps":"500"}'
```

This validates the intent shape, conservative notional/risk limits, minimum expected edge, and the live preflight blockers. It always returns `accepted=false`, `dry_run_only=true`, and `real_orders_enabled=false`; it does not sign, submit, persist, cancel, or place anything.

Successful dry-run validations are written to `journal.events` with `event_type='clob_order_intent_dry_run'` and the response includes `journaled=true` plus `journal_event_id`.

For a read-only market metadata validation of a token:

```bash
curl -sS -X POST http://localhost:8080/clob/order-intent/market-validation \
  -H 'Content-Type: application/json' \
  -d '{"token_id":"123","side":"buy","order_type":"limit","size":"1","price":"0.5","expected_edge_bps":"500"}'
```

This calls public CLOB metadata endpoints for tick size and negative-risk status, then validates the submitted limit price against the returned tick size and legal range `[tick_size, 1 - tick_size]`. It always returns `paper_only=true`, `real_orders_enabled=false`, `request_sent=false`, `post_order_called=false`, and `post_orders_called=false`. Results are journaled as `clob_market_metadata_validation` so Hermes can include market validation coverage in the CLOB safety loop.

For a signed payload dry-run of a limit order:

```bash
curl -sS -X POST http://localhost:8080/clob/order-intent/signature-dry-run \
  -H 'Content-Type: application/json' \
  -d '{"token_id":"123","side":"buy","order_type":"limit","size":"1","price":"0.5","expected_edge_bps":"500","confirm_signed_payload_dry_run":true}'
```

This uses the active L2 session plus `POLYMARKET_PRIVATE_KEY` to build and sign a local SDK order payload when the binary is built with `native-l2`. It returns only safe summaries such as `signed_payload_built`, `signed_payload_verified`, `payload_version`, `signature_masked`, `signature_length`, `would_post=false`, `post_order_called=false`, and `post_orders_called=false`. The full signature is never returned, logged, journaled, posted, or persisted. It does not submit `POST /order`, submit `POST /orders`, cancel, refresh allowances, mutate balances, or place anything.

The deploy verifier intentionally calls the same endpoint with `confirm_signed_payload_dry_run=false`. That path should return `signed_payload_built=false` plus the `signed_payload_dry_run_confirmation_missing` blocker, proving the route and no-post guard without producing a signature during routine deploy checks.

For a non-submitting CLOB order POST request dry-run:

```bash
curl -sS -X POST http://localhost:8080/clob/order-intent/post-request-dry-run \
  -H 'Content-Type: application/json' \
  -d '{"token_id":"123","side":"buy","order_type":"limit","size":"1","price":"0.5","expected_edge_bps":"500","confirm_signed_payload_dry_run":true,"confirm_order_post_request_dry_run":true}'
```

This builds the signed payload locally and serializes the would-be `POST /order` request without sending it. Returned request details are redacted: full signatures and L2 HMACs are not exposed, and the response includes `would_send=false`, `post_order_called=false`, and `post_orders_called=false`. Successful and failed post-request dry-runs are recorded as `journal.events` with `event_type='clob_order_post_request_dry_run'` so Hermes can include the safety loop in its reflections.

The deploy verifier intentionally calls this endpoint with `confirm_order_post_request_dry_run=false`. That path should return `post_request_dry_run_built=false` plus the `order_post_request_dry_run_confirmation_missing` blocker, proving the route and no-send guard without signing or serializing a live order during routine deploy checks.

For a fail-closed submit facade evaluation:

```bash
APPROVAL_ID="$(
  curl -sS -X POST http://localhost:8080/clob/order-intent/human-approval \
    -H 'Content-Type: application/json' \
    -d '{"token_id":"123","side":"buy","order_type":"limit","size":"1","price":"0.5","expected_edge_bps":"500","decision":"approve_facade","confirm_human_approval_workflow":true,"note":"operator reviewed for submit-facade validation only","operator":"runbook"}' \
  | grep -o '"journal_event_id":"[^"]*"' | cut -d'"' -f4
)"

curl -sS -X POST http://localhost:8080/clob/order-intent/submit-facade \
  -H 'Content-Type: application/json' \
  -d "{\"token_id\":\"123\",\"side\":\"buy\",\"order_type\":\"limit\",\"size\":\"1\",\"price\":\"0.5\",\"expected_edge_bps\":\"500\",\"confirm_signed_payload_dry_run\":false,\"confirm_order_post_request_dry_run\":false,\"confirm_real_order_submission\":true,\"human_approval_event_id\":\"$APPROVAL_ID\",\"human_approval_token\":\"\",\"human_approval_note\":\"safety check only\",\"operator\":\"runbook\"}"
```

The first call records a `clob_order_human_approval` event with a deterministic subject hash, a 15-minute expiry, and `approved_for_facade=true`. The second call validates that event against the same intent and evaluates the future real-order submission shape without sending anything. It checks the post-request preview state plus explicit submission confirmation, journaled human approval validity, a fresh `clob_collateral_readiness` checkpoint, fail-closed kill-switch availability/state, projected per-order notional, projected total exposure, daily-loss limit, `POLYTRADER_ENABLE_REAL_ORDER_SUBMISSION`, and the current paper-mode startup gate. It must return `submission_facade_only=true`, `human_approval_event_valid=true`, `fresh_collateral_readiness_valid=false` until both collateral and allowance are positive in a recent journaled snapshot, `kill_switch_and_risk_limits_available=true`, `submit_decision='rejected_fail_closed'`, `reconciliation_status='reconciled_no_send'`, `reconciliation_journaled=true`, `request_sent=false`, `post_order_called=false`, `post_orders_called=false`, `real_orders_enabled=false`, and `ready_for_real_orders=false`. Results are journaled as `clob_order_submit_facade`, then a linked `clob_order_submit_reconciliation` event is written so Hermes can reflect on both the blocked gate state and the no-exchange-order reconciliation.

To inspect recent submit reconciliation events:

```bash
curl 'http://localhost:8080/clob/order-intent/submit-reconciliations?limit=10'
# or behind the public subpath:
curl 'http://localhost:8080/polytrader/clob/order-intent/submit-reconciliations?limit=10'
```

Each event must remain paper-only and include `request_sent=false`, `post_order_called=false`, `post_orders_called=false`, and an expected exchange state of `no_order_created`.

The deploy verifier intentionally uses a real journaled approval event but keeps the post-request preview confirmation disabled. That should still return blockers such as `post_request_preview_built`, `kill_switch_open`, `explicit_real_trading_config_unlock`, and `paper_mode_still_active`, while also returning passing risk checks such as `projected_order_notional_within_limit`, `projected_total_exposure_within_limit`, and `daily_loss_within_limit`. This proves the facade fails closed during routine deploy checks even after human approval workflow and risk-limit validation succeed.

To inspect recent journaled dry-runs:

```bash
curl 'http://localhost:8080/clob/order-intent/dry-runs?limit=10'
# or behind the public subpath:
curl 'http://localhost:8080/polytrader/clob/order-intent/dry-runs?limit=10'
```

`limit` is clamped to `1..=50`.

To inspect one dry-run and all of its paper-only reviews:

```bash
curl 'http://localhost:8080/clob/order-intent/dry-runs/<journal_event_id>'
# or behind the public subpath:
curl 'http://localhost:8080/polytrader/clob/order-intent/dry-runs/<journal_event_id>'
```

This returns the dry-run event, a compact summary, blocker list, latest review, and up to 50 matching review events. The summary includes `approval_blocked` and `recommended_review_decision`; current guidance is intentionally conservative and recommends `would_reject` whenever any blocker is present.

To record a paper-only operator review of a dry-run event:

```bash
curl -sS -X POST http://localhost:8080/clob/order-intent/dry-runs/<journal_event_id>/review \
  -H 'Content-Type: application/json' \
  -d '{"decision":"would_reject","note":"blocked by current preflight","operator":"dashboard"}'
```

Allowed decisions are `would_approve`, `would_reject`, and `needs_rework`. Reviews are written as separate append-only `journal.events` rows with `event_type='clob_order_intent_review'`. If the decision differs from `recommended_review_decision`, a non-empty `note` is required and the journaled review records `matches_guidance=false`. They do not approve, sign, submit, persist, cancel, or place a real order.

To inspect recent review decisions directly:

```bash
curl 'http://localhost:8080/clob/order-intent/reviews?limit=10'
# or behind the public subpath:
curl 'http://localhost:8080/polytrader/clob/order-intent/reviews?limit=10'
```

The response includes `decision_counts` for the returned window and the raw review audit events. `limit` is clamped to `1..=50`.

To inspect review coverage over recent dry-runs:

```bash
curl 'http://localhost:8080/clob/order-intent/review-summary?limit=50'
# or behind the public subpath:
curl 'http://localhost:8080/polytrader/clob/order-intent/review-summary?limit=50'
```

The summary reports reviewed/unreviewed counts, review coverage percentage, decision counts, conservative guidance counts, latest-review alignment counts, latest-review latency, and top blockers across the returned dry-run window.

To inspect one compact review health rollup for recent dry-runs:

```bash
curl 'http://localhost:8080/clob/order-intent/review-health?limit=50'
# or behind the public subpath:
curl 'http://localhost:8080/polytrader/clob/order-intent/review-health?limit=50'
```

The health rollup is derived from the same recent dry-run window as the summary endpoint. It reports a `status` of `empty`, `ok`, or `needs_attention`, plus machine-readable reasons such as `unreviewed_dry_runs`, `guidance_exceptions`, and `slow_latest_review_latency`. Slow latest-review latency currently means the max latest-review age in the returned window is at least 12 hours. The `reason_details` object repeats the key counts and thresholds behind those reasons.

The response also includes `recommended_actions`. Current action IDs are:
- `review_unreviewed_dry_runs`: inspect `/clob/order-intent/review-queue?limit=10`; carries `unreviewed_count` and `review_stale_after_seconds`.
- `inspect_guidance_exceptions`: inspect `/clob/order-intent/review-guidance-exceptions?limit=10`; carries `guidance_exception_count`.
- `inspect_review_latency`: inspect `/clob/order-intent/review-summary?limit=50`; carries `max_latency_seconds` and `slow_latency_after_seconds`.
- `no_recent_dry_runs` or `none`: informational actions when the window is empty or healthy.

To inspect reviewed dry-runs whose latest paper review differs from conservative guidance:

```bash
curl 'http://localhost:8080/clob/order-intent/review-guidance-exceptions?limit=10'
# or behind the public subpath:
curl 'http://localhost:8080/polytrader/clob/order-intent/review-guidance-exceptions?limit=10'
```

This returns only reviewed dry-runs where the latest `clob_order_intent_review` decision does not match `recommended_review_decision`.

To inspect every historical paper review that explicitly overrode conservative guidance:

```bash
curl 'http://localhost:8080/clob/order-intent/review-guidance-overrides?limit=10'
# or behind the public subpath:
curl 'http://localhost:8080/polytrader/clob/order-intent/review-guidance-overrides?limit=10'
```

This returns review events journaled with `matches_guidance=false`, including the operator note. Unlike guidance exceptions, this remains visible even if a later review changes the latest-review state.

To inspect review backlog freshness:

```bash
curl 'http://localhost:8080/clob/order-intent/review-backlog'
# or behind the public subpath:
curl 'http://localhost:8080/polytrader/clob/order-intent/review-backlog'
```

The backlog response reports the current unreviewed count, oldest/newest unreviewed dry-run timestamps, oldest unreviewed age in seconds, and a `status` of `empty`, `fresh`, or `stale`. `stale` currently means the oldest unreviewed dry-run is at least 24 hours old.

To inspect the focused queue of unreviewed dry-runs:

```bash
curl 'http://localhost:8080/clob/order-intent/review-queue?limit=10'
# or behind the public subpath:
curl 'http://localhost:8080/polytrader/clob/order-intent/review-queue?limit=10'
```

The queue returns the oldest dry-run events that do not yet have a matching `clob_order_intent_review` event. Each item includes `dry_run_age_seconds`, `review_stale_after_seconds`, `review_is_stale`, `review_priority`, and `next_review_action` so operators can triage stale or blocker-heavy dry-runs before broader live-send work. Current priorities are `stale_unreviewed`, `blocked_unreviewed`, and `standard_unreviewed`. It is read-only; reviewing an item still writes only a separate paper-only journal event.

The dashboard also includes a compact `Recent CLOB Dry-Runs` card that reads this endpoint, displays summary fields, and shows the latest paper review decision when present.

The dashboard also includes a compact `Recent CLOB Reviews` card that reads the review endpoint and displays recent paper-only review decisions.

The dashboard includes a `CLOB Review Summary` card that reads the review-summary endpoint and displays review coverage, conservative guidance counts, latest-review alignment, latest-review latency, and top blockers.

The dashboard includes a `CLOB Review Health` card that reads the review-health endpoint and displays the compact status, attention reasons, recommended actions, unreviewed count, guidance exception count, and max latest-review age. Recommended actions render as buttons that refresh and scroll to the relevant read-only panel: review queue, guidance exceptions, or review summary.

After deploying dashboard changes, run:

```bash
make k8s-verify
```

This invokes `deploy/verify`, which checks the live pod, L2 startup, in-cluster health, read-only authenticated CLOB status/account/preflight/diagnostics/operator-status endpoints, the paper-only dry-run audit endpoints, the same diagnostics/audit routes through the `/polytrader` subpath, public ngrok SSO boundary, dashboard SSR markers, the CLOB readiness, diagnostics, operator status, account, preflight, dry-run list, reviews, review summary, review backlog, guidance, review queue, detail, and dry-run form panels, review-health recommended actions, and the rendered dashboard JavaScript syntax with `node --check` when Node is available. The verifier exits non-zero if the app pod is not Ready, has restarted, in-cluster `/health` fails, in-cluster `/l2/status` does not report `connected=true`, in-cluster or subpath `/clob/status` does not report a successful read-only paper check, in-cluster or subpath `/clob/account` does not report a successful read-only account snapshot, in-cluster or subpath `/clob/preflight` does not report a successful paper-only diagnostic with `ready_for_real_orders=false`, in-cluster or subpath `/clob/diagnostics` does not report status/account/preflight sections with `ready_for_real_orders=false`, in-cluster or subpath `/clob/operator-status` does not report `paper_only=true`, `real_orders_enabled=false`, `ready_for_real_orders=false`, `operator_status`, and recommended next actions, any read-only dry-run audit route fails to report `paper_only=true` and `real_orders_enabled=false`, required dashboard CLOB readiness/diagnostics/operator/account/preflight/dry-run-audit/review-health markers are missing, or the public ngrok URL does not return HTTP `200` or `302`.

The dashboard includes a `CLOB Readiness` card backed by `/clob/status`. It displays the same read-only safety flags used by deploy verification: L2 connection state, read-only live-check state, `paper_only`, `real_orders_enabled`, and open-order count.

The dashboard includes a `CLOB Diagnostics` card backed by `/clob/diagnostics`. It displays the aggregate read-only status/account/preflight summary, including open-order count, allowance counts, readiness, and blocker names.

The dashboard includes a `CLOB Operator Status` card backed by `/clob/operator-status`. It displays the read-only operator rollup: CLOB blocker state, review-health state, dry-run counts, unreviewed count, guidance exceptions, final-review audit status/count, final-review coverage-gap probe count, action summary, freshness, and recommended next actions. The rollup keeps the primary operator action first and can also carry secondary review-health actions such as review queue, guidance-exception, review-latency inspection, final-review decision audit inspection, Hermes safety-loop inspection, and broad final-review coverage-gap inspection through `/clob/final-review-decisions?limit=50&gaps_only=true` when Hermes reports incomplete 24h boundary coverage. The `action_summary` object reports total, attention, info, actionable, and primary action fields so operators can assess urgency without parsing the full action list. The `freshness` object reports `generated_at` plus `stale_after_seconds=60` so operators can distinguish fresh status from a stale browser view. The recommended action buttons only refresh and scroll to existing read-only dashboard panels.

The dashboard includes a `CLOB Order Placement Readiness` card backed by `/clob/order-placement-readiness`. It displays the read-only "how far from placing orders" gap report: readiness, current stage, completed/required gates, blocker count, blockers, and the next safe engineering step. It remains display-only and does not enable real trading.

The dashboard includes a `CLOB Account` card backed by `/clob/account`. It displays the authenticated read-only account snapshot summary: open-order count, collateral balance visibility, allowance entry counts, `paper_only`, and `real_orders_enabled=false`.

The dashboard includes a `CLOB Preflight` card backed by `/clob/preflight`. It displays diagnostic future-order blockers such as missing human approval gates, disabled real-order routes, collateral/allowance state, and `ready_for_real_orders=false`. It is display-only and does not enable real trading.

The dashboard includes a `CLOB Guidance Exceptions` card that reads the guidance-exceptions endpoint and displays reviewed dry-runs whose latest paper review differs from conservative guidance.

The dashboard includes a `CLOB Guidance Overrides` card that reads the guidance-overrides endpoint and displays historical paper reviews that overrode conservative guidance.

The dashboard includes a `CLOB Review Backlog` card that reads the review-backlog endpoint and displays unreviewed count plus oldest unreviewed age.

The dashboard includes a `CLOB Review Queue` card that reads the review-queue endpoint and offers the same paper-only review actions for the oldest unreviewed dry-runs.

The dashboard includes a `CLOB Dry-Run Detail` card that reads one dry-run by event id and shows its full audit context plus matching paper-only reviews.

Dry-run summaries in the dashboard include conservative review guidance. A dry-run with any blocker is marked `approval_blocked=true` and recommends `would_reject`; this is advisory paper-review metadata only.

The dashboard includes a `CLOB Dry-Run Intent` form that posts to the dry-run endpoint, shows the rejected/journaled summary, and refreshes the recent dry-run list. It is still dry-run-only.

Set `POLYTRADER_ENABLE_REAL_CLOB_READS=0` to disable even this authenticated read path while keeping L2 derivation available.

## Security notes

- The Secret lives only in the cluster (etcd). On docker-desktop this is local.
- For real clusters use SealedSecrets, External Secrets Operator, or your cloud secret manager.
- The key gives the ability to derive real L2 trading credentials. Treat it with the same care as a wallet seed.
- Never put the real value in this runbook or any committed file.

## Troubleshooting

- Pod still says "No POLYMARKET_PRIVATE_KEY found":
  - Check `kubectl get secret polytrader-l2-auth -n polytrader -o yaml`
  - Make sure the Deployment was restarted after the secret was created.
  - Look at pod logs for the exact "L2" lines on startup.

- Button says "Failed":
  - Confirm the running image was built with `--features native-l2`.
  - Check pod logs for the native SDK error; malformed keys or upstream auth failures are surfaced there without logging the raw key.
  - Confirm `POLYMARKET_PRIVATE_KEY_FILE` points to the mounted secret path and the file is non-empty.

## Related

- `Makefile` targets: `k8s-apply`, `k8s-set-l2-key`
- `src/server.rs` – L2 auth handlers + auto-derive
- `src/ui/app.rs` – L2 chip and button
- `wiki/log.md` – chronological record of these changes
