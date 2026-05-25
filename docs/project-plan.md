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

... [rest of plan unchanged up to here for smallest; see original for full]

## Open Questions & Risks

1. **Wallet & key management** for real mode (HSM? separate signer service? never put hot wallet in main pod).
2. **Exact fee schedule & rewards impact** — measure empirically in paper first.
3. **Resolution disputes / UMA oracle risks** — track historically via Hermes.
4. **Rate limits & reliability** of public APIs under 24/7 polling.
5. **Dioxus maturity** for complex interactive dashboards (charts, live tables) vs. Leptos or even a small TS frontend. Re-evaluate in Phase 1.
6. **LLM cost & latency** for Hermes loops at scale.
7. **Legal / ToS** implications of automated trading on Polymarket (disclose if required; use responsibly).

## 2026-05-25 Next Phase: In-app Authentication Flow Within the Web UI (Dioxus + Axum)

**Context (from user /implement request + constraints)**: Immediately after fees/tax/latency/tiers (IMPL 8c5bc837) and operational goals work. "Next phase" focus: make the web UI itself have a proper auth flow (so it can stand alone independent of ngrok edge SSO, provide user identity inside app e.g. for future personal paper bankroll attribution / per-user journal, "logged in as" in UI). Preserve 100% verified (subpath + <base> brittle string, SSR fidelity from rsx, live JS relative fetches, k8s probes, existing endpoints, no impact on paper/ingester/hermes/strategy, paper-only, $150 context).

**Delivered (wiki-first + smallest viable, no new deps/Cargo, no migs, no main.rs edit)**:
- Config extensions (src/config.rs): GOOGLE_* + ALLOWED_EMAILS + cookie secure flag (clap/env, defaults safe, no breakage).
- Minimal Google OAuth2 flow in Axum (src/server.rs only): /auth/login (state nonce redirect), /auth/callback (reqwest exchange + userinfo, allowlist or any-paper, session), /auth/logout, /auth/whoami (JSON). Dual-mode: prefer ngrok forwarded headers (x-*-email etc) else cookie session. Static OnceLock+Mutex stores (no AppState/main change). Manual cookie/header parse. Heavy RISK/AGENTS comments (hijacking, leakage, subpath Path=, CSRF state, ngrok trust, $150 data exposure, open redirects, no secrets).
- Dioxus UI (src/ui/app.rs only): smallest rsx additions for Login button / user chip + logout link (relative /auth/* under <base>); existing script enhanced for /auth/whoami fetch + DOM populate (exact live-fetch pattern, no sig change, no SSR hack).
- Wiki-first: full detailed log entry appended at EOF of wiki/log.md (distinct "Next Phase" section, modeled exactly on fees entry, reconciliation note for concurrent fees fix-round-1 on top entry; no overlap to fees content). Updates to runbooks/deploy-public-ngrok.md, index.md, this project-plan. Multiple re-reads + git verify before any src.
- All prior behavior 100% (SSR <base> injection, fetches, health public, k8s, no fees files touched).
- fmt/clippy -- -D clean, tests pass.

**Design decisions + rationale** (documented in log + code):
- No new Cargo (avoid overlap with concurrent fees clob-ws edit on Cargo.toml; use reqwest/uuid/chrono/std::sync::OnceLock+Mutex already available or std).
- In-mem sessions + cookie (not DB table): smallest, no mig/wiki-PR yet (per constraints "prefer cookie... if fits"; future after wiki).
- Dual mode + edge header trust: supports both ngrok-protected deploys and standalone/local (key for "self-contained UI").
- Subpath cookie Path + redirect_uri: critical for /polytrader deploys (modeled on existing subpath_prefix logic).
- Optional auth for now (routes public, UI shows status): conservative for paper; foundation for future per-user without breaking.
- Static store: avoids editing main.rs (which was mutated by fees).
- Security minimal but by-design for paper reviewer: state, no open redirect, flags, comments everywhere.
- Credits: none from 5 polymarket repos for *UI* auth (CLOB L1/L2 only); patterns from Axum std + prior deploy ngrok entries in wiki/log/runbook.

**Verification**: See /tmp/grok-impl-summary-5701dfea.md + wiki/log.md entry (git status before/after, cargo fmt/clippy full outputs, re-reads of wiki, sample manual flow notes with envs).

**Next for this phase (out of scope here)**: Full DB-backed sessions table (wiki/schema + mig PR), PKCE, token refresh, production secret mgmt (k8s secrets not env), k8s e2e with real Google client in test allowlist, per-user paper attribution wiring, "logged in" in Hermes reflections.

See wiki/log.md (bottom append) and runbook for complete commands/rationale/anti-pattern handling. Wiki-first + AGENTS + past-issues briefing strictly followed; no overlap with fees-mutated files.

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
