-- Compact "cold" summaries that preserve the historically-useful signal from the two fat tables before
-- the daily GC prunes their raw rows (see src/gc + wiki/roadmap retention plan).

-- Hourly mid per (market, outcome), rolled from orderbook_snapshots before the raw bids/asks blobs
-- (the bulk of the 850MB) are deleted. Keeps the price curve for history at ~1/1000th the size.
CREATE TABLE IF NOT EXISTS market_data.price_history (
    market_id text NOT NULL,
    outcome   text NOT NULL,
    hour      timestamptz NOT NULL,
    mid       numeric(20,8),
    PRIMARY KEY (market_id, outcome, hour)
);

-- Per-day per-signal fire counts, rolled from decision_reports before the raw attribution payloads are
-- deleted. Preserves the long-term signal-health/fire-rate history without the ~1.7KB/report payloads.
CREATE TABLE IF NOT EXISTS journal.signal_daily (
    day     date NOT NULL,
    signal  text NOT NULL,
    reports bigint NOT NULL,
    fired   bigint NOT NULL,
    PRIMARY KEY (day, signal)
);
