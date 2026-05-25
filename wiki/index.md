# polytrader Wiki

**LLM-optimized project knowledge base.** This directory (and subdirs) is the primary context source for agents (Hermes, coding agents, etc.).

Start here, then follow links. For full context, also read:
- [../docs/project-plan.md](../docs/project-plan.md)
- [../AGENTS.md](../AGENTS.md)
- [../MISSION](../MISSION)

## Structure

- **concepts/** — Core ideas, architecture principles, trading mental models.
- **decisions/** — Important architectural, tech, process, and risk decisions with rationale.
- **experiments/** — Past/present/future experiments, backtests, ideas under test.
- **log.md** — Chronological living log of major progress, incidents, learnings.
- **runbooks/** — Operational procedures (build, deploy, debug, incident response).
- **schema.md** — Database schema, important data models, invariants.
- **sources/** — External references, API docs summaries, version pins, data dictionaries.

## Quick Navigation for Agents

1. **Understand the "why" and constraints**: Read [concepts/llm-maintained-project-wiki.md](concepts/llm-maintained-project-wiki.md), [concepts/hermes-self-improvement.md](concepts/hermes-self-improvement.md), [concepts/polymarket-trading.md](concepts/polymarket-trading.md) (create if missing).
2. **Current state & plan**: [../docs/project-plan.md](../docs/project-plan.md), this index, recent [log.md](log.md) entries.
3. **How to change things safely**: [../AGENTS.md](../AGENTS.md), relevant decisions/.
4. **Data & persistence**: [schema.md](schema.md).
5. **External truth**: [sources/polymarket-api.md](sources/polymarket-api.md) and subpages.

## Current Status (Summary)

**Phase**: Bootstrap / foundations.  
**Trading mode**: Paper trading only (mandatory until Phase 3 gates passed).  
**Key open item**: No official Polymarket paper trading; custom high-fidelity simulator required using live public data feeds.

See [log.md](log.md) for the latest entry and active tasks.

## Maintenance

This wiki is **LLM-maintained** with human oversight. Hermes agent is responsible for proposing and (in many cases) applying updates. See the llm-wiki concept page.

Last major refresh: 2026-05 (initial bootstrap).
