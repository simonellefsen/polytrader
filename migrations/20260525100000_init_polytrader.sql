-- sqlx migration: Phase 0 core schema for polytrader
-- Implements tables from wiki/schema.md (adapted for practical bootstrap)
-- All monetary values use NUMERIC mapped to rust_decimal::Decimal
-- Schemas: market_data, paper_trading, journal

CREATE SCHEMA IF NOT EXISTS market_data;
CREATE SCHEMA IF NOT EXISTS paper_trading;
CREATE SCHEMA IF NOT EXISTS journal;

-- Markets (from Gamma API)
CREATE TABLE IF NOT EXISTS market_data.markets (
    gamma_id TEXT PRIMARY KEY,
    slug TEXT NOT NULL,
    question TEXT NOT NULL,
    category TEXT,
    outcomes JSONB NOT NULL,
    clob_token_ids JSONB NOT NULL,
    active BOOLEAN NOT NULL DEFAULT true,
    closed BOOLEAN NOT NULL DEFAULT false,
    last_mid_yes NUMERIC(20,8),
    last_mid_no NUMERIC(20,8),
    volume_24h NUMERIC(30,8),
    liquidity NUMERIC(30,8),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    raw_json JSONB
);

CREATE INDEX IF NOT EXISTS idx_markets_active ON market_data.markets(active) WHERE active = true;

-- Orderbook snapshots (per outcome token, from CLOB public /book)
CREATE TABLE IF NOT EXISTS market_data.orderbook_snapshots (
    id BIGSERIAL PRIMARY KEY,
    token_id TEXT NOT NULL,
    market_id TEXT REFERENCES market_data.markets(gamma_id),
    outcome TEXT,
    bids JSONB NOT NULL,           -- e.g. [{"price":"0.45","size":"12345.67"}, ...]
    asks JSONB NOT NULL,
    mid NUMERIC(20,8),
    spread NUMERIC(20,8),
    fetched_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_obs_token_fetched ON market_data.orderbook_snapshots(token_id, fetched_at DESC);

-- Paper orders (intents, decision context for later Hermes review)
CREATE TABLE IF NOT EXISTS paper_trading.paper_orders (
    id UUID PRIMARY KEY,
    market_id TEXT NOT NULL REFERENCES market_data.markets(gamma_id),
    outcome TEXT NOT NULL CHECK (outcome IN ('Yes', 'No')),
    side TEXT NOT NULL CHECK (side IN ('Buy', 'Sell')),
    order_type TEXT NOT NULL CHECK (order_type IN ('Market', 'Limit')),
    limit_price NUMERIC(20,8),
    size NUMERIC(30,8) NOT NULL CHECK (size > 0),
    status TEXT NOT NULL CHECK (status IN ('Open', 'PartiallyFilled', 'Filled', 'Cancelled', 'Rejected')),
    decision_context JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_paper_orders_market_created ON paper_trading.paper_orders(market_id, created_at DESC);

-- Paper fills (executions against simulated book)
CREATE TABLE IF NOT EXISTS paper_trading.paper_fills (
    id UUID PRIMARY KEY,
    order_id UUID NOT NULL REFERENCES paper_trading.paper_orders(id),
    price NUMERIC(20,8) NOT NULL,
    size NUMERIC(30,8) NOT NULL,
    fee NUMERIC(30,8) NOT NULL DEFAULT 0,
    slippage_bps INTEGER NOT NULL DEFAULT 0,
    against_book JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_paper_fills_order ON paper_trading.paper_fills(order_id);

-- Current positions (updated on fills; recomputable from fills but cached)
CREATE TABLE IF NOT EXISTS paper_trading.paper_positions (
    market_id TEXT NOT NULL REFERENCES market_data.markets(gamma_id),
    outcome TEXT NOT NULL CHECK (outcome IN ('Yes', 'No')),
    shares NUMERIC(30,8) NOT NULL DEFAULT 0,
    avg_entry_price NUMERIC(20,8) NOT NULL DEFAULT 0,
    collateral_locked NUMERIC(30,8) NOT NULL DEFAULT 0,
    last_updated TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (market_id, outcome)
);

-- Portfolio snapshots (history + current state; positions denormalized for audit)
CREATE TABLE IF NOT EXISTS paper_trading.virtual_portfolio_snapshots (
    id BIGSERIAL PRIMARY KEY,
    as_of TIMESTAMPTZ NOT NULL DEFAULT now(),
    virtual_usdc NUMERIC(30,8) NOT NULL,
    total_locked NUMERIC(30,8) NOT NULL DEFAULT 0,
    unrealized_pnl NUMERIC(30,8) NOT NULL DEFAULT 0,
    realized_pnl NUMERIC(30,8) NOT NULL DEFAULT 0,
    snapshot_reason TEXT NOT NULL,
    positions JSONB NOT NULL DEFAULT '[]'::jsonb
);

CREATE INDEX IF NOT EXISTS idx_vps_as_of ON paper_trading.virtual_portfolio_snapshots(as_of DESC);

-- Journal reflections (Hermes output)
CREATE TABLE IF NOT EXISTS journal.reflections (
    id UUID PRIMARY KEY,
    period_start TIMESTAMPTZ,
    period_end TIMESTAMPTZ,
    summary TEXT NOT NULL,
    metrics JSONB,
    recommendations JSONB,
    hermes_version TEXT,
    llm_model TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Seed comment for audit
COMMENT ON SCHEMA paper_trading IS 'All virtual/paper trading state. Never contains real wallet data.';
COMMENT ON TABLE paper_trading.paper_orders IS 'Every paper order intent is recorded here with decision_context for review. Paper-only.';
