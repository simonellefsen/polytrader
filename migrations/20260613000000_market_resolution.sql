-- Resolution tracking: capture each market's final outcome so paper positions can be settled to
-- realized P&L. outcome_prices mirrors Gamma's outcomePrices ([ "1","0" ] when resolved); when a
-- market is closed and exactly one outcome is priced ~$1, resolved_outcome names the winner.
ALTER TABLE market_data.markets
    ADD COLUMN IF NOT EXISTS outcome_prices JSONB,
    ADD COLUMN IF NOT EXISTS resolved_outcome TEXT;
