//! Journal writer — appends structured events for later Hermes analysis.
//!
//! Paper trading path writes to paper_trading.* tables (orders, fills, snapshots).
//! Reflections go to journal.* . All writes are append-only / immutable-ish.

use crate::journal::models::Reflection;
use crate::paper::{PaperFill, PaperOrder, PaperPosition, VirtualPortfolio};
use rust_decimal::Decimal;
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

        // Wire minimal tax producer from the paper fill recording path (per fees-tax-latency-and-execution-tiers.md "treat every paper trade as if it will one day be real for record-keeping purposes" + "The journal should be capable of producing: Per-trade cost basis, Fees paid (deductible...), Realized P&L..." + goals-and-operational-cadence.md "Journal extensions (comments first)" + "Query recent fills... and all decision reports" + "backtest" + "Compare decision reports vs actual outcomes" + log/plan "Ready for next (e.g. fuller backtest harness on DRs vs paper fills ... or wire minimal tax producer)" + writer TODO).
        // Smallest: after recording the paper_fills batch, emit a tax_snapshot jsonb event (reuse record_tax_snapshot wrapper + its sanitize + record_journal_event; no new tables/kinds/migs; payload has fill count + total fee (for deductible) + note for cost basis proxy / future realized P&L reconstruction; source="paper_fills" for attribution).
        // RISK (AGENTS.md + fees-tax-latency-and-execution-tiers.md + goals-and-operational-cadence.md 'Journal extensions (comments first)' non-negotiable):
        // - append-only to journal.events (jsonb payload via reuse of record_journal_event);
        // - paper proxy only (treat every paper trade as if it will one day be real for record-keeping/audit-grade per fees wiki "Tax & Record-Keeping Strategy");
        // - never contains secrets, private keys, L2 HMACs, or anything authorizing real orders (sanitize guard at boundary);
        // - evidence-only for Hermes future attribution of net P&L after modeled 'tax' drag / cost basis (virtual tax reserve is later Phase 3+);
        // - Decimal strings in payload (rust_decimal for all money/price/position per AGENTS); follows exact prior DR generator + tax skeleton reuse pattern;
        // - called on every paper fill (from PaperTradingEngine path via this recording fn in main + tests exercising fills); enables backtest harness on DRs vs paper fills + tax-adjusted per goals without touching trading paths, real, paper defaults, fail-closed, L2, reval, envs, kill, 401s, SSR subpath/<base>, *any* old/polish/DR-stub marker, ui, generator, strategy, clob, hermes consumption (already present), or prior surfaces.
        // - tax_payload contains only aggregate counts + Decimal fees (to_string) + static note; never rationale/decision_context/secrets (sanitized; see sanitize_journal_payload + fees-tax 'treat every paper trade as if real' but evidence-only).
        // - Note: journal events including tax_snapshot use separate pool from the FOR UPDATE tx in submit_order (pre-existing design for audit separation; tax is append-only paper proxy evidence per AGENTS/fees-tax, not used for position math; non-fatal warn on snapshot err is robust like other journal; producer failure does not affect fill recording or paper engine).
        // See wiki/strategies/fees-tax-latency-and-execution-tiers.md + goals-and-operational-cadence.md + decisions/real-order-approval-flow.md (tax producer section) + log (new top entry) + writer::record_tax_snapshot + hermes do_reflection tax_journal_skeleton.
        let total_fee: Decimal = fills.iter().map(|f| f.fee).sum();
        let tax_payload = serde_json::json!({
            "fills_count": fills.len(),
            "total_fee": total_fee.to_string(),
            "note": "paper fill tax proxy snapshot (fees for deductible; size/price in paper_fills table for cost basis); per fees-tax 'treat every paper trade as if it will one day be real for record-keeping purposes' + goals 'Journal extensions'; append-only evidence for Hermes net-after-tax attr + backtest (DRs vs fills + tax-adjusted); limited skeleton (no reserve/calc yet); see writer record_tax_snapshot + sanitize"
        });
        if let Err(e) = self.record_tax_snapshot("paper_fills", tax_payload).await {
            tracing::warn!(error = %e, "tax snapshot from paper fills (non-fatal; skeleton producer; paper proxy only)");
        }
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

    /// Record a generic append-only journal event (reuse for decision reports, future kinds).
    ///
    /// RISK (AGENTS.md + schema + trading safety): append-only to journal.events (jsonb payload);
    /// never contains secrets, private keys, L2 HMACs, or anything that would authorize real orders.
    /// Follows *exact* server record_journal_event pattern (id=uuid v4, no created_at bind since DB default).
    /// Used by 5-min DR generator (DecisionReport net_edge jsonb) + will be used for other observable
    /// events. Hermes consumes via COUNT on event_type. Paper-only. No mig. Decimal/observability preserved.
    /// Duplication with server private (~9791) is acceptable per plan ("exact reuse" + "acceptable for this additive-only tranche (per smallest scope)"); TODO: future consolidate (server delegate to JournalWriter).
    /// + now reused for tax_snapshot skeleton per fees-tax (see record_tax_snapshot).
    ///
    ///   See wiki/schema.md (jsonb for decision reports), goals-and-operational-cadence.md (5-min DR logged),
    ///   decisions/real-order-approval-flow.md (DR generator wiring).
    pub async fn record_journal_event(
        &self,
        event_type: &str,
        source: &str,
        severity: &str,
        payload: serde_json::Value,
    ) -> anyhow::Result<uuid::Uuid> {
        let id = uuid::Uuid::new_v4();
        sqlx::query(
            r#"INSERT INTO journal.events (id, event_type, source, severity, payload)
               VALUES ($1, $2, $3, $4, $5)"#,
        )
        .bind(id)
        .bind(event_type)
        .bind(source)
        .bind(severity)
        .bind(&payload)
        .execute(&self.pool)
        .await?;
        // Changed to debug! (from info!) to address log volume nit at 5min cadence + LIMIT 3 (DR generator calls
        // this on success; server private remains info! for its higher-severity clob_/strategy_ paths).
        // Duplication of INSERT with server private record_journal_event is acceptable for this additive-only
        // tranche (per smallest scope); future: server can delegate to JournalWriter when extending.
        tracing::debug!(id = %id, event_type, "journal event recorded");
        Ok(id)
    }

    /// Minimal defense-in-depth sanitizer for journal payloads at new write boundaries (e.g. tax).
    /// Strips known sensitive keys recursively (defense for copy-paste errors by future producers);
    /// follows "never contains secrets" contract in schema + all RISK comments (machine-enforceable here for tax path).
    /// Current skeleton: no-op on most but removes e.g. l2_*, private*, secret*, key*, signature* (case-insens prefix).
    /// Callers (esp. record_tax_snapshot) must still honor documented contract; this is additive guard.
    /// (Per Issue 5 review nit; keeps as pattern for reuse in generic later if needed.)
    fn sanitize_journal_payload(v: serde_json::Value) -> serde_json::Value {
        use serde_json::Value;
        fn strip(val: Value) -> Value {
            match val {
                Value::Object(mut map) => {
                    let bad_prefixes = [
                        "l2_",
                        "private",
                        "secret",
                        "key",
                        "signature",
                        "hmac",
                        "auth",
                    ];
                    map.retain(|k, _| {
                        let kl = k.to_ascii_lowercase();
                        !bad_prefixes.iter().any(|p| kl.starts_with(p))
                    });
                    let new_map: serde_json::Map<_, _> =
                        map.into_iter().map(|(k, vv)| (k, strip(vv))).collect();
                    Value::Object(new_map)
                }
                Value::Array(arr) => Value::Array(arr.into_iter().map(strip).collect()),
                other => other,
            }
        }
        strip(v)
    }

    /// Record a tax snapshot / position record (for future cost basis, realized P&L, fees paid reconstruction per fees-tax wiki).
    ///
    /// RISK (AGENTS.md + fees-tax-latency-and-execution-tiers.md + goals-and-operational-cadence.md 'Journal extensions (comments first)' non-negotiable):
    /// - append-only to journal.events (jsonb payload via reuse of record_journal_event);
    /// - paper proxy only (treat every paper trade as if it will one day be real for record-keeping/audit-grade per fees wiki "Tax & Record-Keeping Strategy");
    /// - never contains secrets, private keys, L2 HMACs, or anything authorizing real orders;
    /// - evidence-only for Hermes future attribution of net P&L after modeled 'tax' drag / cost basis (virtual tax reserve is later Phase 3+);
    /// - Decimal strings in payload; follows exact prior DR generator reuse pattern (no new tables/kinds beyond event_type string "tax_snapshot"; no mig);
    /// - producer wire from paper_fills now live (2026-06-06 tranche; inside record_paper_fills after INSERTs; data flows on actual paper fills; will see >0 in runs exercising paper submit); called from paper fill path + future/manual/ops for other snapshots; Hermes lightly consumes counts/samples (skeleton vs production; limited, no reserve/calc yet; see fees/goals for fuller).
    /// - source is normalized (trim + <=128 chars); payload sanitized at boundary (Issue 5/6).
    ///
    ///   See wiki/strategies/fees-tax-latency-and-execution-tiers.md +
    ///   goals-and-operational-cadence.md +
    ///   decisions/real-order-approval-flow.md (tax producer section) +
    ///   log.md (this tranche).
    ///
    /// TODO(future): wire calls to record_tax_snapshot from paper fill paths or produce_5min (after DRs) per fees-tax 'treat every paper trade as if it will one day be real' + goals 'Journal extensions' + backtest tie-in; see wiki/log Current State.
    pub async fn record_tax_snapshot(
        &self,
        source: &str,
        payload: serde_json::Value,
    ) -> anyhow::Result<uuid::Uuid> {
        // Issue 6: source normalizer (trim + cap length; allow-list deferred to first real producer wiring; internal callers only).
        let norm_source: String = source.trim().chars().take(128).collect();
        // Issue 5: sanitize at dedicated tax boundary (reuse helper; strips dangerous keys before INSERT; full sample kept for backtest but LLM redacts per hermes).
        let safe_payload = Self::sanitize_journal_payload(payload);
        self.record_journal_event("tax_snapshot", &norm_source, "info", safe_payload)
            .await
    }
}
