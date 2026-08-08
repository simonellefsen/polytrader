-- P5 increment 3b — shadow maker quotes (2026-08-08). MEASUREMENT ONLY: nothing is placed.
--
-- `scan_rewards` estimates what a resting order would capture, but it is a SNAPSHOT estimator: it
-- reads one instant, computes our share of the qualifying depth, and multiplies by the full daily
-- rate. That embeds two assumptions it cannot check, and its own module docs flag both:
--
--   1. That a quote keeps qualifying. The estimate annualises an instantaneous share over 24h. If
--      the midpoint drifts out from under our price after 20 minutes, the real capture is ~1/72 of
--      the headline number. Nobody has measured how long a quote stays inside `rewards_max_spread`.
--
--   2. That resting is free. It is not — a resting order is filled precisely when the market is
--      moving against it, and the reward is compensation for that, not a bonus on top. The module
--      says it plainly: "adverse selection is not modelled at all, and it is the entire risk of
--      making." A capture estimate that ignores it is not an expectancy claim.
--
-- This table is the state a shadow quote needs to answer both, since both are about a specific
-- price surviving over HOURS and so cannot live in memory across the deploys that happen daily.
--
-- Fill P&L is measured over a horizon rather than at the fill instant. Measuring at the instant
-- would be circular: the fill TRIGGER is the mid crossing our price, so the loss at t=0 is negative
-- by construction and we would be reporting our own trigger back to ourselves. The horizon lets the
-- market come back — mean reversion is the maker's actual edge — so the sign of `horizon_pnl_usd`
-- is a real result rather than an artifact.

CREATE TABLE IF NOT EXISTS paper_trading.shadow_quotes (
    id                  uuid PRIMARY KEY,
    market_id           text NOT NULL REFERENCES market_data.markets(gamma_id),
    token_id            text NOT NULL,
    -- 'Bid' = we would rest a buy below the mid; 'Ask' = a sell above it.
    side                text NOT NULL CHECK (side IN ('Bid', 'Ask')),
    price               numeric(20,8) NOT NULL,
    size                numeric(30,8) NOT NULL,

    -- Reward-programme terms captured AT PLACEMENT. Stored rather than re-read because the market's
    -- terms can change under an open quote, and the accrual must be judged against the rule that
    -- was in force when the price was posted.
    daily_rate          numeric(20,8) NOT NULL,
    max_spread          numeric(20,8) NOT NULL,
    mid_at_placement    numeric(20,8) NOT NULL,

    placed_at           timestamptz NOT NULL DEFAULT now(),
    -- Last cycle that evaluated this quote. Accrual integrates over (now - last_evaluated_at), so a
    -- gap in evaluation (restart, dead feed) credits nothing rather than silently back-filling time
    -- the quote may not have spent qualifying.
    last_evaluated_at   timestamptz NOT NULL DEFAULT now(),

    -- Accumulated seconds spent INSIDE the reward band. The answer to assumption (1): compare this
    -- against (now - placed_at) to get the duty cycle the snapshot estimator assumes is 100%.
    qualifying_seconds  numeric(20,4) NOT NULL DEFAULT 0,
    -- Rewards earned so far, integrated per-interval at the share observed in that interval.
    accrued_reward_usd  numeric(20,8) NOT NULL DEFAULT 0,

    status              text NOT NULL DEFAULT 'open'
                        CHECK (status IN ('open', 'filled', 'cancelled')),

    -- Fill + the horizon measurement. Null until the mid crosses our price.
    filled_at           timestamptz,
    mid_at_fill         numeric(20,8),
    -- Mid one horizon later, and the resulting P&L on the shares we would hold. NEGATIVE means we
    -- were picked off; POSITIVE means the move reverted and making paid. This is the number the
    -- whole increment exists to produce.
    mid_at_horizon      numeric(20,8),
    horizon_pnl_usd     numeric(20,8),

    closed_reason       text
);

-- The tracking cycle's only hot query: every open quote, plus filled ones still inside their
-- measurement horizon.
CREATE INDEX IF NOT EXISTS idx_shadow_quotes_open
    ON paper_trading.shadow_quotes (status, filled_at);

-- "Do we already have a quote here?" on placement, so a market cannot accumulate duplicates.
CREATE INDEX IF NOT EXISTS idx_shadow_quotes_market_status
    ON paper_trading.shadow_quotes (market_id, side, status);
