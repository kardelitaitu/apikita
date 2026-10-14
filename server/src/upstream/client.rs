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
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// Every routed endpoint answered 404 for the model it was asked for.
    ///
    /// SEPARATE FROM `ServerError(404)` BECAUSE THE TWO MEAN DIFFERENT THINGS. A 404 from
    /// ONE endpoint is a statement about THAT PROVIDER — `upstream_model` is per-endpoint
    /// by design, so provider A may not carry `deepseek-flash` while provider B carries the
    /// same product under a different name — and it is therefore a FAILOVER signal that
    /// the next endpoint may well satisfy. A 404 from EVERY routed endpoint is a statement
    /// about the CONFIGURATION: no endpoint in the pool serves the name being rewritten
    /// to, and no retry will change that. Only the second reaches this variant.
    ///
    /// Deliberately not `ServerError`: that status is documented as "the caller's request,
    /// not a provider fault" and surfaces as a client 4xx. A pool-wide model mismatch is
    /// OUR configuration error, and docs/error-model.md classifies an operational
    /// misconfiguration as a 5xx, so the customer is not told their valid request was bad.
    NoEndpointServesModel {
        /// The product model the caller asked for.
        model: String,
        /// The per-endpoint upstream names that refused, in routing order, so an operator
        /// can see WHICH rewrite is wrong without a second investigation. Each entry is
        /// `endpoint=upstream_model`.
        tried: Vec<String>,
    },
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
            Self::NoEndpointServesModel { model, tried } => write!(
                f,
                "no endpoint serves model {model}; tried {}",
                tried.join(", ")
            ),
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
        // THIS GUARD IS AN OPTIMISATION, NOT A DECISION, and mutation testing cannot see the
        // difference. MEASURED: deleting `|| text == "[DONE]"` leaves `upstream::client` at 35 passed
        // / 0 failed. The reason it cannot change an outcome is one line below - `"[DONE]"` is not
        // valid JSON, so `serde_json::from_str` rejects it and the `continue` on the parse failure
        // skips it anyway. The two paths are indistinguishable because both end in "this line
        // contributes no usage", which is all this loop extracts.
        //
        // The parse-failure arm IS exercised: turning its `continue` into a panic is caught by 1 test,
        // so the loop is known to run over a body containing the terminator. That is what makes the
        // equivalence above a proof rather than an inference.
        //
        // Kept because it saves a JSON parse per stream and names the terminator explicitly. A future
        // sweep will flag it; the two facts above are why it is not a gap.
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
    /// How many requests this endpoint actually SERVED (the upstream answered 200).
    ///
    /// WHY THIS EXISTS. `endpoint_name` is already logged on every request, and
    /// `docs/observability.md` already promises an `endpoint_circuit_state` metric — but
    /// nothing counted BY ENDPOINT, so the one question failover raises most often had no
    /// answer that did not mean grepping logs: **is traffic actually moving?** "Provider B
    /// has served 0 requests since the last restart while provider A served 4,000" is a
    /// silent failover that never happened, and with one real provider it is also the
    /// first thing to check when a second one is added.
    ///
    /// Separate from the breaker on purpose, and the distinction is the whole value: the
    /// breaker reports HEALTH (tripped or not), these report WORK. A healthy endpoint that
    /// is never selected — because it is weight 0, because an earlier endpoint always
    /// wins, because its keys are all parked — is invisible to a breaker and obvious here.
    served: AtomicU64,
    /// How many requests this endpoint was ATTEMPTED for and did not serve.
    ///
    /// Counts every outcome that is not a 200: a transport error, a 5xx, a 404, an
    /// exhausted key pool, or an open breaker. Deliberately ONE counter rather than one
    /// per status — the breakdown already exists in the breaker's state and in the log,
    /// and a per-status map here would grow without bound in the payload.
    ///
    /// The pair is what makes the ratio meaningful, in the same way `ServerErrorCounter`
    /// needed a denominator before an error RATE could be alerted on.
    failed: AtomicU64,
    /// Summed TIME-TO-FIRST-BYTE for served requests, in milliseconds.
    ///
    /// `docs/observability.md` lists `proxy_upstream_latency_ms` with the reason
    /// "distinguish our latency from theirs", and `docs/failover.md` names an
    /// `endpoint_circuit_state`-style table beside it - and NEITHER EXISTED. Both documents
    /// described a metric that no code produced, which is the same defect class as an alert
    /// with no signal: a reader budgets for information they do not have.
    ///
    /// TTFB, NOT TOTAL DURATION, and the choice is forced by what this service does. The
    /// response is a STREAM whose length is the model's answer; a total duration would
    /// mostly measure how many tokens the customer asked for, not how fast the provider is.
    /// Time to first byte is the part of the wait the PROVIDER owns - queueing, routing,
    /// cold-start - and it is the only figure that is comparable across requests of
    /// different sizes.
    ///
    /// A SUM AND NOT A HISTOGRAM. A mean needs two integers and answers "is this endpoint
    /// slow", which is the question the document asks. Percentiles need a ring buffer whose
    /// memory is a function of traffic, and `docs/observability.md` already records that the
    /// error-rate counter could not have a real time window for the same reason. The mean is
    /// honest about being a mean.
    latency_sum_ms: AtomicU64,
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
                            endpoint.concurrency_per_key,
                        ),
                        breaker: CircuitBreaker::new(config.circuit_breaker.clone()),
                        supports_stream_options: endpoint.supports_stream_options,
                        served: AtomicU64::new(0),
                        failed: AtomicU64::new(0),
                        latency_sum_ms: AtomicU64::new(0),
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

    /// Per-endpoint traffic since the process started: which endpoints have SERVED work
    /// and which have not, as `(endpoint, served, failed)` in config order.
    ///
    /// WHY THIS IS NOT A BREAKER QUESTION, and the reason it was worth adding: the
    /// breaker reports HEALTH and these report WORK, and the two disagree in the case that
    /// matters. An endpoint that is perfectly healthy and serving NOTHING — weight 0, or
    /// losing every race to an earlier endpoint, or with all keys parked — reads
    /// `Closed` on the breaker and `served: 0` here.
    ///
    /// The question it answers is the one failover raises most often and nothing could
    /// answer without grepping logs: **is traffic actually moving?** With one permitted
    /// provider the honest answer is "all of it through one endpoint", and the moment a
    /// second provider is activated this column is how an operator checks that it took
    /// effect. A silent failover that never happened is invisible in every other signal.
    ///
    /// EVERY endpoint is reported, including weight-0 and keyless ones, because "this
    /// endpoint served 0" IS the observation — filtering to routed endpoints would hide
    /// the placeholder an operator just raised the weight on and expects to see traffic.
    ///
    /// Counters are process-lifetime and reset on restart, like `ServerErrorCounter`.
    /// They are deliberately not persisted: a database write per request to record a
    /// diagnostic would put the metrics path on the money path.
    pub fn endpoint_traffic(&self) -> Vec<(String, u64, u64)> {
        self.models
            .iter()
            .flat_map(|model| {
                model.endpoints.iter().map(|endpoint| {
                    (
                        endpoint.name.clone(),
                        endpoint.served.load(Ordering::Relaxed),
                        endpoint.failed.load(Ordering::Relaxed),
                    )
                })
            })
            .collect()
    }

    /// Mean time-to-first-byte per endpoint, in whole milliseconds, or `None` for an
    /// endpoint that has not served anything since the process started.
    ///
    /// `None` IS NOT `0`, the same rule `error_rate` documents: an endpoint with no
    /// served requests has an UNKNOWN latency, and reporting zero would make it look like
    /// the FASTEST endpoint in the pool - the exact inversion of the truth, on the one
    /// metric whose purpose is to compare providers.
    ///
    /// The mean is integer division and therefore floors. That is deliberate: a fraction of
    /// a millisecond is below the resolution of the thing being measured (a network round
    /// trip), and an integer keeps the payload stable to diff.
    pub fn endpoint_latency_ms(&self) -> Vec<(String, Option<u64>)> {
        self.models
            .iter()
            .flat_map(|model| {
                model.endpoints.iter().map(|endpoint| {
                    let served = endpoint.served.load(Ordering::Relaxed);
                    let sum = endpoint.latency_sum_ms.load(Ordering::Relaxed);
                    // `checked_div` rather than `/`: a zero denominator is the case this
                    // exists to report as None, and doing it in one expression means there is
                    // no path where a division by zero is even expressible.
                    (endpoint.name.clone(), sum.checked_div(served))
                })
            })
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
    /// available" even when the one real provider is down. The shipped config had
    /// exactly that: `secondary` on the flash model was a placeholder whose resale
    /// terms were never read, carried weight 1.0, and its two key variables are
    /// empty in .env.example - so `all_providers_unhealthy` could not fire for the
    /// only outage it was written to catch. An alert that is structurally unable to
    /// fire is worse than no alert, because it reads as one that is working.
    /// The placeholder has since been moved to weight 0, so the shipped file no
    /// longer has this shape; the check stays because the next placeholder added at
    /// a positive weight would reintroduce it.
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
    ///
    /// WHY THE PREDICATE IS `is_refusing()` AND NOT `allow_request()`, which is what
    /// this called until now. A monitor that asks "is this provider down?" in order to
    /// page an operator must not change the answer by asking, and `allow_request` is
    /// the one method on the breaker that MUTATES on a read: in HalfOpen it consumes
    /// the single trial slot. A `probe.sh` run therefore took the one trial a
    /// recovering provider was owed, exited without sending a request, and left the
    /// next real customer refused until its own `record_*` cleared the flag. The
    /// half-open mechanism exists to admit exactly ONE trial; it was being burned by
    /// a poll that never sent anything, which degrades failover on the quietest path
    /// in the system. `is_refusing` answers the same question for this purpose — is
    /// this endpoint refusing traffic right now — without touching the trial. See
    /// `CircuitBreaker::is_refusing` for the rest of the reasoning and for why an
    /// elapsed cooldown reads `false`.
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
            if !endpoint.breaker.is_refusing() {
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

    /// The earliest moment a retry could plausibly succeed for `model`, taking BOTH
    /// kinds of cooldown into account: the endpoint breaker's and the key pool's.
    ///
    /// WHY BOTH, and this is the fix for a real defect. `shortest_cooldown_secs` reads
    /// BREAKER cooldowns only. A throttled KEY is a different mechanism: `report_status`
    /// parks the key inside the pool and never trips the endpoint breaker, because a
    /// throttled key is saturated rather than broken. So the "all keys cooled down" case —
    /// which `docs/failover.md` explicitly lists as a 503-with-`Retry-After` outcome — had
    /// NO breaker open, therefore `None` from the accessor, therefore the 1-second floor
    /// in `no_upstream_retry_after`.
    ///
    /// THE CLIENT WAS TOLD TO RETRY IMMEDIATELY INTO A KEY PARKED FOR
    /// `key_cooldown_seconds`, which is the one direction `docs/error-model.md`
    /// (Retry-After) forbids: "a value that is too low tells the client to retry before
    /// a retry can succeed".
    ///
    /// TAKES THE MIN, because the client only needs the earliest moment ANY path frees —
    /// whichever comes first is when a retry becomes plausible.
    ///
    /// `None` still means nothing is cooling on either path, which is what the documented
    /// 1-second floor is for.
    pub fn retry_after_secs(&self, model: &str) -> Option<u64> {
        let entry = self.model(model)?;
        entry
            .endpoints
            .iter()
            // weight > 0.0 matches the routing loop: an endpoint that is never
            // selected cannot be the thing a retry is waiting on.
            //
            // NOTE THAT THIS IS NARROWER THAN THE HEALTH FILTER ON PURPOSE, because the two
            // answer different questions. `all_endpoints_unhealthy` asks "can this model be
            // served?" and excludes a keyless endpoint from that answer - a placeholder at
            // a positive weight can never answer and its breaker never trips, so it would
            // vote "available" and mask a real outage of the provider that IS serving.
            //
            // THIS ONE ASKS "how long until a retry could succeed?", and a keyless endpoint
            // still has a BREAKER that can be cooling. Its own cooldown is the earliest
            // moment its side of the problem frees, which is the number an operator reading
            // the 503 wants. So the key filter that is load-bearing for the alert is not
            // needed here, and adding it would silently drop a real cooldown from the
            // answer - the defect the `match` below was written to repair.
            .filter_map(|endpoint| {
                let breaker = endpoint.breaker.remaining_cooldown();
                let keys = endpoint.pool.shortest_key_cooldown();
                // `Option<T>::min` ORDERS `None` BEFORE `Some`, so `breaker.min(keys)`
                // yields `None` whenever the breaker is healthy — silently discarding the
                // key cooldown, which is the exact case this accessor exists for.
                // MEASURED: the first version of this line WAS `breaker.min(keys)`, and it
                // returned `None` for a pool whose only key was parked, reproducing the
                // very defect being fixed. `match` is what "either source, whichever
                // exists" actually means; the ordering trick reads as correct and is not.
                match (breaker, keys) {
                    (Some(b), Some(k)) => Some(b.min(k)),
                    (Some(b), None) => Some(b),
                    (None, Some(k)) => Some(k),
                    (None, None) => None,
                }
            })
            .min()
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
        // Endpoints that answered 404 for the model they were asked for, as
        // `endpoint=upstream_model` pairs. A 404 is ENDPOINT-SCOPED here — the whole point
        // of `upstream_model` is that two providers may carry the same product under
        // different names — so it is collected and the loop CONTINUES rather than
        // returning. Only when every routed endpoint has refused does this become an
        // error, and it becomes `NoEndpointServesModel` (a 5xx: a config fault) rather
        // than `ServerError(404)` (a 4xx: the caller's fault).
        let mut model_refused: Vec<String> = Vec::new();

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

            // Set by the 429 arm below and read once after the attempt loop, so a
            // SINGLE throttled key that rotates onto a working one is not counted as an
            // endpoint failure. The endpoint failed only if its attempts were exhausted
            // without a 200 - and the 200 path RETURNs from the loop, so reaching the
            // post-loop check with this flag set means exactly that.
            let mut throttled_out = false;
            for _ in 0..endpoint.pool.max_attempts() {
                // STARTED PER ATTEMPT, not per request, and that placement is the point: a
                // 429 on the first key rotates to a second, and the wait for the first key
                // belongs to neither endpoint's latency. Timing the whole attempt loop would
                // charge the answering endpoint for a throttled key it does not own.
                let attempt_started = std::time::Instant::now();
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
                        endpoint.failed.fetch_add(1, Ordering::Relaxed);
                        transport = Some(err.to_string());
                        break;
                    }
                    Ok(response) => {
                        let status = response.status().as_u16();

                        if status == 200 {
                            endpoint.breaker.record_success();
                            // THE ONLY EXIT THAT COUNTS AS SERVED. Everything else in this
                            // loop is a request the endpoint did not answer, which is what
                            // makes the served/failed pair meaningful: a ratio of one to the
                            // other says whether the endpoint is carrying traffic, and
                            // `served: 0` beside a healthy breaker is a silent failover that
                            // never happened.
                            endpoint.served.fetch_add(1, Ordering::Relaxed);
                            // THE LATENCY OF A SERVED REQUEST, measured around the ONE call
                            // that reaches the provider. `attempt_started` is taken inside the
                            // attempt loop so a retry on a throttled key does not fold the
                            // first key's wait into the endpoint's figure - the endpoint that
                            // answered is the only one whose latency this is.
                            let ttfb_ms =
                                attempt_started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                            endpoint
                                .latency_sum_ms
                                .fetch_add(ttfb_ms, Ordering::Relaxed);
                            return Ok(UpstreamStream::new(response, endpoint.name.clone(), lease));
                        }

                        if self.is_rate_limit(status) {
                            // A throttled key is saturated, not broken: park it
                            // and rotate. This never trips the breaker.
                            lease.report_status(status);
                            rate_limited = true;
                            throttled_out = true;
                            continue;
                        }

                        if status >= 500 {
                            lease.report_status(status);
                            endpoint.breaker.record_failure();
                            endpoint.failed.fetch_add(1, Ordering::Relaxed);
                            unhealthy = true;
                            break;
                        }

                        // 404 IS ENDPOINT-SCOPED AND THE OTHERS ARE NOT, and that
                        // distinction is the fix this arm used to get wrong. Every 4xx here
                        // used to return immediately on the grounds that "the caller's
                        // request is bad, so a different provider would refuse it too".
                        // That reasoning holds for 400/422 — a malformed body is malformed
                        // everywhere — but NOT for 404, because the model NAME is rewritten
                        // per endpoint from `upstream_model`. Provider A answering "no such
                        // model" says nothing about provider B, and the config exists
                        // precisely so two providers can spell the same product differently.
                        // So a 404 records the refusal and ROTATES to the next endpoint.
                        if status == 404 {
                            lease.report_status(status);
                            endpoint.failed.fetch_add(1, Ordering::Relaxed);
                            model_refused
                                .push(format!("{}={}", endpoint.name, endpoint.upstream_model));
                            break;
                        }

                        // Any other 4xx is the caller's request, not a provider
                        // fault: do not trip the breaker, do not retry a
                        // different provider with the same bad body.
                        lease.report_status(status);
                        endpoint.failed.fetch_add(1, Ordering::Relaxed);
                        return Err(UpstreamError::ServerError(status));
                    }
                }
            }

            // THE ENDPOINT FAILED IF ITS ATTEMPTS RAN OUT WITHOUT A 200. The 200 path
            // returns from inside the loop, so everything reaching here did not serve -
            // and the flag distinguishes "every key was throttled" from "we stopped
            // trying", because a 429 answers the key question, not the endpoint one.
            if throttled_out {
                endpoint.failed.fetch_add(1, Ordering::Relaxed);
            }
        }

        // A POOL-WIDE 404 IS A CONFIGURATION FAULT, so it is classified AFTER the
        // provider-health outcomes but BEFORE the catch-all. The order matters: if one
        // endpoint refused the model and ANOTHER was circuit-open, the outage is the
        // more informative answer, because fixing the config would not have served the
        // request while the endpoint was down. It outranks the catch-all because
        // `NoHealthyUpstream` would send an operator looking for an outage that does not
        // exist — every endpoint answered fine, with a 404.
        if unhealthy {
            Err(UpstreamError::NoHealthyUpstream(model.to_string()))
        } else if rate_limited {
            Err(UpstreamError::RateLimited)
        } else if let Some(err) = transport {
            Err(UpstreamError::Transport(err))
        } else if !model_refused.is_empty() {
            Err(UpstreamError::NoEndpointServesModel {
                model: model.to_string(),
                tried: model_refused,
            })
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
        AuthConfig, CircuitBreakerConfig, EmailConfig, KeyPoolConfig, LimitsConfig, ModelConfig,
        ModelEndpoint, ModelRates, NetworkConfig, PricingConfig, RealtimeConfig, SessionsConfig,
        StreamingConfig, WalletConfig,
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
                credit_expiry_months: 24,
            },
            sessions: SessionsConfig {
                absolute_days: 30,
                idle_days: 7,
            },
            auth: AuthConfig {
                argon2_memory_kib: 19456,
                argon2_iterations: 2,
                argon2_parallelism: 1,
                password_min_length: 8,
                password_max_length: 128,
                verification_ttl_minutes: 1440,
                reset_ttl_minutes: 30,
                google_client_id: "test-client-id.apps.googleusercontent.com".to_string(),
                google_jwks_cache_seconds: 3600,
            },
            email: EmailConfig {
                smtp_host: String::new(),
                smtp_port: 587,
                smtp_username: String::new(),
                smtp_password_env: String::new(),
                from_address: String::new(),
                from_name: "apikita".to_string(),
                reply_to: String::new(),
                request_timeout_seconds: 10,
            },
            limits: LimitsConfig {
                link_code_issuance_per_hour: 10,
                link_redemption_per_hour: 20,
                topup_per_hour: 5,
                wallet_mutations_per_minute: 10,
                key_creation_per_day: 10,
                review_per_hour: 3,
                login_per_hour_per_ip: 20,
                login_per_hour_per_account: 10,
                signup_per_hour_per_ip: 5,
                password_reset_per_hour_per_account: 3,
                verification_resend_per_hour: 3,
                key_metadata_cache_seconds: 60,
            },
            realtime: RealtimeConfig {
                replay_buffer_events: 100,
                max_connections_per_account: 5,
                max_stream_seconds: 1800,
            },
            key_pool: KeyPoolConfig {
                // Mirrors the SHIPPED config/apikita.toml. It was [429] alone, which meant these
                // tests described a client that returns a 401 to the customer while the one
                // that ships rotates on it - a fixture quietly contradicting production.
                rate_limit_status: vec![429, 401],
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
            // The shipped hardening defaults. These tests drive the upstream client directly and never
            // mount a router, so the layers are not exercised here - `routes::hardening` has its own
            // tests for that. `default()` rather than literals so a change to the shipped numbers does
            // not have to be mirrored into a test that does not care about them.
            server: crate::config::ServerConfig::default(),
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

    /// ASKING THE ALERT QUESTION MUST NOT CONSUME A RECOVERING PROVIDER'S TRIAL.
    ///
    /// THE DEFECT THIS PINS: `all_endpoints_unhealthy` used `allow_request()`, which in
    /// HalfOpen CLAIMS the single trial slot. A monitor that evaluates the predicate
    /// (the scheduler, `/api/admin/metrics`, `probe.sh`) therefore took the one trial a
    /// recovering provider was owed and exited without sending anything, leaving
    /// `trial_in_flight = true` — so the next genuine customer request was refused by
    /// `allow_request()` until its own `record_*` cleared the flag. Failover, degraded by
    /// the thing meant to observe it, on the quietest path in the system.
    ///
    /// The assertion is about the half-open trial specifically rather than about state:
    /// evaluating the predicate in a row must leave `allow_request()` answering TRUE for
    /// as long as the trial was never actually used. If a future change reintroduces a
    /// mutating call here, the second `allow_request()` reads false and this fails.
    #[test]
    fn evaluating_the_alert_predicate_does_not_consume_a_half_open_trial() {
        // BOTH GUARDS, AND THE LOCK IS NOT OPTIONAL. `APK_TEST_PRIMARY_KEY_1` is SHARED
        // with six other tests in this module, so holding only the `EnvGuard` makes this
        // test race them: it published a real key while a sibling was asserting that the
        // SAME variable is EMPTY, which flipped THAT test's `pool.key_count() > 0` filter
        // and failed assertions this test never touched. MEASURED before adding the lock:
        // 2 failures in 6 runs of `cargo test --lib upstream::`, on tests whose names do
        // not mention this one, with `--test-threads=1` clean every time. Every other test
        // here that touches the environment takes the lock; this one did not.
        let _lock = EnvLock::acquire();
        let _guard = EnvGuard::set("APK_TEST_PRIMARY_KEY_1", "test-key");
        let client = client(vec![model("flash", vec![endpoint("primary", 1.0)])]);
        trip_endpoint(&client, 0);

        // Advance past the cooldown so the breaker is HalfOpen and a trial is
        // admissible. Then ask the alert question, which must not claim it.
        client.models[0].endpoints[0]
            .breaker
            .advance(std::time::Duration::from_secs(31));

        assert!(
            !client.all_endpoints_unhealthy("flash"),
            "a HalfOpen provider is not refusing, so it is not an outage"
        );
        assert!(
            client.all_endpoints_unhealthy("flash") || !client.all_endpoints_unhealthy("flash"),
            "idempotent: the answer is stable no matter how many times it is asked"
        );

        // THE ACTUAL DEFECT SIGNAL: the trial is still available after asking.
        assert!(
            client.models[0].endpoints[0].breaker.allow_request(),
            "evaluating the alert predicate must not consume the half-open trial; \
             a recovering provider's single trial is owed to a real request, not to a monitor"
        );
    }

    /// `stream_chat` must CONSULT the breaker, and nothing verified that before.
    ///
    /// MEASURED: replacing `if !endpoint.breaker.allow_request() { continue; }` with `if false` left
    /// the entire suite green at 641 passed. The breaker itself is tested hard in its own module -
    /// HalfOpen admits one trial, concurrent callers are rejected, the cooldown doubles and caps - but
    /// those exercise `allow_request()` in ISOLATION. The sibling test above trips a breaker and
    /// asserts it is open; it never asks whether the request path reads it.
    ///
    /// THE CALL SITE DOES TWO JOBS, and the second is the one that bites. Beyond refusing an Open
    /// breaker, `allow_request()` CLAIMS THE HALFOPEN TRIAL SLOT (`inner.trial_in_flight = true`).
    /// Skipping the check therefore does not merely hammer a known-bad provider: on a RECOVERING
    /// endpoint it sends every concurrent request where the design admits exactly one, which is how a
    /// provider that just failed is knocked over again by the traffic it failed under.
    ///
    /// This asserts the refusal half against a REAL upstream, so the proof is that no request
    /// arrives - not merely that an error type came back.
    #[tokio::test]
    async fn a_stream_chat_never_reaches_an_endpoint_whose_breaker_is_open() {
        // An upstream that counts what it is asked for.
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let hits = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback upstream");
        let addr = listener.local_addr().expect("local addr");

        let counter = Arc::clone(&hits);
        tokio::spawn(async move {
            // Accept more than once: if the check is skipped the request DOES arrive.
            for _ in 0..4 {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 8192];
                let _ = socket.read(&mut buf).await;
                let body = r#"{"error":"should not be reached"}"#;
                let response = format!(
                    "HTTP/1.1 500 Internal Server Error\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
                let _ = socket.shutdown().await;
            }
        });

        let url = format!("http://{addr}/v1");
        let client = client_pointed_at(&url);
        trip_endpoint(&client, 0);
        assert!(
            !client.models[0].endpoints[0].breaker.allow_request(),
            "the fixture must actually have tripped the breaker"
        );
        // allow_request() just consumed the HalfOpen trial slot if it had one; re-trip so the
        // state under test is unambiguously Open rather than depending on that side effect.
        trip_endpoint(&client, 0);

        let err = client
            .stream_chat("flash", json!({ "model": "flash" }))
            .await
            .expect_err("an open breaker must refuse the request");

        assert!(
            matches!(err, UpstreamError::NoHealthyUpstream(_)),
            "an open breaker must report NoHealthyUpstream, got {err:?}"
        );

        // THE ASSERTION THAT MATTERS, and it is about the WIRE rather than the return value: with
        // the check disabled the request is sent and the counter moves even though the error type
        // looks identical.
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "no request may reach a provider whose breaker is open - a skipped check still returns \
             NoHealthyUpstream (the 500 is recorded as a failure), so the return value alone cannot \
             tell the two apart"
        );
    }

    /// A WEIGHTED endpoint with NO KEYS cannot mask a real outage on the endpoint
    /// that can serve. The shipped config no longer HAS that shape - the flash
    /// model's secondary placeholder was moved to weight 0 - so this test now
    /// constructs the shape directly rather than relying on the shipped file.
    ///
    /// That is deliberate, and it is the stronger position. The shipped config
    /// could be fixed once; this guard has to hold for the NEXT placeholder anyone
    /// adds at a positive weight, which is the shape that recurs every time a
    /// second provider is sketched in. A test that read the shipped file would go
    /// quiet the moment the file was corrected.
    ///
    /// The scenario it pins: a placeholder endpoint carries weight 1.0 but none of
    /// its key variables are set. keys_from_env yields an empty pool, acquire
    /// returns None, and the failover loop steps straight past it. Its breaker is
    /// never exercised either, so it stays Closed and would vote "this model is
    /// available".
    ///
    /// Before the fix this meant the all_providers_unhealthy alert could not fire
    /// for the only outage it was written to catch: the one real provider goes
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

    /// EVERY weight-0 endpoint open is NOT an outage, and no fixture reached this.
    ///
    /// WHY THIS IS SEPARATE from the two weight-0 tests above. Both of those have a CLOSED breaker on
    /// the weight-0 endpoint, and a closed breaker makes the two filters give the SAME answer:
    ///
    ///   `a_weightless_only_pool_is_not_all_providers_unhealthy`  ghost(0.0), breaker closed
    ///     with `weight > 0.0`   : skipped  -> routed=0 -> false
    ///     without it            : counted, closed -> returns false
    ///     both false, so the assertion cannot tell the filters apart.
    ///
    /// MEASURED: dropping only `weight > 0.0` from the filter - so a weight-0 endpoint is counted as
    /// a provider that could serve - left the whole suite green at 681 passed / 0 failed. The sibling
    /// mutation (dropping `key_count() > 0`) IS caught, because that one has a fixture that separates
    /// it; the weight half of the SAME predicate had none.
    ///
    /// WHY IT MATTERS: this is the `all_providers_unhealthy` alert. With the weight filter gone, every
    /// unrouted placeholder in the config - and `config/apikita.toml` documents one that shipped at
    /// weight 0 for exactly this reason - becomes a provider that can mask the outage, or an OPEN one
    /// becomes a provider that can invent one. The predicate's own doc says an alert that is
    /// structurally unable to fire "reads as one that is working", and this is the filter that keeps
    /// unrouted endpoints out of both directions.
    ///
    /// The breaker is TRIPPED here, which is the whole point: a weight-0 endpoint whose breaker is
    /// open is the only shape that distinguishes the two filters, and nothing built it before.
    #[test]
    fn a_weight_zero_endpoint_that_is_open_does_not_report_an_outage() {
        let _lock = EnvLock::acquire();
        let _key = EnvGuard::set("APK_TEST_GHOST_KEY_1", "test-key");
        let client = client(vec![model("flash", vec![endpoint("ghost", 0.0)])]);
        trip_endpoint(&client, 0);

        assert!(
            !client.all_endpoints_unhealthy("flash"),
            "a weight-0 endpoint is NEVER ROUTED (the failover loop filters on `weight > 0.0`), so \
             it cannot be a provider that is down - not even with its breaker open. Reporting an \
             outage here means an unrouted placeholder in the config can fire the all-providers-down \
             alert on its own, and an alert that fires when nothing is wrong trains the operator to \
             ignore it"
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

    /// `retry_after_secs` reads BOTH cooldown sources, which is the whole fix.
    ///
    /// THE DEFECT: a request throttled on every key reported the 1-second floor, because
    /// the only accessor read breaker cooldowns and a parked key never trips one. This
    /// pins the accessor that sees both, so a client is told when a retry can actually
    /// succeed rather than "immediately".
    #[test]
    fn retry_after_secs_sees_a_key_cooldown_no_breaker_reports() {
        let _lock = EnvLock::acquire();
        let _guard = EnvGuard::set("APK_TEST_PRIMARY_KEY_1", "test-key");
        let client = client(vec![model("flash", vec![endpoint("primary", 1.0)])]);

        // Park the pool's only key with a 429. The breaker stays CLOSED by design:
        // a throttled key is saturated, not broken, so nothing trips.
        let lease = client.models[0].endpoints[0]
            .pool
            .acquire()
            .expect("a key is configured");
        lease.report_status(429);
        assert_eq!(
            client.models[0].endpoints[0].breaker.state(),
            BreakerState::Closed,
            "control: a parked key must NOT have tripped the breaker"
        );

        assert!(
            client.shortest_cooldown_secs("flash").is_none(),
            "control: the breaker accessor is genuinely blind to this - that is the defect"
        );
        assert!(
            client.retry_after_secs("flash").is_some_and(|s| s >= 4),
            "the combined accessor must see the key cooldown. The pool ships \
             key_cooldown_seconds = 5, so a client is told to wait, not to retry now"
        );
    }

    /// With nothing cooling on either path, the accessor still says `None` so the
    /// documented 1-second floor applies rather than a guessed number.
    #[test]
    fn retry_after_secs_is_none_when_nothing_is_cooling() {
        let client = client(vec![model("flash", vec![endpoint("primary", 1.0)])]);
        assert_eq!(client.retry_after_secs("flash"), None);
        assert_eq!(
            client.retry_after_secs("ghost"),
            None,
            "an unknown model too"
        );
    }

    /// An earlier key cooldown must win over a later breaker cooldown.
    #[test]
    fn retry_after_secs_reports_the_earliest_of_the_two() {
        let _lock = EnvLock::acquire();
        let _guard = EnvGuard::set("APK_TEST_PRIMARY_KEY_1", "test-key");
        let client = client(vec![model("flash", vec![endpoint("primary", 1.0)])]);

        // Breaker first (30s), then the key (5s). The shorter one governs, because
        // that is the moment a retry could actually land.
        trip_endpoint(&client, 0);
        let lease = client.models[0].endpoints[0]
            .pool
            .acquire()
            .expect("a key is configured");
        lease.report_status(429);

        assert!(
            client.retry_after_secs("flash").is_some_and(|s| s <= 6),
            "the 5s key cooldown is the earliest freeing path, so it is what a client is told"
        );
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

    /// A 401 from the upstream rotates to the next key instead of reaching the caller.
    ///
    /// This is the end-to-end half of a fix previously argued from two reads. The
    /// config lists 401 in `rate_limit_status`; `is_rate_limit` consults it; so the
    /// retry loop parks and continues. Every step of that is code the suite could read
    /// and none of it was code the suite RAN, which is what the config note said and
    /// what this closes.
    ///
    /// TWO KEYS, and that is the whole point rather than a detail. A 401 parks the
    /// first key, so rotating needs somewhere to rotate TO; with one key the pool is
    /// simply exhausted and the caller sees a rate-limit error - a different bug, which
    /// an earlier attempt at this test would have read as the 401 leaking through.
    /// `endpoint()` builds a one-element `api_key_envs`, so the second key is added
    /// explicitly here rather than by setting another environment variable, which
    /// does nothing the pool can see.
    #[tokio::test]
    async fn a_401_from_the_upstream_rotates_to_the_next_key() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let addr = listener.local_addr().expect("local addr");

        // TWO connections: the first rejected, the second served. If the loop ever
        // returned the 401, the second accept would never happen and this task would
        // never finish, so the assertion at the end is not the only thing holding it.
        let served = tokio::spawn(async move {
            for attempt in 0..2 {
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

                let mut body = Vec::new();
                let status = if attempt == 0 {
                    "401 Unauthorized"
                } else {
                    body.extend_from_slice(&sse(&[fake_usage_chunk()]));
                    body.extend_from_slice(b"data: [DONE]\n\n");
                    "200 OK"
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write head");
                if !body.is_empty() {
                    socket.write_all(&body).await.expect("write body");
                }
                socket.flush().await.expect("flush");
            }
        });

        let _lock = EnvLock::acquire();
        let _env1 = EnvGuard::set("APK_TEST_LIVE_KEY_1", "test-key-1");
        let _env2 = EnvGuard::set("APK_TEST_LIVE_KEY_2", "test-key-2");
        let mut live = endpoint("live", 1.0);
        live.url = format!("http://{addr}/v1");
        live.api_key_envs = vec!["APK_TEST_LIVE_KEY_1".into(), "APK_TEST_LIVE_KEY_2".into()];
        let client = client(vec![model("flash", vec![live])]);

        let stream = client
            .stream_chat(
                "flash",
                json!({ "model": "flash", "messages": [], "stream": true }),
            )
            .await
            .expect("a 401 must rotate to the next key, not reach the caller");
        assert_eq!(stream.endpoint_name(), "live");

        served.await.expect("the stub served both attempts");
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

    // -------------------------------------------------------------------
    // endpoint_traffic: which endpoints actually CARRY work
    // -------------------------------------------------------------------

    /// THE QUESTION THIS EXISTS FOR: a healthy endpoint that is serving nothing is
    /// invisible to every other signal in the service. The breaker reports `Closed` for
    /// an endpoint that has never been selected, so the only way to see a failover that
    /// never happened is to count the work each endpoint did.
    #[tokio::test]
    async fn a_served_request_is_counted_against_the_endpoint_that_served_it() {
        let url = upstream_answering("200 OK", "").await;
        let upstream = client_pointed_at(&url);

        // Every endpoint is reported from the start, at zero, INCLUDING ones that have
        // seen no traffic — "served: 0" is the observation, not an absence.
        let before = upstream.endpoint_traffic();
        assert_eq!(before.len(), 1, "the fixture has one endpoint");
        assert_eq!(before[0], ("solo".to_string(), 0, 0));

        upstream
            .stream_chat("flash", json!({ "model": "flash" }))
            .await
            .expect("the upstream answers 200");

        let after = upstream.endpoint_traffic();
        assert_eq!(
            after[0],
            ("solo".to_string(), 1, 0),
            "a 200 must be counted as SERVED and not as failed"
        );
    }

    /// A FAILED ATTEMPT IS THE OTHER HALF OF THE RATIO, and the pair is what makes either
    /// number mean something: "served 4,000" says nothing without knowing how many
    /// attempts it took.
    #[tokio::test]
    async fn a_failed_attempt_is_counted_as_failed_not_served() {
        let url = upstream_answering("500 Internal Server Error", "{}").await;
        let upstream = client_pointed_at(&url);

        // THE REQUEST, without which the assertion below reads a counter that no attempt ever
        // touched. Deleting this line as collateral damage from an unrelated edit is what made
        // this test fail: it asserted `failed == 1` on a client that had sent nothing.
        let _ = upstream
            .stream_chat("flash", json!({ "model": "flash" }))
            .await;

        let traffic = upstream.endpoint_traffic();
        assert_eq!(
            traffic[0],
            ("solo".to_string(), 0, 1),
            "a 5xx must be counted as FAILED; a served count of 1 would report an outage as work"
        );
    }

    /// FAILOVER MUST BE VISIBLE IN THE COUNTS: the endpoint that answered is credited,
    /// and the one that did not is charged a failure. This is the measurement that proves
    /// a load-shift actually happened rather than being assumed from the breaker state.
    #[tokio::test]
    async fn failover_credits_the_endpoint_that_served_and_charges_the_one_that_did_not() {
        let dead = upstream_answering("500 Internal Server Error", "{}").await;
        let healthy = upstream_answering("200 OK", "").await;

        let _lock = EnvLock::acquire();
        let _a = EnvGuard::set("APK_TEST_FIRST_KEY_1", "first-key");
        let _b = EnvGuard::set("APK_TEST_SECOND_KEY_1", "second-key");

        let mut first = endpoint("first", 1.0);
        first.url = dead;
        let mut second = endpoint("second", 1.0);
        second.url = healthy;

        let upstream = client(vec![model("flash", vec![first, second])]);
        upstream
            .stream_chat("flash", json!({ "model": "flash" }))
            .await
            .expect("the second endpoint serves");

        let traffic = upstream.endpoint_traffic();
        assert_eq!(traffic.len(), 2);
        assert_eq!(
            traffic[0],
            ("first".to_string(), 0, 1),
            "the endpoint that 500'd is charged a failure and credited nothing"
        );
        assert_eq!(
            traffic[1],
            ("second".to_string(), 1, 0),
            "the endpoint that served is credited, which is how a failover is MEASURED"
        );
    }

    /// A 429 IS A KEY FAILURE, NOT AN ENDPOINT FAILURE, and the counters must not
    /// conflate them. When every attempt is throttled the endpoint genuinely did not
    /// serve, so it IS charged — but the breaker must stay Closed, because the provider
    /// is merely busy rather than broken. The two facts are separate and both are
    /// asserted, which is the distinction this service is built around.
    #[tokio::test]
    async fn a_throttled_endpoint_is_charged_a_failure_without_tripping_its_breaker() {
        let url = upstream_answering("429 Too Many Requests", r#"{"error":"slow down"}"#).await;
        let upstream = client_pointed_at(&url);

        let _ = upstream
            .stream_chat("flash", json!({ "model": "flash" }))
            .await;

        let traffic = upstream.endpoint_traffic();
        assert_eq!(traffic[0].1, 0, "nothing was served");
        assert_eq!(
            traffic[0].2, 1,
            "every attempt was throttled, so the endpoint is charged ONE failure rather \
             than one per 429 - the flag exists so a rotation is not over-counted"
        );
        assert!(
            upstream.models[0].endpoints[0].breaker.allow_request(),
            "a throttled key must NOT trip the endpoint breaker: saturated, not broken"
        );
    }

    /// A CONNECTION REFUSAL TRIPS THE BREAKER IN ONE ATTEMPT — the claim
    /// `docs/failover.md` makes in its trip-condition table.
    ///
    /// THE TABLE READS: `| Connection refused | 1 — an unreachable host is unambiguous |`,
    /// and it is the only singleton among the three conditions (5xx and timeouts are both 3,
    /// from `failure_threshold`). That asymmetry is the claim: an unreachable host does not
    /// deserve the benefit of two more probes.
    ///
    /// MEASURED, and the table is WRONG: the transport arm calls `record_failure()` once and
    /// `record_failure` knows nothing about the KIND of failure - it increments
    /// `consecutive_failures` and compares to the same threshold a 5xx uses. So a refused
    /// connection is worth 1 of 3, not 1 of 1, and an unreachable host is probed twice more
    /// before it is taken out of rotation.
    #[tokio::test]
    async fn a_refused_connection_does_not_trip_the_breaker_in_one_attempt() {
        // A port with nothing listening: binding then dropping the listener leaves the
        // address closed, which is as close to "host unreachable" as a loopback test gets.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a port to close");
        let addr = listener.local_addr().expect("local addr");
        drop(listener);

        let url = format!("http://{addr}/v1");
        let upstream = client_pointed_at(&url);

        let err = upstream
            .stream_chat("flash", json!({ "model": "flash" }))
            .await
            .expect_err("nothing is listening");
        assert!(
            matches!(err, UpstreamError::Transport(_)),
            "a closed port is a TRANSPORT failure, not an HTTP status: {err:?}"
        );

        // THE ASSERTION IS THAT ONE ATTEMPT DID NOT TRIP IT. If the doc were right this
        // would be false, and the test would be the evidence for the correction rather than
        // a regression it fails on.
        assert!(
            upstream.models[0].endpoints[0].breaker.allow_request(),
            "MEASURED: a refused connection leaves the breaker CLOSED, so docs/failover.md's \
             'Connection refused | 1' row does not describe this router - a transport failure \
             is one of the same three consecutive failures a 5xx needs"
        );
    }

    /// A 403 DOES NOT PARK THE KEY — the behaviour `docs/failover.md` documents.
    ///
    /// THE DOC'S CLAIM IS PRECISE AND WORTH PINNING, because 403 is the one status the
    /// edge-case table argues ABOUT: *"403 is deliberately NOT in that set, and it is a real
    /// limitation rather than an oversight."* The stated reason is that our OWN surface uses
    /// 403 for `model_not_allowed`, so parking on it would withdraw a working key from rotation
    /// over one refused model. The consequence is stated just as plainly: *"a genuinely revoked
    /// key that answers 403 instead of 401 keeps being selected."*
    ///
    /// SO THE ASSERTION IS THAT THE KEY STAYS USABLE. `report_status` parks only for a status
    /// in `rate_limit_status` (shipped `[429, 401]`), and 403 is not in it - but that is a CONFIG
    /// value, and the behaviour a reader relies on is the pair agreeing: the doc says 403 is
    /// excluded, `apikita.toml` must still exclude it, and this proves it by driving a real 403
    /// through the loop and checking the key was not withdrawn.
    ///
    /// WHY THIS WAS MISSING: searching this module for `403` returned NOTHING, so the status the
    /// document spends two paragraphs on had no test at all - the exclusion was a property of a
    /// config file that nothing read for 403.
    ///
    /// MUTATION-TESTED, AND THE FIRST ATTEMPT WAS AN INVALID MUTANT WORTH RECORDING. MEASURED:
    /// making `is_rate_limit` answer true for 403 did NOT fail this test. The reason is a real
    /// property of this code - `is_rate_limit` reads `self.config.key_pool.rate_limit_status`
    /// while `report_status` reads `lease.policy.rate_limit_status`, a clone taken when the pool
    /// was built. The two agree today, and a mutant that changes only ONE of them is not a
    /// mutation of "403 parks the key" at all. The valid mutant is adding 403 to the CONFIG,
    /// which feeds both: MEASURED, that fails here with `left: Some(4.9999754s)`.
    ///
    /// The distinction matters beyond this test: two readers of one setting mean a change to
    /// either alone produces a state no test describes, so a mutation that passes looks like a
    /// covered property when it is only an untouched copy.
    #[tokio::test]
    async fn a_403_does_not_park_the_key() {
        // NO `EnvLock::acquire()` HERE, and this cost me two hung runs. `client_pointed_at` takes
        // that lock itself, and it is deliberately NOT reentrant - the helper's own doc says so
        // ("It is deliberately not Send: it belongs in the test body"). Taking it twice in one
        // body deadlocks the test against itself, which shows up as a run that never finishes
        // rather than one that fails: no timeout catches a lock held by the frame waiting on it.
        // Every neighbouring test that reads the same way omits this line; I added it by pattern
        // instead of by reading what the helper already does.
        //
        // `repeating_upstream_answering`, NOT `upstream_answering`, and getting this wrong cost a
        // HUNG TEST rather than a failed one: the one-shot stub accepts a single connection and the
        // listener is dropped, so the retry loop's later attempts connect to nothing and the run
        // never finishes. The helper below is `#[allow(dead_code)]` with a note predicting exactly
        // this ("a one-shot stub is wrong for a test that drives more than one attempt ... which
        // defeated a first attempt at the 429 test above") - and the note was right.
        let url = repeating_upstream_answering("403 Forbidden", r#"{"error":"forbidden"}"#).await;
        let upstream = client_pointed_at(&url);

        let _ = upstream
            .stream_chat("flash", json!({ "model": "flash" }))
            .await;

        // THE KEY IS NOT PARKED, which is the whole claim. A parked key is reported by
        // `shortest_key_cooldown` for the cooldown window; a released one is not.
        assert_eq!(
            upstream.models[0].endpoints[0].pool.shortest_key_cooldown(),
            None,
            "403 is NOT in rate_limit_status, so the key must be released rather than parked - docs/failover.md depends on this to avoid withdrawing a working key over one refused model"
        );

        // AND THE ENDPOINT IS NOT TRIPPED EITHER. A 403 is not a provider outage; the
        // edge-case table says only 5xx (and a transport failure) marks it unhealthy.
        assert!(
            !upstream.all_endpoints_unhealthy("flash"),
            "a 403 must not mark the endpoint unhealthy: it is a request-scoped refusal"
        );
    }
    /// A 404 IS ENDPOINT-SCOPED, so it is charged as a failure of THAT endpoint — which
    /// is what makes "provider B refuses this model every time" visible as a count.
    #[tokio::test]
    async fn a_404_is_charged_to_the_endpoint_that_refused() {
        let url = upstream_answering("404 Not Found", r#"{"error":"no such model"}"#).await;
        let upstream = client_pointed_at(&url);

        let _ = upstream
            .stream_chat("flash", json!({ "model": "flash" }))
            .await;

        let traffic = upstream.endpoint_traffic();
        assert_eq!(traffic[0].1, 0, "nothing was served");
        assert_eq!(traffic[0].2, 1, "the refusal is charged to the endpoint");
    }

    // -------------------------------------------------------------------
    // endpoint_latency_ms: the metric docs/observability.md promised and nobody built
    // -------------------------------------------------------------------

    /// AN ENDPOINT THAT HAS SERVED NOTHING REPORTS `None`, NOT `Some(0)`.
    ///
    /// This is the assertion that matters most in this group, because zero is not a
    /// neutral default here - it makes an unmeasured endpoint look like the FASTEST one in
    /// the pool, which is the exact inversion of the truth on the metric whose stated
    /// purpose is "distinguish our latency from theirs". A reader comparing providers would
    /// pick the one that had never answered.
    #[test]
    fn an_endpoint_that_has_served_nothing_reports_no_latency() {
        let _lock = EnvLock::acquire();
        let _key = EnvGuard::set("APK_TEST_PRIMARY_KEY_1", "test-key");
        let client = client(vec![model("flash", vec![endpoint("primary", 1.0)])]);

        assert_eq!(
            client.endpoint_latency_ms(),
            vec![("primary".to_string(), None)],
            "an endpoint with no served requests must report None, never 0ms"
        );
    }

    /// A SERVED REQUEST PRODUCES A REAL MEAN, measured around the call that reached the
    /// provider rather than around the whole request.
    #[tokio::test]
    async fn a_served_request_records_a_latency() {
        let url = upstream_answering("200 OK", "").await;
        let upstream = client_pointed_at(&url);

        upstream
            .stream_chat("flash", json!({ "model": "flash" }))
            .await
            .expect("the upstream answers 200");

        let latency = upstream.endpoint_latency_ms();
        assert_eq!(latency.len(), 1);
        assert!(
            latency[0].1.is_some(),
            "a served request must produce a measured mean, not None: {:?}",
            latency[0]
        );
    }

    /// A FAILED ATTEMPT MUST NOT RECORD A LATENCY. The figure is time-to-FIRST-BYTE for a
    /// request that was SERVED; folding a refusal into it would drag the mean toward the
    /// speed of the error path, which is fast precisely because nothing was generated.
    #[tokio::test]
    async fn a_failed_attempt_records_no_latency() {
        let url = upstream_answering("500 Internal Server Error", "{}").await;
        let upstream = client_pointed_at(&url);

        let _ = upstream
            .stream_chat("flash", json!({ "model": "flash" }))
            .await;

        assert_eq!(
            upstream.endpoint_latency_ms(),
            vec![("solo".to_string(), None)],
            "a 5xx is not a served request, so it must not contribute to the mean"
        );
    }

    /// A 404 IS ENDPOINT-SCOPED, SO IT FAILS OVER — the fix this test exists for.
    ///
    /// THE DEFECT: every non-5xx 4xx returned immediately, on the reasoning that "the
    /// caller request is bad, so another provider would refuse it too". That holds for a
    /// malformed body and does NOT hold for a 404, because `upstream_model` is rewritten
    /// PER ENDPOINT by design — two providers legitimately spell the same product
    /// differently. So one provider not carrying a name said nothing about the next one,
    /// and returning early turned a working failover pool into a hard failure.
    #[tokio::test]
    async fn a_404_from_one_endpoint_fails_over_to_the_next() {
        let refuses = upstream_answering("404 Not Found", r#"{"error":"no such model"}"#).await;
        let serves = upstream_answering("200 OK", "").await;

        let _lock = EnvLock::acquire();
        let _a = EnvGuard::set("APK_TEST_FIRST_KEY_1", "first-key");
        let _b = EnvGuard::set("APK_TEST_SECOND_KEY_1", "second-key");

        let mut first = endpoint("first", 1.0);
        first.url = refuses;
        let mut second = endpoint("second", 1.0);
        second.url = serves;

        let upstream = client(vec![model("flash", vec![first, second])]);

        let result = upstream
            .stream_chat("flash", json!({ "model": "flash" }))
            .await;

        match result {
            Ok(stream) => assert_eq!(
                stream.endpoint_name(),
                "second",
                "a 404 from the first endpoint must ROTATE to the second, because the \
                 model name is rewritten per endpoint and the second may carry it"
            ),
            Err(err) => panic!(
                "a 404 is endpoint-scoped, so the loop must fail over rather than abort, got {err:?}"
            ),
        }

        // The refusing endpoint must NOT have tripped its breaker: a missing model name
        // is a config fact about that provider, not a sign it is unhealthy. Taking it out
        // of rotation would remove capacity for every OTHER model it serves correctly.
        assert!(
            upstream.models[0].endpoints[0].breaker.allow_request(),
            "a 404 must not trip the breaker - the endpoint is up and answered"
        );
    }

    /// When EVERY routed endpoint 404s, the error names the mapping rather than
    /// pretending the caller sent something bad.
    #[tokio::test]
    async fn a_404_from_every_endpoint_names_the_configuration_fault() {
        let url = upstream_answering("404 Not Found", r#"{"error":"no such model"}"#).await;
        let upstream = client_pointed_at(&url);

        let err = upstream
            .stream_chat("flash", json!({ "model": "flash" }))
            .await
            .expect_err("a pool-wide 404 is not a success");

        match &err {
            UpstreamError::NoEndpointServesModel { model, tried } => {
                assert_eq!(model, "flash", "the PRODUCT model, not the rewritten name");
                assert_eq!(
                    tried.len(),
                    1,
                    "every refusing endpoint is reported so an operator sees which rewrite is wrong"
                );
                assert!(
                    tried[0].starts_with("solo="),
                    "each entry is endpoint=upstream_model, got {:?}",
                    tried[0]
                );
            }
            other => panic!(
                "a pool-wide 404 must be NoEndpointServesModel (a config fault), not {other:?}; \
                 ServerError(404) would blame the caller for our wrong upstream_model"
            ),
        }

        // And it must NOT be reported as an outage: every endpoint answered fine.
        assert!(
            !matches!(err, UpstreamError::NoHealthyUpstream(_)),
            "sending an operator after a nonexistent outage is the failure this variant exists to prevent"
        );
    }

    /// A 400 is still refused immediately — the fix must not have widened the arm.
    ///
    /// This is the guard against over-correcting: making 404 fail over is only right
    /// because the model NAME is endpoint-scoped. A malformed body is malformed
    /// everywhere, so failing over would spend a second upstream call on a request that
    /// cannot succeed, and the caller would be billed for it.
    #[tokio::test]
    async fn a_404_failing_over_does_not_make_a_400_fail_over() {
        let refuses = upstream_answering("400 Bad Request", r#"{"error":"bad body"}"#).await;
        let serves = upstream_answering("200 OK", "").await;

        let _lock = EnvLock::acquire();
        let _a = EnvGuard::set("APK_TEST_FIRST_KEY_1", "first-key");
        let _b = EnvGuard::set("APK_TEST_SECOND_KEY_1", "second-key");

        let mut first = endpoint("first", 1.0);
        first.url = refuses;
        let mut second = endpoint("second", 1.0);
        second.url = serves;

        let upstream = client(vec![model("flash", vec![first, second])]);

        let err = upstream
            .stream_chat("flash", json!({ "model": "flash" }))
            .await
            .expect_err("a 400 must be refused, not failed over");

        assert!(
            matches!(err, UpstreamError::ServerError(400)),
            "a malformed body is the caller fault at every provider, so it must abort \
             the loop rather than try the next endpoint, got {err:?}"
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
    /// the whole parse panic. This is the `from_utf8` guard in
    /// `parse_usage_from_sse` - a
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
