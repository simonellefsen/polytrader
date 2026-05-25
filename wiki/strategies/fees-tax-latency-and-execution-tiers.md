# Fees, Tax, Latency & Execution Tiers

**Date**: 2026-05-25  
**Related**: `goals-and-operational-cadence.md`, `multi-signal-fusion.md`, project-plan Phase 3.1/3.2/3.4, `wiki/integrations/polymarket-apis-and-data-sources.md`

## The Hard Reality for Small Capital (~$150)

With a starting paper bankroll of approximately $150, **fees and latency are not minor details** — they are often the difference between positive and negative expectancy.

Polymarket (on Polygon) has two main cost layers that must be modeled from day one:

### 1. Trading Fees (Polymarket)
- **Taker fees**: Volume-based. At low volumes they are relatively high (historically in the 1–2% range for small traders, though the rewards program and volume tiers can bring effective rates down significantly).
- **Maker rebates**: Sometimes available, but hard to capture consistently at small size.
- **Rewards program**: Volume-based rewards can offset fees, but again, difficult to rely on with tiny notional.

**Critical modeling requirement**: Every Decision Report and every simulated fill in the paper engine **must** calculate **net edge after fees**.

### 2. Gas Fees (Polygon)
Even "free" reads have costs in practice, and every write (if we ever move to real trading) costs gas. For realistic paper trading we should model estimated gas costs on actions that would require on-chain settlement.

### 3. Tax (Jurisdiction-Dependent)
Even while paper trading, we should maintain **audit-grade records** because:
- Good habits transfer when real capital is unlocked.
- Some jurisdictions treat consistent trading activity as business income.
- Cost basis tracking, fee deductibility, and realized vs unrealized gains all matter later.

**Recommendation**: From the very beginning, the journal should capture enough data to reconstruct a full tax position if needed (trade time, instrument, direction, size, entry price, exit price, fees paid, gas if modeled).

## Recommended Architecture: Tiered Execution

We should **not** try to be everything at once with $150. A pragmatic tiered model is strongly preferred:

### Tier 1: Deliberate / Thoughtful Layer (Primary Mode)
- **Cadence**: 5-minute Decision Reports (as defined in `goals-and-operational-cadence.md`).
- **Decision process**: Full FusionEngine + risk/goal filters + **net-of-fees** calculation.
- **Characteristics**:
  - Uses a mix of polling + light streaming for data.
  - Human or Hermes oversight is feasible.
  - Focus on edge quality rather than speed.
- **When to use**: The large majority of activity, especially while capital is small.

### Tier 2: Reactive / Streaming Layer (Selective, Future)
- **Trigger**: Real-time CLOB WebSocket events (orderbook deltas, trades, price updates).
- **Use cases** (only when they clearly have edge after fees):
  - Market making / liquidity provision in deep, high-volume markets.
  - Sniping specific high-conviction events where being first matters (and where the edge is large enough to overcome taker fees + gas).
  - Latency-sensitive arbitrage between related markets.
- **Characteristics**:
  - Requires robust WebSocket management (reconnection, ordering, gap detection).
  - Much stricter fee modeling (you are usually a taker in reactive scenarios).
  - Higher operational complexity and risk of over-trading.
- **Gate**: Should only be enabled for specific strategies after they have proven positive expectancy in Tier 1 paper trading.

### Tier 3: Hybrid (Recommended Long-Term Pattern)
- Use streaming primarily for **data** (fast, accurate orderbook and trade flow).
- Use the deliberate FusionEngine + goal framework for **actual trading decisions**.
- This gives you the best of both worlds: low-latency information without forcing every decision to be made at HFT speed.

## Fee Modeling Requirements (Must Be First-Class)

The paper engine and strategy layer must treat fees as a core input, not an afterthought:

1. **Configurable fee model**
   - Taker fee % (volume tier aware)
   - Maker rebate %
   - Estimated gas cost per action type
   - Rewards offset (if modeled)

2. **Net Edge Calculation**
   Every opportunity evaluated by the FusionEngine should produce:
   - Gross expected edge
   - Estimated fees + gas
   - **Net expected edge** (this is what gets compared against the 4–6% minimum threshold in the goals)

3. **Journaling**
   - Every virtual fill must record estimated and (later) actual fees paid.
   - This enables accurate Hermes attribution ("this signal looked good gross but was destroyed by fees").

## Tax & Record-Keeping Strategy

Even in pure paper mode:

- Treat every paper trade as if it will one day be real for record-keeping purposes.
- The journal should be capable of producing:
  - Per-trade cost basis
  - Fees paid (deductible in many jurisdictions)
  - Realized P&L
  - Unrealized positions

Later (Phase 3+), we can add:
- Virtual tax reserve (automatically set aside X% of paper profits)
- Reporting exports

## Impact on Current Planning (2026-05-25 Transfer)

This requirement strengthens several parts of the existing plan:

- **3.1 Ingester Enhancements**: WebSocket support (already planned, inspired by BTC bot and poly-maker) becomes even more important. We need resilient, ordered, gap-aware streams for the reactive tier.
- **3.2 FusionEngine**: Must be extended to accept fee models as input and output net edge. Different processors may have different fee sensitivity (e.g. market making is maker-fee sensitive).
- **3.4 Risk & Position Layer**: Fee-aware position sizing is mandatory. The risk engine should be able to reject trades whose net edge after fees is too low.
- **3.3 Hermes**: Should explicitly track "fee drag" and "fee-adjusted signal performance" in reflections. This is high-value learning.
- **Goals & Cadence** (the page you just approved): All targets and minimum edge requirements should be understood as **net of fees**.

## Practical Recommendation for Current Capital

With ~$150:

- **Default to Tier 1 (Deliberate 5-min layer)** for the foreseeable future.
- Only consider enabling any streaming-reactive logic for very specific, well-backtested cases where the edge is large and the notional makes gas/fees negligible in relative terms.
- Aggressively model fees in the paper engine from the first day the FusionEngine is wired up. This will naturally steer the system toward higher-quality, lower-turnover opportunities.

This is not a limitation — it is disciplined capital allocation.

---

**Next Steps (within the approved plan)**

1. Extend the PaperTradingEngine to have a first-class, configurable fee + gas model (high priority for 3.4).
2. Make "net edge after fees" a first-class output of the FusionEngine decision reports.
3. Update Hermes reflection to break out fee impact in attribution.
4. (Later) Add optional streaming-reactive execution paths behind strong gates, once Tier 1 has proven itself.

All of this remains fully compatible with the conservative goals and cadences defined in the sister document.

---

## Implementation Status (2026-05-25)

The four concrete next practical steps have been **implemented** as the smallest viable working pieces within the existing architecture (paper-only, ~$150 context, rust_decimal everywhere, no new migrations, jsonb for all new attribution/metrics, heavy risk comments, fmt/clippy clean).

See the top entry in `wiki/log.md` (the 2026-05-25 impl prepend) for full details:
- Exact files changed, design decisions + rationale + risks (e.g. pessimistic fee defaults, double-gating for WS, no silent calcs/fallbacks).
- Commands, verification outputs, AGENTS/wiki-first compliance matrix.
- Proactive handling of past anti-patterns (fidelity via prepend+re-reads, no overclaims on reactive WS ("skeleton; gated; future only; do not enable"), proper stream error handling, doc/impl match).
- Credits to this page + `goals-and-operational-cadence.md` + 5 transferred repos (via the 3.1 decision + integrations page: BTC-bot websocket_manager.py etc., poly-maker websocket_handlers.py, etc.).

**Summary of delivered (one per step, smallest)**:
1. `FeeModel` (struct + net calc methods) first-class in `src/paper/` (models + engine); seeded from existing paper_fee_bps legacy config (config/main untouched for smallest compat during transition; full configurable ctor + dedicated envs deferred to wiring inc). Integrated into fill paths + exposed for opportunities. (Replaces simplistic bps with tier-aware model foundation.)
2. `FusionEngine` extended + new `DecisionReport` (gross + **net edge after fees/gas/slip** + fee_breakdown in attribution json). Primary deliberate tier signal. One path updated in skeleton.
3. Hermes `do_reflection` now breaks out fee impact, fee-adjusted P&L, per-processor fee attribution, vs goals (daily/weekly net targets from cadence page) — all in existing `journal.reflections.metrics` jsonb + summaries.
4. Gated WS skeleton (`ClobWsClient` cfg(feature="clob-ws"), reconnect, basic market channel handling) in `src/ingester/clob_public.rs`; optional Cargo dep; runtime env gate; **zero activation**; poll path untouched; errors never silent. Follows exact transferred patterns + ingester style.

**Key constraints honored**:
- Wiki-first: log prepend + this status append *before* any src edit; re-reads before each search_replace.
- No over-engineering or new concepts beyond the approved tiers/goals pages.
- $150 conservative: fee over-estimation baked in; WS explicitly "skeleton for future high-conviction only after Tier 1 proof".
- Observability: everything via existing journal (fills fee, jsonb decision_context/metrics).
- No behavior change to verified paths (k8s, hermes cadence, endpoints, ingest polling, paper submit).

**Fidelity Reconciliation Note (Fix Round 1 - 2026-05-25)**:
Post-delivery review identified doc/impl drift in original claims (anti#1/3):
- Bullet 1 originally claimed "configurable via extended `Config`": corrected above to accurately reflect smallest viable (legacy paper_fee_bps path only; no Config/main changes beyond required mod; full extension deferred).
- Similar corrections + full 15-file touched list + pre-dirty context documented in paired amend to `wiki/log.md` top entry (Fidelity Reconciliation Note subsection with timeline, exact mismatches, AGENTS compliance, future gate).
- Wiki-first order strictly followed for this Fix Round amend (log + this fees Status edit *before* any code nits or review_file updates). Re-read + `git status --porcelain` verified post-edit (only wikis modified at that step). Restores fidelity for Hermes consumption. No scope or behavior change.

**Status**: Complete for this increment. Ready for follow-up wiring (5-min DecisionReport caller, risk gates using net edge, etc.). Update this section + log.md on next steps.

(No new decision file created; used edits to existing pages per "prefer edit" + smallest rule. If design issues had arisen, Status: wontfix with explanation would have been used.)