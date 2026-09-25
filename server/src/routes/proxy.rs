use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, Response, StatusCode},
    Json,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use std::sync::{Arc, OnceLock};
use tracing::info;
use uuid::Uuid;

use crate::config::AppConfig;
use crate::error::AppError;
use crate::money::calculate_preflight_reservation_idr;
use crate::upstream::{UpstreamClient, UpstreamError};

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

#[derive(Debug, Deserialize, Serialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
}

/// The upstream client, built exactly once per process: it owns the connection
/// pool, the per-endpoint key pools and the circuit breakers, all of which are
/// worthless unless they are shared across requests.
static UPSTREAM: OnceLock<UpstreamClient> = OnceLock::new();

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

    // The upstream client is process-wide state: the pool, the key pools and
    // the breakers only do their job when every request shares them.
    let upstream = UPSTREAM.get_or_init(|| UpstreamClient::new(state.config.clone()));

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

    // 4. Send to the upstream. `payload.model` is passed for routing only:
    // the client rewrites the body's model to the endpoint's upstream_model
    // and forces stream=true.
    let body = serde_json::to_value(&payload).map_err(|e| AppError::Internal(e.to_string()))?;

    let mut stream = upstream
        .stream_chat(&payload.model, body)
        .await
        .map_err(|err| match err {
            UpstreamError::NoModel(_) => AppError::ModelNotAllowed(payload.model.clone()),
            UpstreamError::NoHealthyUpstream(_) => AppError::NoUpstreamAvailable,
            _ => AppError::Internal(err.to_string()),
        })?;

    let endpoint = stream.endpoint_name().to_string();

    // Nothing is buffered whole: the upstream body is polled only as the
    // client reads it, and the key lease stays held until the last chunk.
    // `poll_fn` takes ownership of the stream, so the body is 'static as
    // `Body::from_stream` requires; a `&mut` borrow would not be.
    let chunks = futures_util::stream::poll_fn(move |cx| {
        stream.bytes().poll_next_unpin(cx).map(|item| {
            item.map(|chunk| {
                chunk.map_err(|err| {
                    std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("upstream stream failed: {err}"),
                    )
                })
            })
        })
    });

    info!(
        account_id = %account_id,
        endpoint = %endpoint,
        "Proxy streaming from upstream"
    );

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("X-Accel-Buffering", "no")
        .body(Body::from_stream(chunks))
        .map_err(|e| AppError::Internal(e.to_string()))
}
