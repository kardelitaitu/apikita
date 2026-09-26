//! Applies the SQLite schema. Runs **before** the server, never on its boot.
//!
//! The register is explicit: migrations run *"in CI, after a snapshot, before the
//! server"* and *"never on application boot"*. Deploy order is
//! `Migrate -> server -> health -> frontend`. This binary is that first stage.
//!
//!   DATABASE_URL=sqlite://data/server.db cargo run --bin migrate
//!
//! ## Why this binary creates the database, and the server does not
//!
//! `sqlx` opens SQLite with `create_if_missing: false` (measured:
//! `sqlx-sqlite` 0.8.6, `src/options/mod.rs:198`). That is the behaviour we want on
//! the server: a missing database is a loud failure that means "you have not
//! migrated", not an empty schema-less file that silently accepts nothing. Creating
//! it is this binary's job, so this is the one place that opts in.
//!
//! ## Why WAL is set here
//!
//! `sqlx` deliberately leaves `journal_mode` unset — its own source says so:
//! *"WAL mode is a permanent setting for created databases and changing into or out
//! of it requires an exclusive lock that can't be waited on with
//! `sqlite3_busy_timeout()`."* So it is set exactly once, at creation, by the
//! component that creates the file. Every later connection sees WAL already
//! persisted in the database header, where re-requesting it is a no-op.
//!
//! ## Foreign keys
//!
//! SQLite leaves `PRAGMA foreign_keys` **OFF** by default, which would make every
//! `ON DELETE RESTRICT` in the schema silently inert. `sqlx` turns it on for every
//! connection (same file, line 185), and it is asserted explicitly below so that a
//! future non-sqlx connection cannot quietly remove the backstop.

use std::env;
use std::path::PathBuf;
use std::str::FromStr;

use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// Resolved at compile time from `CARGO_MANIFEST_DIR`, so the binary does not
/// depend on the process working directory.
static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

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

    let options = SqliteConnectOptions::from_str(&database_url)?
        // This binary is the one component allowed to create the database.
        .create_if_missing(true)
        // Permanent, and therefore set at creation. See the module comment.
        .journal_mode(SqliteJournalMode::Wal)
        // Already sqlx's default; asserted so it cannot be lost by accident.
        .foreign_keys(true);

    // SQLite creates the database file but never its parent directory, and a
    // missing directory surfaces only as "unable to open database file".
    let filename: PathBuf = options.get_filename().to_path_buf();
    if let Some(parent) = filename.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)?;
            info!(dir = %parent.display(), "created the database directory");
        }
    }

    // One connection: migrations are a single-writer operation, and a pool would
    // only invite a second connection to race the first for the write lock.
    let pool = match SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
    {
        Ok(pool) => pool,
        Err(err) => {
            error!("Could not open the database at {database_url}: {err}");
            // Non-zero, so a deploy pipeline stops rather than starting a server
            // against an unmigrated database.
            std::process::exit(1);
        }
    };

    if let Err(err) = MIGRATOR.run(&pool).await {
        error!("Migration failed: {err}");
        std::process::exit(1);
    }

    let version = sqlx::query_scalar::<_, i64>("PRAGMA user_version")
        .fetch_one(&pool)
        .await
        .unwrap_or(-1);
    let journal: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&pool)
        .await
        .unwrap_or_else(|_| "unknown".to_string());
    let fk: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(&pool)
        .await
        .unwrap_or(-1);

    // Logged, not assumed: these three are the preconditions the schema's
    // constraints depend on, and a silent regression in any of them turns
    // ON DELETE RESTRICT into decoration.
    info!(
        database = %filename.display(),
        schema_version = version,
        journal_mode = %journal,
        foreign_keys = fk,
        "Migrations applied"
    );

    if journal != "wal" {
        error!("journal_mode is {journal}, not wal — writes are not crash-safe as configured");
        std::process::exit(1);
    }
    if fk != 1 {
        error!("foreign_keys is off — every ON DELETE RESTRICT in the schema is inert");
        std::process::exit(1);
    }

    Ok(())
}
