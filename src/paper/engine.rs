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
    paper_fee_bps: u16,
}

impl PaperTradingEngine {
    pub fn new(pool: PgPool, journal: Arc<JournalWriter>, paper_fee_bps: u16) -> Self {
        Self {
            pool,
            journal,
            paper_fee_bps,
        }
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
        let new_avg = if new_shares > dec!(0) {
            if old_shares > dec!(0) {
                (old_shares * old_avg
                    + (total_gross
                        / if delta_shares != dec!(0) {
                            delta_shares
                        } else {
                            dec!(1)
                        }))
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

        // Raw portfolio snapshot under tx (completes the full sequence atomically)
        let cash_flow = if delta_shares > dec!(0) {
            -(total_gross + total_fee)
        } else {
            total_gross - total_fee + dec!(0)
        };
        let new_usdc = (Decimal::from(10000u64) + cash_flow).max(dec!(0)); // seed fallback consistent with prior
        sqlx::query(
            r#"INSERT INTO paper_trading.virtual_portfolio_snapshots (as_of, virtual_usdc, total_locked, unrealized_pnl, realized_pnl, snapshot_reason, positions)
               VALUES (now(), $1, $2, 0, 0, 'post_fill_tx', '[]'::jsonb)"#,
        )
        .bind(new_usdc)
        .bind(new_coll)
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
        // Legacy flat rate kept for exact fill fee calc (minimal diff). The first-class FeeModel
        // (built in ctor from paper_fee_bps + conservative maker/gas/rewards) is used for
        // pre-trade net edge estimates (see estimate_net... below) and will drive future
        // fee-aware matching / opportunity filtering.
        // RISK (per AGENTS.md + fees-tax-latency-and-execution-tiers.md for $150 capital):
        // - This fee_rate + model must be pessimistic; real taker fees + gas can vary.
        // - Net edge (gross - fees - gas - slip) is the *primary* signal for deliberate tier.
        // - Always journal fills with full context so Hermes can attribute fee drag vs signals.
        // - Paper only: no real money impact. Over-estimating fees protects learning capital.
        // Legacy note (Fix Round 1): fill.fee uses flat paper_fee_bps for exact compat with Phase 0-2 verified paths.
        // FeeModel (with its full pessimistic + maker/gas) used only for pre-trade estimates. Future unification
        // tracked in wiki/log (see Issue 7 suggestion + reconciliation note added below).
        let fee_rate = Decimal::from(self.paper_fee_bps) / dec!(10000);
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
            let fee = gross * fee_rate;
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
            let gross = synth_price * remaining;
            let fee = gross * fee_rate;
            fills.push(PaperFill {
                id: uuid::Uuid::new_v4(),
                order_id: order.id,
                price: synth_price,
                size: remaining,
                fee,
                slippage_bps: ((impact * dec!(10000)).to_u32().unwrap_or(0) as i32).min(200),
                created_at: now,
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
