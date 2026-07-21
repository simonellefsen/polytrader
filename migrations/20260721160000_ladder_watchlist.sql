-- Recurring-ladder detection (see wiki/roadmap "Recurring-ladder detection", 2026-07-17 review #4).
--
-- NegRisk ladder families (e.g. the weekly "elon-musk-of-tweets-<start>-<end>-<bucket>" tweet-count
-- event) get a brand-new Gamma event/slug set for each period, days before the prior period
-- resolves. Today those only enter our tracked universe once they organically rank into the
-- volume-ranked arb-discovery top-N (`ingest_tick`'s step 2) — which can take days after listing,
-- missing the early-book-inefficiency window the roadmap flagged as the opportunistic gap.
--
-- This table is the force-track complement: a periodic detector (src/rotation/ladder.rs) predicts
-- the next period's slugs from the currently-tracked (soon-to-resolve) instance and inserts them
-- here; `ingest_tick`'s must-track query (like `directional_universe`) UNIONs this in, so predicted
-- slugs get fetched-by-slug and upserted into `market_data.markets` as soon as Gamma lists them —
-- independent of volume rank. A wrong prediction is harmless (Gamma just returns nothing for an
-- unlisted slug); this only ever ADDS candidates, the normal edge/Kelly/exposure gates still apply
-- to anything that ends up tradeable.
-- Created via sqlx migrate (app role) so it is owned by `polytrader`, not `postgres` (the
-- out-of-band-ownership permission bug from 2026-07-02 must not recur).
CREATE TABLE IF NOT EXISTS market_data.ladder_watchlist (
    slug             text PRIMARY KEY,
    family_prefix    text NOT NULL,
    source_event_id  text,
    added_at         timestamptz NOT NULL DEFAULT now()
);
