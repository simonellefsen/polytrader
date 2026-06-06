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

- real-order-approval-flow.md (operator-facing human + final review approval workflow for gated real CLOB; risk/collateral snapshots at approve time, UUID flow to submit/dispatch reval, UI panels + pending lists, operator binding via AuthUser, fail-closed invariants; see wiki/log.md 2026-06-03 tranche + schema updates; 2026-06-06 Hermes closed-loop extension on enriched approval events + pre-dispatch linkage for P&L/attr + gated wiki proposals, see log.md; + 2026-06-06 smallest additive UI polish for approval lists ergonomics (richer snap/coll evidence + Hermes attribution hints reuse in existing panels/notes, no new ids/markers), see log.md; + 2026-06-06 Hermes 5-min Decision Report cadence stub integration in self-imp loop (decision_reports_considered_24h + dr_cadence note for visibility/attribution per goals wiki "Ready for next", reuse of approval patterns, no UI change), see log.md; + 2026-06-06 wiring of actual 5-min Decision Report generator (additive in main.rs using existing journal writer extension + strategy::FusionEngine::fuse_net for net_edge_after_fees PRIMARY per goals/strategy skeleton; journals reuse 'decision_report' events jsonb; hermes load now does real COUNT replacing 0 stub + updates cadence/attribution/recs; makes self-imp data actionable, advances tracked 5-min DR cadence; no UI/SSR/deploy change, all prior surfaces preserved), see log.md; + 2026-06-06 extend `do_reflection` to read recent decision reports (smallest self-imp continuation making journaled 5-min DR net_edge PRIMARY actionable in Hermes for attribution vs paper outcomes/approvals + start backtest harness per goals-and-operational-cadence.md "Extend `do_reflection` to also read recent decision reports" + "backtest" + log "Ready for next / backtest" + decisions/project-plan tracked; additive query+metrics in existing hermes.rs only + wiki; dedicated mock test for new path; 100% surfaces preserved; local cargo green), see log.md).
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
