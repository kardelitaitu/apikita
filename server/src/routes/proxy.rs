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
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::AppConfig;
use crate::db::debit_usage_transaction;
use crate::error::AppError;
use crate::money::{calculate_preflight_reservation_idr, calculate_token_cost_idr};
use crate::routes::events::{publish_balance, publish_usage, todays_usage, RealtimeHub};
use crate::upstream::{parse_usage_from_sse, UpstreamClient, UpstreamError, UpstreamStream, Usage};

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<AppConfig>,
    pub http_client: reqwest::Client,
    /// The realtime fan-out behind `GET /events`.
    pub events: Arc<RealtimeHub>,
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

/// Whether `model` is permitted for a key whose stored allowlist is
/// `allowed_models`.
///
/// DENY BY DEFAULT (DEFECT 2): an empty allowlist permits NO model — a key made
/// without an explicit model list cannot call the dearest configured model. Only
/// a model that appears in the list is allowed. The list is matched against the
/// model name exactly as the client requested it (before any upstream rewrite).
fn is_model_allowed(allowed_models: &[String], model: &str) -> bool {
    allowed_models.iter().any(|m| m == model)
}

/// Maximum cached API-key records.
///
/// The cache can only ever be filled by a key that actually exists in the
/// database: a presented key with no row is NOT cached (see `load_key_metadata`),
/// so an unauthenticated caller cannot grow this map. In practice the working set
/// is the number of live keys. The cap is a backstop so a pathological number of
/// keys cannot pin unbounded memory; at roughly 250 bytes per record this bounds
/// the cache at about 1 MiB.
const KEY_CACHE_CAPACITY: usize = 4096;

/// The api-key fields the request path decides on. Caching exactly these keeps a
/// cache hit indistinguishable from a fresh read: the same values drive the same
/// checks.
#[derive(Debug, Clone, PartialEq)]
struct KeyMetadata {
    key_id: Uuid,
    account_id: Uuid,
    models: Value,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
    revoked_at: Option<chrono::DateTime<chrono::Utc>>,
}

struct CacheEntry {
    meta: KeyMetadata,
    stored_at: Instant,
}

/// A bounded, TTL-ed cache of API-key metadata, keyed by the HASHED key.
///
/// The plaintext key is never retained: the caller hashes the presented token and
/// only that digest is used as the key here, so a memory dump of this map cannot
/// yield a usable credential.
///
/// STALENESS TRADEOFF - the documented cost of this cache:
/// a key revoked, or its model allowlist narrowed, will still be honoured for up to
/// `limits.key_metadata_cache_seconds` after the change lands in the database. That
/// window is the price of removing a DB round-trip from every proxied request.
/// It is bounded and configurable, and setting the config value to 0 disables the
/// cache entirely (every lookup misses), which is the way to trade the DB read back
/// for immediate revocation.
///
/// Expiry is judged against a monotonic `Instant`, so a wall-clock adjustment can
/// neither expire entries early nor keep them alive past the TTL.
struct KeyCache {
    entries: HashMap<String, CacheEntry>,
    /// Insertion order, for eviction. Kept in step with `entries`: a key is
    /// pushed exactly once, when it is first inserted.
    order: std::collections::VecDeque<String>,
    ttl: Duration,
    capacity: usize,
}

impl KeyCache {
    fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: std::collections::VecDeque::new(),
            ttl,
            capacity: capacity.max(1),
        }
    }

    /// The cached record, or None when it is absent or past its TTL.
    ///
    /// `now` is a parameter rather than a call to `Instant::now()` so the TTL rule
    /// is testable without sleeping. An expired entry is a miss and is left for
    /// `purge_expired` to reclaim.
    fn get(&self, key_hash: &str, now: Instant) -> Option<KeyMetadata> {
        let entry = self.entries.get(key_hash)?;
        if now.duration_since(entry.stored_at) < self.ttl {
            Some(entry.meta.clone())
        } else {
            None
        }
    }

    fn insert(&mut self, key_hash: String, meta: KeyMetadata, now: Instant) {
        if self.entries.len() >= self.capacity {
            // Expired entries are worthless, so reclaim them first; only if that
            // frees nothing do we evict a live one.
            self.purge_expired(now);
        }

        while self.entries.len() >= self.capacity {
            // FIFO by first insertion. A true LRU would need per-hit bookkeeping
            // for no real gain here: the working set is all live keys, so which
            // one goes is close to arbitrary either way.
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.entries.remove(&oldest);
        }

        // Refresh in place: the key is already in `order`, and pushing it again
        // would put two entries in the queue for one map slot.
        let inserted = self
            .entries
            .insert(
                key_hash.clone(),
                CacheEntry {
                    meta,
                    stored_at: now,
                },
            )
            .is_none();
        if inserted {
            self.order.push_back(key_hash);
        }
    }

    fn purge_expired(&mut self, now: Instant) {
        let ttl = self.ttl;
        self.entries
            .retain(|_, entry| now.duration_since(entry.stored_at) < ttl);
        self.order.retain(|key| self.entries.contains_key(key));
    }

    /// Used by the tests to assert the cap holds; not needed on the hot path.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// The key-metadata cache, built once from config on the first request.
static KEY_CACHE: OnceLock<Mutex<KeyCache>> = OnceLock::new();

fn key_cache(config: &AppConfig) -> &'static Mutex<KeyCache> {
    KEY_CACHE.get_or_init(|| {
        Mutex::new(KeyCache::new(
            Duration::from_secs(config.limits.key_metadata_cache_seconds),
            KEY_CACHE_CAPACITY,
        ))
    })
}

/// Resolve the presented key hash to its metadata, from the cache when fresh and
/// from the database otherwise.
///
/// Only a key that exists in the database is ever inserted, so an unknown key costs
/// one query and leaves no trace - a caller cannot use the cache as a memory
/// amplifier by spraying made-up tokens.
async fn load_key_metadata(
    cache: &Mutex<KeyCache>,
    pool: &PgPool,
    key_hash: &str,
) -> Result<KeyMetadata, AppError> {
    // The guard is scoped so the std Mutex is never held across the await below.
    {
        let cache = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(meta) = cache.get(key_hash, Instant::now()) {
            return Ok(meta);
        }
    }

    let row = sqlx::query(
        r#"
        SELECT id, account_id, models, expires_at, revoked_at
        FROM api_keys
        WHERE key_hash = $1
        "#,
    )
    .bind(key_hash)
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Err(AppError::Unauthenticated);
    };

    let meta = KeyMetadata {
        key_id: row.get("id"),
        account_id: row.get("account_id"),
        models: row.get("models"),
        expires_at: row.get("expires_at"),
        revoked_at: row.get("revoked_at"),
    };

    cache.lock().unwrap_or_else(|e| e.into_inner()).insert(
        key_hash.to_string(),
        meta.clone(),
        Instant::now(),
    );

    Ok(meta)
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

    // Served from the TTL cache when warm, from the database otherwise. The
    // checks below are identical either way - only the age of the row differs.
    let key = load_key_metadata(key_cache(&state.config), &state.pool, &key_hash).await?;

    let key_id = key.key_id;
    let account_id = key.account_id;
    let models_val = key.models.clone();
    let expires_at = key.expires_at;
    let revoked_at = key.revoked_at;

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

    // 2. Authorize model. DENY BY DEFAULT: an empty allowlist permits nothing.
    // docs/website/06-api-keys-and-limits.md:41 states the contract explicitly —
    // a key with an empty allowlist can call nothing, so a key created without
    // an explicit model list is refused (DEFECT 2). Only a model that is present
    // in the list is permitted.
    let allowed_models: Vec<String> = serde_json::from_value(models_val).unwrap_or_default();
    if !is_model_allowed(&allowed_models, &meta.model) {
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
            // A transport error is a network-level failure: reqwest's Display
            // embeds the provider URL, so the raw error MUST NOT reach the client
            // (DEFECT 3, docs/error-model.md:159). Log it server-side at warn
            // level and return a generic upstream-unavailable error instead.
            UpstreamError::Transport(_) => {
                warn!(
                    account_id = %account_id,
                    model = %meta.model,
                    error = %err,
                    "upstream transport error; detail withheld from client"
                );
                AppError::NoUpstreamAvailable
            }
            // None of the remaining variants carry a provider URL or hostname, so
            // the generic internal error is safe to surface.
            other => AppError::Internal(other.to_string()),
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
        state.events.clone(),
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
    events: Arc<RealtimeHub>,
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
        Ok(new_balance) => {
            info!(
                account_id = %account_id,
                cost_idr,
                new_balance,
                input_tokens = usage.input_tokens,
                cache_read_tokens = usage.cache_read_tokens,
                output_tokens = usage.output_tokens,
                "Proxy request settled"
            );

            // Only now that the debit has committed. Emitting before commit
            // could announce a balance that then rolls back
            // (docs/realtime.md:146-157). Scoped to account_id so only this
            // account's dashboard receives it (DEFECT 1).
            publish_balance(&events, account_id, new_balance);

            // The event carries today's CUMULATIVE totals, not this request's
            // delta: absolute values make a lost event self-healing
            // (docs/realtime.md:93). If the read fails the balance event has
            // already gone out and the next settlement will correct the
            // usage figures, so this is logged, not fatal.
            match todays_usage(&pool, account_id).await {
                Ok(totals) => publish_usage(&events, account_id, totals),
                Err(err) => warn!(
                    account_id = %account_id,
                    error = %err,
                    "Usage event skipped: totals unavailable"
                ),
            }
        }
        Err(err) => warn!(
            account_id = %account_id,
            cost_idr,
            error = %err,
            "Proxy settlement failed"
        ),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn meta(revoked_at: Option<chrono::DateTime<chrono::Utc>>) -> KeyMetadata {
        KeyMetadata {
            key_id: Uuid::new_v4(),
            account_id: Uuid::new_v4(),
            models: json!(["deepseek-flash"]),
            expires_at: None,
            revoked_at,
        }
    }

    fn cache(ttl_secs: u64, capacity: usize) -> KeyCache {
        KeyCache::new(Duration::from_secs(ttl_secs), capacity)
    }

    #[test]
    fn a_fresh_entry_is_a_hit() {
        let mut c = cache(60, 16);
        let now = Instant::now();
        let stored = meta(None);

        c.insert("hash-a".into(), stored.clone(), now);

        assert_eq!(c.get("hash-a", now), Some(stored));
        // An unknown key is always a miss, never a default. The handler turns
        // that miss into a fresh read.
        assert_eq!(c.get("hash-b", now), None);
    }

    #[test]
    fn an_entry_at_or_past_its_ttl_is_a_miss() {
        let mut c = cache(60, 16);
        let now = Instant::now();
        c.insert("hash-a".into(), meta(None), now);

        // One second inside the window is still a hit.
        assert!(c.get("hash-a", now + Duration::from_secs(59)).is_some());
        // At the boundary and beyond it the entry must not be served: the
        // handler re-reads instead.
        assert_eq!(c.get("hash-a", now + Duration::from_secs(60)), None);
        assert_eq!(c.get("hash-a", now + Duration::from_secs(61)), None);
    }

    #[test]
    fn a_zero_ttl_disables_the_cache() {
        // The config value is the escape hatch: 0 means every lookup misses, so
        // revocation is immediate again at the cost of a read per request.
        let mut c = cache(0, 16);
        let now = Instant::now();
        c.insert("hash-a".into(), meta(None), now);

        assert_eq!(c.get("hash-a", now), None);
    }

    #[test]
    fn the_capacity_cap_evicts_and_stays_bounded() {
        let mut c = cache(60, 2);
        let now = Instant::now();

        c.insert("a".into(), meta(None), now);
        c.insert("b".into(), meta(None), now);
        c.insert("c".into(), meta(None), now);

        assert_eq!(c.len(), 2, "the cap must hold");
        // FIFO: the oldest insertion went, the two newest stayed.
        assert_eq!(c.get("a", now), None);
        assert!(c.get("b", now).is_some());
        assert!(c.get("c", now).is_some());
        // The order queue must not leak entries the map has dropped, or it
        // would grow without bound while the map stayed capped.
        assert_eq!(c.order.len(), c.len());
    }

    #[test]
    fn an_expired_entry_is_reclaimed_before_a_live_one_is_evicted() {
        let mut c = cache(60, 2);
        let start = Instant::now();

        c.insert("stale".into(), meta(None), start);
        c.insert("live".into(), meta(None), start);

        // Well past the TTL and at capacity: the dead entry is reclaimed rather
        // than evicting the one still inside its window.
        let later = start + Duration::from_secs(120);
        c.insert("new".into(), meta(None), later);

        assert_eq!(c.get("stale", later), None);
        assert!(c.get("new", later).is_some());
        assert_eq!(c.len(), 1, "both expired entries were reclaimed");
    }

    #[test]
    fn re_inserting_a_key_does_not_duplicate_its_order_slot() {
        let mut c = cache(60, 4);
        let now = Instant::now();

        c.insert("a".into(), meta(None), now);
        c.insert("a".into(), meta(None), now);
        c.insert("a".into(), meta(None), now);

        assert_eq!(c.len(), 1);
        assert_eq!(c.order.len(), 1, "one map slot, one queue slot");
    }

    #[test]
    fn a_revoked_key_is_not_served_beyond_the_ttl() {
        let mut c = cache(60, 16);
        let now = Instant::now();

        // Cached while the key was still live.
        c.insert("hash-a".into(), meta(None), now);

        // Inside the window the stale live record is still honoured - the
        // documented staleness cost, asserted rather than assumed.
        assert!(c.get("hash-a", now + Duration::from_secs(30)).is_some());

        // Past the window the entry is gone, so the handler re-reads and sees
        // the revocation. The cache cannot extend a revoked key's life.
        assert_eq!(c.get("hash-a", now + Duration::from_secs(61)), None);
    }

    #[test]
    fn a_purge_drops_expired_entries_and_their_order_slots() {
        let mut c = cache(60, 16);
        let start = Instant::now();

        c.insert("old".into(), meta(None), start);
        c.insert("new".into(), meta(None), start + Duration::from_secs(120));

        c.purge_expired(start + Duration::from_secs(121));

        assert_eq!(c.len(), 1);
        assert_eq!(c.order.len(), 1);
        assert_eq!(c.get("old", start + Duration::from_secs(121)), None);
    }

    #[test]
    fn cached_metadata_carries_every_field_the_checks_read() {
        // A hit must drive the same decision as a fresh read, so the record has
        // to carry the account, the key id, the model allowlist and BOTH dates.
        let revoked = chrono::Utc::now();
        let stored = meta(Some(revoked));

        assert_eq!(stored.revoked_at, Some(revoked));
        assert_eq!(stored.models, json!(["deepseek-flash"]));
        assert_ne!(stored.key_id, Uuid::nil());
        assert_ne!(stored.account_id, Uuid::nil());
    }

    // DEFECT 2 regression: an empty allowlist denies everything. A key created
    // without an explicit model list must not be able to call any model, least
    // of all the dearest one in the pool.
    #[test]
    fn empty_allowlist_denies_every_model() {
        assert!(!is_model_allowed(&[], "deepseek-flash"));
        assert!(!is_model_allowed(&[], "gpt-4o"));
        assert!(!is_model_allowed(&[], ""));
    }

    #[test]
    fn listed_model_is_allowed() {
        let allowed = vec!["deepseek-flash".to_string(), "gpt-4o-mini".to_string()];
        assert!(is_model_allowed(&allowed, "deepseek-flash"));
        assert!(is_model_allowed(&allowed, "gpt-4o-mini"));
    }

    #[test]
    fn unlisted_model_is_denied() {
        let allowed = vec!["deepseek-flash".to_string()];
        assert!(!is_model_allowed(&allowed, "gpt-4o"));
        // A non-empty list does not fall back to allowing anything else.
        assert!(!is_model_allowed(&allowed, "deepseek-flash-v2"));
    }
}
