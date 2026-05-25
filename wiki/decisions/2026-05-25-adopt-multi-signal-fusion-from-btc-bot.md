# Decision: Adopt Multi-Signal Fusion Architecture from Polymarket-BTC-15-Minute-Trading-Bot

**Date**: 2026-05-25  
**Status**: Accepted (wiki-first as part of transfer kickoff per AGENTS + user-approved plan)  
**Deciders**: Grok (implementer) + plan review (human approval of /implement)

## Context
polytrader currently has basic ingester (src/ingester/) producing snapshots and a paper engine, with Hermes reflection on aggregate P&L (src/bin/hermes.rs, journal). No structured signal processors or fusion. The approved detailed transfer plan (from deep dive of 5 repos in /Users/lindau/codex/polymarket-github/) calls for Phase 3.2 "signal processors + FusionEngine" to accelerate strategy brain, with explicit credits and port to Rust/Decimal/journal/Hermes (paper-only). The BTC bot has the most mature, extensible implementation (fusion_engine/signal_fusion.py + 7+ processors inheriting base_processor.py + learning feedback).

Related: wiki/log.md (top transfer entry), strategies/multi-signal-fusion.md (detailed port notes + diagram), integrations/polymarket-apis-and-data-sources.md, hermes-self-improvement.md (new section), project-plan.md (updated Phase 2/3).

## Options Considered
1. **Status quo / simple heuristics + direct LLM**: Keep current (ingester -> paper or LLM in Hermes). Fast, low risk.
2. **Single monolithic "edge model"**: One big module or LLM prompt for all signals. Simpler code but poor debuggability/attribution.
3. **Adopt & port BTC bot multi-signal fusion** (processors as traits, central FusionEngine with attribution, journaled for Hermes closed-loop): Matches plan exactly. Higher initial structure but excellent extensibility, observability, learning (P&L per signal), and direct credits/traceability.
4. **Hybrid or from other repos only** (e.g. openclaw predictor + Poly-Trader AI only): Good for short-horizon or AI but misses the general fusion + multi-processor framework + learning engine from BTC bot.

## Decision
**Adopt option 3**: Port the multi-signal fusion architecture (with explicit credits) as the foundation for Phase 3.2/3.3. Start with smallest skeleton (1-2 processors + basic FusionEngine) after all wiki, using exact polytrader patterns (Decimal, sqlx/journal, ingester integration, heavy risk comments, paper-only, no new migs initially).

## Rationale
- Proven in production-like bot for volatile short-horizon markets; directly transferable to Polymarket binaries.
- Enables closed-loop learning in Hermes (attribution of P&L deltas to specific signals/processors via journal) — core to self-improving system (AGENTS philosophy).
- Excellent observability and experimentability (aligns with journaled, wiki as single source, Hermes proposals).
- Explicit file-level credits satisfy task + AGENTS traceability for LLM/Hermes consumption.
- Smallest viable start + incremental processors avoids big-bang risk.
- Complements (does not replace) other patterns (MM from poly-maker, AI/Kelly from Poly-Trader, momentum from openclaw).

## Tradeoffs
- **Pros**: Modularity, debuggability ("why did we take this trade?"), measurable signal performance, future ML fusion or weight tuning via experiments, strong fit for Hermes 3.3 loop.
- **Cons**: More initial abstractions than status quo; requires careful Decimal math and journal schema discipline (mitigated by no new migs in first increment + wiki/schema process).
- **Risks & Mitigations**: Over-engineering early (mitigated by smallest skeleton only in this run); signal correlation (add divergence processor later); attribution noise (conservative logging + paper forward tests). All paper-only per AGENTS safety.

## Implementation Notes
- See strategies/multi-signal-fusion.md for processor taxonomy, ASCII diagram, Rust trait sketch, journal attribution hooks.
- First code: post all wiki (this decision + log prepend + strategies/ + integrations/ + hermes update + project-plan/index), smallest src/strategy/ or ingester enhancement.
- Follow AGENTS exactly: wiki first (done), paper-only, trading code heavily commented with risk, rust_decimal, update log for changes, fmt/clippy before done.
- Later: full processors, learned fusion, 3.4/3.5 dashboard/obs.
- **Skeleton vs. production reconciliation (2026-05-25)**: Wiki sketches describe target (async/typed); delivered is sync + serde_json::Value (smallest, matches ingester). See multi-signal-fusion.md note + src/strategy/mod.rs. (Added for alignment.)

## Links
- strategies/multi-signal-fusion.md (primary)
- wiki/log.md (top entry)
- concepts/hermes-self-improvement.md (new fusion section)
- integrations/polymarket-apis-and-data-sources.md
- docs/project-plan.md (Phase updates)
- Other decisions (ingester, hermes loop, MM)
- Source: Polymarket-BTC-15-Minute-Trading-Bot/core/strategy_brain/{fusion_engine/signal_fusion.py,signal_processors/base_processor.py + siblings}, feedback/learning_engine.py

*Created wiki-first before any code. Part of clean handoff for re-review (0 issues target).*