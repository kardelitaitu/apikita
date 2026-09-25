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
use crate::money::calculate_preflight_reservation_idr;
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
        let cached = nested_int_field(block, "prompt_tokens_details", "cached_tokens").min(prompt);
        usage = Some(Usage {
            input_tokens: prompt - cached,
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
    value.get(object).map(|inner| int_field(inner, key)).unwrap_or(0)
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
    price: f64,
    max_context_tokens: u64,
    input_peak: f64,
    output_peak: f64,
    endpoints: Vec<EndpointEntry>,
}

impl ModelEntry {
    /// Worst-case pre-flight reservation in IDR for a request that may emit
    /// `max_output_tokens`.
    ///
    /// The reservation is taken BEFORE routing and applies whichever endpoint
    /// serves the request, so it must cover the dearest endpoint in the pool —
    /// otherwise a failover to a dearer provider can overdraw the balance
    /// (docs/failover.md). Upstream cost is configured per model today, so the
    /// endpoints of one model currently tie; the maximum is taken regardless so
    /// per-endpoint rates need no change here. The input side is reserved at
    /// the model's full context window, which is the only upper bound this
    /// layer can see.
    fn worst_case_reservation_idr(&self, max_output_tokens: u64) -> i64 {
        let per_endpoint = || {
            calculate_preflight_reservation_idr(
                self.price,
                self.max_context_tokens,
                self.input_peak,
                max_output_tokens,
                self.output_peak,
            )
        };
        self.endpoints
            .iter()
            .map(|_| per_endpoint())
            .max()
            .unwrap_or_else(per_endpoint)
    }
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
            .tcp_nodelay(true)  // SSE: disable Nagle so small frequent frames land immediately
            .pool_idle_timeout(std::time::Duration::from_secs(30))  // 30s: with pool_max_idle_per_host(100) and 3 endpoints the 90s default holds 300 idle sockets on a 256 MB container
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
                price: model.price,
                max_context_tokens: model.max_context_tokens,
                input_peak: model.rates.input_peak,
                output_peak: model.rates.output_peak,
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
        self.models.iter().map(|model| model.name.as_str()).collect()
    }

    /// Worst-case reservation in IDR, or `None` when the model is unknown.
    pub fn worst_case_reservation_idr(&self, model: &str, max_output_tokens: u64) -> Option<i64> {
        self.model(model)
            .map(|entry| entry.worst_case_reservation_idr(max_output_tokens))
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

        for endpoint in entry.endpoints.iter().filter(|endpoint| endpoint.weight > 0.0) {
            if !endpoint.breaker.allow_request() {
                unhealthy = true;
                continue;
            }

            let payload = prepare_body(&body, &endpoint.upstream_model, endpoint.supports_stream_options);

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
                            return Ok(UpstreamStream::new(
                                response,
                                endpoint.name.clone(),
                                lease,
                            ));
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
            .post(format!("{}/chat/completions", base_url.trim_end_matches('/')))
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
        let streaming = object.get("stream").and_then(Value::as_bool).unwrap_or(true);

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
        NetworkConfig, PricingConfig, RealtimeConfig, SessionsConfig, StreamingConfig, WalletConfig,
    };

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
        ModelEndpoint {
            name: name.to_string(),
            url: format!("https://{name}.example.com/v1"),
            upstream_model: "deepseek-flash".to_string(),
            api_key_envs: vec![format!("APK_TEST_{}_KEY_1", name.to_uppercase())],
            concurrency_per_key: 0,
            weight,
            supports_stream_options: true,
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

    #[test]
    fn worst_case_reservation_covers_the_dearest_endpoint_in_the_pool() {
        let client = client(vec![model(
            "flash",
            vec![endpoint("primary", 1.0), endpoint("secondary", 1.0)],
        )]);

        let reserved = client
            .worst_case_reservation_idr("flash", 4096)
            .expect("configured model");

        // Worst case = the dearest endpoint in the pool, at the model's peak
        // rates, over the full context window:
        //   in  1e6/1e6 * 2676.78 = 2676.78
        //   out 4096/1e6 * 10707.12 = 43.8564
        //   (2676.78 + 43.8564) * 1.5 = 4080.954 -> ceil 4081
        assert_eq!(reserved, 4081);
        assert_eq!(
            reserved,
            calculate_preflight_reservation_idr(1.5, 1_000_000, 2676.78, 4096, 10707.12)
        );
        // The reservation covers the whole context window as well as the output
        // budget, so it is strictly dearer than an output-only reservation.
        // (Today's schema prices rates per MODEL, so the endpoints of one model
        // tie; the max over the pool is what makes a dearer endpoint win once
        // per-endpoint rates exist.)
        assert!(reserved > calculate_preflight_reservation_idr(1.5, 0, 2676.78, 4096, 10707.12));

        // A bigger output budget reserves more; an unknown model reserves none.
        assert!(client.worst_case_reservation_idr("flash", 8192).unwrap() > reserved);
        assert_eq!(client.worst_case_reservation_idr("ghost", 4096), None);
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
                    .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length: ").map(str::to_string))
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
            socket.write_all(response.as_bytes()).await.expect("write head");
            socket.write_all(&body).await.expect("write body");
            socket.flush().await.expect("flush");
            String::from_utf8_lossy(&buffer).into_owned()
        });

        // The key comes from the environment variable the endpoint names.
        std::env::set_var("APK_TEST_LIVE_KEY_1", "test-key");
        let mut live = endpoint("live", 1.0);
        live.url = format!("http://{addr}/v1");
        let client = client(vec![model("flash", vec![live])]);

        let mut stream = client
            .stream_chat("flash", json!({ "model": "flash", "messages": [], "stream": true }))
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
        assert!(request.contains("\"model\":\"deepseek-flash\""), "{request}");
        assert!(request.contains("\"stream\":true"), "{request}");
        assert!(request.contains("authorization: Bearer test-key"), "{request}");
    }

    #[tokio::test]
    async fn stream_chat_reports_an_unknown_model_and_a_missing_key() {
        let client = client(vec![model("flash", vec![endpoint("primary", 1.0)])]);

        let unknown = client
            .stream_chat("ghost", json!({ "model": "ghost" }))
            .await
            .expect_err("unknown model");
        assert!(matches!(unknown, UpstreamError::NoModel(name) if name == "ghost"));

        // No APK_TEST_PRIMARY_KEY_1 in the environment: the pool is empty, so
        // there is nothing to send with and the request fails fast (503).
        std::env::remove_var("APK_TEST_PRIMARY_KEY_1");
        let keyless = client
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
}
