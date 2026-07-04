-- NegRisk event grouping for the event-level arbitrage scanner (see wiki/roadmap).
--
-- Polymarket groups mutually-exclusive outcomes (e.g. "2028 GOP nominee" candidates) into a
-- negRisk EVENT of N binary markets, of which AT MOST ONE resolves Yes. Buying No across k member
-- markets therefore pays at least $(k-1) — an arb whenever sum(best No ask) < k-1, i.e. whenever
-- the implied Yes probabilities sum above 100% (the classic overround). This works on ANY subset
-- of the event's members, so partial book coverage still finds (smaller) arbs.
--
-- Single-market Yes+No arb was measured structurally dead (2026-07-03/04: 430 scans, best cost
-- pinned at $1.000-1.001, zero sub-dollar books); the event level is where real dislocations live.
ALTER TABLE market_data.markets ADD COLUMN IF NOT EXISTS event_id text;
ALTER TABLE market_data.markets ADD COLUMN IF NOT EXISTS neg_risk boolean NOT NULL DEFAULT false;

-- The scanner groups active negRisk members by event.
CREATE INDEX IF NOT EXISTS markets_negrisk_event_idx
    ON market_data.markets (event_id) WHERE neg_risk AND active AND NOT closed;
