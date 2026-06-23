# Roadmap

Forward-looking improvement plan for polytrader, organized around the system's single biggest
constraint. Grounded in observed live behavior (status checks during the 2026-06-22/23 quiet period),
not generic trading-bot theory. Paper-only throughout; nothing here changes the real-order fail-closed
posture.

Cross-references: [index.md](index.md) · [architecture.md](architecture.md) ·
[strategies/multi-signal-fusion.md](strategies/multi-signal-fusion.md) ·
[concepts/hermes-self-improvement.md](concepts/hermes-self-improvement.md) ·
[schema.md](schema.md).

Created 2026-06-23. Owner: operator + coding agents (Hermes may propose updates).

---

## The core constraint: we are data-starved

Almost every observed weakness traces back to **~12–16 settled markets**. Concrete symptoms seen live:

- Hermes is tuning a 5-parameter weight vector on a sample too small to support stable inference.
- Per-signal realized P&L swung from **+$0.74 → −$2.53 between two status checks with *zero* new
  settlements** — the bounded recent-20-report attribution window re-splitting the same handful of
  settled markets as it slides.
- The Iran/Hormuz cluster resolves on geopolitical timescales (weeks; some stuck in UMA dispute), so
  settlement throughput is intrinsically slow.

**Until settlement throughput rises (or we learn offline from history), every downstream improvement is
rate-limited.** That framing drives the tier ordering below.

## Other standing observations (motivate specific items)

- `orderbook_momentum` fires **~93% of reports with avg |score| ≈ 0.065** — a weak, ubiquitous signal
  that effectively *is* the baseline. The system is heavily momentum-driven.
- Two of five signals (`spike_divergence`, `overreaction_fade`) are volatility-gated and **dormant in
  calm regimes** (0% fire). The other two external signals fire <15%. So most cycles run on ~1 thin
  signal.
- The **strict gate beats the lenient (live) gate every single check** (~$29 vs ~$16 total P&L on the
  same fills). The live line is running the worse of the two gates we already shadow-simulate.
- `news_sentiment` fire-rate is volatile (19.9% → 4.5% → 10.1% across checks) — caught by eye, should
  be an automated alert.

---

## Tier 1 — Unblock learning (highest leverage)

1. **Backtest / replay harness** *(chosen first thread — see plan below)*. Replay the journal of
   resolved markets offline to (a) bootstrap signal calibration from history and (b) validate any
   weight/gate/signal change before it touches the live line.
2. **Faster-resolving market universe.** Re-weight the tracked universe toward daily/weekly resolvers
   (sports finals, daily crypto closes, econ prints, weekly political markets) to take settlement
   throughput from ~12/week toward ~10/day — Hermes crosses `FULL_CONFIDENCE_SETTLED=40` in days, not
   never.
3. **Fix attribution causality.** Anchor per-signal realized P&L to the **decision report at entry**
   (the report that actually triggered the trade) instead of a sliding recent-20 window. Removes the
   re-split noise source and makes Hermes attribution stable + causally correct.

## Tier 2 — Signal quality (thin and momentum-dominated)

- **Calibration signal** — per category, track historical implied-prob vs actual resolution; exploit
  structural over/under-pricing of "X-by-date" markets. A structural edge, not a momentum scrape.
- **Theta / convergence signal** — near resolution, price should converge to 0/1; flag laggards.
- **Cross-market correlation** — related markets drifting out of line (extends the arb scanner from
  exact to *statistical* arb).
- **Automated signal-health monitor** — alert on fire-rate / score-distribution shift (automate the
  news-drop catch).

## Tier 3 — Fusion, risk & validation

- **Flip live to the strict gate (or make it adaptive).** Cheapest standalone win given the recurring
  ~$29 vs ~$16 gap; confirm via the harness before flipping.
- **Regime-conditional / per-category weights** — different weights in calm vs volatile, or
  news-heavy for geopolitics vs momentum for sports. Needs Tier 1 data first.
- **Generalize the shadow framework** — run N parallel shadow configs so any proposed change is
  validated as a shadow strategy before promotion.
- **Calibration scorecard** — Brier score / reliability curve on `win_prob_estimate` vs outcomes.

## Tier 4 — Ops polish

Drawdown circuit-breaker (auto-pause execution on equity drop), push-alerts for anomalies currently
caught by hand (WAL archiving flip, LLM health, signal drift), calibration dashboard.

---

## First thread: Backtest / Replay Harness — plan

### Why it's cheap
Every decision report journals each signal's `{score, confidence, edge}` — not just the fused output.
So testing a different **weight vector or gate threshold needs no re-fusion**; we re-weight the stored
scores. The expensive part of a backtest collapses.

### The one stateful wrinkle
The gates are **portfolio-dependent**: `check_pre_trade` reads live exposure, locked collateral, and
the PnL floor, all of which evolve as fills land ([../src/risk/mod.rs](../src/risk/mod.rs)). So the
harness can't re-score reports independently — it must be a **sequential simulator** that walks reports
in `created_at` order, maintains an in-memory portfolio, and settles each position at its market's
resolution time.

### Unlock: two behavior-preserving "pure core" refactors
Both live paths fuse DB access with math; splitting them is a pure win (also makes the live code
unit-testable) and is the prerequisite:

1. `FusionEngine::fuse` → `fuse_from_attribution(scores, weights, fee_ctx)` — a pure function the
   harness and live share, so replay math is identical to production *by construction*.
2. `check_pre_trade(pool, …)` → `gate(exposure: Exposure, net_edge, proposed)` — lift the pure gate
   logic out; live passes a DB-loaded `Exposure`, the harness passes a simulated one. `load_exposure`
   stays put.

### Phasing
- **Phase 0 — pure-core refactor. ✅ DONE (2026-06-23, commit 1062068).** Extracted
  `strategy::fuse_weighted` + `strategy::fuse_from_attribution` (live `fuse` delegates to the former;
  the latter is the harness replay primitive, clamps candidate weights like the live read path) and
  `risk::RiskManager::gate(market_id, net_edge, proposed, &PortfolioExposure)` (now public; gates 1–4
  lifted out of `check_pre_trade`, which just loads exposure + delegates). 10 new unit tests incl. an
  end-to-end check that `fuse_from_attribution` reproduces live `fuse()` exactly. Behavior-preserving:
  live portfolio unchanged post-deploy (realized +$1.21, 16 settled).
- **Phase 1 — sequential simulator + fidelity anchor. ✅ DONE (2026-06-23, commit 6cb7910).** Built as
  a read-only **`polytrader backtest` subcommand** (not a separate bin — reuses `risk`/`strategy`
  natively) in `src/backtest/mod.rs`. `SimPortfolio.settle` calls the production
  `settlement_payout_and_pnl` directly (identical by construction). **Fidelity anchor =
  `realized_from_settlements`**: recompute realized P&L from every journaled settlement via the
  production formula and compare to the live recorded realized (+$1.21 / 16 / 5W-11L). `replay_fills`
  reconstructs the equity path from actual fills; `simulate_counterfactual` is the analysis engine
  (fuse_from_attribution + gate + quarter-Kelly + fill-at-mid). 6 unit tests; polytrader suite 92.
  **Run it:** `make backtest` (read-only, inside the live pod), optionally
  `ARGS="--min-net-edge 0.04"` or `ARGS="--weights name=val,..."`. **Documented Phase-1
  approximations** (deferred to Phase 3): fills at `target_mid` not the walked book; arb legs excluded;
  the counterfactual applies a *fixed* weight vector across history (live weights varied per cycle).
  ✅ **Anchor run confirmed PASS (2026-06-23):** `make backtest` against live data reproduces 16
  settlements / 5W-11L / realized **+$1.21 == live +$1.21**, every per-record formula match. The
  accounting is validated — Phase 2 is unblocked. (`--since` is a counterfactual-only knob and does not
  affect the anchor.) First counterfactual under live config: 49 fills, 14 settled, realized +$56.65 —
  not comparable to live in absolute terms yet (fill-at-mid, no fees, no arb legs, fixed weights;
  Phase-3 fidelity), but the gate/Kelly/dedup/settlement all fire correctly.
- **Phase 2 — config sweep.** Grid over `{min_net_edge, weight vectors, caps}`, rank by realized P&L /
  drawdown / Sharpe. Quantitatively settles the strict-vs-lenient question and validates any Hermes
  weight vector before it goes live.
- **Phase 3 — Level-2 signal replay.** Reconstruct the snapshot (bids/asks/mid/`recent_move_signed`)
  from `market_data.orderbook_snapshots` at each decision time and re-run the *real* `FusionEngine` —
  enables testing **new/changed signals** offline. Unlocks all of Tier 2 cheaply.

### Risks to design around
- **Fidelity** — paper fills execute at mid; the sim's slippage/fill assumptions must match the live
  executor exactly. Fill log = regression oracle.
- **Look-ahead bias** — sim clock must strictly gate to `created_at <= now`; never use a future report
  or snapshot.
- **Snapshot completeness** (Phase 3 only) — orderbook history may be sparse for some markets; Level 1
  sidesteps this by using stored scores.
- **Resolution timing** — settle on the actual resolution timestamp so the equity curve is
  chronologically honest.

---

## Decision log

- **2026-06-23** — Roadmap drafted during the quiet period. Chosen first thread: **backtest/replay
  harness** (Tier 1.1), because it de-risks and accelerates every other tier by enabling offline
  validation.
- **2026-06-23** — **Phase 0 complete** (commit 1062068): pure fusion + risk-gate cores extracted with
  equivalence tests; behavior-preserving, deployed, live unchanged. Decided the harness will be a
  **subcommand of the `polytrader` binary** (one-shot CLI branching before the server starts), not a
  separate bin — so it reuses `risk`/`strategy` natively without a `lib.rs` extraction or the
  duplication the `hermes` bin suffers.
- **2026-06-23** — **Phase 1 complete** (commit 6cb7910): `polytrader backtest` subcommand +
  `SimPortfolio` + fidelity anchor + counterfactual engine, deployed, 6 unit tests. **Open follow-up:**
  run `make backtest` against live data to confirm the anchor PASSes (+$1.21 / 16 / 5W-11L) and capture
  the first counterfactual numbers — agent can't kubectl exec, so this is an operator step. **Next:
  Phase 2** (config sweep over `{min_net_edge, weight vectors}`, ranked by realized P&L / drawdown),
  which calls `simulate_counterfactual` — gated on the anchor passing first.
