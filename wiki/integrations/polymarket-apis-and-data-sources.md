# Polymarket APIs and Data Sources — Transferable Patterns from Community Bots

**Last updated**: 2026-05-25 (wiki-first transfer kickoff per approved plan + AGENTS.md)  
**Status**: Initial synthesis for Phase 3.1+ (data/ingester enhancements). Living doc; update via Hermes or PR.

**Primary Credits & Inspiration** (explicit per task; all 5 repos deeply analyzed via list_dir/read/grep):
- **Polymarket-BTC-15-Minute-Trading-Bot** (core/ingestion/adapters/unified_adapter.py, managers/websocket_manager.py + rate_limiter.py, validators/data_validator.py, providers/custom_data_provider.py, core/nautilus_core/providers/, execution/polymarket_client.py, data_sources/ with binance/coinbase/news + solana adapters, patch_gamma_markets.py): Excellent unified ingestion, WS managers with reconnection/rate limit, data validation, multi-source adapters, custom providers. Fusion/strategy brain downstream consumers. Strong observability (performance_tracker, grafana).
- **openclaw-ai-polymarket-trading-bot** (src/connectors/polymarket.ts, src/engine/paperTrader.ts + features.ts + predictor.ts, src/clobSignature.ts + clobVerify.ts, config.ts): TS/JS patterns for CLOB auth (signing), connector abstraction, paper trader integration, LLM scorer in models/llmScorer.ts. Clean separation of data fetch vs engine.
- **poly-maker** (poly_data/polymarket_client.py + websocket_handlers.py + data_processing.py + global_state.py + trading_utils.py, data_updater/, poly_stats/, main.py + update_markets.py): Heavy Python client for Gamma/CLOB polling + WS, data processing pipelines, account stats, merger utils (poly_merger/). Practical MM/liquidity aware data handling.
- **Poly-Trader** (fetch_polymarket_data.py, fetch_order_book.py, fetch_real_markets.py + polymarket_ai_*.py + polymarket_combined.py + place_*.py, app.py): Broad fetchers for markets/orderbooks, AI search/simple/combined, bet history, profits tracking. Good for edge detection + historical.
- **agents/** (agents/polymarket/polymarket.py + gamma.py, agents/application/trade.py + executor.py + prompts.py, connectors/, utils/): Python agentic patterns (trade executor, cron, prompts for LLM), Polymarket/Gamma wrappers, news/search connectors. Emphasis on autonomous execution loops with human gates in design.

See also existing [../sources/polymarket-api.md](../sources/polymarket-api.md) for baseline API reference (Gamma https://gamma-api.polymarket.com/, CLOB https://clob.polymarket.com/ public reads + auth for writes).

## Core APIs (Public Reads for Paper Mode — Our Focus)

### Gamma API (Markets, Events, Prices, Metadata)
- Base: `https://gamma-api.polymarket.com/`
- Key public endpoints used across bots: `/markets` (search, active/resolved, category, slug/id), `/events`, `/prices` (historical/current), individual market lookup.
- Common patterns from sources:
  - Polling + caching (rate-limited; BTC bot uses custom rate_limiter + backoff; poly-maker uses data_updater + google utils for persistence).
  - Normalization + validation (BTC bot data_validator.py; openclaw features extraction).
  - Patch/fix scripts (BTC bot patch_gamma_markets.py for data quirks).
- In polytrader: Current `src/ingester/gamma.rs` + `mod.rs` `ingest_tick` uses it for `list_active_markets`, upserts to `market_data.markets` (jsonb raw + normalized outcomes/clob_token_ids), with polite sleeps.
- Transfer tip: Adopt unified_adapter + validator patterns from BTC bot for robustness; add historical price replay for backtests (Phase 3.5).

### CLOB Public (Orderbooks, Trades, Tickers, Prices)
- Base: `https://clob.polymarket.com/`
- Public (no auth for reads, critical for paper): `/orderbook`, `/trades`, `/ticker`, `/prices`, WS public feeds.
- Auth layer (for later real, gated): L1 EIP-712 wallet sig -> L2 apiKey/secret/passphrase; then L2 HMAC per req (see openclaw clobSignature.ts + clobVerify.ts; poly-maker abis + client; agents + Poly-Trader place_bet patterns).
- Common patterns:
  - Orderbook fetch per token_id (binary Yes/No shares priced ~0-1); depth for slippage modeling.
  - WS for low-latency (BTC bot websocket_manager.py with reconnection, managers/rate_limiter; poly-maker websocket_handlers.py + global_state).
  - Snapshot + diff handling; mid/ spread computation.
- In polytrader: `src/ingester/clob_public.rs` + mod.rs `ClobPublicClient::get_orderbook`, stores `market_data.orderbook_snapshots` (jsonb bids/asks arrays + mid + fetched_at), updates denorm mids on markets. `ingest_tick` loops outcomes, polite 250ms sleeps.
- Transfer tip: Port WS manager + reconnection + validator from BTC bot for Phase 3.1 enhancement (current is poll-only); add depth-derived slippage to paper engine. Avoid float — use rust_decimal everywhere (per AGENTS.md).

### Data API & Other (Selective)
- https://data-api.polymarket.com/ (some endpoints need key; used sparingly in bots for volume/rewards).
- On-chain (Polygon): for real fills later (all 5 repos touch via clients or direct); agents + Poly-Trader have wallet/balance checks.
- External augmentation (news/social, on-chain Solana in BTC bot data_sources/): for edge signals (Phase 3.2/3.3).

### Rate Limiting, Resilience, Validation (Key Transferable)
- All serious bots implement: exponential backoff, per-endpoint limiters (BTC bot rate_limiter.py, WS managers), retries, data validation (schema + outlier detection in BTC validator + openclaw features).
- Error observability: structured logs, performance tracking (BTC monitoring/), grafana (BTC grafana/).
- In polytrader current: basic warn on fetch fail in ingest_tick, no WS yet, conservative sleeps.
- **Recommendation for 3.1**: Enhance ingester with BTC-inspired unified adapter trait (Rust: trait DataSource + impls for Gamma/CLOB/WS), rate limiter crate or simple, json validation, publish raw events to journal for Hermes/attribution. Keep paper-only.

## Data Models & Ingestion Patterns
- Markets: id/slug/question/outcomes (array), clob_token_ids, active/closed/resolved, probs/liquidity/volume, raw_jsonb.
- Orderbook snapshots: token/market/outcome, bids/asks (price/size arrays as jsonb), mid, fetched_at. Append-only for audit (current schema.md).
- Portfolio/fills: paper_trading.* (virtual USDC, positions with Decimal shares/avg_entry/collateral, orders/fills).
- Journal (key for Hermes/3.3): reflections, experiments, decisions (use for signal performance attribution later).
- Bots patterns: normalized events over time (BTC nautilus-inspired data_engine), feature extraction (openclaw features.ts), processing pipelines (poly-maker data_processing).
- Current polytrader: sqlx upserts/inserts in ingest_tick (see src/ingester/mod.rs: ON CONFLICT for markets, INSERT snapshots; jsonb heavy; best-effort mid update).

**Porting Notes (Rust/Hermes)**: 
- Use `rust_decimal::Decimal` (never f64 for prices/positions per AGENTS + coding standards).
- sqlx for all DB (compile-checked, as in ingester/journal).
- Tokio + reqwest for clients; async WS (tokio-tungstenite or similar, modeled on bot managers).
- Journal every significant ingest/decision for observability (anti-pattern #5 avoidance: no silent fallbacks).
- PaperTradingEngine consumes snapshots (see src/paper/*).

## Phase 3.1+ Roadmap Ties (per approved plan)
- 3.1 data/ingester: WS + unified + validation + rate limit from BTC bot + poly-maker; more sources (news via agents connectors).
- 3.2 signals: Ingester emits normalized events for processors (orderbook, tick velocity, sentiment from bots).
- 3.5 obs/validation: Grafana-like + performance_tracker ports; data_validator patterns.
- Full credits + diagrams in sibling strategies/ pages.

**Risks (Paper Only)**: Public endpoints rate-limited/unreliable; always model slippage from depth; no real funds. All per AGENTS philosophy.

See strategies/ for how these feeds power fusion/MM/etc. Update this page as patterns are ported (Hermes will propose).

---
*This page created wiki-first before any code changes for the transfer. Explicit credits ensure attribution and self-improvement traceability.*