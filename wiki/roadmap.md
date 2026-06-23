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
- **Phase 2 — config sweep. ✅ DONE (2026-06-23, commit d94d8dc).** `polytrader backtest sweep` runs two
  grids (A: gate threshold 0.02..0.06; B: weight presets) and ranks by total P&L. Key addition:
  **mark-to-market** — realized alone can't rank configs (the ~14 resolved Iran markets are entered under
  *every* config since near-extreme prices clear every gate and Kelly pins at the $20 cap), so
  `SimResult.unrealized` marks still-open positions at each market's latest mid (binary complement via
  `build_marks`); config differences live there. `simulate_counterfactual` now borrows reports (one load,
  reused across configs). **Findings (relative only, not live-comparable):** (1) the **gate threshold
  barely matters** — total P&L is flat ~+93.2 across 0.02..0.06, so the live "strict beats lenient
  ~$29 vs ~$16" gap is an artifact of that comparison's subset methodology, not a real edge-level effect;
  (2) **`orderbook_momentum` is load-bearing** — de-weighting it to 0.25 craters total P&L (+93 → +73),
  while boosting any single signal above the ~neutral Hermes weights yields no gain. Caveat: `max_dd` is
  config-invariant (computed from the realized path); a marks-aware equity curve is future work. Run:
  `/app/polytrader backtest sweep`.
- **Phase 3 — realistic fills + fees. ✅ DONE (2026-06-23, commit a1b5ed3).** The single-run
  counterfactual re-prices its fill decisions against the real order book instead of `target_mid`.
  Two-phase: the at-mid pass records a `fill_log`; books are loaded lazily (only the ~50 entered
  markets, nearest snapshot ≤ decision time) and each fill is re-walked via `walk_asks_limit_buy` (a
  pure mirror of the production `match_against_book` buy/limit path) + a 50bps taker fee
  (`reprice_realistic`). Output shows `[fill@mid]` vs `[book-walk]` side by side. **Live result: 49/49
  fills had a book; realistic re-pricing took total P&L +$93.25 → +$74.01 (~21% haircut)** — the
  fill-at-mid optimism quantified, and the counterfactual is now a credible standalone backtest. The
  sweep stays at fill-at-mid (relative ranking); realistic pricing is on the single run for the
  absolute number. **Still NOT a live reproduction** (different strategy: no arb/both-sides legs, fixed
  weights) — the fidelity anchor remains the live check.
  - **Deferred / future:** true Level-2 signal replay (re-run the *whole* FusionEngine on reconstructed
    snapshots to test NEW signals) is only partially feasible — the book-based processors reconstruct
    from `orderbook_snapshots`, but the external `yahoo`/`news` snapshot block was fetched live and
    isn't stored, so a full 5-processor re-run can't be faithfully reconstructed. Also future: a
    marks-aware equity curve for per-config drawdown/Sharpe; realistic fills inside the sweep.

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
  `SimPortfolio` + fidelity anchor + counterfactual engine. Anchor confirmed **PASS** against live data
  (+$1.21 / 16 / 5W-11L).
- **2026-06-23** — **Phase 2 complete** (commit d94d8dc): config sweep + mark-to-market. Findings: gate
  threshold barely affects total P&L; `orderbook_momentum` is load-bearing (de-weighting it craters
  return), Hermes's ~neutral weights are near-optimal among presets.
- **2026-06-23** — **Phase 3 complete** (commit a1b5ed3): realistic book-walk fills + 50bps fees on the
  single run. Fill-at-mid was ~21% optimistic (+$93.25 → +$74.01). **The backtest-harness roadmap
  thread (Tier 1.1) is now complete through Phase 3.** Remaining harness ideas are explicitly deferred
  (full Level-2 signal replay is only partly feasible — external signals aren't stored; marks-aware
  equity curve; realistic fills in the sweep). **Natural next focus:** back to the broader roadmap —
  Tier 1.2 (faster-resolving market universe, to fatten the settled sample) or Tier 2 (new signals:
  calibration / theta / cross-market), both of which the harness can now validate.
