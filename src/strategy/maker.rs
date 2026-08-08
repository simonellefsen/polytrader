//! Shadow maker quotes — P5 increment 3b. TRACKS virtual quotes over time; places NOTHING.
//!
//! [`crate::strategy::rewards`] answers "what share of this reward pool could we capture right
//! now?". This module answers the two questions that one structurally cannot, because both are
//! about a specific price surviving over hours rather than about one instant:
//!
//! 1. **Does a quote keep qualifying?** The scanner multiplies an instantaneous share by a full
//!    daily rate. If the midpoint drifts out from under our price after twenty minutes, the real
//!    capture is a small fraction of the headline. `qualifying_seconds / age` is the duty cycle the
//!    scanner assumes is 100%.
//!
//! 2. **What does resting cost?** A resting order is filled exactly when the market is moving
//!    against it. The reward is compensation for that, not a bonus on top of it. The rewards module
//!    says so directly — "adverse selection is not modelled at all, and it is the entire risk of
//!    making" — and until it is, a positive capture estimate is not an expectancy claim.
//!
//! ## The measurement, and why it is done at a horizon
//!
//! A quote is considered filled when the midpoint crosses it: a bid at 0.60 fills once the mid
//! reaches 0.60 or below. Measuring P&L at that instant would be **circular** — the trigger *is*
//! the mid crossing our price, so a loss at t=0 is guaranteed by construction and we would be
//! reporting our own trigger condition back to ourselves as a finding.
//!
//! So P&L is measured one horizon later instead. That lets the move revert, and mean reversion is
//! precisely what a maker is paid for; a bid filled on a downtick that recovers is a WIN, and the
//! horizon is what allows that outcome to be observed at all. The sign of `horizon_pnl_usd` is
//! therefore a real result rather than an artifact of the trigger.
//!
//! ## This tracker does NOT re-quote, and that bounds every number it produces
//!
//! A real maker re-prices when the midpoint moves. This one places a quote and lets it drift:
//! nothing ever moves a resting price back inside the band. Measured 2026-08-08 after five hours,
//! the aggregate duty cycle fell 96.6% -> 71.8%, dragged down by a tail of quotes sitting at 1.7%
//! and 3.4% — out of band for hours, earning nothing, still counted as tracked.
//!
//! So the duty cycle here is the **no-re-quote** duty cycle, a LOWER bound on what a real strategy
//! would sustain. It does not follow that the strategy looks better than measured, because
//! re-quoting cuts both ways: a price dragged back to the band is a price back in front of the
//! order flow, so more qualifying time buys more fills and more adverse selection. The two effects
//! push the net in opposite directions and neither is measured here.
//!
//! Compounding this, [`quote_price`] deliberately sits at the FAR EDGE of the band, which is the
//! choice that maximises time-to-fill and simultaneously minimises time-to-disqualification — any
//! adverse move at all puts it outside. A quote at the midpoint would hold its qualification far
//! longer and be hit far sooner. That placement is therefore not a neutral detail but a free
//! parameter that dominates the duty cycle, and it was picked (see `quote_price`) rather than
//! measured. Treat "duty cycle 72%" as "duty cycle at the band edge, without re-quoting", never as
//! a property of maker quoting as such.
//!
//! ## Known bias, stated rather than buried
//!
//! The mid-crossing fill rule UNDER-counts fills: a resting bid can be hit by a seller without the
//! midpoint ever reaching it. Under-counted fills mean under-counted adverse selection, which
//! flatters the strategy. This is the optimistic direction, and it is accepted here only because
//! the alternative — inferring fills from trades we do not receive — is not observable from the
//! book feed we have. Read a positive result as "not yet falsified", not as "proven".

use anyhow::Result;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::ingester::clob_ws::LiveBookStore;
use crate::strategy::rewards::RewardCandidate;

/// Seconds in a day, for pro-rating a daily reward rate over an evaluation interval.
const SECONDS_PER_DAY: i64 = 86_400;

/// How long after a fill the P&L is measured. One hour: long enough for a transient sweep to
/// revert (which is the maker's edge and must be observable), short enough that the number is about
/// execution rather than about the market's multi-day direction.
pub const FILL_HORIZON_SECS: i64 = 3_600;

/// Cap on concurrently tracked quotes. Bounded because each one costs a book read per cycle, and
/// because a measurement that tracks everything is a strategy wearing a measurement's clothes.
const MAX_OPEN_QUOTES: usize = 40;

/// A quote is only worth tracking if the scanner thinks it could earn something. Below this the
/// accrual is rounding noise and the slot is better spent on a market that might teach us something.
const MIN_TRACKED_DAILY_USD: Decimal = dec!(0.10);

/// How long past its horizon a filled quote may wait for a readable book before it is abandoned as
/// unmeasurable.
///
/// This exists because of a measured selection bias, not as a tidy-up. At 6h the tracker had three
/// measured fills averaging a gap-through (|mid_at_fill - price|) of **0.0087**, and one fill it
/// could NOT price whose gap-through was **0.0500** — 5.7x further. The mechanism is plausible and
/// unkind: a market that moves violently is also one whose book goes thin, wide, or drops out of the
/// live feed, so the fills most likely to be unmeasurable are the worst ones. Silently leaving them
/// pending would quietly delete the left tail of the P&L distribution.
///
/// So they are abandoned and COUNTED (`fills_abandoned`). An abandoned fill is not a zero — it is a
/// known unknown, and the count is what stops the measured mean from being read as complete.
const MAX_HORIZON_WAIT_SECS: i64 = FILL_HORIZON_SECS * 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuoteSide {
    Bid,
    Ask,
}

impl QuoteSide {
    pub fn as_str(&self) -> &'static str {
        match self {
            QuoteSide::Bid => "Bid",
            QuoteSide::Ask => "Ask",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MakerShadowDiagnostics {
    /// Quotes resting at the end of this cycle.
    pub open_quotes: usize,
    /// Quotes newly placed this cycle.
    pub placed: usize,
    /// Quotes the midpoint crossed this cycle.
    pub filled: usize,
    /// Fills that reached their measurement horizon this cycle.
    pub horizons_measured: usize,
    /// Rewards accrued across every open quote, since placement.
    pub accrued_reward_usd: Decimal,
    /// Realised horizon P&L across every measured fill. **This is the number that matters**: netted
    /// against `accrued_reward_usd` it is the first honest read on whether making pays here.
    pub horizon_pnl_usd: Decimal,
    /// Share of tracked time spent inside the reward band, in percent. The scanner assumes 100.
    pub duty_cycle_pct: Decimal,
    /// Quotes skipped because the book could not be read this cycle — accrue nothing rather than
    /// assume they kept qualifying.
    pub unpriced: usize,
    /// Fills past their horizon still waiting for a readable book. A backlog here means
    /// `horizon_pnl_usd` describes fewer fills than have actually happened.
    pub fills_overdue: usize,
    /// Fills given up on as unmeasurable (see `MAX_HORIZON_WAIT_SECS`). **Read this alongside
    /// `horizon_pnl_usd` or read neither**: the unmeasurable fills skew toward the violent moves, so
    /// a mean computed without them is optimistic by an unknown amount, and this count is the only
    /// signal of how much is missing.
    pub fills_abandoned: usize,
}

/// Whether a price rests inside the reward-qualifying band around the midpoint.
///
/// Inclusive at the boundary: the published rule is "within `max_spread` of the midpoint", and a
/// quote exactly at the limit qualifies. Being strict here would understate the duty cycle by
/// silently disqualifying the exact price a real quoting strategy would choose — the furthest one
/// that still counts, since that is where capital works hardest.
pub fn is_qualifying(price: Decimal, mid: Decimal, max_spread: Decimal) -> bool {
    (price - mid).abs() <= max_spread
}

/// Whether the market has moved through a resting quote, i.e. we would have been filled.
///
/// A bid fills when the mid falls to or below it; an ask fills when the mid rises to or above it.
/// See the module docs for why this under-counts fills and why that bias is accepted.
pub fn would_fill(side: QuoteSide, price: Decimal, mid: Decimal) -> bool {
    match side {
        QuoteSide::Bid => mid <= price,
        QuoteSide::Ask => mid >= price,
    }
}

/// Reward earned over `elapsed_secs` at `share` of a `daily_rate` pool.
///
/// Integrated per interval rather than extrapolated from one instant, which is the entire
/// difference between this and the snapshot scanner: the share is re-observed every cycle, so a
/// quote that is crowded out midway through the day earns the crowded rate from that point on.
/// Negative or zero elapsed time earns nothing — clock skew must not mint rewards.
pub fn accrue_reward(daily_rate: Decimal, share: Decimal, elapsed_secs: i64) -> Decimal {
    if elapsed_secs <= 0 || share <= Decimal::ZERO || daily_rate <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    let capped = elapsed_secs.min(SECONDS_PER_DAY);
    daily_rate * share * Decimal::from(capped) / Decimal::from(SECONDS_PER_DAY)
}

/// P&L on shares acquired by a filled quote, marked at a later midpoint.
///
/// A filled bid leaves us LONG at `fill_price`, so it profits when the mid recovers above it; a
/// filled ask leaves us SHORT, and profits when the mid falls below it. Both are per-share times
/// size.
pub fn horizon_pnl(
    side: QuoteSide,
    fill_price: Decimal,
    mid_at_horizon: Decimal,
    size: Decimal,
) -> Decimal {
    let per_share = match side {
        QuoteSide::Bid => mid_at_horizon - fill_price,
        QuoteSide::Ask => fill_price - mid_at_horizon,
    };
    per_share * size
}

/// Where a quote would rest: `max_spread` away from the mid, on the given side.
///
/// The furthest price that still qualifies, deliberately. A quote nearer the mid earns the same
/// reward (qualification is a threshold, not a gradient, under the published rule we model) while
/// being filled sooner and more often — so the edge of the band is where a real strategy would sit,
/// and measuring anywhere else would measure a strategy nobody would run.
///
/// Clamped into (0, 1): prices outside the probability range are not quotable, and a market whose
/// mid sits within `max_spread` of a boundary would otherwise produce a nonsensical quote.
pub fn quote_price(side: QuoteSide, mid: Decimal, max_spread: Decimal) -> Option<Decimal> {
    let raw = match side {
        QuoteSide::Bid => mid - max_spread,
        QuoteSide::Ask => mid + max_spread,
    };
    (raw > Decimal::ZERO && raw < Decimal::ONE).then_some(raw)
}

/// Candidates worth opening a shadow quote on, given what is already tracked.
///
/// Pure so the selection rule is testable without a database: it is the one place where a
/// measurement could quietly become a strategy (by concentrating on whatever looked best last
/// cycle), and the cap plus the already-tracked filter are what keep it a sample.
pub fn select_new_quotes<'a>(
    ranked: &'a [RewardCandidate],
    already_tracked: &std::collections::HashSet<String>,
    open_count: usize,
) -> Vec<&'a RewardCandidate> {
    let room = MAX_OPEN_QUOTES.saturating_sub(open_count);
    ranked
        .iter()
        .filter(|c| !already_tracked.contains(&c.market_id))
        .filter(|c| c.estimated_daily_usd >= MIN_TRACKED_DAILY_USD)
        // Only quote what we can OBSERVE. A candidate priced from a polled snapshot rather than the
        // live book is one whose snapshot may already be ~52 minutes old (measured 2026-08-08),
        // well past this module's own 30-minute freshness bar — so it would be placed and then sit
        // unevaluated, accruing nothing and, if filled, never yielding a P&L number.
        //
        // This does not eliminate the problem, because a book can go unreadable AFTER placement;
        // `fills_abandoned` exists for that. It removes the avoidable half: never start tracking
        // something already known to be unobservable.
        .filter(|c| c.from_live_book)
        .take(room)
        .collect()
}

/// One tracking cycle: evaluate every open quote against the current book, accrue, detect fills,
/// measure horizons, then open quotes on new candidates. Places nothing anywhere.
pub async fn track_shadow_quotes(
    pool: &PgPool,
    books: Option<&LiveBookStore>,
    ranked: &[RewardCandidate],
) -> Result<MakerShadowDiagnostics> {
    let mut diag = MakerShadowDiagnostics::default();
    let now = chrono::Utc::now();

    // Open quotes, plus filled ones still awaiting their horizon measurement.
    let rows: Vec<ShadowQuoteRow> = sqlx::query_as(
        r#"SELECT id, market_id, token_id, side, price, size, daily_rate, max_spread,
                  placed_at, last_evaluated_at, qualifying_seconds, accrued_reward_usd,
                  status, filled_at
             FROM paper_trading.shadow_quotes
            WHERE status = 'open'
               OR (status = 'filled' AND mid_at_horizon IS NULL)"#,
    )
    .fetch_all(pool)
    .await?;

    let mut tracked_age_secs = Decimal::ZERO;
    let mut tracked_qualifying_secs = Decimal::ZERO;

    for row in &rows {
        let side = if row.side == "Bid" {
            QuoteSide::Bid
        } else {
            QuoteSide::Ask
        };

        let mid_now = current_mid(pool, books, &row.token_id, &row.market_id).await;
        let Some(mid) = mid_now else {
            // No readable book. Advance nothing: crediting qualifying time we did not observe is
            // exactly how a measurement starts flattering itself.
            diag.unpriced += 1;
            // A FILLED quote with no book is different from an open one: it is a P&L number we owe
            // and cannot collect. Track how overdue the backlog is, and eventually give up loudly
            // rather than leave it pending forever — an invisible pending fill is a deleted data
            // point, and the deleted ones skew bad (see MAX_HORIZON_WAIT_SECS).
            if row.status == "filled" {
                if let Some(filled_at) = row.filled_at {
                    let overdue = (now - filled_at).num_seconds();
                    if overdue > MAX_HORIZON_WAIT_SECS {
                        sqlx::query(
                            "UPDATE paper_trading.shadow_quotes
                                SET status = 'cancelled', last_evaluated_at = now(),
                                    closed_reason = 'horizon unmeasurable — book unreadable'
                              WHERE id = $1",
                        )
                        .bind(row.id)
                        .execute(pool)
                        .await?;
                        diag.fills_abandoned += 1;
                    } else if overdue > FILL_HORIZON_SECS {
                        diag.fills_overdue += 1;
                    }
                }
            }
            continue;
        };

        // A filled quote is done accruing; it is only here to have its horizon measured.
        if row.status == "filled" {
            let Some(filled_at) = row.filled_at else {
                continue;
            };
            if (now - filled_at).num_seconds() < FILL_HORIZON_SECS {
                continue;
            }
            let pnl = horizon_pnl(side, row.price, mid, row.size);
            sqlx::query(
                "UPDATE paper_trading.shadow_quotes
                    SET mid_at_horizon = $1, horizon_pnl_usd = $2, last_evaluated_at = now()
                  WHERE id = $3",
            )
            .bind(mid)
            .bind(pnl)
            .bind(row.id)
            .execute(pool)
            .await?;
            diag.horizons_measured += 1;
            diag.horizon_pnl_usd += pnl;
            continue;
        }

        let elapsed = (now - row.last_evaluated_at).num_seconds();
        let qualifying = is_qualifying(row.price, mid, row.max_spread);

        // Re-observe the share every cycle rather than trusting the placement-time estimate — a
        // quote crowded out halfway through the day should earn the crowded rate from then on.
        let share = current_share(
            pool,
            books,
            &row.token_id,
            &row.market_id,
            row.size,
            mid,
            row.max_spread,
        )
        .await
        .unwrap_or(Decimal::ZERO);
        let earned = if qualifying {
            accrue_reward(row.daily_rate, share, elapsed)
        } else {
            Decimal::ZERO
        };

        tracked_age_secs += Decimal::from((now - row.placed_at).num_seconds().max(0));
        tracked_qualifying_secs += row.qualifying_seconds
            + if qualifying {
                Decimal::from(elapsed.max(0))
            } else {
                Decimal::ZERO
            };

        if would_fill(side, row.price, mid) {
            sqlx::query(
                "UPDATE paper_trading.shadow_quotes
                    SET status = 'filled', filled_at = now(), mid_at_fill = $1,
                        qualifying_seconds = qualifying_seconds + $2,
                        accrued_reward_usd = accrued_reward_usd + $3,
                        last_evaluated_at = now(),
                        closed_reason = 'mid crossed the quote'
                  WHERE id = $4",
            )
            .bind(mid)
            .bind(Decimal::from(if qualifying { elapsed.max(0) } else { 0 }))
            .bind(earned)
            .bind(row.id)
            .execute(pool)
            .await?;
            diag.filled += 1;
        } else {
            sqlx::query(
                "UPDATE paper_trading.shadow_quotes
                    SET qualifying_seconds = qualifying_seconds + $1,
                        accrued_reward_usd = accrued_reward_usd + $2,
                        last_evaluated_at = now()
                  WHERE id = $3",
            )
            .bind(Decimal::from(if qualifying { elapsed.max(0) } else { 0 }))
            .bind(earned)
            .bind(row.id)
            .execute(pool)
            .await?;
            diag.open_quotes += 1;
        }
    }

    // Open quotes on candidates we are not already tracking.
    let already: std::collections::HashSet<String> =
        rows.iter().map(|r| r.market_id.clone()).collect();
    for cand in select_new_quotes(ranked, &already, diag.open_quotes) {
        // Quote the side that is cheaper to hold. Both sides qualify under the published rule, so
        // this is a capital choice, not an edge claim.
        let side = if cand.mid <= dec!(0.5) {
            QuoteSide::Bid
        } else {
            QuoteSide::Ask
        };
        let Some(price) = quote_price(side, cand.mid, cand.max_spread) else {
            continue;
        };
        sqlx::query(
            r#"INSERT INTO paper_trading.shadow_quotes
                 (id, market_id, token_id, side, price, size, daily_rate, max_spread,
                  mid_at_placement)
               VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)"#,
        )
        .bind(uuid::Uuid::new_v4())
        .bind(&cand.market_id)
        // The scanner prices off the Yes book, so a quote follows the same token.
        .bind(
            yes_token_id(pool, &cand.market_id)
                .await
                .unwrap_or_default(),
        )
        .bind(side.as_str())
        .bind(price)
        .bind(cand.min_size)
        .bind(cand.daily_rate)
        .bind(cand.max_spread)
        .bind(cand.mid)
        .execute(pool)
        .await?;
        diag.placed += 1;
        diag.open_quotes += 1;
    }

    diag.accrued_reward_usd = sqlx::query_scalar::<_, Decimal>(
        "SELECT COALESCE(SUM(accrued_reward_usd), 0) FROM paper_trading.shadow_quotes",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(Decimal::ZERO)
    .round_dp(4);
    diag.horizon_pnl_usd = sqlx::query_scalar::<_, Decimal>(
        "SELECT COALESCE(SUM(horizon_pnl_usd), 0) FROM paper_trading.shadow_quotes
          WHERE horizon_pnl_usd IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(Decimal::ZERO)
    .round_dp(4);
    diag.duty_cycle_pct = if tracked_age_secs > Decimal::ZERO {
        (tracked_qualifying_secs / tracked_age_secs * Decimal::ONE_HUNDRED).round_dp(1)
    } else {
        Decimal::ZERO
    };

    Ok(diag)
}

#[derive(sqlx::FromRow)]
struct ShadowQuoteRow {
    id: uuid::Uuid,
    market_id: String,
    token_id: String,
    side: String,
    price: Decimal,
    size: Decimal,
    daily_rate: Decimal,
    max_spread: Decimal,
    placed_at: chrono::DateTime<chrono::Utc>,
    last_evaluated_at: chrono::DateTime<chrono::Utc>,
    qualifying_seconds: Decimal,
    #[allow(dead_code)]
    accrued_reward_usd: Decimal,
    status: String,
    filled_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Current midpoint from the live book where fresh, else the most recent snapshot.
async fn current_mid(
    pool: &PgPool,
    books: Option<&LiveBookStore>,
    token_id: &str,
    market_id: &str,
) -> Option<Decimal> {
    if let Some(m) = books
        .and_then(|b| b.get_fresh(token_id))
        .and_then(|b| b.mid())
    {
        return Some(m);
    }
    sqlx::query_scalar::<_, Option<Decimal>>(
        "SELECT mid FROM market_data.orderbook_snapshots
          WHERE market_id = $1 AND outcome = 'Yes' AND fetched_at > now() - interval '30 minutes'
          ORDER BY fetched_at DESC LIMIT 1",
    )
    .bind(market_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .flatten()
    .filter(|m| *m > Decimal::ZERO)
}

/// Our share of the qualifying depth right now, re-observed rather than carried from placement.
async fn current_share(
    pool: &PgPool,
    books: Option<&LiveBookStore>,
    token_id: &str,
    market_id: &str,
    our_size: Decimal,
    mid: Decimal,
    band: Decimal,
) -> Option<Decimal> {
    let (bids, asks) = match books.and_then(|b| b.get_fresh(token_id)) {
        Some(book) => (book.bid_levels(), book.ask_levels()),
        None => {
            let row: Option<(serde_json::Value, serde_json::Value)> = sqlx::query_as(
                "SELECT bids, asks FROM market_data.orderbook_snapshots
                  WHERE market_id = $1 AND outcome = 'Yes'
                    AND fetched_at > now() - interval '30 minutes'
                  ORDER BY fetched_at DESC LIMIT 1",
            )
            .bind(market_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
            let (b, a) = row?;
            (
                crate::strategy::rewards::parse_levels(&b),
                crate::strategy::rewards::parse_levels(&a),
            )
        }
    };
    let depth = crate::strategy::rewards::qualifying_depth(&bids, &asks, mid, band);
    // Same zero-depth guard as the scanner: an empty band is an unreadable book, not a 100% share.
    (depth > Decimal::ZERO).then(|| crate::strategy::rewards::estimated_share(our_size, depth))
}

async fn yes_token_id(pool: &PgPool, market_id: &str) -> Option<String> {
    let tokens: serde_json::Value =
        sqlx::query_scalar("SELECT clob_token_ids FROM market_data.markets WHERE gamma_id = $1")
            .bind(market_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()?;
    tokens.get(0)?.as_str().map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The band is inclusive at its edge, and `quote_price` deliberately sits exactly there — so if
    /// this were strict, every quote the strategy places would be born disqualified.
    #[test]
    fn a_quote_at_the_exact_band_edge_qualifies() {
        assert!(is_qualifying(dec!(0.465), dec!(0.5), dec!(0.035)));
        assert!(is_qualifying(dec!(0.535), dec!(0.5), dec!(0.035)));
        assert!(!is_qualifying(dec!(0.464), dec!(0.5), dec!(0.035)));
        // And the price the placement path chooses is exactly that edge, both sides.
        let p = quote_price(QuoteSide::Bid, dec!(0.5), dec!(0.035)).unwrap();
        assert!(is_qualifying(p, dec!(0.5), dec!(0.035)));
        let p = quote_price(QuoteSide::Ask, dec!(0.5), dec!(0.035)).unwrap();
        assert!(is_qualifying(p, dec!(0.5), dec!(0.035)));
    }

    /// Fill direction is the easiest thing here to invert, and inverting it would report adverse
    /// selection with the sign flipped — i.e. it would say making is profitable precisely when it
    /// is being picked off.
    #[test]
    fn a_bid_fills_when_the_mid_falls_and_an_ask_when_it_rises() {
        assert!(would_fill(QuoteSide::Bid, dec!(0.60), dec!(0.59)));
        assert!(would_fill(QuoteSide::Bid, dec!(0.60), dec!(0.60)));
        assert!(!would_fill(QuoteSide::Bid, dec!(0.60), dec!(0.61)));

        assert!(would_fill(QuoteSide::Ask, dec!(0.60), dec!(0.61)));
        assert!(would_fill(QuoteSide::Ask, dec!(0.60), dec!(0.60)));
        assert!(!would_fill(QuoteSide::Ask, dec!(0.60), dec!(0.59)));
    }

    /// A filled bid is LONG and profits on recovery; a filled ask is SHORT and profits on decline.
    /// Both directions are asserted because a sign error here inverts the headline conclusion.
    #[test]
    fn horizon_pnl_is_positive_when_the_move_reverts() {
        // Bid filled at 0.60, mid recovers to 0.62 → +0.02 x 200 shares.
        assert_eq!(
            horizon_pnl(QuoteSide::Bid, dec!(0.60), dec!(0.62), dec!(200)),
            dec!(4.00)
        );
        // ...and stays down at 0.58 → picked off, -0.02 x 200.
        assert_eq!(
            horizon_pnl(QuoteSide::Bid, dec!(0.60), dec!(0.58), dec!(200)),
            dec!(-4.00)
        );
        // Ask filled at 0.60, mid falls back to 0.58 → the short profits.
        assert_eq!(
            horizon_pnl(QuoteSide::Ask, dec!(0.60), dec!(0.58), dec!(200)),
            dec!(4.00)
        );
        assert_eq!(
            horizon_pnl(QuoteSide::Ask, dec!(0.60), dec!(0.62), dec!(200)),
            dec!(-4.00)
        );
    }

    /// Accrual is pro-rated over the interval actually observed. The snapshot scanner's headline is
    /// this same number with elapsed = a full day, which is the assumption under test.
    #[test]
    fn rewards_accrue_pro_rata_and_never_from_nothing() {
        // $1000/day pool, 1% share, one hour → 1000 * 0.01 * 3600/86400 = 0.41666...
        let earned = accrue_reward(dec!(1000), dec!(0.01), 3600);
        assert_eq!(earned.round_dp(5), dec!(0.41667));
        // A full day at the same share is the scanner's headline: the whole 1%.
        assert_eq!(accrue_reward(dec!(1000), dec!(0.01), 86400), dec!(10));

        // Degenerate inputs mint nothing. Backwards clocks in particular must not pay.
        assert_eq!(accrue_reward(dec!(1000), dec!(0.01), 0), Decimal::ZERO);
        assert_eq!(accrue_reward(dec!(1000), dec!(0.01), -600), Decimal::ZERO);
        assert_eq!(
            accrue_reward(dec!(1000), Decimal::ZERO, 3600),
            Decimal::ZERO
        );
        // An interval longer than a day is capped, so a quote resumed after a long outage cannot
        // book more than a day's pool in one cycle.
        assert_eq!(accrue_reward(dec!(1000), dec!(0.01), 999_999), dec!(10));
    }

    /// Prices outside (0,1) are not quotable. A market trading at 0.02 with a 3.5c band would
    /// otherwise generate a bid at -0.015.
    #[test]
    fn quotes_outside_the_probability_range_are_refused() {
        assert_eq!(quote_price(QuoteSide::Bid, dec!(0.02), dec!(0.035)), None);
        assert_eq!(quote_price(QuoteSide::Ask, dec!(0.98), dec!(0.035)), None);
        assert_eq!(
            quote_price(QuoteSide::Bid, dec!(0.50), dec!(0.035)),
            Some(dec!(0.465))
        );
    }

    fn candidate(id: &str, est: Decimal) -> RewardCandidate {
        RewardCandidate {
            market_id: id.to_string(),
            question: "q".into(),
            daily_rate: dec!(100),
            min_size: dec!(200),
            max_spread: dec!(0.035),
            mid: dec!(0.5),
            qualifying_depth: dec!(1000),
            estimated_share: dec!(0.1),
            estimated_daily_usd: est,
            capital_usd: dec!(100),
            from_live_book: true,
        }
    }

    /// Selection must not re-quote a market it is already tracking (which would double-count both
    /// the reward and the risk), must respect the cap, and must skip candidates too small to teach
    /// us anything.
    #[test]
    fn selection_skips_tracked_markets_respects_the_cap_and_ignores_dust() {
        let ranked = vec![
            candidate("a", dec!(5)),
            candidate("b", dec!(4)),
            candidate("dust", dec!(0.01)),
            candidate("c", dec!(3)),
        ];
        let mut tracked = std::collections::HashSet::new();
        tracked.insert("b".to_string());

        let picked = select_new_quotes(&ranked, &tracked, 0);
        let ids: Vec<&str> = picked.iter().map(|c| c.market_id.as_str()).collect();
        assert_eq!(ids, vec!["a", "c"], "tracked and dust both excluded");

        // A candidate we cannot observe is never quoted, however attractive it looks. Its snapshot
        // may already be past this module's freshness bar, so it would rest unevaluated -- accruing
        // nothing and, if filled, never producing a P&L number.
        let mut stale = candidate("snapshot_only", dec!(99));
        stale.from_live_book = false;
        let with_stale = vec![stale, candidate("a", dec!(5))];
        let picked = select_new_quotes(&with_stale, &std::collections::HashSet::new(), 0);
        let ids: Vec<&str> = picked.iter().map(|c| c.market_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["a"],
            "the highest-ranked candidate is skipped when it is not live-priced"
        );

        // At the cap, nothing new opens — the measurement stays a bounded sample.
        assert!(select_new_quotes(&ranked, &tracked, MAX_OPEN_QUOTES).is_empty());
        // One slot left, one taken.
        assert_eq!(
            select_new_quotes(&ranked, &tracked, MAX_OPEN_QUOTES - 1).len(),
            1
        );
    }
}
