# Polymarket API & Data Sources

**Last verified**: 2026-05 (bootstrap research)  
**Important**: Always cross-check official docs before relying on specifics; APIs evolve.

## Official Documentation

- Main API reference: https://docs.polymarket.com/api-reference/introduction
- Client SDKs page: https://docs.polymarket.com/api-reference/clients-sdks
- Authentication: https://docs.polymarket.com/api-reference/authentication

## Core APIs

### 1. Gamma API (Markets & Metadata)

- Base: `https://gamma-api.polymarket.com/`
- Purpose: Rich market/event data, search, prices, volumes, resolutions.
- Key endpoints (public):
  - `/markets` — list/search markets with filters (active, resolved, category, etc.)
  - `/events`
  - `/prices` (historical + current)
  - Individual market by slug or id.
- Used heavily by paper trading ingester for discovery and ground-truth outcomes.

### 2. CLOB (Central Limit Order Book) — Trading & Orderbook

- Base: `https://clob.polymarket.com/`
- Public (no auth) read endpoints:
  - `/orderbook` (or per-market)
  - `/trades`
  - `/ticker`
  - `/prices` (mid, best bid/ask)
  - WebSocket for live updates (public channels).
- Authenticated (L2 headers required) write endpoints:
  - POST orders (market, limit, various TIF)
  - Cancel, batch cancel
  - Get open orders, positions (for the API key's wallet)

### 3. Data API

- `https://data-api.polymarket.com/`
- Some endpoints public, many require API key (L2).
- Historical fills, PnL, volume, etc.

### 4. Official Rust SDKs (Preferred)

**Current primary**: `polymarket_client_sdk_v2`  
Repo: https://github.com/Polymarket/rs-clob-client-v2

- Actively referenced in official docs (as of 2026-05).
- Features: L1→L2 key derivation (alloy signer), full authenticated CLOB client, Gamma client, WS, order builders, decimal-safe types.
- Dependencies it brings: alloy, reqwest, rust_decimal, chrono, serde, etc.
- **Recommendation**: Depend on this crate for the **real trading adapter**. Do not duplicate its auth or low-level protocol logic.

Older archived repo: https://github.com/Polymarket/rs-clob-client (do not use for new work).

Other community Rust crates (polyfill-rs, etc.) exist for specialized performance needs; evaluate later.

### 5. Authentication Model (Two-Layer)

1. **L1 (Wallet)**: EIP-712 structured signature from a Polygon-compatible private key (or Ledger/Trezor via alloy) to create or refresh L2 credentials for a given wallet address.
2. **L2 (API Key)**: Short-lived or long-lived creds (apiKey, secret, passphrase). Every trading HTTP request must include 5 headers + HMAC signature computed with the L2 secret.

The official v2 SDK abstracts most of this.

**Critical for polytrader**:
- Paper trading path **never** needs L1/L2 keys (uses only public reads).
- Real path will require secure secret injection (never in git, prefer k8s secrets + external signer if possible).

## Market Mechanics (Summary for Agents)

- Binary (Yes/No) outcome shares.
- Price = probability (in cents, 0–100 or 0.00–1.00 depending on representation).
- Collateral: USDC on Polygon (or wrapped variants).
- Resolution: Usually via UMA optimistic oracle or Polymarket-specific process; disputes possible.
- Fees: Taker fees apply; volume-based tiers + rewards program can reduce effective cost or pay rebates. Measure live.
- Liquidity: Varies enormously by market (election markets can be deep; niche can be thin → high slippage).

## Paper Trading Data Sources (Our Simulator)

We will **not** use authenticated trading endpoints for simulation.

Instead:

- Poll Gamma for market list + metadata + historical prices.
- Poll / WS public CLOB for live orderbook snapshots + recent trades (to build realistic book + simulate impact).
- When "placing" a paper order: match against cached live book state + apply configurable slippage + fee model.
- On resolution (from Gamma): auto-settle paper positions at 0 or 1 and credit collateral.

This gives **realistic live-market conditions** without risking capital.

## Rate Limits, Reliability, Costs

- Document observed limits here as we run (Hermes will help track).
- Public endpoints are generally generous but not unlimited for 24/7 high-frequency.
- WS connections should be used for efficiency where possible.
- Polygon gas for real settlements (small but non-zero).

## External References & Community Tools

- https://github.com/agent-next/polymarket-paper-trader (Python paper trading + MCP example — study matching/slippage logic, do not copy wholesale).
- Various open-source bots and dashboards on GitHub (search "polymarket bot").
- Polymarket agent-skills repo (if public patterns emerge).

## Version Pins & Change Log

- 2026-05: v2 Rust SDK is the canonical choice per official docs.
- Monitor https://github.com/Polymarket for new official Rust releases or breaking changes.

**Action for agents**: Before implementing any new integration, re-read this page + the live official docs links. Update this page with any discrepancies found.
