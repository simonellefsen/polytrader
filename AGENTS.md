# AGENTS.md — polytrader

Guidelines for humans and AI agents working on this codebase.

## Philosophy

- **Safety first, always.** We start with paper trading only. Real money trading requires explicit human approval gates, strict risk limits, and thorough review.
- **Self-improving system.** The Hermes agent + LLM-maintained wiki is a first-class citizen. Code changes, trade outcomes, experiments, and reflections must feed back into documentation and strategy evolution.
- **Primarily Rust.** Core logic, trading engine, data models, Dioxus UI, and Hermes should be Rust where practical. Use Python only for complex ML/experiments if Rust crates are immature (justify in decisions/).
- **Observable & journaled.** Every significant action (market update, simulated fill, decision, reflection) should be logged and/or written to the journal tables for later analysis.

## Project Structure (Target)

```
.
├── Cargo.toml
├── src/                  # Main polytrader binary + library
│   ├── main.rs
│   ├── api/              # Polymarket client wrappers + paper trader
│   ├── models/
│   ├── db/
│   ├── ui/               # Dioxus components & router
│   ├── hermes/           # (or separate binary) reflection logic
│   └── ...
├── wiki/                 # LLM-ingestible knowledge base (critical)
│   ├── index.md
│   ├── concepts/
│   ├── decisions/
│   ├── experiments/
│   ├── log.md
│   ├── runbooks/
│   ├── schema.md
│   └── sources/
├── docs/                 # Human-oriented deeper docs
├── deploy/
│   ├── k8s/
│   │   └── base/
│   │       ├── namespace.yaml
│   │       ├── kustomization.yaml
│   │       ├── postgres/...
│   │       ├── polytrader.yaml
│   │       └── hermes.yaml
│   └── scripts/
├── Dockerfile*
└── README.md, AGENTS.md
```

## Coding Standards

- Prefer clarity and auditability over micro-optimizations.
- All trading-related code must be heavily commented with risk implications.
- Database schema changes go through `wiki/schema.md` + migration PR.
- New concepts/strategies documented in `wiki/concepts/` or `experiments/` before heavy implementation.
- Use `rust_decimal` for all money/price/position math (never float for finance).
- Configuration via env + (later) config CRD or file. No secrets in repo.
- Async: Tokio + sqlx (postgres) + reqwest.

## Documentation & Wiki

- The `wiki/` directory is the single source of truth for agent context and project memory.
- Update relevant wiki pages (especially `decisions/`, `log.md`, `experiments/`) as part of any non-trivial change.
- Hermes agent will propose and/or apply wiki updates autonomously (with human review for high-impact items).
- Keep `sources/` up to date with API versions, data sources, external references.

## Hermes Agent

- Runs as a separate long-lived process/deployment.
- Responsibilities: periodic market outcome review, trade P&L attribution, strategy backtesting proposals, anomaly detection, wiki maintenance, experiment suggestions.
- Must produce structured reflections that are stored and surfaced in the UI.

## Deployment

- Target: Kubernetes (docker-desktop dev, later cloud).
- Postgres: CloudNativePG cluster with 2 replicas (primary + standby) as specified.
- Namespace: `polytrader`.
- Use kustomize bases + overlays.
- All services containerized. Prefer multi-stage Rust builds.

## Trading Safety Rules (Non-Negotiable Early Phase)

1. Paper trading mode is the **only** enabled path until explicitly unlocked.
2. Paper trader must realistically model: taker fees, slippage (based on orderbook depth), latency, partial fills.
3. Position sizing, max exposure per market/category, daily loss limits enforced in simulator and (later) real path.
4. No auto-approval of real orders. Human-in-the-loop or strict circuit breakers required.
5. All order intents (paper or real) logged with full context before execution.

## When Adding Features

1. Write or update the relevant wiki entry first (concept / decision / experiment).
2. Implement behind feature flag or paper-only if trading related.
3. Add tests + journaled observability.
4. Update runbooks if deployment/ops impact.
5. Run Hermes-style reflection mentally or invoke it: "What did we learn? What should be documented?"

## References

- Polymarket API & SDK docs (see wiki/sources/)
- Dioxus docs
- CloudNativePG documentation

Violations of safety or documentation discipline will be called out in reviews.
