//! Test-only support: a migrated SQLite database in a temporary directory.
//!
//! ## Why this module exists (plan section 5.5)
//!
//! Every money test in `db.rs` used to carry
//! `#[ignore = "requires live Postgres"]`, and the three modules that touch the
//! database each read `DATABASE_URL` and assumed somebody had already migrated
//! whatever it pointed at. That made the most important tests in the codebase the
//! ones that never ran: `concurrent_requests_cannot_overdraw_a_one_request_balance`
//! is the only real-concurrency proof of the overdraw fix, and CI never executed it.
//!
//! SQLite removes the excuse. The database is a file, so a test can build its own:
//! no server to start, no external setup, no shared state between tests — and
//! therefore no teardown that has to delete rows by name in the right foreign-key
//! order. Two tests that used to race each other through one database now cannot
//! see each other at all.
//!
//! ## What it deliberately does not do
//!
//! It does not open the pool itself. `TestDb::new` creates and migrates the file,
//! then hands it to [`crate::db::init_pool`] — the constructor production uses — so
//! the pragmas under test (`journal_mode`, `foreign_keys`, `busy_timeout`,
//! `synchronous`) are the production ones rather than a test-only approximation.
//! A test that built its own pool could pass while the server failed.
//!
//! It does not seed `wallets.balance_idr`. Money enters a wallet only through
//! [`crate::db::credit_topup_transaction`], which writes the matching `+` ledger row
//! in the same transaction. A fixture that writes the balance directly manufactures
//! the very drift the money tests assert against — a fixture that cannot pass while
//! the code under test is correct.

use std::path::Path;
use std::str::FromStr;

use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;
use tempfile::TempDir;
use uuid::Uuid;

use crate::db::{credit_topup_transaction, init_pool, TopupCreditResult};

/// Resolved at compile time from `CARGO_MANIFEST_DIR`, exactly as `bin/migrate.rs`
/// does. The tests therefore apply the real migration rather than a transcription
/// of it, and a schema change that is not expressible in the migration shows up as
/// a test failure instead of as a fixture that happens to agree with itself.
static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// The connection string `init_pool` needs to reach a file at `path`.
///
/// `SqliteConnectOptions::from_str` strips the `sqlite://` prefix and takes
/// everything up to the first `?` as the filename (measured: `sqlx-sqlite` 0.8.6,
/// `src/options/parse.rs:168`), so a Windows path keeps its backslashes and needs no
/// escaping. Asserted by a test below rather than assumed, because the alternative
/// failure mode is a test that silently writes to a *different* database.
fn sqlite_url(path: &Path) -> String {
    format!("sqlite://{}", path.display())
}

/// A migrated SQLite database that lives and dies with the test.
pub struct TestDb {
    /// The pool the tests use. Same constructor, and therefore the same pragmas,
    /// as the server.
    pub pool: SqlitePool,
    /// Held only so the directory is removed when the test ends. Never read.
    _dir: TempDir,
}

impl TestDb {
    /// A fresh, migrated database in its own temporary directory.
    ///
    /// Three steps, and the order matters. The file is created and migrated by a
    /// single connection, exactly as `bin/migrate.rs` does, because
    /// [`init_pool`] deliberately does not set `create_if_missing` and because WAL
    /// has to be entered while the database has no other readers — sqlx's own
    /// source notes that changing into WAL needs an exclusive lock
    /// `sqlite3_busy_timeout()` cannot wait on.
    pub async fn new() -> Self {
        let dir = tempfile::tempdir().expect("create a temp directory for the test database");
        let path = dir.path().join("test.db");
        let url = sqlite_url(&path);

        let setup_options = SqliteConnectOptions::from_str(&url)
            .expect("parse the test database url")
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true);

        let setup = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(setup_options)
            .await
            .expect("create the test database file");
        MIGRATOR
            .run(&setup)
            .await
            .expect("apply the real migration to the test database");
        setup.close().await;

        let pool = init_pool(&url).await.expect("open the test pool");
        TestDb { pool, _dir: dir }
    }

    /// The directory the database file lives in. Test-only, for asserting that it
    /// really is cleaned up.
    pub fn dir_path(&self) -> &Path {
        self._dir.path()
    }

    /// Closes the pool and removes the directory.
    ///
    /// `pool.close()` is awaited rather than merely dropped because it waits for
    /// every connection to close, which is what releases the file handles. Dropping
    /// the pool only *signals* the close and returns immediately, so the removal
    /// would race it.
    ///
    /// MEASURED, because the assumption is wrong in the convenient direction: while
    /// a pooled SQLite connection is open, `remove_dir_all` on that directory is
    /// REFUSED on Windows (SQLite's win32 VFS does not ask for `FILE_SHARE_DELETE`
    /// for the main database or its `-wal`/`-shm` sidecars). A test that panics
    /// therefore never reaches `close` and leaves one small directory behind in the
    /// system temp. That is the accepted cost of per-test isolation: bounded, and
    /// far cheaper than the single shared database these tests used to race
    /// through, which was the alternative.
    pub async fn close(self) {
        let TestDb { pool, _dir } = self;
        pool.close().await;
        drop(pool);
        drop(_dir);
    }
}

// ---------------------------------------------------------------------------
// Fixtures. Free functions rather than methods, so an assertion helper that was
// handed only a pool can still build its own rows without duplicating INSERT SQL.
// ---------------------------------------------------------------------------

/// An account row, shaped the way the login path creates one.
///
/// `id`, `created_at` and `updated_at` are all bound from Rust because the SQLite
/// schema has no `DEFAULT` for them (plan section 4.1, correction 1). This is the
/// defect the compiler cannot see: removing a column default breaks INSERTs at
/// runtime only, so the old fixture
/// `INSERT INTO accounts (pb_user_id) VALUES (?)` still type-checked and failed at
/// `NOT NULL constraint failed: accounts.id`.
pub async fn account(pool: &SqlitePool) -> Uuid {
    let id = Uuid::new_v4();
    let now = chrono::Utc::now();
    sqlx::query(
        "INSERT INTO accounts (id, pb_user_id, created_at, updated_at) VALUES (?, ?, ?, ?)",
    )
    .bind(id.hyphenated())
    .bind(format!("pb_test_{}", id.simple()))
    .bind(now)
    .bind(now)
    .execute(pool)
    .await
    .expect("create the test account");
    id
}

/// The zero-balance wallet the login path creates. Consistent on its own
/// (`0 = SUM` of no ledger rows), so a test may start here and still reconcile.
pub async fn wallet(pool: &SqlitePool, account_id: Uuid) {
    sqlx::query("INSERT INTO wallets (account_id, balance_idr, updated_at) VALUES (?, 0, ?)")
        .bind(account_id.hyphenated())
        .bind(chrono::Utc::now())
        .execute(pool)
        .await
        .expect("create the zero-balance wallet");
}

/// An account plus its wallet, which is what most fixtures actually want.
pub async fn account_with_wallet(pool: &SqlitePool) -> Uuid {
    let account_id = account(pool).await;
    wallet(pool, account_id).await;
    account_id
}

/// A live API key. `usage_daily.api_key_id` is part of the unique scope, so a real
/// key row is needed before any usage can be recorded against one.
pub async fn api_key(pool: &SqlitePool, account_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO api_keys (id, account_id, key_hash, prefix, created_at)
         VALUES (?, ?, ?, 'apk_test', ?)",
    )
    .bind(id.hyphenated())
    .bind(account_id.hyphenated())
    .bind(format!("h_{}", id.simple()))
    .bind(chrono::Utc::now())
    .execute(pool)
    .await
    .expect("create the test api key");
    id
}

/// A `pending` topup, returning the `order_id` a webhook would carry.
pub async fn pending_topup(pool: &SqlitePool, account_id: Uuid, amount_idr: i64) -> String {
    let order_id = format!("order_test_{}", Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO topups (id, account_id, amount_idr, order_id, status, created_at)
         VALUES (?, ?, ?, ?, 'pending', ?)",
    )
    .bind(Uuid::new_v4().hyphenated())
    .bind(account_id.hyphenated())
    .bind(amount_idr)
    .bind(&order_id)
    .bind(chrono::Utc::now())
    .execute(pool)
    .await
    .expect("create the pending topup");
    order_id
}

/// Puts money into the wallet through the real money-in path, so the ledger stays
/// consistent. Never write `balance_idr` directly: see the module comment.
pub async fn fund(pool: &SqlitePool, account_id: Uuid, amount_idr: i64) {
    let order_id = pending_topup(pool, account_id, amount_idr).await;
    match credit_topup_transaction(pool, &order_id, amount_idr).await {
        Ok(TopupCreditResult::Settled { .. }) => {}
        other => panic!("the fixture must fund through the real top-up path: {other:?}"),
    }
}

/// The wallet balance, which must always equal `ledger_sum`.
pub async fn balance(pool: &SqlitePool, account_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = ?")
        .bind(account_id.hyphenated())
        .fetch_one(pool)
        .await
        .expect("read the wallet balance")
}

/// The sum of the account's ledger rows — the other side of the invariant.
pub async fn ledger_sum(pool: &SqlitePool, account_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT COALESCE(SUM(delta_idr), 0) FROM ledger WHERE account_id = ?")
        .bind(account_id.hyphenated())
        .fetch_one(pool)
        .await
        .expect("sum the ledger")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The URL the helper builds must name the file it was given.
    ///
    /// This is not ceremony. If `from_str` mis-parsed the path, every test would
    /// still pass — against a database in the wrong place, or against one another.
    /// Measured here so the assumption is falsifiable.
    #[test]
    fn the_test_database_url_round_trips_to_the_file_it_names() {
        let path = std::env::temp_dir()
            .join("apikita-url-round-trip")
            .join("test.db");
        let options = SqliteConnectOptions::from_str(&sqlite_url(&path)).expect("parse the url");
        assert_eq!(
            options.get_filename(),
            path.as_path(),
            "the url must resolve to the file it was built from"
        );
    }

    /// The migration actually lands: the tables the fixtures write to exist, and
    /// the pragmas production depends on are the ones in force.
    ///
    /// Asserted rather than assumed because `TestDb::new` is the only place the
    /// suite proves the schema is applied at all — if the migrator silently ran
    /// nothing, every fixture would fail with a confusing "no such table".
    #[tokio::test]
    async fn a_new_test_database_is_migrated_and_has_the_production_pragmas() {
        let db = TestDb::new().await;

        let tables: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'usage_daily'",
        )
        .fetch_one(&db.pool)
        .await
        .expect("read the schema");
        assert_eq!(tables, 1, "the migration must have created usage_daily");

        let journal: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&db.pool)
            .await
            .expect("read journal_mode");
        assert_eq!(journal, "wal", "the test pool must run in WAL");

        let fk: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&db.pool)
            .await
            .expect("read foreign_keys");
        assert_eq!(fk, 1, "foreign keys must be on, or every RESTRICT is inert");

        let busy: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
            .fetch_one(&db.pool)
            .await
            .expect("read busy_timeout");
        assert_eq!(
            busy, 5_000,
            "busy_timeout is what replaces FOR UPDATE waiting"
        );

        db.close().await;
    }

    /// The schema refuses the timestamp format SQLite's own functions emit.
    ///
    /// `datetime('now')` produces `2026-09-25 05:09:19` — space-separated and
    /// without an offset. Mixed with the RFC3339 values Rust binds, that form
    /// compares unpredictably: `'T'` sorts after a space, so an expiry that had
    /// already passed read as still in the future. Every timestamp column
    /// therefore carries a GLOB CHECK, and this asserts the CHECK actually fires.
    ///
    /// It is what makes "bind every timestamp from Rust" a rule the database
    /// enforces rather than one a reviewer has to remember.
    #[tokio::test]
    async fn the_schema_refuses_a_timestamp_sqlite_would_have_written() {
        let db = TestDb::new().await;

        let space_format = sqlx::query(
            "INSERT INTO accounts (id, pb_user_id, created_at, updated_at) VALUES (?, ?, ?, ?)",
        )
        .bind(Uuid::new_v4().hyphenated())
        .bind("pb_space_format")
        .bind("2026-09-25 07:00:00")
        .bind("2026-09-25 07:00:00")
        .execute(&db.pool)
        .await;

        assert!(
            space_format.is_err(),
            "the space-separated timestamp must be refused by the GLOB CHECK"
        );

        db.close().await;
    }

    /// The database really is temporary: closing it removes its directory.
    ///
    /// Asserted rather than assumed because the tempting assumption is false. While
    /// a pooled SQLite connection is open, `remove_dir_all` on that directory is
    /// REFUSED on Windows — measured, and the reason `TestDb::close` awaits
    /// `pool.close()` instead of trusting drop order. Without this test, a change
    /// that replaced `close` with a plain drop would leak a temp directory per test
    /// run and nobody would notice until a CI runner filled its disk.
    #[tokio::test]
    async fn the_temp_database_is_removed_when_it_is_closed() {
        let db = TestDb::new().await;
        let dir_path = db.dir_path().to_path_buf();

        assert!(
            dir_path.join("test.db").exists(),
            "the database file must exist while the database is open"
        );

        db.close().await;

        assert!(
            !dir_path.exists(),
            "closing the test database must remove its directory: {}",
            dir_path.display()
        );
    }
}
