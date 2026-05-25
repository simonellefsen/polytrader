# Goals, Risk Parameters & Operational Cadence

**Date**: 2026-05-25 (added as part of Phase 3 strategy brain + Hermes operationalization, wiki-first per AGENTS and the approved 5-repo transfer plan)  
**Capital baseline**: ~$150 USD (≈ DKK 1,000) virtual paper bankroll.  
**Philosophy**: With very small capital, the primary objective is **consistent positive expectancy + rapid learning**, not aggressive growth. All goals are deliberately conservative. Process (edge identification, risk discipline, complete journaling, Hermes attribution) > raw P&L outcome. Everything runs in paper mode only until explicit human gates.

This page defines measurable daily/weekly goals and the two main cadences that drive the system:
- **5-minute opportunity scan + Decision Report** (main "trader" loop, powered by the FusionEngine + signal processors from the transfer plan).
- **Hourly Hermes self-improvement / reflection** (P&L attribution against goals, signal performance, gated proposals).

All numbers and cadences are designed to integrate directly with:
- `src/strategy/` (FusionEngine for 5min decisions)
- Enhanced Hermes (hourly reflection + goal tracking)
- Journal (decision reports + goal snapshots + attribution)
- Paper engine + risk layer (hard enforcement of limits)
- Dioxus UI (live goal progress, decision reports, Hermes reflections)

## Core Risk Parameters (Hard Limits — Non-Negotiable)

These are enforced in the paper engine, decision layer, and before any trade is logged/accepted. They are the foundation that makes the goals realistic.

- **Starting virtual bankroll**: $150
- **Max risk per trade** (position sizing): 1% of current bankroll (≈ $1.50 at start). Use conservative Kelly or fixed-fractional sizing on top of this.
- **Max total exposure** at any moment: 15% of current bankroll.
- **Daily loss limit**: 5% of the starting balance for that day → hard stop trading for the remainder of the day (Hermes still runs reflections).
- **Weekly drawdown limit**: 15% peak-to-trough → reduce size or pause new entries; trigger deeper Hermes review.
- **Minimum required edge**: Trades are only considered when the fused decision report shows **net** expected edge (after estimated fees + slippage + gas) of at least 4–6%. See `fees-tax-latency-and-execution-tiers.md` for detailed fee modeling requirements. Lower net-edge opportunities are logged for learning but not executed.
- **Minimum confidence / fusion score**: Configurable threshold (initially 0.65–0.75) before a Decision Report recommends action.

These limits are small enough that even a bad week is survivable for learning, while still allowing meaningful signal attribution.

## Daily Goals (Realistic for $150 Paper Capital)

Focus on process quality + modest positive expectancy.

- **Opportunity quality**: Identify and fully log 5–10 high-quality setups per day via the 5-minute Decision Reports (even if only 1–3 are taken).
- **Trade discipline**: Execute 1–4 paper trades per day **only** when the fused report + risk rules align with the above limits and minimum edge.
- **Outcome target**: +0.8% to +2.5% net on current bankroll on a "good" day (or simply "positive expectancy realized + zero risk-limit violations + 100% journal completeness").
- **Hermes output**: At least one high-quality hourly reflection that explicitly attributes P&L to specific signals/processors vs. daily goal progress.
- **Learning artifact**: End-of-day note in journal or wiki/experiments/ (e.g., "Today’s momentum processor worked well on high-volume crypto markets; low-liquidity political markets hurt edge capture").

**Failure modes that still count as "successful day" for learning**: Hit daily loss limit early (discipline exercised), or no trades taken because no setup met the edge threshold.

## Weekly Goals

- **Net P&L**: +3% to +8% on starting weekly bankroll (ambitious but achievable with good edge capture; primary success metric is **consistency** and drawdown control, not the exact percentage).
- **Trade statistics**: Overall win rate ≥ 55–60% **or** clearly positive expectancy across all trades taken (documented via Hermes attribution).
- **Risk compliance**: Zero breaches of daily/weekly drawdown limits. Max realized drawdown for the week < 12%.
- **Hermes self-improvement**: At least 2–3 concrete, reviewed outputs from the hourly reflections:
  - At least one weight adjustment or processor disable/enable proposal (gated via the lowrisk wiki proposal mechanism).
  - At least one new experiment idea logged in `wiki/experiments/` (e.g., "hybrid Kelly sizing from ai-edge-kelly.md patterns during high-volatility weeks").
- **Experiment / validation**: Run at least one small forward-paper-test or historical replay using the new FusionEngine + different weight mixes.
- **Documentation**: All significant learnings (good and bad) captured in `wiki/log.md` or `wiki/experiments/`.

**Stretch (only after 2+ weeks of clean goal adherence)**: Aim for the upper end of the weekly range while keeping drawdown < 10%.

## Operational Cadence (How the System Actually Runs)

### Every ~5 Minutes — "Trader" / Opportunity Layer (Decision Reports)
This is the main active loop for finding and acting on opportunities. It runs frequently so the system stays responsive to fast-moving Polymarket markets (especially short-horizon or event-driven ones).

- Triggered by the ingester tick or a lightweight dedicated timer (can piggy-back on existing 5-minute patterns already present in hermes).
- **Actions**:
  1. Pull latest market/orderbook snapshots (via enhanced ingester per Phase 3.1).
  2. Run all enabled signal processors (momentum, orderbook, spike, sentiment, liquidity, AI-edge, etc. — see `wiki/strategies/multi-signal-fusion.md`).
  3. Fuse via `FusionEngine` (weighted + consensus logic from the BTC-bot transfer) → single fused edge score + rich per-signal attribution.
  4. Apply risk/goal filters (current bankroll, daily P&L so far, exposure limits, min edge/confidence).
  5. Generate **Decision Report** (structured object or JSON logged to journal):
     - Ranked list of top opportunities (market, side, recommended virtual size, expected edge, confidence, dominant signals).
     - Full attribution (e.g., "orderbook_processor contributed +0.8 edge, momentum +0.3, sentiment -0.2").
     - Risk check result + which goal it supports.
  6. (Optional in this phase) Auto-submit the top 1–2 to the paper engine if they pass all gates (behind feature flag for safety).
  7. Log everything + update running goal progress counters.

This directly uses the `src/strategy/` module delivered in the 2026-05-25 transfer increment. The 5-minute frequency matches the "every 5 minutes or so" request while staying lightweight (most work is pure CPU + DB reads of recent snapshots).

**Output visible in**:
- Journal (queryable by Hermes and UI).
- Future Dioxus "Live Opportunities" / "Decision Report" panel (builds on Phase 2 SSR cards).

### Every 60 Minutes — Hermes Self-Improvement & Reflection Layer
This is the "meta" loop (much less frequent than the current ~5–10 min reflection in Phase 2 code, per the user's explicit request for "every hour").

- Runs on its own timer (independent of the 5-min trader loop).
- **Actions** (extends the existing `do_reflection` + gated proposal logic):
  1. Query recent fills, portfolio snapshots, and all decision reports logged in the last hour (plus daily/weekly goal state).
  2. Full P&L attribution broken down by:
     - Time-of-day / market category.
     - Individual signal processors (from the fusion attribution).
     - Progress vs. daily and weekly goals.
  3. Compare decision reports (what the system "wanted" to do 5–60 min ago) vs. actual outcomes.
  4. Regime / anomaly detection (e.g., "all momentum signals degraded after 14:00 UTC this week").
  5. Local + optional LLM synthesis produces:
     - Natural language summary of goal progress.
     - Concrete recommendations (weight changes, new experiments, risk parameter tweaks).
     - Gated autonomous low-risk wiki proposals (extend the existing `augment_wiki_proposal_if_gated` mechanism) — e.g., "Increase orderbook_processor weight; add liquidity filter from poly-maker patterns".
  6. Store rich reflection (existing `journal.reflections` table + new goal-specific metrics JSONB).
  7. Update persistent goal progress (daily/weekly counters) for UI and future reflections.

This hourly cadence gives Hermes enough data to produce meaningful attribution while avoiding noise from very short time windows. It directly feeds the closed-loop learning vision in `wiki/concepts/hermes-self-improvement.md` (Phase 3.3 extension) and the transferred `learning_engine` patterns from the BTC bot.

### End-of-Day / End-of-Week Deeper Synthesis
- Triggered manually or by a daily/weekly timer.
- Hermes does a longer-horizon review against the weekly goals.
- Can kick off small experiment runner jobs (historical replay of different fusion configs on the week's snapshots).
- Produces a weekly "state of the system" reflection + any major wiki updates.

## How This Fits the Existing Plan & Architecture

- **3.2 Signal Processors + FusionEngine**: The 5-minute Decision Report **is** the first real consumer of the `FusionEngine`. The 5-min loop is the natural place to call it.
- **3.3 Hermes Enhancements + Learning Loop**: The hourly reflection is the primary closed-loop mechanism. Goal attribution + decision-report vs. outcome comparison is exactly the "per-signal P&L attribution" described in the transfer plan.
- **3.4 Risk/Position/MM + Dashboard**: All goals and cadences are enforced here. UI surfaces current goal progress, live Decision Reports, and Hermes reflections (natural extension of Phase 2 cards).
- **3.1 & 3.5**: Richer ingester events + better observability/performance tracking directly support the frequent scans and attribution.

No new database tables are required in the smallest increment (reuse existing journal tables + jsonb for decision reports and goal snapshots, exactly as the skeleton already anticipates).

## Implementation Notes (Smallest Viable Next Steps)

1. **Config** (env or simple struct): `DAILY_RISK_PCT`, `WEEKLY_DRAWDOWN_LIMIT`, `MIN_EDGE_PCT`, `HERMES_REFLECTION_INTERVAL_SECS=3600`, `DECISION_SCAN_INTERVAL_SECS=300`, goal thresholds, etc.
2. **5-min layer**: Lightweight timer (or driven by ingester) that calls the existing + new strategy code and writes a `decision_report` JSONB record.
3. **Hermes hourly**: Change the current ~5–10 min reflection to hourly (or keep a lightweight 5-min idle tick + full reflection only on the hour). Extend `do_reflection` to also read recent decision reports and goal state.
4. **Journal extensions** (comments first): Add `decision_report` and `goal_progress` JSONB columns or separate lightweight tables later.
5. **UI**: Simple cards showing "Daily goal: 1.4% / 2.0% (70%)", "Current risk utilization: 8%", live top Decision Report, recent Hermes reflections.
6. **Wiki & decisions**: This page + updates to hermes concepts + project-plan (already in progress).

All changes remain paper-only, use `rust_decimal::Decimal` everywhere, follow existing patterns, and are heavily commented with risk implications per AGENTS.md.

See the top entry in `wiki/log.md` for the execution status of this goal/cadence definition and the linked decisions/ for the rationale behind the conservative numbers.

**Explicit Credits**: The cadence ideas build on patterns observed across the transferred repos (frequent short-horizon scanning from openclaw, learning/feedback loops from the BTC 15-min bot, risk discipline from poly-maker, etc.), adapted to our small-capital + journal + Hermes architecture.

This framework turns the abstract "self-improving agent" into a concrete, measurable, hourly + 5-minute operating system while staying safely inside the paper-only, observable, wiki-first plan we are executing.