# Decision: Operational Goals, Risk Limits & Cadences for ~$150 Paper Bankroll

**Date**: 2026-05-25  
**Status**: Accepted  
**Related**: wiki/strategies/goals-and-operational-cadence.md (the full definition), the 2026-05-25 polymarket-github transfer plan (especially 3.2 FusionEngine + 3.3 Hermes learning loop), AGENTS.md safety & paper-first philosophy, current ~$150 / DKK 1000 virtual capital.

## Context
With a very small starting paper bankroll (~$150), aggressive return targets are unrealistic and dangerous. The system needs clear, measurable daily/weekly goals + defined cadences so that:
- The 5-minute opportunity/decision layer (powered by the new FusionEngine) has something concrete to optimize toward.
- The hourly Hermes reflection can do meaningful P&L attribution against goals + signal performance.
- Risk discipline is enforced before any trade is accepted.
- Progress is visible in the UI and journal for both humans and Hermes.

This decision formalizes the operating rhythm on top of the architecture delivered so far and the patterns transferred from the 5 polymarket-github repos.

## Options Considered
1. **Keep current vague / no explicit goals** (just "make money in paper").  
   Rejected — no way to drive the FusionEngine or give Hermes useful attribution targets. Leads to undefined "success".

2. **Aggressive targets** (e.g. +5–10% daily, high-frequency trading).  
   Rejected — completely unrealistic and high-risk with $150 after fees/slippage. Violates AGENTS safety-first + paper learning mandate.

3. **Conservative, process-oriented goals + two clear cadences** (5-min Decision Reports + hourly Hermes reflection) with hard risk limits (1% per trade, 5% daily loss, 15% weekly DD, min edge thresholds).  
   **Selected**. Matches the scale of the capital, leverages the transferred signal fusion + learning patterns, stays fully paper-only, and gives concrete work for the 3.2/3.3 layers.

## Decision
Adopt the full framework defined in `wiki/strategies/goals-and-operational-cadence.md`:

- **Hard risk limits** (enforced in paper engine + decision layer): 1% max risk per trade, 15% max exposure, 5% daily loss limit (pause), 15% weekly drawdown limit, minimum 4–6% edge after fees/slippage.
- **Daily goals**: 5–10 logged high-quality opportunities, 1–4 disciplined trades only when gates are met, +0.8% to +2.5% net target (or positive expectancy + zero limit breaches + complete journaling), one high-quality Hermes reflection with goal attribution.
- **Weekly goals**: +3% to +8% net (ambitious but secondary to consistency), win rate ≥55–60% or clear positive expectancy, max DD <12%, ≥2–3 concrete Hermes outputs (weight tweaks, experiments), at least one fusion experiment.
- **Cadence**:
  - Every ~5 minutes: Run signal processors + FusionEngine → structured Decision Report (opportunities + per-signal attribution + risk/goal-filtered sizing) logged to journal.
  - Every 60 minutes: Hermes full reflection with P&L attribution vs goals + signals, decision quality review, gated wiki proposals for adjustments.
- Everything is designed to be the first real consumer of the `src/strategy/` FusionEngine skeleton and the closed-loop learning vision in the hermes concepts page.

## Rationale
- Conservative numbers protect the tiny bankroll while still allowing statistically meaningful signal attribution after a few hundred paper trades.
- Clear cadences give the 5-min trader layer and the hourly Hermes layer distinct, non-overlapping responsibilities (fast decisions vs deep self-improvement).
- Directly accelerates two of the highest-value parts of the approved transfer plan (3.2 multi-signal fusion for opportunity scanning, 3.3 Hermes learning/attribution loop).
- Fully observable via the journal (decision reports + goal snapshots + reflections) — exactly what Hermes needs to improve itself.
- Matches the spirit of the patterns transferred from the 5 repos (frequent short-horizon scanning + learning from outcomes) while staying inside our paper-only + journal-as-source-of-truth constraints.

## Trade-offs & Risks
- **Lower absolute returns** in the short term (by design — learning phase).
- Requires the journal to be rich enough for attribution (already the plan; the 5-min decision reports will help).
- 5-minute cadence adds some load (mitigated by keeping it lightweight CPU + recent snapshot reads; can be driven by the ingester tick).
- Goals may need tuning after real data (expected; Hermes + weekly reviews will propose adjustments via the gated wiki mechanism).

## Implementation Path (Smallest First)
1. Wiki-first (this decision + the goals page + log entry + updates to hermes concepts + project-plan) — done.
2. Config for the limits + cadences + goal targets (env vars or simple struct).
3. 5-min Decision Report generator (calls the FusionEngine, applies risk/goal filters, logs structured report).
4. Extend Hermes hourly reflection to also read recent decision reports + goal progress and produce attribution + proposals.
5. UI cards for current goal progress + live Decision Reports (builds on existing Phase 2 SSR).
6. Hard enforcement of risk limits in the paper engine before any trade is accepted.

All paper-only, using existing patterns (Decimal, journal writer, tracing, no new migrations initially).

## Consequences
- The system now has a clear "heartbeat" (5 min decisions + hourly self-improvement) with measurable success criteria.
- Future work on risk engine (3.4), UI, and experiment runner has concrete targets to optimize against.
- Hermes becomes much more useful because it has explicit goals to reason about.

See `wiki/strategies/goals-and-operational-cadence.md` for the complete, living specification (including exact suggested numbers and integration points into the architecture).

*Decision recorded wiki-first per AGENTS.md. Explicit credits to the overall 2026-05-25 transfer plan and the patterns observed in the 5 source repos (especially frequent scanning + outcome-based learning loops).*