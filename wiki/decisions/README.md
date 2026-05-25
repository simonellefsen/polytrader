# Decisions

This directory records important, durable decisions with context, alternatives, and (retrospectively) outcomes.

Each decision file should contain:
- Date decided
- Problem / context
- Options considered
- Decision + rationale
- Consequences / follow-ups
- (Later) Review date + outcome

## Index (newest first)

- (Initial bootstrap decisions captured in [../../docs/project-plan.md](../../docs/project-plan.md) and early log entries. Individual decision files will be created as we formalize them.)
- 2026-05-25 wiki-git fidelity alignment for deploy/next-phase scaffolding (see log.md top "Fidelity amend" + Next phase bullet for the AGENTS process correction round choosing option a: wiki amend first then commit; no new .md file per smallest viable).
- 2026-05-25-adopt-multi-signal-fusion-from-btc-bot.md (adoption of multi-signal fusion + FusionEngine for Phase 3.2/3.3; credits to BTC bot signal_fusion.py + base_processor.py etc.; see strategies/multi-signal-fusion.md).
- 2026-05-25-port-market-making-liquidity-from-poly-maker.md (port MM/liquidity patterns for 3.4; credits to poly-maker/poly_data/*).
- 2026-05-25-hermes-fusion-learning-loop.md (Hermes closed-loop attribution for 3.3; credits to BTC learning_engine + Poly-Trader profits).
- 2026-05-25-data-ingester-enhancements-for-3-1.md (ingester WS/validation for 3.1; credits to BTC ingestion + poly-maker/openclaw clients/WS).

Example future entries:
- `2026-05-25-use-official-rust-sdk-v2.md`
- `2026-05-26-paper-trading-fidelity-requirements.md`
- `2026-06-XX-dioxus-vs-leptos-revisit.md`
