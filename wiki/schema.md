# Database Schema

**Engine**: PostgreSQL via CloudNativePG (2 replicas: primary + standby)  
**Access**: sqlx (Rust, compile-time checked queries)  
**Migration tool**: TBD (sqlx migrate, refinery, or custom — decide in Phase 0)  
**Conventions**: snake_case, timestamps with time zone (timestamptz), use rust_decimal for all monetary/price/quantity columns (never double precision for finance).

## High-Level Domains

1. **market_data** — Raw and normalized snapshots from Gamma + CLOB. Append-only where possible for auditability.
2. **paper_trading** — All virtual positions, orders, fills, portfolio snapshots. The "safe playground".
3. **real_trading** — (Future, strictly gated) Mirror of paper but for live wallet. Separate schema or heavy RLS.
4. **journal** — Immutable(ish) record of decisions, reflections, experiments, anomalies. Primary input for Hermes.
5. **config** — Runtime tunable parameters, strategy weights, risk limits (versioned).

## Core Tables (Initial Draft — Evolving)

### market_data.markets
- id (uuid or polymarket market id as text)
- slug, question, category, outcomes (jsonb)
- active, resolved, resolution_outcome (nullable)
- created_at, end_date, resolved_at
- current_prob_yes, current_liquidity, volume_24h, etc. (from latest snapshot)
- raw_gamma_json (jsonb, for future fields)

### market_data.orderbook_snapshots (or time-series / hypertable if using timescaledb later)
- market_id
- timestamp
- bids (jsonb array of {price, size})
- asks (jsonb)
- mid, spread, depth_1%, etc. (precomputed)

### paper_trading.virtual_portfolio
- id
- as_of (timestamptz)
- virtual_usdc_balance
- total_collateral_locked
- unrealized_pnl, realized_pnl
- snapshot_reason (periodic, post_fill, post_resolution, manual)

### paper_trading.positions
- market_id, outcome ("Yes" | "No")
- shares (decimal)
- avg_entry_price (decimal)
- collateral_locked (decimal)
- last_updated

### paper_trading.orders (intents + status)
- id (ulid or uuid)
- market_id, side, order_type (market|limit), tif
- limit_price (nullable), size_requested
- status (open, filled, cancelled, rejected)
- created_at, filled_at, etc.
- decision_context_id (fk to journal.decisions or similar)

### paper_trading.fills
- id
- order_id
- fill_price, fill_size, fee_paid, slippage_realized
- against_book_state (jsonb snapshot at match time)
- created_at

### journal.reflections
- id
- period_start, period_end
- summary (text), metrics (jsonb: sharpe, winrate, drawdown, etc.)
- recommendations (jsonb array)
- hermes_version, llm_model, prompt_hash
- created_at

### journal.events
- id
- event_type, source, severity
- payload (jsonb)
- created_at

Append-only audit events for diagnostics and safety gates that are not paper orders/fills or Hermes reflections. Current CLOB uses:
- `clob_order_intent_dry_run`: hypothetical order intent validation report.
- `clob_order_intent_review`: paper-only operator review of a dry-run event (`would_approve`, `would_reject`, or `needs_rework`).
- `clob_collateral_readiness`: read-only collateral/allowance readiness snapshot for the active L2 wallet. It records balance/allowance blocker state, operator actions, `request_sent=false`, and no order POST calls so Hermes can track whether the external wallet blockers have changed.
- `clob_market_metadata_validation`: read-only CLOB market metadata validation for a token. It records tick-size and negative-risk lookup results, price tick/range validation, `request_sent=false`, and no order POST calls.
- `clob_order_post_request_dry_run`: redacted, non-submitting preview of the would-be CLOB `POST /order` request. It records no full signatures, no L2 HMACs, no private keys, and no API secrets. Hermes consumes these events for safety-loop reflections.
- `clob_order_human_approval`: short-lived, journaled human approval workflow event keyed to a deterministic order-intent subject hash. It can validate the submit facade but is not a live-trading approval.
- `clob_order_submit_facade`: fail-closed real-order submission facade evaluation. It records gate status for human approval, fresh collateral-readiness checkpoint, kill switch, per-order exposure, total exposure, daily loss, paper mode, and explicit config unlocks, but must always record `request_sent=false` while real trading is disabled.
- `clob_order_submit_reconciliation`: submit-facade reconciliation audit event linked to the facade event. It records the submit/reject decision, `reconciliation_status='reconciled_no_send'`, `request_sent=false`, no exchange order id, and expected exchange state `no_order_created`.
- `clob_real_trading_unlock_status`: read-only report for explicit real-trading unlock state. It records env/config gate status, paper-mode state, kill-switch state, live-sender absence, `request_sent=false`, and no order POST calls.
- `clob_live_sender_design_readiness`: read-only design-readiness package for the deliberately absent live sender. It records live-sender implementation blockers, final-review audit evidence, explicit unlock state, kill-switch state, paper-mode state, `request_sent=false`, and no order POST calls.
- `clob_live_sender_design_review`: read-only ADR-style design contract for a future live sender. It records required sender boundaries, pre-submit guards, prohibited shortcuts, design-review evidence, `implementation_permitted=false`, `request_sent=false`, and no order POST calls.
- `clob_live_sender_boundary_status`: read-only status for the code-level live-sender trait boundary. It records that `LiveOrderSender` exists, the only implementation is `FailClosedLiveOrderSender`, `network_sender_present=false`, `accepted_for_network_dispatch=false`, `request_sent=false`, and no order POST calls.
- `clob_final_review_readiness`: read-only aggregate package over latest journaled CLOB gate evidence. It records whether final human review is still blocked by collateral, allowance, unlock, kill-switch, paper mode, live-sender absence, or missing no-send reconciliation evidence.
- `clob_final_review_decision`: audit-only operator decision linked to a `clob_final_review_readiness` event. It records `acknowledge_blocked`, `reject_live_trading`, or `needs_rework` decisions with `approved_for_real_orders=false`, `request_sent=false`, and no order POST calls.

Payloads must not contain secrets, raw private keys, or L2 API secrets. Review events are not approvals for real trading; they are analysis/audit records only.

### journal.experiments
- id
- hypothesis (text)
- method, parameters (jsonb)
- results (jsonb)
- conclusion, promoted_to_prod (bool)
- created_by (hermes|human)
- created_at

(Additional tables for real_trading mirror, wiki_edits audit, strategy_params, etc. will be added.)

## Invariants & Rules (Enforced in App + Checked by Hermes)

- Paper balance never goes negative (except transient during settlement).
- No real_trading rows exist while real mode is disabled at compile/runtime flag.
- Every fill has a corresponding order and market snapshot context.
- Resolutions from Gamma are the source of truth for settlement; manual overrides audited.

## Evolution

Schema changes require:
1. Update this file (wiki/schema.md) with rationale.
2. New migration file.
3. Hermes reflection on impact to existing journal data / backfills.
4. UI + ingester + trading engine updates in same change set where possible.

See runbooks for migration execution on k8s (with standby promotion safety).

**This document is the contract.** Code queries and Hermes analysis must stay in sync with it.

## 2026-05-25 Phase 0 Implementation Notes (actual vs draft)
- Added `paper_trading.paper_positions` (current state, upserted on fills) + `virtual_portfolio_snapshots.positions` JSONB denorm for snapshots.
- `paper_orders` gained `outcome` (Yes/No) + `decision_context` JSONB (required for binary + Hermes).
- `orderbook_snapshots` keyed primarily by `token_id` (CLOB reality for Yes/No shares) with optional market_id/outcome.
- `markets` uses TEXT gamma_id (not uuid), stores last_mid_yes/no + raw.
- All implemented via one migration + sqlx; no real_trading tables (enforced).
- Outcome strings are normalized to title-case "Yes"/"No" in ingestion + engine submit before any DB write (to satisfy the case-sensitive CHECK in migration); lookups tolerate case.
- See wiki/log.md 2026-05-25 Phase 0 Core entry for rationale + follow-ups. Next schema change requires wiki update + migration PR.
