use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Json},
};
use chrono::{DateTime, NaiveDate, Utc};
use rand_core::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use std::collections::HashMap;
use uuid::Uuid;

use crate::error::AppError;
use crate::routes::proxy::{invalidate_key_cache, AppState};

#[derive(Debug, Serialize)]
pub struct ApiKeyDto {
    pub id: Uuid,
    pub prefix: String,
    pub label: Option<String>,
    pub models: Value,
    pub spend_limit_idr: i64,
    pub spend_used_idr: i64,
    pub rate_limit_rpm: i32,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
pub struct CreateKeyRequest {
    pub label: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub spend_limit_idr: i64,
    #[serde(default)]
    pub token_limit: i64,
    #[serde(default)]
    pub rate_limit_rpm: i32,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct CreateKeyResponse {
    pub id: Uuid,
    pub key: String,
    pub prefix: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateKeyRequest {
    pub label: Option<String>,
    pub models: Option<Vec<String>>,
    pub spend_limit_idr: Option<i64>,
    pub token_limit: Option<i64>,
    pub rate_limit_rpm: Option<i32>,
    pub expires_at: Option<DateTime<Utc>>,
}

fn hash_string(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    hex::encode(hasher.finalize())
}

/// Length of the rolling `spend_limit_idr` window, in days, inclusive of today.
///
/// docs/website/06-api-keys-and-limits.md fixes this at "the trailing 30 days"
/// (a rolling window, deliberately not a calendar month). No window length is
/// configured in config.rs, so there is nothing to read instead of this.
///
/// Public because the PROXY enforces this same window on the request path: one
/// constant, so the limit the proxy blocks on and the spend the dashboard shows
/// cannot drift apart.
pub const SPEND_WINDOW_DAYS: i64 = 30;

/// First day (inclusive) of the rolling spend window ending on `today`.
/// The window holds exactly 30 day-values, so it starts 29 days back.
pub(crate) fn spend_window_start(today: NaiveDate) -> NaiveDate {
    today - chrono::Duration::days(SPEND_WINDOW_DAYS - 1)
}

/// Whether a `usage_daily.day` row falls inside the rolling spend window.
fn in_spend_window(day: NaiveDate, today: NaiveDate) -> bool {
    day >= spend_window_start(today)
}

/// Folds per-day usage rows into one window total per API key.
/// Rows outside the window are dropped; keys with no rows are absent (read as 0).
///
/// The summed value is `usage_daily.cost_idr`, which the settlement path already
/// stores as the money.rs `calculate_token_cost_idr` result — no second formula.
fn fold_spend_in_window(rows: &[(Uuid, NaiveDate, i64)], today: NaiveDate) -> HashMap<Uuid, i64> {
    let mut spend: HashMap<Uuid, i64> = HashMap::new();
    for &(key_id, day, cost_idr) in rows {
        if !in_spend_window(day, today) {
            continue;
        }
        *spend.entry(key_id).or_insert(0) += cost_idr;
    }
    spend
}

/// 30-day spend already recorded against one key, in IDR.
///
/// Public because the proxy enforces the per-key limit with this exact read
/// (DEFECT 1): same table, same column, same window. A second formula on the
/// request path is how the dashboard number and the enforcement number start
/// disagreeing, and a key showing "limit reached" that still gets served is
/// precisely the defect this closes.
pub(crate) async fn key_spend_used(
    pool: &PgPool,
    account_id: Uuid,
    key_id: Uuid,
    today: NaiveDate,
) -> Result<i64, AppError> {
    let used: Option<i64> = sqlx::query_scalar(
        r#"
        SELECT SUM(cost_idr)::bigint
        FROM usage_daily
        WHERE account_id = $1 AND api_key_id = $2 AND day >= $3
        "#,
    )
    .bind(account_id)
    .bind(key_id)
    .bind(spend_window_start(today))
    .fetch_one(pool)
    .await?;

    Ok(used.unwrap_or(0))
}

/// 30-day token usage already recorded against one key, in tokens.
///
/// Public for the same reason as `key_spend_used`: the proxy enforces
/// `token_limit` with this exact read, over the same `usage_daily` rows and the
/// same `spend_window_start` boundary as the spend limit. Two limits, one
/// window, one source — so they cannot disagree with each other or with the
/// dashboard.
///
/// All three token classes are summed because `token_limit` counts tokens, not
/// money: cache-read tokens are ~50x cheaper than output tokens but they are
/// still tokens consumed, and `usage_daily` is the only place the request path
/// records them.
pub(crate) async fn key_tokens_used(
    pool: &PgPool,
    account_id: Uuid,
    key_id: Uuid,
    today: NaiveDate,
) -> Result<i64, AppError> {
    let used: Option<i64> = sqlx::query_scalar(
        r#"
        SELECT SUM(input_tokens + cache_read_tokens + output_tokens)::bigint
        FROM usage_daily
        WHERE account_id = $1 AND api_key_id = $2 AND day >= $3
        "#,
    )
    .bind(account_id)
    .bind(key_id)
    .bind(spend_window_start(today))
    .fetch_one(pool)
    .await?;

    Ok(used.unwrap_or(0))
}

/// Rejects a requested `spend_limit_idr` that cannot be enforced, reporting the
/// key's current window usage so the operator can see where it stands.
///
/// 0 means "no limit" (docs/website/06-api-keys-and-limits.md). A negative limit
/// has no meaning and would fail every request, so it is refused here rather than
/// stored and silently obeyed.
fn check_spend_limit(requested_idr: i64, spend_used_idr: i64) -> Result<(), AppError> {
    if requested_idr < 0 {
        return Err(AppError::KeyLimitExceeded {
            details: Some(json!({
                "reason": "invalid_spend_limit_idr",
                "spend_limit_idr": requested_idr,
                "spend_used_idr": spend_used_idr,
                "window_days": SPEND_WINDOW_DAYS,
            })),
        });
    }
    Ok(())
}

async fn resolve_account_from_cookie(pool: &PgPool, headers: &HeaderMap) -> Result<Uuid, AppError> {
    let cookie_hdr = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .ok_or(AppError::Unauthenticated)?;

    for piece in cookie_hdr.split(';') {
        let piece = piece.trim();
        if let Some(token) = piece.strip_prefix("session=") {
            let token_hash = hash_string(token);
            let session = sqlx::query(
                "SELECT account_id FROM sessions WHERE token_hash = $1 AND revoked_at IS NULL AND expires_at > now()",
            )
            .bind(token_hash)
            .fetch_optional(pool)
            .await?;

            if let Some(s) = session {
                return Ok(s.get("account_id"));
            }
        }
    }

    Err(AppError::Unauthenticated)
}

pub async fn list_keys(
    State(pool): State<PgPool>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let account_id = resolve_account_from_cookie(&pool, &headers).await?;

    let keys = sqlx::query(
        r#"
        SELECT
            id, prefix, label, models, spend_limit_idr, rate_limit_rpm,
            expires_at, last_used_at, revoked_at
        FROM api_keys
        WHERE account_id = $1
        ORDER BY created_at DESC
        "#,
    )
    .bind(account_id)
    .fetch_all(&pool)
    .await?;

    // Real 30-day spend per key. usage_daily.cost_idr is the settled cost of the
    // key's usage; a key with no rows in the window reads as 0.
    let today = Utc::now().date_naive();
    let usage_rows = sqlx::query(
        r#"
        SELECT api_key_id, day, cost_idr
        FROM usage_daily
        WHERE account_id = $1 AND api_key_id IS NOT NULL AND day >= $2
        "#,
    )
    .bind(account_id)
    .bind(spend_window_start(today))
    .fetch_all(&pool)
    .await?;

    let usage: Vec<(Uuid, NaiveDate, i64)> = usage_rows
        .into_iter()
        .map(|r| {
            let key_id: Uuid = r.get("api_key_id");
            let day: NaiveDate = r.get("day");
            let cost_idr: i64 = r.get("cost_idr");
            (key_id, day, cost_idr)
        })
        .collect();
    let spend_by_key = fold_spend_in_window(&usage, today);

    let response: Vec<ApiKeyDto> = keys
        .into_iter()
        .map(|k| {
            let id: Uuid = k.get("id");
            ApiKeyDto {
                id,
                prefix: k.get("prefix"),
                label: k.get("label"),
                models: k.get("models"),
                spend_limit_idr: k.get("spend_limit_idr"),
                spend_used_idr: spend_by_key.get(&id).copied().unwrap_or(0),
                rate_limit_rpm: k.get("rate_limit_rpm"),
                expires_at: k.get("expires_at"),
                last_used_at: k.get("last_used_at"),
                revoked_at: k.get("revoked_at"),
            }
        })
        .collect();

    Ok(Json(response))
}

pub async fn create_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CreateKeyRequest>,
) -> Result<impl IntoResponse, AppError> {
    let account_id = resolve_account_from_cookie(&state.pool, &headers).await?;

    // Abuse guard: `limits.key_creation_per_day` (docs/decisions.md) exists to
    // blunt limit circumvention by key-spam. Enforced from the `api_keys` rows,
    // so a revoked key still counts - revoking one to mint another is exactly
    // the circumvention the cap is for. The cap comes from the config the app
    // already owns.
    crate::abuse::enforce_creation_cap(
        &state.pool,
        "api_keys",
        crate::abuse::key_creation_window(),
        state.config.limits.key_creation_per_day,
        account_id,
        Utc::now(),
    )
    .await?;

    let random_bytes: String = (0..43)
        .map(|_| {
            let idx = (rand_core::OsRng.next_u32() % 62) as usize;
            b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ"[idx] as char
        })
        .collect();

    let full_key = format!("apk_live_{}", random_bytes);
    let prefix = format!("apk_live_{}", &random_bytes[..4]);
    let key_hash = hash_string(&full_key);

    let models_json = serde_json::to_value(&payload.models).unwrap_or(json!([]));

    // A key starts with no usage, so its window spend is 0; validate the
    // requested limit against that before storing it.
    check_spend_limit(payload.spend_limit_idr, 0)?;

    let key_record = sqlx::query(
        r#"
        INSERT INTO api_keys (
            account_id, key_hash, prefix, label, models,
            spend_limit_idr, token_limit, rate_limit_rpm, expires_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        RETURNING id
        "#,
    )
    .bind(account_id)
    .bind(key_hash)
    .bind(&prefix)
    .bind(payload.label)
    .bind(models_json)
    .bind(payload.spend_limit_idr)
    .bind(payload.token_limit)
    .bind(payload.rate_limit_rpm)
    .bind(payload.expires_at)
    .fetch_one(&state.pool)
    .await?;

    let id: Uuid = key_record.get("id");

    Ok((
        StatusCode::CREATED,
        Json(CreateKeyResponse {
            id,
            key: full_key,
            prefix,
        }),
    ))
}

pub async fn update_key(
    // The whole state, not just the pool: the invalidation below reaches the
    // proxy's process-wide cache, which is built from `config`.
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(payload): Json<UpdateKeyRequest>,
) -> Result<impl IntoResponse, AppError> {
    let account_id = resolve_account_from_cookie(&state.pool, &headers).await?;

    // Lowering a limit below what the key has already spent in the window leaves
    // it immediately over limit. Validate against the real spend so the operator
    // sees the number rather than a silently ineffective limit.
    if let Some(requested) = payload.spend_limit_idr {
        let spend_used_idr =
            key_spend_used(&state.pool, account_id, id, Utc::now().date_naive()).await?;
        check_spend_limit(requested, spend_used_idr)?;
    }

    let models_json = payload.models.map(|m| serde_json::to_value(m).unwrap());

    let res: Option<String> = sqlx::query_scalar(
        r#"
        UPDATE api_keys
        SET
            label = COALESCE($1, label),
            models = COALESCE($2, models),
            spend_limit_idr = COALESCE($3, spend_limit_idr),
            token_limit = COALESCE($4, token_limit),
            rate_limit_rpm = COALESCE($5, rate_limit_rpm),
            expires_at = COALESCE($6, expires_at)
        WHERE id = $7 AND account_id = $8 AND revoked_at IS NULL
        RETURNING key_hash
        "#,
    )
    .bind(payload.label)
    .bind(models_json)
    .bind(payload.spend_limit_idr)
    .bind(payload.token_limit)
    .bind(payload.rate_limit_rpm)
    .bind(payload.expires_at)
    .bind(id)
    .bind(account_id)
    .fetch_optional(&state.pool)
    .await?;

    // A narrowed limit, allowlist or expiry is an enforcement input too: the
    // cached record still holds the old values, so without this the proxy would
    // keep honouring the previous (looser) limit for up to the cache TTL.
    // Dropped after the update commits, and only in THIS process.
    let Some(key_hash) = res else {
        return Err(AppError::NotFound("Key not found or revoked".into()));
    };
    invalidate_key_cache(&state.config, &key_hash);

    Ok(StatusCode::OK)
}

pub async fn revoke_key(
    // The whole state, not just the pool: the proxy's key-metadata cache is a
    // process-wide singleton built from `config`, so reaching the same instance
    // the request path uses needs the config it was built from.
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let account_id = resolve_account_from_cookie(&state.pool, &headers).await?;

    // RETURNING the hash is what makes the invalidation below possible: the
    // proxy cache is keyed by the SHA-256 of the presented token, and the
    // plaintext key exists nowhere in the database (it was shown once, at
    // creation). The hash is also all `invalidate_key_cache` will ever take, so
    // the plaintext never has to be reconstructed to evict an entry.
    let revoked_hash: Option<String> = sqlx::query_scalar(
        r#"
        UPDATE api_keys SET revoked_at = now()
        WHERE id = $1 AND account_id = $2 AND revoked_at IS NULL
        RETURNING key_hash
        "#,
    )
    .bind(id)
    .bind(account_id)
    .fetch_optional(&state.pool)
    .await?;

    // The update committed, so the revocation is durable; dropping the cached
    // record is what makes it take effect NOW instead of at the TTL boundary
    // (DEFECT 2: a revoked key used to stay usable for up to
    // `limits.key_metadata_cache_seconds`). A second revoke matches no row and
    // returns None — there is nothing left to invalidate, and the response is
    // the same 204 either way, so the call stays idempotent.
    //
    // Only THIS process's cache is dropped. Instances behind the load balancer
    // keep their own copy until its TTL expires; that residual window is the
    // documented cost of the cache and is not closed by this call.
    if let Some(key_hash) = revoked_hash {
        invalidate_key_cache(&state.config, &key_hash);
    }

    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn test_spend_window_boundary() {
        let today = day(2026, 5, 20);
        let start = spend_window_start(today);
        assert_eq!(start, day(2026, 4, 21));
        assert_eq!((today - start).num_days() + 1, SPEND_WINDOW_DAYS);
        assert!(in_spend_window(start, today));
        assert!(in_spend_window(today, today));
        assert!(!in_spend_window(start - chrono::Duration::days(1), today));
        assert!(!in_spend_window(day(2026, 4, 20), today));
        assert_eq!(spend_window_start(day(2026, 3, 1)), day(2026, 1, 31));
    }

    #[test]
    fn test_fold_spend_in_window() {
        let today = day(2026, 5, 20);
        let start = spend_window_start(today);
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let rows = vec![
            (a, today, 700),
            (a, start, 300),
            (a, start - chrono::Duration::days(1), 9_999),
            (b, start - chrono::Duration::days(1), 500),
        ];
        let spend = fold_spend_in_window(&rows, today);
        assert_eq!(spend.get(&a).copied(), Some(1_000));
        assert_eq!(spend.get(&b).copied(), None);
        assert_eq!(spend.len(), 1);
        assert_eq!(spend.values().sum::<i64>(), 1_000);
    }

    #[test]
    fn test_check_spend_limit() {
        assert!(check_spend_limit(0, 0).is_ok());
        assert!(check_spend_limit(50_000, 12_340).is_ok());
        let err = check_spend_limit(-1, 12_340).unwrap_err();
        assert!(matches!(&err, AppError::KeyLimitExceeded { .. }));
        assert_eq!(err.code(), "key_limit_exceeded");
        assert_eq!(err.status_code(), StatusCode::PAYMENT_REQUIRED);
        let details = err.details().unwrap();
        assert_eq!(details["spend_limit_idr"], -1);
        assert_eq!(details["spend_used_idr"], 12_340);
        assert_eq!(details["window_days"], 30);
    }
}
