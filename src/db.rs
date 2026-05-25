//! Postgres connection pool, migrations, and repository traits.
//! Uses sqlx with compile-time query checking.
//!
//! SAFETY: This is the paper-only DB path. No real_trading tables or writes exist.

use sqlx::postgres::{PgPool, PgPoolOptions};
use std::time::Duration;

/// Create pool with exponential backoff retry (important for CNPG startup ordering).
/// Runs all embedded sqlx migrations on first successful connection (idempotent).
pub async fn create_pool(database_url: &str) -> anyhow::Result<PgPool> {
    const MAX_RETRIES: u32 = 20;
    const INITIAL_BACKOFF_MS: u64 = 500;
    const MAX_BACKOFF_MS: u64 = 10_000;

    let mut backoff = INITIAL_BACKOFF_MS;
    let mut last_error = None;

    for attempt in 1..=MAX_RETRIES {
        match PgPoolOptions::new()
            .max_connections(10)
            .acquire_timeout(Duration::from_secs(8))
            .connect(database_url)
            .await
        {
            Ok(pool) => {
                tracing::info!(
                    "Database connection established on attempt {}/{}",
                    attempt,
                    MAX_RETRIES
                );

                tracing::info!("Running database migrations (paper-only schema)...");
                sqlx::migrate!("./migrations")
                    .run(&pool)
                    .await
                    .map_err(|e| anyhow::anyhow!("migration failed: {}", e))?;

                tracing::info!("Migrations applied successfully. DB ready for paper trading.");

                // Final health check
                ping(&pool).await?;
                tracing::info!("Database health check passed.");

                return Ok(pool);
            }
            Err(e) => {
                last_error = Some(e);
                if attempt < MAX_RETRIES {
                    tracing::warn!(
                        "Database connection attempt {}/{} failed. Retrying in {}ms...",
                        attempt,
                        MAX_RETRIES,
                        backoff
                    );
                    tokio::time::sleep(Duration::from_millis(backoff)).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF_MS);
                }
            }
        }
    }

    Err(anyhow::anyhow!(
        "Failed to connect to database after {} attempts. Last error: {:?}",
        MAX_RETRIES,
        last_error
    ))
}

/// Simple health/ping query.
pub async fn ping(pool: &PgPool) -> anyhow::Result<()> {
    sqlx::query("SELECT 1 as health").execute(pool).await?;
    Ok(())
}
