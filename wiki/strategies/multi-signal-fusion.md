# Multi-Signal Fusion Engine

**Last updated**: 2026-05-25 (wiki-first per approved transfer plan + AGENTS.md)  
**Credits**: Heavily inspired by (and credits to) **Polymarket-BTC-15-Minute-Trading-Bot/core/strategy_brain/fusion_engine/signal_fusion.py** + **signal_processors/base_processor.py** (and siblings: divergence_processor.py, spike_detector.py, sentiment_processor.py, orderbook_processor.py, tick_velocity_processor.py, deribit_pcr_processor.py) + **strategies/btc_15min_strategy.py** + feedback/learning_engine.py. Also cross-referenced patterns from the other 4 repos for generality. Exact file paths in /Users/lindau/codex/polymarket-github/Polymarket-BTC-15-Minute-Trading-Bot/.

**Overview & Why Transfer**
The BTC bot demonstrates a clean, extensible multi-signal architecture for short-horizon (15m) edge in volatile markets — directly transferable to Polymarket binary markets (election, crypto, sports). Individual "processors" (specialized signal generators) produce normalized signals (strength, confidence, edge estimate, metadata). A central FusionEngine combines them (weighted sum, consensus, divergence detection, or learned model) into a single actionable score for the strategy/execution layer. Downstream: experiment tracking + closed-loop learning (which signals win on P&L after resolution) via feedback/learning_engine.

This directly supports polytrader Phase 3.2 (signal processors + FusionEngine) + 3.3 (Hermes enhancements + learning loop) + 3.5 (observability/validation of signal attribution).

**Current Baseline in polytrader (pre-transfer)**
- Ingester (src/ingester/mod.rs + clob_public/gamma) produces market + orderbook snapshots (sqlx upserts to market_data.* with jsonb + Decimal-friendly mids).
- No signal processors or fusion yet. Simple heuristics or direct LLM in future Hermes.
- Journal (src/journal/) + reflections in hermes.rs for P&L attribution (Decimal deltas) — perfect hook for per-signal contribution tracking.
- Paper-only engine (src/paper/).

**Processor Taxonomy (from BTC bot core)**
- BaseProcessor: abstract, common interface (compute_signal(market_state, context) -> Signal(score: float, confidence, edge, ts, metadata)).
- SpikeDetector: volatility/volume spikes.
- SentimentProcessor: news/social sentiment (via external adapters).
- DivergenceProcessor: price vs. on-chain or cross-market divergence.
- OrderbookProcessor: imbalance, depth, spread signals.
- TickVelocityProcessor: momentum in recent ticks.
- DeribitPCRProcessor: put/call ratio (options skew, adaptable to poly proxy data).
- Others extensible.

Fusion: signal_fusion.py aggregates (e.g. weighted average + rules for conflict/divergence; can be simple or ML).

**ASCII Diagram (Fusion Flow)**
```
Live Data (Gamma/CLOB/WS from ingester)
          |
          v
+-------------+   +-------------+   +-------------+
| Spike       |   | Orderbook   |   | Sentiment   |   ... (N processors)
| Detector    |   | Processor   |   | Processor   |
+------+------+   +------+------+   +------+------+
       |                 |                 |
       v                 v                 v
   Signal(score, conf, edge, meta)   [all normalized, Decimal-safe in port]
       \                |                /
        \---------------+---------------/
                        |
                        v
               +----------------+
               | FusionEngine   |
               | (weights,      |
               | consensus,     |
               | divergence     |
               | filter)        |
               +--------+-------+
                        |
                        v
               Fused Score + Attribution (per-signal contrib)
                        |
                        v
               Strategy / Paper Engine (risk gates)
                        |
                        v
               Journal (signal_type, score, market, timestamp, expected_edge)
                        |
                        v
               Resolution -> Hermes Reflection (P&L attribution per signal)
```

**Porting to Rust / polytrader (Exact Patterns to Follow)**
- **Trait for extensibility** (Rust equivalent of base_processor.py):
  ```rust
  use rust_decimal::Decimal;
  // in src/strategy/ (new, post-wiki)
  pub trait SignalProcessor: Send + Sync {
      fn name(&self) -> &'static str;
      async fn compute_signal(&self, market: &MarketSnapshot, ctx: &Context) -> Result<Signal>;
  }
  pub struct Signal { pub score: Decimal, pub confidence: Decimal, pub edge: Option<Decimal>, pub metadata: serde_json::Value, ... }
  ```
- **FusionEngine**: owns Vec<Box<dyn SignalProcessor>>, `fuse(&self, ...) -> FusedDecision { ... }`. Use Decimal for all math (AGENTS: "Use `rust_decimal` for all money/price/position math (never float for finance)").
- **Journal integration (critical for 3.3 Hermes learning)**: On fusion/decision, INSERT to journal (existing tables or lightweight extension via schema process) with signal contributions. Enables "which signals drove the profitable paper trades?" queries in Hermes reflections. No new migrations in smallest increment (use existing journal.reflections + metadata jsonb).
- **Heavy risk comments** (per AGENTS "All trading-related code must be heavily commented with risk implications"): e.g. "Risk: Overfitting to historical signals; mitigate via temporal train/test + forward paper testing only. Paper-only: no real capital exposure."
- **Inputs**: Extend ingester to feed normalized events (avoid duplicating fetches).
- **Skeleton vs. production note (reconciliation for this 2026-05-25 increment)**: The initial skeleton in src/strategy/mod.rs uses synchronous fns + &serde_json::Value (for immediate zero-dep compilability with existing ingester jsonb snapshots + no new types; matches current patterns exactly). The sketch above (async + custom MarketSnapshot/Context) represents the target evolution in later 3.2 wiring / 3.1 ingester enrichment per plan. See src/strategy/mod.rs for current; full typed/async + more processors in follow-ups. (Added for doc/impl alignment per review.)
- **Observability**: tracing + journal; later Grafana ports from BTC monitoring/.
- **Learning loop (3.3)**: Hermes (src/bin/hermes.rs) post-resolution attributes P&L delta to the signals active at entry time (via journal). Propose weight tweaks or new processors to wiki/experiments/.

**Paper-Only & Safety**
- All signals/fusion feed PaperTradingEngine only (src/paper/engine.rs). Strict risk limits (position size, exposure) in fusion output or engine.
- No real orders. Human review for any promoted strategy params (via Hermes proposals gated).
- Backtesting: use historical snapshots from market_data (Phase 3.5).

**Experimentation & Hermes Tie-in**
- Log every fused decision + per-signal scores to journal.
- Hermes experiment loop (wiki/concepts/hermes-self-improvement.md) can backtest processor subsets or weight sets on paper history.
- Closed-loop: after resolutions, measure "alpha per processor" (win rate, Sharpe contribution). Update fusion weights or disable weak signals autonomously (low-risk) or via proposal.

**Next Steps (per plan, smallest first)**
- 3.2: Skeleton FusionEngine + 1-2 processors (orderbook + simple momentum) in Rust, wired to ingester/journal (see Phase 3.2 in project-plan update).
- 3.3: Extend Hermes reflection to consume signal attribution.
- Full ports of other processors (sentiment via news connectors from agents/ or BTC data_sources/news_social/).

**Risks & Mitigations**
- Signal correlation / multicollinearity → divergence processor + regularization in fusion.
- Latency in multi-processor → async + caching (inspired by BTC rate limiters).
- Over-reliance on one source → multi-source (Gamma + CLOB + external) as in bots.

See integrations/polymarket-apis-and-data-sources.md for data feeds, ai-edge-kelly.md etc for complementary strategies, decisions/ for adoption rationale.

*Wiki-first creation. Explicit credits for traceability and Hermes consumption. Smallest viable doc to enable code increment.*