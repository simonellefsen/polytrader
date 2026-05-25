# Hermes Self-Improvement Agent

## Role

Hermes is the **meta-agent** of the polytrader system. While the main `polytrader` service focuses on market monitoring, paper trading execution, and user dashboard, Hermes focuses on **the system itself**:

- Learning from outcomes (trade P&L, prediction accuracy, resolution surprises).
- Detecting patterns, biases, and failure modes in strategies and data.
- Proposing and (where safe) applying improvements to code, parameters, prompts, wiki content, and experiments.
- Maintaining long-term project memory and coherence via the wiki.

It runs as a **separate deployment** (own binary, own resource limits, own restart policy) so it can be paused or scaled independently without touching live trading loops.

## Core Loops

### 1. Reflection Loop (Periodic + Triggered)

- On market resolution: fetch all paper (and later real) positions taken in that market, compare entry probabilities/edge assumptions vs. actual outcome.
- Daily/weekly: aggregate P&L attribution, win rate by category, edge decay over time, common failure signatures.
- Produce structured **reflection records** stored in DB (journal.reflections).
- Generate natural-language summary + concrete recommendations (e.g. "Avoid low-liquidity election markets after 48h pre-resolution; edge collapses").

### 2. Wiki Maintenance Loop

- Read recent reflections + relevant concept/decision pages.
- Identify drift (e.g. "our documented fee model is now inaccurate per latest volume tier").
- Propose patch (unified diff or section rewrite) with confidence and rationale.
- In autonomous mode (dev): apply low-risk patches directly + commit + log.
- High-risk or strategy changes: surface in UI for human approval.

### 3. Experiment Loop

- Maintain a backlog of hypotheses in `wiki/experiments/`.
- When idle or on schedule: select one, design a backtest or forward-test (paper), execute (using historical snapshots or live replay), record results.
- Promote winning experiments into production strategy config (with gates).

### 4. Anomaly / Health Loop

- Monitor journal for invariants violations (e.g. paper balance going negative, duplicate fills, API error rate spikes).
- Detect strategy "regime changes" (sudden performance drop).
- Alert via UI + (future) external channels. Suggest root-cause analysis.

## Implementation Notes (Target)

- **Language**: Rust (same workspace or separate crate in monorepo).
- **Inputs**:
  - Postgres (journal, market_data, trading schemas) via sqlx.
  - Filesystem (wiki/ markdown) — read + proposed writes.
  - LLM client (xAI Grok or configurable) for synthesis, critique, drafting.
- **Outputs**:
  - DB rows (reflections, experiment_results, proposed_actions).
  - Wiki file updates (git-committed in dev; PRs later).
  - Metrics / health scores.
- **Scheduling**: Tokio + cron-like (or external scheduler job that pokes an HTTP endpoint on hermes).
- **Safety**: Hermes **never** directly places real orders. It can only influence paper config and wiki. Any code changes it proposes go through normal review/CI.

## Data Model Touchpoints

(See [../schema.md](../schema.md) when written.)

Key tables Hermes owns or heavily writes:
- reflections
- experiments
- strategy_journal
- wiki_edits (audit log of automated changes)

## Relationship to Main Agent

- Main polytrader is the "trader".
- Hermes is the "research director + risk manager + librarian".
- They share the DB and (read-only for Hermes in early phases) some config.
- UI surfaces both: live trading state + "Hermes thoughts" panel with latest reflections and open proposals.

## Success Criteria

- After 30 days of paper trading, Hermes can point at 5+ concrete, measurable improvements it drove (e.g. "new category filter improved simulated Sharpe by 0.4").
- Wiki remains accurate and useful even as the team (human + agents) grows.
- New failure modes are caught by Hermes before they repeat 3x.

## Risks & Mitigations

- **Hermes hallucinating improvements** → All autonomous changes logged + reversible; high-impact gated.
- **Infinite reflection loops / cost** → Strict budgets, timeboxing, human review of expensive LLM calls.
- **Overfitting to past paper data** → Explicit train/test temporal splits in experiments; forward testing emphasized.

Hermes is what makes polytrader a *learning system* rather than a static bot.

## Phase 1 Implementation (2026-05-25)

Richer reflection loop implemented in `src/bin/hermes.rs` (separate binary/deployment preserved):
- Periodic (configurable ~5min) + DB-backed: reads `paper_trading.virtual_portfolio_snapshots`, `paper_trading.paper_fills`, `market_data.markets` using sqlx patterns matching server/journal.
- Local P&L attribution (Decimal deltas on realized/unrealized, fill counts, basic metrics JSON).
- Synthesis: always local NL summary + recommendations; optional LLM (reqwest to OpenAI-compatible /chat/completions via `LLM_API_ENDPOINT` + `LLM_API_KEY` envs only; robust timeout/error fallback to local-only, never secrets or real paths).
- Storage: INSERT to pre-existing `journal.reflections` (exact schema + writer pattern; structured + text).
- Logging: rich structured activity for observability; proposals for wiki/experiments noted in logs/DB for future autonomous loop.
- Safety: paper-only (no trading influence yet); full error handling; no DB tx needed for read+append here (idempotent inserts).

Dioxus UI skeleton introduced (see `src/ui/` rsx App with use_signal + demo + `server.rs` dashboard): functional design source + hand-written axum HTML mirror renders safety banner + live data views (JS sim of signals/fetch to preserved JSON endpoints). Hybrid axum preserves all Phase 0 probes, JSON, subpath rewrite+base compat exactly (no Dioxus router/SSR yet per smallest Phase 1).

Wiki updates (this file + log.md) performed first per AGENTS. No schema changes required. Future: Hermes will read its own reflections for drift detection + low-risk wiki patch proposals (autonomous or UI-gated).

This phase delivers the "working richer loops + Dioxus skeleton" per project plan. All per AGENTS.md (clarity, audit via journal+wiki, paper gate).

## Phase 2 Increment (2026-05-25)

- **Real Dioxus client hydration (SSR + live fetch reactivity)**: `src/ui/app.rs` rsx! (with use_signal) is now the actual rendered source of truth for the dashboard (via dioxus SSR in `server.rs` dashboard_handler). Removed hand-written mirror duplication. <base href> for /polytrader subpath rewrite compat injected in wrapper (exact Phase 0/1 fidelity preserved for probes at root, all JSON endpoints, public subpath). Client live updates: real fetch to relative /markets + /paper/portfolio (works under base) + DOM mutation for cards (vanilla JS sim of reactivity included in rsx output; full WASM hydration + server_fns + dioxus asset bundle deferred to keep Docker/Make/k8s-apply unchanged + smallest). Follows existing axum merge pattern + dioxus 0.7 fullstack/web/server features (already enabled).
- **Deeper Hermes autonomous low-risk behavior**: First gated wiki patch proposal logic in `src/bin/hermes.rs` (on top of richer P&L/LLM loop). Env-gated (HERMES_AUTONOMOUS_WIKI_PROPOSALS=lowrisk to enable; default off for safety). Generates safe, append-only proposal text (e.g. reflection summary + recs as markdown snippet/diff for wiki/concepts or log), embeds in stored recommendations + metrics, emits structured "autonomous_low_risk_wiki_proposal" log. Heavily commented for risk (never mutates fs in prod pods since runtime image has no wiki source; proposals are for human review via wiki PRs or future local apply). Builds directly on Phase 1 "wiki_proposal" log note + concept vision. Paper-only, journaled, Decimal, existing sqlx/reqwest patterns exactly. No new tables.
- **Initial real tests**: Explicit TODO(test) scaffolding in hermes.rs + ui/app.rs replaced with minimal viable #[cfg(test)] coverage (pure unit tests for P&L delta attribution logic + serde of Simple* structs; uses existing dev-dep tokio-test; no DB/network; exercises new paths + Phase 1 core; `make test` now has substance).
- Wiki-first (this update + log.md entry prepended before any src/Cargo), AGENTS followed exactly (paper gate 100%, smallest, patterns copied, no scope creep, fmt/clippy planned, deploy/make/k8s intact, observability).

All success criteria targeted for working verified increment per task. Hermes now produces richer autonomous proposal activity in logs when gated. Dioxus UI now truly driven by rsx (SSR). Feeds self-improvement loop. Next phase will add WASM bundle + experiment runner + more.

## Phase 3.2/3.3 Extension: Multi-Signal Fusion Engine + Closed-Loop Learning from External Bots (2026-05-25 transfer)

**Context from approved plan**: Direct acceleration of Strategy Brain / Self-Improvement via wiki-first transfer of proven patterns from the 5 polymarket-github repos (see wiki/log.md top entry for kickoff, integrations/polymarket-apis-and-data-sources.md, and the 4 new strategies/ pages with per-file credits).

**New Core Loop: Signal Fusion + Attribution**
- Ingester (enhanced per 3.1) emits normalized market/orderbook events (building on src/ingester/mod.rs sqlx + jsonb snapshots using exact existing patterns: PgPool, upsert, tracing, polite sleeps, Decimal for mids where ported).
- Dedicated processors (trait-based, see strategies/multi-signal-fusion.md for Rust sketch following base_processor.py) compute per-signal scores (Decimal strength/edge/confidence) for momentum (short-horizon from openclaw), liquidity (poly-maker), AI edge (Poly-Trader), spikes/divergence/sentiment/orderbook (BTC bot).
- FusionEngine aggregates (weighted, consensus, or divergence-aware) into fused decision + per-signal attribution metadata.
- Decision logged to journal (existing tables + jsonb for signal contribs; no new migrations in smallest increment) with full context (market, signals active, expected edge, risk params).
- Paper execution (src/paper/) uses fused output behind all existing gates.

**Closed-Loop Learning (Hermes Integration)**
- Post-resolution (or periodic): Hermes (src/bin/hermes.rs, richer P&L loop) queries journal for signals active on resolved trades.
- Computes attribution: realized/unrealized Decimal P&L delta broken down by signal type/processor (e.g. "spike_detector contributed +142 USDC virtual on 3 trades, momentum -18").
- Metrics: win rate, edge capture, decay per signal; regime detection (e.g. "momentum fails in low-liquidity").
- Synthesis: local + optional LLM (existing reqwest path) produces recs: "Increase weight on orderbook_processor; disable sentiment in election category; new experiment: hybrid Kelly from ai-edge-kelly.md".
- Output: structured reflection (existing INSERT), + gated autonomous wiki proposal (extend Phase 2 lowrisk gate) to update weights in config/wiki or propose new processor in experiments/.
- UI surfaces: reflection viewer + "signal performance" cards (builds on Phase 2 SSR).

**Experiment Tracking**
- Backlog in wiki/experiments/ (or new strategy experiments).
- Hermes can select fusion config variants, run paper forward-tests or historical replay on snapshots, record results (P&L curves per signal mix).
- Winning configs promoted (gated) to runtime fusion weights.

**Implementation Notes (Target, Smallest First)**
- **Skeleton vs. production reconciliation (2026-05-25 increment)**: Delivered skeleton (src/strategy/mod.rs) is sync + serde_json::Value for smallest viable (zero new deps/types, matches ingester jsonb). Target (async + typed snapshots) in follow-up per 3.2/3.3 plan. Attribution design (Decimal + json) matches exactly for Hermes. See multi-signal-fusion.md for details + credits. (Added for doc/impl alignment per review.)
- Language: Rust (new src/strategy/ or extension in ingester; trait SignalProcessor + FusionEngine struct).
- Inputs: sqlx from journal/market_data + ingester events (exact patterns from current ingester/journal).
- Outputs: journal rows (attribution), DB metrics, wiki proposals (via gated path in hermes).
- Safety (AGENTS non-negotiable): Paper-only. All fusion/attribution heavily commented with risk (e.g. "Risk: Correlated signals can amplify drawdowns; fusion must include explicit conflict/divergence gates + max exposure. Attribution relies on complete journal; never silent unwrap on missing signals — log and degrade gracefully.").
- No float: rust_decimal::Decimal for every score, edge, P&L slice, position impact.
- Observability: tracing + journal as source of truth (avoids anti-pattern #5 silent fallbacks).
- Scheduling: piggyback on existing hermes loop + ingest tick; no new deps initially.

**Explicit Credits (for Hermes consumption & traceability)**
- Fusion/processor design + learning_engine: Polymarket-BTC-15-Minute-Trading-Bot/core/strategy_brain/fusion_engine/signal_fusion.py + signal_processors/* + feedback/learning_engine.py + monitoring/.
- Short-horizon features/predictor/LLM scorer: openclaw-ai-polymarket-trading-bot/src/engine/{features.ts, predictor.ts} + src/models/llmScorer.ts + paperTrader.ts, positionStore.ts.
- AI edge + profits/Kelly-like: Poly-Trader/polymarket_ai_*.py + polymarket_profits.py + fetch_*.py.
- MM/liquidity data pipelines: poly-maker/poly_data/{polymarket_client,websocket_handlers,data_processing,trading_utils}.py + stats/.
- Agentic executor + prompts/gamma wrappers: agents/agents/{polymarket/*,application/trade.py,executor.py,prompts.py}.
- Data/ingestion resilience: cross-cutting from all 5 (esp. BTC unified_adapter + WS, openclaw connectors, poly-maker client/handlers).

**Risks & Mitigations**
- Overfitting to transferred signals → Temporal splits, forward paper testing only, Hermes anomaly detection.
- Attribution noise (partial fills, timing) → Conservative confidence intervals in Decimal math; journal every micro-decision.
- LLM hallucinated recs → Gated proposals only (existing lowrisk env); human review for weight changes.
- Cost/latency → Budgeted LLM calls; local synthesis first (as Phase 1/2).

**Success Criteria (for this extension)**
- After 7+ days paper with fusion: Hermes can attribute >=1 concrete improvement to a specific transferred signal (e.g. "orderbook_processor + fusion lifted Sharpe 0.3 in crypto category").
- Wiki + journal remain coherent; new processors added via experiment promotion.
- 0 real-money exposure; all verified Phase 0-2 behaviors (k8s-apply, hermes ts, subpath, probes, JSON, fmt/clippy) untouched.

### Operational Cadence & Goal Framework (added 2026-05-25)
See the new dedicated page `wiki/strategies/goals-and-operational-cadence.md` for the complete definition (conservative risk limits for $150 paper bankroll, daily/weekly goals focused on process + positive expectancy, 5-minute Decision Report layer powered by the FusionEngine, hourly Hermes reflection with explicit goal + per-signal P&L attribution).

This is the concrete operating rhythm that makes the abstract self-improvement loops measurable:
- **5 minutes**: Trader/decision layer runs signal processors + fusion → structured Decision Report (opportunities + attribution + risk/goal-filtered sizing) logged to journal.
- **60 minutes**: Hermes runs full reflection: attributes recent P&L and decision quality against the daily/weekly goals, detects regimes, produces gated wiki proposals for weight/goal adjustments.

The hourly reflection replaces the previous more frequent internal tick for deeper, less noisy self-improvement. The 5-minute layer is the primary consumer of the strategy brain delivered in the transfer increment. All changes remain paper-only and journaled.

This section + the linked goals page added wiki-first (log entry first, then this cross-reference). See updated `docs/project-plan.md` for how the cadence maps to the 3.x sub-phases.

*All per AGENTS.md (wiki first, journaled, paper gate, Decimal, heavy comments, decisions for choices). Avoided anti-patterns (accurate timeline in log + this amend, no silent paths in descriptions, fidelity to git via search_replace prepend first).*
