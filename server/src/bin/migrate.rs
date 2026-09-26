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

    run(&database_url).await
}

/// Applies the schema and asserts the preconditions the schema's constraints
/// depend on. Split from `main` so every branch can be pinned by a unit test
/// without spawning a real deploy process.
async fn run(database_url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let options = SqliteConnectOptions::from_str(database_url)?
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
            return Err(format!("could not open the database at {database_url}: {err}").into());
        }
    };

    if let Err(err) = MIGRATOR.run(&pool).await {
        error!("Migration failed: {err}");
        return Err(format!("migration failed: {err}").into());
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

    check_preconditions(&journal, fk)?;

    Ok(())
}

/// The three preconditions the schema's constraints depend on, as a pure check
/// so the failure branches can be pinned without a misconfigured database.
fn check_preconditions(journal: &str, fk: i64) -> Result<(), Box<dyn std::error::Error>> {
    if journal != "wal" {
        error!("journal_mode is {journal}, not wal — writes are not crash-safe as configured");
        return Err(format!("journal_mode is {journal}, not wal").into());
    }
    if fk != 1 {
        error!("foreign_keys is off — every ON DELETE RESTRICT in the schema is inert");
        return Err("foreign_keys is off".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;
    use uuid::Uuid;

    #[test]
    fn preconditions_pass_when_wal_and_foreign_keys_are_on() {
        check_preconditions("wal", 1).expect("wal + fk=1 is the healthy state");
    }

    #[test]
    fn preconditions_reject_a_non_wal_journal() {
        check_preconditions("memory", 1).expect_err("a non-wal journal must fail the sweep");
    }

    #[test]
    fn preconditions_reject_foreign_keys_off() {
        check_preconditions("wal", 0).expect_err("fk off must fail the sweep");
    }

    /// A unique on-disk SQLite URL in the system temp directory.
    ///
    /// sqlx on Windows wants a forward-slashed path under `sqlite://` (e.g.
    /// `sqlite://C:/Users/...`); a backslash form makes WAL sidecar creation
    /// fail with ERROR_INVALID_NAME. See `.workbuddy-ai/memory/2026-09-26.md`.
    fn temp_db_url() -> (String, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("apikita-migrate-test-{}.db", Uuid::new_v4()));
        (format!("sqlite://{}", path.to_str().unwrap().replace('\\', "/")), path)
    }

    #[tokio::test]
    async fn run_applies_the_schema_and_reports_success() {
        let (url, path) = temp_db_url();
        run(&url)
            .await
            .expect("migrate run must succeed against a fresh database");

        // The schema must actually exist now: reopen and confirm a table landed.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("reopen the migrated database");
        let table_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'",
        )
        .fetch_one(&pool)
        .await
        .expect("count tables");
        assert!(table_count > 0, "migrations must have created tables");

        pool.close().await;
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn run_rejects_an_unparseable_database_url() {
        // sqlx is lenient with a bare path (it treats "foo.db" as a relative
        // file and create_if_missing would just make it), so a genuinely
        // rejected URL is one with the wrong scheme: `SqliteConnectOptions`
        // enforces `sqlite://` and errors on anything else.
        run("postgres://localhost/db")
            .await
            .expect_err("a URL sqlx cannot interpret must surface as an error, not a silent exit");
    }
}
