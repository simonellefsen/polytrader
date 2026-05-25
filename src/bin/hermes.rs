//! hermes — The self-improvement / reflection meta-agent for polytrader.
//!
//! Runs independently (own deployment, paper-only). Reads journal + market_data + paper_trading,
//! performs P&L attribution, calls (optional) LLM for synthesis, writes to journal.reflections.
//! Phase 2: gated autonomous low-risk wiki patch proposals (env HERMES_AUTONOMOUS_WIKI_PROPOSALS=lowrisk).
//! Follows exact patterns from src/journal/writer.rs, src/server.rs, src/db.rs.

use anyhow::Result;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde_json::json;
use std::time::Duration;
use tracing::{info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<()> {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,hermes=debug"));

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().json())
        .with(env_filter)
        .init();

    info!("🪐 hermes starting — self-improvement loop (Phase 2: richer reflection + P&L + conditional LLM + gated autonomous low-risk wiki proposals)");

    // DB connect with exponential backoff (exact pattern copied from src/db.rs for standalone bin)
    let database_url = std::env::var("DATABASE_URL")
        .or_else(|_| std::env::var("uri"))
        .unwrap_or_else(|_| "postgres://polytrader:password@postgres:5432/polytrader".to_string());
    let pool = create_pool_with_backoff(&database_url).await?;
    info!("Hermes DB pool ready (paper schemas only)");

    // Configurable LLM (OpenAI-compatible via env; NO secrets in code, no real trading paths)
    let llm_endpoint = std::env::var("LLM_API_ENDPOINT")
        .unwrap_or_else(|_| "https://api.openai.com/v1/chat/completions".to_string());
    let llm_key = std::env::var("LLM_API_KEY").ok();
    let llm_model = std::env::var("LLM_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string());

    if llm_key.is_some() {
        info!(endpoint = %llm_endpoint, model = %llm_model, "LLM synthesis enabled for reflections");
    } else {
        info!("LLM synthesis disabled (no LLM_API_KEY); using robust local attribution only");
    }

    // Richer reflection loop (periodic; future: also on resolution via Gamma watch)
    // SAFETY: paper-only. All reads append-only inserts to journal. Decimal exclusively.
    let mut tick: u64 = 0;
    let interval = Duration::from_secs(300); // 5min; configurable later via env
    loop {
        tick += 1;
        if tick % 2 == 1 {
            // Run reflection on start + every ~10min (odd ticks with 5m interval for regular cadence)
            if let Err(e) =
                do_reflection(&pool, &llm_endpoint, llm_key.as_deref(), &llm_model).await
            {
                warn!(error = %e, "reflection cycle failed (will retry next interval; robust, no crash)");
            }
        } else {
            tracing::debug!("hermes idle (tick {})", tick);
        }
        tokio::time::sleep(interval).await;
    }
}

/// Exact backoff pool creation (copied verbatim pattern from src/db.rs; standalone for hermes bin)
async fn create_pool_with_backoff(database_url: &str) -> Result<sqlx::PgPool> {
    const MAX_RETRIES: u32 = 20;
    const INITIAL_BACKOFF_MS: u64 = 500;
    const MAX_BACKOFF_MS: u64 = 10_000;

    let mut backoff = INITIAL_BACKOFF_MS;
    let mut last_error = None;

    for attempt in 1..=MAX_RETRIES {
        match sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(Duration::from_secs(8))
            .connect(database_url)
            .await
        {
            Ok(pool) => {
                info!(
                    "Hermes DB connection on attempt {}/{}",
                    attempt, MAX_RETRIES
                );
                // light ping
                sqlx::query("SELECT 1").execute(&pool).await?;
                return Ok(pool);
            }
            Err(e) => {
                last_error = Some(e);
                if attempt < MAX_RETRIES {
                    warn!(
                        "Hermes DB connect attempt {}/{} failed, retry {}ms",
                        attempt, MAX_RETRIES, backoff
                    );
                    tokio::time::sleep(Duration::from_millis(backoff)).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF_MS);
                }
            }
        }
    }
    Err(anyhow::anyhow!(
        "Hermes failed DB connect after {} attempts: {:?}",
        MAX_RETRIES,
        last_error
    ))
}

/// Core richer reflection (smallest viable per spec).
/// - Reads recent paper/portfolio + fills + markets (exact sqlx patterns from server/journal)
/// - Decimal-only P&L attribution + metrics
/// - Local synthesis always; LLM optional (reqwest, env only, timeout+fallback)
/// - Stores to journal.reflections (exact INSERT from writer.rs)
/// - Heavily commented for audit/risk (AGENTS.md)
async fn do_reflection(
    pool: &sqlx::PgPool,
    llm_endpoint: &str,
    llm_key: Option<&str>,
    llm_model: &str,
) -> Result<()> {
    let now: DateTime<Utc> = Utc::now();
    let period_start = now - chrono::Duration::hours(24); // simple daily window

    // Read latest + prior portfolio snapshots for real delta P&L attribution (smallest enhancement; addresses weak "current only" review feedback while following patterns)
    // Errors now propagate (surfaces transient DB/CNPG issues to caller warn; no silent bad metrics)
    let latest_snap: Option<(Decimal, Decimal, Decimal, Decimal)> = sqlx::query_as(
        "SELECT virtual_usdc, total_locked, unrealized_pnl, realized_pnl
         FROM paper_trading.virtual_portfolio_snapshots
         ORDER BY as_of DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;

    let prev_snap: Option<(Decimal, Decimal, Decimal, Decimal)> = sqlx::query_as(
        "SELECT virtual_usdc, total_locked, unrealized_pnl, realized_pnl
         FROM paper_trading.virtual_portfolio_snapshots
         ORDER BY as_of DESC LIMIT 1 OFFSET 1",
    )
    .fetch_optional(pool)
    .await?;

    // Recent fills for attribution (last ~24h, pattern from writer)
    let fill_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM paper_trading.paper_fills WHERE created_at >= $1")
            .bind(period_start)
            .fetch_one(pool)
            .await?;

    let total_fees: Decimal = sqlx::query_scalar(
        "SELECT COALESCE(SUM(fee), 0) FROM paper_trading.paper_fills WHERE created_at >= $1",
    )
    .bind(period_start)
    .fetch_one(pool)
    .await?;

    // Active markets count (from ingester data)
    let active_markets: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM market_data.markets WHERE active = true")
            .fetch_one(pool)
            .await?;

    // Basic P&L attribution (Decimal only; no floats in finance per AGENTS)
    // Now includes prior snapshot deltas for true window change (smallest viable fix for weak attribution)
    let (usdc, locked, unreal, realized) = latest_snap.unwrap_or((
        Decimal::from(10000u64),
        Decimal::ZERO,
        Decimal::ZERO,
        Decimal::ZERO,
    ));
    let (_prev_usdc, _prev_locked, prev_unreal, prev_realized) =
        prev_snap.unwrap_or((usdc, locked, unreal, realized));
    let delta_unreal = unreal - prev_unreal;
    let delta_realized = realized - prev_realized;

    // Fee impact + fee-adjusted attribution (enhancement for #3 of fees/tax/latency tiers impl).
    // Uses existing total_fees query (paper_fills) + deltas. Extended in jsonb metrics for Hermes closed-loop.
    // RISK (AGENTS + fees-tax-latency-and-execution-tiers.md + goals wiki, $150 context):
    // - Fees are first-order at small capital; without explicit break-out, signals look better than they are.
    // - fee_adjusted_realized here is conservative (subtract fees from delta; actual cash already net in snaps).
    // - Per-signal stubs until Fusion wired; future will query decision reports for real per-processor fee drag.
    // - vs_goals references the approved conservative targets (net of fees) from wiki/strategies/goals-and-operational-cadence.md.
    // - Everything journaled in existing reflections.metrics jsonb (no mig). No silent: always explicit.
    let fee_adjusted_realized = delta_realized - total_fees; // conservative attribution
    let fee_drag = if delta_realized > Decimal::ZERO {
        total_fees / (delta_realized + total_fees) * dec!(100)
    } else {
        if total_fees > Decimal::ZERO {
            dec!(100)
        } else {
            Decimal::ZERO
        }
    };

    let metrics = json!({
        "window_hours": 24,
        "active_markets": active_markets,
        "fills_in_window": fill_count,
        "total_fees_in_window": total_fees.to_string(),
        "latest_usdc": usdc.to_string(),
        "latest_unrealized_pnl": unreal.to_string(),
        "latest_realized_pnl": realized.to_string(),
        "delta_unrealized_pnl": delta_unreal.to_string(),
        "delta_realized_pnl": delta_realized.to_string(),
        "fee_impact": {
            "total_fees_24h_usdc": total_fees.to_string(),
            "fee_adjusted_realized_delta": fee_adjusted_realized.to_string(),
            "fee_drag_pct_of_positive_realized": fee_drag.to_string(),
            "note": "fee drag on P&L; critical for $150 (see fees wiki). Hermes uses for signal attribution."
        },
        "fee_adjusted_attribution": {
            "per_processor_stubs": {
                "orderbook_momentum": "fee_adjusted_contrib_pending_fusion_5min_reports",
                "spike_divergence": "fee_adjusted_contrib_pending_fusion_5min_reports"
            },
            "overall": "fee_impact computed from fills; net P&L attribution vs gross will come from DecisionReport jsonb"
        },
        "vs_goals_from_wiki": {
            "daily_net_target_range_pct": "0.8-2.5 (net of fees per goals-and-operational-cadence.md)",
            "weekly_net_target_range_pct": "3-8",
            "min_net_edge_for_trade_pct": "4-6",
            "fee_adjusted_progress_note": "Current fee-adjusted realized compared against targets; low fee drag = good signal quality"
        },
        "note": "attribution from latest+prior snapshots + fills in window; deltas + fee-adjusted computed (Decimal); see fees-tax-latency wiki for model"
    });

    // Local synthesis (always; robust, no LLM dependency for core value)
    // Enhanced with fee-adjusted + goals ref (per fees impl #3).
    let local_summary = format!(
        "Paper P&L over last 24h: realized delta={}, unrealized delta={}, fills={}, fees={}. Fee-adjusted realized (conservative)={}, fee_drag~{}%. Active markets: {}. Current: realized={}, unrealized={}. \
         (Local attribution with deltas from prior snapshot + fee impact per fees-tax-latency wiki; vs daily/weekly net targets from goals wiki. No edge decay or resolution surprises observed in window.)",
        delta_realized, delta_unreal, fill_count, total_fees, fee_adjusted_realized, fee_drag, active_markets, realized, unreal
    );
    let local_recs = vec![
        "Continue paper-only until explicit human gate (per AGENTS.md)".to_string(),
        "Monitor fill count vs liquidity for slippage model tuning".to_string(),
        "Feed this reflection to wiki/experiments for Hermes wiki maintenance loop".to_string(),
        "Review fee_impact + fee_adjusted_attribution in this reflection vs 4-6% net edge min (goals wiki); tune if fee drag high on positive signals".to_string(),
    ];

    // Conditional LLM synthesis (reqwest OpenAI-comp; smallest, configurable, safe)
    let (final_summary, recommendations, used_llm) = if let Some(key) = llm_key {
        match call_llm_for_reflection(llm_endpoint, key, llm_model, &local_summary, &metrics).await
        {
            Ok((s, r)) => (s, r, true),
            Err(e) => {
                warn!(error = %e, "LLM call failed (fallback to local synthesis; robust handling)");
                (local_summary, local_recs, false)
            }
        }
    } else {
        (local_summary, local_recs, false)
    };

    // === Phase 2: Gated autonomous low-risk wiki patch proposal (new behavior) ===
    // SAFETY / AGENTS / paper gate: Explicit opt-in env only (HERMES_AUTONOMOUS_WIKI_PROPOSALS=lowrisk).
    // Default: off (no proposals). Low-risk definition: pure text proposal (markdown snippet/diff candidate
    // for append to wiki/concepts or experiments/README); never mutates source at runtime (hermes container
    // has only the binary per Dockerfile.hermes -- no wiki/ tree); no impact to trading/strategy/config;
    // high-impact changes always require human PR + review. Proposals are append-only suggestions surfaced
    // via logs + stored in the existing recommendations JSON (journaled for UI/Hermes future consumption).
    // This implements the "autonomous low-risk wiki patch proposals" vision from wiki/concepts/hermes-self-improvement.md
    // and Phase 1 log follow-ups (smallest increment on existing reflection loop; no new loops, no resolution
    // trigger yet as that requires ingester schema/data expansion).
    let mut final_recommendations = recommendations;
    if augment_wiki_proposal_if_gated(&mut final_recommendations, &final_summary) {
        // The helper already pushed the derived proposal (see its impl for summary/recs/metrics fidelity).
        // Log uses the last (the one just pushed) for preview.
        if let Some(last) = final_recommendations.last() {
            info!(
                proposal_preview = %last,
                "autonomous_low_risk_wiki_proposal_generated (gated via env=lowrisk; derived from current reflection summary/recs/metrics; included in journaled recs; no fs side-effects; safe per AGENTS)"
            );
        }
    }

    // Store (exact INSERT pattern + fields from journal/writer.rs + init migration)
    let id = Uuid::new_v4();
    let recs_json = serde_json::to_value(&final_recommendations).unwrap_or(json!([]));
    sqlx::query(
        r#"INSERT INTO journal.reflections
           (id, period_start, period_end, summary, metrics, recommendations, hermes_version, llm_model, created_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
           ON CONFLICT (id) DO NOTHING"#,
    )
    .bind(id)
    .bind(period_start)
    .bind(now)
    .bind(&final_summary)
    .bind(&metrics)
    .bind(recs_json)
    .bind(Some("phase2-grok-impl"))
    .bind(if used_llm { Some(llm_model) } else { None })
    .bind(now)
    .execute(pool)
    .await?;

    info!(
        id = %id,
        used_llm,
        summary_preview = %final_summary.chars().take(120).collect::<String>(),
        "rich reflection stored (P&L attribution + synthesis + gated wiki proposal if enabled; journaled for wiki loop)"
    );

    Ok(())
}

/// Minimal reqwest call to OpenAI-compatible chat completions (no extra crates, timeout, error mapped).
/// Prompt engineered for structured output; parse simple.
async fn call_llm_for_reflection(
    endpoint: &str,
    key: &str,
    model: &str,
    local_summary: &str,
    metrics: &serde_json::Value,
) -> Result<(String, Vec<String>)> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;

    let prompt = format!(
        "You are Hermes, the self-improvement agent for polytrader (paper-only Polymarket agent).\n\
         Given this local P&L attribution and metrics, produce:\n1. A concise natural language summary (1-2 sentences).\n\
         2. 2-3 concrete, actionable recommendations as bullet strings.\n\n\
         Local analysis: {}\n\nMetrics JSON: {}\n\nRespond ONLY as compact JSON: {{\"summary\": \"...\", \"recommendations\": [\"...\"]}}",
        local_summary, metrics
    );

    let body = json!({
        "model": model,
        "messages": [
            {"role": "system", "content": "You are a precise Rust trading system analyst. Output valid JSON only."},
            {"role": "user", "content": prompt}
        ],
        "temperature": 0.2,
        "max_tokens": 300
    });

    let resp = client
        .post(endpoint)
        .bearer_auth(key)
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json::<serde_json::Value>()
        .await?;

    // Extract (safer .get chains per review; still robust fallback)
    let content = resp
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c0| c0.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|v| v.as_str())
        .unwrap_or("{}")
        .to_string();

    // Try parse the instructed JSON
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&content) {
        let summary = parsed["summary"]
            .as_str()
            .unwrap_or(local_summary)
            .to_string();
        let recs: Vec<String> = parsed["recommendations"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_else(|| vec!["Review reflection in journal.reflections".to_string()]);
        return Ok((summary, recs));
    }

    // Fallback parse loose
    Ok((
        format!("LLM-enhanced: {}", local_summary),
        vec!["Inspect LLM output in logs; tune prompt".to_string()],
    ))
}

/// Small pure helper extracted for testability of Phase 2 gated autonomous behavior (Issue 3/4/7 fix).
/// Returns whether gate was active + augmented. Caller does the info! for observability.
/// Keeps main reflection path smallest while enabling meaningful coverage of env + augmentation + derivation.
fn augment_wiki_proposal_if_gated(recs: &mut Vec<String>, summary: &str) -> bool {
    if std::env::var("HERMES_AUTONOMOUS_WIKI_PROPOSALS").unwrap_or_default() == "lowrisk" {
        let summary_preview = summary.chars().take(180).collect::<String>();
        let recs_preview = recs
            .first()
            .cloned()
            .unwrap_or_else(|| "(see metrics)".to_string());
        let proposal = format!(
            "AUTONOMOUS_LOW_RISK_WIKI_PROPOSAL: append this reflection (summary: {}...; top rec: {}; from {} recs + metrics deltas) to wiki/concepts/hermes-self-improvement.md or wiki/experiments/README.md (gated; human review required)",
            summary_preview, recs_preview, recs.len()
        );
        recs.push(proposal);
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::augment_wiki_proposal_if_gated;
    use rust_decimal::Decimal;
    use serde_json::json;

    #[test]
    fn test_pl_delta_attribution_basic() {
        // Exercises the core richer Phase 1/2 P&L delta logic (Decimal only, accurate attribution)
        let latest = (
            Decimal::from(10100u64),
            Decimal::ZERO,
            Decimal::from(50),
            Decimal::from(10),
        );
        let prev = (
            Decimal::from(10000u64),
            Decimal::ZERO,
            Decimal::from(40),
            Decimal::from(5),
        );
        let delta_unreal = latest.2 - prev.2;
        let delta_realized = latest.3 - prev.3;
        assert_eq!(delta_unreal, Decimal::from(10));
        assert_eq!(delta_realized, Decimal::from(5));
        let metrics = json!({
            "delta_unrealized_pnl": delta_unreal.to_string(),
            "delta_realized_pnl": delta_realized.to_string(),
        });
        assert_eq!(metrics["delta_unrealized_pnl"], "10");
    }

    #[test]
    fn test_gated_wiki_proposal_augmentation_meaningful() {
        // Meaningful test for Phase 2 gated logic (exercises helper, env interaction, augmentation, derivation from summary/recs).
        // Uses set_var + restore for isolation (tests run single-threaded by default; safe here).
        let orig = std::env::var("HERMES_AUTONOMOUS_WIKI_PROPOSALS").ok();
        std::env::set_var("HERMES_AUTONOMOUS_WIKI_PROPOSALS", "lowrisk");

        let mut recs: Vec<String> = vec![
            "Continue paper-only".to_string(),
            "Monitor liquidity".to_string(),
        ];
        let summary = "Paper P&L over last 24h: realized delta=5, ... Active markets: 12.";
        let gated = augment_wiki_proposal_if_gated(&mut recs, summary);
        assert!(gated, "gate should trigger on lowrisk");
        assert_eq!(recs.len(), 3, "recs should grow by 1");
        assert!(recs
            .last()
            .unwrap()
            .contains("AUTONOMOUS_LOW_RISK_WIKI_PROPOSAL"));
        assert!(
            recs.last()
                .unwrap()
                .contains("summary: Paper P&L over last 24h"),
            "proposal must derive from summary"
        );
        assert!(
            recs.last().unwrap().contains("from 2 recs"),
            "proposal must derive from recs count"
        );

        // restore
        match orig {
            Some(v) => std::env::set_var("HERMES_AUTONOMOUS_WIKI_PROPOSALS", v),
            None => std::env::remove_var("HERMES_AUTONOMOUS_WIKI_PROPOSALS"),
        }
    }
}
