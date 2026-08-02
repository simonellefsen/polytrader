//! Maker liquidity-rewards scanner — SHADOW ONLY, no orders, no execution.
//!
//! Polymarket pays a per-market daily budget to orders that *rest* in the book, whether or not they
//! fill (`clobRewards[].rewardsDailyRate`, parsed 2026-08-02). Qualification is published per
//! market: at least `rewards_min_size` shares, resting within `rewards_max_spread` cents of the
//! midpoint. Makers also pay no fee on this venue (`takerOnly: true` on every fee schedule
//! sampled), so the gross reward is the whole gross return.
//!
//! ## Why this is a scanner and not a strategy
//!
//! The repo rule is harness-first: falsify cheaply before building. This module measures what we
//! *would* capture and journals it. It places nothing. Two things must be true before any of it
//! becomes a strategy, and neither is established yet:
//!
//! 1. **The share model here is first-order.** It assumes reward share ≈
//!    `our_size / (qualifying_depth + our_size)`. Polymarket's real formula scores orders by
//!    proximity to the midpoint and generally rewards two-sided quoting, so a one-sided resting
//!    order can earn materially less than this implies. Treat every number as an **upper bound**.
//! 2. **Adverse selection is not modelled at all, and it is the entire risk of making.** A resting
//!    order that earns rewards is an order that can be hit — and it is hit precisely when the
//!    market is moving against it. The reward is compensation for that, not free money. Nothing
//!    here nets the reward against expected fill P&L, so a positive number in this scan is NOT a
//!    positive expectancy claim.
//!
//! ## The finding that motivated it
//!
//! Measured live on 2026-08-02, the ranking by *pool size* is close to the inverse of the ranking
//! by *capturable share* — big pools attract deep competing liquidity:
//!
//! | market | pool $/day | qualifying depth | our share | est $/day |
//! |---|---|---|---|---|
//! | US invade Iran | 1000 | 1,941,491 | 0.010% | 0.10 |
//! | Spider-Man box office | 200 | 4,357 | 2.24% | **4.49** |
//!
//! So "chase the biggest reward" is an actively wrong heuristic, and the scan exists to rank by the
//! thing that actually pays.

use anyhow::Result;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::ingester::clob_ws::LiveBookStore;

/// A market whose reward programme we could plausibly qualify in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewardCandidate {
    pub market_id: String,
    pub question: String,
    /// Daily reward budget for the whole market (USD), shared across all qualifying makers.
    pub daily_rate: Decimal,
    /// Minimum resting size (shares) to qualify.
    pub min_size: Decimal,
    /// Max distance from the midpoint, in PRICE (already converted from Gamma's cents).
    pub max_spread: Decimal,
    pub mid: Decimal,
    /// Total resting size already inside the qualifying band, both sides. Our competition.
    pub qualifying_depth: Decimal,
    /// `min_size / (qualifying_depth + min_size)` — the first-order share estimate.
    pub estimated_share: Decimal,
    /// `daily_rate × estimated_share`, the headline number. An UPPER BOUND (see module docs).
    pub estimated_daily_usd: Decimal,
    /// Capital tied up posting `min_size` at `mid` (USD).
    pub capital_usd: Decimal,
    /// True when the book came from the live WS feed rather than a polled snapshot.
    pub from_live_book: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RewardDiagnostics {
    /// Active markets advertising a reward programme.
    pub markets_with_rewards: usize,
    /// …of which we could price (a fresh book on at least one side).
    pub markets_priced: usize,
    /// Sum of every advertised daily budget (USD) — the POOL, not our share.
    pub pool_usd_day: Decimal,
    /// Sum of `estimated_daily_usd` across candidates. Upper bound.
    pub estimated_capturable_usd_day: Decimal,
    /// Capital required to post `min_size` in every candidate.
    pub capital_required_usd: Decimal,
    /// Legs priced from the live WS book vs the polled snapshot.
    pub priced_from_live: usize,
    pub priced_from_snapshot: usize,
    /// Markets excluded because NOTHING is resting inside the reward band.
    ///
    /// These are not opportunities, they are unreadable books. See `scan_rewards` for why they are
    /// excluded rather than counted as a 100% share.
    pub zero_depth_excluded: usize,
}

/// Resting size within `band` of `mid`, summed across both sides.
///
/// Both sides count because the reward programmes we sampled qualify orders on either side of the
/// book; a one-sided read would halve the apparent competition and double the apparent share.
pub(crate) fn qualifying_depth(
    bids: &[(Decimal, Decimal)],
    asks: &[(Decimal, Decimal)],
    mid: Decimal,
    band: Decimal,
) -> Decimal {
    bids.iter()
        .chain(asks.iter())
        .filter(|(price, _)| (*price - mid).abs() <= band)
        .map(|(_, size)| *size)
        .sum()
}

/// First-order share of a reward pool for `our_size` against `competing_depth`.
///
/// Deliberately includes our own size in the denominator: posting size *dilutes the pool we are
/// joining*, so `our/(theirs + ours)` is right and `our/theirs` would overstate — unboundedly so in
/// a thin book, which is exactly where this scan finds its best candidates.
pub(crate) fn estimated_share(our_size: Decimal, competing_depth: Decimal) -> Decimal {
    let total = competing_depth + our_size;
    if total <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    our_size / total
}

/// Rank best-capturable first — NOT by pool size, which is close to the inverse (see module docs).
pub(crate) fn rank(mut candidates: Vec<RewardCandidate>) -> Vec<RewardCandidate> {
    candidates.sort_by(|a, b| {
        b.estimated_daily_usd
            .cmp(&a.estimated_daily_usd)
            .then_with(|| a.market_id.cmp(&b.market_id))
    });
    candidates
}

type RewardRow = (
    String,
    String,
    Decimal,
    Option<Decimal>,
    Option<Decimal>,
    String,
    serde_json::Value,
    serde_json::Value,
    Option<Decimal>,
);

/// Scan every active reward-paying market and estimate what we could capture.
///
/// Prices from the live WS book where one is fresh and in sync, falling back to the polled
/// snapshot — the same precedence the negRisk scanner uses.
pub async fn scan_rewards(
    pool: &PgPool,
    books: Option<&LiveBookStore>,
) -> Result<(Vec<RewardCandidate>, RewardDiagnostics)> {
    let rows: Vec<RewardRow> = sqlx::query_as(
        r#"
        SELECT m.gamma_id, m.question, m.rewards_daily_rate, m.rewards_min_size,
               m.rewards_max_spread, snap.token_id, snap.bids, snap.asks, snap.mid
        FROM market_data.markets m
        JOIN LATERAL (
            SELECT token_id, bids, asks, mid FROM market_data.orderbook_snapshots
            WHERE market_id = m.gamma_id AND outcome = 'Yes'
              AND fetched_at > now() - interval '30 minutes'
            ORDER BY fetched_at DESC LIMIT 1
        ) snap ON true
        WHERE m.active AND NOT m.closed AND m.rewards_daily_rate IS NOT NULL
        "#,
    )
    .fetch_all(pool)
    .await?;

    let total_with_rewards: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM market_data.markets
         WHERE active AND NOT closed AND rewards_daily_rate IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(0);

    let mut diag = RewardDiagnostics {
        markets_with_rewards: total_with_rewards as usize,
        ..Default::default()
    };
    let mut out = Vec::new();

    for (
        market_id,
        question,
        rate,
        min_size,
        max_spread_cents,
        token_id,
        bids_j,
        asks_j,
        snap_mid,
    ) in rows
    {
        // A programme with no stated qualification rule cannot be reasoned about; skip rather than
        // invent a default, since the whole point is to model the published rule.
        let (Some(min_size), Some(max_spread_cents)) = (min_size, max_spread_cents) else {
            continue;
        };
        let max_spread = max_spread_cents / Decimal::from(100); // Gamma states this in cents

        let live = books.and_then(|b| b.get_fresh(&token_id));
        let (bids, asks, from_live) = match &live {
            Some(book) => (book.bid_levels(), book.ask_levels(), true),
            None => (parse_levels(&bids_j), parse_levels(&asks_j), false),
        };

        let mid = match live.as_ref().and_then(|b| b.mid()).or(snap_mid) {
            Some(m) if m > Decimal::ZERO => m,
            _ => continue,
        };

        let qualifying_depth = qualifying_depth(&bids, &asks, mid, max_spread);

        // Nothing resting inside the reward band. Arithmetically our share would be 100% and we
        // would book the ENTIRE daily budget — and the first 8 hours of live data showed exactly
        // that: 12 of 94 scans hit this, contributing an average $191.67, i.e. **35.8% of the
        // headline estimate**, almost all of it one market repeatedly reading zero.
        //
        // Treat it as unreadable rather than free. A book with no size within a few cents of its
        // own midpoint is pathologically wide or momentarily empty, not a market we could rest
        // 200 shares in and collect a pool from — and if it genuinely were, other makers would
        // arrive the instant it paid. This is the same "the error lands hardest on the markets the
        // scan recommends" failure the module guards against for `our/theirs`; the zero-denominator
        // case needed guarding too.
        if qualifying_depth <= Decimal::ZERO {
            diag.zero_depth_excluded += 1;
            continue;
        }

        let share = estimated_share(min_size, qualifying_depth);

        diag.markets_priced += 1;
        if from_live {
            diag.priced_from_live += 1;
        } else {
            diag.priced_from_snapshot += 1;
        }

        out.push(RewardCandidate {
            market_id,
            question,
            daily_rate: rate,
            min_size,
            max_spread,
            mid,
            qualifying_depth,
            estimated_share: share.round_dp(6),
            estimated_daily_usd: (rate * share).round_dp(4),
            capital_usd: (min_size * mid).round_dp(2),
            from_live_book: from_live,
        });
    }

    diag.pool_usd_day = out.iter().map(|c| c.daily_rate).sum();
    diag.estimated_capturable_usd_day = out
        .iter()
        .map(|c| c.estimated_daily_usd)
        .sum::<Decimal>()
        .round_dp(2);
    diag.capital_required_usd = out
        .iter()
        .map(|c| c.capital_usd)
        .sum::<Decimal>()
        .round_dp(2);

    Ok((rank(out), diag))
}

fn parse_levels(v: &serde_json::Value) -> Vec<(Decimal, Decimal)> {
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|l| {
                    let p = l.get("price")?.as_str()?.parse::<Decimal>().ok()?;
                    let s = l.get("size")?.as_str()?.parse::<Decimal>().ok()?;
                    Some((p, s))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn cand(id: &str, rate: Decimal, daily: Decimal) -> RewardCandidate {
        RewardCandidate {
            market_id: id.into(),
            question: id.into(),
            daily_rate: rate,
            min_size: dec!(200),
            max_spread: dec!(0.035),
            mid: dec!(0.5),
            qualifying_depth: dec!(0),
            estimated_share: dec!(0),
            estimated_daily_usd: daily,
            capital_usd: dec!(100),
            from_live_book: true,
        }
    }

    #[test]
    fn only_size_inside_the_band_competes_for_the_pool() {
        // Qualification is published as "within N cents of the midpoint", so depth outside the band
        // is not competition — counting the whole book would understate our share on wide markets.
        let bids = vec![(dec!(0.49), dec!(100)), (dec!(0.40), dec!(9999))];
        let asks = vec![(dec!(0.51), dec!(150)), (dec!(0.60), dec!(9999))];
        let depth = qualifying_depth(&bids, &asks, dec!(0.50), dec!(0.035));
        assert_eq!(depth, dec!(250), "the two far levels must not count");
    }

    #[test]
    fn the_band_is_inclusive_at_its_edge() {
        let bids = vec![(dec!(0.465), dec!(10))];
        assert_eq!(
            qualifying_depth(&bids, &[], dec!(0.50), dec!(0.035)),
            dec!(10)
        );
    }

    #[test]
    fn posting_size_dilutes_the_pool_we_are_joining() {
        // our/(theirs+ours), never our/theirs — the latter is unbounded as a book thins, which is
        // exactly where this scan finds its best candidates, so the error would land hardest
        // precisely on the markets it recommends.
        assert_eq!(estimated_share(dec!(200), dec!(1800)), dec!(0.1));
        assert_eq!(estimated_share(dec!(0), dec!(0)), dec!(0));
    }

    #[test]
    fn a_book_with_nothing_in_the_band_is_excluded_not_scored_as_a_full_pool() {
        // The arithmetic says share = 200/(0+200) = 100%, i.e. we book the whole daily budget.
        // Live data called that out as nonsense: 12 of 94 scans in the first 8 hours hit a
        // zero-depth book and contributed 35.8% of the headline estimate off it. `scan_rewards`
        // now skips these into `zero_depth_excluded`, so this test pins the arithmetic that
        // MOTIVATES the skip rather than the skip itself — if this ever stops returning 1, the
        // guard in the scanner is no longer load-bearing and should be re-derived, not deleted.
        assert_eq!(estimated_share(dec!(200), dec!(0)), dec!(1));
        assert_eq!(
            qualifying_depth(&[(dec!(0.10), dec!(500))], &[], dec!(0.50), dec!(0.035)),
            dec!(0),
            "a book whose only size sits 40c from the mid has zero QUALIFYING depth"
        );
    }

    #[test]
    fn a_deep_book_makes_a_big_pool_worth_less_than_a_small_one() {
        // The live 2026-08-02 finding, as a test: ranking by pool size is close to the INVERSE of
        // ranking by what is capturable, so the scanner must not sort by daily_rate.
        let big_pool_deep = estimated_share(dec!(200), dec!(1941491)) * dec!(1000);
        let small_pool_thin = estimated_share(dec!(100), dec!(4357)) * dec!(200);
        assert!(
            small_pool_thin > big_pool_deep,
            "$200/day in a thin book ({small_pool_thin}) should beat $1000/day in a deep one ({big_pool_deep})"
        );
    }

    #[test]
    fn ranking_is_by_capturable_not_by_pool_size() {
        let ranked = rank(vec![
            cand("huge_pool", dec!(1000), dec!(0.10)),
            cand("small_pool", dec!(200), dec!(4.49)),
            cand("mid_pool", dec!(500), dec!(1.06)),
        ]);
        let order: Vec<&str> = ranked.iter().map(|c| c.market_id.as_str()).collect();
        assert_eq!(order, vec!["small_pool", "mid_pool", "huge_pool"]);
    }
}
