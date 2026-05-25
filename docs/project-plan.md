# polytrader Project Plan

**Version**: 0.1 (Bootstrap)  
**Date**: 2026-05 (initial)  
**Owner**: simonellefsen + agents

## Mission Summary

Automate participation in Polymarket prediction markets using a Rust/Dioxus 24/7 agentic system with strong self-improvement loops (Hermes) and an LLM-consumable wiki. Prioritize safety via realistic paper trading before any real capital.

See [MISSION](../MISSION) and [wiki/index.md](../wiki/index.md).

## Key Research Findings (Polymarket)

### Simulation / Paper Trading Support

**Conclusion**: Polymarket has **no official sandbox, testnet, or paper trading environment**.

- All trading occurs on Polygon mainnet (chain ID 137) against real USDC (or collateralized positions).
- The CLOB and Gamma APIs are live/prod only.
- Community has built several paper trading simulators (e.g. Python-based orderbook replay with local matching).

**Implication for polytrader**:
- We **must** implement our own high-fidelity paper trading engine as Phase 0/1 deliverable.
- The simulator will consume **live public data**:
  - Gamma API (markets, events, prices, volumes)
  - CLOB public endpoints (order books, recent trades, tickers, prices) — no auth required for reads.
- Simulation fidelity requirements:
  - Realistic taker fees (Polymarket ~0.5–2% depending on volume/rewards; confirm exact).
  - Slippage modeled from live orderbook depth + configurable impact function.
  - Latency simulation.
  - Partial fills, queue priority (simplified FIFO or pro-rata).
  - Resolution & payout simulation (using market outcome when resolved).
- Later: "shadow" mode where real orders are placed in parallel with paper for comparison (high risk, gated).
- Third-party paper traders (e.g. https://github.com/agent-next/polymarket-paper-trader) can be studied for techniques but we own our implementation for integration with our journal + Hermes.

### API & SDK Landscape (May 2026)

**Strong official Rust support** (preferred path):

- Primary maintained SDK: **polymarket_client_sdk_v2** (https://github.com/Polymarket/rs-clob-client-v2)
  - Built on alloy (Ethereum signing), reqwest, rust_decimal, tokio.
  - Covers: L1 wallet signature → L2 API key derivation, full CLOB authenticated trading (market/limit orders, cancellations, WS), Gamma markets client, data APIs.
  - Actively referenced in official docs.
- Older `rs-clob-client` is archived.
- Other community crates exist (polyfill-rs for perf, etc.) but we start with the official v2 SDK.

**Data / Read APIs (public, no key)**:
- Gamma: https://gamma-api.polymarket.com/ (markets, events, prices, search, etc.)
- CLOB public: https://clob.polymarket.com/ (orderbook, trades, ticker, prices, ws public)
- Data API (some require key): https://data-api.polymarket.com/

**Authentication (two layers)**:
1. **L1**: EIP-712 signature from a Polygon wallet (private key or hardware) to obtain/derive L2 API credentials (apiKey, secret, passphrase). One-time or refresh.
2. **L2**: For every trading request, 5 custom headers + HMAC-SHA256 signature over timestamp + method + path + body using the L2 secret.

**Order Types** (CLOB):
- GTC, FOK, IOC, GTD.
- Market (immediate), Limit.
- Buy/sell Yes or No shares (binary outcome shares priced 0–1).

**Risks / Notes**:
- Real funds required even for small tests (gas + USDC on Polygon).
- Rate limits exist (document in wiki/sources as we discover).
- Rewards program for volume — may affect effective fees.

**Recommendation**:
- Depend on `polymarket_client_sdk_v2` for the **real trading path**.
- Build a **PaperTradingEngine** trait + impl that shares the same high-level interface (submit_order, cancel, get_positions, etc.) but executes against simulated book + local matching engine fed by live public feeds.
- This allows the rest of the agent (strategy, UI, journal, Hermes) to be mode-agnostic.

## Architecture Overview

### Services / Deployables

1. **polytrader** (main Rust binary + Dioxus)
   - Long-running Tokio runtime.
   - Components:
     - Market data ingester (Gamma polling + CLOB WS/public feeds).
     - PaperTradingEngine (core simulation).
     - (Future) RealTradingAdapter (wrapping the official SDK, behind kill switch).
     - Decision / strategy engine (initially simple rules + LLM-assisted; later more sophisticated).
     - Journal writer (to Postgres).
     - Dioxus web server (dashboard: markets browser, paper portfolio, open sim orders, P&L charts, Hermes reflections, config).
   - Exposes: HTTP (Dioxus), perhaps gRPC or internal for Hermes.
   - Config: trading mode (paper only initially), risk params, LLM endpoint, wallet (read-only for paper).

2. **hermes** (Rust binary, separate deployment)
   - Periodic + event-driven loops:
     - Post-resolution market reviews (outcome vs. pre-trade probabilities, edge quality).
     - Trade attribution & P&L explainability.
     - Anomaly / bug detection in journal.
     - Strategy experiment proposals.
     - Wiki updates / synthesis (LLM calls to propose patches to markdown).
     - Research tasks (e.g. "study historical resolution accuracy of certain categories").
   - Writes structured reflections + action items to DB + wiki (via PR or direct file update in dev).
   - Can be triggered manually from UI.

3. **postgres** (CloudNativePG, 2 replicas)
   - Primary + hot standby.
   - Schemas:
     - market_data (raw snapshots, orderbooks)
     - trading (paper_positions, paper_orders, paper_fills, virtual_portfolio)
     - journal (trades, reflections, experiments, decisions)
     - real_trading (gated; separate schema or row-level security later)
   - Backups, WAL archiving as per reference patterns.

### Data Flow (Paper Phase)

Live data (Gamma + CLOB) → Ingester → DB snapshots + in-memory book cache → PaperTradingEngine (on decision signals) → Journaled fills → Portfolio state → UI + Hermes input.

Decisions can be:
- Rule-based (e.g. probability mispricings vs. external signals)
- LLM-proposed (with human or higher-tier approval)
- Hybrid

### Tech Stack

- **Language**: Rust (primary). Dioxus for UI. sqlx + Postgres. Tokio. alloy (via SDK). rust_decimal. tracing.
- **LLM**: xAI Grok API (or configurable). Used for analysis, reflection, wiki drafting, strategy ideation. Not for direct high-stakes order placement without oversight initially.
- **UI**: Dioxus (full Rust, server-rendered + interactive hydration). Charts via echarts or dioxus-charts or canvas.
- **Deployment**: Docker + Kubernetes (docker-desktop → cloud). kustomize.
- **Observability**: tracing + OpenTelemetry (future), structured logs, DB journal as source of truth.
- **Versioning**: Git. Wiki changes committed.

## Phased Roadmap

### Phase 0: Bootstrap & Foundations (Current)

- [x] Read MISSION, research Polymarket (API, SDK, simulation reality).
- [ ] Project skeleton: README, AGENTS.md, wiki/ structure, docs/plan.
- [ ] Basic Rust Cargo workspace or single crate with Dioxus template (`dx new` or manual).
- [ ] Postgres connection test + basic schema (markets, paper_trades).
- [ ] k8s namespace + basic cnpg 2-replica template (adapted from patterns).
- [ ] Initial wiki content: sources/polymarket-api.md, concepts/llm-wiki, hermes, etc.
- [ ] Hermes agent stub (binary that can read DB + call LLM + write reflection).
- [ ] CI / build basics (cargo check, fmt, clippy).

**Exit criteria**: Local `cargo run` shows a minimal Dioxus "hello polytrader" page. `kubectl apply` creates namespace + postgres instance (even if not fully wired).

### Phase 1: Paper Trading Core (Safety Critical)

- Implement PaperTradingEngine trait + realistic local matching.
- Live market ingester for a configurable set of markets (e.g. politics, crypto, sports).
- Basic strategy: e.g. "ingest external probabilities or simple heuristics, place paper limit orders when edge > threshold".
- Full journal + portfolio accounting (virtual USDC balance, share positions, realized/unrealized P&L, fees paid).
- Dioxus dashboard:
  - Market browser + current orderbook snapshot.
  - Paper portfolio & recent fills.
  - Manual paper order placement (for testing).
  - Basic P&L curve.
- Risk controls: max position size, category exposure, daily loss limit (sim only).
- Hermes: first reflection loop (e.g. "review last 50 paper trades, summarize edge quality").

**Exit**: Can run 24h in paper mode on docker-desktop, generate realistic fills against live books, Hermes produces initial wiki update proposals. All money movement is virtual.

### Phase 2: Self-Improvement & Polish

- Richer journal schema + full-text / vector search for reflections (pgvector?).
- Hermes advanced workflows: experiment runner (backtest ideas on historical data), automatic wiki synthesis, anomaly alerts.
- Dioxus UI: live WS updates, reflection viewer, "what-if" simulator, strategy config editor.
- Better data model: normalized events, outcomes, probabilities over time.
- Initial real-trading adapter (SDK integration) behind multiple explicit flags + UI kill switch. **Still disabled by default**.
- Observability, alerts (email/webhook on large paper drawdown).
- Documentation: runbooks complete, schema documented.

### Phase 3: Gated Real Trading & Scaling

- Human approval workflows for real orders (or staged rollout: small size → larger).
- Production risk engine (circuit breakers, kill switches, wallet balance monitoring).
- Multi-market, multi-category strategies with portfolio optimization.
- Rewards optimization, fee-aware execution.
- Cloud deployment (real secrets, monitoring).
- Advanced agent: multi-step reasoning, external data sources (news, on-chain, social), ensemble models.
- Audit & tax reporting helpers.

### Phase 4+: Future

- Mobile Dioxus app?
- On-chain verification of agent decisions (ZK or simple commitments)?
- Community / shared strategy marketplace?
- Integration with other prediction platforms.

## Open Questions & Risks

1. **Wallet & key management** for real mode (HSM? separate signer service? never put hot wallet in main pod).
2. **Exact fee schedule & rewards impact** — measure empirically in paper first.
3. **Resolution disputes / UMA oracle risks** — track historically via Hermes.
4. **Rate limits & reliability** of public APIs under 24/7 polling.
5. **Dioxus maturity** for complex interactive dashboards (charts, live tables) vs. Leptos or even a small TS frontend. Re-evaluate in Phase 1.
6. **LLM cost & latency** for Hermes loops at scale.
7. **Legal / ToS** implications of automated trading on Polymarket (disclose if required; use responsibly).

## Next Immediate Steps (as of this writing)

See [wiki/log.md](../wiki/log.md) for the living task list.

1. Initialize Rust + Dioxus project (Phase 0).
2. Stand up local cnpg postgres via k8s manifests (even empty).
3. Flesh out full wiki/ (api sources, trading concepts, schema draft).
4. Prototype minimal market ingester + paper orderbook fetcher in Rust.
5. Define initial DB schema (migrations via sqlx or refinery).
6. Scaffold Hermes as separate binary in same workspace.
7. First end-to-end paper trade simulation loop (hardcoded simple strategy).

## Success Metrics (Early)

- 7+ consecutive days of stable paper trading with realistic volume and journal entries.
- Hermes produces at least one actionable wiki improvement or strategy tweak per day.
- Dashboard usable for manual oversight and review.
- Zero accidental real-money orders (enforced by architecture).

---

This plan is living. Update it via PR + wiki/decisions/ entries as we learn.
