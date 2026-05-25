# Decision: No Official Simulation — Implement Custom High-Fidelity Paper Trader

**Date**: 2026-05-25  
**Status**: Accepted  
**Deciders**: Initial bootstrap (agent-assisted research + human direction)

## Context

Polymarket operates exclusively on Polygon mainnet with real USDC collateral. There is no public testnet, sandbox, demo account, or official paper trading API.

Community members have built third-party paper trading simulators (notably Python-based ones that replay live orderbooks locally).

The project requires safe 24/7 autonomous "play" from day one, before any real capital is at risk.

## Options Considered

1. **Use only real money from the start** (small test wallet) — Rejected. Violates safety mandate in MISSION and AGENTS.md.
2. **Adopt an existing open-source paper trader** (e.g. agent-next/polymarket-paper-trader) as core engine — Rejected for Phase 1. Good for inspiration and techniques, but we need:
   - Deep integration with our Postgres journal + Hermes reflection loops.
   - Rust/Dioxus native (or easily callable).
   - Full control over fidelity model, risk limits, and accounting.
   - Ability to evolve it in lockstep with our strategy and data model.
3. **Build custom paper trading engine** using only public (unauth) Gamma + CLOB read endpoints + local matching simulation. — **Selected**.

## Decision

Implement a first-class `PaperTradingEngine` (trait + realistic impl) inside the Rust codebase.

- Fed exclusively by live public data feeds (Gamma markets/prices + CLOB orderbook/trades/ticker WS or polling).
- Local order matching with configurable:
  - Taker fee schedule (initially conservative estimate, later measured).
  - Slippage / market impact model (orderbook-depth aware).
  - Latency and partial fill simulation.
- Produces the same journal artifacts (orders, fills, positions, portfolio snapshots) as the future real adapter.
- Same high-level API surface as the real SDK-backed adapter → strategy/decision code is mode-agnostic.
- Explicit "paper only" compile-time and runtime gates; real adapter is behind multiple kill switches.

## Rationale

- Highest fidelity possible without real money (live books = real liquidity conditions).
- Enables rapid iteration and Hermes-driven learning from day one.
- Positions us well for "shadow trading" (parallel real + paper) later as a validation step.
- Avoids dependency on unmaintained or loosely-coupled third-party simulators.
- Aligns with "primarily Rust" goal.

## Consequences & Follow-ups

- **Must** implement realistic simulation before any strategy sophistication (Phase 1 blocker).
- Measurement task: run paper volume and empirically determine effective fees + typical impact on Polymarket books.
- Future: consider contributing fidelity improvements back to community paper trading efforts.
- Risk: Simulation can still be optimistic (e.g. underestimating adverse selection or queue priority). Hermes must watch for "paper edge that disappears in real".
- Update [sources/polymarket-api.md](../sources/polymarket-api.md) and [concepts/polymarket-trading.md](../concepts/polymarket-trading.md) with simulation details as built.

## Review Trigger

- After first 1000 simulated fills or first week of continuous paper trading, whichever sooner: review realism vs. any external benchmarks or small real test trades (if/when approved).
