-- Per-market Polymarket taker fee rate, synced from the Gamma `feeSchedule.rate` / `feesEnabled`
-- each ingest cycle. NULL = not yet synced (callers fall back to the category default). 0 = fee-free
-- (e.g. geopolitics). The fee itself is `shares × rate × price × (1 - price)` (see polymarket_taker_fee).
-- Stored so historical P&L (per day/week/month) reflects the fee schedule in force, even if it changes.
ALTER TABLE market_data.markets
    ADD COLUMN IF NOT EXISTS taker_fee_rate NUMERIC(10,5);
