//! PaperTradingEngine implementation.
//!
//! High-fidelity paper matching fed by DB orderbook snapshots (populated by ingester from live public CLOB).
//! ALL finance math uses rust_decimal::Decimal exclusively — no floats.
//! Every submit + fill is journaled. Positions + portfolio updated.
//!
//! RISK IMPLICATIONS (per AGENTS.md):
//! - This is SIMULATED only. Fills do not affect real capital or Polymarket.
//! - Slippage and fee models are conservative approximations; real books can have worse queue/impact/adverse selection.
//! - No position limits or kill switches yet (Phase 0 bootstrap). Add before any strategy scaling.
//! - Bootstrap uses latest snapshot or synthetic mid; thin books or stale snapshots = optimistic fills possible.

use crate::ingester::{OrderbookSnapshot, PriceSize};
use crate::journal::JournalWriter;
use crate::paper::models::*;
use anyhow::{Context, Result};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use sqlx::{PgPool, Row};
use std::str::FromStr;
use std::sync::Arc;

pub struct PaperTradingEngine {
    pool: PgPool,
    journal: Arc<JournalWriter>,
    /// P5 live orderbook feed, when one is running. Empty (or absent) means every fill matches
    /// against the polled snapshot, exactly as before.
    live_books: Option<crate::ingester::clob_ws::LiveBookStore>,
}

/// What one leg of a basket would fill right now, per `PaperTradingEngine::plan_basket`.
#[derive(Debug, Clone)]
pub struct BasketLegPlan {
    pub market_id: String,
    pub outcome: String,
    /// Size the basket needs from this leg. A basket is only an arb at a COMMON unit count, so a
    /// leg that can fill 90% is not 90% useful — it is a leg that breaks the payout floor.
    pub requested: Decimal,
    /// Size available at or inside the leg's limit price, excluding synthetic (bookless) fills.
    pub fillable: Decimal,
    /// Cost of the fillable portion, fees included.
    pub cost: Decimal,
    /// False when no book could be loaded at all (market never ingested, or GC'd).
    pub had_book: bool,
}

impl BasketLegPlan {
    /// Whether this leg can supply the whole requested size. Deliberately all-or-nothing: see
    /// `requested`.
    pub fn is_complete(&self) -> bool {
        self.fillable >= self.requested
    }

    /// Fraction of the requested size the book can supply, for diagnostics. Zero-size legs report
    /// complete (1) rather than dividing by zero — a basket never requests zero, but a diagnostic
    /// that panics is worse than one that is uninteresting.
    pub fn fill_ratio(&self) -> Decimal {
        if self.requested <= dec!(0) {
            dec!(1)
        } else {
            (self.fillable / self.requested).min(dec!(1))
        }
    }
}

/// The pre-flight verdict for a whole basket.
#[derive(Debug, Clone)]
pub struct BasketPlan {
    pub legs: Vec<BasketLegPlan>,
}

impl BasketPlan {
    /// The decision. Every leg must be able to fill in full — this is the property that makes the
    /// >= (legs-1) payout floor real, and it does not degrade gracefully.
    pub fn is_executable(&self) -> bool {
        !self.legs.is_empty() && self.legs.iter().all(|l| l.is_complete())
    }

    /// Legs the book cannot fill in full — the ones that would have broken the basket.
    pub fn short_legs(&self) -> usize {
        self.legs.iter().filter(|l| !l.is_complete()).count()
    }

    /// The binding constraint: the least-fillable leg. A basket is exactly as executable as this.
    pub fn worst_fill_ratio(&self) -> Decimal {
        self.legs
            .iter()
            .map(|l| l.fill_ratio())
            .min()
            .unwrap_or(dec!(0))
    }

    /// Cost of the legs that CAN fill — i.e. the capital the old sequential loop would have
    /// committed to a basket that was never going to be complete.
    pub fn committed_cost_if_executed(&self) -> Decimal {
        self.legs.iter().map(|l| l.cost).sum()
    }

    /// Compact per-leg detail for the journal. Only the short legs, since a complete basket's legs
    /// are uninteresting and this is written on every scan.
    pub fn short_leg_detail(&self) -> Vec<serde_json::Value> {
        self.legs
            .iter()
            .filter(|l| !l.is_complete())
            .map(|l| {
                serde_json::json!({
                    "market_id": l.market_id,
                    "outcome": l.outcome,
                    "requested": l.requested.to_string(),
                    "fillable": l.fillable.to_string(),
                    "fill_ratio": l.fill_ratio().round_dp(4).to_string(),
                    "had_book": l.had_book,
                })
            })
            .collect()
    }
}

impl PaperTradingEngine {
    pub fn new(pool: PgPool, journal: Arc<JournalWriter>) -> Self {
        Self {
            pool,
            journal,
            live_books: None,
        }
    }

    /// Match fills against the live WebSocket book where one is fresh and in sync.
    ///
    /// This has to move in lockstep with the arb scanner's price source, and the reason is
    /// specific: negRisk legs are placed as **limit** orders at the price the scanner saw. If the
    /// scanner reads a live ask of 0.66 while the matcher walks a snapshot whose best ask is still
    /// 0.67, the limit is simply never marketable and the basket records a partial-or-unfilled
    /// every cycle. The failure is safe — a stale book can never make us *overpay* through a limit
    /// — but a scanner that finds baskets the engine structurally cannot fill is worse than no
    /// scanner change at all. One book, read by both.
    pub fn with_live_books(mut self, books: crate::ingester::clob_ws::LiveBookStore) -> Self {
        self.live_books = Some(books);
        self
    }

    /// Submit paper order. Loads latest book snapshot from DB for the (market, outcome).
    /// Produces realistic fills (limit walks book; market applies depth slippage + taker fee).
    /// Journals order + fills + updated portfolio snapshot.
    /// Returns the fills produced (may be partial or multiple levels).
    pub async fn submit_order(&self, mut order: PaperOrder) -> Result<Vec<PaperFill>> {
        tracing::info!(
            order_id = %order.id,
            market = %order.market_id,
            outcome = %order.outcome,
            side = ?order.side,
            r#type = ?order.order_type,
            size = %order.size,
            limit = ?order.limit_price,
            "paper order submit received"
        );

        // Normalize outcome casing to canonical "Yes"/"No" *before any DB interaction* for CHECK compliance and consistency with ingestion path.
        order.outcome = if order.outcome.eq_ignore_ascii_case("yes") {
            "Yes".to_string()
        } else if order.outcome.eq_ignore_ascii_case("no") {
            "No".to_string()
        } else {
            order.outcome
        };

        // Working sqlx transaction wrapper for the *full* submit sequence (order + fills + pos RMW + snapshot) + FOR UPDATE lock on the position row.
        // This delivers the atomicity + race protection requested in the critical review item. Writer and update fn left unchanged (append-only on pool; pos/snapshot logic inlined under tx for the contended path).
        let mut tx = self
            .pool
            .begin()
            .await
            .context("begin submit tx for atomicity")?;

        // Lock the exact (market, outcome) position row for the duration of the write path (prevents concurrent RMW races / lost updates).
        sqlx::query(
            "SELECT 1 FROM paper_trading.paper_positions WHERE market_id = $1 AND outcome = $2 FOR UPDATE",
        )
        .bind(&order.market_id)
        .bind(&order.outcome)
        .execute(&mut *tx)
        .await
        .ok(); // row may not exist yet — lock not required for first insert

        // 1. Persist intent (journal on pool — append-only, low contention)
        order.status = OrderStatus::Open;
        if order.decision_context.is_none() {
            order.decision_context = Some(
                serde_json::json!({"source": "manual_or_stub", "note": "Phase 0 bootstrap submit"}),
            );
        }
        self.journal.record_paper_order(&order).await?;

        // 2-3. Load + match (read-only snapshot + pure compute)
        let book = self
            .load_latest_book_snapshot(&order.market_id, &order.outcome)
            .await
            .context("loading book snapshot")?;
        let fills = self.match_against_book(&mut order, book.as_ref()).await?;

        if fills.is_empty() {
            order.status = OrderStatus::Rejected;
            self.journal.record_paper_order(&order).await?;
            tx.commit().await.ok();
            tracing::warn!(order_id = %order.id, "order rejected (no liquidity or limit not crossed)");
            return Ok(vec![]);
        }

        self.journal.record_paper_fills(&fills).await?;

        // 5. Critical contended path (pos RMW + snapshot) under the tx + lock for atomicity guarantee.
        // (Minimal inline of the essential load/compute/upsert + snapshot INSERT using the cash/gross logic already present in update_positions_and_snapshot.
        // This is the smallest dupe needed to keep writer/update unchanged while delivering a working tx wrapper for the full sequence.)
        let (old_shares, old_avg, _old_coll) = sqlx::query_as::<_, (Decimal, Decimal, Decimal)>(
            "SELECT shares, avg_entry_price, collateral_locked FROM paper_trading.paper_positions WHERE market_id = $1 AND outcome = $2",
        )
        .bind(&order.market_id)
        .bind(&order.outcome)
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or((dec!(0), dec!(0), dec!(0)));

        let mut delta_shares: Decimal = dec!(0);
        let mut total_fee: Decimal = dec!(0);
        for f in &fills {
            let signed = if matches!(order.side, OrderSide::Buy) {
                f.size
            } else {
                -f.size
            };
            delta_shares += signed;
            total_fee += f.fee;
        }
        let total_gross: Decimal = fills.iter().map(|f| f.price * f.size).sum();

        let new_shares = old_shares + delta_shares;
        let new_avg = if new_shares <= dec!(0) {
            dec!(0)
        } else if matches!(order.side, OrderSide::Sell) {
            // A sale never changes the cost basis of what remains — only buys re-average. (The old
            // buy formula applied to a negative delta corrupted avg_entry on partial sells.)
            old_avg
        } else if old_shares > dec!(0) {
            (old_shares * old_avg + total_gross) / new_shares
        } else {
            fills.first().map(|f| f.price).unwrap_or(dec!(0.5))
        };
        // Realized P&L from closing shares at market (exit path): proceeds vs cost basis, per fill.
        // Without this the sale's P&L evaporates — the cash identity below only returns the freed
        // cost basis (locked drops by shares×avg), so selling at 0.80 what we bought at 0.50 would
        // credit cash 0.50/share. Fees stay in total_fees_agg (they are cash-model-wide).
        let realized_delta: Decimal = if matches!(order.side, OrderSide::Sell) {
            fills.iter().map(|f| (f.price - old_avg) * f.size).sum()
        } else {
            dec!(0)
        };
        let new_coll = if new_shares > dec!(0) {
            new_shares * new_avg.abs()
        } else {
            dec!(0)
        };

        // Raw pos upsert under tx (the RMW that needed the lock)
        sqlx::query(
            r#"INSERT INTO paper_trading.paper_positions (market_id, outcome, shares, avg_entry_price, collateral_locked, last_updated)
               VALUES ($1,$2,$3,$4,$5, now())
               ON CONFLICT (market_id, outcome) DO UPDATE SET shares=EXCLUDED.shares, avg_entry_price=EXCLUDED.avg_entry_price, collateral_locked=EXCLUDED.collateral_locked, last_updated=now()"#,
        )
        .bind(&order.market_id)
        .bind(&order.outcome)
        .bind(new_shares)
        .bind(new_avg)
        .bind(new_coll)
        .execute(&mut *tx)
        .await?;

        // Raw portfolio snapshot under tx (completes the full sequence atomically).
        // FIX: total_locked + virtual_usdc must reflect AGGREGATE state across all open positions,
        // not just this order. The old code wrote total_locked=new_coll (this position only) and
        // virtual_usdc=10000+this_order_cashflow, so multi-position portfolios were badly undercounted
        // — which in turn made the RiskManager exposure cap (which reads total_locked) far too loose.
        // Aggregate from the position + fill tables (within the tx, after the upsert above).
        let total_locked_agg: Decimal = sqlx::query_scalar(
            "SELECT COALESCE(SUM(collateral_locked), 0) FROM paper_trading.paper_positions",
        )
        .fetch_one(&mut *tx)
        .await?;
        // Fees SINCE THE LAST PAPER RESET only (see write_mark_to_market_snapshot): the reset preserves
        // fills for audit but re-baselines cash, so lifetime fees would permanently re-subtract pre-reset
        // fees from the fresh $10k seed. Reset-boundary filter keeps the post-fill cash consistent.
        let total_fees_agg: Decimal = sqlx::query_scalar(
            "SELECT COALESCE(SUM(fee), 0) FROM paper_trading.paper_fills
             WHERE created_at >= COALESCE(
               (SELECT max(as_of) FROM paper_trading.virtual_portfolio_snapshots
                WHERE snapshot_reason = 'manual_paper_reset'), '-infinity'::timestamptz)",
        )
        .fetch_one(&mut *tx)
        .await?;
        // Carry forward cumulative realized P&L from settlements (do NOT reset to 0 on each fill,
        // else a fill after a settlement would wipe realized P&L — the input the "proven" gate needs),
        // plus any P&L this order just realized by selling at market (autonomous exits).
        let last_realized: Decimal = sqlx::query_scalar(
            "SELECT realized_pnl FROM paper_trading.virtual_portfolio_snapshots ORDER BY as_of DESC LIMIT 1",
        )
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(dec!(0));
        let realized_agg = last_realized + realized_delta;
        // Unrealized is recomputed LIVE (2026-07-21 fix — same root cause as the 2026-07-20
        // settlement fix, see compute_live_unrealized_pnl's doc comment in main.rs), not carried
        // forward: the prior "carry forward last_unrealized" (2026-07-15 fix, replacing an earlier
        // hardcoded 0) still double-counted an autonomous EXIT sell for one cycle — a closing sell's
        // gain/loss landed in the fresh realized_delta above AND stayed in the stale carried-forward
        // unrealized (which still counted the position at its pre-sale mark) until the next
        // mark_to_market tick recomputed it, spiking the P&L chart and snapping back ~5min later
        // (confirmed live 2026-07-21 12:45 UTC: a post_fill_tx snapshot read +20.41 total, the next
        // mark_to_market read +15.50). The position upsert above already zeroed this order's shares
        // if it was a full close, so this recompute (within the same tx, reading its own write)
        // naturally excludes it — same `shares > 0` query, run against `&mut *tx` instead of the pool
        // so it sees the just-updated row before commit.
        let last_unrealized: Decimal = sqlx::query_scalar(
            "SELECT COALESCE(SUM(
                 p.shares * (
                     CASE WHEN p.outcome = 'Yes' THEN m.last_mid_yes ELSE m.last_mid_no END
                     - p.avg_entry_price
                 )
             ), 0)
             FROM paper_trading.paper_positions p
             JOIN market_data.markets m ON m.gamma_id = p.market_id
             WHERE p.shares > 0
               AND (CASE WHEN p.outcome = 'Yes' THEN m.last_mid_yes ELSE m.last_mid_no END) IS NOT NULL",
        )
        .fetch_one(&mut *tx)
        .await
        .unwrap_or(dec!(0));
        // Cash identity: seed − open cost basis − fees + realized P&L (settlements + market exits).
        let new_usdc = (Decimal::from(10000u64) - total_locked_agg - total_fees_agg + realized_agg)
            .max(dec!(0));
        sqlx::query(
            r#"INSERT INTO paper_trading.virtual_portfolio_snapshots (as_of, virtual_usdc, total_locked, unrealized_pnl, realized_pnl, snapshot_reason, positions)
               VALUES (now(), $1, $2, $3, $4, 'post_fill_tx', '[]'::jsonb)"#,
        )
        .bind(new_usdc)
        .bind(total_locked_agg)
        .bind(last_unrealized)
        .bind(realized_agg)
        .execute(&mut *tx)
        .await?;

        // 6. Final order status (journal on pool)
        order.status = if fills.iter().map(|f| f.size).sum::<Decimal>() >= order.size {
            OrderStatus::Filled
        } else {
            OrderStatus::PartiallyFilled
        };
        self.journal.record_paper_order(&order).await?;

        tx.commit().await.context("commit submit tx")?;

        tracing::info!(order_id = %order.id, num_fills = fills.len(), "paper order executed and journaled (full tx + FOR UPDATE delivered)");
        Ok(fills)
    }

    /// P5 increment 3 — pre-flight a multi-leg basket against every leg's book AT ONE INSTANT,
    /// committing nothing.
    ///
    /// The problem this exists to solve: `submit_order` commits each leg independently, so a
    /// basket executed as a `for` loop over its legs can buy legs 1-2, fail on leg 3, and leave us
    /// holding a DIRECTIONAL position where an arb was intended. A buy-all-No negRisk basket pays at
    /// least (legs-1) per unit only when it is COMPLETE; an incomplete one degrades to a floor of
    /// (filled_legs - 1), which is no floor at all.
    ///
    /// Measured on this system over 2026-08-02..08 (119 settled events): 97 complete baskets earned
    /// +$1,297.79 with a worst single event of **-$0.50** (fee/rounding noise, the floor holding),
    /// while 22 materially-partial baskets lost -$56.64 with a worst event of **-$22.02** and 16 of
    /// 22 losing. The difference in means is significant (t ~ 3.07); more to the point it is
    /// structural rather than statistical — a complete basket *cannot* lose.
    ///
    /// What this does NOT claim: real venues offer no atomic cross-market fill, and neither does
    /// this. What a real implementation can do is exactly what happens here — read every book,
    /// decide, then fire. So "pre-flight, then commit all or nothing" is a FAITHFUL simulation of
    /// the achievable execution, not an optimistic one. The residual — the book moving between the
    /// plan and the commit — is real, survives this change, and is measured by the caller comparing
    /// the plan against the fills it actually got.
    ///
    /// Fills are planned against the same source `submit_order` will match on (live WS book where
    /// one is fresh and in sync, polled snapshot otherwise), so the plan and the commit agree by
    /// construction unless the book genuinely moves in between.
    ///
    /// Note that each leg's book is therefore loaded TWICE — once here and once by `submit_order`.
    /// That is deliberate, not an oversight to optimise away. Caching the planned book and handing
    /// it to the commit would make the plan right by construction and destroy the only measurement
    /// that matters: `preflight_missed`, the rate at which the book moves between the decision and
    /// the fill. This runs on the execution path (a handful of baskets an hour), not the scan path,
    /// so the cost is a few extra reads against liquidity we are about to spend real size on.
    pub async fn plan_basket(&self, orders: &[PaperOrder]) -> Result<BasketPlan> {
        let mut legs = Vec::with_capacity(orders.len());
        for order in orders {
            let book = self
                .load_latest_book_snapshot(&order.market_id, &order.outcome)
                .await
                .context("loading book snapshot for basket pre-flight")?;
            let had_book = book.is_some();
            let mut probe = order.clone();
            let fills = self.match_against_book(&mut probe, book.as_ref()).await?;

            // Count ONLY liquidity that a real book actually showed. `match_against_book` fills a
            // MARKET order's remainder off the last known mid and tags it `synthetic_no_book`;
            // treating that as fillable would let a basket pass pre-flight on liquidity that does
            // not exist. Basket legs are limit orders today (so this cannot currently trigger), but
            // the guard is what makes that a property of the code rather than of the caller.
            let real: Vec<&PaperFill> = fills
                .iter()
                .filter(|f| {
                    f.against_book
                        .as_ref()
                        .and_then(|b| b.get("source"))
                        .and_then(|s| s.as_str())
                        != Some("synthetic_no_book")
                })
                .collect();

            legs.push(BasketLegPlan {
                market_id: order.market_id.clone(),
                outcome: order.outcome.clone(),
                requested: order.size,
                fillable: real.iter().map(|f| f.size).sum(),
                cost: real.iter().map(|f| f.price * f.size + f.fee).sum(),
                had_book,
            });
        }
        Ok(BasketPlan { legs })
    }

    /// Map (market_id, "Yes"/"No") -> token_id via DB, fetch latest snapshot row, parse jsonb.
    async fn load_latest_book_snapshot(
        &self,
        market_id: &str,
        outcome: &str,
    ) -> Result<Option<OrderbookSnapshot>> {
        // Fetch tokens + outcomes ordering from markets
        let row = sqlx::query(
            "SELECT clob_token_ids, outcomes FROM market_data.markets WHERE gamma_id = $1",
        )
        .bind(market_id)
        .fetch_optional(&self.pool)
        .await?;

        let (tokens, outcomes): (Vec<String>, Vec<String>) = if let Some(r) = row {
            let t: Vec<String> =
                serde_json::from_value(r.get("clob_token_ids")).unwrap_or_default();
            let o: Vec<String> = serde_json::from_value(r.get("outcomes")).unwrap_or_default();
            (t, o)
        } else {
            return Ok(None);
        };

        let idx = outcomes
            .iter()
            .position(|o| o.eq_ignore_ascii_case(outcome))
            .unwrap_or(0);
        let token_id = match tokens.get(idx) {
            Some(t) if !t.is_empty() => t.clone(),
            _ => return Ok(None),
        };

        // Prefer the live book. `get_fresh` withholds anything desynced or on a dead connection,
        // so falling through to the snapshot covers both "no feed" and "a feed we do not trust".
        if let Some(live) = self
            .live_books
            .as_ref()
            .and_then(|b| b.get_fresh(&token_id))
        {
            let to_levels = |v: Vec<(Decimal, Decimal)>| {
                v.into_iter()
                    .map(|(p, s)| PriceSize {
                        price: p.to_string(),
                        size: s.to_string(),
                    })
                    .collect::<Vec<_>>()
            };
            let mid = live.mid();
            return Ok(Some(OrderbookSnapshot {
                token_id,
                bids: to_levels(live.bid_levels()),
                asks: to_levels(live.ask_levels()),
                mid,
                fetched_at: chrono::Utc::now(),
            }));
        }

        // Latest snapshot for that token
        let snap_row = sqlx::query(
            "SELECT bids, asks, mid, fetched_at FROM market_data.orderbook_snapshots \
             WHERE token_id = $1 ORDER BY fetched_at DESC LIMIT 1",
        )
        .bind(&token_id)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(r) = snap_row {
            let bids: Vec<PriceSize> = serde_json::from_value(r.get("bids")).unwrap_or_default();
            let asks: Vec<PriceSize> = serde_json::from_value(r.get("asks")).unwrap_or_default();
            let mid: Option<Decimal> = r.get("mid");
            let fetched_at: chrono::DateTime<chrono::Utc> = r.get("fetched_at");
            Ok(Some(OrderbookSnapshot {
                token_id,
                bids,
                asks,
                mid,
                fetched_at,
            }))
        } else {
            Ok(None)
        }
    }

    /// Core matching. Decimal arithmetic only. Walks book levels for realism.
    /// Market orders: consume depth, apply simple cumulative slippage on top of vwap-ish.
    /// Limit orders: only fill at or better than limit.
    async fn match_against_book(
        &self,
        order: &mut PaperOrder,
        book_opt: Option<&OrderbookSnapshot>,
    ) -> Result<Vec<PaperFill>> {
        // RISK (per AGENTS.md + fees-tax-latency-and-execution-tiers.md for $150 capital):
        // - This fee_rate + model must be pessimistic; real taker fees + gas can vary.
        // - Net edge (gross - fees - gas - slip) is the *primary* signal for deliberate tier.
        // - Always journal fills with full context so Hermes can attribute fee drag vs signals.
        // - Paper only: no real money impact. Over-estimating fees protects learning capital.
        // Fee model: Polymarket's REAL taker fee (shares × rate × p × (1−p); geopolitics is fee-free) —
        // see crate::polymarket_fee. Replaces the old flat paper_fee_bps × notional. The per-market rate
        // synced from Gamma (`taker_fee_rate`) is preferred; the category default is the fallback. Rate
        // and slug are looked up once per order.
        let (market_slug, stored_fee_rate): (String, Option<Decimal>) = sqlx::query_as(
            "SELECT COALESCE(slug, ''), taker_fee_rate FROM market_data.markets WHERE gamma_id = $1",
        )
        .bind(&order.market_id)
        .fetch_optional(&self.pool)
        .await?
        .unwrap_or_default();
        let fee_rate =
            stored_fee_rate.unwrap_or_else(|| crate::polymarket_taker_fee_rate(&market_slug));
        let mut remaining = order.size;
        let mut fills = vec![];
        let now = chrono::Utc::now();

        if remaining <= dec!(0) {
            return Ok(fills);
        }

        // Determine side to walk first (Buy hits asks/sellers; Sell hits bids).
        let is_buy = matches!(order.side, OrderSide::Buy);

        // Fallback synthetic book if no snapshot yet (use mid from markets if present).
        // Clone *only* the side we will walk (avoids unnecessary clone of the other side's Vec).
        let (levels_vec, base_mid) = if let Some(book) = book_opt {
            if is_buy {
                (book.asks.clone(), book.mid)
            } else {
                (book.bids.clone(), book.mid)
            }
        } else {
            // Synthetic from last known mid in markets table (Yes/No)
            let mid_col = if order.outcome.eq_ignore_ascii_case("yes") {
                "last_mid_yes"
            } else {
                "last_mid_no"
            };
            let q = format!(
                "SELECT {} as mid FROM market_data.markets WHERE gamma_id = $1",
                mid_col
            );
            let mid: Option<Decimal> = sqlx::query(&q)
                .bind(&order.market_id)
                .fetch_optional(&self.pool)
                .await?
                .and_then(|r| r.get("mid"));
            (vec![], mid)
        };

        // CRITICAL: the walk below assumes the BEST price is first, but CLOB snapshots are not
        // guaranteed best-first (observed asks sorted worst-first: [0.999, …, 0.168]). Without this
        // sort, a market buy fills the most expensive asks first (catastrophic overspend) and a
        // limit buy breaks on the first over-limit level (never fills). Sort best-first explicitly:
        // asks ascending (lowest first) for buys, bids descending (highest first) for sells.
        let mut levels_vec = levels_vec;
        levels_vec.sort_by(|a, b| {
            let pa = Decimal::from_str(&a.price).unwrap_or(Decimal::ZERO);
            let pb = Decimal::from_str(&b.price).unwrap_or(Decimal::ZERO);
            if is_buy {
                pa.cmp(&pb)
            } else {
                pb.cmp(&pa)
            }
        });
        let levels: &[PriceSize] = &levels_vec; // for the for loop below (owned vec from clone or empty)

        let mut filled_size = dec!(0);
        let mut total_cost = dec!(0); // for vwap-ish

        for level in levels {
            if remaining <= dec!(0) {
                break;
            }
            let level_price = Decimal::from_str(&level.price).context("parse book price")?;
            let level_size = Decimal::from_str(&level.size).unwrap_or_else(|_| {
                tracing::warn!(market=%order.market_id, outcome=%order.outcome, "bad size in orderbook snapshot; using 0 (ingest may need attention)");
                dec!(0)
            });

            // Limit price check
            if let Some(lim) = order.limit_price {
                if is_buy && level_price > lim {
                    break; // ask too expensive
                }
                if !is_buy && level_price < lim {
                    break; // bid too low
                }
            }

            let take = if level_size > remaining {
                remaining
            } else {
                level_size
            };
            if take <= dec!(0) {
                continue;
            }

            // Simple depth slippage for MARKET orders only (extra bps on marginal levels)
            let mut exec_price = level_price;
            if matches!(order.order_type, OrderType::Market) {
                // Impact: 2bps per 1000 shares or simple linear (conservative for thin books)
                let impact_bps = (take / dec!(1000)) * dec!(2);
                let impact = impact_bps / dec!(10000);
                if is_buy {
                    exec_price = level_price + (level_price * impact);
                } else {
                    exec_price = level_price - (level_price * impact);
                }
            }

            let gross = exec_price * take;
            let fee = crate::polymarket_fee(fee_rate, exec_price, take);
            let slippage_bps = if let Some(m) = base_mid {
                // rough vs mid
                ((exec_price - m).abs() / m * dec!(10000))
                    .to_u32()
                    .unwrap_or(0) as i32
            } else {
                0
            };

            let fill = PaperFill {
                id: uuid::Uuid::new_v4(),
                order_id: order.id,
                price: exec_price,
                size: take,
                fee,
                slippage_bps: slippage_bps.min(500), // cap for bootstrap
                created_at: now,
                // Audit trail: the actual level consumed plus the book's best price and depth, so a
                // later investigation can tell a real book walk from the synthetic fallback below
                // WITHOUT re-deriving it from orderbook_snapshots (which are not sorted best-first).
                against_book: Some(serde_json::json!({
                    "source": "book_walk",
                    "level_price": level_price.to_string(),
                    "level_size": level_size.to_string(),
                    "taken": take.to_string(),
                    "best_price": levels.first().map(|l| l.price.clone()),
                    "levels_available": levels.len(),
                    "mid_at_fill": base_mid.map(|m| m.to_string()),
                })),
            };

            total_cost += gross;
            filled_size += take;
            remaining -= take;
            fills.push(fill);
        }

        // If still remaining after book (or no book) for MARKET: fill the rest at synthetic price (last mid + impact)
        if matches!(order.order_type, OrderType::Market) && remaining > dec!(0) {
            let base = base_mid.unwrap_or(dec!(0.5));
            let impact = (remaining / dec!(5000)) * dec!(0.01); // up to 1% extra for huge size (use *remaining* after partial book consumption)
            let synth_price = if is_buy {
                base + base * impact
            } else {
                base - base * impact
            };
            let fee = crate::polymarket_fee(fee_rate, synth_price, remaining);
            fills.push(PaperFill {
                id: uuid::Uuid::new_v4(),
                order_id: order.id,
                price: synth_price,
                size: remaining,
                fee,
                slippage_bps: ((impact * dec!(10000)).to_u32().unwrap_or(0) as i32).min(200),
                created_at: now,
                // Explicitly flagged: NO real liquidity backed this portion — it was priced off the
                // last known mid. Anything marked `synthetic` should be treated as optimistic and is
                // the first thing to check when a fill looks too good.
                against_book: Some(serde_json::json!({
                    "source": "synthetic_no_book",
                    "base_mid": base.to_string(),
                    "impact": impact.to_string(),
                    "levels_available": levels.len(),
                    "note": "market order remainder filled off last-known mid; no book liquidity matched",
                })),
            });
            filled_size += remaining;
            // remaining = 0;
        }

        if !fills.is_empty() {
            tracing::info!(
                order_id = %order.id,
                fills = fills.len(),
                filled = %filled_size,
                "generated paper fills (all Decimal math, book or synthetic)"
            );
        }
        Ok(fills)
    }

    /// Update paper_positions and record a new portfolio snapshot after the trade.
    async fn update_positions_and_snapshot(
        &self,
        order: &PaperOrder,
        fills: &[PaperFill],
    ) -> Result<()> {
        // Aggregate fills for this outcome
        let mut delta_shares: Decimal = dec!(0);
        let mut volume: Decimal = dec!(0); // for avg
        let mut total_fee: Decimal = dec!(0);

        for f in fills {
            let signed = if matches!(order.side, OrderSide::Buy) {
                f.size
            } else {
                -f.size
            };
            delta_shares += signed;
            volume += f.size; // simplistic
            total_fee += f.fee;
        }

        // Compute total gross proceeds/cost from fills for accurate cash accounting (fixes missing buy deduction bug)
        let total_gross: Decimal = fills.iter().map(|f| f.price * f.size).sum();
        if delta_shares == dec!(0) {
            return Ok(());
        }

        // Load or init current position
        let current: Option<(Decimal, Decimal, Decimal)> = sqlx::query_as(
            "SELECT shares, avg_entry_price, collateral_locked FROM paper_trading.paper_positions \
             WHERE market_id = $1 AND outcome = $2",
        )
        .bind(&order.market_id)
        .bind(&order.outcome)
        .fetch_optional(&self.pool)
        .await?;

        let (old_shares, old_avg, _old_coll) = current.unwrap_or((dec!(0), dec!(0), dec!(0)));

        let new_shares = old_shares + delta_shares;
        let new_avg = if new_shares > dec!(0) && volume > dec!(0) {
            if old_shares > dec!(0) {
                (old_shares * old_avg
                    + volume * /* approx */ fills.first().map(|f| f.price).unwrap_or(dec!(0.5)))
                    / new_shares
            } else {
                fills.first().map(|f| f.price).unwrap_or(dec!(0.5))
            }
        } else {
            dec!(0)
        };
        let new_coll = if new_shares > dec!(0) {
            new_shares * new_avg.abs()
        } else {
            dec!(0)
        };

        // Upsert position
        sqlx::query(
            r#"INSERT INTO paper_trading.paper_positions (market_id, outcome, shares, avg_entry_price, collateral_locked, last_updated)
               VALUES ($1,$2,$3,$4,$5, now())
               ON CONFLICT (market_id, outcome) DO UPDATE SET
                 shares = EXCLUDED.shares,
                 avg_entry_price = EXCLUDED.avg_entry_price,
                 collateral_locked = EXCLUDED.collateral_locked,
                 last_updated = now()"#,
        )
        .bind(&order.market_id)
        .bind(&order.outcome)
        .bind(new_shares)
        .bind(new_avg)
        .bind(new_coll)
        .execute(&self.pool)
        .await?;

        // Simple portfolio delta (bootstrap: fees reduce cash; no full mark-to-market yet)
        // Load last snapshot or seed
        let last_snap: Option<(Decimal, Decimal, Decimal, Decimal)> = sqlx::query_as(
            "SELECT virtual_usdc, total_locked, unrealized_pnl, realized_pnl FROM paper_trading.virtual_portfolio_snapshots ORDER BY as_of DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?;

        let (last_usdc, _last_locked, last_unrl, last_rl) = last_snap.unwrap_or((
            Decimal::from(10000u64), // fallback; better seeded in main
            dec!(0),
            dec!(0),
            dec!(0),
        ));

        let realized_delta = if delta_shares < dec!(0) {
            // simplistic sell pnl
            -delta_shares
                * (/* sell price avg */fills.last().map(|f| f.price).unwrap_or(dec!(0.5)) - old_avg)
        } else {
            dec!(0)
        };

        // Proper cash flow (double-entry style for paper):
        // Buy: cash outflow = gross cost + fee
        // Sell: cash inflow = gross proceeds - fee + realized_pnl
        let cash_flow = if delta_shares > dec!(0) {
            -(total_gross + total_fee)
        } else {
            total_gross - total_fee + realized_delta
        };
        let new_usdc = (last_usdc + cash_flow).max(dec!(0));
        let new_locked = new_coll; // approx
        let snap = VirtualPortfolio {
            virtual_usdc: new_usdc.max(dec!(0)),
            total_locked: new_locked,
            unrealized_pnl: last_unrl, // TODO mark to market in future
            realized_pnl: last_rl + realized_delta,
            as_of: chrono::Utc::now(),
        };

        // Fetch current positions for snapshot denorm
        let positions: Vec<PaperPosition> = sqlx::query_as(
            "SELECT market_id, outcome, shares, avg_entry_price, collateral_locked FROM paper_trading.paper_positions",
        )
        .fetch_all(&self.pool)
        .await?;

        self.journal
            .record_portfolio_snapshot(&snap, "post_fill", &positions)
            .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leg(requested: &str, fillable: &str) -> BasketLegPlan {
        BasketLegPlan {
            market_id: "m".to_string(),
            outcome: "No".to_string(),
            requested: Decimal::from_str(requested).unwrap(),
            fillable: Decimal::from_str(fillable).unwrap(),
            cost: dec!(0),
            had_book: true,
        }
    }

    /// The whole point of the increment: a basket is executable only when EVERY leg can fill in
    /// full. "Nearly all of it" is the failure mode, not a partial success — the >= (legs-1) payout
    /// floor exists at a common unit count across all legs or it does not exist.
    #[test]
    fn a_basket_is_executable_only_when_every_leg_fills_in_full() {
        let all_good = BasketPlan {
            legs: vec![leg("100", "100"), leg("100", "250"), leg("100", "100")],
        };
        assert!(all_good.is_executable());
        assert_eq!(all_good.short_legs(), 0);

        // One leg 1 share short out of 300 requested across the basket. The old sequential loop
        // would have bought the other two legs and held a directional position.
        let one_short = BasketPlan {
            legs: vec![leg("100", "100"), leg("100", "99"), leg("100", "100")],
        };
        assert!(!one_short.is_executable());
        assert_eq!(one_short.short_legs(), 1);
    }

    /// An empty plan must NOT read as executable. `Iterator::all` is vacuously true on an empty
    /// collection, so the natural one-liner would wave through a basket with no legs at all —
    /// which is exactly what a bug upstream (an opportunity whose legs failed to build) looks like.
    #[test]
    fn an_empty_basket_is_not_executable() {
        assert!(!BasketPlan { legs: vec![] }.is_executable());
    }

    /// The binding constraint is the WORST leg, not the average. A basket with three perfect legs
    /// and one that can supply a tenth is a tenth of a basket, and reporting ~78% would make an
    /// unexecutable basket look marginal.
    #[test]
    fn worst_fill_ratio_reports_the_binding_leg_not_the_average() {
        let plan = BasketPlan {
            legs: vec![
                leg("100", "100"),
                leg("100", "100"),
                leg("100", "100"),
                leg("100", "10"),
            ],
        };
        assert_eq!(plan.worst_fill_ratio(), dec!(0.1));
        // Over-deep legs cap at 1 so a single very deep leg cannot mask a starved one by pulling
        // the ratio above parity.
        let deep = BasketPlan {
            legs: vec![leg("100", "9999"), leg("100", "50")],
        };
        assert_eq!(deep.worst_fill_ratio(), dec!(0.5));
    }

    /// Diagnostics must not panic on a degenerate input. A zero-size request cannot arise from the
    /// sizing path today, but `fill_ratio` is called on every scan and a division by zero here
    /// would take down the executor rather than skip a basket.
    #[test]
    fn a_zero_size_leg_does_not_divide_by_zero() {
        assert_eq!(leg("0", "0").fill_ratio(), dec!(1));
        assert!(leg("0", "0").is_complete());
    }

    /// Only the short legs are journaled, with enough detail to tell a thin book from a missing
    /// one — `had_book` distinguishes "the market has no depth at our limit" from "we never
    /// ingested this market", which need different fixes.
    #[test]
    fn short_leg_detail_names_only_the_legs_that_failed() {
        let mut missing = leg("100", "0");
        missing.had_book = false;
        missing.market_id = "gone".to_string();
        let plan = BasketPlan {
            legs: vec![leg("100", "100"), missing, leg("300", "100")],
        };
        let detail = plan.short_leg_detail();
        assert_eq!(detail.len(), 2, "complete legs are not worth journaling");
        assert_eq!(detail[0]["market_id"], "gone");
        assert_eq!(detail[0]["had_book"], false);
        assert_eq!(detail[0]["fill_ratio"], "0");
        // A thin book, distinguishable from the missing one above by `had_book` alone -- the two
        // need different fixes (widen the limit / size down, versus repair ingest).
        assert_eq!(detail[1]["had_book"], true);
        assert_eq!(detail[1]["fill_ratio"], "0.3333");
        assert_eq!(detail[1]["requested"], "300");
    }
}
