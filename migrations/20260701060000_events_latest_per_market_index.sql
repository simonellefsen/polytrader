-- "Latest event of type T per market" lookups (the /board's latest decision_report and news_cache per
-- market) were written as DISTINCT ON scans over ALL decision_reports (~92k rows, 85% of the events
-- table, growing ~3k/day) with an external-merge sort that SPILLED ~41MB to disk (~1.3s, and worsening).
-- This composite expression index turns them into 50 LATERAL LIMIT-1 index lookups (~0.5ms total).
-- Serves any: WHERE event_type = ? AND payload->>'market_id' = ? ORDER BY created_at DESC LIMIT 1.
CREATE INDEX IF NOT EXISTS idx_events_type_market_created
    ON journal.events (event_type, (payload->>'market_id'), created_at DESC);
