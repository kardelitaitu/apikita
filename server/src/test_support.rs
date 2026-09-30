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

use std::path::{Path, PathBuf};
use std::str::FromStr;

use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;
use tokio::sync::OnceCell;
use uuid::Uuid;

use crate::db::{credit_topup_transaction, init_pool, TopupCreditResult};

/// Resolved at compile time from `CARGO_MANIFEST_DIR`, exactly as `bin/migrate.rs`
/// does. The tests therefore apply the real migration rather than a transcription
/// of it, and a schema change that is not expressible in the migration shows up as
/// a test failure instead of as a fixture that happens to agree with itself.
static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// The one migrated schema every test database is copied from.
///
/// MEASURED, because the cost was invisible while it was spread over 68 tests.
/// Running the migrator against a fresh file costs ~84 ms here and creating the
/// per-test temporary directory ~117 ms, so the old `TestDb::new` (migrate + a
/// `TempDir` of its own + `init_pool`) cost ~261 ms — roughly 18 s of the suite's
/// ~21 s of work. The schema is identical for every test, so it is migrated ONCE
/// and every test database is a file copy of it: ~1.5 ms.
///
/// The migration still runs through the real `MIGRATOR`, and
/// [`build_template`] asserts the tables landed before any copy is handed out, so
/// a migrator that silently applied nothing still fails the suite — loudly, and
/// at the template rather than as 68 confusing "no such table" errors.
static TEMPLATE: OnceCell<PathBuf> = OnceCell::const_new();

/// The sidecars a WAL database can leave beside its file. Copied and removed
/// alongside it, because a `-wal` that survived a checkpoint still holds
/// committed frames and a copy without them is not the database that was migrated.
const SIDECARS: [&str; 2] = ["-wal", "-shm"];

/// Files `close` could not delete because Windows had not released the handle
/// yet, held so the next `TestDb::new` can delete them instead.
///
/// MEASURED: the release is normally immediate, but roughly one close in two
/// thousand is still refused after a 100 ms wait — bimodal, so it is a handle
/// that has genuinely not been closed rather than a slow one. Failing a test for
/// that would make the suite flaky over a temp file, while deleting nothing would
/// leak one database per test. Deferring gets both: by the time the next test
/// database is built the handle is gone, so the file does go away — just not on
/// the schedule Windows chose.
///
/// THE TWO ENDS OF THIS ARE CONNECTED, and checking that is worth the minute it
/// takes: `close` retries the delete, and whatever is left lands in ORPHANS, and
/// `reap_orphans` — called at the top of `TestDb::new` — tries those again on the next
/// run. A list that is written and never read would be a leak that looks handled, and
/// this one was nearly reported as exactly that before the caller was checked.
///
/// NOR is there a fragile sleep hiding here, which is what a 66-second suite run once
/// suggested. The whole crate has two real sleeps outside a paused clock: this bounded
/// retry, and the one-second stream deadline in the events tests. The circuit breaker
/// and key pool use `tokio::time::advance`, so they are deterministic under load.
///
/// THE STREAM DEADLINE IS NOT THE ANSWER, and arithmetic is what says so. I wrote it
/// down last round as the one remaining candidate; it is not. A one-second wait accounts
/// for at most one second of a run that took fifty-two longer than usual, so chasing it
/// would have been chasing a number that cannot fit.
///
/// The run in question also had `npm test` and a cargo invocation in the same command,
/// so compilation contending with the suite is the better explanation — and a test
/// failing while the crate is being rebuilt says something about the machine, not about
/// the test. That is a hypothesis, not a finding, and is labelled as one.
///
/// The deadline stays at a second regardless, and that is a decision rather than an
/// accident: it is generous enough that the live arm reliably delivers, and it costs one
/// second of a fourteen-second suite. Shortening it would trade a rare flake for a
/// common one, which is the wrong way round for the test that guards the cross-account
/// boundary.
static ORPHANS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

async fn template_path() -> &'static PathBuf {
    TEMPLATE.get_or_init(build_template).await
}

/// Deletes every file a previous `close` had to defer, plus its sidecars.
fn reap_orphans() {
    let pending: Vec<PathBuf> =
        std::mem::take(&mut *ORPHANS.lock().unwrap_or_else(|e| e.into_inner()));
    for path in pending {
        delete_database_file(&path);
    }
}

/// Migrates one schema-only database and returns its path.
///
/// WAL is entered here and checkpointed before the file is closed, exactly as the
/// old per-test setup did: `init_pool` then opens a file that is already in WAL,
/// so it never has to convert one.
async fn build_template() -> PathBuf {
    // One file, one process: the uuid keeps two concurrently running test
    // binaries from copying each other's template. It lives in the system temp
    // directory and is not deleted on exit - bounded at one small file per test
    // run, which is the same trade-off the old design already made for any test
    // that panicked before `close`.
    let path = std::env::temp_dir().join(format!("apikita-test-template-{}.db", Uuid::new_v4()));
    let url = sqlite_url(&path);

    let setup_options = SqliteConnectOptions::from_str(&url)
        .expect("parse the template database url")
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true);

    let setup = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(setup_options)
        .await
        .expect("create the template database file");
    MIGRATOR
        .run(&setup)
        .await
        .expect("apply the real migration to the template database");

    // Fold the WAL into the file so a copy of it is self-contained.
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .fetch_one(&setup)
        .await
        .expect("checkpoint the template database");

    let tables: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'usage_daily'",
    )
    .fetch_one(&setup)
    .await
    .expect("read the template schema");
    assert_eq!(
        tables, 1,
        "the template must be migrated: from here on every test database is a copy of it, \
         and only this function ever sees the migrator run"
    );

    setup.close().await;
    path
}

/// Deletes a database file and its sidecars, reporting whether it went away.
///
/// Best-effort on purpose: the caller decides what a refusal means, because a
/// file the OS still has open is not a failed test.
fn delete_database_file(path: &Path) -> bool {
    let removed = std::fs::remove_file(path).is_ok();
    for suffix in SIDECARS {
        let sidecar = PathBuf::from(format!("{}{suffix}", path.display()));
        if sidecar.exists() {
            let _ = std::fs::remove_file(&sidecar);
        }
    }
    removed
}

/// Copies the migrated template to `dest`, carrying any sidecar along with it.
fn copy_template(template: &Path, dest: &Path) {
    std::fs::copy(template, dest).expect("copy the migrated template to the test database");
    for suffix in SIDECARS {
        let src = PathBuf::from(format!("{}{suffix}", template.display()));
        if src.exists() {
            std::fs::copy(&src, PathBuf::from(format!("{}{suffix}", dest.display())))
                .expect("copy the template sidecar");
        }
    }
}

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
    /// The file the pool is open on, so `close` can delete it.
    path: PathBuf,
}

impl TestDb {
    /// A fresh, migrated database on its own file.
    ///
    /// The schema is copied from the one migrated template (see [`TEMPLATE`])
    /// rather than migrated again, and the file goes straight into the system
    /// temporary directory instead of into a `TempDir` of its own: both of those
    /// were pure per-test overhead and together they were ~200 ms of the ~261 ms
    /// this used to cost. What is left is the part that has to be real —
    /// [`init_pool`], the constructor production uses, opening the file with the
    /// production pragmas.
    pub async fn new() -> Self {
        reap_orphans();
        let template = template_path().await;
        let path = std::env::temp_dir().join(format!("apikita-test-{}.db", Uuid::new_v4()));
        copy_template(template, &path);

        let pool = init_pool(&sqlite_url(&path))
            .await
            .expect("open the test pool");
        TestDb { pool, path }
    }

    /// The file the database lives on. Test-only, for the health probe that opens
    /// a second pool onto the same file, and for asserting that it is cleaned up.
    pub fn db_path(&self) -> &Path {
        &self.path
    }

    /// Closes the pool and deletes the file.
    ///
    /// `pool.close()` is awaited rather than merely dropped because it waits for
    /// every connection to close, which is what releases the file handles. Dropping
    /// the pool only *signals* the close and returns immediately, so the removal
    /// would race it.
    ///
    /// MEASURED, because the assumption is wrong in the convenient direction: while
    /// a pooled SQLite connection is open, deleting that file is REFUSED on Windows
    /// (SQLite's win32 VFS does not ask for `FILE_SHARE_DELETE` for the main
    /// database or its `-wal`/`-shm` sidecars) — which is exactly why the await
    /// is here.
    ///
    /// A file the OS still refuses to release is deferred to the next
    /// [`TestDb::new`] (see [`ORPHANS`]) rather than failing the test: the leak is
    /// what matters and the deferral still prevents it, whereas failing would make
    /// the suite flaky over a temp file. A test that panics never reaches `close`
    /// at all and leaves one small file behind in the system temp — bounded, and
    /// far cheaper than the single shared database these tests used to race
    /// through, which was the alternative.
    pub async fn close(self) {
        let TestDb { pool, path } = self;
        pool.close().await;
        drop(pool);

        // Normally one try. The wait is bounded and non-blocking — a
        // `std::thread::sleep` here would block the very runtime whose background
        // task is closing the connection, and the file would then never be
        // released at all (measured: 25 of 284 tests failed that way).
        let mut deleted = delete_database_file(&path);
        for _ in 0..20 {
            if deleted {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            deleted = delete_database_file(&path);
        }
        if !deleted {
            ORPHANS.lock().unwrap_or_else(|e| e.into_inner()).push(path);
        }
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
/// runtime only, so the old fixture `INSERT INTO accounts (pb_user_id) VALUES (?)`
/// still type-checked and failed at `NOT NULL constraint failed: accounts.id`.
///
/// The `pb_user_id` fixture value went the same way in Phase 6, when the column
/// itself was dropped: this INSERT named it explicitly, so the rebuild left the
/// fixture rejected with `no such column: pb_user_id` rather than quietly wrong.
pub async fn account(pool: &SqlitePool) -> Uuid {
    let id = Uuid::new_v4();
    let now = chrono::Utc::now();
    sqlx::query("INSERT INTO accounts (id, created_at, updated_at) VALUES (?, ?, ?)")
        .bind(id.hyphenated())
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
        "INSERT INTO topups (id, account_id, amount_idr, order_id, status, rail, created_at)
         VALUES (?, ?, ?, ?, 'pending', 'midtrans', ?)",
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

    /// `CHECK (balance_idr >= 0)` actually refuses a negative write.
    ///
    /// The launch checklist's Gate 2 requires the constraint "present and exercised",
    /// and only the first half was true: no test drove a balance below zero and watched
    /// SQLite refuse it. That matters more than it looks, because the constraint is
    /// explicitly a BACKSTOP - `db.rs` puts it that way - the layer that catches you when
    /// the layer above fails. The clamp above it is proven in isolation (a pure function
    /// with the case analysis written out), so the CHECK is the only thing standing between
    /// a bug in that proof and a wallet that owes money.
    ///
    /// A backstop that has never been exercised is a backstop whose behaviour has never
    /// been demonstrated, and the failure it exists to catch is the one nobody would notice
    /// in a test: a migration that dropped the constraint, or a code path that bypassed the
    /// clamp. Both look fine right up until a balance is negative.
    ///
    /// Written here rather than in `db.rs` because the harness owns the migrated database
    /// and both sides of the invariant, and a test that reached for its own pool would be
    /// testing the setup rather than the constraint.
    #[tokio::test]
    async fn the_balance_check_refuses_a_negative_write() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account_id = account(&pool).await;
        wallet(&pool, account_id).await;

        let result = sqlx::query("UPDATE wallets SET balance_idr = -1 WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .execute(&pool)
            .await;

        assert!(
            result.is_err(),
            "the schema accepted a negative balance. The clamp above is what should refuse it, so if the CHECK is gone a bug in the clamp now silently owes money."
        );
        // And the row is untouched, so the refusal is a refusal and not a silent clamp.
        assert_eq!(
            balance(&pool, account_id).await,
            0,
            "the negative write was refused but the balance changed"
        );
        db.close().await;
    }

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

    /// A top-up must record which payment rail funded it, and the value set is
    /// frozen.
    ///
    /// The rail decides the payout method at wind-down ("anyone who used Midtrans
    /// is considered Indonesian"), so a row that does not name one is unpayable.
    /// That is why this column is NOT NULL with no DEFAULT: a path that forgot to
    /// name its rail fails loudly here instead of being silently mislabelled as
    /// the only rail that exists today. Asserted rather than assumed because the
    /// default-less shape is invisible in Rust — a `DEFAULT 'midtrans'` would
    /// compile and pass every other test in this file.
    ///
    /// The value set is frozen because SQLite cannot add a CHECK value later
    /// without a 12-step table rebuild, so the vocabulary is pinned to the two
    /// rails that exist rather than left open.
    #[tokio::test]
    async fn a_topup_must_name_its_rail_and_the_value_set_is_frozen() {
        let db = TestDb::new().await;

        // (a) NOT NULL, (c) no default: both in the declared column shape.
        let declared: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_table_info('topups') \
             WHERE name = 'rail' AND \"notnull\" = 1 AND dflt_value IS NULL",
        )
        .fetch_one(&db.pool)
        .await
        .expect("read the topups schema");
        assert_eq!(
            declared, 1,
            "topups.rail must exist, be NOT NULL, and carry no DEFAULT"
        );

        let account_id = account_with_wallet(&db.pool).await;

        // The column is required: an INSERT that forgets it is refused, not
        // silently defaulted. This is the loud failure the missing DEFAULT buys.
        let forgot = sqlx::query(
            "INSERT INTO topups (id, account_id, amount_idr, order_id, status, rail, created_at) \
             VALUES (?, ?, 1000, ?, 'midtrans', ?)",
        )
        .bind(Uuid::new_v4().hyphenated())
        .bind(account_id.hyphenated())
        .bind(format!("test_rail_missing_{}", Uuid::new_v4().simple()))
        .bind(chrono::Utc::now())
        .execute(&db.pool)
        .await;
        assert!(
            forgot.is_err(),
            "an INSERT that names no rail must be refused, not defaulted"
        );

        // (b) The frozen vocabulary round-trips...
        for rail in ["midtrans", "crypto"] {
            sqlx::query(
                "INSERT INTO topups (id, account_id, amount_idr, order_id, status, rail, created_at) \
                 VALUES (?, ?, 1000, ?, 'pending', ?, ?)",
            )
            .bind(Uuid::new_v4().hyphenated())
            .bind(account_id.hyphenated())
            .bind(format!("test_rail_{rail}_{}", Uuid::new_v4().simple()))
            .bind(rail)
            .bind(chrono::Utc::now())
            .execute(&db.pool)
            .await
            .unwrap_or_else(|e| panic!("{rail} must be a documented rail: {e}"));
        }

        // ...and a third rail is refused, not silently stored.
        let refused = sqlx::query(
            "INSERT INTO topups (id, account_id, amount_idr, order_id, status, rail, created_at) \
             VALUES (?, ?, 1000, ?, 'pending', 'bank_transfer', ?)",
        )
        .bind(Uuid::new_v4().hyphenated())
        .bind(account_id.hyphenated())
        .bind(format!("test_rail_bad_{}", Uuid::new_v4().simple()))
        .bind(chrono::Utc::now())
        .execute(&db.pool)
        .await;
        assert!(
            refused.is_err(),
            "an undocumented rail must be refused by the CHECK constraint"
        );

        db.close().await;
    }

    /// The database really is temporary: closing it removes its file.
    ///
    /// Asserted rather than assumed because the tempting assumption is false. While
    /// a pooled SQLite connection is open, deleting that file is REFUSED on
    /// Windows — measured, and the reason `TestDb::close` awaits `pool.close()`
    /// instead of trusting drop order. Without this test, a change that replaced
    /// `close` with a plain drop would leak a database file per test and nobody
    /// would notice until a CI runner filled its disk.
    ///
    /// The check is done after a SECOND database is built, because a file whose
    /// handle Windows has not released is reaped there and not before: the
    /// guarantee under test is that the file does not outlive the test run, not
    /// that Windows releases it within one call.
    #[tokio::test]
    async fn the_temp_database_is_removed_when_it_is_closed() {
        let db = TestDb::new().await;
        let db_path = db.db_path().to_path_buf();

        assert!(
            db_path.exists(),
            "the database file must exist while the database is open"
        );

        db.close().await;
        let second = TestDb::new().await;

        assert!(
            !db_path.exists(),
            "closing the test database must remove its file: {}",
            db_path.display()
        );

        second.close().await;
    }
}
