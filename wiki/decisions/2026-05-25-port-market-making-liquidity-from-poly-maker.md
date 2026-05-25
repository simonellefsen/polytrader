# Decision: Port Market-Making & Liquidity-Aware Patterns from poly-maker

**Date**: 2026-05-25  
**Status**: Accepted (wiki-first transfer kickoff)  
**Deciders**: Grok per approved plan

## Context
See main fusion decision and strategies/market-making-liquidity.md. polytrader paper engine currently lacks explicit liquidity modeling (depth, spreads, adverse selection). poly-maker provides practical pipelines + utils for this on Polymarket CLOB.

## Options Considered
1. Ignore liquidity (current simple snapshots suffice for directional only).
2. Build custom from scratch.
3. Port/adapt poly-maker patterns (client/WS, data_processing, trading_utils, stats) for liquidity signals/filters + MM simulation in paper.

## Decision
Adopt 3: Use poly-maker patterns for liquidity-aware extensions (Phase 3.4), as complement to fusion (one input processor or filter).

## Rationale
- Liquidity is key risk/edge factor on Polymarket (thin markets, resolution squeezes).
- poly-maker has battle-tested handling (WS + processing + stats) directly applicable.
- Enables realistic paper MM simulation (maker vs taker, inventory skew) + feeds risk limits.
- Explicit credits + complements multi-signal-fusion and ai-edge-kelly (liquidity regime for sizing).

## Tradeoffs
- Pros: Better slippage modeling, new alpha source, stats for Hermes.
- Cons: Adds complexity to execution sim (mitigated: paper-only, smallest via wiki first).
- Risks: Adverse selection (mitigated by paper + journaled analysis).

## Implementation Notes
- See strategies/market-making-liquidity.md for details.
- Integrate as processor or analyzer in FusionEngine (Decimal depth calcs).
- 3.4 dashboard + 3.5 obs.

## Links
- strategies/market-making-liquidity.md
- integrations/polymarket-apis-and-data-sources.md (WS/client credits)
- wiki/log.md top, hermes-self-improvement (new section)
- Source: poly-maker/poly_data/{polymarket_client.py,websocket_handlers.py,data_processing.py,trading_utils.py} + stats/ + main/update scripts

*Wiki-first decision record.*