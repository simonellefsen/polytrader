# Decision: Enhance Data Ingester for Phase 3.1 (WS, Validation, Unified, Rate Limiting)

**Date**: 2026-05-25  
**Status**: Accepted (wiki-first)  
**Deciders**: Grok per plan

## Context
Current ingester (src/ingester/mod.rs + clob_public.rs + gamma.rs) is solid poll-based (sqlx upserts to market_data, jsonb snapshots, polite sleeps, error warn, Gamma + CLOB public). Plan 3.1 calls for enhancements inspired by the 5 repos to support signal processors (richer events, WS for low latency, validation, resilience).

## Options Considered
1. Leave as-is (poll sufficient for bootstrap).
2. Full rewrite.
3. Incremental enhancements (WS manager + reconnection + rate limit from BTC bot, unified adapter pattern, data_validator from BTC, client/WS from poly-maker/openclaw, fetch patterns from Poly-Trader/agents) — smallest first, paper-only, exact Rust patterns.

## Decision
Adopt 3: Incremental 3.1 enhancements post-wiki, starting with skeleton hooks in smallest code (if any in this increment) or follow-up.

## Rationale
- Enables low-latency signals (momentum, orderbook) without breaking existing poll paths.
- Validation + resilience reduce bad data into fusion (anti-pattern avoidance).
- Direct credits to real implementations that work on the same APIs.
- Preserves all verified (k8s-apply, hermes, probes, JSON) — no Docker/Make/k8s changes.

## Tradeoffs
- Pros: Better data for 3.2+, observability.
- Cons: WS adds complexity (mitigated: optional, behind config, paper first).
- Risks: Rate limit evasion or disconnect storms (mitigated by bot-proven limiters + backoff).

## Implementation Notes
- See integrations/polymarket-apis-and-data-sources.md (detailed credits + current mapping).
- Follow src/ingester/mod.rs patterns exactly (sqlx, tracing, Result, Decimal for new mids/derived).
- No new migs; extend snapshots or use jsonb.
- 3.5 obs/validation tie-in (data_validator ports).

## Links
- integrations/polymarket-apis-and-data-sources.md
- strategies/multi-signal-fusion.md (downstream consumers)
- wiki/log.md top
- Source: Polymarket-BTC-15-Minute-Trading-Bot/core/ingestion/* (unified_adapter, websocket_manager, rate_limiter, data_validator, providers), poly-maker/poly_data/{polymarket_client.py,websocket_handlers.py}, openclaw/src/connectors/polymarket.ts + engine, Poly-Trader/fetch_*.py, agents/agents/polymarket/*.

*Wiki-first decision.*