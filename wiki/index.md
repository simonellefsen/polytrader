# polytrader Wiki

**LLM-optimized project knowledge base.** This directory (and subdirs) is the primary context source for agents (Hermes, coding agents, etc.).

Start here, then follow links. For full context, also read:
- [../docs/project-plan.md](../docs/project-plan.md)
- [../AGENTS.md](../AGENTS.md)

## Structure

- **concepts/** — Core ideas, architecture principles, trading mental models.
- **decisions/** — Important architectural, tech, process, and risk decisions with rationale.
- **experiments/** — Past/present/future experiments, backtests, ideas under test.
- **integrations/** — (Added 2026-05-25 transfer) Polymarket APIs, data sources, and patterns from community bots (with explicit credits).
- **log.md** — Chronological living log of major progress, incidents, learnings.
- **runbooks/** — Operational procedures (build, deploy, debug, incident response).
- **schema.md** — Database schema, important data models, invariants.
- **sources/** — External references, API docs summaries, version pins, data dictionaries.
- **strategies/** — (Added 2026-05-25 transfer) Detailed strategy docs (multi-signal fusion, MM/liquidity, AI/Kelly edge, short-horizon momentum, **goals & operational cadence**, etc.) with ports, diagrams, credits to 5 repos, and Hermes integration notes.

## Quick Navigation for Agents

1. **Understand the "why" and constraints**: Read [concepts/llm-maintained-project-wiki.md](concepts/llm-maintained-project-wiki.md), [concepts/hermes-self-improvement.md](concepts/hermes-self-improvement.md), [concepts/polymarket-trading.md](concepts/polymarket-trading.md) (create if missing).
2. **Current state & plan**: [../docs/project-plan.md](../docs/project-plan.md), this index, recent [log.md](log.md) entries.
3. **How to change things safely**: [../AGENTS.md](../AGENTS.md), relevant decisions/.
4. **Data & persistence**: [schema.md](schema.md).
5. **External truth**: [sources/polymarket-api.md](sources/polymarket-api.md) and subpages.

## Current Status (Summary)

**Phase**: Phase 2 Self-Improvement & Polish + post-Phase 2 WASM prep start + 2026-05-25 wiki-first kickoff of polymarket-github 5-repo transfer (Phase 3 strategy brain subphases 3.1-3.5; see log.md top entry + new integrations/ + strategies/ + 4 decisions/ + hermes concepts extension + project-plan amend). Concurrent/next: 2026-05-25 in-app web UI auth flow (Google OAuth minimal + dual edge+app + user identity in Dioxus SSR/JS; see new log entry at bottom + runbook update; implemented wiki-first before src).  
**Trading mode**: Paper trading only (mandatory until Phase 3 gates passed).  
**Key open item**: Full WASM hydration + expanded tests + resolution triggers + deeper autonomous + transfer code increments (see log.md top for gaps + transfer execution; wiki structure for fusion/MM/AI/momentum/ingester now live with credits). UI now has foundation for per-user personalization via auth.

See [log.md](log.md) for the latest entry and active tasks.

## Maintenance

This wiki is **LLM-maintained** with human oversight. Hermes agent is responsible for proposing and (in many cases) applying updates. See the llm-wiki concept page.

Last major refresh: 2026-05-25 (post-Phase 2 deploy + fidelity wiki-git alignment fix round + fees fix-round-1 + next-phase auth wiki-first appends).
