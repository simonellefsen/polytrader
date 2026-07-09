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

## 📋 Open items / TODO (live backlog, most recent first)

Deferred follow-ups surfaced during diagnostic checks but not yet built. Each has a full writeup in
the dated Decision-log entry below; this is the at-a-glance index.

- [ ] **Spread-aware entry gating** (2026-07-08). 82% of post-reset losses are execution friction
  (fees $17 + slippage ~$33 vs gross ≈ −$11); the executor crosses whatever spread exists. Gate
  entries on estimated round-trip cost (spread + fee) vs net edge. *Complements the maker-exec TODO.*
  → *Expanded into the "Path to profitability" plan below (P1).*

## 🎯 Path to profitability (added 2026-07-08, operator-requested)

The post-reset ledger decomposes as: gross signal P&L ≈ **flat** (−$11/4d), execution friction
≈ **−$50** (82% of losses), negrisk arb ≈ **+$3.28 guaranteed** (the only consistently-positive
strategy, ~$0.80/day). So the path is: (a) stop paying friction on flat-edge trades, (b) scale the
one thing that provably works, (c) build the execution capability that unlocks the rest. Ranked by
certainty-of-benefit ÷ effort:

- [ ] **P1 — Friction-aware entry gate** (small; validate in the backtest harness first). The gate
  today compares net edge (entry fee only) to a static 2%. Change: estimated ROUND-TRIP cost =
  entry fee + exit fee + current spread + observed slippage-by-price-band, and require
  `net_edge ≥ k × round_trip_cost` (k≈1.5–2). Measured basis: fills below $0.20 paid **422bps
  average slippage** vs 101bps above $0.80 — the cost model must be price-band-aware, not flat.
- [x] **P2 — Entry price band → CORRECTED to "widen the stop-loss" (2026-07-09).** The harness-
  validation the plan called for OVERTURNED this item. Decomposing realized P&L by entry-price band
  showed the naive [0.15, 0.85] band was backwards: the >0.85 favorites were the ONLY profitable
  band (+$1.41) and 0.70–0.85 was the biggest loser (−$41.78), so the band would have cut winners
  and kept losers. Splitting by realization type revealed the true cause — see the "stop-loss
  widened" entry below. The price band is NOT implemented; the favorite-only edge (consistent with
  the calibration "high-conviction underpriced" finding) is logged as a future signal-quality lever,
  not acted on at n=3 settlements.
- [ ] **P3 — Scale negrisk: event-completion ingestion + fee-free priority** (small-medium). The
  scanner only sees legs already in the ingest universe, so baskets are PARTIAL (subset overround).
  When a scan finds a negrisk event with ≥3 visible legs and implied-Yes sum ≥ ~0.97, add ALL its
  member slugs to the must-track ingest set — full coverage captures the full overround instead of
  a slice. Prioritize FEE-FREE categories (geopolitics/politics ladders like "next PM"/nominee
  events) where the 0.2% net threshold is genuinely capturable — on 3%-fee sports events the
  measured 1.0–1.4% overrounds are structurally uncapturable, so today's scanner mostly stares at
  markets it can never trade.
- [ ] **P4 — Ladder monotonicity scanner** (medium; new strategy, needs payoff math first). Date
  ladders we ALREADY ingest via rotation ("GPT-5.6 by July 7/8/9/10", "WTI reach 80/85/90") obey
  hard constraints: P(by d₁) ≤ P(by d₂) for d₁<d₂ (and the WTI ladder is monotone in strike). A
  violation is a structural mispricing tradeable long-only (buy later-date Yes + earlier-date No;
  bounded, near-riskless payoff when the violation exceeds combined costs). Cheap to scan (same
  books), and low-liquidity ladder tails are exactly where retail mispricings persist. This is the
  same "structure beats prediction" thesis negrisk validated.
- [ ] **P5 — WebSocket CLOB feed** (large; the multi-unlock). One investment removes the three
  biggest ceilings at once: (a) maker execution — post resting orders: ZERO fees + 20–25% rebates
  (turns the $50 friction into a rebate income), (b) simultaneous multi-leg fills — makes in-play
  negrisk capturable, where the only RICH overrounds live (27% seen on Canada–Morocco), (c) honest
  fill simulation for everything above. Do after P1–P3 prove out; it's the step that would matter
  for any real-money future.
- [ ] **P6 — Turnover budget** (trivial). Cap autonomous directional entries per day (e.g. 6, best
  net-edge first). Friction scales linearly with trade count; a flat-gross book cannot outrun it —
  fewer, better trades is a direct P&L improvement at zero information cost.

Sequencing: P2+P6 are one-liners, P1 next (harness-validate the gate constant), P3 alongside (it
feeds the proven strategy), P4 when the payoff math is written down, P5 as the deliberate big bet.
Honest caveat: P1/P2/P6 mostly stop the bleeding (→ ~breakeven); the PROFIT upside lives in
P3/P4/P5 (structural trades) — consistent with the 06-29 verdict that structure, not directional
prediction, is where this system's edge has ever appeared.
- [ ] **Hermes image-freshness deploy guard** (2026-07-08). `hermes:local` sat stale from 06-24 to 07-08
  while every deploy re-tagged and "rolled out" the old image (behaviorally confirmed: live weights map
  still contained the 06-29-retired overreaction_fade). A post-deploy check comparing the image's
  Created date to the build time (or embedding a build-stamp the pod logs at boot) would make this
  impossible to miss. *The manual rebuild fixed it; the guard prevents recurrence.*
- [ ] **Signal-flip exit debounce** (2026-07-06). The re-entry cooldown stops flip OSCILLATION, but a
  single noisy DR can still trigger a flip exit; requiring the flip to persist for 2 consecutive DR
  cycles would cut false exits further. *Small; evaluate after a few days of cooldown data.*
- [ ] **event_id-based cluster key** (2026-07-05). Rotation ladder promotions (e.g. 6× `gpt-5pt6-released-by-july-N`)
  are one correlated underlying event, but `risk::cluster_key` drops them in the exempt "uncorrelated"
  bucket, so up to ~$120 (6 × $20 cap) can concentrate on one binary. Add an event_id-derived cluster
  key so the concentration cap groups a ladder as one bet. *Low priority at paper scale (~1.2% bankroll).*
- [ ] **Signal-calibration `(1−p)` ceiling clamp** (2026-07-05). `fuse_net` can report a large net edge on
  a ceiling-priced side (16% on a 0.9995 No); Kelly correctly zeroes it, but the phantom edge pollutes
  the scorecard. Clamp fused edge by the remaining price headroom in the DR generator. *Signal-quality, not urgent.*
- [ ] **Maker-execution simulation** (2026-07-04). Posting resting limit orders pays ZERO taker fee and
  earns the 20–25% maker rebate — a real lever on fee-enabled markets (sports/crypto). Needs resting-order
  queue + fill-probability modeling, which honest snapshot-based ingestion can't do. *Blocked on a WebSocket feed.*
- [ ] **Arb execution realism** (standing). Snapshot-based legs can mis-fill (one leg fills, the other
  moves) — fine in paper, but real-money arb needs simultaneous WebSocket fills. *Same WS prerequisite as maker sim.*

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

- **Standstill diagnosis + 4-track rebuild — 2026-07-04.** Operator review: "the algorithm is at a
  standstill." Root-caused it as idle BY CONSTRUCTION, three legs pinned at once: (a) single-market
  Yes+NO arb structurally dead (430 scans since the 100-market widening, best cost pinned $1.000–1.001,
  zero sub-dollar — breadth confirmed rather than fixed it); (b) the hand-curated bootstrap set had
  DECAYED — 29 of 50 slugs resolved (June Iran/Hormuz deadlines; even the Jul/Aug/Dec deadline variants
  resolved early with the June deal), and the 21 survivors are multi-month/-year horizons that never
  clear the 2% edge gate; (c) the learning loop starved — 0 settlements/24h AND no autonomous exit path
  existed (Sell was manual-API-only), so realized feedback was gated on months-out resolutions. Plus a
  DR-selection lottery: `ORDER BY updated_at DESC LIMIT 20` over a ~140-market universe that refreshes
  wholesale each tick meant the ~20 DR slots filled with arbitrary arb-only discovery markets, starving
  the directional-eligible set of decision reports at all. Fixes, all four tracks (one deploy):
  1. **Automated market rotation** (`src/rotation/`, `market_data.directional_universe`, migration
     20260704090000): 6h job promotes active/order-book/binary/NON-sports markets ending within
     `POLYTRADER_ROTATION_MAX_DAYS` (30) above `POLYTRADER_ROTATION_MIN_VOL24H` ($5k), cap
     `POLYTRADER_ROTATION_LIMIT` (20); demotes on close/resolution/expiry (insert-only — resolution is
     final). Directional eligibility = bootstrap env ∪ active rows; ingester must-track UNION keeps
     promoted markets ingested regardless of discovery rank; DR query now ranks directional-eligible
     markets FIRST (limit 20→40). Client-side end-date re-check guards against Gamma ignoring
     end_date_min/max. Journals `directional_rotation`.
  2. **Autonomous exits** (`src/exits/`, gated `POLYTRADER_AUTONOMOUS_EXITS=on`): 5-min evaluator closes
     positions at market on take-profit (+25%), stop-loss (−15%), time-stop (14d), or signal-flip
     (latest DR targets the opposite outcome at ≥ live gate). Skips closed/resolved (settlement owns
     those) and stale marks (>30min). Journals `autonomous_paper_exit` per round-trip for Hermes.
     **Required an engine fix:** the cash model was buy-only — a Sell's P&L would have evaporated
     (cash only got the cost basis back) and the buy-averaging formula corrupted avg_entry on partial
     sells (never triggered before: no-pyramiding meant positions always started flat). `submit_order`
     now keeps avg on sells, computes realized_delta = Σ(price−avg)×size, and carries it into the
     post_fill_tx snapshot's realized_pnl (mark-to-market then propagates it).
  3. **NegRisk EVENT-level arb** (`src/strategy/negrisk.rs`, migration 20260704100000 adds
     markets.event_id + neg_risk): at most one member of a negRisk event resolves Yes ⇒ buying No
     across k members pays ≥ k−1; arb whenever Σ(1−ask_No) > 1 (implied probs overround). Holds for ANY
     SUBSET of members, so partial book coverage still finds arbs — scans what the universe already
     ingests, no event-wide fetch needed. Per-leg real fees; MIN_MEMBERS=3 (2-member events = the binary
     scanner's job); journals `negrisk_arb_scan` + `autonomous_negrisk_arb_execution`; executor mirrors
     the two-leg guard (skip only when EVERY leg already holds No) + shares the $250 arb cap. This is
     the structural answer to "single-book arb is dead": keeping N books of one event mutually
     consistent is much harder than keeping one book efficient.
  4. **Hygiene:** gate-sim fill query reset-filtered (4th reset-boundary site — the panel showed 78
     pre-reset ghost fills/$9.4k notional against an empty portfolio); the go-live "proven" gate's
     settled COUNT reset-filtered (5th site — 37 pre-reset settlements could have satisfied
     MIN_SETTLED=10 against a post-reset track record that proved nothing); bootstrap env pruned
     50→29 slugs (dead ones only produced closed=true fallback churn every tick).
  **RESET-BOUNDARY RULE (now 5 occurrences):** any query that aggregates journal events, fills, or
  win/loss counts across time MUST filter `created_at >= (SELECT max(as_of) FROM
  virtual_portfolio_snapshots WHERE snapshot_reason='manual_paper_reset')`. Grep for new aggregate
  queries in review.
  **First-deploy leak + fix (same day, image local-1783169442):** the first rotation pass promoted 13
  SPORTS markets — Polymarket's scheduled-match slugs are league-PREFIXED (`wta-`, `atp-`, `cs2-`,
  `val-`, `lol-`) and `arb_category`'s substring keywords missed them all (plus "wimbledon"), so the
  Pegula market leaked a directional fill for the SECOND time ($18.69 No) and a WTA match filled
  $3.89 — which the new exit evaluator sold 2s later on signal-flip (first live proof of the Sell
  path + realized-P&L capture working end-to-end). Fixes: prefix-anchored `pre()` matcher in
  `arb_category` (substring would false-positive: "oval-office" contains "val-") + "wimbledon"/
  "tennis"/"grand-slam" keywords + regression tests on the leaked slugs; wrong rows DELETED from
  directional_universe; paper reset → clean $10k. Verified post-fix: rotation pass promoted 0 of 100
  candidates (top-volume short-dated pool is ~all sports matches — correctly excluded now; the 2
  legit promotions, musk-tweets + SBF-pardon, persist), next mark held exactly $10,000.00 (no
  claw-back — the reset-aware cash fixes hold), 0 stray fills. **NegRisk scanner first live data:
  8 events / ~77 member books per pass, best implied-Yes sum 0.990** — genuinely near the 1.00 arb
  line from below (vs the single-market scanner permanently pinned at $1.001 from the wrong side).
  LESSON for the classifier: Polymarket slug taxonomies are league-prefixed for scheduled games;
  any "non-sports" filter must anchor prefixes, not just substrings.

- **Taker/maker fee overhaul — DONE 2026-07-04 (operator asked to audit taker & maker algorithms).**
  Verified against docs.polymarket.com/trading/fees + maker-rebates + taker-rebates: taker fee =
  `shares × rate × p × (1−p)` per category (geo 0, sports 0.03, crypto 0.07, finance/politics/
  mentions/tech 0.04, econ/culture/weather/other 0.05); **MAKERS ARE NEVER CHARGED** (they earn
  20–25% fee-curve-weighted rebates; takers have a tiered rebate program — ignored as conservative,
  ~nil at Bronze/paper volumes). Audit results: engine fills + both arb scanners were already
  correct (per-market stored rate, real formula; every sim fill crosses the book so always-taker is
  right — sells through the exit path are taker fills too, same symmetric formula). THREE stale
  sites fixed:
  1. **`fuse_net` had a UNIT BUG + flat model:** it subtracted `notional×bps + gas − offset` in
     RAW USDC from the FRACTIONAL gross edge — on the $10 DR notional that over-penalized every
     decision report ~10× (≈5.1 points instead of ≈0.5) — and its flat 50bps ignored that
     geopolitics is FREE while crypto at low p costs rate×(1−p) ≈ up to 7% of notional. Now:
     `fee_frac = rate×(1−p) + gas/notional`, per-market rate + per-side price via the reworked
     `FeeContext {taker_fee_rate, price, est_gas_usdc}` (flat taker_bps/maker_bps/rewards_offset
     fields deleted). Verified live: sports p=0.515 → 0.01555; crypto p=0.9995 → 0.001035;
     geopolitics → 0.0010 (gas only). Attribution journals `fee_cost_frac` (what's subtracted) +
     `est_fees_and_gas` (USDC, audit); the backtest's `report_fee` prefers the new key and falls
     back to the old one so historical replays still subtract exactly what the live gate saw.
     **Effect on behavior: geopolitics/high-p edges were being under-reported ~4–5 points — some
     previously-rejected DRs will now correctly clear the 2% gate; low-p crypto entries now carry
     their true ~5–7% cost.**
  2. **Backtest Phase-3 realistic fills charged flat 0.5% of cost** — now per-level
     `polymarket_fee` with a per-market rate map (`load_fee_rates`: stored Gamma rate else category
     default — same resolution order as the live engine).
  3. **Dead flat-fee plumbing deleted:** `FeeModel` (models.rs, never called), the engine's unused
     `paper_fee_bps` ctor param/field, `POLYTRADER_PAPER_FEE_BPS` config knob, and server's
     `paper_fee_bps_from_env`. The server candidates endpoint also passed `target_mid` (a PRICE) as
     the costing notional — now $10 like the DR generator, with per-market FeeContext.
  Also added the **"mentions" category** (0.04; `-say-`/`of-tweets`/`tweet-count` slugs — the
  rotation-promoted musk-tweets market was falling to Other/0.05).
  **Maker-side disposition (roadmap item, not built):** posting resting limit orders instead of
  crossing would pay ZERO fee + earn the 20–25% maker rebate — a real edge lever on fee-enabled
  markets (sports/crypto), but it requires simulating resting-order queues + fill probability,
  which snapshot-based 5-min ingestion can't do honestly. Revisit only with a WebSocket feed
  (same prerequisite as simultaneous-fill arb execution).

- **Exit evaluator broke a risk-free basket — found & fixed 2026-07-04 evening check.** FIRST LIVE
  NEGRISK CAPTURE fired at 18:17 on the in-play Canada–Morocco WC exact-score event (event 650891):
  11 No-legs × 5 units, cost $48.64, guaranteed payout ≥ $50 (max one exact score resolves Yes) ⇒
  risk-free +$1.21 net — the scanner saw an implied-Yes sum of up to **1.272** (27% overround,
  in-play books). Then the NEW exit evaluator treated the legs as directional positions: in-play
  exact-score prices swing violently, TP/SL fired on 5 of 11 legs (−$5.42 sold), 6 legs settled
  (+$1.41) → realized **−$4.01 instead of guaranteed +$1.21** (a $5.22 structural loss; arithmetic
  reconciles exactly with the portfolio snapshot). Root cause: exits had no concept of BASKET
  positions — selling any leg re-introduces exactly the risk the structure eliminated. Also latent:
  the two-leg Yes+NO arb pairs had the same exposure, and exits selling legs re-arms the negrisk
  no-pyramiding guard (skip only when ALL legs held) → potential buy/exit churn loop (didn't
  trigger — only 1 execution — but was live). **Fix:** the exits query now skips any position whose
  post-reset entry order came from `autonomous_arb_executor` / `autonomous_negrisk_arb_executor`
  (arb legs are hold-to-resolution by design); this also closes the churn loop at the source.
  **Positive findings from the same incident:** the negrisk scanner+executor work end-to-end
  (detect → 11-leg basket fill → settlement), settlements on the 6 held legs paid correctly, and
  the mark-to-market cash identity held throughout ($9,995.78 = 10,000 − 4.005 − ~$0.22 fees).
  Minor cosmetic: the `settlement` snapshot at 19:33 wrote cash $9,948.71 (freed collateral not yet
  credited) and the same-second mark_to_market corrected it to $9,995.78 — a 1-tick equity-curve
  dip, self-healing, not chased.

- **Rotation tag gate (the whack-a-mole ender) — 2026-07-05 morning check.** Overnight the rotation
  restored real directional flow: 3 entries, 2 exactly the intended profile (WTI-dip-to-65 Yes @
  0.63, SBF-pardon No @ 0.983) — but the third was `will-t1-win-msi-2026` (LoL esports, $17.14 Yes)
  and `cricmlc-was-san` (Major League Cricket) was promoted untraded: the THIRD slug-format
  generation to dodge the keyword classifier in two days. Cleanup: both rows deleted, T1 position
  sold via POST /paper/orders (+$0.51 realized after fees — lucky, not clean). **Structural fix:**
  the market/slug Gamma endpoints carry NO tags, but `/events/{id}` does (T1's event:
  ["Sports","Esports","league of legends","lol"]) — rotation now fetches the parent event's tags
  for each would-be promotion and hard-rejects any tagged Sports/Esports, FAIL-CLOSED (no event id
  or fetch error ⇒ no promotion, retried next pass). Slug keywords stay as the cheap first filter
  (added `cric*`/`cricket`/`-msi-` + regression tests); the tag gate is the data-driven backstop
  that doesn't depend on us predicting slug formats. Deployed local-1783234361; first pass: 100
  candidates, all filtered by the (now-complete-for-today) keywords, 0 promoted, gate armed.
  Known minor: tag-rejects consume promotion slots within a pass (`take(room)` runs before the
  gate) — at 6h cadence with a refreshing pool this self-heals; not worth churn.
  **Overall morning state: healthy.** No real errors (75 log hits all routine CLOB decode noise),
  exits correctly did NOT touch anything overnight (no arb baskets fired; the 3 directional
  positions sat within TP/SL bands), settlements clean, cash identity holds ($9,941.29 = 10,000 −
  4.005 realized − fees − $53.80 locked… identity verified), DB bounded.

- **Midday check 2026-07-05 — routing bug from the fee work, found & fixed (local-1783253913).**
  `arb_category` has TWO jobs — fee-rate classifier AND arb-only router — and adding "mentions" for
  the 0.04 fee rate silently made every mentions market arb-only, vetoing the rotation-promoted
  musk-tweets market (a DR showed 16% net edge but the executor skipped it with no rejection event
  — the veto ran before eligibility). Fix in `maybe_execute_opportunity`: rotation-active markets
  are eligible AS GRANTED (the promotion pipeline's event-tag gate already vetoed sports/esports;
  the slug-category veto no longer re-applies), while bootstrap slugs keep the historical
  `is_arb_only_market` veto (that list deliberately contains arb-only WC/crypto markets). RULE for
  future category additions: adding a category to `arb_category` changes BOTH fees and routing —
  check both consumers.
  Post-fix verification surfaced a correct-but-notable chain: musk-tweets now reaches the executor,
  and Kelly sizes it to ZERO — its No is priced 0.9995 (bracket effectively decided), so there is
  nothing rational to buy. The fused "16% net edge" on a ceiling-priced No is a SIGNAL-CALIBRATION
  smell (fusion doesn't respect the price ceiling; the edge should shrink as p→1) — the safety
  stack (Kelly) neutralizes it, but a `net_edge vs (1−p) ceiling` clamp in the DR generator would
  stop these phantom edges polluting the scorecard. Noted as a signal-quality follow-up, not
  urgent.
  Also verified quiet-but-alive: 480 DRs/h (full 40-slot set), 12 arb + 12 negrisk scans/h, ingest
  current, 0 risk rejections (nothing eligible had a sizable real edge), negrisk best sum 1.010 —
  correctly filtered (on 3%-fee sports events a ~1% overround is net-negative after per-leg fees).
  The manual T1 sale's realized capture verified in the books (−4.005 → −3.053, locked freed
  exactly $17.14). **Real limiter now: rotation candidate SUPPLY** (3 active; the top-100
  short-dated volume pool is a wall of sports matches). Next lever: paginate discovery past the
  sports wall (Gamma offset param) and/or tag-filtered discovery queries.

- **Exit↔entry churn loop — found & fixed, morning check 2026-07-06 (image local-1783313120).**
  The paginated rotation set (16 markets) unleashed the directional engine overnight: 19 fills, 17
  exits, 16 settlements, 2 negrisk baskets — and realized P&L bled −$3.05 → −$52.77. Decomposition:
  settlements fine (+$1.69/16), take-profit fine (+$4.57), **stop-losses −$54 across 9 exits at avg
  3h hold**. Two churn mechanisms, both measured live: (1) **stop→rebuy loop** — a stop-loss frees
  the market, the next 5-min DR still likes it, the executor re-buys (england-mexico-rescheduled
  bought 4× in one night); (2) **side oscillation** — the both-sides DR eval flips its target on
  small mid moves, the signal-flip exit sells, the executor buys the OTHER side (WTI-85 flipped
  Yes⇄No 5×). Each round trip pays spread + fees. Root causes: a −15% RELATIVE stop on cheap shares
  is pennies of bid/ask noise (0.18 entry stops on a 2.7¢ wobble), and nothing stopped re-entry.
  **Fixes:** (a) `POLYTRADER_REENTRY_COOLDOWN_HOURS` (24) — after any autonomous exit, the market
  is blocked from directional re-entry, PER-MARKET so a side-flip can't dodge it; (b)
  `POLYTRADER_EXIT_MIN_ABS_MOVE` (0.04) — the stop only fires when the mid also moved ≥4¢ absolute
  (high-priced entries unaffected; their 15% already exceeds it). Worst case is now ONE stop-loss
  per market per day instead of a loop. **Protections that held:** the arb-leg exclusion kept exits
  OFF both overnight negrisk baskets (3-leg +1.6¢/u and 13-leg +4.7¢/u, complete, awaiting
  resolution) — the 07-04 basket incident did not repeat. Deploy note: the first `make k8s-deploy`
  silently no-op'd (docker-frontend DeadlineExceeded masked by the tail pipe — the [[feedback]]
  about not piping make through head applies to error-masking generally); caught by verifying the
  pod image, retried OK.

- **Discovery pagination SHIPPED — evening check 2026-07-05 (image local-1783275412 + floor fix).**
  Measured the sports wall first: page 0 of the short-dated volume ranking has ~5 non-sports
  candidates; pages 1–4 hold ~150 (Fed brackets, BTC weeklies, Iran/Hormuz July deadlines, GPT-5.6
  release ladder, WTI ladder, box office…). `discover_directional_markets` now fetches 5 pages
  (500 candidates, early-stop on a short page), and the rotation loop no longer pre-truncates to
  `room` (tag-rejects were burning promotion slots). First paginated pass: **15 promoted**
  (GPT-5.6 date ladder ×6, WTI ladder ×3, Khamenei ×2, Trump–Starmer, Machado-Venezuela…), and the
  TAG GATE proved itself on formats keywords can't anticipate — rejected chess tournaments (tagged
  "Sports"), FIBA basketball (bkfibaqaf-), KBO baseball (kbo-). Two follow-ups from the same pass:
  (1) it promoted `btc-updown-5m-*` — a perpetual 5-MINUTE crypto binary that expired 5s after
  promotion; added `POLYTRADER_ROTATION_MIN_HOURS` (default 12) as a time-to-resolution floor
  (sub-12h markets can't complete an ingest+DR cycle and the updown series would waste a slot
  every pass); row deleted, verified next pass promotes 0 junk (16 active, cap 20). (2) Noted, not
  built: ladder promotions concentrate exposure on ONE underlying event (6× GPT-5.6 dates ⇒ up to
  ~$120 correlated at the $20 cap) and `risk::cluster_key` won't group them (they fall in the
  exempt "uncorrelated" bucket) — an event_id-based cluster key would close this; at paper scale
  (1.2% of bankroll) it's low priority. Also added `crint-` (cricket international) to the keyword
  prefilter with a regression test.

- **Stop-loss WIDENED 0.15 → 0.50 — the exits, not the entries, were the loss (2026-07-09).**
  Acting on the "path to profitability" plan, ran the harness-validation it called for BEFORE
  coding — and it redirected the whole conclusion. Decomposed post-reset realized P&L two ways:
  - **By entry-price band:** >0.85 favorites were the ONLY positive band (+$1.41); 0.70–0.85 was the
    worst (−$41.78). The planned P2 [0.15,0.85] band would have been BACKWARDS (cut winners, keep
    losers) — so P2 is dropped, not built. (The favorite-only edge echoes the calibration finding
    that high-conviction bets are underpriced; a future signal lever, not touched at n=3.)
  - **By realization type — the decisive cut:** `exit <0.30 −$25.13 / exit 0.30–0.85 −$59.83 /
    exit >0.85 −$3.03 / settle >0.85 +$4.44`. **100% of losses are EXITS; every position held to
    resolution WON (3/3).** Of the −$88 exit total, stop-loss is ~−$69 (the rest signal-flip −$6,
    take-profit +$4.6). Even post-07-06-fix, the 8 stops fired right at the −15.6% threshold, held
    0.6–3.5d, six of eight on the correlated WTI ladder (one oil wobble trips the whole ladder).
  **Root insight:** a prediction-market position is ALREADY bounded — a share going to $0 loses at
  most its entry cost, there is no leverage/blowup tail a tight stop protects against — and these
  prices mean-revert short-term (long-documented: "unrealized marks revert to cost"). So a −15% stop
  systematically sells noise AND pays friction to do it; it is strictly value-destroying here.
  **Fix:** stop-loss default 0.15 → 0.50 (env `POLYTRADER_EXIT_STOP_LOSS_PCT`) — fires only on a
  genuine thesis-collapse (a halved position); ordinary wobble now rides to resolution or the 14d
  time-stop (which still frees dead capital), take-profit still locks real gains, abs-move floor
  unchanged. This should convert the bulk of the −$69 stop-loss bleed into held positions that (on
  the 3/3 evidence, tiny but directionally clear) resolve at or above cost. Expectation to verify
  next check: far fewer stop-loss exits, realized P&L bleed rate drops sharply. **Caveat:** n=3
  settlements is thin; if held positions start resolving as LOSSES the picture changes — but holding
  a bounded position through mean-reverting noise still dominates realizing that noise + friction.
  Also flags the correlated-cluster problem again (6/8 stops were WTI ladder) → the event_id
  cluster-key TODO would have sized that whole ladder as one bet.

- **Evening check 2026-07-08 (hermes local-1783541404): tuning verified live + fee-drag insight.**
  The rebuilt learning loop is demonstrably working: 13 weight updates in 5h, theta walking
  0.979→0.921→0.926 in damped steps toward its negative-realized target, momentum easing to 0.990.
  Also fixed the hermes slow-query spam: the 7d fire-rate baseline (~3–4s jsonb aggregate) was
  recomputed every 5-min reflection; now cached 1h (`signal_fire_counts_7d_cached`, mirrors
  server.rs's pattern) — one fresh compute per hour/boot, zero slow-statement WARNs after.
  **Strategic quantification (echoing Hermes's own LLM synthesis "flat before fees"):** since the
  07-04 reset, realized is −$61.03, of which fees $17.19 + est. slippage ~$33.18 ≈ **$50 (82%) is
  pure execution friction**; gross signal P&L is ≈ −$11 over 4 days (near-flat). The book isn't
  losing on direction — it's bleeding ~$12/day in taker friction at current turnover. The levers,
  in order: (1) reduce turnover further (the churn fixes already cut it), (2) the maker-execution
  TODO (zero fees + 20–25% rebates — the structural fix, blocked on WS), (3) spread-aware entry
  gating (don't cross wide spreads for thin edges).

- **Learning loop was HALF-closed — found & fixed, check 2026-07-08 (hermes local-1783523692).**
  Weight tuning had gone silent for 37h ("no change this cycle" every reflection). Root-caused to
  THREE stacked issues in Hermes's realized-P&L attribution (`load_per_signal_realized_pnl` +
  `load_calibration`):
  1. **Exit round-trips were invisible.** Attribution read ONLY `paper_position_settled` — but since
     the 07-04 exits feature, autonomous exits are the DOMINANT realization path (TP/SL/time-stop/
     signal-flip close positions in hours-days). Every exit's realized P&L — including the −$51
     churn losses — never fed signal learning. The exits feature's stated purpose was "realized
     feedback for Hermes in days"; the Hermes side of the pipe was never connected. Fixed: the
     attribution sample is now settlements UNION exits (net = realized_gross − fees).
  2. **6th reset-boundary occurrence:** the settled query was LIFETIME — its 74 rows = 22 phantom
     06-24 re-settlements + 16 legacy + 36 post-reset. The phantoms attributed fake +P&L to whatever
     fired (momentum/theta). Fixed with the standard post-reset filter.
  3. **Arb legs polluted attribution:** a negrisk/Yes+NO leg's settlement reflects the basket
     structure, not the fusion signals that happened to fire on that market's DRs (the 07-05 WC
     baskets included bootstrap markets WITH decision reports → false credit). Fixed: arb-executor-
     entered positions are excluded from attribution AND calibration (mirrors the exits exclusion).
  **Effect was immediate and dramatic:** attributable sample 74 → 39; per-signal realized flipped
  from the contaminated (+7.76 theta / +6.72 momentum) to the TRUTH (−48.86 theta / −15.06 news /
  −4.66 momentum) — and the first strategy_weights event in 37h landed seconds after deploy (theta
  1.007 → 0.979, damped steps). The system had been learning from fiction; now it learns from what
  actually happened, including its own exits.
  **Discovered en route — the hermes image was STALE SINCE 06-24:** every `hermes:local-*` deploy
  tag pointed at a June-24 image (docker CreatedAt identical across all tags), behaviorally
  confirmed by the retired overreaction_fade still appearing in the live weights map. All hermes-
  side changes for two weeks had silently not shipped. Manual `docker build -f Dockerfile.hermes`
  produced a fresh image (build itself works; historical mechanism unclear — likely repeated
  transient BuildKit frontend failures aborting `docker-build` after the polytrader line on the
  days hermes code changed). Freshness guard added to the TODO list.

- **Unbounded orderbook_snapshots growth — found & fixed, check 2026-07-06 (image local-1783352736).**
  DB size trend prompted a look: `orderbook_snapshots`' oldest row was **2026-06-13** — over 3 weeks,
  despite the documented 48h retention window. Root cause: `prune_orderbook_snapshots` always keeps
  the LATEST snapshot per (market, outcome) — correct for the CURRENT live working set, but any
  market that permanently rotates OUT of the ingest universe (arb-discovery samples ~150/tick,
  rotation discovery ~500/pass) never gets a newer snapshot, so its stale "latest" row is kept
  FOREVER. Measured: 201 distinct stuck markets (402 rows), **183 of them not even formally closed**
  in our DB — just discovery/rotation churn that happened to rank once and never again.
  `rollup_price_history` already rolls up EVERY row past the 48h window regardless of latest-status,
  so the price signal was never at risk — only the raw book was leaking. Checked all consumers
  (arb/negrisk scanners use a 30-min freshness bound; paper engine + `fetch_latest_book` just want
  "the latest, whatever it is") — nothing depends on a stale row surviving, since any market with a
  live purpose (bootstrap/rotation-active/held-position) is guaranteed a fresh snapshot every ingest
  tick via the must-track union, so it never goes this stale in the first place. **Fix:** added
  `STALE_LATEST_CAP_DAYS = 3` — the keep-latest exception is now capped; past 3 days a "latest" row
  is pruned regardless. Verified post-deploy: oldest row dropped from 06-13 to within the 3-day cap,
  55,167 rows deleted in the first pass, `orderbook_snapshots` at 122K live rows / **0 dead tuples**
  (autovacuum keeping pace). This was a slow, compounding leak — it would have kept growing roughly
  in proportion to the total distinct markets ever sampled by discovery over the system's lifetime.
  **Same check confirmed the 2026-07-06 anti-churn fix (cooldown + abs-move floor) is working as
  designed**, one full day in: zero cooldown breaches since deploy (grep-verified: every rebuy-after-
  exit row is dated before the 04:45 deploy), stop-loss frequency down ~8× (2 exits/24h at −$6.69,
  avg 38h held, vs 9 exits/10h at −$54, avg 3h held pre-fix) — the fix is converting noise-driven
  round-trips into occasional, larger, more deliberate exits. Also reconfirmed the arb-leg exclusion:
  a new 5-leg negrisk basket (guaranteed +$0.61) filled and sat untouched by exits. No settlement
  gaps, zero non-routine errors, rotation turning over normally (3 promoted/3 demoted/24h, 0 tag-gate
  leaks). Portfolio still net-negative (realized −$56.28) — consistent with the long-documented "no
  proven directional edge" finding, not a new bug; the churn fix slowed the bleed rate, it didn't
  (and can't) manufacture edge that isn't there.

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
