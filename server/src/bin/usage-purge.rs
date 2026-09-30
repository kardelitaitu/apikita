//! Nightly retention sweep for the age-based tables: `usage_events` (90 days),
//! `usage_daily` (24 months), expired/revoked `sessions` (30 days), expired
//! verification/password-reset links (as soon as they expire) and terminal
//! Telegram link codes (used or expired, plus one day).
//!
//! `docs/data-retention.md` states all five periods. They are only true if
//! something deletes the rows, and nothing on the request path should: the
//! settlement writes one row per billed request, and adding a per-request delete
//! to it to do work that has to happen once a day would be the wrong trade.
//!
//! So it is a binary, alongside `ip-purge`, run by whatever schedules the backup
//! and reconciliation jobs (`docs/backup-and-restore.md`, `tools/reconcile`).
//! Idempotent: running it twice in a day removes nothing the second time.
//!
//!   DATABASE_URL=... cargo run --bin usage-purge
//!
//! It deletes ONLY `usage_events`. `usage_daily` is the 24-month aggregate the
//! spend window and reconciliation read, and it is deliberately not touched here;
//! `ledger` and `topups` are financial records kept forever.
//!
//! THE LINK PURGE WAS ADDED BECAUSE IT HAD NO CALLER AT ALL. The identity port
//! shipped `identity::tokens::purge_expired` with a unit test and nothing that
//! invoked it, so expired links were written and never removed while the privacy
//! page said they were. It lives in this sweep rather than a third binary because
//! this file's own doc-comment already argues the case: two retention jobs are two
//! places the policy can be forgotten, and the gap proved it.
//!
//! THE LINK-CODE PURGE ARRIVED THE SAME WAY, one round later and from the other
//! direction: `db.rs` listed `link_codes` among the tables the sweep deliberately
//! did NOT touch, on the grounds that its rule was "a different shape". The link
//! purge above had already made that shape this sweep's business, so the exclusion
//! was left over from a design this file had outgrown — while holding a published
//! 24-hour window that nothing implemented. A stale refusal is harder to find than
//! a missing call, because it reads as a decision someone thought about.

use std::env;

use apikita_server::{db, ip_tracking};

use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,apikita_server=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let database_url = env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is not set")?;

    run(&database_url).await
}

/// Runs the retention sweep. Split from `main` so the connect-and-purge path can
/// be pinned by a unit test against a real (migrated) database without spawning a
/// scheduler process.
async fn run(database_url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let pool = match db::init_pool(database_url).await {
        Ok(pool) => pool,
        Err(err) => {
            error!("Could not connect to Sqlite: {err}");
            // Non-zero, so a scheduler notices a sweep that did not run. Silence
            // here is how a privacy promise quietly stops being kept.
            return Err(format!("could not connect to Sqlite: {err}").into());
        }
    };

    let today = ip_tracking::today_utc();
    let purged = db::purge_expired_usage(&pool, today).await?;

    info!(
        usage_events_deleted = purged.usage_events,
        usage_daily_deleted = purged.usage_daily,
        sessions_deleted = purged.sessions,
        identity_tokens_deleted = purged.identity_tokens,
        events_retention_days = db::USAGE_EVENTS_RETENTION_DAYS,
        daily_retention_days = db::USAGE_DAILY_RETENTION_DAYS,
        session_retention_days = db::SESSION_RETENTION_DAYS,
        "retention sweep complete"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::str::FromStr;
    use uuid::Uuid;

    /// A migrated on-disk SQLite URL in the system temp directory.
    async fn migrated_temp_db() -> (String, std::path::PathBuf) {
        let path =
            std::env::temp_dir().join(format!("apikita-usage-purge-test-{}.db", Uuid::new_v4()));
        let url = format!("sqlite://{}", path.to_str().unwrap().replace('\\', "/"));
        let options = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
            .unwrap()
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("open temp db");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("apply migrations");
        pool.close().await;
        (url, path)
    }

    #[tokio::test]
    async fn run_rejects_a_database_that_cannot_be_opened() {
        run("sqlite:///nonexistent-dir-xyz-abc-123/server.db")
            .await
            .expect_err("a database that cannot be opened must surface as an error");
    }

    #[tokio::test]
    async fn run_purges_against_a_migrated_database() {
        let (url, path) = migrated_temp_db().await;
        run(&url)
            .await
            .expect("the sweep must complete against a migrated database");
        let _ = std::fs::remove_file(&path);
    }
}
