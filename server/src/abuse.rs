//! Per-account abuse guards.
//!
//! `config/apikita.toml` declares the per-account caps under `[limits]`, and
//! `docs/decisions.md` settles their values ("Top-up creation 5/hour per
//! account", "Key creation 10/day per account"); `docs/server/api-spec.md` names
//! both as abuse targets in its cross-cutting rules ("Rate-limit: login, key
//! creation, link-code redemption, top-up creation").
//!
//! COUNTED FROM THE ROWS THEMSELVES, NOT AN IN-MEMORY COUNTER. The proxy's
//! per-key limiter (`routes/proxy.rs`) is a process-local fixed window, which is
//! the right shape for a per-minute request ceiling. These caps ask a different
//! question — "how many rows has this account created in the window?" — and
//! `topups` and `api_keys` already record the answer, with a timestamp and an
//! index on `(account_id, created_at)`. Reading that is exact, survives a
//! restart, and stays correct behind more than one instance.
//!
//! ONE GUARD, parameterized by the table it counts and the window it counts
//! over. Both capped tables are the same question asked twice, so they share one
//! implementation; a second copy of the boundary rule is how the two answers
//! start disagreeing. The CAP ITSELF is not read here: the limit belongs to the
//! config the application already owns (`AppState.config`), and is passed in, so
//! this module holds no state and no second view of the configuration.

use chrono::{DateTime, Duration, Utc};
use sqlx::{SqlitePool, Row};
use uuid::Uuid;

use crate::error::AppError;

/// The trailing window `limits.topup_per_hour` is stated over.
pub fn topup_window() -> Duration {
    Duration::hours(1)
}

/// The trailing window `limits.key_creation_per_day` is stated over.
///
/// ROLLING 24 hours, not a calendar day. `docs/decisions.md` chose a rolling
/// 30-day window for the spend limit precisely to avoid a "month-boundary
/// cliff", and the same reasoning applies to a daily cap. A calendar day would
/// additionally make the cap depend on which timezone the reader assumes
/// (WIB/WITA/WIT), which the register went out of its way to avoid.
pub fn key_creation_window() -> Duration {
    Duration::days(1)
}

/// Seconds until the window frees, for a `Retry-After`.
///
/// The window frees when its OLDEST row ages out, so the answer is
/// `oldest + window - now`, rounded UP to whole seconds (a client waiting half a
/// second must not be told `0`) and floored at 1 — a `Retry-After: 0` invites an
/// immediate retry that is refused again, which reads as a broken limiter
/// (docs/error-model.md).
fn retry_after_secs(oldest: DateTime<Utc>, window: Duration, now: DateTime<Utc>) -> u64 {
    let remaining_ms = (oldest + window)
        .signed_duration_since(now)
        .num_milliseconds();
    if remaining_ms <= 0 {
        return 1;
    }
    // Ceiling division by hand: `div_ceil` on a signed integer is not stable in
    // this toolchain's std yet. `remaining_ms` is positive here.
    ((remaining_ms + 999) / 1000) as u64
}

/// The whole cap decision, pure: `None` allows, `Some(retry_after_secs)` refuses.
///
/// This is the ONLY place the boundary lives. `>=` rather than `>`: `used` is
/// what is already on the books, so an account sitting exactly on its cap is out
/// of budget and the next call is the one that would exceed it. A limit of `0`
/// disables the cap, the convention every other configurable ceiling in this
/// project follows (`min_monthly_tokens`, `rate_limit_rpm`, `spend_limit_idr`),
/// so a value read from the config can turn the guard off without a code change.
fn cap_outcome(
    used: i64,
    limit: u32,
    oldest_in_window: Option<DateTime<Utc>>,
    window: Duration,
    now: DateTime<Utc>,
) -> Option<u64> {
    if limit == 0 || used < i64::from(limit) {
        return None;
    }

    // `oldest_in_window` is always present when the count reached a non-zero
    // limit, so the fallback only guards the arithmetic, never a real window.
    let retry_after_secs =
        oldest_in_window.map_or(1, |oldest| retry_after_secs(oldest, window, now));

    Some(retry_after_secs)
}

/// Refuses the request once `account_id` has `limit` rows in `table` created
/// within the trailing `window`.
///
/// EVERY created row counts, whatever its status. The act being capped is
/// creating the row — a Midtrans session, an API key — and a pending session
/// that is never paid still consumed one, while a revoked key is exactly what an
/// account would revoke to mint another. Counting only settled rows would let an
/// abuser evade the cap by never completing anything.
///
/// `table` must be a compile-time constant naming a table with `account_id` and
/// `created_at` columns; it is never request-derived, which is what makes the
/// interpolation below safe. `limit` comes from the caller's config.
pub async fn enforce_creation_cap(
    pool: &SqlitePool,
    table: &'static str,
    window: Duration,
    limit: u32,
    account_id: Uuid,
    now: DateTime<Utc>,
) -> Result<(), AppError> {
    let sql = format!(
        "SELECT COUNT(*) AS used, MIN(created_at) AS oldest \
         FROM {table} WHERE account_id = ? AND created_at >= ?"
    );

    let row = sqlx::query(&sql)
        .bind(account_id.hyphenated())
        .bind(now - window)
        .fetch_one(pool)
        .await?;

    let used: i64 = row.get("used");
    let oldest_in_window: Option<DateTime<Utc>> = row.get("oldest");

    match cap_outcome(used, limit, oldest_in_window, window, now) {
        None => Ok(()),
        Some(retry_after_secs) => Err(AppError::RateLimited { retry_after_secs }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_limit_turns_the_cap_off() {
        let now = Utc::now();
        assert!(cap_outcome(0, 0, None, topup_window(), now).is_none());
        assert!(cap_outcome(10_000_000, 0, Some(now), topup_window(), now).is_none());
    }

    #[test]
    fn the_cap_is_reached_at_the_limit_not_past_it() {
        let now = Utc::now();
        let window = topup_window();

        assert!(
            cap_outcome(4, 5, Some(now), window, now).is_none(),
            "one under the cap is fine"
        );
        assert_eq!(
            cap_outcome(5, 5, Some(now), window, now),
            Some(3600),
            "exactly at the cap is out of budget"
        );
        assert_eq!(
            cap_outcome(6, 5, Some(now), window, now),
            Some(3600),
            "over the cap stays refused"
        );
    }

    #[test]
    fn the_refusal_is_the_time_until_the_oldest_row_ages_out() {
        let now = Utc::now();
        let window = topup_window();

        // Created `now`: the full window still remains.
        assert_eq!(cap_outcome(5, 5, Some(now), window, now), Some(3600));
        // Half the window already elapsed.
        assert_eq!(
            cap_outcome(5, 5, Some(now - Duration::minutes(30)), window, now),
            Some(1800)
        );
    }

    #[test]
    fn the_refusal_rounds_up_and_is_never_zero() {
        let now = Utc::now();
        let window = topup_window();

        // A row that ages out in 500ms leaves 500ms of window: that must read as
        // 1, not 0 — a zero invites an instant retry that is refused again.
        let half_a_second_left = now - window + Duration::milliseconds(500);
        assert_eq!(
            cap_outcome(5, 5, Some(half_a_second_left), window, now),
            Some(1)
        );

        // 1ms of window left still reads as 1.
        let one_ms_left = now - window + Duration::milliseconds(1);
        assert_eq!(cap_outcome(5, 5, Some(one_ms_left), window, now), Some(1));

        // Already aged out: floor at 1 rather than a negative.
        let aged_out = now - window - Duration::milliseconds(1);
        assert_eq!(cap_outcome(5, 5, Some(aged_out), window, now), Some(1));
    }

    #[test]
    fn the_two_windows_match_the_config_key_names() {
        // apikita.toml states `topup_per_hour` and `key_creation_per_day`; these
        // windows are the unit those names promise.
        assert_eq!(topup_window(), Duration::hours(1));
        assert_eq!(key_creation_window(), Duration::days(1));
    }

    // -----------------------------------------------------------------------
    // Live Sqlite. Ignored rather than silently skipped, exactly like the
    // settlement tests in `db.rs`: a test that asserts nothing is worse than no
    // test.
    //
    //   DATABASE_URL=... cargo test --lib abuse:: -- --ignored
    // -----------------------------------------------------------------------

    /// The configured caps, read the way the application reads them.
    fn configured_limits() -> crate::config::LimitsConfig {
        let paths = ["../config/apikita.toml", "config/apikita.toml"];
        for path in paths {
            if std::path::Path::new(path).exists() {
                return crate::config::AppConfig::load_from_file(path)
                    .expect("parse apikita.toml")
                    .limits;
            }
        }
        panic!("could not find apikita.toml for testing");
    }

    async fn test_pool() -> SqlitePool {
        let database_url = std::env::var("DATABASE_URL")
            .expect("set DATABASE_URL to a migrated Sqlite instance");
        crate::db::init_pool(&database_url)
            .await
            .expect("connect to Sqlite")
    }

    async fn create_account(pool: &SqlitePool) -> Uuid {
        let pb_user_id = format!("test_{}", Uuid::new_v4().simple());
        sqlx::query_scalar("INSERT INTO accounts (pb_user_id) VALUES (?) RETURNING id")
            .bind(&pb_user_id)
            .fetch_one(pool)
            .await
            .expect("create account")
    }

    /// Deletes every row the fixture created, in FK order: `topups` references
    /// `accounts` ON DELETE RESTRICT, so the children go first.
    async fn delete_fixture(pool: &SqlitePool, account_id: Uuid) {
        for statement in [
            "DELETE FROM topups WHERE account_id = ?",
            "DELETE FROM api_keys WHERE account_id = ?",
            "DELETE FROM accounts WHERE id = ?",
        ] {
            sqlx::query(statement)
                .bind(account_id.hyphenated())
                .execute(pool)
                .await
                .unwrap_or_else(|err| panic!("cleanup failed on `{statement}`: {err}"));
        }
    }

    async fn insert_topup(pool: &SqlitePool, account_id: Uuid) {
        sqlx::query("INSERT INTO topups (account_id, amount_idr, order_id) VALUES (?, 10000, ?)")
            .bind(account_id.hyphenated())
            .bind(format!("test_cap_{}", Uuid::new_v4().simple()))
            .execute(pool)
            .await
            .expect("insert topup");
    }

    #[ignore = "requires live Sqlite: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn the_topup_cap_lets_the_limit_through_and_refuses_the_next() {
        let pool = test_pool().await;
        let account_id = create_account(&pool).await;
        let limit = configured_limits().topup_per_hour;
        assert!(limit > 0, "the fixture assumes a configured hourly cap");

        let now = Utc::now();
        for _ in 0..limit - 1 {
            insert_topup(&pool, account_id).await;
        }

        assert!(
            enforce_creation_cap(&pool, "topups", topup_window(), limit, account_id, now)
                .await
                .is_ok(),
            "one under the cap must be allowed"
        );

        insert_topup(&pool, account_id).await;
        let err = enforce_creation_cap(&pool, "topups", topup_window(), limit, account_id, now)
            .await
            .expect_err("a top-up at the cap must be refused");

        assert_eq!(err.status_code(), axum::http::StatusCode::TOO_MANY_REQUESTS);
        match err {
            AppError::RateLimited { retry_after_secs } => {
                assert!(
                    (1..=3601).contains(&retry_after_secs),
                    "Retry-After {retry_after_secs} must be inside the hourly window"
                );
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }

        // A row older than the window no longer counts, so the cap frees.
        //
        // Bound from Rust, not `datetime('now','-2 hours')`: the SQLite function
        // emits the space-separated format, which the `created_at` GLOB CHECK
        // refuses (plan section 4.6, rule 3). The Postgres `now() - interval`
        // form has no equivalent that both computes the offset and keeps the
        // RFC3339-offset representation.
        sqlx::query("UPDATE topups SET created_at = ? WHERE account_id = ?")
            .bind(Utc::now() - Duration::hours(2))
            .bind(account_id.hyphenated())
            .execute(&pool)
            .await
            .expect("age the rows out of the window");

        assert!(
            enforce_creation_cap(
                &pool,
                "topups",
                topup_window(),
                limit,
                account_id,
                Utc::now()
            )
            .await
            .is_ok(),
            "rows outside the window must not count against the cap"
        );

        delete_fixture(&pool, account_id).await;
    }

    #[ignore = "requires live Sqlite: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn the_key_creation_cap_refuses_past_the_configured_daily_limit() {
        let pool = test_pool().await;
        let account_id = create_account(&pool).await;
        let limit = configured_limits().key_creation_per_day;
        assert!(limit > 0, "the fixture assumes a configured daily cap");

        let now = Utc::now();
        for _ in 0..limit {
            sqlx::query(
                "INSERT INTO api_keys (account_id, key_hash, prefix) VALUES (?, ?, 'apk_test')",
            )
            .bind(account_id.hyphenated())
            .bind(format!("test_hash_{}", Uuid::new_v4().simple()))
            .execute(&pool)
            .await
            .expect("insert api key");
        }

        let err = enforce_creation_cap(
            &pool,
            "api_keys",
            key_creation_window(),
            limit,
            account_id,
            now,
        )
        .await
        .expect_err("a key at the cap must be refused");

        assert_eq!(err.status_code(), axum::http::StatusCode::TOO_MANY_REQUESTS);
        match err {
            AppError::RateLimited { retry_after_secs } => {
                assert!(
                    (1..=86_401).contains(&retry_after_secs),
                    "Retry-After {retry_after_secs} must be inside the daily window"
                );
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }

        delete_fixture(&pool, account_id).await;
    }
}
