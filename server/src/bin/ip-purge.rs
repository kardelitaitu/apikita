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
    let pool = match db::init_pool(&database_url).await {
        Ok(pool) => pool,
        Err(err) => {
            error!("Could not connect to Sqlite: {err}");
            // Non-zero, so a scheduler notices a sweep that did not run.
            // Silence here is how a privacy promise quietly stops being kept.
            std::process::exit(1);
        }
    };

    let purged = ip_tracking::purge_expired(&pool, ip_tracking::today_utc()).await?;

    info!(
        seen_deleted = purged.seen,
        daily_deleted = purged.daily,
        "IP-tracking retention sweep complete"
    );

    Ok(())
}
