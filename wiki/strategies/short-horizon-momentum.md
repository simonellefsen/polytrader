# Short-Horizon Momentum & Predictor Strategies

**Last updated**: 2026-05-25 (wiki-first per approved plan + AGENTS.md)  
**Credits**: Inspired by **openclaw-ai-polymarket-trading-bot/** (src/engine/features.ts, predictor.ts, paperTrader.ts, positionStore.ts, models/llmScorer.ts, connectors/polymarket.ts + orderExecution.ts, main.ts, uiServer.ts). Clean TS architecture for feature extraction, LLM-augmented prediction/scoring, paper trading simulation, and short-term momentum/edge signals on Polymarket. Complements data from agents/ (news/social connectors) and BTC bot (tick velocity / divergence processors).

**Overview**
OpenClaw provides a modern full-stack (TS) example of a Polymarket bot with strong separation: connectors for data/auth, engine for features/prediction/paper, LLM scorer for edge, UI server. Short-horizon momentum (tick velocity, recent order flow, feature-based predictors) is a natural complement to longer-term fusion or AI search. Directly informs 3.2 processors + 3.4 dashboard.

**Key Transferable Patterns**
- Feature engineering (features.ts): derived metrics from orderbooks, prices, volumes, on-chain hints.
- Predictor + LLM scorer (predictor.ts, llmScorer.ts): hybrid statistical + LLM for probability/edge.
- Paper trader + position store (paperTrader.ts, positionStore.ts): realistic simulation with fills.
- Execution connector with signing (clobSignature, orderExecution).
- UI + server for live monitoring (uiServer.ts, ui/).

**Mapping to polytrader**
- Ingester snapshots feed features (extend current clob_public/Gamma).
- Paper engine (src/paper/) can incorporate momentum filters.
- Hermes LLM calls (hermes.rs) parallel the llmScorer pattern.
- Dioxus UI (post-Phase 2 hydration) for predictor outputs.

**Rust Port Notes (Exact Patterns)**
- Feature structs + pure computation (no side effects) — easy to port to Rust modules.
- Predictor trait returning Decimal edge/prob + explanation (for journal + Hermes).
- Use existing reqwest/LLM path for scorer; keep gated.
- Heavy comments: "Risk: Short-horizon momentum decays fast in thin markets; combine with liquidity filters (market-making-liquidity.md) and fusion. Paper simulation must model latency + partial fills accurately per AGENTS paper rules. Momentum signals prone to reversal at resolution — always cross with outcome probs."
- Journal every momentum signal + prediction for post-resolution attribution.

**Paper Safety**
- Momentum signals only influence paper orders. Strict inventory/ exposure controls.

**Phase 3.2 / 3.4 Ties**
- One of the 1-2 initial processors in smallest FusionEngine skeleton (e.g. simple tick velocity from orderbook snapshots).
- Dashboard: momentum heatmaps + predictor confidence cards (build on Phase 2 SSR).
- Complements multi-signal-fusion (momentum as dedicated processor) and ai-edge-kelly (hybrid LLM features).

See integrations/polymarket-apis-and-data-sources.md (connectors), other strategies/, concepts/hermes-self-improvement.md (learning which momentum regimes work).

*Wiki-first doc with explicit credits to openclaw paths. Enables targeted 3.2 code.*