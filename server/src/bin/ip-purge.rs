//! Nightly retention sweep for the IP-tracking tables.
//!
//! `docs/ip-tracking.md` promises that `key_ip_seen` hashes live 7 days and
//! `key_ip_daily` counts live 90. That promise is only true if something
//! deletes them, and nothing on the request path should: the proxy records a
//! source on every served request, and adding a second write plus a scan to
//! that path to do work that has to happen once a day would be the wrong trade
//! on a throughput-sensitive proxy.
//!
//! So it is a binary, run by whatever schedules the backup and reconciliation
//! jobs (`docs/backup-and-restore.md`, `tools/reconcile`). Idempotent: running
//! it twice in a day removes nothing the second time.
//!
//!   DATABASE_URL=... cargo run --bin ip-purge
//!
//! **The salt is not touched by this.** Deleting rows enforces the retention
//! window; the daily salt rotation is what makes the surviving hashes
//! unlinkable. Both are needed, and neither substitutes for the other.

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
            // Non-zero, so a scheduler notices a sweep that did not run.
            // Silence here is how a privacy promise quietly stops being kept.
            return Err(format!("could not connect to Sqlite: {err}").into());
        }
    };

    let purged = ip_tracking::purge_expired(&pool, ip_tracking::today_utc()).await?;

    info!(
        seen_deleted = purged.seen,
        daily_deleted = purged.daily,
        link_attempts_deleted = purged.link_attempts,
        "IP-tracking retention sweep complete"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    // `from_str` is the `FromStr` trait method, so the trait belongs in scope
    // where it is USED. At crate scope it compiled the binary with an unused
    // import, which `cargo clippy -- -D warnings` - the CI gate - rejects.
    use sqlx::sqlite::SqlitePoolOptions;
    use std::str::FromStr;
    use uuid::Uuid;

    /// A migrated on-disk SQLite URL in the system temp directory.
    async fn migrated_temp_db() -> (String, std::path::PathBuf) {
        let path =
            std::env::temp_dir().join(format!("apikita-ip-purge-test-{}.db", Uuid::new_v4()));
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
