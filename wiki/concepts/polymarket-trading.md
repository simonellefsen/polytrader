# Polymarket Trading Concepts

(Stub — expand heavily during Phase 0/1)

## Core Objects

- **Market**: A binary (or multi-outcome in future) prediction contract. Has slug, question, outcomes (Yes/No), current prices, liquidity, volume, end date, resolution status.
- **Order**: Limit or market, buy/sell, size (in shares or collateral), price (implied prob).
- **Position**: Net shares held in a specific outcome for a given market + virtual collateral locked.
- **Fill**: Execution of (part of) an order at a certain price/time. Includes simulated fees and slippage in paper mode.
- **Resolution**: Final outcome (0 or 1 for Yes/No). Triggers automatic payout in simulator.

## Paper Trading Fidelity Goals

- Match against **live** orderbook depth when deciding fill price/quantity.
- Model taker fee accurately (start with conservative 1%, refine with measurement).
- Account for price impact of our own order size (simple square-root or linear impact for starters).
- Simulate realistic latency (network + matching delay) so strategies don't assume instant fills.
- Handle edge cases: market halts, low liquidity (wide spreads), sudden news moves.

## Strategy Building Blocks (Early Ideas)

- Probability edge vs. "fair" value (from model, crowd, or external signals).
- Liquidity filters (avoid thin books).
- Time decay / event proximity filters.
- Category-specific models (elections behave differently from crypto or sports).
- Portfolio construction: diversification across uncorrelated markets, position sizing via Kelly or fractional.

## Risk Framework (Mandatory)

- Per-market max exposure (% of virtual bankroll).
- Category/sector concentration limits.
- Max daily / weekly paper loss stop.
- "Circuit breaker" on unusual volatility or API degradation.
- Kill switch for entire paper engine (UI + config + signal file).

All of the above must be configurable, logged on every decision, and reviewable by Hermes.

## Journaling Requirements

Every paper order intent, fill, cancellation, and resolution settlement must produce immutable journal entries with full context (market state snapshot, decision rationale or LLM prompt+response, risk params at time, etc.).

This is the raw material for Hermes reflection.

## Real Trading (Future, Gated)

When enabled:
- Same high-level interfaces as paper (different impl).
- Additional pre-flight checks (wallet balance, gas, open exposure, human approval token or timeout).
- Separate schema or strict partitioning so paper and real never mix.
- Full audit trail for regulatory/compliance if ever needed.

## Common Pitfalls Observed in Community

- Over-trading low-liquidity markets (death by a thousand slippage cuts).
- Ignoring resolution risk / dispute windows.
- Assuming historical edge persists after fees + impact.
- No kill switch when strategy starts losing (paper or real).

Document observed gotchas here as we discover them.
