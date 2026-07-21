//! Directional market rotation.
//!
//! The hand-curated bootstrap allowlist decays (its short-dated markets resolve, the survivors are
//! multi-year horizons that never move enough to clear the edge gate), which stalls directional
//! trading between manual refreshes. This job keeps `market_data.directional_universe` fresh:
//!
//!  - **Demote** active rows whose market has closed/resolved (or whose end date has passed).
//!  - **Promote** up to the configured cap: active, order-book, binary, NON-sports markets ending
//!    within `POLYTRADER_ROTATION_MAX_DAYS`, above the `POLYTRADER_ROTATION_MIN_VOL24H` liquidity
//!    floor, ranked by 24h volume.
//!
//! The directional executor treats (bootstrap env ∪ active rows here) as the only
//! directional-eligible universe; every other market stays arb-only. Paper-only throughout.

use anyhow::Result;
use rust_decimal::Decimal;
use sqlx::PgPool;

pub mod ladder; // recurring-ladder (weekly NegRisk family) next-period slug prediction

use crate::ingester::GammaClient;

/// Row counts from one rotation pass (journaled for observability).
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct RotationStats {
    pub demoted: u64,
    pub promoted: u64,
    pub active_after: i64,
    pub candidates_usable: usize,
    pub cap: i64,
}

fn env_i64(key: &str, default: i64) -> i64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// One rotation pass: demote dead rows, then top up to the cap from Gamma's short-dated pool.
/// `is_arb_only` is the slug classifier (sports/etc. must never become directional-eligible).
pub async fn run_rotation(
    pool: &PgPool,
    gamma: &GammaClient,
    is_arb_only: impl Fn(&str) -> bool,
) -> Result<RotationStats> {
    let cap = env_i64("POLYTRADER_ROTATION_LIMIT", 0);
    let max_days = env_i64("POLYTRADER_ROTATION_MAX_DAYS", 30);
    let min_hours = env_i64("POLYTRADER_ROTATION_MIN_HOURS", 12);
    let min_vol = Decimal::from(env_i64("POLYTRADER_ROTATION_MIN_VOL24H", 5_000));
    let mut stats = RotationStats {
        cap,
        ..Default::default()
    };
    if cap <= 0 {
        // Rotation disabled — still demote so a previously-promoted set can drain cleanly.
        stats.demoted = demote_dead(pool).await?;
        return Ok(stats);
    }

    stats.demoted = demote_dead(pool).await?;

    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM market_data.directional_universe WHERE demoted_at IS NULL",
    )
    .fetch_one(pool)
    .await?;
    let room = (cap - active).max(0) as usize;

    if room > 0 {
        // Paginated fetch: page 0 of the short-dated volume ranking is a wall of sports matches
        // (~5 non-sports of 100, measured 2026-07-05); the real directional candidates (Fed
        // brackets, BTC weeklies, Iran deadlines …) live on pages 1–4.
        let candidates = gamma.discover_directional_markets(5, max_days).await?;
        stats.candidates_usable = candidates.len();
        let horizon = chrono::Utc::now() + chrono::Duration::days(max_days);
        // NOT pre-truncated to `room`: the tag gate below rejects candidates one by one, and a
        // pre-truncated iterator would let tag-rejects (e.g. a page of keyword-dodging sports
        // slugs) burn promotion slots. Instead iterate the whole filtered ranking and stop once
        // `room` promotions actually landed.
        let picks = candidates
            .into_iter()
            .filter(|m| !m.slug.is_empty() && !is_arb_only(&m.slug))
            .filter(|m| m.volume_24hr.unwrap_or(Decimal::ZERO) >= min_vol)
            // Belt-and-suspenders re-check of the Gamma end_date_min/max query params: a market
            // with no parseable end date, or one outside the window, must never be promoted (the
            // whole point is short-dated), even if the API ignores the params. The LOWER bound is
            // a real floor, not just "now": Polymarket runs perpetual ultra-fast series
            // (btc-updown-5m-*, hourly crypto binaries) that rank high on volume and would waste a
            // promotion slot every pass — one was promoted 2026-07-05 with 5 SECONDS to expiry.
            // Days-to-weeks is the target profile; sub-`min_hours` markets can't even complete one
            // ingest+DR cycle.
            .filter(|m| {
                let min_end = chrono::Utc::now() + chrono::Duration::hours(min_hours);
                m.end_date
                    .as_deref()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| {
                        let d = d.with_timezone(&chrono::Utc);
                        d > min_end && d <= horizon
                    })
                    .unwrap_or(false)
            });
        for m in picks {
            if stats.promoted as usize >= room {
                break;
            }
            // TAG GATE (the structural fix for slug-keyword whack-a-mole): before promoting,
            // check the parent EVENT's tags — Polymarket's own taxonomy. Sports/esports must
            // never be directional-eligible, and new slug formats keep dodging the keyword
            // classifier (wta-/cs2-/val- on 07-04, will-t1-win-msi + cricmlc- on 07-05, each a
            // real leaked promotion). FAIL CLOSED: no event id or a failed tags fetch ⇒ no
            // promotion this pass (retried next rotation).
            let Some(event_id) = m.event_id.as_deref() else {
                tracing::debug!(slug = %m.slug, "rotation: no event id; skipping (fail-closed)");
                continue;
            };
            match gamma.event_tags(event_id).await {
                Ok(tags) => {
                    if tags.iter().any(|t| {
                        t.eq_ignore_ascii_case("sports") || t.eq_ignore_ascii_case("esports")
                    }) {
                        tracing::info!(slug = %m.slug, ?tags,
                            "rotation: rejected by event tag gate (sports/esports stay arb-only)");
                        continue;
                    }
                }
                Err(e) => {
                    tracing::warn!(slug = %m.slug, error = %e,
                        "rotation: event tags fetch failed; skipping promotion (fail-closed)");
                    continue;
                }
            }
            let end_ts = m
                .end_date
                .as_deref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&chrono::Utc));
            // Insert-only: a slug that was demoted (resolved) stays demoted — resolution is final.
            let inserted = sqlx::query(
                "INSERT INTO market_data.directional_universe
                     (slug, gamma_id, end_date, volume_24hr, source)
                 VALUES ($1, $2, $3, $4, 'rotation')
                 ON CONFLICT (slug) DO NOTHING",
            )
            .bind(&m.slug)
            .bind(&m.id)
            .bind(end_ts)
            .bind(m.volume_24hr)
            .execute(pool)
            .await?
            .rows_affected();
            if inserted > 0 {
                stats.promoted += 1;
                tracing::info!(slug = %m.slug, vol24 = ?m.volume_24hr, end = ?m.end_date,
                    "rotation: promoted to directional universe");
            }
        }
    }

    stats.active_after = sqlx::query_scalar(
        "SELECT count(*) FROM market_data.directional_universe WHERE demoted_at IS NULL",
    )
    .fetch_one(pool)
    .await?;
    Ok(stats)
}

/// Demote active rows whose market is closed/resolved in our DB or whose end date has passed.
/// (A just-promoted market may not be in market_data.markets until the next ingest tick — the
/// LEFT JOIN keeps those alive.)
async fn demote_dead(pool: &PgPool) -> Result<u64> {
    let n = sqlx::query(
        "UPDATE market_data.directional_universe du SET demoted_at = now()
         WHERE du.demoted_at IS NULL
           AND (du.end_date < now()
                OR EXISTS (SELECT 1 FROM market_data.markets m
                            WHERE m.slug = du.slug
                              AND (m.closed OR m.resolved_outcome IS NOT NULL)))",
    )
    .execute(pool)
    .await?
    .rows_affected();
    if n > 0 {
        tracing::info!(
            demoted = n,
            "rotation: demoted closed/resolved/expired markets"
        );
    }
    Ok(n)
}

/// Is this slug currently directional-eligible via rotation? (The executor's hard gate unions this
/// with the bootstrap env allowlist. The ingester and the DR generator query the table directly
/// in SQL — see ingest_tick's must-track UNION and produce_5min_decision_report's ORDER BY.)
pub async fn is_active(pool: &PgPool, slug: &str) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM market_data.directional_universe
                        WHERE slug = $1 AND demoted_at IS NULL)",
    )
    .bind(slug)
    .fetch_one(pool)
    .await
    .unwrap_or(false)
}
