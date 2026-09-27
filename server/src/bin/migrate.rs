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
    refuse_drive_relative_db_path(&filename.to_string_lossy())?;
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

    let version = applied_schema_version(&pool).await;
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

/// The schema version the log reports: the version sqlx actually applied,
/// from sqlx's own tracking table. `PRAGMA user_version` read 0 forever —
/// sqlx never sets it — so the "logged, not assumed" version line was a
/// constant zero (measured 2026-09-26 against a freshly migrated database).
async fn applied_schema_version(pool: &sqlx::SqlitePool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations")
        .fetch_one(pool)
        .await
        .unwrap_or(0)
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

/// Refuse a database path that SPELLS itself absolute but is drive-relative on
/// this host: a leading `/` (or `\`) with no drive prefix, as an MSYS/Git-Bash
/// URL like `sqlite:///c/dev/...` produces on Windows.
///
/// MEASURED (2026-09-26): such a URL made this binary treat `/c/dev/...` as
/// `C:\c\dev\...` — `create_dir_all` fabricated that tree at the ROOT OF C:,
/// the whole schema was built there, and the log printed the path the operator
/// wrote. The server (create_if_missing=false) fails loudly on the same URL,
/// so the divergence is invisible until a deploy starts a server against an
/// unmigrated database. The relative-path form (`sqlite://data/server.db`,
/// resolved against the cwd) stays accepted: that is the documented spelling.
fn refuse_drive_relative_db_path(filename: &str) -> Result<(), Box<dyn std::error::Error>> {
    let looks_absolute = filename.starts_with('/') || filename.starts_with('\\');
    if looks_absolute && !std::path::Path::new(filename).is_absolute() {
        error!(
            database_url_path = %filename,
            "this path starts with a separator but is not absolute on this host \
             (an MSYS-style /c/... URL is drive-relative on Windows); it would be \
             created at the drive root (C:\\c\\...). Pass a drive-letter absolute \
             path (sqlite://C:/dev/...) or the documented relative form \
             (sqlite://data/server.db, resolved against server/)"
        );
        return Err(format!(
            "database path '{filename}' is not absolute on this host; refusing to \
             create it at a drive-relative location"
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;
    use uuid::Uuid;

    #[test]
    fn an_msys_style_absolute_url_is_refused_instead_of_creating_the_schema_at_the_drive_root() {
        // MEASURED (2026-09-26): `DATABASE_URL=sqlite:///c/dev/... cargo run
        // --bin migrate` treated "/c/dev/..." as a DRIVE-RELATIVE Windows path,
        // fabricated C:\c\dev\... at the root of C:, built the whole 18-table
        // schema there, and logged the fictional path the operator wrote. The
        // recovery hint other tools print ("create it with 'DATABASE_URL=...
        // cargo run --bin migrate'") is spelled in exactly this MSYS form, so
        // the trap sits on the documented path.
        let refused = refuse_drive_relative_db_path("/c/dev/apikita/.agents/mig-probe.db");
        if cfg!(windows) {
            assert!(
                refused.is_err(),
                "a /c/... path is drive-relative on Windows and must be refused: {refused:?}"
            );
        } else {
            // On Unix the same string IS an absolute path and harmless here.
            refused.expect("/c/... is absolute on Unix");
        }
    }

    #[test]
    fn a_windows_absolute_and_a_relative_path_are_both_accepted() {
        refuse_drive_relative_db_path("C:/dev/apikita/server/data/server.db")
            .expect("a drive-letter absolute path is unambiguous");
        refuse_drive_relative_db_path("data/server.db")
            .expect("a relative path is resolved against the cwd by design");
        refuse_drive_relative_db_path("/dev/apikita/server.db")
            .expect_err("a root-relative path (leading separator, no drive) is the same trap");
    }

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
        (
            format!("sqlite://{}", path.to_str().unwrap().replace('\\', "/")),
            path,
        )
    }

    #[tokio::test]
    async fn the_reported_schema_version_is_the_sqlx_applied_version() {
        // MEASURED (2026-09-26): the log claimed schema_version=0 on a freshly
        // migrated database whose _sqlx_migrations row says 20260925000000 —
        // sqlx never sets PRAGMA user_version, so the "logged, not assumed"
        // version line reported a constant zero. The reported value must be
        // the version sqlx actually applied.
        let (url, path) = temp_db_url();
        run(&url)
            .await
            .expect("migrate run must succeed against a fresh database");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("reopen the migrated database");

        let applied: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations")
                .fetch_one(&pool)
                .await
                .expect("read the sqlx migrations table");

        // Derived from the embedded migration list, not hard-coded: a pinned
        // constant here rotted the moment a second migration was added (the
        // 20260926000000 redemption-attempts file). Whatever migrations exist,
        // the database must have applied the newest of them.
        let expected: i64 = MIGRATOR
            .migrations
            .iter()
            .map(|migration| migration.version)
            .max()
            .expect("the migration list is never empty");
        assert_eq!(applied, expected, "the fixture migration ran");

        assert_eq!(
            applied_schema_version(&pool).await,
            applied,
            "the logged schema_version must be the sqlx-applied version, not PRAGMA user_version"
        );

        pool.close().await;
        let _ = std::fs::remove_file(&path);
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
        let table_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'")
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
