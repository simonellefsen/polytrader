# Market Making & Liquidity Provision Strategies

**Last updated**: 2026-05-25 (wiki-first transfer per approved plan + AGENTS.md)  
**Credits**: Inspired by **poly-maker/** (poly_data/polymarket_client.py, websocket_handlers.py, data_processing.py, trading_utils.py, data_updater/find_markets.py + trading_utils.py, main.py, update_markets.py, poly_stats/account_stats.py, poly_merger/). Strong focus on live data pipelines, liquidity-aware trading, account/position stats, and practical MM utilities for Polymarket. Also referenced patterns from other 4 repos for data feeds.

**Overview**
Poly-maker demonstrates production-grade data ingestion + trading utils tailored for Polymarket's CLOB, with emphasis on liquidity (depth, spreads, volume) and stats tracking. Useful for building market-making overlays or liquidity-sensitive signal filters on top of directional fusion (see multi-signal-fusion.md). Provides patterns for realistic slippage modeling, position management, and stats that feed risk/position sizing (Phase 3.4).

**Key Transferable Patterns from poly-maker**
- Client + WS handling for real-time books/trades (websocket_handlers + global_state for consistent view).
- Data processing pipelines (normalization, feature computation from raw snapshots).
- Trading utils (order construction, risk-aware sizing, fee/rewards awareness).
- Market discovery/updaters + stats (account performance, volume, P&L slices).
- Merger/safe helpers (poly_merger/ for JS-side reconciliation).
- Emphasis on live vs. snapshot consistency and error resilience.

**Mapping to polytrader**
- Current ingester (src/ingester/ + schema market_data.orderbook_snapshots with jsonb depth) provides the raw material.
- Paper engine (src/paper/) can be extended with liquidity-aware execution (modeled on poly-maker trading_utils).
- Journal + Hermes for post-trade liquidity impact analysis.

**Rust Port Notes (Follow Exact Existing Patterns)**
- Use Decimal for all liquidity calcs (depth sums, effective prices, slippage curves) — never float.
- Extend ClobPublicClient or add LiquidityAnalyzer that consumes snapshots.
- Store derived liquidity metrics in snapshots or separate table (via schema process later; no unnecessary migs now).
- Heavy comments: "Risk: Providing liquidity in low-volume markets can lead to adverse selection post-resolution; paper-only simulation of queue position and partial fills required. Always model taker fees + rewards impact."
- Integrate with FusionEngine output (e.g. "only MM when directional edge low but liquidity premium high").

**Paper Safety**
- MM in paper only: simulate maker rebates vs. taker fees, adverse selection on resolution.
- Position limits + inventory skew controls (inspired by poly-maker account_stats).

**Phase 3.4 Tie-in**
- Combine with risk/position/MM dashboard (new UI panels for liquidity heatmaps, simulated maker P&L).
- Hermes can analyze "liquidity provision alpha" separately from directional signals.

**Complements**
- multi-signal-fusion.md (use liquidity signals as one processor input).
- ai-edge-kelly.md (Kelly sizing adjusted for liquidity regime).
- integrations/polymarket-apis-and-data-sources.md (WS + client patterns).

*Wiki-first doc. Credits explicit. Enables smallest code for liquidity-aware extensions in 3.4 without scope creep.*