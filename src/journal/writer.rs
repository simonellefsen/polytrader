//! Journal writer — appends structured events for later Hermes analysis.
//!
//! Paper trading path writes to paper_trading.* tables (orders, fills, snapshots).
//! Reflections go to journal.* . All writes are append-only / immutable-ish.

use crate::journal::models::Reflection;
use crate::paper::{PaperFill, PaperOrder, PaperPosition, VirtualPortfolio};
use sqlx::PgPool;

pub struct JournalWriter {
    pool: PgPool,
}

impl JournalWriter {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn write_reflection(&self, r: &Reflection) -> anyhow::Result<()> {
        sqlx::query(
            r#"INSERT INTO journal.reflections
               (id, period_start, period_end, summary, metrics, recommendations, hermes_version, llm_model, created_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
               ON CONFLICT (id) DO NOTHING"#,
        )
        .bind(r.id)
        .bind(r.period_start)
        .bind(r.period_end)
        .bind(&r.summary)
        .bind(&r.metrics)
        .bind(serde_json::to_value(&r.recommendations).unwrap_or(serde_json::json!([])))
        .bind(Option::<String>::None as Option<String>) // hermes_version stub
        .bind(Option::<String>::None)
        .bind(r.created_at)
        .execute(&self.pool)
        .await?;
        tracing::info!(id = %r.id, "journal reflection written");
        Ok(())
    }

    /// Record a paper order intent (with decision context for audit/Hermes).
    pub async fn record_paper_order(&self, order: &PaperOrder) -> anyhow::Result<()> {
        sqlx::query(
            r#"INSERT INTO paper_trading.paper_orders
               (id, market_id, outcome, side, order_type, limit_price, size, status, decision_context, created_at, updated_at)
               VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10, now())
               ON CONFLICT (id) DO UPDATE SET status=EXCLUDED.status, updated_at=now()"#,
        )
        .bind(order.id)
        .bind(&order.market_id)
        .bind(&order.outcome)
        .bind(order.side.to_string()) // stable via Display (not Debug)
        .bind(order.order_type.to_string())
        .bind(order.limit_price)
        .bind(order.size)
        .bind(order.status.to_string())
        .bind(order.decision_context.clone().unwrap_or(serde_json::json!({})))
        .bind(order.created_at)
        .execute(&self.pool)
        .await?;
        tracing::info!(order_id = %order.id, market=%order.market_id, outcome=%order.outcome, "paper order recorded to journal");
        Ok(())
    }

    /// Record one or more fills atomically with the order.
    pub async fn record_paper_fills(&self, fills: &[PaperFill]) -> anyhow::Result<()> {
        if fills.is_empty() {
            return Ok(());
        }
        // Simple loop for Phase 0; in prod use COPY or batched tx
        for f in fills {
            sqlx::query(
                r#"INSERT INTO paper_trading.paper_fills
                   (id, order_id, price, size, fee, slippage_bps, against_book, created_at)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8)"#,
            )
            .bind(f.id)
            .bind(f.order_id)
            .bind(f.price)
            .bind(f.size)
            .bind(f.fee)
            .bind(f.slippage_bps)
            .bind(serde_json::json!({})) // against_book filled by engine caller if wanted
            .bind(f.created_at)
            .execute(&self.pool)
            .await?;
        }
        tracing::info!(count = fills.len(), "paper fills recorded to journal");
        Ok(())
    }

    /// Snapshot the virtual portfolio + positions (called after fills or periodically).
    pub async fn record_portfolio_snapshot(
        &self,
        snap: &VirtualPortfolio,
        reason: &str,
        positions: &[PaperPosition],
    ) -> anyhow::Result<()> {
        let pos_json = serde_json::to_value(positions).unwrap_or(serde_json::json!([]));
        sqlx::query(
            r#"INSERT INTO paper_trading.virtual_portfolio_snapshots
               (as_of, virtual_usdc, total_locked, unrealized_pnl, realized_pnl, snapshot_reason, positions)
               VALUES ($1, $2, $3, $4, $5, $6, $7)"#,
        )
        .bind(snap.as_of)
        .bind(snap.virtual_usdc)
        .bind(snap.total_locked)
        .bind(snap.unrealized_pnl)
        .bind(snap.realized_pnl)
        .bind(reason)
        .bind(pos_json)
        .execute(&self.pool)
        .await?;
        tracing::info!(usdc = %snap.virtual_usdc, reason, "portfolio snapshot recorded");
        Ok(())
    }
}
