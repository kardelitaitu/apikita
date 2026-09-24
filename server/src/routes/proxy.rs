use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, Response, StatusCode},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use std::sync::Arc;
use tracing::info;
use uuid::Uuid;

use crate::config::AppConfig;
use crate::db::debit_usage_transaction;
use crate::error::AppError;
use crate::money::{calculate_preflight_reservation_idr, calculate_token_cost_idr};

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<AppConfig>,
    pub http_client: reqwest::Client,
}

impl axum::extract::FromRef<AppState> for PgPool {
    fn from_ref(state: &AppState) -> Self {
        state.pool.clone()
    }
}

#[derive(Debug, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<Value>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub stream: Option<bool>,
}

fn hash_string(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    hex::encode(hasher.finalize())
}

pub async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ChatCompletionRequest>,
) -> Result<Response<Body>, AppError> {
    // 1. Authenticate via Bearer token
    let auth_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(AppError::Unauthenticated)?;

    let presented_key = auth_header
        .strip_prefix("Bearer ")
        .ok_or(AppError::Unauthenticated)?
        .trim();

    let key_hash = hash_string(presented_key);

    let key_record = sqlx::query(
        r#"
        SELECT
            id, account_id, models, spend_limit_idr,
            expires_at, revoked_at
        FROM api_keys
        WHERE key_hash = $1
        "#,
    )
    .bind(key_hash)
    .fetch_optional(&state.pool)
    .await?;

    let key = match key_record {
        Some(k) => k,
        None => return Err(AppError::Unauthenticated),
    };

    let key_id: Uuid = key.get("id");
    let account_id: Uuid = key.get("account_id");
    let models_val: Value = key.get("models");
    let expires_at: Option<chrono::DateTime<chrono::Utc>> = key.get("expires_at");
    let revoked_at: Option<chrono::DateTime<chrono::Utc>> = key.get("revoked_at");

    if revoked_at.is_some() {
        return Err(AppError::KeyRevoked);
    }

    if let Some(exp) = expires_at {
        if exp < chrono::Utc::now() {
            return Err(AppError::KeyExpired);
        }
    }

    // 2. Authorize model
    let allowed_models: Vec<String> = serde_json::from_value(models_val).unwrap_or_default();
    if !allowed_models.is_empty() && !allowed_models.contains(&payload.model) {
        return Err(AppError::ModelNotAllowed(payload.model.clone()));
    }

    let model_cfg = state
        .config
        .models
        .iter()
        .find(|m| m.name == payload.model)
        .ok_or_else(|| AppError::ModelNotAllowed(payload.model.clone()))?;

    // 3. Pre-flight wallet balance check
    let wallet = sqlx::query("SELECT balance_idr FROM wallets WHERE account_id = $1")
        .bind(account_id)
        .fetch_optional(&state.pool)
        .await?;

    let current_balance: i64 = wallet.map(|w| w.get("balance_idr")).unwrap_or(0);

    if current_balance <= 0 {
        return Err(AppError::InsufficientBalance {
            details: Some(json!({ "balance_idr": current_balance, "required_idr": 1 })),
        });
    }

    let max_output = payload
        .max_tokens
        .unwrap_or(state.config.streaming.default_max_output_tokens)
        .min(state.config.streaming.hard_max_output_tokens);

    let reservation = calculate_preflight_reservation_idr(
        model_cfg.price,
        500,
        model_cfg.rates.input_peak,
        max_output,
        model_cfg.rates.output_peak,
    );

    if reservation > current_balance && !state.config.streaming.allow_negative_balance_overdraft {
        return Err(AppError::InsufficientBalance {
            details: Some(json!({
                "balance_idr": current_balance,
                "required_idr": reservation
            })),
        });
    }

    let actual_input = 200;
    let actual_cache = 0;
    let actual_output = 150;

    let cost_idr = calculate_token_cost_idr(
        model_cfg.price,
        actual_input,
        model_cfg.rates.input_peak,
        actual_cache,
        model_cfg.rates.cache_read_peak,
        actual_output,
        model_cfg.rates.output_peak,
    );

    let _new_balance = debit_usage_transaction(
        &state.pool,
        account_id,
        Some(key_id),
        actual_input as i64,
        actual_cache as i64,
        actual_output as i64,
        cost_idr,
        Some("proxy_chat"),
    )
    .await?;

    info!(
        account_id = %account_id,
        cost_idr,
        "Proxy request settled successfully"
    );

    let mock_response = json!({
        "id": format!("chatcmpl-{}", uuid::Uuid::new_v4()),
        "object": "chat.completion",
        "created": chrono::Utc::now().timestamp(),
        "model": payload.model,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "ApiKita Proxy: Connection established and usage settled."
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": actual_input,
            "completion_tokens": actual_output,
            "total_tokens": actual_input + actual_output
        }
    });

    let body = serde_json::to_string(&mock_response).unwrap();
    let res = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .map_err(|e| AppError::Internal(e.to_string()))?;

    Ok(res)
}
