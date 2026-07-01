-- The arb scanner, fetch_latest_book, and recent_move all look up the latest orderbook snapshot by
-- (market_id, outcome, fetched_at DESC), but the only index was on token_id — so each "latest book"
-- lookup did a parallel sort scan of the 316k-row / 850MB snapshots table. The arb scanner does ~100
-- of these per cycle (50 markets × 2 legs) → ~17s PER SCAN. This matching index turns each into an
-- index seek (~0.02ms), taking the whole arb scan from ~17s to ~1ms.
-- (Follow-up: orderbook_snapshots grows unbounded — a retention job that keeps only recent + the latest
-- per (market,outcome) would cap the 850MB. Deferred.)
CREATE INDEX IF NOT EXISTS idx_obs_market_outcome_fetched
    ON market_data.orderbook_snapshots (market_id, outcome, fetched_at DESC);
