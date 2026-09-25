use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{header, HeaderMap, Response, StatusCode},
};
use futures_util::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::AppConfig;
use crate::db::debit_usage_transaction;
use crate::error::AppError;
use crate::money::{calculate_preflight_reservation_idr, calculate_token_cost_idr};
use crate::upstream::{parse_usage_from_sse, UpstreamClient, UpstreamError, UpstreamStream, Usage};

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

/// The only fields the enforcement order needs before forwarding. The body is
/// forwarded verbatim, so this is a lens over it rather than the payload: a
/// parameter this struct does not know about — `temperature`, `tools`, `stop`,
/// `top_p`, ... — still reaches the upstream untouched.
#[derive(Debug, Deserialize)]
struct RequestMeta {
    model: String,
    #[serde(default)]
    max_tokens: Option<u64>,
}

/// How many trailing bytes are retained to parse the usage block.
///
/// Only the tail is kept, never the body: usage arrives in the final SSE
/// chunks, while every chunk is forwarded the moment it arrives. 64 KiB is far
/// more than one usage chunk needs and stays a fixed, negligible allocation.
const USAGE_TAIL_CAP: usize = 64 * 1024;

/// The upstream client, built exactly once per process: it owns the connection
/// pool, the per-endpoint key pools and the circuit breakers, all of which are
/// worthless unless they are shared across requests.
static UPSTREAM: OnceLock<UpstreamClient> = OnceLock::new();

fn hash_string(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    hex::encode(hasher.finalize())
}

/// What the tee saw once the upstream stream ended.
enum StreamEnd {
    /// The stream completed and the tail parsed into a usage report.
    Settled(Usage),
    /// The stream completed but reported no usage — truncated, or killed
    /// mid-way. Nothing may be billed from this.
    NoUsage,
}

/// Forwards every upstream chunk the moment it arrives while retaining only the
/// last `USAGE_TAIL_CAP` bytes, so the trailing usage block can be parsed on
/// completion.
///
/// It owns the upstream stream rather than borrowing it, so it is `'static` and
/// can be handed straight to `Body::from_stream`. Dropping it — a client that
/// hangs up mid-answer — drops the upstream stream, which releases its key lease
/// and leaves the settlement receiver empty-handed: nothing is billed for an
/// answer the customer never finished receiving.
struct MeteredStream {
    inner: Option<UpstreamStream>,
    tail: Vec<u8>,
    /// Handed the outcome exactly once, when the stream ends.
    settle: Option<tokio::sync::oneshot::Sender<StreamEnd>>,
    done: bool,
}

impl MeteredStream {
    fn new(inner: UpstreamStream, settle: tokio::sync::oneshot::Sender<StreamEnd>) -> Self {
        Self {
            inner: Some(inner),
            tail: Vec::new(),
            settle: Some(settle),
            done: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.tail.extend_from_slice(chunk);
        if self.tail.len() > USAGE_TAIL_CAP {
            let excess = self.tail.len() - USAGE_TAIL_CAP;
            self.tail.drain(..excess);
        }
    }

    /// The upstream ended. `upstream_ok` is false when the end was a transport
    /// error: a stream that died mid-way is not a success and must not be
    /// reported as one to the key pool.
    ///
    /// Returns whether a usage block was seen — the only evidence that the
    /// answer actually completed rather than being cut off.
    fn finish(&mut self, upstream_ok: bool) -> bool {
        self.done = true;

        let usage = parse_usage_from_sse(&self.tail);

        if let Some(inner) = self.inner.take() {
            if upstream_ok && usage.is_some() {
                inner.finish_ok();
            } else {
                // 0 is a transport-level cut, never a configured rate-limit
                // status: the key is freed without a cooldown.
                inner.finish_status(0);
            }
        }

        if let Some(settle) = self.settle.take() {
            let end = match usage {
                Some(usage) => StreamEnd::Settled(usage),
                None => StreamEnd::NoUsage,
            };
            let _ = settle.send(end);
        }

        usage.is_some()
    }
}

impl Stream for MeteredStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }

        // Bound the borrow of `inner` before touching the rest of `self`.
        let polled = match this.inner.as_mut() {
            Some(inner) => inner.bytes().poll_next_unpin(cx),
            None => Poll::Ready(None),
        };

        match polled {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => {
                if this.finish(true) {
                    Poll::Ready(None)
                } else {
                    // The upstream closed without ever reporting usage: the
                    // answer was truncated. Saying so is the only honest
                    // outcome — a client cannot tell a partial answer from a
                    // complete one (docs/error-model.md).
                    Poll::Ready(Some(Ok(Bytes::from(error_event(
                        "upstream_incomplete",
                        "The upstream stream ended before the answer completed.",
                    )))))
                }
            }
            Poll::Ready(Some(Ok(chunk))) => {
                this.push(&chunk);
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(err))) => {
                this.finish(false);
                // The status line is long gone, so a mid-stream failure can
                // only be announced in-band (docs/error-model.md). Never
                // retried: a retry would append a second answer and bill for
                // both (docs/failover.md:133). The detail is logged, not sent:
                // a reqwest error carries the provider URL, and the client must
                // never see provider names (docs/error-model.md:159).
                warn!(error = %err, "Upstream stream failed mid-answer");
                Poll::Ready(Some(Ok(Bytes::from(error_event(
                    "upstream_failed",
                    "The upstream stream failed before the answer completed.",
                )))))
            }
        }
    }
}

/// A terminal SSE error event, then the stream closes cleanly.
fn error_event(code: &str, message: &str) -> String {
    let payload = json!({
        "error": {
            "code": code,
            "message": message,
            "request_id": format!("req_{}", Uuid::new_v4().simple()),
        }
    });
    format!("event: error\ndata: {payload}\n\n")
}

/// The inbound body with `"stream": true` guaranteed and nothing else changed.
fn ensure_streaming(body: &[u8]) -> Result<Value, AppError> {
    let mut value: Value = serde_json::from_slice(body)
        .map_err(|err| AppError::InvalidRequest(format!("malformed request body: {err}")))?;
    value
        .as_object_mut()
        .ok_or_else(|| AppError::InvalidRequest("request body must be a JSON object".into()))?
        .insert("stream".to_string(), Value::Bool(true));
    Ok(value)
}

pub async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
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

    // Read only what the checks below need; the body itself goes upstream as-is.
    let meta: RequestMeta = serde_json::from_slice(&body)
        .map_err(|err| AppError::InvalidRequest(format!("malformed request body: {err}")))?;

    // 2. Authorize model
    let allowed_models: Vec<String> = serde_json::from_value(models_val).unwrap_or_default();
    if !allowed_models.is_empty() && !allowed_models.contains(&meta.model) {
        return Err(AppError::ModelNotAllowed(meta.model.clone()));
    }

    let model_cfg = state
        .config
        .models
        .iter()
        .find(|m| m.name == meta.model)
        .ok_or_else(|| AppError::ModelNotAllowed(meta.model.clone()))?;

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

    // 4. Pre-flight worst-case reservation, taken BEFORE routing.
    //
    // Input is estimated from the raw body length (~4 bytes per token, floored
    // at 1); the output ceiling is the request's own cap, clamped to the hard
    // limit. The reservation must cover the dearest endpoint in the pool, not
    // the one that happens to serve the request — otherwise a failover to a
    // dearer provider can overdraw the balance (docs/failover.md:162-165).
    let estimated_input = (body.len() as u64 / 4).max(1);
    let max_output = meta
        .max_tokens
        .unwrap_or(state.config.streaming.default_max_output_tokens)
        .min(state.config.streaming.hard_max_output_tokens);

    // One reservation per endpoint, and the dearest wins. The rates live on
    // the model today, so the endpoints tie and this is a no-op - but it is
    // the rule that must hold once per-endpoint rates exist. The max(1) keeps
    // a model with no endpoints registered reserving at the model rate,
    // exactly as UpstreamClient::worst_case_reservation_idr does.
    let reservation = (0..model_cfg.endpoints.len().max(1))
        .map(|_| {
            calculate_preflight_reservation_idr(
                model_cfg.price,
                estimated_input,
                model_cfg.rates.input_peak,
                max_output,
                model_cfg.rates.output_peak,
            )
        })
        .max()
        .unwrap_or(0);

    if reservation > current_balance && !state.config.streaming.allow_negative_balance_overdraft {
        return Err(AppError::InsufficientBalance {
            details: Some(json!({
                "balance_idr": current_balance,
                "required_idr": reservation
            })),
        });
    }

    // 5. Route and stream. The raw inbound body goes up with `stream: true`
    // ensured; the client rewrites `model` to the endpoint's upstream_model.
    let upstream_body = ensure_streaming(&body)?;

    let stream = upstream
        .stream_chat(&meta.model, upstream_body)
        .await
        .map_err(|err| match err {
            UpstreamError::NoModel(_) => AppError::ModelNotAllowed(meta.model.clone()),
            UpstreamError::NoHealthyUpstream(_) => AppError::NoUpstreamAvailable,
            _ => AppError::Internal(err.to_string()),
        })?;

    let endpoint = stream.endpoint_name().to_string();

    info!(
        account_id = %account_id,
        endpoint = %endpoint,
        reservation,
        "Proxy streaming from upstream"
    );

    // 6. Settlement runs detached: the client is served first, and a billing
    // failure must not turn a completed answer into an error the customer never
    // saw. The tee reports back through this channel when the stream ends.
    let (settle_tx, settle_rx) = tokio::sync::oneshot::channel::<StreamEnd>();

    tokio::spawn(settle_after_stream(
        settle_rx,
        state.pool.clone(),
        state.config.clone(),
        account_id,
        key_id,
        meta.model.clone(),
    ));

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("X-Accel-Buffering", "no")
        .body(Body::from_stream(MeteredStream::new(stream, settle_tx)))
        .map_err(|e| AppError::Internal(e.to_string()))
}

/// Bills the request once the upstream stream has ended.
async fn settle_after_stream(
    end: tokio::sync::oneshot::Receiver<StreamEnd>,
    pool: PgPool,
    config: Arc<AppConfig>,
    account_id: Uuid,
    key_id: Uuid,
    model: String,
) {
    let end = match end.await {
        Ok(end) => end,
        // The response was dropped before the stream ended: the client hung up.
        // The tee dropped the upstream stream with it; nothing to settle.
        Err(_) => return,
    };

    let usage = match end {
        StreamEnd::Settled(usage) => usage,
        StreamEnd::NoUsage => {
            // docs/failover.md: a stream that ends without a usage block was
            // truncated. Settle nothing rather than inventing token counts.
            warn!(
                account_id = %account_id,
                model = %model,
                "Upstream stream ended without usage; nothing settled"
            );
            return;
        }
    };

    let Some(model_cfg) = config.models.iter().find(|m| m.name == model) else {
        return;
    };

    // Cache-read tokens stay their own counter, never folded into input: they
    // are a subset of the upstream's prompt_tokens and cost ~50x less
    // (docs/local-development.md:113-117).
    let cost_idr = calculate_token_cost_idr(
        model_cfg.price,
        usage.input_tokens as u64,
        model_cfg.rates.input_peak,
        usage.cache_read_tokens as u64,
        model_cfg.rates.cache_read_peak,
        usage.output_tokens as u64,
        model_cfg.rates.output_peak,
    );

    match debit_usage_transaction(
        &pool,
        account_id,
        Some(key_id),
        usage.input_tokens,
        usage.cache_read_tokens,
        usage.output_tokens,
        cost_idr,
        None,
    )
    .await
    {
        Ok(new_balance) => info!(
            account_id = %account_id,
            cost_idr,
            new_balance,
            input_tokens = usage.input_tokens,
            cache_read_tokens = usage.cache_read_tokens,
            output_tokens = usage.output_tokens,
            "Proxy request settled"
        ),
        Err(err) => warn!(
            account_id = %account_id,
            cost_idr,
            error = %err,
            "Proxy settlement failed"
        ),
    }
}
