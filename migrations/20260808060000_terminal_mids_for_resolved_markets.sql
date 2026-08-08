-- Correct stale book-derived mids on already-resolved markets (2026-08-08).
--
-- A resolved market has a KNOWN terminal price, but `last_mid_*` was last written from an orderbook
-- and never revisited (the ingest loop skips book fetches for closed markets). When such a book
-- empties, `mid_from_book` falls back to (bid 0 + ask 1) / 2 = 0.5.
--
-- Observed live: an exact-score leg resolved No, and a 384.92-share winning position worth $384.92
-- was marked at $192.46 — a fictional -$190.20 unrealized on the dashboard, with exposure reading
-- 20.1%. It self-corrects on the next 5-minute settlement pass, but until then equity, unrealized
-- P&L and the drawdown-breaker input are all wrong, and that input must never be fiction.
--
-- The ingester now writes terminal mids at the moment resolution is captured. This backfills rows
-- that resolved BEFORE that change shipped, since those markets are closed and will never be
-- re-ingested with a book again.
--
-- Only 'Yes'/'No' winners are touched. The parser passes other outcome labels through, and for
-- those we know the market resolved but not which side of a Yes/No position won — so leave them
-- rather than guess.

UPDATE market_data.markets
   SET last_mid_yes = 1, last_mid_no = 0
 WHERE resolved_outcome = 'Yes'
   AND (last_mid_yes IS DISTINCT FROM 1 OR last_mid_no IS DISTINCT FROM 0);

UPDATE market_data.markets
   SET last_mid_yes = 0, last_mid_no = 1
 WHERE resolved_outcome = 'No'
   AND (last_mid_yes IS DISTINCT FROM 0 OR last_mid_no IS DISTINCT FROM 1);
