use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Json},
};
use axum_extra::extract::cookie::{Cookie, SameSite};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::AppError;

#[derive(Debug, Deserialize)]
pub struct AuthExchangeRequest {
    pub pb_token: String,
}

#[derive(Debug, Serialize)]
pub struct AuthExchangeResponse {
    pub account_id: Uuid,
    pub balance_idr: i64,
}

fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

pub async fn exchange_token(
    State(pool): State<PgPool>,
    headers: HeaderMap,
    Json(payload): Json<AuthExchangeRequest>,
) -> Result<impl IntoResponse, AppError> {
    if payload.pb_token.trim().is_empty() {
        return Err(AppError::InvalidRequest("pb_token is required".into()));
    }

    let pb_user_id = format!("pb_{}", &hash_token(&payload.pb_token)[..16]);

    let mut tx = pool.begin().await?;

    let account = sqlx::query(
        r#"
        INSERT INTO accounts (pb_user_id)
        VALUES ($1)
        ON CONFLICT (pb_user_id) DO UPDATE SET updated_at = now()
        RETURNING id, status
        "#,
    )
    .bind(&pb_user_id)
    .fetch_one(&mut *tx)
    .await?;

    let account_id: Uuid = account.get("id");
    let account_status: String = account.get("status");

    if account_status != "active" {
        return Err(AppError::Unauthenticated);
    }

    let wallet = sqlx::query(
        r#"
        INSERT INTO wallets (account_id, balance_idr)
        VALUES ($1, 0)
        ON CONFLICT (account_id) DO NOTHING
        RETURNING balance_idr
        "#,
    )
    .bind(account_id)
    .fetch_optional(&mut *tx)
    .await?;

    let balance_idr = match wallet {
        Some(w) => w.get("balance_idr"),
        None => {
            let existing = sqlx::query("SELECT balance_idr FROM wallets WHERE account_id = $1")
                .bind(account_id)
                .fetch_one(&mut *tx)
                .await?;
            existing.get("balance_idr")
        }
    };

    let session_token = format!("apk_sess_{}", Uuid::new_v4().simple());
    let token_hash = hash_token(&session_token);
    let expires_at = Utc::now() + Duration::days(30);

    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    sqlx::query(
        "INSERT INTO sessions (account_id, token_hash, expires_at, user_agent) VALUES ($1, $2, $3, $4)",
    )
    .bind(account_id)
    .bind(token_hash)
    .bind(expires_at)
    .bind(user_agent)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    let cookie = Cookie::build(("session", session_token))
        .path("/")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::days(30))
        .build();

    let mut response_headers = HeaderMap::new();
    response_headers.insert(header::SET_COOKIE, cookie.to_string().parse().unwrap());

    Ok((
        StatusCode::OK,
        response_headers,
        Json(AuthExchangeResponse {
            account_id,
            balance_idr,
        }),
    ))
}

pub async fn logout(
    State(pool): State<PgPool>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    if let Some(cookie_hdr) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) {
        for piece in cookie_hdr.split(';') {
            let piece = piece.trim();
            if let Some(token) = piece.strip_prefix("session=") {
                let token_hash = hash_token(token);
                let _ = sqlx::query("UPDATE sessions SET revoked_at = now() WHERE token_hash = $1")
                    .bind(token_hash)
                    .execute(&pool)
                    .await;
            }
        }
    }

    let cookie = Cookie::build(("session", ""))
        .path("/")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::seconds(0))
        .build();

    let mut response_headers = HeaderMap::new();
    response_headers.insert(header::SET_COOKIE, cookie.to_string().parse().unwrap());

    Ok((StatusCode::NO_CONTENT, response_headers))
}

pub async fn logout_all(
    State(pool): State<PgPool>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    if let Some(cookie_hdr) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) {
        for piece in cookie_hdr.split(';') {
            let piece = piece.trim();
            if let Some(token) = piece.strip_prefix("session=") {
                let token_hash = hash_token(token);
                let session = sqlx::query("SELECT account_id FROM sessions WHERE token_hash = $1 AND revoked_at IS NULL")
                    .bind(token_hash)
                    .fetch_optional(&pool)
                    .await?;

                if let Some(s) = session {
                    let account_id: Uuid = s.get("account_id");
                    let _ = sqlx::query(
                        "UPDATE sessions SET revoked_at = now() WHERE account_id = $1 AND revoked_at IS NULL",
                    )
                    .bind(account_id)
                    .execute(&pool)
                    .await;
                }
            }
        }
    }

    let cookie = Cookie::build(("session", ""))
        .path("/")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::seconds(0))
        .build();

    let mut response_headers = HeaderMap::new();
    response_headers.insert(header::SET_COOKIE, cookie.to_string().parse().unwrap());

    Ok((StatusCode::NO_CONTENT, response_headers))
}
