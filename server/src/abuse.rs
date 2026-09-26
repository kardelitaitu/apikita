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
use sqlx::{Row, SqlitePool};
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
    use crate::test_support::{self, TestDb};

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
    // Database tests. Each gets its own migrated database in a temp directory,
    // so they run by default and cannot see each other's rows.
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

    /// One top-up at the configured amount, as the create path writes one.
    async fn insert_topup(pool: &SqlitePool, account_id: Uuid) {
        test_support::pending_topup(pool, account_id, 10_000).await;
    }

    #[tokio::test]
    async fn the_topup_cap_lets_the_limit_through_and_refuses_the_next() {
        let db = TestDb::new().await;
        let account_id = test_support::account(&db.pool).await;
        let limit = configured_limits().topup_per_hour;
        assert!(limit > 0, "the fixture assumes a configured hourly cap");

        let now = Utc::now();
        for _ in 0..limit - 1 {
            insert_topup(&db.pool, account_id).await;
        }

        assert!(
            enforce_creation_cap(&db.pool, "topups", topup_window(), limit, account_id, now)
                .await
                .is_ok(),
            "one under the cap must be allowed"
        );

        insert_topup(&db.pool, account_id).await;
        let err = enforce_creation_cap(&db.pool, "topups", topup_window(), limit, account_id, now)
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
            .execute(&db.pool)
            .await
            .expect("age the rows out of the window");

        assert!(
            enforce_creation_cap(
                &db.pool,
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

        db.close().await;
    }

    #[tokio::test]
    async fn the_key_creation_cap_refuses_past_the_configured_daily_limit() {
        let db = TestDb::new().await;
        let account_id = test_support::account(&db.pool).await;
        let limit = configured_limits().key_creation_per_day;
        assert!(limit > 0, "the fixture assumes a configured daily cap");

        let now = Utc::now();
        for _ in 0..limit {
            test_support::api_key(&db.pool, account_id).await;
        }

        let err = enforce_creation_cap(
            &db.pool,
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

        db.close().await;
    }

    // -----------------------------------------------------------------------
    // 2. The OTHER half of the cap: the HANDLER's own refusal.
    //
    // The two tests above drive `enforce_creation_cap` directly, and
    // routes::account's live suite drives `create_topup` (the topups half). The
    // api_keys half - `create_key` answering 429 because its account is at
    // `limits.key_creation_per_day` - was asserted nowhere: the guard's RETURN
    // VALUE was covered, the status the CUSTOMER receives was not. These tests
    // close that, and pin the boundary arithmetic against real rows rather than
    // against Duration constants.
    //
    // The response HEADERS matter as much as the status: error.rs decides the
    // Retry-After in exactly ONE place, so the header a client honours is a
    // property of the refusal this handler produced.
    // -----------------------------------------------------------------------

    use crate::config::{AppConfig, LimitsConfig};
    use crate::ip_tracking::{parse_cidrs, DailySalt};
    use crate::routes::events::RealtimeHub;
    use crate::routes::keys::{create_key, CreateKeyRequest};
    use crate::routes::proxy::AppState;
    use axum::body::to_bytes;
    use axum::extract::State;
    use axum::http::{header, HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::Json;
    use serde_json::{json, Value};
    use std::sync::Arc;

    /// The AppState the router would hand `create_key`, built from the same
    /// config file the server loads.
    fn live_app_state(pool: SqlitePool) -> AppState {
        let config = AppConfig::load_from_file("../config/apikita.toml")
            .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
            .expect("config/apikita.toml must load for the live tests");
        let trusted = parse_cidrs(&config.network.trusted_proxy_cidrs)
            .expect("the config validates its own trusted proxy rules");

        AppState {
            pool,
            http_client: reqwest::Client::new(),
            events: Arc::new(RealtimeHub::new(&config.realtime)),
            config: Arc::new(config),
            ip_salt: Arc::new(DailySalt::new()),
            trusted_proxies: Arc::from(trusted.into_boxed_slice()),
        }
    }

    /// A real `sessions` row and the cookie that resolves to it, so the handler
    /// is reached through the production authentication path
    /// (`resolve_account_from_cookie`) rather than a hand-passed account id.
    async fn session_cookie(pool: &SqlitePool, account_id: Uuid) -> HeaderMap {
        let token = format!("apk_sess_{}", Uuid::new_v4().simple());
        // Every NOT NULL column is bound from Rust: the SQLite schema has no
        // DEFAULT for id, last_seen_at or created_at (plan section 4.1,
        // correction 1), and now() + interval has no SQLite spelling that also
        // keeps the RFC3339-offset form the timestamp GLOB CHECK requires.
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO sessions (id, account_id, token_hash, expires_at, last_seen_at, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(Uuid::new_v4().hyphenated())
        .bind(account_id.hyphenated())
        .bind(crate::routes::hash_token(&token))
        .bind(now + Duration::days(30))
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .expect("create session");

        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            format!("a=1; session={token}; b=2").parse().unwrap(),
        );
        headers
    }

    /// `count` topups rows stamped with an EXPLICIT `created_at`.
    ///
    /// Pinned rather than left to the column default: the subject of every test
    /// below is where a row sits relative to a window boundary, and the code
    /// under test must not manufacture its own inputs.
    async fn insert_topups_at(
        pool: &SqlitePool,
        account_id: Uuid,
        created_at: DateTime<Utc>,
        count: u32,
    ) {
        for _ in 0..count {
            sqlx::query(
                "INSERT INTO topups (id, account_id, amount_idr, order_id, status, created_at)
                 VALUES (?, ?, 10000, ?, 'pending', ?)",
            )
            .bind(Uuid::new_v4().hyphenated())
            .bind(account_id.hyphenated())
            .bind(format!("test_cap_{}", Uuid::new_v4().simple()))
            .bind(created_at)
            .execute(pool)
            .await
            .expect("insert topup at a pinned instant");
        }
    }

    /// `count` api_keys rows stamped with an EXPLICIT `created_at`, for the
    /// same reason: the boundary is the subject.
    async fn insert_api_keys_at(
        pool: &SqlitePool,
        account_id: Uuid,
        created_at: DateTime<Utc>,
        count: u32,
    ) {
        for _ in 0..count {
            sqlx::query(
                "INSERT INTO api_keys (id, account_id, key_hash, prefix, created_at)
                 VALUES (?, ?, ?, 'apk_test', ?)",
            )
            .bind(Uuid::new_v4().hyphenated())
            .bind(account_id.hyphenated())
            .bind(format!("test_hash_{}", Uuid::new_v4().simple()))
            .bind(created_at)
            .execute(pool)
            .await
            .expect("insert api key at a pinned instant");
        }
    }

    /// Moves an existing fixture's rows to a new instant, so one account can
    /// exercise several boundary positions without a second fixture.
    async fn repin_api_keys(pool: &SqlitePool, account_id: Uuid, created_at: DateTime<Utc>) {
        sqlx::query("UPDATE api_keys SET created_at = ? WHERE account_id = ?")
            .bind(created_at)
            .bind(account_id.hyphenated())
            .execute(pool)
            .await
            .expect("re-pin the fixture's api_keys rows");
    }

    fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    }

    /// Drives a handler exactly the way the router does and keeps the HEADERS:
    /// the Retry-After is not in the body, so a status-and-body helper alone
    /// cannot see the half of the refusal the client acts on.
    async fn respond_with_headers<F, T>(result: F) -> (StatusCode, HeaderMap, Value)
    where
        F: std::future::Future<Output = Result<T, AppError>>,
        T: IntoResponse,
    {
        let res = match result.await {
            Ok(ok) => ok.into_response(),
            Err(err) => err.into_response(),
        };
        let status = res.status();
        let headers = res.headers().clone();
        let bytes = to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("every response must have a readable body");
        let body: Value = serde_json::from_slice(&bytes)
            .expect("docs/error-model.md:10 - every response is JSON");
        (status, headers, body)
    }

    /// `create_key` driven through the handler, headers included.
    async fn call_create_key(
        state: &AppState,
        headers: &HeaderMap,
    ) -> (StatusCode, HeaderMap, Value) {
        respond_with_headers(create_key(
            State(state.clone()),
            headers.clone(),
            Json(CreateKeyRequest {
                label: Some("abuse cap fixture".to_string()),
                models: Vec::new(),
                spend_limit_idr: 0,
                token_limit: 0,
                rate_limit_rpm: 0,
                expires_at: None,
            }),
        ))
        .await
    }

    async fn count_api_keys(pool: &SqlitePool, account_id: Uuid) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM api_keys WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .fetch_one(pool)
            .await
            .expect("count api keys")
    }

    /// (a) The HANDLER refuses with 429, and the Retry-After it names is the
    /// time until the OLDEST row ages out - not the newest, and not the window.
    #[tokio::test]
    async fn live_create_key_refuses_with_429_and_the_retry_after_of_the_oldest_row() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account_id = test_support::account(&pool).await;
        let headers = session_cookie(&pool, account_id).await;
        let state = live_app_state(pool.clone());

        let limit = configured_limits().key_creation_per_day;
        assert!(limit > 0, "the fixture assumes a configured daily cap");

        // The OLDEST row is deliberately DISTINCT from the rest. Nine rows are a
        // minute old (reading them would report ~86340s), and the tenth is
        // pinned to age out EXACTLY 1800s after `now`. The handler runs a few
        // milliseconds later, so it sees 1799.99x and rounds up to 1800 - a full
        // second of headroom, against a few milliseconds of observed latency.
        //
        // 1800 is therefore discriminating: a Retry-After read off
        // MAX(created_at) instead of MIN would answer ~86340, one read off the
        // window length would answer 86400, and one read off "now" would answer
        // 86400 as well. None of them can produce 1800.
        let now = Utc::now();
        let ages_out_in_1800s = now - key_creation_window() + Duration::seconds(1800);
        insert_api_keys_at(&pool, account_id, now - Duration::seconds(60), limit - 1).await;
        insert_api_keys_at(&pool, account_id, ages_out_in_1800s, 1).await;

        let (status, refusal, body) = call_create_key(&state, &headers).await;

        assert_eq!(
            status,
            StatusCode::TOO_MANY_REQUESTS,
            "an account at its key_creation_per_day cap must be refused with 429, not 403/422/500. body: {body}"
        );
        assert_eq!(body["error"]["code"], json!("rate_limited"), "{body}");
        assert_eq!(
            header_str(&refusal, header::RETRY_AFTER).as_deref(),
            Some("1800"),
            "the Retry-After must be the time until the OLDEST row ages out"
        );

        // The refusal wrote nothing: the cap counts rows, so a refused request
        // that still inserted one would refill its own budget.
        assert_eq!(
            count_api_keys(&pool, account_id).await,
            i64::from(limit),
            "a refused request must not create a row"
        );

        db.close().await;
    }

    /// (b) Against REAL rows, the refusal rounds UP and is never zero. The unit
    /// test at abuse.rs:179 pins the arithmetic; this pins the same property
    /// when the number is driven by an actual timestamp on an actual row.
    #[tokio::test]
    async fn live_the_handler_refusal_rounds_up_and_is_never_zero() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account_id = test_support::account(&pool).await;
        let headers = session_cookie(&pool, account_id).await;
        let state = live_app_state(pool.clone());

        let limit = configured_limits().key_creation_per_day;
        assert!(limit > 0, "the fixture assumes a configured daily cap");
        insert_api_keys_at(&pool, account_id, Utc::now(), limit).await;

        // (i) 300ms of window left. A client told "0" retries immediately and is
        // refused again, which reads as a broken limiter - so it must read 1.
        // The 300ms is re-derived per call and the margin below absorbs the few
        // milliseconds the handler takes, so the assertion cannot race itself.
        let almost_aged_out = Utc::now() - key_creation_window() + Duration::milliseconds(300);
        repin_api_keys(&pool, account_id, almost_aged_out).await;
        let (status, refusal, body) = call_create_key(&state, &headers).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
        assert_eq!(
            header_str(&refusal, header::RETRY_AFTER).as_deref(),
            Some("1"),
            "a sub-second remainder must be reported as 1, never 0"
        );

        // (ii) 2.9s left rounds UP to 3; truncation would answer 2 and send the
        // client back before the window has actually freed.
        let just_under_three = Utc::now() - key_creation_window() + Duration::milliseconds(2900);
        repin_api_keys(&pool, account_id, just_under_three).await;
        let (status, refusal, body) = call_create_key(&state, &headers).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
        assert_eq!(
            header_str(&refusal, header::RETRY_AFTER).as_deref(),
            Some("3"),
            "2.9s of window must round UP to 3"
        );

        db.close().await;
    }

    /// (c) The two windows are genuinely different, measured against the SAME
    /// real rows: an age that is outside the 1-hour topup window is inside the
    /// 24-hour key window. Duration constants alone cannot show this.
    #[tokio::test]
    async fn live_the_two_windows_differ_measured_against_real_rows() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account_id = test_support::account(&pool).await;
        let headers = session_cookie(&pool, account_id).await;
        let state = live_app_state(pool.clone());

        let topup_limit = configured_limits().topup_per_hour;
        let key_limit = configured_limits().key_creation_per_day;
        assert!(
            topup_limit > 0 && key_limit > 0,
            "the fixture assumes live caps"
        );

        let now = Utc::now();
        let two_hours_old = now - Duration::hours(2);
        insert_topups_at(&pool, account_id, two_hours_old, topup_limit).await;
        insert_api_keys_at(&pool, account_id, two_hours_old, key_limit).await;

        // 1 hour: the rows are OUTSIDE, so a full cap's worth of them does not
        // reach the cap.
        assert!(
            enforce_creation_cap(
                &pool,
                "topups",
                topup_window(),
                topup_limit,
                account_id,
                now
            )
            .await
            .is_ok(),
            "rows two hours old must not count against the one-hour topup cap"
        );

        // 24 hours: the SAME rows are INSIDE, so the same count refuses.
        let err = enforce_creation_cap(
            &pool,
            "topups",
            key_creation_window(),
            topup_limit,
            account_id,
            now,
        )
        .await
        .expect_err("the same rows are inside the 24-hour window");
        assert_eq!(err.status_code(), StatusCode::TOO_MANY_REQUESTS);

        // And the real handler uses the 24-hour window for keys, so the same
        // two-hour-old history refuses it while it left the topup cap alone.
        let (status, _, body) = call_create_key(&state, &headers).await;
        assert_eq!(
            status,
            StatusCode::TOO_MANY_REQUESTS,
            "create_key counts a 24-hour window: two-hour-old rows must refuse. body: {body}"
        );

        db.close().await;
    }

    /// (d) A limit of 0 turns the cap OFF, live, through the handler: the same
    /// rows that refuse above are let through and the key is actually minted.
    #[tokio::test]
    async fn live_a_zero_limit_turns_the_key_cap_off_at_the_handler() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account_id = test_support::account(&pool).await;
        let headers = session_cookie(&pool, account_id).await;

        // The operator's "off" switch is the config value, so the config is what
        // this test changes - not the guard's code.
        let base = live_app_state(pool.clone());
        let limits = LimitsConfig {
            key_creation_per_day: 0,
            ..base.config.limits.clone()
        };
        let state = AppState {
            config: Arc::new(AppConfig {
                limits,
                ..(*base.config).clone()
            }),
            ..base.clone()
        };

        let seeded = configured_limits().key_creation_per_day;
        assert!(seeded > 0, "the fixture assumes a configured daily cap");
        insert_api_keys_at(&pool, account_id, Utc::now(), seeded).await;

        let (status, headers, body) = call_create_key(&state, &headers).await;

        assert_eq!(
            status,
            StatusCode::CREATED,
            "key_creation_per_day = 0 disables the cap (abuse::cap_outcome), so \
             {seeded} rows inside the window must not refuse. body: {body}"
        );
        assert!(
            header_str(&headers, header::RETRY_AFTER).is_none(),
            "a 201 must not carry a Retry-After"
        );
        assert_eq!(
            count_api_keys(&pool, account_id).await,
            i64::from(seeded) + 1,
            "the handler must have proceeded to mint the key"
        );

        db.close().await;
    }
}
