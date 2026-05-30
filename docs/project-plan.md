# polytrader Project Plan

**Version**: 0.1 (Bootstrap)  
**Date**: 2026-05 (initial)  
**Owner**: simonellefsen + agents

## Mission Summary

Automate participation in Polymarket prediction markets using a Rust/Dioxus 24/7 agentic system with strong self-improvement loops (Hermes) and an LLM-consumable wiki. Prioritize safety via realistic paper trading before any real capital.

See [wiki/index.md](../wiki/index.md) and [AGENTS.md](../AGENTS.md).

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

**2026-05-25 L2 Auth Pivot (smallest viable UI component for paper-only learning, post-Google clarification)**: Per user verbatim clarification ("UI component to authenticate with Polymarket for the API https://docs.polymarket.com/api-reference/authentication (L2...)") + "/implement go with the plan above". Added smallest UI status/expiry + connect/derive flow (browser EIP-712 wallet sign + backend derive proxy + mem+cookie storage exact Google pt_sess pattern + dual coexist with live Google 5701dfea/978b365b dashboard layer; no Google code altered). Full paper gates + $150 + "zero effect on engine even if connected" + long-lived phrasing + heavy RISK. Wiki-first (log prepend + this integrations cross-ref + this note). See wiki/log.md top 2026-05-25 L2 58dff3a2 entry for complete details, Commands, verification, fidelity (Google preserved), credits (official docs 2026-05-25 + openclaw patterns), anti-patterns. No real trading/CLOB wiring/DB/Cargo change. Local gates (fmt/clippy/test 4/4) passed. (Future 3.4: gated real use behind AGENTS review.)

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
