use std::sync::OnceLock;

use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Json},
};
use axum_extra::extract::cookie::{Cookie, SameSite};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};
use uuid::fmt::Hyphenated;
use uuid::Uuid;

use crate::config::{AppConfig, SessionsConfig};
use crate::error::AppError;
use crate::routes::{session_token_from_cookie_header, SESSION_COOKIE};

/// SHA-256 hex of a session token, the value the sessions row stores.
/// The account a request's session cookie resolves to, against SQLite.
///
/// The shared resolver in `crate::routes` takes a `SqlitePool` and works for the
/// handlers; this test-side copy exists because the auth tests below drive the
/// handler AND the resolver against the same pool, and the production one is the
/// only other place that knows the hash-then-compare rule. Both are the same
/// three lines, so a divergence would fail a test rather than pass silently.
#[cfg(test)]
async fn resolve_account_from_cookie(
    pool: &SqlitePool,
    headers: &HeaderMap,
) -> Result<Uuid, AppError> {
    let cookie_hdr = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .ok_or(AppError::Unauthenticated)?;

    let token = session_token_from_cookie_header(cookie_hdr).ok_or(AppError::Unauthenticated)?;

    let session = sqlx::query(
        "SELECT account_id FROM sessions WHERE token_hash = ? AND revoked_at IS NULL AND expires_at > ?",
    )
    .bind(hash_token(token))
    .bind(chrono::Utc::now())
    .fetch_optional(pool)
    .await?;

    match session {
        Some(s) => Ok(s.get::<Hyphenated, _>("account_id").into_uuid()),
        None => Err(AppError::Unauthenticated),
    }
}

fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// PocketBase collection whose auth tokens are accepted. Identity lives in
/// PocketBase; Sqlite holds only the pb_user_id reference
/// (docs/architecture/identity.md).
const PB_USERS_COLLECTION: &str = "users";

/// Local-development default. .env.example and docs/local-development.md both
/// pin PocketBase to this address; production sets POCKETBASE_URL.
const DEFAULT_POCKETBASE_URL: &str = "http://127.0.0.1:8090";

#[derive(Debug, Deserialize)]
pub struct AuthExchangeRequest {
    pub pb_token: String,
}

#[derive(Debug, Serialize)]
pub struct AuthExchangeResponse {
    pub account_id: Uuid,
    pub balance_idr: i64,
}

// ---------------------------------------------------------------------------
// PocketBase token verification
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct PbAuthRefreshResponse {
    record: PbRecord,
}

#[derive(Debug, Deserialize)]
struct PbRecord {
    id: String,
}

/// Endpoint that re-validates a PocketBase auth token and returns the current
/// record. PocketBase answers 401 for a token that is invalid, expired, or
/// belongs to a deleted user.
fn pb_refresh_url(base: &str) -> String {
    format!(
        "{}/api/collections/{}/auth-refresh",
        base.trim_end_matches('/'),
        PB_USERS_COLLECTION
    )
}

/// Trust-boundary validation of the id PocketBase hands back: it becomes
/// accounts.pb_user_id, so refuse anything that is not a plausible record id.
fn normalize_pb_user_id(raw: &str) -> Option<String> {
    let id = raw.trim();
    if id.is_empty() || id.len() > 64 || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(id.to_string())
}

/// Extract the record id from a PocketBase auth-refresh body.
fn parse_pb_user_id(body: &str) -> Option<String> {
    let parsed: PbAuthRefreshResponse = serde_json::from_str(body).ok()?;
    normalize_pb_user_id(&parsed.record.id)
}

fn pocketbase_base_url() -> String {
    std::env::var("POCKETBASE_URL").unwrap_or_else(|_| DEFAULT_POCKETBASE_URL.to_string())
}

/// Shared client so PocketBase connections are pooled rather than rebuilt per
/// login. Built once, on first exchange.
fn pb_http_client() -> Result<&'static reqwest::Client, AppError> {
    if let Some(client) = PB_HTTP_CLIENT.get() {
        return Ok(client);
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| AppError::Internal(format!("failed to build PocketBase client: {e}")))?;
    Ok(PB_HTTP_CLIENT.get_or_init(|| client))
}

static PB_HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

/// What a PocketBase auth-refresh HTTP status means for OUR caller.
///
/// Extracted from `verify_pb_token` because it is the whole of the contract and
/// it needs no network to state: the function below cannot be driven without a
/// live PocketBase (which is why two tests in this crate carry `#[ignore]`), but
/// this decision can be, and that is precisely the part that was wrong.
///
/// The distinction is between an upstream that is BROKEN and one that ANSWERED:
///
/// - **2xx** - the token is good; the caller parses the record id out of the body,
///   so a 2xx with an unreadable body is still a rejection (`parse_pb_user_id`)
///   rather than a retryable outage: PocketBase answered, we just cannot use it.
/// - **any other status with a 5xx or 429 shape** - PocketBase is down, restarting,
///   or throttling us. `AppError::Internal` -> 500, so the client retries instead
///   of throwing the session away (docs/architecture/identity.md: an auth outage
///   must not lock users out). Before this mapping existed, every one of these was
///   collapsed into 401 by `!status.is_success()`, and a 30-second PocketBase
///   restart logged every signed-in customer out.
/// - **any remaining 4xx** - PocketBase actively refused this credential. That is
///   the only shape that may be `Unauthenticated`.
///
/// Classified by `is_server_error() || TOO_MANY_REQUESTS` rather than by an
/// explicit 4xx list on purpose: a 5xx shape PocketBase grows later is treated as
/// an outage by default, which is the safe direction - the cost of a wrong "retry"
/// is a wasted request, and the cost of a wrong "your token is dead" is a
/// customer logged out mid-use.
fn pb_status_to_error(status: reqwest::StatusCode) -> Result<(), AppError> {
    if status.is_success() {
        return Ok(());
    }

    if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(AppError::Internal(format!(
            "PocketBase auth-refresh returned {status} (outage, retryable)"
        )));
    }

    Err(AppError::Unauthenticated)
}

/// Verify a PocketBase auth token and return the real record id behind it.
///
/// A rejected token is 401. A PocketBase outage is *not* a rejected token: it
/// returns 500 so the client retries instead of discarding a good session
/// (docs/architecture/identity.md - an auth outage must not lock users out).
/// The status split lives in `pb_status_to_error`, which is unit-tested; only the
/// round trip to PocketBase is untestable without a live provider.
async fn verify_pb_token(token: &str) -> Result<String, AppError> {
    let auth_value =
        reqwest::header::HeaderValue::from_str(token).map_err(|_| AppError::Unauthenticated)?;

    let url = pb_refresh_url(&pocketbase_base_url());

    let response = pb_http_client()?
        .post(&url)
        .header(reqwest::header::AUTHORIZATION, auth_value)
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("PocketBase auth-refresh unreachable: {e}")))?;

    // An outage and a rejection are different answers, and only the second one
    // may end a customer's session - see `pb_status_to_error`.
    pb_status_to_error(response.status())?;

    let body = response
        .text()
        .await
        .map_err(|e| AppError::Internal(format!("PocketBase auth-refresh body unreadable: {e}")))?;

    parse_pb_user_id(&body).ok_or(AppError::Unauthenticated)
}

// ---------------------------------------------------------------------------
// Session config
// ---------------------------------------------------------------------------

static SESSIONS_CONFIG: OnceLock<SessionsConfig> = OnceLock::new();

/// SessionsConfig is not part of the router state (State<SqlitePool>), so the config
/// file is read once per process and cached. Same resolution order as main.rs:
/// APIKITA_CONFIG_PATH, then config/, then ../config/.
pub(crate) fn sessions_config() -> Result<&'static SessionsConfig, AppError> {
    if let Some(config) = SESSIONS_CONFIG.get() {
        return Ok(config);
    }

    let path =
        std::env::var("APIKITA_CONFIG_PATH").unwrap_or_else(|_| "config/apikita.toml".into());
    let loaded = AppConfig::load_from_file(&path)
        .or_else(|_| AppConfig::load_from_file("../config/apikita.toml"))
        .map_err(|e| AppError::Internal(format!("failed to load session config: {e}")))?;

    Ok(SESSIONS_CONFIG.get_or_init(|| loaded.sessions))
}

// ---------------------------------------------------------------------------
// Cookie helpers
// ---------------------------------------------------------------------------

/// The opaque session cookie. Attributes per docs/server/api-spec.md:
/// HttpOnly; Secure; SameSite=Lax.
///
/// Returns a `Result` rather than panicking on a parse failure. The value was
/// `.unwrap()`d, which meant a malformed header would panic INSIDE the login
/// handler — a 500 on the endpoint every customer uses to sign in, and one that
/// would be reported as an application crash rather than as the malformed cookie
/// it is. The parse cannot fail for a token this function generates, but the
/// failure is now a returned error the caller can answer honestly.
pub(crate) fn session_cookie(value: String, max_age_days: i64) -> Result<HeaderMap, AppError> {
    let cookie = Cookie::build((SESSION_COOKIE, value))
        .path("/")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::days(max_age_days))
        .build();

    let header_value = cookie
        .to_string()
        .parse()
        .map_err(|_| AppError::Internal("session cookie is not a valid header value".into()))?;

    let mut headers = HeaderMap::new();
    headers.insert(header::SET_COOKIE, header_value);
    Ok(headers)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn exchange_token(
    State(pool): State<SqlitePool>,
    headers: HeaderMap,
    Json(payload): Json<AuthExchangeRequest>,
) -> Result<impl IntoResponse, AppError> {
    if payload.pb_token.trim().is_empty() {
        return Err(AppError::InvalidRequest("pb_token is required".into()));
    }

    // Identity comes from PocketBase, never from the token's shape.
    let pb_user_id = verify_pb_token(payload.pb_token.trim()).await?;

    let sessions = sessions_config()?;

    // `BEGIN IMMEDIATE`, not sqlx's default deferred `BEGIN`: this transaction
    // writes, and taking the write lock up front means no lock upgrade can fail
    // with SQLITE_BUSY_SNAPSHOT (plan section 4.3, trap 2).
    let mut tx = crate::db::begin_immediate(&pool).await?;

    // `id`, `created_at` and `updated_at` are all bound. The Postgres schema
    // defaulted them to `gen_random_uuid()` and `now()`; both defaults were
    // deliberately removed so no SQL-side time can be written (plan section 4.6).
    // On conflict the freshly generated `id` is discarded and only `updated_at`
    // moves — the same semantics the Postgres `DO UPDATE SET updated_at = now()`
    // had, expressed through `excluded` so one instant serves the whole insert.
    let now = Utc::now();
    let account = sqlx::query(
        r#"
        INSERT INTO accounts (id, pb_user_id, created_at, updated_at)
        VALUES (?, ?, ?, ?)
        ON CONFLICT (pb_user_id) DO UPDATE SET updated_at = excluded.updated_at
        RETURNING id, status
        "#,
    )
    .bind(Uuid::new_v4().hyphenated())
    .bind(&pb_user_id)
    .bind(now)
    .bind(now)
    .fetch_one(&mut *tx)
    .await?;

    let account_id: Uuid = account.get::<Hyphenated, _>("id").into_uuid();
    let account_status: String = account.get("status");

    if account_status != "active" {
        return Err(AppError::Unauthenticated);
    }

    // `updated_at` had a Postgres `now()` default and is now bound.
    let wallet = sqlx::query(
        r#"
        INSERT INTO wallets (account_id, balance_idr, updated_at)
        VALUES (?, 0, ?)
        ON CONFLICT (account_id) DO NOTHING
        RETURNING balance_idr
        "#,
    )
    .bind(account_id.hyphenated())
    .bind(now)
    .fetch_optional(&mut *tx)
    .await?;

    let balance_idr = match wallet {
        Some(w) => w.get("balance_idr"),
        None => {
            let existing = sqlx::query("SELECT balance_idr FROM wallets WHERE account_id = ?")
                .bind(account_id.hyphenated())
                .fetch_one(&mut *tx)
                .await?;
            existing.get("balance_idr")
        }
    };

    let session_token = format!("apk_sess_{}", Uuid::new_v4().simple());
    let token_hash = hash_token(&session_token);
    // Absolute lifetime from config. The idle bound (7d) is measured from
    // `last_seen_at`, which the schema now carries and this insert seeds. The
    // expiry is derived from the same instant as the row's other timestamps.
    let expires_at = now + Duration::days(sessions.absolute_days as i64);

    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // `id` had a Postgres `gen_random_uuid()` default; `last_seen_at` and
    // `created_at` had `now()`. All three are bound now. Seeding `last_seen_at`
    // with the login instant means an unused session dies on the idle bound.
    sqlx::query(
        "INSERT INTO sessions (id, account_id, token_hash, expires_at, last_seen_at, user_agent, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(Uuid::new_v4().hyphenated())
    .bind(account_id.hyphenated())
    .bind(token_hash)
    .bind(expires_at)
    .bind(now)
    .bind(user_agent)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    let response_headers = session_cookie(session_token, sessions.absolute_days as i64)?;

    Ok((
        StatusCode::OK,
        response_headers,
        Json(AuthExchangeResponse {
            account_id,
            balance_idr,
        }),
    ))
}

/// Revoke the current session row. A failed revoke is reported rather than
/// swallowed: the cookie is only cleared once the row is actually dead.
pub async fn logout(
    State(pool): State<SqlitePool>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    if let Some(token) = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(session_token_from_cookie_header)
    {
        sqlx::query(
            "UPDATE sessions SET revoked_at = ? WHERE token_hash = ? AND revoked_at IS NULL",
        )
        .bind(Utc::now())
        .bind(hash_token(token))
        .execute(&pool)
        .await?;
    }

    Ok((StatusCode::NO_CONTENT, session_cookie(String::new(), 0)?))
}

/// Revoke every live session for the account - other devices are logged out
/// immediately (docs/server/api-spec.md, auth).
pub async fn logout_all(
    State(pool): State<SqlitePool>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    if let Some(token) = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(session_token_from_cookie_header)
    {
        let session = sqlx::query(
            "SELECT account_id FROM sessions WHERE token_hash = ? AND revoked_at IS NULL AND expires_at > ?",
        )
        .bind(hash_token(token))
        .bind(Utc::now())
        .fetch_optional(&pool)
        .await?;

        if let Some(s) = session {
            let account_id: Uuid = s.get::<Hyphenated, _>("account_id").into_uuid();
            sqlx::query(
                "UPDATE sessions SET revoked_at = ? WHERE account_id = ? AND revoked_at IS NULL",
            )
            .bind(Utc::now())
            .bind(account_id.hyphenated())
            .execute(&pool)
            .await?;
        }
    }

    Ok((StatusCode::NO_CONTENT, session_cookie(String::new(), 0)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The documented promise the code does not keep: "A PocketBase outage is
    /// *not* a rejected token: it returns 500 so the client retries instead of
    /// discarding a good session (docs/architecture/identity.md - an auth outage
    /// must not lock users out)" (the doc comment on `verify_pb_token` itself).
    ///
    /// Only the TRANSPORT-error branch upheld that. A PocketBase that ANSWERED with
    /// a 500 - a crashed collection, a proxy error page, a restarted instance -
    /// was collapsed by `!response.status().is_success()` into
    /// `AppError::Unauthenticated`, i.e. a 401. The client then DISCARDS the user's
    /// token and sends them to the login page: the outage is reported as "your
    /// credential is bad", which is both false and unrecoverable by retrying.
    ///
    /// Written RED FIRST against the pre-fix code, which returned `Unauthenticated`
    /// for every one of the 5xx/429 cases below.
    ///
    /// The platform vendors the mapping into a pure function deliberately: no
    /// network seam is needed to pin it, which is the reason the two remaining
    /// `#[ignore]`d tests in this crate have to exist at all.
    #[test]
    fn a_pocketbase_outage_is_retryable_and_only_a_rejection_is_unauthenticated() {
        use reqwest::StatusCode as Pb;

        // The upstream is BROKEN, not the credential. 500/502/503/504 are outage
        // shapes; 429 is "try again shortly" and explicitly not a bad token.
        for status in [
            Pb::INTERNAL_SERVER_ERROR,
            Pb::BAD_GATEWAY,
            Pb::SERVICE_UNAVAILABLE,
            Pb::GATEWAY_TIMEOUT,
            Pb::TOO_MANY_REQUESTS,
        ] {
            assert!(
                matches!(pb_status_to_error(status), Err(AppError::Internal(_))),
                "a PocketBase {status} is an OUTAGE: it must be Internal (500, retryable), never Unauthenticated, or a customer's good session is thrown away and they cannot retry their way back in"
            );
        }

        // The upstream ANSWERED and rejected the credential. This is the only
        // shape that may tell the client its token is dead.
        for status in [
            Pb::UNAUTHORIZED,
            Pb::FORBIDDEN,
            Pb::NOT_FOUND,
            Pb::BAD_REQUEST,
        ] {
            assert!(
                matches!(pb_status_to_error(status), Err(AppError::Unauthenticated)),
                "a PocketBase {status} is a REJECTED credential and must stay a 401 - changing this would let a dead token look like a transient failure"
            );
        }

        // The positive control: success is not an error at all, so the mapping
        // cannot be "everything is Internal" or "everything is Unauthenticated".
        assert!(
            pb_status_to_error(Pb::OK).is_ok(),
            "a 2xx must map to Ok, or nothing above measures a real boundary"
        );
    }

    #[test]
    fn refresh_url_trims_trailing_slash() {
        assert_eq!(
            pb_refresh_url("http://127.0.0.1:8090"),
            "http://127.0.0.1:8090/api/collections/users/auth-refresh"
        );
        assert_eq!(
            pb_refresh_url("https://id.example.com/"),
            "https://id.example.com/api/collections/users/auth-refresh"
        );
    }

    #[test]
    fn parses_record_id_from_auth_refresh_body() {
        let body = r#"{"token":"new.jwt.value","record":{"id":"a1b2c3d4e5f6g7h","email":"u@example.com"}}"#;
        assert_eq!(parse_pb_user_id(body).as_deref(), Some("a1b2c3d4e5f6g7h"));
    }

    #[test]
    fn rejects_unparseable_or_empty_record_id() {
        assert_eq!(parse_pb_user_id("not json"), None);
        assert_eq!(parse_pb_user_id("{}"), None);
        assert_eq!(parse_pb_user_id(r#"{"record":{}}"#), None);
        assert_eq!(parse_pb_user_id(r#"{"record":{"id":""}}"#), None);
        // An error body is not an identity.
        assert_eq!(
            parse_pb_user_id(r#"{"code":401,"message":"Failed to authenticate."}"#),
            None
        );
    }

    #[test]
    fn normalizes_pb_user_id() {
        assert_eq!(
            normalize_pb_user_id("  abc123  ").as_deref(),
            Some("abc123")
        );
        assert_eq!(normalize_pb_user_id(""), None);
        assert_eq!(normalize_pb_user_id("   "), None);
        assert_eq!(normalize_pb_user_id("has space"), None);
        assert_eq!(normalize_pb_user_id("has-dash"), None);
        assert_eq!(normalize_pb_user_id(&"a".repeat(65)), None);
    }

    #[test]
    fn session_cookie_carries_expected_attributes() {
        let headers = session_cookie("apk_sess_abc".into(), 30).expect("a valid token parses");
        let value = headers
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .expect("set-cookie present");
        assert!(value.starts_with("session=apk_sess_abc"), "got {value}");
        assert!(value.contains("HttpOnly"), "got {value}");
        assert!(value.contains("Secure"), "got {value}");
        assert!(value.contains("SameSite=Lax"), "got {value}");
        assert!(value.contains("Max-Age=2592000"), "got {value}");

        // Clearing the cookie expires it immediately.
        let cleared = session_cookie(String::new(), 0).expect("a clear-cookie parses");
        let value = cleared
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .expect("set-cookie present");
        assert!(value.contains("Max-Age=0"), "got {value}");
    }

    // -----------------------------------------------------------------------
    // LIVE-DATABASE TESTS - the three auth handlers
    //
    // Everything above is pure: pb_refresh_url, normalize_pb_user_id,
    // parse_pb_user_id and session_cookie. None of the three handlers -
    // exchange_token, logout, logout_all - had ever been executed by any test.
    // The tests below run them against a real, migrated Postgres, and
    // exchange_token against the real local PocketBase (docker-compose.yml),
    // because verify_pb_token has no seam to fake: mocking the network would
    // test the mock. They are #[ignore]d so the default suite stays green
    // without a database:
    //
    //   DATABASE_URL=postgres://postgres:dev@localhost:5432/apikita \
    //     cargo test --lib -- --ignored --test-threads=1 routes::auth
    // -----------------------------------------------------------------------

    use crate::db::{credit_topup_transaction, TopupCreditResult};
    use crate::test_support::{self, TestDb};
    use axum::body::to_bytes;
    use chrono::DateTime;
    use serde_json::{json, Value};

    /// docs/observability.md's reconciliation, scoped to one account:
    /// wallets.balance_idr must equal SUM(ledger.delta_idr).
    async fn ledger_drift_rows(pool: &SqlitePool, account_id: Uuid) -> i64 {
        sqlx::query_scalar(
            r#"
            SELECT COUNT(*) FROM (
                SELECT w.account_id
                FROM wallets w
                LEFT JOIN ledger l ON l.account_id = w.account_id
                WHERE w.account_id = ?
                GROUP BY w.account_id, w.balance_idr
                HAVING w.balance_idr <> COALESCE(SUM(l.delta_idr), 0)
            ) AS drift
            "#,
        )
        .bind(account_id.hyphenated())
        .fetch_one(pool)
        .await
        .expect("reconciliation query")
    }

    /// Money enters a wallet only through the REAL path: a topups row, then
    /// credit_topup_transaction, which settles it and appends the matching +
    /// ledger row in the same transaction. Writing wallets.balance_idr directly
    /// would manufacture the very drift the drift assertion then reports.
    async fn settle_topup(pool: &SqlitePool, account_id: Uuid, amount_idr: i64) {
        let order_id = format!("test_topup_{}", Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO topups (id, account_id, amount_idr, order_id, status, rail, created_at)
             VALUES (?, ?, ?, ?, 'pending', 'midtrans', ?)",
        )
        .bind(Uuid::new_v4().hyphenated())
        .bind(account_id.hyphenated())
        .bind(amount_idr)
        .bind(&order_id)
        .bind(Utc::now())
        .execute(pool)
        .await
        .expect("create topup");

        let credited = credit_topup_transaction(pool, &order_id, amount_idr)
            .await
            .expect("credit the top-up through the real money path");
        assert!(
            matches!(credited, TopupCreditResult::Settled { .. }),
            "the fixture must settle the top-up, got {credited:?}"
        );
    }

    /// Sessions that are actually usable: unrevoked AND unexpired. Both bounds
    /// matter - an expired-but-unrevoked row is not a credential, and counting
    /// it as "live" would make an expired cookie look like a valid session.
    async fn live_sessions(pool: &SqlitePool, account_id: Uuid) -> i64 {
        // Both bounds are bound: an earlier port bound only the account, leaving the
        // expiry comparison against NULL - which is never TRUE, so every count came
        // back 0 and the assertions below read as "logout deleted rows".
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM sessions WHERE account_id = ? AND revoked_at IS NULL AND expires_at > ?",
        )
        .bind(account_id.hyphenated())
        .bind(Utc::now())
        .fetch_one(pool)
        .await
        .expect("count live sessions")
    }

    /// A live session for an existing account, created the way exchange_token
    /// creates one: the row holds only the SHA-256 of the token.
    async fn add_live_session(
        pool: &SqlitePool,
        account_id: Uuid,
        expires_at: DateTime<Utc>,
    ) -> String {
        let token = format!("apk_sess_{}", Uuid::new_v4().simple());
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO sessions (id, account_id, token_hash, expires_at, last_seen_at, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(Uuid::new_v4().hyphenated())
        .bind(account_id.hyphenated())
        .bind(hash_token(&token))
        .bind(expires_at)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .expect("create session");
        token
    }

    /// An account with a live session, built the way the login path builds one:
    /// an accounts row, the zero-balance wallets row, and a sessions row.
    struct LiveAccount {
        account_id: Uuid,
        token: String,
    }

    async fn live_account(pool: &SqlitePool) -> LiveAccount {
        // Ported: the Postgres original leaned on column DEFAULTS for `accounts.id`,
        // `accounts.created_at` and `wallets.updated_at`; the strict SQLite schema has
        // none, so the shared fixture binds them (plan section 4.1, correction 1).
        let account_id = test_support::account(pool).await;
        test_support::wallet(pool, account_id).await;

        let token = add_live_session(pool, account_id, Utc::now() + Duration::days(30)).await;

        LiveAccount { account_id, token }
    }

    fn cookie_header(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            format!("a=1; session={token}; b=2").parse().unwrap(),
        );
        headers
    }

    /// Everything the assertions need out of a handler's response. The raw body
    /// is kept as bytes because logout answers 204 with no body at all, so the
    /// body cannot be assumed to be JSON.
    struct LiveResponse {
        status: StatusCode,
        headers: HeaderMap,
        body: Vec<u8>,
    }

    /// Drives a handler exactly the way the router does.
    async fn call<F, T>(result: F) -> LiveResponse
    where
        F: std::future::Future<Output = Result<T, AppError>>,
        T: IntoResponse,
    {
        let response = match result.await {
            Ok(ok) => ok.into_response(),
            Err(err) => err.into_response(),
        };
        let status = response.status();
        let headers = response.headers().clone();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("every response must have a readable body")
            .to_vec();
        LiveResponse {
            status,
            headers,
            body,
        }
    }

    impl LiveResponse {
        fn body_text(&self) -> String {
            String::from_utf8_lossy(&self.body).to_string()
        }

        /// The JSON body. Only called where one is expected: docs/error-model.md
        /// makes every error a JSON body, and the success paths under test here
        /// are 200 (JSON) or 204 (empty).
        fn json(&self) -> Value {
            assert!(!self.body.is_empty(), "expected a JSON body, got none");
            serde_json::from_slice(&self.body)
                .unwrap_or_else(|e| panic!("response body is not JSON ({e}): {}", self.body_text()))
        }

        /// The session cookie the handler set. All three handlers always set one.
        fn set_cookie(&self) -> String {
            self.headers
                .get(header::SET_COOKIE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_else(|| panic!("no Set-Cookie header on a {}", self.status))
                .to_string()
        }

        /// The session cookie the handler cleared, as the credential it carries
        /// - which must be nothing.
        fn cleared_token(&self) -> Option<String> {
            session_token_from_cookie_header(&self.set_cookie()).map(|t| t.to_string())
        }
    }

    // -----------------------------------------------------------------------
    // PocketBase identity fixtures. exchange_token verifies its token against
    // PocketBase over the network, so the fixture is a REAL PocketBase record
    // with a real auth token - never a stub.
    // -----------------------------------------------------------------------

    struct PbIdentity {
        record_id: String,
        token: String,
    }

    /// The PocketBase collection base. The record endpoints hang off /records,
    /// but auth-with-password is served by the collection itself - putting it
    /// under /records is a 404, not an auth failure.
    fn pb_collection_url() -> String {
        format!(
            "{}/api/collections/{}",
            pocketbase_base_url().trim_end_matches('/'),
            PB_USERS_COLLECTION
        )
    }

    fn pb_records_url() -> String {
        format!("{}/records", pb_collection_url())
    }

    /// Create a PocketBase user with a recognisable prefix and mint a real auth
    /// token for it. The record id PocketBase generates is alphanumeric and
    /// lowercase, so it survives normalize_pb_user_id unchanged and is exactly
    /// what exchange_token must write to accounts.pb_user_id.
    async fn pocketbase_test_identity(client: &reqwest::Client, tag: &str) -> PbIdentity {
        let email = format!("test_auth_{tag}@apikita-test.invalid");
        let password = format!("test-pw-{tag}");

        let created: Value = client
            .post(pb_records_url())
            .json(&json!({
                "email": email,
                "password": password,
                "passwordConfirm": password,
            }))
            .send()
            .await
            .expect("PocketBase must be reachable: docker compose up -d")
            .json()
            .await
            .expect("PocketBase answers JSON");
        let record_id = created["id"]
            .as_str()
            .unwrap_or_else(|| panic!("PocketBase did not return a record id: {created}"))
            .to_string();

        let auth: Value = client
            .post(format!("{}/auth-with-password", pb_collection_url()))
            .json(&json!({ "identity": email, "password": password }))
            .send()
            .await
            .expect("PocketBase auth must answer")
            .json()
            .await
            .expect("PocketBase answers JSON");
        let token = auth["token"]
            .as_str()
            .unwrap_or_else(|| panic!("PocketBase did not return a token: {auth}"))
            .to_string();

        PbIdentity { record_id, token }
    }

    /// Remove the PocketBase fixture. Best effort on purpose: it runs after the
    /// assertions, and a cleanup hiccup must not masquerade as a failed
    /// assertion (or hide one).
    async fn delete_pb_identity(client: &reqwest::Client, identity: &PbIdentity) {
        let _ = client
            .delete(format!("{}/{}", pb_records_url(), identity.record_id))
            .header(reqwest::header::AUTHORIZATION, identity.token.clone())
            .send()
            .await;
    }

    /// The account a PocketBase identity resolved to, if any.
    async fn account_for_pb_user(pool: &SqlitePool, pb_user_id: &str) -> Option<Uuid> {
        sqlx::query_scalar("SELECT id FROM accounts WHERE pb_user_id = ?")
            .bind(pb_user_id)
            .fetch_optional(pool)
            .await
            .expect("query accounts")
    }

    // -----------------------------------------------------------------------
    // 1. exchange_token
    // -----------------------------------------------------------------------

    // The ONLY remaining exception in this module, and it is not about the database:
    // `verify_pb_token` has no seam to fake, so this test drives a real local
    // PocketBase (POCKETBASE_URL) end to end. Its database half is now the same
    // per-test migrated SQLite file every other test uses, so what is missing is
    // the identity provider, not Postgres.
    #[ignore = "requires a live local PocketBase at POCKETBASE_URL: this test drives a real auth exchange and has no seam to fake"]
    #[tokio::test]
    async fn live_exchange_token_returns_the_real_balance_and_stores_only_the_hash() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let client = reqwest::Client::new();

        let tag = Uuid::new_v4().simple().to_string();
        let fresh = pocketbase_test_identity(&client, &format!("fresh{tag}")).await;
        let seeded = pocketbase_test_identity(&client, &format!("seeded{tag}")).await;

        // The second identity already owns an account with REAL money, credited
        // through the only sanctioned path, so the exchange has to read the
        // wallet rather than assume a new account.
        let seeded_pb_user_id = seeded.record_id.clone();
        // Ported: `accounts.id`, `created_at`, `updated_at` and `wallets.updated_at`
        // have no DEFAULT in the strict SQLite schema, so the Postgres shape (which
        // relied on one) fails at runtime rather than at compile time.
        let seeded_account_id = test_support::account(&pool).await;
        sqlx::query("UPDATE accounts SET pb_user_id = ? WHERE id = ?")
            .bind(&seeded_pb_user_id)
            .bind(seeded_account_id.hyphenated())
            .execute(&pool)
            .await
            .expect("link the pre-existing account to its PocketBase identity");
        test_support::wallet(&pool, seeded_account_id).await;
        settle_topup(&pool, seeded_account_id, 73_500).await;

        assert!(
            account_for_pb_user(&pool, &fresh.record_id).await.is_none(),
            "fixture hygiene: the fresh identity must not own an account yet"
        );

        let outcome = tokio::spawn(exchange_token_assertions(
            pool.clone(),
            fresh.record_id.clone(),
            fresh.token.clone(),
            fresh.token.clone(),
            seeded_account_id,
            seeded.token.clone(),
        ));
        let outcome = outcome.await;

        // Teardown: sessions -> wallets -> accounts for whatever the handler
        // created, then the PocketBase records.
        let mut created = vec![seeded_account_id];
        for pb_user_id in [&fresh.record_id, &seeded.record_id] {
            if let Some(account_id) = account_for_pb_user(&pool, pb_user_id).await {
                created.push(account_id);
            }
        }
        db.close().await;
        delete_pb_identity(&client, &fresh).await;
        delete_pb_identity(&client, &seeded).await;
        outcome.expect("the exchange_token assertions panicked");
    }

    async fn exchange_token_assertions(
        pool: SqlitePool,
        fresh_pb_user_id: String,
        fresh_token: String,
        rejected_token: String,
        seeded_account_id: Uuid,
        seeded_token: String,
    ) {
        // --- a brand-new identity: the account and wallet are created ---
        let mut headers = HeaderMap::new();
        headers.insert(header::USER_AGENT, "apikita-live-test".parse().unwrap());

        let response = call(exchange_token(
            State(pool.clone()),
            headers.clone(),
            Json(AuthExchangeRequest {
                pb_token: fresh_token.clone(),
            }),
        ))
        .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "body: {}",
            response.body_text()
        );

        let body = response.json();
        let account_id = Uuid::parse_str(
            body["account_id"]
                .as_str()
                .unwrap_or_else(|| panic!("account_id must be a uuid string: {body}")),
        )
        .expect("account_id must parse as a uuid");
        assert_eq!(
            body["balance_idr"],
            json!(0),
            "a brand-new account has no money, so the exchange must report 0: {body}"
        );

        // The identity written to Postgres is the one PocketBase returned.
        assert_eq!(
            account_for_pb_user(&pool, &fresh_pb_user_id).await,
            Some(account_id),
            "accounts.pb_user_id must be the PocketBase record id, and the handler must create the account"
        );

        // The wallet the balance came from.
        let balance: i64 =
            sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = ?")
                .bind(account_id.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("the exchange must have created the wallet");
        assert_eq!(balance, 0, "the created wallet starts empty");

        // --- the cookie is an opaque credential, and only its hash is stored ---
        let token = response.cleared_token().unwrap_or_else(|| {
            panic!(
                "the login must set a session cookie: {}",
                response.set_cookie()
            )
        });
        assert!(
            token.starts_with("apk_sess_"),
            "the session cookie must carry the opaque token, got {token}"
        );

        let rows: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT token_hash, user_agent FROM sessions WHERE account_id = ?")
                .bind(account_id.hyphenated())
                .fetch_all(&pool)
                .await
                .expect("read the session rows");
        assert_eq!(rows.len(), 1, "one exchange = exactly one session row");
        assert_eq!(
            rows[0].0,
            hash_token(&token),
            "the row must hold the SHA-256 of the cookie value"
        );
        assert_ne!(
            rows[0].0, token,
            "the plaintext token must never be persisted - only its hash"
        );
        assert_eq!(
            rows[0].1.as_deref(),
            Some("apikita-live-test"),
            "the session must record the requesting user agent"
        );

        // The session lifetime comes from config, not from a constant.
        let expires_at: DateTime<Utc> =
            sqlx::query_scalar("SELECT expires_at FROM sessions WHERE account_id = ?")
                .bind(account_id.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("read expires_at");
        let configured_days = sessions_config()
            .expect("config/apikita.toml must resolve")
            .absolute_days as i64;
        let drift_minutes = (expires_at - Utc::now() - Duration::days(configured_days))
            .num_minutes()
            .abs();
        assert!(
            drift_minutes <= 5,
            "the session must expire in the configured {configured_days} days, got {expires_at} ({drift_minutes} minutes off)"
        );

        // --- the cookie resolves, through the one reader every endpoint uses ---
        assert_eq!(
            resolve_account_from_cookie(&pool, &cookie_header(&token))
                .await
                .expect("the fresh session must resolve"),
            account_id
        );

        // --- a second exchange reuses the account and the wallet ---
        let again = call(exchange_token(
            State(pool.clone()),
            HeaderMap::new(),
            Json(AuthExchangeRequest {
                pb_token: fresh_token,
            }),
        ))
        .await;
        assert_eq!(again.status, StatusCode::OK, "body: {}", again.body_text());
        let again_body = again.json();
        assert_eq!(
            again_body["account_id"].as_str(),
            Some(account_id.to_string().as_str()),
            "accounts.pb_user_id is UNIQUE, so a second login must resolve the same account: {again_body}"
        );
        let wallet_rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM wallets WHERE account_id = ?")
                .bind(account_id.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("count wallets");
        assert_eq!(wallet_rows, 1, "the wallet must not be duplicated");
        let session_rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE account_id = ?")
                .bind(account_id.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("count sessions");
        assert_eq!(session_rows, 2, "each exchange is its own session");

        // --- an existing account with real money: the REAL balance is returned ---
        let seeded = call(exchange_token(
            State(pool.clone()),
            HeaderMap::new(),
            Json(AuthExchangeRequest {
                pb_token: seeded_token,
            }),
        ))
        .await;
        assert_eq!(
            seeded.status,
            StatusCode::OK,
            "body: {}",
            seeded.body_text()
        );
        let seeded_body = seeded.json();
        assert_eq!(
            seeded_body["account_id"].as_str(),
            Some(seeded_account_id.to_string().as_str()),
            "an existing pb_user_id must resolve to its existing account: {seeded_body}"
        );
        assert_eq!(
            seeded_body["balance_idr"],
            json!(73_500),
            "the exchange must return the wallet's REAL balance, not a constant: {seeded_body}"
        );
        assert_eq!(
            ledger_drift_rows(&pool, seeded_account_id).await,
            0,
            "the fixture must not have manufactured ledger drift"
        );

        // --- the guard that runs before any network call ---
        let empty = call(exchange_token(
            State(pool.clone()),
            HeaderMap::new(),
            Json(AuthExchangeRequest {
                pb_token: "   ".into(),
            }),
        ))
        .await;
        assert_eq!(
            empty.status,
            StatusCode::BAD_REQUEST,
            "an empty pb_token is a malformed request, not a rejected credential: {}",
            empty.body_text()
        );
        assert_eq!(empty.json()["error"]["code"], json!("invalid_request"));

        // --- a token PocketBase rejects is a clean 401, never a panic ---
        let rejected = call(exchange_token(
            State(pool.clone()),
            HeaderMap::new(),
            Json(AuthExchangeRequest {
                pb_token: format!("not-a-real-pb-token-{rejected_token}"),
            }),
        ))
        .await;
        assert_eq!(
            rejected.status,
            StatusCode::UNAUTHORIZED,
            "PocketBase rejects an unknown token with 401, so the exchange must answer 401: {}",
            rejected.body_text()
        );
        assert_eq!(rejected.json()["error"]["code"], json!("unauthenticated"));
        assert!(
            account_for_pb_user(&pool, &fresh_pb_user_id)
                .await
                .is_some(),
            "a rejected exchange must not disturb the sessions that already exist"
        );
    }

    // -----------------------------------------------------------------------
    // 2. logout
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn live_logout_revokes_exactly_this_session_and_clears_the_cookie() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let victim = live_account(&pool).await;
        let sibling = live_account(&pool).await;

        let outcome = tokio::spawn(logout_assertions(
            pool.clone(),
            victim.account_id,
            victim.token.clone(),
            sibling.account_id,
            sibling.token.clone(),
        ));
        let outcome = outcome.await;

        db.close().await;
        outcome.expect("the logout assertions panicked");
    }

    async fn logout_assertions(
        pool: SqlitePool,
        victim_id: Uuid,
        victim_token: String,
        sibling_id: Uuid,
        sibling_token: String,
    ) {
        // A request with no cookie at all is still a 204 that clears the cookie.
        let anonymous = call(logout(State(pool.clone()), HeaderMap::new())).await;
        assert_eq!(anonymous.status, StatusCode::NO_CONTENT);
        assert!(
            anonymous.body.is_empty(),
            "docs/server/api-spec.md: logout is a 204 with no body, got {}",
            anonymous.body_text()
        );
        assert_eq!(
            anonymous.cleared_token(),
            None,
            "the cleared cookie must carry no credential: {}",
            anonymous.set_cookie()
        );
        assert!(
            anonymous.set_cookie().contains("Max-Age=0"),
            "the cookie must be expired immediately: {}",
            anonymous.set_cookie()
        );

        let live_before: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sessions WHERE account_id = ? AND revoked_at IS NULL",
        )
        .bind(victim_id.hyphenated())
        .fetch_one(&pool)
        .await
        .expect("count live sessions");
        assert_eq!(
            live_before, 1,
            "the fixture must start with one live session"
        );

        // --- the real logout ---
        let response = call(logout(State(pool.clone()), cookie_header(&victim_token))).await;
        assert_eq!(response.status, StatusCode::NO_CONTENT);
        assert!(response.body.is_empty(), "logout answers 204, no body");
        assert_eq!(
            response.cleared_token(),
            None,
            "logout must clear the cookie it just revoked: {}",
            response.set_cookie()
        );

        // The row is REVOKED, not deleted: the audit trail survives.
        let revoked_at: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT revoked_at FROM sessions WHERE token_hash = ?")
                .bind(hash_token(&victim_token))
                .fetch_one(&pool)
                .await
                .expect("the session row must still exist");
        assert!(
            revoked_at.is_some(),
            "logout must set revoked_at, not delete the row"
        );

        // The revoked session stops resolving - this is what "real revocation"
        // means for every authenticated endpoint, which all share this reader.
        assert!(
            resolve_account_from_cookie(&pool, &cookie_header(&victim_token))
                .await
                .is_err(),
            "a revoked session must not resolve an account"
        );

        // The sibling session on the OTHER account is untouched, so the UPDATE
        // cannot be revoking more than the row it names.
        assert_eq!(
            resolve_account_from_cookie(&pool, &cookie_header(&sibling_token))
                .await
                .expect("the other account's session must survive"),
            sibling_id
        );
        let sibling_live: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sessions WHERE account_id = ? AND revoked_at IS NULL",
        )
        .bind(sibling_id.hyphenated())
        .fetch_one(&pool)
        .await
        .expect("count the sibling's live sessions");
        assert_eq!(sibling_live, 1);

        // A second logout is idempotent AND must not move the revocation
        // timestamp: that is the "AND revoked_at IS NULL" guard doing its job.
        let second = call(logout(State(pool.clone()), cookie_header(&victim_token))).await;
        assert_eq!(second.status, StatusCode::NO_CONTENT);
        let revoked_again: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT revoked_at FROM sessions WHERE token_hash = ?")
                .bind(hash_token(&victim_token))
                .fetch_one(&pool)
                .await
                .expect("read revoked_at again");
        assert_eq!(
            revoked_again, revoked_at,
            "a second logout must not rewrite the original revocation time"
        );

        // The victim account is otherwise intact.
        let sessions_left: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE account_id = ?")
                .bind(victim_id.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("count the victim's sessions");
        assert_eq!(sessions_left, 1, "logout must not delete rows");
        assert_eq!(
            ledger_drift_rows(&pool, victim_id).await,
            0,
            "the fixture must not have manufactured ledger drift"
        );
    }

    // -----------------------------------------------------------------------
    // 3. logout_all
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn live_logout_all_revokes_every_session_for_the_account_only() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let device_a = live_account(&pool).await;
        // device_b is a SECOND session on the SAME account, which is the whole
        // point of "sign out everywhere": two accounts prove nothing.
        let device_b =
            add_live_session(&pool, device_a.account_id, Utc::now() + Duration::days(30)).await;
        let other = live_account(&pool).await;

        let outcome = tokio::spawn(logout_all_assertions(
            pool.clone(),
            device_a.account_id,
            device_a.token.clone(),
            device_b,
            other.account_id,
            other.token.clone(),
        ));
        let outcome = outcome.await;

        db.close().await;
        outcome.expect("the logout_all assertions panicked");
    }

    async fn logout_all_assertions(
        pool: SqlitePool,
        account_id: Uuid,
        token_a: String,
        token_b: String,
        other_id: Uuid,
        other_token: String,
    ) {
        // A third device for the same account: logout-all must reach sessions it
        // was not told about.
        let token_c = add_live_session(&pool, account_id, Utc::now() + Duration::days(30)).await;

        let live_before = live_sessions(&pool, account_id).await;
        assert_eq!(
            live_before, 3,
            "the fixture must start with three live sessions"
        );

        // --- sign out everywhere, driven by ONE of them ---
        let response = call(logout_all(State(pool.clone()), cookie_header(&token_a))).await;
        assert_eq!(response.status, StatusCode::NO_CONTENT);
        assert!(
            response.body.is_empty(),
            "docs/server/api-spec.md: logout-all is a 204 with no body"
        );
        assert_eq!(
            response.cleared_token(),
            None,
            "logout-all must clear the caller's cookie: {}",
            response.set_cookie()
        );

        let live_after = live_sessions(&pool, account_id).await;
        assert_eq!(
            live_after, 0,
            "docs/architecture/identity.md: sign out everywhere revokes EVERY session for the account"
        );

        // Revoked, not deleted: all three rows are still there.
        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .fetch_one(&pool)
            .await
            .expect("count all sessions");
        assert_eq!(total, 3, "revocation must not delete the session rows");

        // Every one of the account's sessions - including the one that asked -
        // is now dead.
        for (label, token) in [("a", &token_a), ("b", &token_b), ("c", &token_c)] {
            assert!(
                resolve_account_from_cookie(&pool, &cookie_header(token))
                    .await
                    .is_err(),
                "session {label} must be revoked"
            );
        }

        // The OTHER account's session is untouched: the UPDATE is scoped by
        // account_id, not global.
        assert_eq!(
            resolve_account_from_cookie(&pool, &cookie_header(&other_token))
                .await
                .expect("another account's session must survive logout-all"),
            other_id
        );

        // A second logout-all has nothing left to revoke and is still a 204.
        let second = call(logout_all(State(pool.clone()), cookie_header(&token_a))).await;
        assert_eq!(
            second.status,
            StatusCode::NO_CONTENT,
            "logout-all must be idempotent: {}",
            second.body_text()
        );
        assert_eq!(live_sessions(&pool, account_id).await, 0);

        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            0,
            "the fixture must not have manufactured ledger drift"
        );
    }

    /// The "expires_at > ?" clause in logout_all's SELECT. Without it, an
    /// expired-but-unrevoked cookie would be a credential that can sign every
    /// other device out - the one thing a dead session must not be able to do.
    #[tokio::test]
    async fn live_logout_all_ignores_a_session_that_is_not_live() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account = live_account(&pool).await;
        let expired_token =
            add_live_session(&pool, account.account_id, Utc::now() - Duration::days(1)).await;

        let outcome = tokio::spawn(logout_all_ignores_dead_sessions(
            pool.clone(),
            account.account_id,
            account.token.clone(),
            expired_token,
        ));
        let outcome = outcome.await;

        db.close().await;
        outcome.expect("the logout_all dead-session assertions panicked");
    }

    async fn logout_all_ignores_dead_sessions(
        pool: SqlitePool,
        account_id: Uuid,
        live_token: String,
        expired_token: String,
    ) {
        // An expired session cannot resolve an account, so it must not be able
        // to revoke anything.
        assert!(
            resolve_account_from_cookie(&pool, &cookie_header(&expired_token))
                .await
                .is_err(),
            "the fixture's expired session must not resolve"
        );

        let expired = call(logout_all(
            State(pool.clone()),
            cookie_header(&expired_token),
        ))
        .await;
        assert_eq!(
            expired.status,
            StatusCode::NO_CONTENT,
            "an expired cookie is not an error, it is just not a credential: {}",
            expired.body_text()
        );
        let live_after_expired = live_sessions(&pool, account_id).await;
        assert_eq!(
            live_after_expired, 1,
            "an EXPIRED session must not sign the account's live sessions out"
        );

        // No cookie at all: same answer.
        let anonymous = call(logout_all(State(pool.clone()), HeaderMap::new())).await;
        assert_eq!(anonymous.status, StatusCode::NO_CONTENT);
        let live_after_anonymous = live_sessions(&pool, account_id).await;
        assert_eq!(
            live_after_anonymous, 1,
            "a cookie-less logout-all must revoke nothing"
        );

        // ...and the live session still works, so nothing was silently killed.
        assert_eq!(
            resolve_account_from_cookie(&pool, &cookie_header(&live_token))
                .await
                .expect("the live session must be untouched"),
            account_id
        );
    }
}
