//! Autonomous position exits (paper-only).
//!
//! Buy-and-hold-until-resolution starves the learning loop: directional markets resolve in
//! weeks-to-years, so Hermes gets no realized win/loss feedback and capital sits locked in stale
//! theses. This evaluator turns every position into a bounded round-trip — it closes at market
//! (Sell walks the live bid book via the paper engine) when any rule fires:
//!
//!  - **take-profit**: mark ≥ entry × (1 + POLYTRADER_EXIT_TAKE_PROFIT_PCT)
//!  - **stop-loss**:   mark ≤ entry × (1 − POLYTRADER_EXIT_STOP_LOSS_PCT)
//!  - **time-stop**:   held longer than POLYTRADER_EXIT_MAX_HOLD_DAYS (thesis went nowhere)
//!  - **signal-flip**: the latest decision report targets the OPPOSITE outcome with net edge ≥
//!    the live gate (the system now believes the other side is the value side)
//!
//! Resolved/closed markets are NOT exited here — the settlement path owns those (selling into a
//! dead book would fill against stale bids). Gated by POLYTRADER_AUTONOMOUS_EXITS=on.

use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use sqlx::PgPool;
use std::sync::Arc;

use crate::journal::JournalWriter;
use crate::paper::{OrderSide, OrderType, PaperOrder, PaperTradingEngine};

fn dec_env(key: &str, default: Decimal) -> Decimal {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// One evaluation pass over all open positions. Called from the 5-min decision cadence. Errors on
/// one position never abort the pass.
pub async fn evaluate_exits(
    pool: &PgPool,
    engine: &Arc<PaperTradingEngine>,
    journal: &Arc<JournalWriter>,
) {
    let enabled = std::env::var("POLYTRADER_AUTONOMOUS_EXITS")
        .map(|v| v.trim().eq_ignore_ascii_case("on"))
        .unwrap_or(false);
    if !enabled {
        return;
    }
    let take_profit = dec_env("POLYTRADER_EXIT_TAKE_PROFIT_PCT", dec!(0.25));
    // Stop-loss WIDENED 0.15 → 0.50 on 2026-07-09 after a P&L-by-realization-type decomposition:
    // since the 07-04 reset EVERY loss came from exits (−$88 over 38 exits, of which stop-loss was
    // ~−$69), while EVERY position held to resolution WON (settlements +$4.44, 3/3). A prediction-
    // market position is already BOUNDED — a share heading to $0 can lose at most its entry cost,
    // there is no leverage/blowup tail for a tight stop to protect against — and these prices
    // mean-revert short-term, so a −15% stop systematically SELLS NOISE and pays friction to do it.
    // Evidence: post-07-06-fix stops still fired right at the −15.6% threshold, held only 0.6–3.5d,
    // six of eight on the correlated WTI ladder (one oil wobble trips the whole ladder). At 0.50 the
    // stop only fires on a genuine thesis-COLLAPSE (a position that has halved), while ordinary
    // wobble rides through to resolution or the time-stop. The time-stop still frees dead capital,
    // take-profit still locks real gains. Reversible via env; abs-move floor unchanged.
    let stop_loss = dec_env("POLYTRADER_EXIT_STOP_LOSS_PCT", dec!(0.50));
    // Absolute-move floor for the stop: a small RELATIVE drop on a cheap share is pennies of pure
    // bid/ask noise (a 0.18 entry stops on a 2.7¢ wobble — 9 such stops bled −$54 overnight
    // 2026-07-05→06). The stop only fires when the mid has ALSO moved this much in absolute
    // price. High-priced entries are unaffected (their 50% is already > the floor).
    let min_abs_move = dec_env("POLYTRADER_EXIT_MIN_ABS_MOVE", dec!(0.04));
    let max_hold_days = dec_env("POLYTRADER_EXIT_MAX_HOLD_DAYS", dec!(14));
    let min_net_edge = crate::risk::RiskConfig::from_env().min_net_edge;

    // Open positions on live (tradeable) markets with a fresh mark. updated_at < 30 min guards
    // against exiting on a stale mid when ingestion has hiccuped for this market.
    // (market_id, outcome, shares, avg_entry, mark_mid, slug, opened_at)
    type PosRow = (
        String,
        String,
        Decimal,
        Decimal,
        Option<Decimal>,
        String,
        Option<chrono::DateTime<chrono::Utc>>,
    );
    let rows: Vec<PosRow> = sqlx::query_as(
        "SELECT p.market_id, p.outcome, p.shares, p.avg_entry_price,
                CASE WHEN p.outcome = 'Yes' THEN m.last_mid_yes ELSE m.last_mid_no END,
                m.slug,
                -- Entry time = first Buy fill since the last paper reset. The no-pyramiding guard
                -- means a position has exactly one entry order, so this is exact, not a proxy.
                (SELECT min(f.created_at)
                   FROM paper_trading.paper_fills f
                   JOIN paper_trading.paper_orders o ON o.id = f.order_id
                  WHERE o.market_id = p.market_id AND o.outcome = p.outcome AND o.side = 'Buy'
                    AND f.created_at >= COALESCE(
                      (SELECT max(as_of) FROM paper_trading.virtual_portfolio_snapshots
                        WHERE snapshot_reason = 'manual_paper_reset'), '-infinity'::timestamptz))
         FROM paper_trading.paper_positions p
         JOIN market_data.markets m ON m.gamma_id = p.market_id
         WHERE p.shares > 0
           AND m.closed = false
           AND m.resolved_outcome IS NULL
           AND m.updated_at > now() - interval '30 minutes'
           -- ARB LEGS ARE HOLD-TO-RESOLUTION BY DESIGN: their profit is the guaranteed payout
           -- structure ACROSS legs (Yes+NO pair < $1; negrisk basket pays >= k-1), and selling any
           -- leg re-introduces exactly the risk the structure eliminated. 2026-07-04: this
           -- evaluator sold 5 of 11 legs of a risk-free negrisk basket into in-play exact-score
           -- price swings, turning a guaranteed +$1.21 into -$4.01. Any position whose entry came
           -- from an arb executor is therefore invisible to TP/SL/time-stop/signal-flip.
           AND NOT EXISTS (
             SELECT 1 FROM paper_trading.paper_orders o
              WHERE o.market_id = p.market_id AND o.outcome = p.outcome AND o.side = 'Buy'
                AND o.created_at >= COALESCE(
                  (SELECT max(as_of) FROM paper_trading.virtual_portfolio_snapshots
                    WHERE snapshot_reason = 'manual_paper_reset'), '-infinity'::timestamptz)
                AND o.decision_context->>'source'
                    IN ('autonomous_arb_executor', 'autonomous_negrisk_arb_executor'))",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    for (market_id, outcome, shares, avg_entry, mid, slug, opened_at) in rows {
        let Some(mid) = mid else { continue };
        if avg_entry <= dec!(0) || shares <= dec!(0) {
            continue;
        }
        let move_pct = (mid - avg_entry) / avg_entry;
        let held_days = opened_at
            .map(|t| Decimal::from((chrono::Utc::now() - t).num_seconds()) / dec!(86400))
            .unwrap_or(dec!(0));

        let reason = if move_pct >= take_profit {
            Some("take_profit")
        } else if move_pct <= -stop_loss && (mid - avg_entry).abs() >= min_abs_move {
            Some("stop_loss")
        } else if held_days >= max_hold_days {
            Some("time_stop")
        } else if signal_flipped(pool, &market_id, &outcome, min_net_edge).await {
            Some("signal_flip")
        } else {
            None
        };
        let Some(reason) = reason else { continue };

        tracing::info!(market = %market_id, slug = %slug, outcome = %outcome, %shares,
            entry = %avg_entry, mark = %mid, move_pct = %move_pct.round_dp(4), reason,
            "autonomous exit: closing position at market (paper)");

        let order = PaperOrder {
            id: uuid::Uuid::new_v4(),
            market_id: market_id.clone(),
            outcome: outcome.clone(),
            side: OrderSide::Sell,
            order_type: OrderType::Market,
            limit_price: None,
            size: shares,
            status: crate::paper::OrderStatus::Open,
            created_at: chrono::Utc::now(),
            decision_context: Some(serde_json::json!({
                "source": "autonomous_exit",
                "reason": reason,
                "entry_avg": avg_entry.to_string(),
                "mark_mid": mid.to_string(),
                "move_pct": move_pct.round_dp(4).to_string(),
                "held_days": held_days.round_dp(2).to_string(),
                "paper_only": true,
            })),
        };
        match engine.submit_order(order).await {
            Ok(fills) if !fills.is_empty() => {
                let sold: Decimal = fills.iter().map(|f| f.size).sum();
                let proceeds: Decimal = fills.iter().map(|f| f.price * f.size).sum();
                let fees: Decimal = fills.iter().map(|f| f.fee).sum();
                let realized_gross = proceeds - sold * avg_entry;
                let vwap = if sold > dec!(0) { proceeds / sold } else { mid };
                let _ = journal
                    .record_journal_event(
                        "autonomous_paper_exit",
                        "polytrader_exits",
                        "info",
                        serde_json::json!({
                            "market_id": market_id,
                            "slug": slug,
                            "outcome": outcome,
                            "reason": reason,
                            "shares_sold": sold.to_string(),
                            "entry_avg": avg_entry.to_string(),
                            "exit_vwap": vwap.round_dp(4).to_string(),
                            "realized_gross": realized_gross.round_dp(4).to_string(),
                            "fees": fees.round_dp(4).to_string(),
                            "held_days": held_days.round_dp(2).to_string(),
                            "move_pct_at_decision": move_pct.round_dp(4).to_string(),
                            "paper_only": true,
                            "real_orders_enabled": false,
                            "note": "Position closed at market by the autonomous exit evaluator (TP/SL/time-stop/signal-flip). Realized P&L is captured in the post_fill_tx snapshot; this event is the per-trade record for Hermes round-trip learning.",
                        }),
                    )
                    .await;
                tracing::info!(market = %market_id, outcome = %outcome, %sold,
                    realized_gross = %realized_gross.round_dp(4), reason, "autonomous exit filled");
            }
            Ok(_) => {
                tracing::warn!(market = %market_id, outcome = %outcome, reason,
                    "autonomous exit rejected (no bid liquidity or stale book); will retry next cycle");
            }
            Err(e) => {
                tracing::warn!(market = %market_id, outcome = %outcome, error = %e, reason,
                    "autonomous exit failed; will retry next cycle");
            }
        }
    }
}

/// Latest decision report for this market targets the OPPOSITE outcome with a tradeable edge —
/// the fused signals now say the other side is the value side.
async fn signal_flipped(
    pool: &PgPool,
    market_id: &str,
    held_outcome: &str,
    min_net_edge: Decimal,
) -> bool {
    let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT payload->>'target_outcome', payload->'report'->>'net_edge_after_fees'
         FROM journal.events
         WHERE event_type = 'decision_report' AND payload->>'market_id' = $1
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(market_id)
    .fetch_optional(pool)
    .await
    .unwrap_or(None);
    let Some((Some(target), Some(edge))) = row else {
        return false;
    };
    let edge: Decimal = edge.parse().unwrap_or(Decimal::ZERO);
    !target.eq_ignore_ascii_case(held_outcome) && edge >= min_net_edge
}
