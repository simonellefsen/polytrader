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

## ⚠️ The real open question: does the strategy have any edge? (added 2026-06-29)

After the full Phase 0–3 harness + Tier 2/4 tooling, the **underlying strategy is not demonstrating
durable edge**, and this — not tooling — is now the central question:

- **Genuine track record: +$1.21 realized over 16 settled markets, 5W/11L (31% hit-rate).** Thin and
  weak. (The displayed +$5.41 includes ~$4.20 of frozen phantom from the 2026-06-24 resolved-market
  re-trade incident — to be cleared by a paper reset.)
- **Unrealized marks mean-revert to cost.** On 2026-06-26 unrealized was +$9.39; by 2026-06-29 it had
  bled to **+$0.03** with zero settlements — the apparent gains were noise, not captured edge.
- **Phase 2 sweep already showed we're at a thin local optimum** — the gate threshold doesn't move P&L,
  Hermes's weights are ~neutral-optimal, and `orderbook_momentum` (a 93%-fire, ~0.05-score baseline
  signal) is load-bearing. There's no obvious lever left to pull within the current signal set.
- **The book is structurally long-dated** (most positions resolve Q4-2026 → 2028-11; only ~2 resolve at
  any near-term date), so live confirmation of edge will take months regardless.

**✅ INVESTIGATION DONE (2026-06-29) — verdict: no positive directional edge; it is NEGATIVE on the only
clean data.** Decomposed the 16 clean pre-incident settlements (12 markets, all Iran-ceasefire cluster):
- **Directional single-side bets: −$77.00 (1W / 7L).** The actual signal-driven decisions LOST money.
  The losers are almost entirely `overreaction_fade` buying "No" (fading the Iran peace/ceasefire/nuclear
  markets) — which then resolved **Yes** (a real June-2026 catalyst). The fade bet against a genuine
  event and was wrong 7 of 8 times. This is exactly [[feedback-overreaction-naive-on-correct-extremes]].
- **Both-sides quasi-arb: +$78.21 (4 markets).** The ONLY source of profit was accidentally holding
  Yes+No on markets where the combined price was <$1 — a structural quasi-arbitrage, NOT signal edge.
  **It no longer happens** (disabled by the one-position-per-market guard), so going forward the strategy
  is purely directional → expected to lose on this evidence.
- **Net +$1.21** = the quasi-arb (+78) barely covering the directional losses (−77). The headline "profit"
  was an artifact of a now-removed quirk.

**Caveats:** tiny (12 markets) and entirely one correlated event (Iran ceasefire), so not statistically
conclusive — but there is *zero* evidence of positive directional edge and clear evidence of loss.

**Implications for direction (these matter more than any remaining knob):**
1. **`overreaction_fade` is the prime suspect** — it drove the directional losses by fading real
   catalysts. Either gate it much harder (only fade with strong volatility/no-catalyst evidence) or
   retire it. Re-examine whether the other directional signals add anything once it's removed.
2. **The accidental profit came from arb-like structure, not signals.** The one real money-maker was a
   sub-$1 both-sides position. That points toward **leaning into actual arbitrage** (the YES+NO scanner)
   over directional prediction — possibly the only place edge has shown up at all.
3. **A different market class may be required** (e.g. the sports markets we currently only arb), since
   the geopolitics-directional thesis is unproven-to-negative. This is the honest, important conclusion:
   the tooling is excellent; the directional alpha is not there.

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
- **Theta / convergence signal. ✅ DONE (2026-06-24, commit 2486eae).** New 6th FusionEngine processor
  `theta_convergence`: near resolution, lean toward the side the market already favors, scaled by
  lean × time-urgency (`score = (mid−0.5) × (HORIZON−days)/HORIZON × GAIN`, HORIZON 14d, GAIN 0.5).
  In the "buy the target outcome" frame (target = cheaper side), it's usually NEGATIVE on near-expiry
  underdogs — a brake on `overreaction_fade`'s longshot buys — and positive on a favored target.
  Dormant far out / on coin-flips / without an end date. Low confidence (≤0.45); Hermes learns its
  weight. **Plumbing:** the gamma ingester now captures `endDate` into `raw_json` (and the upsert
  refreshes raw_json — it didn't before, so existing rows would never gain it); the DR generator
  computes `days_to_resolution` into the snapshot; added to all attribution/scorecard/health signal
  lists (now 6). **Verified live:** the June-30 cluster fires correctly, e.g. mid 0.023 / 5.3d →
  score −0.148 (underdog_converges_down_avoid_target); far-horizon markets stay neutral.
  - **Follow-up: either-side generator. ✅ BUILT, opt-in (2026-06-24, commit ac00f9c).** The 5-min DR
    generator can now evaluate BOTH outcomes and target the higher-net-edge side
    (`POLYTRADER_DR_EVAL_BOTH_SIDES=on`; **default OFF** = unchanged cheaper-side behavior, verified
    inert post-deploy). This unlocks theta's positive "buy the converging favorite" case and lets the
    book act on the calibration finding (high-conviction bets underpriced) — but only once enabled, a
    paper-behavior change left to the operator. Same evaluation/fusion math both ways; external (metered
    news) fetched once per market and shared across sides. To realize the value: set the flag and let
    Hermes attribute/learn the favorite-side trades.
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

- **Reset-boundary awareness for settlements — DONE 2026-07-03.** A `POST /paper/reset` zeroes the
  portfolio snapshot (writes a `manual_paper_reset` snapshot) but PRESERVES the journal, so any code that
  summed ALL `paper_position_settled` events reconciled against a stale, pre-reset baseline. Two sites
  carried this bug: (1) the **backtest fidelity anchor** (`load_settlements`) read a false MISMATCH
  (+$5.41 recomputed vs 0 live); (2) the **`/trades` dashboard** (`trades_data_handler` settlements card +
  per-signal realized hit-rate) reported **37 settled / 26 wins / +$5.41** — 22 of them the 06-24
  money-pump phantom (net −$14.06) — against a true post-reset realized of 0. Fix: all three settlement
  queries now filter `created_at >= COALESCE((SELECT max(as_of) FROM …snapshots WHERE snapshot_reason =
  'manual_paper_reset'), '-infinity')`. Verified live: panel now shows **0/0/$0** (reconciles with
  portfolio realized 0) and the backtest **ANCHOR: PASS** (0 == 0). Self-limiting for future resets.
  - **THIRD site of the same bug, found 2026-07-03 ~1h after the arb-activation reset (image
    local-1783112138):** the CASH recompute in `write_mark_to_market_snapshot` (main.rs) and the
    `post_fill_tx` snapshot (paper/engine.rs) both computed `virtual_usdc = 10000 − locked − SUM(all
    fills' fees) + realized`. `SUM(fee)` was LIFETIME, but a reset preserves fills for audit — so the
    first mark after a reset re-subtracted the pre-reset fee total from the fresh $10k seed, silently
    clawing the balance back to the pre-reset EQUITY. Observed: reset wrote $10,000 at 19:41, the 19:45
    mark overwrote it with **$9,953.20 = exactly pre-reset cash $9,818.01 + locked $135.19**. (This also
    means the 06-29 reset's "$9,947 clean baseline" was never really $10k — same bug.) Fix: both fee sums
    now carry the `manual_paper_reset` boundary filter. Self-corrected live to **$10,000** on the next
    mark (0 post-reset fills ⇒ fees 0). Delta-based cash path (engine.rs ~L576) was already reset-safe.

- **Arb activation — DONE 2026-07-03 (image local-1783107611).** Investigated the "arb threshold/margin"
  lever and PROVED it a dead end: over 1,883 scans/7d the market was efficient (~$1.001) in 1,882; the
  cost distribution is BIMODAL (either ~$1.001 or a rare genuine dislocation), so there is NO continuum of
  near-misses below $1 for a lower `MIN_NET_PROFIT` to catch — and trading the $1.001 near-misses is
  buying a guaranteed loss. Left `MIN_NET_PROFIT` at 0.2%. The REAL findings + fixes:
  1. **Missed-arb bug:** real arbs DO appear (~1/wk, gross 2–3%; e.g. a 0.968 book on 616902/Fed on 07-01)
     but only ONE ever executed (06-19). Cause: the no-pyramiding guard skipped any market where we held
     ANY shares — and the 8 legacy directional holds (from the 06-29 pre-routing window) sat in exactly the
     arb-eligible markets, sterilizing them. Fix (main.rs `execute_arb_opportunity`): skip only if we hold
     BOTH legs (an existing arb pair); a directional single-side hold no longer blocks a risk-free arb.
  2. **Frequency lever = breadth, not threshold.** New `POLYTRADER_ARB_DISCOVERY_LIMIT` (=150 in manifest):
     each ingest tick now ALSO pulls the top-N active binary order-book markets by 24h volume
     (`GammaClient::discover_arb_markets`, filtered to `enableOrderBook` + 2 outcomes), deduped with the
     bootstrap slugs AND every held-position market (settlement-tracking guarantee — a held market that
     rotates out of top-N is still re-ingested). Live: scannable universe **32 → 100 active markets**, 0
     CLOB-error flood (enableOrderBook filter), scanner query still 0.58ms. NOTE: **Gamma caps a page at
     100**, so limit=150 is effectively 100 — going wider needs offset pagination (the next breadth lever).
  3. **Freshness bound:** the scanner LATERAL now requires a book `fetched_at > now()-30min`, so a market
     that rotates out of the universe isn't arb'd on a stale phantom book (matters with 100 rotating mkts).
  4. **Clean reset:** cleared the 8 legacy directional holds (−$9 unrealized, net-negative strategy) → fresh
     $10k arb-only baseline; those markets are now free for arb. Fills/orders preserved for audit.
  **Honest expectation:** even at 100 markets, arb is modest — dislocations are small (that 0.968 was ~$0.80
  on $48 depth, economy fee ate half) and rare. ~3× the books ≈ ~3× the shots (~a few/wk). Next breadth
  lever if wanted: paginate discovery past Gamma's 100-cap. Directional stays retired (arb-only routing).
  5. **Directional-routing gap closed 2026-07-04 (image local-1783137796).** Overnight, the directional
     executor evaluated (and risk-gate-REJECTED, 5×, no fills) a DISCOVERY market — 2645374 "Will Ayo
     Dosunmu play for the Toronto Raptors in 2026-27?", an NBA market whose slug dodged `arb_category`'s
     "sports" keywords, so `is_arb_only_market` returned false → directional-eligible. Widening the universe
     had unintentionally widened the DIRECTIONAL surface too (only the gate stopped a net-negative fill).
     Robust fix (no keyword whack-a-mole): `maybe_execute_opportunity` now also skips any market whose slug
     is NOT in `POLYTRADER_BOOTSTRAP_MARKETS` — the curated set is the only directional-eligible universe;
     every discovery market is arb-only by definition. Baseline held clean ($10k, 0 positions) throughout.

Drawdown circuit-breaker (auto-pause execution on equity drop), push-alerts for anomalies currently
caught by hand (WAL archiving flip, LLM health, signal drift), calibration dashboard.

- **Drawdown circuit-breaker. ✅ DONE (2026-06-24, commits bda857a + ed6142a).** Two halves:
  - *Observability* (bda857a): Hermes reflection carries a `drawdown` block (current NAV, all-time peak,
    current & max drawdown %) and journals a rate-limited `drawdown_alert` when NAV falls >
    `HERMES_DRAWDOWN_ALERT_PCT` (default 10%) from peak. Live: NAV ~9966, peak ~10056, max drawdown
    1.01% → quiet.
  - *Behavior, opt-in* (ed6142a): the directional executor halts NEW entries while NAV is >=
    `POLYTRADER_DRAWDOWN_HALT_PCT` below peak. **Default OFF** (env unset → disabled, zero overhead,
    behavior unchanged — verified inert post-deploy). Decisions baked as documented defaults: halt new
    entries only (no liquidation); risk-free arb executor unaffected; **auto-resumes** when drawdown
    recovers (no persisted latch / manual reset); halt journaled (de-spammed once/hour). NAV =
    virtual_usdc + total_locked + unrealized_pnl (matches /trades + the monitor).
  - **To enable:** set `POLYTRADER_DRAWDOWN_HALT_PCT` (e.g. 15). **Possible follow-ups if desired:** a
    manual-reset latch (don't auto-resume until an operator clears it), or extending the halt to the arb
    path. Left as defaults pending operator preference.

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
  monitoring.
- **2026-06-30/07-01 — ops diagnostics from the periodic checks.** (1) *CLOB fetch flood fixed* (commit
  bf2db16): ~352 "CLOB orderbook fetch failed" WARNs/hr were the ingester fetching dead books for the 16
  closed/resolved markets (of 50 tracked); skip book-fetch when `m.closed` → 0. (2) *Slow-statement WARNs
  root-caused + fixed* (commit 9b64f35): the chronic ~1.3s/183-per-hour query was NOT the 7d
  signal-health aggregate (that's an index scan, ~350ms warm; its earlier WARNs were cold-cache after
  restarts) — it was the /board's "latest decision_report/news per market", a DISTINCT ON over all ~92k
  decision_reports with a 41MB external-merge disk sort. Fixed with `idx_events_type_market_created`
  (event_type, (payload->>'market_id'), created_at DESC) + rewriting to a markets-driven LATERAL LIMIT-1
  (~1.3s → 0.5ms; WARNs 183/hr → ~0). *Correction to the note above: DISTINCT ON couldn't use a plain
  index (reads all rows to dedup); the LATERAL rewrite is what unlocks per-market index seeks.* (3)
  *Remaining benign:* ~33/hr "bootstrap slug delisted" (stale watchlist — resolved Iran slugs gone from
  Gamma; harmless), the 8 redeploy-artifact directional positions (long-dated, ~−$5 drift).
- **2026-07-01 (2nd check) — two more slow queries fixed** (commit 39acc06). (4) *Arb scanner ~17s PER
  CYCLE* — the core query of the arb-only strategy: `orderbook_snapshots` (316k rows / 850MB) was only
  indexed on `token_id`, but the arb scanner / `fetch_latest_book` / `recent_move` all look up the latest
  book by (market_id, outcome, fetched_at DESC) → ~100 parallel-sort-scans per scan. New
  `idx_obs_market_outcome_fetched` → 17s → **1ms** (also un-blocks the 5-min loop the 17s scan was
  stalling). (5) *7d signal-health aggregate ~1.6s, polled every 15s* — reads ~21k decision_report
  payloads (no index helps); wrapped in a 5-min process cache (`health_7d_baseline_cache`) → ~95% fewer
  runs. (I under-measured this one earlier as "fine warm" in an isolated session — it's genuinely slow
  under prod load.) **Remaining follow-ups:** *(b)* a flaky CLOB test
  (`place_limit_order_bails_early_on_non_limit`) fails only under parallel `cargo test`, passes single-
  threaded/in isolation. *(c)* stale failed `postgres-backup-retention` Job records (~13d old, pre-WAL-fix)
  clutter `kubectl get` — cosmetic, backups are healthy.
- **2026-07-01 — DB retention / GC DONE** (commits 2808d0e + 87328fe). The DB was ~1.13GB growing
  ~60MB/day, unbounded. Root cause: two fat append-only tables (orderbook_snapshots 863MB, decision_report
  payloads 162MB) whose *useful* signal is thin. New daily in-app GC (`src/gc`, spawned from main ~2min
  after boot then 24h) with **hot/warm/cold tiering + rollups**: (1) orderbook mids >48h → hourly
  `market_data.price_history`, prune raw keeping latest-per-(market,outcome); (2) decision_reports >30d →
  per-day per-signal `journal.signal_daily`, prune raw; (3) telemetry (llm_health/real_account_balance)
  >14d dropped; (4) portfolio equity snapshots >7d downsampled to 1/hour (event-markers always kept). All
  deletes batched (10k); rollups idempotent; journals a `gc_run` event. **Result: first pass deleted
  274k stale snapshots + 2.7k old marks; VACUUM FULL → 1.13GB → 372MB (−67%), and now BOUNDED
  steady-state** (orderbook ~140MB @48h + events @30d; autovacuum reclaims daily deletes, no recurring
  VACUUM FULL). price_history (3MB) + signal_daily preserve the price/signal history compactly.
  - **2026-07-02 revised plateau estimate:** the original "~370MB" undercounted `journal.events` — at
    observed ~5,200 `decision_report`/day × ~1.7KB, decision_reports *alone* trend to ~270MB at the 30d
    window (172MB at 19 days on 07-02), so the real steady-state is **~450–480MB**. Still bounded and
    self-limiting: oldest report is 2026-06-13, so the 30d prune+rollup first fires ~2026-07-13, after
    which events plateaus. **APPLIED 2026-07-02: dropped `REPORT_RAW_DAYS` 30→14** to bring the plateau
    down to ~300MB (halves the report table to ~130MB). The daily `signal_daily` rollup still preserves
    the per-day fire-count history; the only tradeoff is the backtest harness now has ~14d (not 30d) of
    raw attribution to replay. First deploy's GC pass will prune the 14–19d-old reports (rolling them to
    `signal_daily` first), reclaimed by autovacuum over the following day.
  - **2026-07-02 — latent rollup-permission bug found & fixed while applying the above.** The 30→14 change
    triggered the *first-ever* report prune (prior passes had `reports_deleted: 0`), which exposed that
    **both rollup tables (`market_data.price_history`, `journal.signal_daily`) were owned by `postgres`,
    not the app role `polytrader`** — created out-of-band as superuser during the retention build. So the
    app (connecting as `polytrader`) got `permission denied` on every rollup INSERT: the rollups had been
    silently failing since 07-01 while the prunes (on `polytrader`-owned `events`/`orderbook_snapshots`)
    succeeded. Net damage small — only today's June 13–17 report batch was pruned before being summarized
    (5 days of fire-counts, low value). Fix: `ALTER TABLE … OWNER TO polytrader` on both (now consistent
    with every other app table; `has_table_privilege` INSERT = t/t), and backfilled `signal_daily` for the
    current boundary. Won't recur: ownership is persistent, and a fresh cluster runs sqlx migrations as
    `polytrader` so the tables are owned correctly there.
- **2026-06-24** — **Calibration scorecard DONE** (commit bd77832, Tier 3). Brier + reliability buckets
  on entry `win_prob_estimate` vs outcomes, in Hermes reflection metrics. First live read: skill +0.28,
  model underconfident on high-conviction bets. Pure `compute_calibration` unit-tested; join is
  entry-report anchored (reuses the Tier 1.3 basis).
- **2026-06-24** — **Drawdown monitor + alert DONE** (Tier 4, observability half, commit bda857a).
  `drawdown` block in reflection metrics + rate-limited `drawdown_alert` on NAV fall from peak (threshold
  via HERMES_DRAWDOWN_ALERT_PCT, default 10%). Live max drawdown 1.01% → quiet.
- **2026-06-24** — **Either-side DR generator BUILT, opt-in** (commit ac00f9c). The generator can now
  target the higher-net-edge side instead of always the cheaper one (POLYTRADER_DR_EVAL_BOTH_SIDES,
  default OFF, ships inert). Unlocks theta's favorite case + the calibration high-conviction edge when
  enabled. The cheaper-side skeleton choice that had been flagged "arbitrary for limited wiring" since
  the 5-min DR generator landed is now addressed (behind a flag).
- **2026-06-24** — **Theta/convergence signal DONE** (Tier 2, commit 2486eae). First new FusionEngine
  processor since the external signals: a near-resolution convergence tilt. Required plumbing the gamma
  `endDate` through the ingester (+ fixing the upsert to refresh raw_json) into a `days_to_resolution`
  snapshot field. Verified firing live on the June-30 cluster; dormant elsewhere. Hermes will now
  attribute/weight it as the 6th signal. Note: theta's positive "buy the favorite" case is gated out by
  the cheaper-side target selection — a fuller either-side generator is the unlock (ties to the
  calibration finding that high-conviction bets are underpriced).
- **2026-06-24** — **Drawdown circuit-breaker DONE** (Tier 4, behavior half, commit ed6142a). Opt-in
  executor halt on NAV drawdown via POLYTRADER_DRAWDOWN_HALT_PCT, **default OFF** (ships inert; verified
  disabled post-deploy). Halts new directional entries only, auto-resumes on recovery, arb path
  unaffected. Follows the gated-autonomous-feature pattern so no behavior change ships until enabled.
  Tier 4 ops items (signal-health, LLM-health, drawdown) are now complete; remaining roadmap work is
  Tier 2 structural signals.
- **2026-06-29 — strategy pivot acted on the edge verdict** (commits 3d42474, 5477051):
  (1) **Retired `overreaction_fade`** (drove the directional losses by fading the real Iran ceasefire);
  unwired from the engine, dropped from the scorecard (now 5 signals).
  (2) **Expanded arb-only** from sports to a broad `arb_category` classifier (crypto/esports/finance/
  economy/tech/geopolitics/elections/culture/weather/sports) → directional executor skips ~all markets.
  **Nuance:** the arb scanner already scans ALL markets, so this only stops the net-negative directional
  engine — it doesn't widen arb reach.
  (3) **Investigated the arb threshold/margin** — the binding constraint is **not** the threshold.
  Over 3,569 scans only **5 (0.14%) had a sub-$1 book** (efficient market; avg best total cost $1.0009);
  the real arbs that appeared ($0.90 on 06-19 w/ $270 depth, ~$27 risk-free profit; $0.98 on 06-24)
  cleared the threshold easily, while the two thin near-misses ($0.997, $0.999) fail on *gross* margin
  too small to beat fees (lowering the threshold wouldn't help). The actual throttle was the
  **$20 arb notional cap** (shared with the directional executor) — it captured ~$2 of that $27.
  **Fixed (commit 5477051):** arb is risk-free on price, so the cap is now $250 (env
  `POLYTRADER_ARB_NOTIONAL_CAP`), bounded by depth. **Honest conclusion:** arb on this efficient, slow
  market set is RARE + execution-limited (only 1 arb ever filled in 10 days; snapshot legs can mis-fill)
  — this captures the edge that *exists* but is **not a new profit engine**. The deeper lever for real
  arb is execution realism (simultaneous WebSocket fills), a large investment likely not worth it given
  scarcity. **Net strategic state: neither directional nor top-of-book arb shows capturable edge on the
  current geopolitics-heavy universe; a different market class remains the open structural question.**
- **2026-06-29 — corrected the fee model to Polymarket's real per-category schedule** (commit 367c957,
  prompted by the operator finding docs.polymarket.com/trading/fees). The codebase used a flat 50bps of
  notional; the real fee is `shares × feeRate × p × (1−p)` (peaks at p=0.5, zero at extremes, symmetric)
  with a **per-category rate, and Geopolitics is FEE-FREE** (the bulk of our book). New
  `polymarket_taker_fee[_rate]` (rates: geo 0, sports 0.03, crypto 0.07, finance/tech/politics 0.04,
  econ/culture/weather/other 0.05) unit-tested against the published 100-share tables. Wired into the
  **arb scanner** (per-leg real fee; MIN_NET_PROFIT lowered 0.5%→0.2% since zero-fee thin arbs are real
  risk-free profit) and the **paper engine** (per-fill, so executed arbs aren't mis-charged). Effect:
  thin sub-$1 geopolitics arbs the old assumption wrongly rejected now qualify, and the paper sim stops
  over-charging fee-free markets. **Minor follow-up:** the DR generator's `FeeContext` (flat 50bps) for
  directional net-edge is still flat — low priority since directional is now arb-only/muted.
