# Decision: Extend Hermes with Fusion + Closed-Loop Signal Attribution Learning

**Date**: 2026-05-25  
**Status**: Accepted (wiki-first)  
**Deciders**: Grok per plan

## Context
Hermes (src/bin/hermes.rs) has richer Phase 1/2 reflection (P&L Decimal deltas, LLM, gated wiki proposals) on aggregate. Plan requires 3.3 enhancements for per-signal attribution from the transferred fusion (to close the loop: "which processors win?").

## Options Considered
1. Keep aggregate-only reflections.
2. Manual post-hoc analysis.
3. Extend existing loop with journal-based signal attribution, metrics per processor, gated proposals for weights/experiments (ties directly to fusion decision + BTC bot learning_engine + Poly-Trader profits patterns).

## Decision
Adopt 3: Extend Hermes as the consumer/learner for the fusion system (Phase 3.3).

## Rationale
- Realizes AGENTS "self-improving system" + Hermes as "research director".
- Journal already designed for this (immutable record of decisions + outcomes).
- Direct port of closed-loop from BTC bot feedback/ + Poly-Trader profits + openclaw predictor eval.
- Gated autonomous proposals (existing mechanism) keep safety.
- Enables measurable improvements (e.g. "fusion + learning improved paper Sharpe").

## Tradeoffs
- Pros: True learning, wiki evolution, experiment promotion.
- Cons: More DB queries in reflection (mitigated: existing sqlx patterns).
- Risks: Noisy attribution (mitigated: conservative Decimal math + paper tests; log warnings not silent fallbacks).

## Implementation Notes
- Build on Phase 2 gated path + richer P&L.
- New section in hermes-self-improvement.md (added).
- Use journal for signals; no new tables initially.
- See strategies/multi-signal-fusion.md + hermes-self-improvement.md new section.

## Links
- concepts/hermes-self-improvement.md (new fusion section)
- strategies/multi-signal-fusion.md
- wiki/log.md (transfer entry)
- decision-adopt-multi-signal-fusion-from-btc-bot.md
- Source credits: BTC bot feedback/learning_engine.py + monitoring/, Poly-Trader polymarket_profits.py, openclaw predictor/llmScorer eval patterns, agents executor.

*Wiki-first.*