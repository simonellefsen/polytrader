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
2. **Faster-resolving market universe. ❌ INVESTIGATED 2026-06-23 — structurally infeasible right now.**
   Gamma scan showed: fast-resolving markets (next ~16d) are almost all **decided extreme longshots**
   (Yes 0.0005–0.007 → uninformative No-resolutions); the only fast markets with **genuine uncertainty**
   are **sports** (FIFA World Cup, MLB), which polytrader trades **arb-only** (our signals don't model
   match outcomes); genuinely-uncertain DIRECTIONAL markets all resolve slowly (elections, World Cup
   winner). So the data starvation is *structural*, not a watchlist oversight — can't fatten the settled
   sample with the current strategy scope. Forks if more data is wanted: build sports-directional
   capability (new signal class, big), or accept slow data and do no-data-dependent work. **Do NOT stuff
   the bootstrap list with June-30 longshots.**
3. **Fix attribution causality. ✅ DONE (2026-06-24, commit 8944768).** `load_per_signal_realized_pnl`
   now attributes each settled position's realized P&L to its **entry decision report** — the latest
   `decision_report` at or before the first `autonomous_paper_execution`/`filled` that opened the
   position — instead of the old `ORDER BY created_at DESC LIMIT 20` sliding window. The old window
   took snapshots from days *after* entry and re-split the same P&L as new reports arrived (the
   +$0.74→−$2.53 swing source). The entry report is frozen once the position exists, so attribution is
   causally correct **and** stable. Falls back to the market's earliest report if no entry fill is
   journaled (legacy positions), still time-anchored to open — never the sliding snapshot. **Validated:**
   pre-deploy the entry-report scores differed materially from the latest-report scores the old code
   used (sign flips, e.g. mkt 2262261 +0.40 entry vs −0.429 latest; signals that fired at entry but read
   0 now, e.g. mkt 2508398 0.455→0); post-deploy two consecutive reflection cycles (04:01, 04:11)
   produced **identical** per-signal attribution with no new settlements — swing noise gone. Pairs with
   the recency-of-activity weight discount (commit 93268ff, 2026-06-24): that damps a stale boost from
   the *firing* side, this removes re-split noise from the *attribution* side.
   - **Follow-up (not done):** the airtight version journals the triggering report's id with each fill
     in the execution path so attribution links by id, not timestamp. Temporal anchoring is robust here
     (entry reports precede fills by ms; works retroactively on existing data) but an explicit link
     would remove any reliance on clock ordering for future positions.

## Tier 2 — Signal quality (thin and momentum-dominated)

- **Calibration signal** — per category, track historical implied-prob vs actual resolution; exploit
  structural over/under-pricing of "X-by-date" markets. A structural edge, not a momentum scrape.
- **Theta / convergence signal** — near resolution, price should converge to 0/1; flag laggards.
- **Cross-market correlation** — related markets drifting out of line (extends the arb scanner from
  exact to *statistical* arb).
- **Automated signal-health monitor. ✅ DONE (2026-06-23, commit 34b0a47).** The `/trades` scorecard now
  carries a recent-3h fire-rate alongside the 24h baseline and a pure `signal_health` classifier
  (degraded = fire-rate >½ drop, dormant = went silent, elevated = doubled/woke up, insufficient_data,
  else ok), shown as a colored badge. Automates the manual eyeballing that caught the news
  19.9%→4.5% drop; only alarms on drops from an active (≥5%) baseline so dormant-by-design signals
  aren't false-flagged. ~~**Limitation:** 3h-vs-24h catches *sudden* shifts, not multi-day gradual
  decay.~~ **✅ Limitation fixed 2026-06-24** — see the 7-day baseline + push alert under Tier 4 below
  (commits 5c61e7d, 5577ada).

## Tier 3 — Fusion, risk & validation

- **Flip live to the strict gate. ❌ DROPPED 2026-06-24 — not a real edge (invalidated by Phase 2
  backtest).** Originally proposed as the "cheapest standalone win" on a recurring live ~$29 vs ~$16
  gap, but the sweep (Phase 2, commit d94d8dc) showed total P&L is **flat ~+93.2 across gate
  thresholds 0.02..0.06** — the live gap was an artifact of that comparison's subset methodology, not
  an edge-level effect (see Phase 2 findings below). Don't spend effort flipping the gate. (An
  *adaptive* gate could still be explored later, but not motivated by the strict-vs-lenient gap.)
- **Regime-conditional / per-category weights** — different weights in calm vs volatile, or
  news-heavy for geopolitics vs momentum for sports. Needs Tier 1 data first.
- **Generalize the shadow framework** — run N parallel shadow configs so any proposed change is
  validated as a shadow strategy before promotion.
- **Calibration scorecard. ✅ DONE (2026-06-24, commit bd77832).** Brier score + reliability curve on the
  model's entry `win_prob_estimate` vs actual settled outcomes, in Hermes reflection metrics
  (`calibration`). Entry-report anchored (same basis as P&L attribution); reports Brier, the climatology
  reference + Brier **skill** score, and 5 reliability buckets. **First live read (12 settled): Brier
  0.176 vs 0.243 ref → skill +0.28** (beats base-rate), but the buckets show the model is mildly
  **overconfident on low-conviction bets** (predicted ~0.35, won 0.25) and **underconfident on
  high-conviction ones** (predicted ~0.66, won 1.00, n=3). Thin sample, caveated; auto-sharpens as
  markets resolve. Potential future use: a confidence-recalibration map, or sizing more aggressively on
  high-conviction signals once the high-end underconfidence holds up on more data.

## Tier 4 — Ops polish

Drawdown circuit-breaker (auto-pause execution on equity drop), push-alerts for anomalies currently
caught by hand (WAL archiving flip, LLM health, signal drift), calibration dashboard.

- **Drawdown monitor + alert. ✅ DONE (2026-06-24, commit bda857a) — observability half.** Hermes
  reflection now carries a `drawdown` block (current NAV, all-time peak, current & max drawdown %) and
  journals a rate-limited `drawdown_alert` when NAV falls > `HERMES_DRAWDOWN_ALERT_PCT` (default 10%)
  from peak. NAV = virtual_usdc + total_locked + unrealized_pnl (matches /trades). Live: NAV ~9966,
  peak ~10056, max drawdown 1.01% → no alert (book stable). **The auto-pause CIRCUIT-BREAKER (halting
  execution on breach) is deliberately NOT built** — it's a trading-behavior change that needs an
  explicit operator decision: (a) drawdown threshold, (b) pause-vs-alert-only, (c) auto-resume vs manual
  reset, (d) gating env (default off, like the other autonomous features). The monitor + alert is the
  safe precursor and gives the trigger signal the breaker would consume.

- **Signal-health monitor — longer baseline window. ✅ DONE (2026-06-24, commits 5c61e7d + 5577ada).**
  The 3h-vs-24h comparison was blind to *multi-day gradual decay* (the 24h baseline erodes along with
  the signal — what masked `news_sentiment`'s ~20%→~1.8% slide, reading `ok`). Now: (1) the `/trades`
  scorecard adds a `health_7d` classification comparing the 24h fire-rate to a **7-day baseline**
  (commit 5c61e7d), surfaced as a second badge; the baseline is a **slim count-only SQL aggregate**
  (`count(*) FILTER` per signal, cast-free `~ '[1-9]'` zero-check, no payloads loaded — validated
  instant over ~39k reports). (2) Hermes's reflection loop **pushes** it (commit 5577ada): a
  `signal_health` block in reflection metrics + a rate-limited (once/6h per signal+status)
  `signal_health_alert` event journaled whenever a signal degrades/goes dormant from an active weekly
  baseline. Dormant-by-design signals (quiet both windows) stay `ok` (no false alarm). Verified live:
  all signals currently `ok`, `alerts_journaled: 0`.

- **Push-alerts for hand-caught anomalies — LLM health. ✅ DONE (2026-06-24, commit 472d2e9).** Extended
  the same push pattern to LLM/AI health: `journal_llm_health` already wrote a routine `llm_health` event
  every cycle (mostly "ok" noise); it now also PUSHES a rate-limited (once/1h per status+cause)
  `llm_health_alert` when the model is disabled/failing (out-of-credits, auth, rate-limit). No trading
  effect (Hermes falls back to local synthesis) but AI reflections/wiki proposals pause until restored.
  Refactored the rate-limited-journal logic into a shared `maybe_journal_alert` helper used by both the
  signal-health and LLM-health alerts.
  - **WAL-archiving flip — deliberately NOT in Hermes.** Investigated 2026-06-24: `pg_stat_archiver` is
    **per-instance and misleading on replicas** — the replica `polytrader-postgres-1` showed 45,576
    failures / last archive 2026-06-17 (frozen stats from when it was previously primary), while the
    actual **primary `polytrader-postgres-2` archived healthily seconds before the check**. A naive
    Hermes check would false-alarm on whichever instance its pool hit. WAL-archiving health belongs in
    **primary-aware CNPG cluster monitoring**, not the trading meta-agent. No real issue found.
  - **Follow-up (optional):** drawdown circuit-breaker (Tier 4 lead line) is the remaining ops item; it
    touches the execution path (auto-pause), so it's a behavior change, not pure observability.

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
- **2026-06-24** — **Hermes weight-tuning hardening (two Tier-1-adjacent fixes).**
  (1) *Recency-of-activity discount* (commit 93268ff): `compute_weight_adjustments` scaled a signal's
  realized-P&L boost/trim by `min(1, recent_fire_rate / attribution_window_fire_rate)`, so a signal
  that earned credit while active but has since gone quiet drifts back toward neutral instead of staying
  over-trusted on stale evidence (e.g. `news_sentiment`). Ratio not absolute floor, so consistently-
  selective signals aren't penalized; doesn't regress 32e1edd (dormant-by-design signals are handled
  separately). (2) *Attribution causality* (commit 8944768, **Tier 1.3 done**): anchored per-signal
  realized P&L to the entry decision report instead of the sliding recent-20 window — kills the re-split
  swing noise; two consecutive post-deploy cycles produced identical attribution.
- **2026-06-24** — **Tier 3 strict-gate flip DROPPED.** Reconciled the "cheapest standalone win" bullet
  with the Phase 2 finding that gate threshold barely moves total P&L (~+93.2 flat across 0.02..0.06);
  the live strict-vs-lenient gap was a subset-methodology artifact, not an edge. Not worth flipping.
- **2026-06-24** — **Signal-health longer-baseline DONE** (commits 5c61e7d + 5577ada). Fixed the
  multi-day-decay blindspot: `/trades` scorecard gained a 24h-vs-7d `health_7d` badge (slim cast-free
  aggregate), and Hermes now pushes rate-limited `signal_health_alert` events from its reflection loop.
  Closes the Tier 4 signal-health follow-up and the Tier 2 monitor's noted limitation.
- **2026-06-24** — **LLM-health push-alert DONE** (commit 472d2e9) via a shared `maybe_journal_alert`
  helper. **WAL-archiving alerting deliberately NOT built into Hermes** — investigation showed
  `pg_stat_archiver` is per-instance and misleading on replicas (replica froze at 45k failures / last
  archive 06-17 while the primary archived healthily in real time); it belongs in primary-aware CNPG
  monitoring. Noted the 7d signal-health aggregate is a seq scan (warm ~420ms, cold ~2.67s); a partial
  index `events(created_at) WHERE event_type='decision_report'` would help but is a schema migration
  (deferred — warm perf is acceptable for the 10-min cycle / on-demand dashboard).
- **2026-06-24** — **Calibration scorecard DONE** (commit bd77832, Tier 3). Brier + reliability buckets
  on entry `win_prob_estimate` vs outcomes, in Hermes reflection metrics. First live read: skill +0.28,
  model underconfident on high-conviction bets. Pure `compute_calibration` unit-tested; join is
  entry-report anchored (reuses the Tier 1.3 basis).
- **2026-06-24** — **Drawdown monitor + alert DONE** (Tier 4, observability half). `drawdown` block in
  reflection metrics + rate-limited `drawdown_alert` on NAV fall from peak (threshold via
  HERMES_DRAWDOWN_ALERT_PCT, default 10%). Live max drawdown 1.01% → quiet. The auto-pause circuit-breaker
  is deliberately deferred — it changes trading behavior and needs an operator decision on
  threshold/pause-policy/auto-resume/gating.
