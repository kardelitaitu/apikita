use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Json},
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::AppError;

#[derive(Debug, Serialize)]
pub struct MeResponse {
    pub account_id: Uuid,
    pub balance_idr: i64,
    pub usage_today: UsageTodayDto,
    pub telegram_linked: bool,
    pub status: String,
}

#[derive(Debug, Serialize)]
pub struct UsageTodayDto {
    pub input_tokens: i64,
    pub cache_read_tokens: i64,
    pub output_tokens: i64,
    pub cost_idr: i64,
}

#[derive(Debug, Deserialize)]
pub struct CreateTopupRequest {
    pub amount_idr: i64,
}

#[derive(Debug, Serialize)]
pub struct CreateTopupResponse {
    pub topup_id: Uuid,
    pub order_id: String,
    pub snap_token: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LimitQuery {
    pub limit: Option<i64>,
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

pub async fn get_me(
    State(pool): State<PgPool>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let account_id = resolve_account_from_cookie(&pool, &headers).await?;

    let account = sqlx::query("SELECT status FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_one(&pool)
        .await?;

    let status: String = account.get("status");

    let wallet = sqlx::query("SELECT balance_idr FROM wallets WHERE account_id = $1")
        .bind(account_id)
        .fetch_optional(&pool)
        .await?;

    let balance_idr: i64 = wallet.map(|w| w.get("balance_idr")).unwrap_or(0);

    let today = Utc::now().date_naive();
    let usage_today = sqlx::query(
        r#"
        SELECT
            COALESCE(SUM(input_tokens), 0) AS input_tokens,
            COALESCE(SUM(cache_read_tokens), 0) AS cache_read_tokens,
            COALESCE(SUM(output_tokens), 0) AS output_tokens,
            COALESCE(SUM(cost_idr), 0) AS cost_idr
        FROM usage_daily
        WHERE account_id = $1 AND day = $2
        "#,
    )
    .bind(account_id)
    .bind(today)
    .fetch_one(&pool)
    .await?;

    let tg_link = sqlx::query("SELECT telegram_id FROM telegram_links WHERE account_id = $1")
        .bind(account_id)
        .fetch_optional(&pool)
        .await?;

    Ok(Json(MeResponse {
        account_id,
        balance_idr,
        usage_today: UsageTodayDto {
            input_tokens: usage_today.get("input_tokens"),
            cache_read_tokens: usage_today.get("cache_read_tokens"),
            output_tokens: usage_today.get("output_tokens"),
            cost_idr: usage_today.get("cost_idr"),
        },
        telegram_linked: tg_link.is_some(),
        status,
    }))
}

pub async fn get_usage(
    State(pool): State<PgPool>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let account_id = resolve_account_from_cookie(&pool, &headers).await?;

    let rows = sqlx::query(
        r#"
        SELECT
            day,
            SUM(input_tokens) AS input_tokens,
            SUM(cache_read_tokens) AS cache_read_tokens,
            SUM(output_tokens) AS output_tokens,
            SUM(cost_idr) AS cost_idr
        FROM usage_daily
        WHERE account_id = $1
        GROUP BY day
        ORDER BY day DESC
        LIMIT 30
        "#,
    )
    .bind(account_id)
    .fetch_all(&pool)
    .await?;

    let result: Vec<_> = rows
        .into_iter()
        .map(|r| {
            let day: chrono::NaiveDate = r.get("day");
            let in_tok: Option<i64> = r.get("input_tokens");
            let cache_tok: Option<i64> = r.get("cache_read_tokens");
            let out_tok: Option<i64> = r.get("output_tokens");
            let cost: Option<i64> = r.get("cost_idr");
            json!({
                "day": day,
                "input_tokens": in_tok.unwrap_or(0),
                "cache_read_tokens": cache_tok.unwrap_or(0),
                "output_tokens": out_tok.unwrap_or(0),
                "cost_idr": cost.unwrap_or(0),
            })
        })
        .collect();

    Ok(Json(result))
}

pub async fn get_topups(
    State(pool): State<PgPool>,
    headers: HeaderMap,
    Query(query): Query<LimitQuery>,
) -> Result<impl IntoResponse, AppError> {
    let account_id = resolve_account_from_cookie(&pool, &headers).await?;
    let limit = query.limit.unwrap_or(20).clamp(1, 100);

    let rows = sqlx::query(
        r#"
        SELECT id, amount_idr, order_id, status, created_at, settled_at
        FROM topups
        WHERE account_id = $1
        ORDER BY created_at DESC
        LIMIT $2
        "#,
    )
    .bind(account_id)
    .bind(limit)
    .fetch_all(&pool)
    .await?;

    let result: Vec<_> = rows
        .into_iter()
        .map(|r| {
            let id: Uuid = r.get("id");
            let amount_idr: i64 = r.get("amount_idr");
            let order_id: String = r.get("order_id");
            let status: String = r.get("status");
            let created_at: chrono::DateTime<Utc> = r.get("created_at");
            let settled_at: Option<chrono::DateTime<Utc>> = r.get("settled_at");
            json!({
                "id": id,
                "amount_idr": amount_idr,
                "order_id": order_id,
                "status": status,
                "created_at": created_at,
                "settled_at": settled_at,
            })
        })
        .collect();

    Ok(Json(result))
}

pub async fn create_topup(
    State(pool): State<PgPool>,
    headers: HeaderMap,
    Json(payload): Json<CreateTopupRequest>,
) -> Result<impl IntoResponse, AppError> {
    let account_id = resolve_account_from_cookie(&pool, &headers).await?;

    let past_settled = sqlx::query("SELECT count(*) AS count FROM topups WHERE account_id = $1 AND status = 'settled'")
        .bind(account_id)
        .fetch_one(&pool)
        .await?;

    let count: i64 = past_settled.get("count");
    let min_required = if count == 0 { 50_000 } else { 10_000 };

    if payload.amount_idr < min_required {
        return Err(AppError::InvalidRequest(format!(
            "Amount must be at least {} IDR",
            min_required
        )));
    }

    let topup_id = Uuid::new_v4();
    let order_id = format!("topup_{}", topup_id);
    let snap_token = Some(format!("mock_snap_{}", Uuid::new_v4()));

    sqlx::query(
        "INSERT INTO topups (id, account_id, amount_idr, order_id, status, snap_token) VALUES ($1, $2, $3, $4, 'pending', $5)",
    )
    .bind(topup_id)
    .bind(account_id)
    .bind(payload.amount_idr)
    .bind(&order_id)
    .bind(&snap_token)
    .execute(&pool)
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(CreateTopupResponse {
            topup_id,
            order_id,
            snap_token,
        }),
    ))
}
