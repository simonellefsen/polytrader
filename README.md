# polytrader

Autonomous / assisted agent for Polymarket prediction markets.

**Status**: Bootstrap / planning phase. See [wiki/index.md](wiki/index.md) and [docs/project-plan.md](docs/project-plan.md).

## Goals

- Build a 24/7 Rust/Dioxus application that monitors, analyzes, and trades (initially in simulation) on Polymarket.
- Strong emphasis on **safety**: start exclusively with high-fidelity paper trading against live orderbooks and data.
- Self-improving system via a dedicated **Hermes agent** that performs research, reflection, and documentation updates.
- LLM-maintained project wiki for knowledge capture and agent context.
- Production-grade deployment: Kubernetes (docker-desktop initially), CloudNativePG (cnpg) Postgres with replicas, containerized services.

## Core Components

- **polytrader** (main): Rust + Dioxus web UI. Long-running service handling market data ingestion, paper trading engine, decision loops, real trading path (gated), and serving the dashboard.
- **hermes**: Background agent for monitoring, trade journaling analysis, outcome review, strategy experiments, and wiki/self-improvement updates.
- **postgres** (cnpg, 2 replicas): Persistent store for markets, positions (paper + real), order history, reflections, journal entries.
- Supporting: Docker, k8s manifests, configuration, observability scaffolding.

## Key Decisions (Early)

- **No official paper trading/sandbox** on Polymarket. All trading is live on Polygon mainnet with real USDC collateral. We implement our own high-fidelity paper trading simulator using public Gamma API + unauthenticated CLOB endpoints (order books, tickers, trades).
- Leverage the **official Polymarket Rust SDK v2** (`polymarket_client_sdk_v2`) for the authenticated real-trading path. Do not re-implement low-level auth, signing, or CLOB protocol.
- UI: Dioxus (Rust web + SSR/interactive dashboard). One primary Rust codebase.
- Architecture inspired by proven patterns: structured wiki/, deploy/k8s/, separate hermes process, heavy journaling for LLM reflection.

## Quick Start (Future)

See runbooks once implemented.

## Links

- Polymarket: https://polymarket.com/
- API Docs: https://docs.polymarket.com/
- GitHub (this project): https://github.com/simonellefsen/polytrader
- Official Rust SDK: https://github.com/Polymarket/rs-clob-client-v2

## Contributing / Agent Notes

See [AGENTS.md](AGENTS.md) and the wiki for conventions.
