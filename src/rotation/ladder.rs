//! Recurring-ladder detection (wiki/roadmap "Recurring-ladder detection", 2026-07-17 review #4).
//!
//! Some NegRisk events are WEEKLY recurring families — e.g. the Musk tweet-count ladder
//! (`elon-musk-of-tweets-<start>-<end>-<bucket>`) — where Gamma lists the NEXT period's markets
//! (a brand-new event id) days before the current period resolves (confirmed live 2026-07-21: the
//! july-21-to-28 event was already listed 3 days before the july-14-to-21 one closed). Today those
//! only enter our tracked universe once they organically rank into the volume-ranked arb-discovery
//! top-N (`ingester::ingest_tick` step 2) — which can take days, missing the early-book-
//! inefficiency window that makes newly-listed ladders the best-priced entry.
//!
//! This module predicts the next period's slugs from the currently-tracked (soon-to-resolve)
//! instance and writes them to `market_data.ladder_watchlist`; `ingest_tick`'s must-track query
//! UNIONs that table in (same mechanism as `directional_universe`), so predicted slugs get
//! fetched-by-slug and upserted as soon as Gamma lists them — independent of volume rank. A wrong
//! prediction is harmless: Gamma just returns nothing for an unlisted slug, and this only ever ADDS
//! candidates — the normal edge/Kelly/exposure gates still apply to anything tradeable that results.
//!
//! **Scope of this first cut:** only fixed-cadence date-range slugs (`<prefix>-<mon>-<day>-<mon>-
//! <day>-<suffix>`, next window = same span repeated) are handled — this covers the Musk ladder.
//! Variable-cadence families (e.g. per-FOMC-meeting Fed-rate ladders, which don't recur at a fixed
//! interval) are NOT predicted here; left as a follow-up per the roadmap entry.

use anyhow::Result;
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use sqlx::PgPool;
use std::collections::BTreeMap;

/// Same invariant as the negRisk arb scanner (`strategy::negrisk::MIN_MEMBERS`): a 2-member event
/// is just a binary market, not a ladder — not worth the prediction machinery.
const LADDER_MIN_MEMBERS: usize = 3;

const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

fn month_index(name: &str) -> Option<u32> {
    MONTHS.iter().position(|m| *m == name).map(|i| i as u32 + 1)
}

/// One parsed date-range ladder slug: `<prefix>-<mon1>-<day1>-<mon2>-<day2>-<suffix>`, e.g.
/// `"elon-musk-of-tweets-july-14-july-21-140-159"` → prefix `"elon-musk-of-tweets"`, mon1 `"july"`
/// day1 `14`, mon2 `"july"` day2 `21`, suffix `"140-159"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LadderSlugParts {
    pub prefix: String,
    pub mon1: String,
    pub day1: u32,
    pub mon2: String,
    pub day2: u32,
    pub suffix: String,
}

/// Find the first (leftmost) `month-day-month-day` token window in a hyphenated slug and split
/// around it. Leftmost-first is deliberate: a subject prefix is never empty for a real ladder slug
/// (there's always a "what is this event" phrase before the dates), so requiring a non-empty prefix
/// also rejects a slug that starts with a bare date range.
pub fn parse_date_range_slug(slug: &str) -> Option<LadderSlugParts> {
    let tokens: Vec<&str> = slug.split('-').collect();
    if tokens.len() < 5 {
        return None;
    }
    for i in 1..tokens.len().saturating_sub(3) {
        let (t0, t1, t2, t3) = (tokens[i], tokens[i + 1], tokens[i + 2], tokens[i + 3]);
        let (Some(_), Some(d1), Some(_), Some(d2)) = (
            month_index(t0),
            t1.parse::<u32>().ok().filter(|d| (1..=31).contains(d)),
            month_index(t2),
            t3.parse::<u32>().ok().filter(|d| (1..=31).contains(d)),
        ) else {
            continue;
        };
        return Some(LadderSlugParts {
            prefix: tokens[..i].join("-"),
            mon1: t0.to_string(),
            day1: d1,
            mon2: t2.to_string(),
            day2: d2,
            suffix: tokens[i + 4..].join("-"),
        });
    }
    None
}

/// Resolve the (year-ambiguous — slugs carry no year) two calendar dates a parsed slug's date-range
/// embeds, anchored to the member's own year-bearing `end_date` (which is date2, the window close).
fn resolve_window_dates(
    parts: &LadderSlugParts,
    end_date: DateTime<Utc>,
) -> Option<(NaiveDate, NaiveDate)> {
    let mon1 = month_index(&parts.mon1)?;
    let mon2 = month_index(&parts.mon2)?;
    let year2 = end_date.year();
    let date2 = NaiveDate::from_ymd_opt(year2, mon2, parts.day2)?;
    // mon1 > mon2 numerically ⇒ the range wraps a year boundary (e.g. december → january).
    let year1 = if mon1 > mon2 { year2 - 1 } else { year2 };
    let date1 = NaiveDate::from_ymd_opt(year1, mon1, parts.day1)?;
    if date1 >= date2 {
        return None; // sanity: a window must have positive span
    }
    Some((date1, date2))
}

/// Predict the NEXT period's `(mon1, day1, mon2, day2)` by shifting the window forward by its own
/// span — true for the observed weekly Musk ladder (fixed cadence = window length). See the module
/// doc comment for why variable-cadence families are out of scope.
pub fn next_ladder_window(
    parts: &LadderSlugParts,
    end_date: DateTime<Utc>,
) -> Option<(String, u32, String, u32)> {
    let (date1, date2) = resolve_window_dates(parts, end_date)?;
    let span = date2 - date1;
    let next_date1 = date2;
    let next_date2 = date2 + span;
    Some((
        MONTHS[(next_date1.month() - 1) as usize].to_string(),
        next_date1.day(),
        MONTHS[(next_date2.month() - 1) as usize].to_string(),
        next_date2.day(),
    ))
}

/// Stats from one detection pass (journaled for observability, mirroring `RotationStats`).
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct LadderStats {
    /// NegRisk families (≥3 active members, share an event id) inspected.
    pub families_checked: usize,
    /// Families near resolution whose next-period slugs were parsed + predicted.
    pub families_extended: usize,
    /// Predicted slugs newly inserted into the watchlist (dedup via ON CONFLICT DO NOTHING).
    pub slugs_added: usize,
}

/// Scan active negRisk events nearing resolution (`end_date` within `lookahead_days`); for any
/// whose members are a consistent date-range ladder family, predict + watchlist the next period's
/// slugs so `ingest_tick` force-tracks them ahead of volume-based discovery.
pub async fn detect_and_extend_ladders(pool: &PgPool, lookahead_days: i64) -> Result<LadderStats> {
    let mut stats = LadderStats::default();

    let rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT slug, event_id, raw_json->>'end_date'
         FROM market_data.markets
         WHERE neg_risk AND active AND NOT closed AND event_id IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;

    let mut by_event: BTreeMap<String, Vec<(String, Option<DateTime<Utc>>)>> = BTreeMap::new();
    for (slug, event_id, end_date_raw) in rows {
        let Some(event_id) = event_id else { continue };
        let end_date = end_date_raw
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc));
        by_event.entry(event_id).or_default().push((slug, end_date));
    }

    let horizon = Utc::now() + Duration::days(lookahead_days);

    for (event_id, members) in by_event {
        if members.len() < LADDER_MIN_MEMBERS {
            continue;
        }
        let Some(end_date) = members.iter().filter_map(|(_, d)| *d).max() else {
            continue;
        };
        if end_date > horizon {
            continue; // not yet near resolution; nothing to extend yet
        }
        stats.families_checked += 1;

        let parsed: Option<Vec<(String, LadderSlugParts)>> = members
            .iter()
            .map(|(slug, _)| parse_date_range_slug(slug).map(|p| (slug.clone(), p)))
            .collect();
        let Some(parsed) = parsed else {
            continue; // not every member parses as a date-range ladder slug — skip
        };
        let first = &parsed[0].1;
        let consistent = parsed
            .iter()
            .all(|(_, p)| p.prefix == first.prefix && p.mon2 == first.mon2 && p.day2 == first.day2);
        if !consistent {
            continue; // mixed family — bail conservatively rather than guess wrong
        }
        let Some((mon1, day1, mon2, day2)) = next_ladder_window(first, end_date) else {
            continue;
        };

        let mut any_added = false;
        for (_, parts) in &parsed {
            let next_slug = if parts.suffix.is_empty() {
                format!("{}-{}-{}-{}-{}", parts.prefix, mon1, day1, mon2, day2)
            } else {
                format!(
                    "{}-{}-{}-{}-{}-{}",
                    parts.prefix, mon1, day1, mon2, day2, parts.suffix
                )
            };
            let inserted = sqlx::query(
                "INSERT INTO market_data.ladder_watchlist (slug, family_prefix, source_event_id)
                 VALUES ($1, $2, $3) ON CONFLICT (slug) DO NOTHING",
            )
            .bind(&next_slug)
            .bind(&first.prefix)
            .bind(&event_id)
            .execute(pool)
            .await?
            .rows_affected();
            if inserted > 0 {
                stats.slugs_added += 1;
                any_added = true;
                tracing::info!(slug = %next_slug, family = %first.prefix, source_event = %event_id,
                    "ladder: predicted + watchlisted next-period slug");
            }
        }
        if any_added {
            stats.families_extended += 1;
        }
    }

    Ok(stats)
}

/// Drop watchlist rows that are no longer useful: either Gamma never listed the predicted slug
/// (bad guess — stale after `stale_days`) or the market showed up and has since closed/resolved
/// (job done; `ingest_tick`'s held-position UNION covers settlement from here). Keeps the table
/// small instead of growing unbounded across weeks of ladder cycles.
pub async fn prune_stale_ladder_watchlist(pool: &PgPool, stale_days: i64) -> Result<u64> {
    let n = sqlx::query(
        "DELETE FROM market_data.ladder_watchlist w
         WHERE (w.added_at < now() - ($1 || ' days')::interval
                AND NOT EXISTS (SELECT 1 FROM market_data.markets m WHERE m.slug = w.slug))
            OR EXISTS (SELECT 1 FROM market_data.markets m
                        WHERE m.slug = w.slug AND (m.closed OR m.resolved_outcome IS NOT NULL))",
    )
    .bind(stale_days.to_string())
    .execute(pool)
    .await?
    .rows_affected();
    if n > 0 {
        tracing::info!(pruned = n, "ladder: pruned stale/resolved watchlist rows");
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn parses_the_live_musk_ladder_slug() {
        let p = parse_date_range_slug("elon-musk-of-tweets-july-14-july-21-140-159").unwrap();
        assert_eq!(p.prefix, "elon-musk-of-tweets");
        assert_eq!(p.mon1, "july");
        assert_eq!(p.day1, 14);
        assert_eq!(p.mon2, "july");
        assert_eq!(p.day2, 21);
        assert_eq!(p.suffix, "140-159");
    }

    #[test]
    fn parses_slug_with_no_trailing_suffix() {
        let p = parse_date_range_slug("some-event-march-1-march-8").unwrap();
        assert_eq!(p.prefix, "some-event");
        assert_eq!(p.suffix, "");
    }

    #[test]
    fn rejects_a_bare_date_range_with_no_subject_prefix() {
        // A real ladder slug always has a subject before the dates; a leading date range is
        // ambiguous (nothing to anchor the family on) and must be rejected, not guessed at.
        assert!(parse_date_range_slug("july-14-july-21-140-159").is_none());
    }

    #[test]
    fn rejects_slugs_with_no_date_range() {
        assert!(parse_date_range_slug("will-bitcoin-reach-90000-by-december-31-2026").is_none());
    }

    #[test]
    fn predicts_next_weekly_window_matching_live_gamma_data() {
        // Confirmed live 2026-07-21: july-14-to-21 (closing) and july-21-to-28 (already listed).
        let p = parse_date_range_slug("elon-musk-of-tweets-july-14-july-21-140-159").unwrap();
        let end_date = Utc.with_ymd_and_hms(2026, 7, 21, 16, 0, 0).unwrap();
        let (mon1, day1, mon2, day2) = next_ladder_window(&p, end_date).unwrap();
        assert_eq!(
            (mon1.as_str(), day1, mon2.as_str(), day2),
            ("july", 21, "july", 28)
        );
    }

    #[test]
    fn predicts_across_a_year_boundary() {
        let p = parse_date_range_slug("some-ladder-december-29-january-5-bucket").unwrap();
        let end_date = Utc.with_ymd_and_hms(2027, 1, 5, 16, 0, 0).unwrap();
        let (mon1, day1, mon2, day2) = next_ladder_window(&p, end_date).unwrap();
        assert_eq!(
            (mon1.as_str(), day1, mon2.as_str(), day2),
            ("january", 5, "january", 12)
        );
    }

    #[test]
    fn rejects_non_monotonic_window() {
        // day2 before day1 within the same claimed month pair — not a valid forward window.
        let p = LadderSlugParts {
            prefix: "x".into(),
            mon1: "july".into(),
            day1: 21,
            mon2: "july".into(),
            day2: 14,
            suffix: "".into(),
        };
        let end_date = Utc.with_ymd_and_hms(2026, 7, 14, 16, 0, 0).unwrap();
        assert!(next_ladder_window(&p, end_date).is_none());
    }
}
