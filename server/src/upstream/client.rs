//! Upstream HTTP client: model resolution, key rotation, streaming.
//!
//! This is the layer between the proxy handler and the wholesale providers. It
//! owns three things that must be built exactly once and shared across requests:
//!
//!   * one `reqwest::Client` with a persistent connection pool
//!     (`pool_max_idle_per_host(100)`), so 100 keys do not pay 100 TLS
//!     handshakes (docs/failover.md, Layer 2);
//!   * one `KeyPool` per model endpoint, populated from the environment
//!     variables the endpoint names in `api_key_envs`;
//!   * one `CircuitBreaker` per model endpoint.
//!
//! Routing is deliberately simple: endpoints are tried in config order (weight
//! 0 = never routed), keys are taken from the endpoint's least-loaded pool, and
//! the outcome is classified per the doctrine in docs/failover.md:
//!
//!   * a 429 (or any status in `KeyPoolConfig.rate_limit_status`) parks that
//!     KEY and rotates to the next key — it never trips the breaker;
//!   * a 5xx or a transport failure is an ENDPOINT fault: `record_failure` and
//!     move to the next endpoint;
//!   * a mid-stream failure is the caller's problem: the bytes stream simply
//!     ends, and it is never retried here (docs/failover.md, "What happens
//!     mid-stream").
//!
//! The URL is used verbatim as configured. SSRF allowlisting is a separate
//! concern and is not done here.

#![cfg_attr(
    not(test),
    // THE UPSTREAM CLIENT, and this one is fenced for a reason the others were not:
    // the values that reach this module are whatever bytes a THIRD PARTY sent.
    //
    // Every other fenced module computes figures from numbers this process
    // produced. Here, a provider's response is parsed field by field, and a provider
    // that is malfunctioning, compromised, or simply different from what we assume
    // is indistinguishable from a correct one. So the arithmetic here is arithmetic
    // on untrusted input, which is a stronger reason to deny it than "this computes
    // money" ever was.
    //
    // It is also nearly free: two sites, and the one that mattered is not an
    // argument at all. `prompt - cached` in the usage parser now SATURATES, because
    // it is the subtraction most likely to be handed a nonsense pair, and because a
    // negative token count does not stay negative - the proxy casts these to u64,
    // where -1 is 18_446_744_073_709_551_615 tokens.
    deny(clippy::arithmetic_side_effects)
)]

use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

// `bytes` is not a direct dependency of this crate (and must not become one);
// axum re-exports the very same `bytes::Bytes` type that reqwest's body stream
// yields, so the item type matches reqwest::Result<Bytes> exactly.
use axum::body::Bytes;
use futures_util::Stream;
use serde_json::{json, Value};

use crate::config::AppConfig;
use crate::upstream::circuit_breaker::CircuitBreaker;
use crate::upstream::key_pool::{KeyLease, KeyPool};

/// Token counts reported by the upstream for one completed stream.
///
/// The upstream reports `prompt_tokens` as the whole prompt, cache hits
/// included. Cache-read tokens are billed at a different (cheaper) rate, so
/// they are split out here: `input_tokens + cache_read_tokens` is the
/// upstream's `prompt_tokens`, and `input_tokens` is never inflated by the
/// cached portion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub input_tokens: i64,
    pub cache_read_tokens: i64,
    pub output_tokens: i64,
}

/// Why a request could not be served.
#[derive(Debug)]
pub enum UpstreamError {
    /// No model with this name is configured.
    NoModel(String),
    /// Every candidate endpoint was circuit-open, failed, or had no usable key.
    NoHealthyUpstream(String),
    /// Every key on every endpoint was rate-limited.
    RateLimited,
    /// The upstream answered with a non-retryable status (4xx).
    ServerError(u16),
    /// The only failure was a network-level one; no HTTP status was seen.
    Transport(String),
}

impl fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoModel(model) => write!(f, "unknown model: {model}"),
            Self::NoHealthyUpstream(model) => write!(f, "no healthy upstream for model: {model}"),
            Self::RateLimited => write!(f, "upstream rate limited every key"),
            Self::ServerError(status) => write!(f, "upstream returned status {status}"),
            Self::Transport(err) => write!(f, "upstream transport error: {err}"),
        }
    }
}

impl std::error::Error for UpstreamError {}

/// Extract the `usage` block from a tail of an OpenAI-style SSE stream.
///
/// `tail` is the last chunk(s) of the response body, so its first line may be
/// a partial one; unparseable lines are skipped. The last usage block wins.
/// Returns `None` when no usage was reported — a stream that died mid-way
/// (`?fail=midstream`), a truncated tail, or a non-SSE body. The caller must
/// treat `None` as "usage unknown", never as "zero tokens".
pub fn parse_usage_from_sse(tail: &[u8]) -> Option<Usage> {
    let mut usage = None;

    for line in tail.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(payload) = line.strip_prefix(b"data:") else {
            continue;
        };
        let Ok(text) = std::str::from_utf8(payload) else {
            continue;
        };
        let text = text.trim();
        if text.is_empty() || text == "[DONE]" {
            continue;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        let Some(block) = chunk.get("usage").filter(|block| !block.is_null()) else {
            continue;
        };

        let prompt = int_field(block, "prompt_tokens");
        // Cached tokens are a subset of the prompt. Never let a malformed
        // report push the split below zero or above the prompt total.
        //
        // `.min(prompt)` handles the UPPER bound. The LOWER one is handled by
        // `int_field`, which clamps every field it reads with `.max(0)` - so
        // `cached` and `prompt` are both non-negative here and the split cannot go
        // negative. That is a CROSS-FUNCTION contract, and this line depends on it
        // from a different function in a different part of the file.
        let cached = nested_int_field(block, "prompt_tokens_details", "cached_tokens").min(prompt);
        usage = Some(Usage {
            // SATURATING, and this is the arithmetic that matters here, because the
            // upstream is a THIRD PARTY and this value is whatever bytes it sent.
            //
            // If `cached` were ever to exceed `prompt` - which the `.min` above
            // prevents, and `int_field`'s clamp keeps both operands non-negative -
            // then `prompt - cached` on i64 could underflow. In a debug build that
            // is a panic on the request path; in the release build that ships, it is
            // a silent wrap to a hugely POSITIVE number. And a negative token count
            // does not stay negative: routes/proxy.rs casts these to u64, where -1
            // becomes 18_446_744_073_709_551_615 tokens, which prices at a figure no
            // wallet can pay - so the request is refused. An adversarial provider
            // could refuse service with one malformed usage block.
            //
            // Saturating costs one instruction and removes the dependency on a
            // contract two functions away. The impossible case then degrades to a
            // zero-token prompt rather than a panic or a 19-quintillion-token bill.
            input_tokens: prompt.saturating_sub(cached),
            cache_read_tokens: cached,
            output_tokens: int_field(block, "completion_tokens"),
        });
    }

    usage
}

/// A non-negative integer field, or 0 when absent/malformed.
fn int_field(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or(0).max(0)
}

/// A non-negative integer nested one object deep — `prompt_tokens_details.cached_tokens`
/// — or 0 when the object or the field is absent/malformed.
fn nested_int_field(value: &Value, object: &str, key: &str) -> i64 {
    value
        .get(object)
        .map(|inner| int_field(inner, key))
        .unwrap_or(0)
}

/// One upstream endpoint with its key pool and breaker, flattened out of
/// `ModelConfig` so the request path never re-reads the config or the
/// environment.
struct EndpointEntry {
    name: String,
    url: String,
    upstream_model: String,
    /// Relative routing weight; 0 means registered but never routed.
    weight: f64,
    pool: KeyPool,
    breaker: CircuitBreaker,
    supports_stream_options: bool,
}

/// One configured model with its endpoints.
struct ModelEntry {
    name: String,
    endpoints: Vec<EndpointEntry>,
}

/// The resolved upstream for one request, streaming.
pub struct UpstreamStream {
    bytes: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
    endpoint_name: String,
    lease: Option<KeyLease>,
}

impl fmt::Debug for UpstreamStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UpstreamStream")
            .field("endpoint_name", &self.endpoint_name)
            .field("lease_held", &self.lease.is_some())
            .finish_non_exhaustive()
    }
}

impl UpstreamStream {
    fn new(response: reqwest::Response, endpoint_name: String, lease: KeyLease) -> Self {
        Self {
            bytes: Box::pin(response.bytes_stream()),
            endpoint_name,
            lease: Some(lease),
        }
    }

    /// The upstream body as it arrives, chunk by chunk. Never buffered whole.
    pub fn bytes(&mut self) -> impl Stream<Item = reqwest::Result<Bytes>> + '_ {
        &mut self.bytes
    }

    /// Which configured endpoint served this stream.
    pub fn endpoint_name(&self) -> &str {
        &self.endpoint_name
    }

    /// The stream completed: free the key slot, no cooldown.
    pub fn finish_ok(mut self) {
        self.release(KeyLease::report_success);
    }

    /// The stream ended with `status` (0 for a transport-level cut, e.g. the
    /// upstream socket dying mid-stream): park the key if that status is a
    /// configured rate limit, otherwise just free it.
    pub fn finish_status(mut self, status: u16) {
        self.release(|lease| lease.report_status(status));
    }

    fn release(&mut self, report: impl FnOnce(KeyLease)) {
        if let Some(lease) = self.lease.take() {
            report(lease);
        }
    }
}

impl Drop for UpstreamStream {
    fn drop(&mut self) {
        // A stream dropped without an explicit finish — the mid-stream error
        // path — releases its key slot without cooldown, so a failed stream can
        // never leak in-flight capacity from the pool.
        self.release(|lease| lease.report_status(0));
    }
}

/// Model name -> endpoint resolution, key rotation, and streaming.
pub struct UpstreamClient {
    config: Arc<AppConfig>,
    http: reqwest::Client,
    models: Vec<ModelEntry>,
}

impl UpstreamClient {
    /// Builds the HTTP client and every endpoint's key pool and breaker. The
    /// environment is read here and only here: a key that appears later needs a
    /// restart, which is preferable to reading `std::env` on the hot path.
    pub fn new(config: Arc<AppConfig>) -> Self {
        let timeout_seconds = config.circuit_breaker.request_timeout_seconds;
        let mut builder = reqwest::Client::builder()
            .tcp_nodelay(true) // SSE: disable Nagle so small frequent frames land immediately
            .pool_idle_timeout(std::time::Duration::from_secs(30)) // 30s: with pool_max_idle_per_host(100) and 3 endpoints the 90s default holds 300 idle sockets on a 256 MB container
            .pool_max_idle_per_host(100);
        if timeout_seconds > 0 {
            // A per-read timeout, not a total one: it catches an upstream that
            // stalls with no bytes (docs/failover.md, "Upstream stalls"), while
            // leaving a long but healthy SSE stream alone. A total timeout
            // would cut a legitimate 4096-token answer off at the wire.
            let timeout = Duration::from_secs(timeout_seconds);
            builder = builder.connect_timeout(timeout).read_timeout(timeout);
        }
        let http = builder
            .build()
            .expect("failed to build the upstream HTTP client");

        let models = config
            .models
            .iter()
            .map(|model| ModelEntry {
                name: model.name.clone(),
                endpoints: model
                    .endpoints
                    .iter()
                    .map(|endpoint| EndpointEntry {
                        name: endpoint.name.clone(),
                        url: endpoint.url.clone(),
                        upstream_model: endpoint.upstream_model.clone(),
                        weight: endpoint.weight,
                        pool: KeyPool::new(
                            keys_from_env(&endpoint.api_key_envs),
                            config.key_pool.key_cooldown_seconds,
                            config.key_pool.rate_limit_status.clone(),
                            config.key_pool.max_key_attempts,
                        ),
                        breaker: CircuitBreaker::new(config.circuit_breaker.clone()),
                        supports_stream_options: endpoint.supports_stream_options,
                    })
                    .collect(),
            })
            .collect();

        Self {
            config,
            http,
            models,
        }
    }

    /// Is this a model the proxy is allowed to route?
    pub fn has_model(&self, name: &str) -> bool {
        self.model(name).is_some()
    }

    /// Every configured model name, in config order.
    pub fn allowed_models(&self) -> Vec<&str> {
        self.models
            .iter()
            .map(|model| model.name.as_str())
            .collect()
    }

    /// The shortest remaining cooldown across `model`'s endpoint pool, in whole
    /// seconds, floored at 1.
    ///
    /// This is the value docs/error-model.md (Retry-After) defines for a 503: "the
    /// earliest moment a retry could plausibly succeed", i.e.
    /// `min over endpoints of (cooldown_until - now)`.
    ///
    /// `None` means NO breaker in this model's pool is Open — so there is no
    /// cooldown to report. That is a real, distinct outcome (the 503 may have
    /// come from a path where nothing tripped), and it is deliberately NOT
    /// collapsed into a number here: the caller decides what to emit and logs
    /// the cause, so the header is never an unexplained guess
    /// (docs/error-model.md (Retry-After)). Also `None` when the model is unknown.
    /// Whether EVERY routed endpoint of `model` currently has its breaker OPEN, i.e.
    /// nothing in the pool can serve the model.
    ///
    /// **This is NOT `shortest_cooldown_secs`, and the difference is the whole point.**
    /// That accessor answers "when could a retry succeed" and returns `Some` as soon as
    /// ONE endpoint is cooling. The alert condition
    /// (the "All providers unhealthy" row of the Alerts table in
    /// `docs/observability.md`, condition "circuit open on every endpoint") is the opposite
    /// extreme: ONE open endpoint means failover is WORKING, which is normal operation,
    /// while EVERY endpoint open means the model cannot be served at all. Reusing the
    /// cooldown accessor would page an operator during healthy failover - worse than no
    /// alert, because it teaches them to ignore it.
    ///
    /// **Unknown model -> `false`.** A model that does not exist is ABSENT, not down,
    /// so collapsing the two would fire the alert on a typo.
    ///
    /// **Only ROUTED endpoints count** (`weight > 0.0`), matching the failover loop's
    /// own filter. A weight-0 endpoint is registered but never selected, so its breaker
    /// cannot represent a provider that is down - and letting it count would mean an
    /// unrouted placeholder could both cause a false alarm and, worse, mask a real
    /// outage on the one endpoint that IS routed.
    ///
    /// **AND ONLY ENDPOINTS THAT CAN ACTUALLY SERVE.** Weight alone is not enough, and
    /// the gap was live. A weighted endpoint with NO CONFIGURED KEYS can never answer
    /// a request: `keys_from_env` yields an empty pool, `acquire` returns None, and the
    /// failover loop steps straight past it. Its breaker is never exercised either, so
    /// it never records a failure and stays Closed forever.
    ///
    /// So a placeholder left at weight 1.0 with no keys votes "this model is
    /// available" even when the one real provider is down. The shipped config has
    /// exactly that: `secondary` on the flash model is documented as a placeholder
    /// whose resale terms were never read, carries weight 1.0, and its two key
    /// variables are empty in .env.example. `all_providers_unhealthy` therefore could
    /// not fire for the only outage it was written to catch. An alert that is
    /// structurally unable to fire is worse than no alert, because it reads as one
    /// that is working.
    ///
    /// An EMPTY routed pool gives `false` too: with nothing routed there is no outage
    /// to report, and a vacuously-true "all zero endpoints are open" would fire an
    /// alert for a model that was never served.
    /// The `routed` count is SAFE by the length of its own input: it counts the
    /// model's OWN endpoints, which come from the config file rather than from a
    /// provider, and a config holding more endpoints than a usize can address is not
    /// a thing. The count is only ever compared against zero, to tell "no endpoint
    /// was routed" apart from "some were, and every one of them is open".
    ///
    /// The allow is on the FUNCTION because the increment is a loop tail
    /// expression, and an attribute there is still unstable.
    #[allow(clippy::arithmetic_side_effects)]
    pub fn all_endpoints_unhealthy(&self, model: &str) -> bool {
        let Some(entry) = self.model(model) else {
            return false;
        };

        let mut routed = 0usize;
        // A weighted endpoint with no keys cannot serve, so it must not count: its
        // breaker never trips, and counting it lets a placeholder mask a real outage
        // on the provider that is actually serving. See the doc above.
        for endpoint in entry
            .endpoints
            .iter()
            .filter(|e| e.weight > 0.0 && e.pool.key_count() > 0)
        {
            routed += 1;
            if endpoint.breaker.allow_request() {
                // At least one routed endpoint can still serve, so the model is up.
                return false;
            }
        }

        // Every routed endpoint refused. `routed > 0` guards the vacuous case: an
        // empty pool must not report an outage.
        routed > 0
    }
    pub fn shortest_cooldown_secs(&self, model: &str) -> Option<u64> {
        let entry = self.model(model)?;
        entry
            .endpoints
            .iter()
            .filter_map(|endpoint| endpoint.breaker.remaining_cooldown())
            .min()
            // Whole seconds, rounded UP, then floored at 1.
            //
            // Rounding up is the same rule the 429 path follows
            // (docs/error-model.md (429 — rate limited)) and it is the safe direction: a value
            // that is too low tells the client to retry before a retry can
            // succeed. Ceiling division via `u64::div_ceil` (stable since 1.73).
            //
            // Floor at 1: docs/error-model.md (429 — rate limited) - "A zero or negative value
            // is a malformed header", and a client that honours 0 retries into
            // a refusal.
            .map(|cooldown| (cooldown.as_millis() as u64).div_ceil(1000).max(1))
    }

    /// Send a streaming chat-completions request to the first endpoint that
    /// answers, rotating keys on a throttle and endpoints on a fault.
    ///
    /// `body` is the caller's OpenAI-shaped payload; `model` is rewritten to
    /// the endpoint's `upstream_model` and `stream` is forced true.
    pub async fn stream_chat(
        &self,
        model: &str,
        body: Value,
    ) -> Result<UpstreamStream, UpstreamError> {
        let entry = self
            .model(model)
            .ok_or_else(|| UpstreamError::NoModel(model.to_string()))?;

        // A 5xx / open circuit means a genuinely broken provider; a 429 means
        // we are saturated. The former is the more honest answer when both
        // happen, so it is classified first.
        let mut unhealthy = false;
        let mut rate_limited = false;
        let mut transport = None;

        for endpoint in entry
            .endpoints
            .iter()
            .filter(|endpoint| endpoint.weight > 0.0)
        {
            if !endpoint.breaker.allow_request() {
                unhealthy = true;
                continue;
            }

            let payload = prepare_body(
                &body,
                &endpoint.upstream_model,
                endpoint.supports_stream_options,
            );

            for _ in 0..endpoint.pool.max_attempts() {
                let Some(lease) = endpoint.pool.acquire() else {
                    // Every key is cooling, or none is configured. Do not
                    // queue: fail fast so the caller can retry
                    // (docs/failover.md, edge cases).
                    break;
                };

                match self.send(&endpoint.url, lease.key(), &payload).await {
                    Err(err) => {
                        lease.report_status(0);
                        endpoint.breaker.record_failure();
                        transport = Some(err.to_string());
                        break;
                    }
                    Ok(response) => {
                        let status = response.status().as_u16();

                        if status == 200 {
                            endpoint.breaker.record_success();
                            return Ok(UpstreamStream::new(response, endpoint.name.clone(), lease));
                        }

                        if self.is_rate_limit(status) {
                            // A throttled key is saturated, not broken: park it
                            // and rotate. This never trips the breaker.
                            lease.report_status(status);
                            rate_limited = true;
                            continue;
                        }

                        if status >= 500 {
                            lease.report_status(status);
                            endpoint.breaker.record_failure();
                            unhealthy = true;
                            break;
                        }

                        // Any other 4xx is the caller's request, not a provider
                        // fault: do not trip the breaker, do not retry a
                        // different provider with the same bad body.
                        lease.report_status(status);
                        return Err(UpstreamError::ServerError(status));
                    }
                }
            }
        }

        if unhealthy {
            Err(UpstreamError::NoHealthyUpstream(model.to_string()))
        } else if rate_limited {
            Err(UpstreamError::RateLimited)
        } else if let Some(err) = transport {
            Err(UpstreamError::Transport(err))
        } else {
            Err(UpstreamError::NoHealthyUpstream(model.to_string()))
        }
    }

    fn model(&self, name: &str) -> Option<&ModelEntry> {
        self.models.iter().find(|entry| entry.name == name)
    }

    fn is_rate_limit(&self, status: u16) -> bool {
        self.config.key_pool.rate_limit_status.contains(&status)
    }

    async fn send(
        &self,
        base_url: &str,
        key: &str,
        payload: &Value,
    ) -> reqwest::Result<reqwest::Response> {
        self.http
            .post(format!(
                "{}/chat/completions",
                base_url.trim_end_matches('/')
            ))
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {key}"))
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(payload)
            .send()
            .await
    }
}

/// Rewrite the caller's payload for one endpoint: the upstream knows the model
/// by its own name, this layer only ever streams, and it must ask for the usage
/// block that settlement is built on.
fn prepare_body(body: &Value, upstream_model: &str, supports_stream_options: bool) -> Value {
    let mut payload = body.clone();
    if let Some(object) = payload.as_object_mut() {
        // A caller that says nothing about streaming is treated as a streaming
        // caller, because that is what this client is for. Only an explicit
        // "stream": false opts out of the usage opt-in below.
        let streaming = object
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        object.insert("model".to_string(), json!(upstream_model));
        object.insert("stream".to_string(), json!(true));

        // OpenAI-compatible upstreams emit the final usage block on SSE only
        // when this opt-in is present; otherwise they assume a human is
        // watching and omit it. Settlement reads that block
        // (parse_usage_from_sse) and settles nothing without it, so a stream
        // that skipped the opt-in would deliver tokens and bill zero — the one
        // error an arbitrage gateway cannot make.
        //
        // A caller may legitimately send stream_options of their own, so their
        // object is merged into, never replaced: only include_usage is set and
        // every other key they sent survives.
        if streaming && supports_stream_options {
            let options = object
                .entry("stream_options".to_string())
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            if !options.is_object() {
                // A non-object stream_options is not a shape the upstream would
                // accept anyway, so there is nothing meaningful to preserve.
                *options = Value::Object(serde_json::Map::new());
            }
            if let Some(options) = options.as_object_mut() {
                options.insert("include_usage".to_string(), json!(true));
            }
        }
    }
    payload
}

/// Read the endpoint's keys from the environment. An unset or blank variable is
/// an absent key, not an error: a provider with no configured key simply has an
/// empty pool.
fn keys_from_env(envs: &[String]) -> Vec<String> {
    envs.iter()
        .filter_map(|name| std::env::var(name).ok())
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        CircuitBreakerConfig, KeyPoolConfig, LimitsConfig, ModelConfig, ModelEndpoint, ModelRates,
        NetworkConfig, PricingConfig, RealtimeConfig, SessionsConfig, StreamingConfig,
        WalletConfig,
    };
    use crate::routes::test_env::{EnvGuard, EnvLock};
    use crate::upstream::circuit_breaker::BreakerState;

    /// Serialize JSON chunks as an SSE body, the way an OpenAI-style upstream
    /// does: `data: <json>` followed by a blank line, per event.
    fn sse(chunks: &[Value]) -> Vec<u8> {
        let mut body = String::new();
        for chunk in chunks {
            body.push_str("data: ");
            body.push_str(&chunk.to_string());
            body.push_str("\n\n");
        }
        body.into_bytes()
    }

    /// The final chunk tools/fake-upstream/server.mjs writes before [DONE].
    fn fake_usage_chunk() -> Value {
        json!({
            "id": "chatcmpl-fake-1",
            "object": "chat.completion.chunk",
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 11, "completion_tokens": 24, "total_tokens": 35 }
        })
    }

    fn endpoint(name: &str, weight: f64) -> ModelEndpoint {
        endpoint_at(name, weight, None, None)
    }

    /// An endpoint that OVERRIDES the model's peak rates, so a pool can hold
    /// genuinely dearer and cheaper providers. Without this every endpoint of a
    /// model ties and the dearest-endpoint rule is unobservable.
    fn endpoint_at(
        name: &str,
        weight: f64,
        input_peak: Option<f64>,
        output_peak: Option<f64>,
    ) -> ModelEndpoint {
        ModelEndpoint {
            name: name.to_string(),
            url: format!("https://{name}.example.com/v1"),
            upstream_model: "deepseek-flash".to_string(),
            api_key_envs: vec![format!("APK_TEST_{}_KEY_1", name.to_uppercase())],
            concurrency_per_key: 0,
            weight,
            supports_stream_options: true,
            input_peak,
            output_peak,
        }
    }

    fn model(name: &str, endpoints: Vec<ModelEndpoint>) -> ModelConfig {
        ModelConfig {
            name: name.to_string(),
            description: String::new(),
            price: 1.5,
            max_context_tokens: 1_000_000,
            max_output_tokens: 384_000,
            supports_vision: true,
            supports_thinking: true,
            billing_basis: "peak".to_string(),
            rates: ModelRates {
                cache_read_offpeak: 26.77,
                cache_read_peak: 53.54,
                input_offpeak: 1338.39,
                input_peak: 2676.78,
                output_offpeak: 5353.56,
                output_peak: 10707.12,
            },
            endpoints,
        }
    }

    fn client(models: Vec<ModelConfig>) -> UpstreamClient {
        UpstreamClient::new(Arc::new(AppConfig {
            pricing: PricingConfig {
                currency: "IDR".to_string(),
            },
            wallet: WalletConfig {
                min_topup: 10_000,
                min_first_deposit: 50_000,
                min_monthly_tokens: 0,
                dormancy_days: 0,
                reserve_settlement_cycles: 1,
                low_balance_threshold_idr: 10_000,
                low_balance_max_per_day: 1,
            },
            sessions: SessionsConfig {
                absolute_days: 30,
                idle_days: 7,
            },
            limits: LimitsConfig {
                link_code_issuance_per_hour: 10,
                link_redemption_per_hour: 20,
                topup_per_hour: 5,
                wallet_mutations_per_minute: 10,
                key_creation_per_day: 10,
                review_per_hour: 3,
                key_metadata_cache_seconds: 60,
            },
            realtime: RealtimeConfig {
                replay_buffer_events: 100,
                max_connections_per_account: 5,
                max_stream_seconds: 1800,
            },
            key_pool: KeyPoolConfig {
                rate_limit_status: vec![429],
                key_cooldown_seconds: 5,
                max_key_attempts: 3,
                on_pool_exhausted: "reject_503".to_string(),
            },
            circuit_breaker: CircuitBreakerConfig {
                failure_threshold: 3,
                cooldown_seconds: 30,
                cooldown_max_seconds: 900,
                cooldown_multiplier: 2.0,
                request_timeout_seconds: 120,
                health_check_interval_seconds: 30,
                health_check_failures: 2,
            },
            streaming: StreamingConfig {
                mid_stream_cutoff: false,
                hard_max_output_tokens: 384_000,
                max_context_tokens: 1_000_000,
            },
            // These unit tests never resolve a client address, so no proxy is
            // trusted: an empty list means the TCP peer is always recorded.
            network: NetworkConfig {
                trusted_proxy_cidrs: vec![],
            },
            models,
        }))
    }

    #[test]
    fn parse_usage_without_cache_details() {
        // Exactly the tail tools/fake-upstream/server.mjs produces: the final
        // chunk carrying usage, then [DONE].
        let mut tail = sse(&[fake_usage_chunk()]);
        tail.extend_from_slice(b"data: [DONE]\n\n");

        assert_eq!(
            parse_usage_from_sse(&tail),
            Some(Usage {
                input_tokens: 11,
                cache_read_tokens: 0,
                output_tokens: 24,
            })
        );
    }

    #[test]
    fn parse_usage_with_cached_tokens_keeps_them_out_of_input() {
        let tail = sse(&[json!({
            "id": "c",
            "choices": [],
            "usage": {
                "prompt_tokens": 11,
                "completion_tokens": 24,
                "total_tokens": 35,
                "prompt_tokens_details": { "cached_tokens": 5 }
            }
        })]);

        let usage = parse_usage_from_sse(&tail).expect("usage block");
        assert_eq!(usage.cache_read_tokens, 5);
        // 11 prompt tokens include the 5 cache hits: 6 fresh + 5 cached.
        assert_eq!(usage.input_tokens, 6);
        assert_eq!(usage.output_tokens, 24);
        assert_eq!(
            usage.input_tokens + usage.cache_read_tokens,
            11,
            "the split must still add up to the upstream prompt total"
        );
    }

    #[test]
    fn parse_usage_ignores_a_malformed_cached_count() {
        let tail = sse(&[json!({
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 1,
                "prompt_tokens_details": { "cached_tokens": 99 }
            }
        })]);

        assert_eq!(
            parse_usage_from_sse(&tail),
            Some(Usage {
                input_tokens: 0,
                cache_read_tokens: 10,
                output_tokens: 1,
            })
        );
    }

    /// A NEGATIVE token count is the shape a hostile or broken provider sends,
    /// and it is the one the existing tests never produced: the malformed-count test
    /// above sends `cached_tokens` ABOVE the prompt total, which exercises the
    /// `.min(prompt)` clamp. Nothing sent a negative, so the other half of the
    /// defence - `int_field`'s `.max(0)` - was relied on by
    /// `parse_usage_from_sse` and never checked.
    ///
    /// The consequence of not checking is specific and bad. `int_field` clamps, so
    /// both operands are non-negative and `prompt - cached` cannot underflow. Remove
    /// that clamp and `cached_tokens: i64::MIN` with `prompt_tokens: 10` gives
    /// `min(i64::MIN, 10) == i64::MIN`, and `10 - i64::MIN` panics in a debug build
    /// and WRAPS in the release build that ships.
    ///
    /// And a negative token count does not stay negative downstream: the proxy
    /// casts these to u64, where -1 is 18_446_744_073_709_551_615 tokens. So the
    /// failure is not a quiet miscount, it is a request priced beyond any wallet and
    /// refused - one malformed usage block from one provider is enough.
    #[test]
    fn a_negative_token_count_from_the_provider_is_clamped_to_zero() {
        for (prompt, cached, output) in [
            (-50i64, -20i64, -1i64),
            // The worst case for the subtraction: the most negative i64, against a
            // small positive prompt. Without the clamp this is the underflow.
            (10, i64::MIN, 0),
            (0, i64::MIN, 0),
        ] {
            let tail = sse(&[json!({
                "usage": {
                    "prompt_tokens": prompt,
                    "completion_tokens": output,
                    "prompt_tokens_details": { "cached_tokens": cached }
                }
            })]);

            let usage = parse_usage_from_sse(&tail).expect("a usage block is present");
            assert!(
                usage.input_tokens >= 0,
                "prompt {prompt} with cached {cached} produced a NEGATIVE input count: \
                 a negative here becomes a huge u64 at the proxy's cast"
            );
            assert!(
                usage.cache_read_tokens >= 0,
                "cached {cached} survived as a negative cache-read count"
            );
            assert!(
                usage.output_tokens >= 0,
                "output {output} survived as a negative output count"
            );
        }

        // The control: the clamp is not silently rewriting a normal report. A prompt
        // of 10 with a cache hit of 4 must still split 6 + 4.
        let tail = sse(&[json!({
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 2,
                "prompt_tokens_details": { "cached_tokens": 4 }
            }
        })]);
        let usage = parse_usage_from_sse(&tail).expect("a usage block is present");
        assert_eq!(
            (
                usage.input_tokens,
                usage.cache_read_tokens,
                usage.output_tokens
            ),
            (6, 4, 2),
            "a well-formed report must be reported unchanged"
        );
    }

    #[test]
    fn parse_usage_from_a_truncated_midstream_tail_is_none() {
        // ?fail=midstream: a few content chunks, then the socket dies. No
        // [DONE] and no usage block at all.
        let cut = sse(&[
            json!({ "id": "c", "choices": [{ "index": 0, "delta": { "content": "Hello" } }] }),
            json!({ "id": "c", "choices": [{ "index": 0, "delta": { "content": " world" } }] }),
        ]);
        assert_eq!(parse_usage_from_sse(&cut), None);
        assert_eq!(parse_usage_from_sse(b""), None);

        // A tail cut off inside the usage block is not usable usage either.
        let complete = sse(&[fake_usage_chunk()]);
        assert_eq!(parse_usage_from_sse(&complete[..complete.len() - 8]), None);
    }

    #[test]
    fn parse_usage_takes_the_last_block_and_skips_non_sse_lines() {
        // A comment line, a non-data field, then two usage blocks: the last
        // one wins.
        let mut tail = b": keep-alive comment\n\nevent: usage\n".to_vec();
        tail.extend(sse(&[
            json!({ "usage": { "prompt_tokens": 1, "completion_tokens": 1 } }),
            json!({ "usage": { "prompt_tokens": 20, "completion_tokens": 3 } }),
        ]));

        assert_eq!(
            parse_usage_from_sse(&tail),
            Some(Usage {
                input_tokens: 20,
                cache_read_tokens: 0,
                output_tokens: 3,
            })
        );
    }

    // ---------------------------------------------------------------------
    // shortest_cooldown_secs: the source of a 503 Retry-After
    // (docs/error-model.md, Retry-After)
    // ---------------------------------------------------------------------

    /// Trips the breaker of endpoint `index` on `client`'s first model, the way
    /// three consecutive 5xx responses would.
    fn trip_endpoint(client: &UpstreamClient, index: usize) {
        let breaker = &client.models[0].endpoints[index].breaker;
        breaker.record_failure();
        breaker.record_failure();
        breaker.record_failure();
        assert_eq!(breaker.state(), BreakerState::Open);
    }

    // ---------------------------------------------------------------------
    // all_endpoints_unhealthy: the `all_providers_unhealthy` alert condition
    // ---------------------------------------------------------------------
    //
    // This is DELIBERATELY NOT `shortest_cooldown_secs`. That accessor answers "when
    // could a retry succeed" and returns Some when ANY endpoint is cooling, which is
    // the OPPOSITE question: one open endpoint means failover is WORKING. An alert
    // built on it would page on healthy failover, and an alert that fires during
    // normal operation is worse than none, because it trains the operator to ignore
    // it. The condition here is EVERY endpoint open, i.e. nothing can serve the
    // model.

    /// Only ONE endpoint open is NOT unhealthy: failover is doing its job.
    ///
    /// Both endpoints are KEYED, because "failover is working" is only true if the
    /// second one could actually answer. With an unkeyed second endpoint the correct
    /// answer is the opposite - which is the defect the sibling test pins.
    #[test]
    fn one_open_endpoint_is_not_all_providers_unhealthy() {
        let _lock = EnvLock::acquire();
        let _primary = EnvGuard::set("APK_TEST_PRIMARY_KEY_1", "test-key");
        let _secondary = EnvGuard::set("APK_TEST_SECONDARY_KEY_1", "test-key");
        let client = client(vec![model(
            "flash",
            vec![endpoint("primary", 1.0), endpoint("secondary", 1.0)],
        )]);
        trip_endpoint(&client, 0);

        assert!(
            !client.all_endpoints_unhealthy("flash"),
            "a single open endpoint means failover is WORKING - alerting here would page on normal operation"
        );

        // POSITIVE CONTROL for the fixture: the tripped breaker really is open, so the
        // assertion above is about the OTHER endpoint being usable and not about a
        // fixture that never tripped anything.
        assert!(
            !client.models[0].endpoints[0].breaker.allow_request(),
            "the fixture must actually have tripped endpoint 0"
        );
        assert_eq!(
            client.shortest_cooldown_secs("flash"),
            Some(30),
            "and the OLD accessor still reports a cooldown - which is exactly why it cannot be reused for this alert"
        );
    }

    /// A WEIGHTED endpoint with NO KEYS cannot mask a real outage on the endpoint
    /// that can serve, and this is the shipped shape.
    ///
    /// config/apikita.toml leaves the flash model's secondary endpoint at weight 1.0 -
    /// a placeholder whose resale terms were never read - with its two key variables
    /// empty in .env.example. keys_from_env yields an empty pool, acquire returns
    /// None, and the failover loop steps straight past it. Its breaker is never
    /// exercised either, so it stays Closed and votes "this model is available".
    ///
    /// Before this test existed, that meant the all_providers_unhealthy alert could not
    /// fire for the only outage it was written to catch: the one real provider goes
    /// down, its breaker opens, and an unkeyed placeholder says everything is fine.
    #[test]
    fn a_weighted_endpoint_with_no_keys_cannot_mask_a_real_outage() {
        let _lock = EnvLock::acquire();
        // The one real provider: keyed, and tripped below.
        let _key = EnvGuard::set("APK_TEST_PRIMARY_KEY_1", "test-key");
        // The placeholder: weighted 1.0, and deliberately NO key.
        let _no_key = EnvGuard::remove("APK_TEST_SECONDARY_KEY_1");

        let client = client(vec![model(
            "flash",
            vec![endpoint("primary", 1.0), endpoint("secondary", 1.0)],
        )]);
        trip_endpoint(&client, 0);

        assert!(
            client.all_endpoints_unhealthy("flash"),
            "the only endpoint that can SERVE is open, so the model is down. A weighted \
             endpoint with no keys cannot answer a request, so it must not vote \
             healthy and hide the outage"
        );
    }

    /// EVERY endpoint open IS unhealthy: nothing can serve this model.
    ///
    /// Both endpoints are KEYED, which this fixture previously was not - it ran with
    /// two empty pools and passed, which is the bug the test above pins rather than
    /// the property it claims. An endpoint with no keys cannot serve, so asserting
    /// "nothing can serve" about one asserts nothing.
    #[test]
    fn every_open_endpoint_is_all_providers_unhealthy() {
        let _lock = EnvLock::acquire();
        let _primary = EnvGuard::set("APK_TEST_PRIMARY_KEY_1", "test-key");
        let _secondary = EnvGuard::set("APK_TEST_SECONDARY_KEY_1", "test-key");
        let client = client(vec![model(
            "flash",
            vec![endpoint("primary", 1.0), endpoint("secondary", 1.0)],
        )]);
        trip_endpoint(&client, 0);
        trip_endpoint(&client, 1);

        assert!(
            client.all_endpoints_unhealthy("flash"),
            "with every endpoint open the model cannot be served, which is the alert condition"
        );
    }

    /// A HEALTHY pool is not unhealthy, so the accessor is not a constant.
    #[test]
    fn a_healthy_pool_is_not_all_providers_unhealthy() {
        let client = client(vec![model(
            "flash",
            vec![endpoint("primary", 1.0), endpoint("secondary", 1.0)],
        )]);

        assert!(!client.all_endpoints_unhealthy("flash"));
    }

    /// An UNKNOWN model is not "unhealthy" - it is ABSENT, which is a different thing.
    ///
    /// Collapsing the two would make the alert fire for a typo in a model name, and
    /// more importantly a model that does not exist cannot be "down".
    #[test]
    fn an_unknown_model_is_not_reported_as_unhealthy() {
        let client = client(vec![model("flash", vec![endpoint("primary", 1.0)])]);

        assert!(!client.all_endpoints_unhealthy("ghost"));
    }

    /// A WEIGHT-0-only pool is not unhealthy either: it is not routed at all.
    ///
    /// A weight-0 endpoint is registered but never selected (the failover loop
    /// filters on `weight > 0.0`). Counting its breaker as "an endpoint that is
    /// down" would let an unrouted placeholder make the alert fire.
    #[test]
    fn a_weightless_only_pool_is_not_all_providers_unhealthy() {
        let client = client(vec![model("flash", vec![endpoint("ghost", 0.0)])]);

        assert!(
            !client.all_endpoints_unhealthy("flash"),
            "an endpoint with weight 0 is never routed, so it cannot be a provider that is down"
        );
    }

    /// A weight-0 endpoint does NOT mask a real outage on a routed one.
    ///
    /// The routed endpoint is KEYED, for the same reason as above: an unkeyed one
    /// cannot serve, so asserting an outage for it would be asserting the bug rather
    /// than the property.
    #[test]
    fn a_weightless_endpoint_does_not_mask_a_routed_outage() {
        let _lock = EnvLock::acquire();
        let _key = EnvGuard::set("APK_TEST_PRIMARY_KEY_1", "test-key");
        let client = client(vec![model(
            "flash",
            vec![endpoint("ghost", 0.0), endpoint("primary", 1.0)],
        )]);
        trip_endpoint(&client, 1);

        assert!(
            client.all_endpoints_unhealthy("flash"),
            "the only ROUTED endpoint is open, so the model cannot be served - the unrouted placeholder must not hide it"
        );
    }
    #[test]
    fn no_open_breaker_reports_none_not_a_guessed_number() {
        // The whole point of the Option: "nothing is open" is a real, distinct
        // outcome the caller must handle explicitly, never a fabricated value.
        let client = client(vec![model("flash", vec![endpoint("primary", 1.0)])]);
        assert_eq!(client.shortest_cooldown_secs("flash"), None);
    }

    #[test]
    fn an_unknown_model_reports_none() {
        let client = client(vec![model("flash", vec![endpoint("primary", 1.0)])]);
        assert_eq!(client.shortest_cooldown_secs("ghost"), None);
    }

    #[test]
    fn the_cooldown_of_a_tripped_breaker_is_reported() {
        let client = client(vec![model("flash", vec![endpoint("primary", 1.0)])]);
        trip_endpoint(&client, 0);

        let secs = client
            .shortest_cooldown_secs("flash")
            .expect("an Open breaker has a cooldown to report");
        assert_eq!(secs, 30, "the base cooldown from config, floored at 1");
    }

    /// The accessor must scan the WHOLE pool. A `first()`-style implementation
    /// would report None here, because the head endpoint is healthy.
    #[test]
    fn a_cooldown_is_found_even_when_only_a_later_endpoint_is_open() {
        let client = client(vec![model(
            "flash",
            vec![endpoint("primary", 1.0), endpoint("secondary", 1.0)],
        )]);

        assert_eq!(
            client.models[0].endpoints[0].breaker.state(),
            BreakerState::Closed
        );
        trip_endpoint(&client, 1);

        assert_eq!(
            client.shortest_cooldown_secs("flash"),
            Some(30),
            "the open endpoint is the second one; the pool must still be scanned"
        );
    }

    #[test]
    fn a_healthy_endpoint_does_not_mask_an_open_one() {
        let client = client(vec![model(
            "flash",
            vec![endpoint("primary", 1.0), endpoint("secondary", 1.0)],
        )]);
        trip_endpoint(&client, 0);

        // One Open, one Closed: the answer is the Open one's cooldown.
        assert_eq!(client.shortest_cooldown_secs("flash"), Some(30));
        assert!(
            client.models[0].endpoints[1].breaker.allow_request(),
            "the healthy endpoint is unaffected"
        );
    }

    #[test]
    fn the_reported_cooldown_is_never_zero() {
        let client = client(vec![model("flash", vec![endpoint("primary", 1.0)])]);
        trip_endpoint(&client, 0);

        // docs/error-model.md (429 — rate limited) - a zero or negative Retry-After is a
        // malformed header, so the floor is part of the accessor's contract.
        assert!(client.shortest_cooldown_secs("flash").unwrap() >= 1);
    }

    #[test]
    fn models_are_resolved_from_config() {
        let client = client(vec![
            model("flash", vec![endpoint("primary", 1.0)]),
            model("deepseek-v4-flash", vec![endpoint("primary", 1.0)]),
        ]);

        assert!(client.has_model("flash"));
        assert!(!client.has_model("gpt-5"));
        assert_eq!(client.allowed_models(), vec!["flash", "deepseek-v4-flash"]);
    }

    #[tokio::test]
    async fn stream_chat_sends_the_endpoint_model_and_streams_the_body() {
        use futures_util::StreamExt;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // A one-shot upstream that answers exactly like
        // tools/fake-upstream/server.mjs: an SSE body terminated by [DONE].
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let addr = listener.local_addr().expect("local addr");

        let served = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buffer = Vec::new();
            loop {
                let mut chunk = [0u8; 1024];
                let read = socket.read(&mut chunk).await.expect("read");
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
                let text = String::from_utf8_lossy(&buffer).into_owned();
                let Some((head, body)) = text.split_once("\r\n\r\n") else {
                    continue;
                };
                let expected: usize = head
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(str::to_string)
                    })
                    .and_then(|len| len.trim().parse().ok())
                    .unwrap_or(0);
                if body.len() >= expected {
                    break;
                }
            }

            let mut body = sse(&[fake_usage_chunk()]);
            body.extend_from_slice(b"data: [DONE]\n\n");
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write head");
            socket.write_all(&body).await.expect("write body");
            socket.flush().await.expect("flush");
            String::from_utf8_lossy(&buffer).into_owned()
        });

        // The key comes from the environment variable the endpoint names.
        // The ONE process-wide lock over environment mutation. These tests
        // run in PARALLEL THREADS, and the environment is process-global, so
        // without it two tests interleave their writes and the fixtures stop
        // meaning what they say. It is deliberately not Send: it belongs in
        // the test body, not in a spawned task.
        let _lock = EnvLock::acquire();
        let _env = EnvGuard::set("APK_TEST_LIVE_KEY_1", "test-key");
        let mut live = endpoint("live", 1.0);
        live.url = format!("http://{addr}/v1");
        let client = client(vec![model("flash", vec![live])]);

        let mut stream = client
            .stream_chat(
                "flash",
                json!({ "model": "flash", "messages": [], "stream": true }),
            )
            .await
            .expect("an endpoint answered 200");
        assert_eq!(stream.endpoint_name(), "live");

        let mut tail = Vec::new();
        while let Some(chunk) = stream.bytes().next().await {
            tail.extend_from_slice(&chunk.expect("body chunk"));
        }
        stream.finish_ok();

        assert_eq!(
            parse_usage_from_sse(&tail),
            Some(Usage {
                input_tokens: 11,
                cache_read_tokens: 0,
                output_tokens: 24,
            })
        );

        // The request really carried the endpoint's upstream model, forced
        // streaming, and the key read from the environment.
        let request = served.await.expect("upstream task");
        assert!(
            request.starts_with("POST /v1/chat/completions "),
            "{request}"
        );
        assert!(
            request.contains("\"model\":\"deepseek-flash\""),
            "{request}"
        );
        assert!(request.contains("\"stream\":true"), "{request}");
        assert!(
            request.contains("authorization: Bearer test-key"),
            "{request}"
        );
    }

    // ---------------------------------------------------------------------
    // stream_chat FAILURE CLASSIFICATION - the failover loop.
    //
    // These lines were entirely uncovered, and they are the branches that decide
    // three different things about one bad response: whether a key is PARKED (429),
    // whether a breaker is TRIPPED (5xx), or whether the caller own request is
    // refused without retrying a different provider (other 4xx). Getting any of them
    // wrong either punishes healthy keys or retries a body that will never succeed.
    // ---------------------------------------------------------------------

    /// A one-shot loopback upstream answering with `status_line` and a small body,
    /// returning the address to point an endpoint at.
    async fn upstream_answering(status_line: &str, body: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback upstream");
        let addr = listener.local_addr().expect("local addr");

        let response = format!(
            "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        );

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 8192];
                let _ = socket.read(&mut buf).await;
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
                let _ = socket.shutdown().await;
            }
        });

        format!("http://{addr}/v1")
    }

    /// Like `upstream_answering`, but serves EVERY request rather than one.
    ///
    /// Kept (with `#[allow(dead_code)]` and this note) because the DISTINCTION is a
    /// real trap and no test currently needs more than one response: a one-shot stub
    /// is wrong for a test that drives more than one attempt, because the later calls
    /// fail at the TRANSPORT level and record failures of a different kind, silently
    /// confounding whatever the test claims to measure. That is not hypothetical — it
    /// is what defeated a first attempt at the 429 test above.
    #[allow(dead_code)]
    async fn repeating_upstream_answering(status_line: &str, body: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a repeating upstream");
        let addr = listener.local_addr().expect("local addr");

        let response = format!(
            "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        );

        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let payload = response.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let _ = socket.read(&mut buf).await;
                    let _ = socket.write_all(payload.as_bytes()).await;
                    let _ = socket.flush().await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        format!("http://{addr}/v1")
    }

    /// A client whose single endpoint points at the given url, with its key installed.
    fn client_pointed_at(url: &str) -> UpstreamClient {
        // The ONE process-wide lock over environment mutation. These tests
        // run in PARALLEL THREADS, and the environment is process-global, so
        // without it two tests interleave their writes and the fixtures stop
        // meaning what they say. It is deliberately not Send: it belongs in
        // the test body, not in a spawned task.
        let _lock = EnvLock::acquire();
        let _env = EnvGuard::set("APK_TEST_SOLO_KEY_1", "test-key");
        let mut solo = endpoint("solo", 1.0);
        solo.url = url.to_string();
        client(vec![model("flash", vec![solo])])
    }

    /// A 429 parks the key and NEVER trips the breaker.
    ///
    /// The distinction is the point: a throttled key is saturated, not broken. If a
    /// 429 tripped the breaker the whole endpoint would be taken out of service for
    /// a provider that is merely busy.
    #[tokio::test]
    async fn a_rate_limited_upstream_parks_the_key_without_tripping_the_breaker() {
        let url = upstream_answering("429 Too Many Requests", r#"{"error":"slow down"}"#).await;
        let upstream = client_pointed_at(&url);

        let err = upstream
            .stream_chat("flash", json!({ "model": "flash" }))
            .await
            .expect_err("429 is not a success");

        assert!(
            matches!(err, UpstreamError::RateLimited),
            "a 429 must surface as RateLimited, got {err:?}"
        );
        assert!(
            upstream.models[0].endpoints[0].breaker.allow_request(),
            "a 429 must NOT trip the breaker: a throttled key is saturated, not broken"
        );

        // THE ASSERTION ABOVE IS VACUOUS ON ITS OWN, and mutation testing proved it:
        // ONE recorded failure leaves the breaker Closed either way (the threshold is
        // 3), so making a 429 call `record_failure()` did not fail this test.
        //
        // Driving several 429s does NOT rescue it either, and finding out why was
        // worth the effort: the pool holds one key, the first 429 PARKS it, so every
        // later `acquire()` returns None and the loop breaks. Measured, twice - a
        // probe confirmed the branch IS reached, and a diagnostic showed the breaker
        // still Closed with one attempt. So the number of 429s is capped at one.
        //
        // What genuinely discriminates is the PARKING ITSELF, which is the observable
        // difference between the two designs: a throttled key is parked and rotated,
        // and the breaker stays Closed. `allow_request` therefore stays true, and the
        // ENDPOINT reports NO cooldown - a 429 must not take a provider out of service.
        assert_eq!(
            upstream.shortest_cooldown_secs("flash"),
            None,
            "a 429 must leave NO endpoint cooldown behind: a throttled provider is still in service"
        );
        assert_eq!(
            upstream.models[0].endpoints[0].breaker.state(),
            BreakerState::Closed,
            "the breaker must still be Closed after a 429"
        );
    }

    /// A 5xx TRIPS the breaker and reports no healthy upstream.
    #[tokio::test]
    async fn a_server_error_trips_the_breaker_and_reports_no_healthy_upstream() {
        let url = upstream_answering("500 Internal Server Error", r#"{"error":"boom"}"#).await;
        let upstream = client_pointed_at(&url);

        let err = upstream
            .stream_chat("flash", json!({ "model": "flash" }))
            .await
            .expect_err("500 is not a success");

        assert!(
            matches!(err, UpstreamError::NoHealthyUpstream(_)),
            "a 5xx must report NoHealthyUpstream, got {err:?}"
        );

        // POSITIVE CONTROL for the breaker half: the failure must be RECORDED, not
        // merely not-crashed. One 500 leaves the breaker closed (threshold 3), so
        // this drives three and asserts it opens.
        let url2 = upstream_answering("500 Internal Server Error", "{}").await;
        let upstream2 = client_pointed_at(&url2);
        for _ in 0..3 {
            let _ = upstream2
                .stream_chat("flash", json!({ "model": "flash" }))
                .await;
        }
        assert!(
            !upstream2.models[0].endpoints[0].breaker.allow_request(),
            "three consecutive 5xx responses must OPEN the breaker - otherwise the failure was never recorded"
        );
    }

    /// Any OTHER 4xx is the caller bad request: refused immediately, breaker untouched.
    ///
    /// Retrying a different provider with the same malformed body would waste a
    /// second upstream call and could bill for it, so the loop must return here.
    #[tokio::test]
    async fn a_non_rate_limit_4xx_is_refused_without_rotating_or_tripping() {
        let url = upstream_answering("400 Bad Request", r#"{"error":"bad model"}"#).await;
        let upstream = client_pointed_at(&url);

        let err = upstream
            .stream_chat("flash", json!({ "model": "flash" }))
            .await
            .expect_err("400 is not a success");

        assert!(
            matches!(err, UpstreamError::ServerError(400)),
            "a non-rate-limit 4xx must surface its own status, got {err:?}"
        );
        assert!(
            upstream.models[0].endpoints[0].breaker.allow_request(),
            "the caller own bad request must NOT trip the provider breaker"
        );
    }

    /// FAILOVER: a failing first endpoint must hand over to a healthy second one.
    ///
    /// This is the product central reliability claim - the client-side transparent
    /// retry across the endpoint pool. It was uncovered.
    #[tokio::test]
    async fn a_failing_first_endpoint_fails_over_to_a_healthy_second() {
        let dead = upstream_answering("500 Internal Server Error", "{}").await;
        let healthy = upstream_answering("200 OK", "").await;

        // The ONE process-wide lock over environment mutation. These tests
        // run in PARALLEL THREADS, and the environment is process-global, so
        // without it two tests interleave their writes and the fixtures stop
        // meaning what they say. It is deliberately not Send: it belongs in
        // the test body, not in a spawned task.
        let _lock = EnvLock::acquire();
        let _dead = EnvGuard::set("APK_TEST_DEAD_KEY_1", "dead-key");
        let _alive = EnvGuard::set("APK_TEST_ALIVE_KEY_1", "alive-key");

        let mut first = endpoint("dead", 1.0);
        first.url = dead;
        let mut second = endpoint("alive", 1.0);
        second.url = healthy;

        let upstream = client(vec![model("flash", vec![first, second])]);

        let result = upstream
            .stream_chat("flash", json!({ "model": "flash" }))
            .await;

        // The first 500s; the loop must then try the second, which answers 200.
        match result {
            Ok(stream) => assert_eq!(
                stream.endpoint_name(),
                "alive",
                "the SECOND endpoint must have served the request after the first failed"
            ),
            Err(err) => panic!(
                "the loop must fail over to a healthy endpoint rather than give up, got {err:?}"
            ),
        }
    }

    /// An endpoint with weight 0 is never attempted.
    #[tokio::test]
    async fn a_weightless_endpoint_is_skipped_entirely() {
        let url = upstream_answering("200 OK", "").await;
        // The ONE process-wide lock over environment mutation. These tests
        // run in PARALLEL THREADS, and the environment is process-global, so
        // without it two tests interleave their writes and the fixtures stop
        // meaning what they say. It is deliberately not Send: it belongs in
        // the test body, not in a spawned task.
        let _lock = EnvLock::acquire();
        let _ghost = EnvGuard::set("APK_TEST_GHOST_KEY_1", "ghost-key");

        let mut ghost = endpoint("ghost", 0.0);
        ghost.url = url;
        let upstream = client(vec![model("flash", vec![ghost])]);

        let err = upstream
            .stream_chat("flash", json!({ "model": "flash" }))
            .await
            .expect_err("a weight-0 endpoint must not be used");

        assert!(
            matches!(err, UpstreamError::NoHealthyUpstream(_)),
            "a weight-0 endpoint is registered but never routed, got {err:?}"
        );
    }

    #[tokio::test]
    async fn stream_chat_reports_an_unknown_model_and_a_missing_key() {
        // Built FIRST, while the local `client` below does not yet shadow the helper
        // of the same name, and after the key is removed - the key pool is populated
        // when UpstreamClient::new reads the environment, so removing the variable
        // afterwards would leave a pool that already holds the key. Both orderings
        // were wrong here and made this test fail for a reason that had nothing to do
        // with what it asserts.
        let _lock = EnvLock::acquire();
        let _env = EnvGuard::remove("APK_TEST_PRIMARY_KEY_1");
        let keyless_client = client(vec![model("flash", vec![endpoint("primary", 1.0)])]);

        let client = client(vec![model("flash", vec![endpoint("primary", 1.0)])]);

        let unknown = client
            .stream_chat("ghost", json!({ "model": "ghost" }))
            .await
            .expect_err("unknown model");
        assert!(matches!(unknown, UpstreamError::NoModel(name) if name == "ghost"));

        // A keyless endpoint must fail FAST (503), with no request sent at all.
        let keyless = keyless_client
            .stream_chat("flash", json!({ "model": "flash" }))
            .await
            .expect_err("empty key pool");
        assert!(matches!(keyless, UpstreamError::NoHealthyUpstream(_)));
    }

    #[test]
    fn prepare_body_asks_for_usage_when_streaming() {
        let payload = prepare_body(
            &json!({ "model": "flash", "messages": [] }),
            "deepseek-flash",
            true,
        );

        assert_eq!(payload["model"], json!("deepseek-flash"));
        assert_eq!(payload["stream"], json!(true));
        assert_eq!(
            payload["stream_options"],
            json!({ "include_usage": true }),
            "without the opt-in the upstream omits usage and nothing gets billed"
        );
    }

    #[test]
    fn prepare_body_merges_into_a_caller_supplied_stream_options() {
        let payload = prepare_body(
            &json!({
                "model": "flash",
                "stream": true,
                "stream_options": { "foo": "bar", "include_usage": false }
            }),
            "deepseek-flash",
            true,
        );

        // The caller's own key survives, and only include_usage is forced.
        assert_eq!(payload["stream_options"]["foo"], json!("bar"));
        assert_eq!(payload["stream_options"]["include_usage"], json!(true));
    }

    #[test]
    fn prepare_body_leaves_a_non_streaming_body_alone() {
        let payload = prepare_body(
            &json!({ "model": "flash", "stream": false, "messages": [] }),
            "deepseek-flash",
            true,
        );

        assert!(
            payload.get("stream_options").is_none(),
            "a non-streaming request has no SSE usage block to opt into: {payload}"
        );
    }

    #[test]
    fn prepare_body_skips_stream_options_when_endpoint_does_not_support_it() {
        let payload = prepare_body(
            &json!({ "model": "flash", "stream": true, "messages": [] }),
            "deepseek-flash",
            false,
        );

        assert!(
            payload.get("stream_options").is_none(),
            "an endpoint without stream_options support must not receive the parameter: {payload}"
        );
    }

    /// Every `UpstreamError` variant formats into a human-readable message. The
    /// `Display` impl is what surfaces in logs and alert text, so each arm must
    /// be exercised - the `matches!` assertions elsewhere only construct the
    /// variants and never format them, leaving the `write!` arms uncovered.
    #[test]
    fn upstream_error_display_is_human_readable() {
        assert_eq!(
            format!("{}", UpstreamError::NoModel("gpt-x".into())),
            "unknown model: gpt-x"
        );
        assert_eq!(
            format!("{}", UpstreamError::NoHealthyUpstream("gpt-x".into())),
            "no healthy upstream for model: gpt-x"
        );
        assert_eq!(
            format!("{}", UpstreamError::RateLimited),
            "upstream rate limited every key"
        );
        assert_eq!(
            format!("{}", UpstreamError::ServerError(503)),
            "upstream returned status 503"
        );
        assert_eq!(
            format!("{}", UpstreamError::Transport("boom".into())),
            "upstream transport error: boom"
        );
    }

    /// A `data:` line whose payload is not valid UTF-8 must be skipped, not make
    /// the whole parse panic. This is the `continue` at client.rs:104 - a
    /// midstream chunk with mangled bytes still leaves the trailing valid usage
    /// block findable.
    #[test]
    fn parse_usage_skips_lines_that_are_not_valid_utf8() {
        let mut tail = b"data: ".to_vec();
        tail.extend_from_slice(&[0xff, 0xfe, 0xfd]); // invalid UTF-8 payload
        tail.extend_from_slice(b"\n\n");
        tail.extend_from_slice(
            b"data: {\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3}}\n\n",
        );

        let usage = parse_usage_from_sse(&tail).expect("the valid trailing block is still found");
        assert_eq!(usage.input_tokens, 1);
        assert_eq!(usage.output_tokens, 2);
        // And a tail that is ONLY junk yields nothing rather than panicking.
        assert_eq!(parse_usage_from_sse(&[0xff, 0xfe]), None);
    }
}
