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
