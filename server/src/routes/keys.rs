use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Json},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use uuid::Uuid;
use rand_core::RngCore;

use crate::error::AppError;

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

    let response: Vec<ApiKeyDto> = keys
        .into_iter()
        .map(|k| ApiKeyDto {
            id: k.get("id"),
            prefix: k.get("prefix"),
            label: k.get("label"),
            models: k.get("models"),
            spend_limit_idr: k.get("spend_limit_idr"),
            spend_used_idr: 0,
            rate_limit_rpm: k.get("rate_limit_rpm"),
            expires_at: k.get("expires_at"),
            last_used_at: k.get("last_used_at"),
            revoked_at: k.get("revoked_at"),
        })
        .collect();

    Ok(Json(response))
}

pub async fn create_key(
    State(pool): State<PgPool>,
    headers: HeaderMap,
    Json(payload): Json<CreateKeyRequest>,
) -> Result<impl IntoResponse, AppError> {
    let account_id = resolve_account_from_cookie(&pool, &headers).await?;

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
    .fetch_one(&pool)
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
    State(pool): State<PgPool>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(payload): Json<UpdateKeyRequest>,
) -> Result<impl IntoResponse, AppError> {
    let account_id = resolve_account_from_cookie(&pool, &headers).await?;

    let models_json = payload.models.map(|m| serde_json::to_value(m).unwrap());

    let res = sqlx::query(
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
        RETURNING id
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
    .fetch_optional(&pool)
    .await?;

    if res.is_none() {
        return Err(AppError::NotFound("Key not found or revoked".into()));
    }

    Ok(StatusCode::OK)
}

pub async fn revoke_key(
    State(pool): State<PgPool>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let account_id = resolve_account_from_cookie(&pool, &headers).await?;

    sqlx::query("UPDATE api_keys SET revoked_at = now() WHERE id = $1 AND account_id = $2")
        .bind(id)
        .bind(account_id)
        .execute(&pool)
        .await?;

    Ok(StatusCode::NO_CONTENT)
}
