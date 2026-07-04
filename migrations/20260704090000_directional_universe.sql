-- Directional-eligible market rotation (see wiki/roadmap "Automated market rotation").
--
-- The hand-curated POLYTRADER_BOOTSTRAP_MARKETS allowlist decays: its short-dated markets resolve
-- (the June 2026 Iran/Hormuz set) and the survivors are multi-year horizons that never clear the
-- edge gate, so directional trading grinds to a halt between manual refreshes. This table is the
-- DB-backed, self-refreshing complement: a periodic rotation job promotes active, order-book,
-- binary, non-sports markets resolving within a bounded window (fresh + short-dated = the profile
-- the June set proved out), and demotes them when they close/resolve.
--
-- Active row = demoted_at IS NULL. The directional executor treats (bootstrap env ∪ active rows)
-- as the ONLY directional-eligible universe; everything else stays arb-only.
-- Created via sqlx migrate (app role) so it is owned by `polytrader`, not `postgres` — the
-- out-of-band-ownership permission bug from 2026-07-02 must not recur.
CREATE TABLE IF NOT EXISTS market_data.directional_universe (
    slug        text PRIMARY KEY,
    gamma_id    text NOT NULL,
    promoted_at timestamptz NOT NULL DEFAULT now(),
    demoted_at  timestamptz,
    -- Snapshot of the promotion rationale (for audit/Hermes; not refreshed after promotion).
    end_date    timestamptz,
    volume_24hr numeric(20, 2),
    source      text NOT NULL DEFAULT 'rotation'
);

CREATE INDEX IF NOT EXISTS directional_universe_active_idx
    ON market_data.directional_universe (slug) WHERE demoted_at IS NULL;
