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
use sqlx::{SqlitePool, Row};
use uuid::Uuid;
use uuid::fmt::Hyphenated;

use crate::config::{AppConfig, SessionsConfig};
use crate::error::AppError;

/// Cookie carrying the opaque session value. Only its SHA-256 is stored
/// (docs/website/02-data-model.md, sessions).
const SESSION_COOKIE: &str = "session";

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

fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
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

/// Verify a PocketBase auth token and return the real record id behind it.
///
/// A rejected token is 401. A PocketBase outage is *not* a rejected token: it
/// returns 500 so the client retries instead of discarding a good session
/// (docs/architecture/identity.md - an auth outage must not lock users out).
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

    if !response.status().is_success() {
        return Err(AppError::Unauthenticated);
    }

    let body = response.text().await.map_err(|e| {
        AppError::Internal(format!("PocketBase auth-refresh body unreadable: {e}"))
    })?;

    parse_pb_user_id(&body).ok_or(AppError::Unauthenticated)
}

// ---------------------------------------------------------------------------
// Session config
// ---------------------------------------------------------------------------

static SESSIONS_CONFIG: OnceLock<SessionsConfig> = OnceLock::new();

/// SessionsConfig is not part of the router state (State<SqlitePool>), so the config
/// file is read once per process and cached. Same resolution order as main.rs:
/// APIKITA_CONFIG_PATH, then config/, then ../config/.
fn sessions_config() -> Result<&'static SessionsConfig, AppError> {
    if let Some(config) = SESSIONS_CONFIG.get() {
        return Ok(config);
    }

    let path = std::env::var("APIKITA_CONFIG_PATH").unwrap_or_else(|_| "config/apikita.toml".into());
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
fn session_cookie(value: String, max_age_days: i64) -> HeaderMap {
    let cookie = Cookie::build((SESSION_COOKIE, value))
        .path("/")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::days(max_age_days))
        .build();

    let mut headers = HeaderMap::new();
    headers.insert(header::SET_COOKIE, cookie.to_string().parse().unwrap());
    headers
}

/// Read the session cookie value out of a raw Cookie: header.
fn session_token_from_cookie_header(cookie_header: &str) -> Option<&str> {
    cookie_header.split(';').find_map(|piece| {
        let piece = piece.trim();
        piece
            .strip_prefix(SESSION_COOKIE)
            .and_then(|rest| rest.strip_prefix('='))
            .filter(|token| !token.is_empty())
    })
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

    let mut tx = pool.begin().await?;

    let account = sqlx::query(
        r#"
        INSERT INTO accounts (pb_user_id)
        VALUES (?)
        ON CONFLICT (pb_user_id) DO UPDATE SET updated_at = now()
        RETURNING id, status
        "#,
    )
    .bind(&pb_user_id)
    .fetch_one(&mut *tx)
    .await?;

    let account_id: Uuid = account.get::<Hyphenated, _>("id").into_uuid();
    let account_status: String = account.get("status");

    if account_status != "active" {
        return Err(AppError::Unauthenticated);
    }

    let wallet = sqlx::query(
        r#"
        INSERT INTO wallets (account_id, balance_idr)
        VALUES (?, 0)
        ON CONFLICT (account_id) DO NOTHING
        RETURNING balance_idr
        "#,
    )
    .bind(account_id.hyphenated())
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
    // Absolute lifetime from config. The idle bound needs a last-seen column the
    // schema does not have yet (docs/website/02-data-model.md, sessions).
    let expires_at = Utc::now() + Duration::days(sessions.absolute_days as i64);

    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    sqlx::query(
        "INSERT INTO sessions (account_id, token_hash, expires_at, user_agent) VALUES (?, ?, ?, ?)",
    )
    .bind(account_id.hyphenated())
    .bind(token_hash)
    .bind(expires_at)
    .bind(user_agent)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    let response_headers = session_cookie(session_token, sessions.absolute_days as i64);

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
            "UPDATE sessions SET revoked_at = now() WHERE token_hash = ? AND revoked_at IS NULL",
        )
        .bind(hash_token(token))
        .execute(&pool)
        .await?;
    }

    Ok((StatusCode::NO_CONTENT, session_cookie(String::new(), 0)))
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
            "SELECT account_id FROM sessions WHERE token_hash = ? AND revoked_at IS NULL AND expires_at > now()",
        )
        .bind(hash_token(token))
        .fetch_optional(&pool)
        .await?;

        if let Some(s) = session {
            let account_id: Uuid = s.get::<Hyphenated, _>("account_id").into_uuid();
            sqlx::query(
                "UPDATE sessions SET revoked_at = now() WHERE account_id = ? AND revoked_at IS NULL",
            )
            .bind(account_id.hyphenated())
            .execute(&pool)
            .await?;
        }
    }

    Ok((StatusCode::NO_CONTENT, session_cookie(String::new(), 0)))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(normalize_pb_user_id("  abc123  ").as_deref(), Some("abc123"));
        assert_eq!(normalize_pb_user_id(""), None);
        assert_eq!(normalize_pb_user_id("   "), None);
        assert_eq!(normalize_pb_user_id("has space"), None);
        assert_eq!(normalize_pb_user_id("has-dash"), None);
        assert_eq!(normalize_pb_user_id(&"a".repeat(65)), None);
    }

    #[test]
    fn extracts_session_token_from_cookie_header() {
        assert_eq!(
            session_token_from_cookie_header("a=1; session=apk_sess_deadbeef; b=2"),
            Some("apk_sess_deadbeef")
        );
        assert_eq!(
            session_token_from_cookie_header("session=apk_sess_x"),
            Some("apk_sess_x")
        );
        assert_eq!(session_token_from_cookie_header("other=1"), None);
        assert_eq!(session_token_from_cookie_header("session="), None);
        assert_eq!(session_token_from_cookie_header(""), None);
        // A cookie whose name merely ends in "session" is not ours.
        assert_eq!(session_token_from_cookie_header("notsession=1"), None);
    }

    #[test]
    fn session_cookie_carries_expected_attributes() {
        let headers = session_cookie("apk_sess_abc".into(), 30);
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
        let cleared = session_cookie(String::new(), 0);
        let value = cleared
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .expect("set-cookie present");
        assert!(value.contains("Max-Age=0"), "got {value}");
    }

    #[test]
    fn session_hash_is_sha256_hex_and_never_the_token() {
        // Only the hash reaches Sqlite; the cookie value never does.
        assert_eq!(
            hash_token("apk_sess_abc"),
            "c943c9214781fe698239bd2827dda2ed0fd7c0c746cd3ab44785da20083a1a9e"
        );
        assert_eq!(hash_token(""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_ne!(hash_token("apk_sess_abc"), hash_token("apk_sess_abd"));
    }
}
