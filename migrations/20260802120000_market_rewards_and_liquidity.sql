-- Gamma fields we were already fetching on every ingest tick and discarding (2026-08-02).
--
-- The headline one is rewards_daily_rate: Polymarket pays a per-market daily budget to RESTING
-- orders whether or not they fill (one market observed at $1,000/day), and rewards_min_size /
-- rewards_max_spread state the qualification rule exactly. Together with the venue's takerOnly
-- fee schedule (makers pay nothing) that is the concrete, measurable form of the P5 maker thesis
-- — and it needed no new API integration, only parsing a response we already had.
--
-- tick_size bounds how finely a maker can undercut. We were fetching it from the CLOB /tick-size
-- endpoint in a dry-run-only path and dropping the tick_size the WS feed puts on every book frame,
-- while it sat unparsed in the Gamma payload.
--
-- markets.liquidity already exists (init migration, 2026-05-25) and has NEVER been written. It is
-- left alone here and simply starts being populated by the ingest upsert.

ALTER TABLE market_data.markets
    ADD COLUMN IF NOT EXISTS tick_size          NUMERIC(10,5),
    ADD COLUMN IF NOT EXISTS rewards_daily_rate NUMERIC(20,8),
    ADD COLUMN IF NOT EXISTS rewards_min_size   NUMERIC(20,8),
    ADD COLUMN IF NOT EXISTS rewards_max_spread NUMERIC(20,8);

COMMENT ON COLUMN market_data.markets.tick_size IS
    'Gamma orderPriceMinTickSize — minimum price increment (0.01 / 0.001).';
COMMENT ON COLUMN market_data.markets.rewards_daily_rate IS
    'Sum of clobRewards[].rewardsDailyRate (USD/day) paid to resting orders. NULL = no program.';
COMMENT ON COLUMN market_data.markets.rewards_min_size IS
    'Gamma rewardsMinSize — min resting order size (shares) to qualify for rewards.';
COMMENT ON COLUMN market_data.markets.rewards_max_spread IS
    'Gamma rewardsMaxSpread — max distance from midpoint (cents) a resting order may sit and qualify.';

-- Partial index: reward-paying markets are a small minority of the table, and the only query that
-- will ever use this column ranks them by budget. Kept partial so it stays tiny as markets grow.
CREATE INDEX IF NOT EXISTS markets_rewards_daily_rate_idx
    ON market_data.markets (rewards_daily_rate DESC)
    WHERE rewards_daily_rate IS NOT NULL AND active AND NOT closed;
