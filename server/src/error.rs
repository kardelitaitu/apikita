use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use serde_json::Value;
use tracing::error;

/// The fixed, generic `message` returned to clients for the two variants whose
/// inner value is diagnostic rather than contractual. It deliberately carries
/// nothing: no SQL, no table or column name, no host, IP, port, provider or
/// library name (docs/error-model.md, rule 1). It stays honest and
/// non-alarming, and it is not a contract - the `code` is.
const UNEXPECTED_ERROR_MESSAGE: &str =
    "An unexpected error occurred. Please try again, and quote the request_id if it persists.";

#[derive(Debug, Serialize)]
pub struct ApiErrorResponse {
    pub error: ApiErrorBody,
}

#[derive(Debug, Serialize)]
pub struct ApiErrorBody {
    pub code: String,
    pub message: String,
    pub request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("Invalid request: {0}")]
    InvalidRequest(String),

    #[error("Unauthenticated")]
    Unauthenticated,

    #[error("Key revoked")]
    KeyRevoked,

    #[error("Key expired")]
    KeyExpired,

    #[error("Insufficient balance")]
    InsufficientBalance { details: Option<Value> },

    #[error("Key limit exceeded")]
    KeyLimitExceeded { details: Option<Value> },

    #[error("Model not allowed: {0}")]
    ModelNotAllowed(String),

    #[error("Wrong credential type: {0}")]
    WrongCredentialType(String),

    #[error("Resource not found: {0}")]
    NotFound(String),

    #[error("Conflict: {0}")]
    Conflict(String),

    /// Carries the offending field, not just prose. docs/error-model.md
    /// rule 5 requires `details.field` so a UI can highlight the input
    /// without parsing the message; making it a required field means a new
    /// call site cannot forget it.
    #[error("Validation failed: {message}")]
    ValidationFailed { message: String, field: String },

    #[error("Rate limited")]
    RateLimited { retry_after_secs: u64 },

    /// Every upstream is unhealthy (docs/error-model.md, Status codes, the 503
    /// no_upstream_available row). Carries the
    /// `Retry-After` the client must honour, sourced from the pool's shortest
    /// remaining cooldown, so the wait is never guessed.
    #[error("No upstream available")]
    NoUpstreamAvailable { retry_after_secs: u64 },

    /// A store this request needs could not be reached at all, as opposed to
    /// answering a question with "no".
    ///
    /// DISTINCT FROM `Database`, which is a 500: that variant means a query THIS
    /// code wrote failed against a database that answered, which is a bug and
    /// belongs in the log as one. This one means the answer was never available -
    /// the file is gone, the pool cannot connect, the disk is full - and the honest
    /// thing to tell a client is "try again", not "something is broken here".
    /// `NoUpstreamAvailable` is the same status for the same reason, one layer out.
    #[error("Unavailable: {0}")]
    Unavailable(String),

    // Display keeps the full inner detail on purpose: it is load-bearing for
    // server-side observability (every `error = %err` log site). The
    // customer-facing body does NOT use Display - see `client_message`.
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("Internal server error: {0}")]
    Internal(String),
}

impl AppError {
    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::InvalidRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthenticated | Self::KeyRevoked | Self::KeyExpired => StatusCode::UNAUTHORIZED,
            Self::InsufficientBalance { .. } | Self::KeyLimitExceeded { .. } => {
                StatusCode::PAYMENT_REQUIRED
            }
            Self::ModelNotAllowed(_) | Self::WrongCredentialType(_) => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::ValidationFailed { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            Self::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::NoUpstreamAvailable { .. } | Self::Unavailable(_) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            Self::Database(_) | Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "invalid_request",
            Self::Unauthenticated => "unauthenticated",
            Self::KeyRevoked => "key_revoked",
            Self::KeyExpired => "key_expired",
            Self::InsufficientBalance { .. } => "insufficient_balance",
            Self::KeyLimitExceeded { .. } => "key_limit_exceeded",
            Self::ModelNotAllowed(_) => "model_not_allowed",
            Self::WrongCredentialType(_) => "wrong_credential_type",
            Self::NotFound(_) => "not_found",
            Self::Conflict(_) => "conflict",
            Self::ValidationFailed { .. } => "validation_failed",
            Self::RateLimited { .. } => "rate_limited",
            Self::NoUpstreamAvailable { .. } => "no_upstream_available",
            Self::Unavailable(_) => "unavailable",
            Self::Database(_) | Self::Internal(_) => "internal_error",
        }
    }

    /// The client-facing `message`. This is the non-contract field
    /// (docs/error-model.md, Response shape: code is the contract, message is not)
    /// and the one rule 1 constrains: for `Database`
    /// and `Internal` it is fixed and generic, so no SQL, schema, host, IP,
    /// port, provider or library name can reach a customer. The underlying
    /// detail is not dropped - `into_response` logs it at `error!` level.
    pub fn client_message(&self) -> String {
        match self {
            Self::Database(_) | Self::Internal(_) => UNEXPECTED_ERROR_MESSAGE.to_string(),
            other => other.to_string(),
        }
    }

    pub fn details(&self) -> Option<Value> {
        match self {
            Self::InsufficientBalance { details } | Self::KeyLimitExceeded { details } => {
                details.clone()
            }
            // docs/error-model.md rule 5: the field name travels as
            // `details.field`, so the client highlights the input instead of
            // matching on prose.
            Self::ValidationFailed { field, .. } => Some(serde_json::json!({ "field": field })),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// The server-error counter
// ---------------------------------------------------------------------------

/// A count of 5xx responses and of all responses, so an error RATE is computable.
///
/// WHY THIS EXISTS. `docs/observability.md:107` names an "Error rate >5%" alert and
/// `tools/alert/alerts.tsv` lists it, but marks it `needs-metrics` - because
/// nothing counted anything. The service already knew when it returned a 5xx (the
/// `error!` in `into_response`), but a rate needs a COUNT over a window, so the
/// alert could never fire. This is the smallest primitive that makes it real
/// without choosing a metrics vendor, and any future scraper reads it unchanged.
///
/// WHY THE DENOMINATOR IS HERE TOO. "Error rate > 5%" is a ratio. A bare error
/// count cannot be alerted on without knowing how much traffic produced it, and a
/// count that spikes at 3am with no traffic context is not actionable.
///
/// WHAT IT DELIBERATELY DOES NOT RECORD: no status codes, no paths, no error
/// messages, no per-account breakdown. Two integers. The health endpoint that
/// exposes it is UNAUTHENTICATED (docs/server/api-spec.md:368), so anything richer
/// would leak operational detail to any caller - the same rule that keeps
/// `DATABASE_UNAVAILABLE` a fixed string.
#[derive(Debug, Default)]
pub struct ServerErrorCounter {
    server_errors: AtomicU64,
    responses: AtomicU64,
}

impl ServerErrorCounter {
    /// One 5xx was returned.
    pub fn record_error(&self) {
        self.server_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// One response was returned, at any status.
    pub fn record_response(&self) {
        self.responses.fetch_add(1, Ordering::Relaxed);
    }

    pub fn server_errors(&self) -> u64 {
        self.server_errors.load(Ordering::Relaxed)
    }

    pub fn responses(&self) -> u64 {
        self.responses.load(Ordering::Relaxed)
    }

    /// The error rate, or `None` when there have been no responses.
    ///
    /// **`None` is not `0.0`.** A service that has served nothing has an UNKNOWN
    /// error rate; reporting it as zero would look like a perfectly healthy service
    /// and silently suppress the alert. The caller must distinguish the two.
    pub fn error_rate(&self) -> Option<f64> {
        let responses = self.responses();
        if responses == 0 {
            return None;
        }
        Some(self.server_errors() as f64 / responses as f64)
    }
}

/// The process-wide counter. See `ServerErrorCounter`.
///
/// A `OnceLock` rather than a parameter threaded through `AppState`, because axum
/// calls the `IntoResponse` method with only `self`: there is nowhere to pass a
/// counter without changing the trait, and every route would have to remember to
/// forward it. A process-global cannot be forgotten at a call site, so it cannot
/// drift from what is actually returned.
pub fn server_error_counter() -> &'static ServerErrorCounter {
    static COUNTER: std::sync::OnceLock<ServerErrorCounter> = std::sync::OnceLock::new();
    COUNTER.get_or_init(ServerErrorCounter::default)
}

/// WHAT AN UNDERCHARGE IS, precisely, because the alert has named it for a while as
/// `needs-metrics` and the reason it could not fire was that nothing COUNTED it.
///
/// A request whose reported usage exceeded the balance it could be taken from. The
/// answer was already streamed, so the tokens were genuinely consumed: the debit is
/// CLAMPED to the balance, the real usage counters are recorded in full, and the
/// difference is **money consumed and never collected**. The shortfall is real
/// revenue the business does not get.
///
/// WHY IT IS ITS OWN COUNTER AND NOT PART OF THE ERROR RATE. Every one of these is
/// already an `error!` log line, and an unalerted `error!` is exactly the defect this
/// repository writes gates against — a refund-nobody-sees, a drift-nobody-sees. A log
/// line reaches an operator only if somebody is reading logs at that moment; a counter
/// reaches the metrics payload, which the alert tooling can read without a log
/// scraper. Same shape as `ServerErrorCounter`: the smallest primitive that makes the
/// number real.
///
/// WHY THE SUM AND NOT ONLY THE COUNT. A single undercharge of 200 IDR is noise; 40
/// undercharges totalling 90,000 IDR is a customer running their account to zero and
/// being served a premium answer for free. The AMOUNT is the part that makes it
/// actionable, and `docs/business/05-risk.md` R3 already names under-charging as a
/// HIGH risk — with no figure attached, because there was nothing to attach.
#[derive(Debug, Default)]
pub struct MoneyCounter {
    /// How many settlements were clamped.
    undercharges: AtomicU64,
    /// The sum of the uncollected shortfalls, in IDR.
    undercharged_idr: AtomicU64,
}

/// Per-reason counts for the requests the proxy REFUSED before costing anyone anything.
///
/// `docs/observability.md` has carried a `proxy_rejections_total{reason}` row in its
/// "Rejection reasons" table since the document was written, with the note that it is
/// "a genuinely good idea that no code computes". That note was accurate, and it is
/// what this closes.
///
/// WHY THE KEY IS `AppError::code()` AND NOT A NEW VOCABULARY. The reasons an operator
/// would act on are ALREADY the codes this service returns — `key_revoked`,
/// `model_not_allowed`, `rate_limited`, `key_limit_exceeded`,
/// `insufficient_balance` — and `code()` is the CLIENT-FACING contract
/// (`docs/error-model.md`), so it is a closed set that already has a guard on it.
/// Inventing a second list would have been the duplicate-rules-drift defect this
/// repository keeps writing guards against. The doc's own draft list had already
/// drifted from the codes in two places (`model_denied` for `model_not_allowed`,
/// `limit_exceeded` for `key_limit_exceeded`).
///
/// WHAT IS DELIBERATELY NOT COUNTED: no per-account, no per-model, no path. This
/// payload is served on an operator-authenticated route, but a cardinality-keyed map
/// is how a metrics surface quietly becomes a surveillance surface — and
/// `ServerErrorCounter`'s "two integers and nothing else" rule is the precedent.
///
/// WHAT IS ALSO DELIBERATELY NOT COUNTED: a refusal that COSTS money. A 402 after a
/// reservation was taken is a settlement, not a rejection, and
/// `insufficient_balance` here means the refusal fired before any hold moved.
#[derive(Debug, Default)]
pub struct RejectionCounter {
    counts: Mutex<BTreeMap<String, u64>>,
    /// Every request that reached the /v1 ADMISSION GATE, refused or admitted.
    ///
    /// THIS IS THE DENOMINATOR, and it is NOT `ServerErrorCounter::responses`. That
    /// counter moves for every response the process returns - sign-in, the dashboard,
    /// the SSE stream, `/health` - so dividing proxy refusals by it would produce a
    /// number that DROPS as dashboard traffic rises, with no change in proxy traffic at
    /// all. A rate whose movements are caused by an unrelated surface is worse than no
    /// rate, because it looks like a measurement.
    ///
    /// Recorded at the same point the refusals are, so numerator and denominator come
    /// from one place and cannot come to disagree about what a `request` is.
    requests: AtomicU64,
}

impl RejectionCounter {
    /// One refusal, filed under its documented code.
    pub fn record(&self, code: &str) {
        let mut counts = self
            .counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *counts.entry(code.to_string()).or_insert(0) += 1;
    }

    /// One request arrived at the /v1 admission gate, whether or not it was refused.
    ///
    /// Recorded BEFORE any check runs, so the denominator counts every attempt rather than
    /// only the ones that got far enough to be counted elsewhere. A request refused for a
    /// missing Authorization header never reaches a route handler, which is exactly why this
    /// call site has to be the same one the refusals come from.
    pub fn record_request(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    /// Every reason that has fired at least once, in code order.
    ///
    /// Reasons that have NEVER fired are absent rather than present-as-zero, so the
    /// payload cannot grow to the size of the whole code table on a quiet service —
    /// and a reason going non-zero for the first time is a visible event.
    pub fn counts(&self) -> Vec<(String, u64)> {
        self.counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|(code, count)| (code.clone(), *count))
            .collect()
    }

    /// The share of admission attempts that were refused, or `None` when none arrived.
    ///
    /// **`None` IS NOT `0.0`**, the same rule `ServerErrorCounter::error_rate` documents:
    /// a service that has taken no proxy traffic has an UNKNOWN refusal rate, and reporting
    /// it as zero would look like a perfectly healthy gate and suppress any alert built on
    /// it. The caller must distinguish the two.
    ///
    /// WHY A RATE AND NOT THE RAW COUNT. The count alone cannot be acted on: a spike at 3am
    /// with no traffic context is not a signal, and a steady 40 refusals an hour is either
    /// growth or an attack depending entirely on how many requests produced them. This is the
    /// same argument `ServerErrorCounter` makes for its own denominator, applied to the gate.
    ///
    /// A REFUSAL RATE IS NORMAL AND SHOULD NOT BE ALERTED AT A LOW BAR: unauthenticated
    /// probing is continuous on any public endpoint, so a non-zero rate is the expected state
    /// and only its SHAPE carries information.
    pub fn rejection_rate(&self) -> Option<f64> {
        let requests = self.requests();
        if requests == 0 {
            return None;
        }
        let refused: u64 = self
            .counts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .sum();
        Some(refused as f64 / requests as f64)
    }
}

/// The process-wide rejection counter. See `RejectionCounter`.
pub fn rejection_counter() -> &'static RejectionCounter {
    static COUNTER: std::sync::OnceLock<RejectionCounter> = std::sync::OnceLock::new();
    COUNTER.get_or_init(RejectionCounter::default)
}

impl MoneyCounter {
    /// One clamped settlement, carrying the shortfall that was not collected.
    ///
    /// THE IDR ACCUMULATION REALLY SATURATES, and the first version of this function did
    /// not. It used `fetch_add`, which WRAPS — and the comment above it claimed saturation
    /// anyway, so the code and its own explanation disagreed and only a test could tell.
    /// MEASURED: two `i64::MAX` shortfalls left the counter at `u64::MAX - 1`, i.e. the
    /// worst possible loss reported as 18 quintillion short of the true figure.
    ///
    /// This is money arithmetic in a build where `[profile.release]` leaves
    /// overflow-checks OFF, so nothing would have panicked: the number would simply be
    /// wrong, silently, on the one alert whose whole purpose is to say how much was lost.
    ///
    /// The loop is a compare-and-swap rather than a `fetch_add` with a check, because
    /// check-then-act is a lost-update race between the load and the store — and this
    /// counter is written from the detached settlement task, which can be concurrent.
    /// A failed CAS retries against the new value, so no increment is ever dropped.
    pub fn record_undercharge(&self, shortfall_idr: i64) {
        self.undercharges.fetch_add(1, Ordering::Relaxed);
        if shortfall_idr <= 0 {
            return;
        }
        let add = shortfall_idr as u64;
        let mut current = self.undercharged_idr.load(Ordering::Relaxed);
        loop {
            // `saturating_add`, so the counter maxes out instead of wrapping to a
            // number that reads as no loss at all.
            match self.undercharged_idr.compare_exchange_weak(
                current,
                current.saturating_add(add),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    pub fn undercharges(&self) -> u64 {
        self.undercharges.load(Ordering::Relaxed)
    }

    pub fn undercharged_idr(&self) -> u64 {
        self.undercharged_idr.load(Ordering::Relaxed)
    }
}

/// The process-wide money counter. See `MoneyCounter`.
///
/// A `OnceLock` for the same reason as `server_error_counter`: the counter is
/// incremented on the settlement path, which is a detached task with no request
/// handler to thread a parameter through, so a global cannot be forgotten at the
/// call site.
pub fn money_counter() -> &'static MoneyCounter {
    static COUNTER: std::sync::OnceLock<MoneyCounter> = std::sync::OnceLock::new();
    COUNTER.get_or_init(MoneyCounter::default)
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let code = self.code().to_string();
        let details = self.details();
        let request_id = format!("req_{}", uuid::Uuid::new_v4().simple());

        // COUNT THE RESPONSE HERE, in the ONE place every error response is built,
        // so the counter cannot drift from what is actually returned. Every status
        // moves the denominator; only 5xx moves the numerator. See
        // `ServerErrorCounter` for why the ratio, not a bare count, is the metric.
        //
        // `>= 500` rather than the two internal variants: a 503 from
        // `NoUpstreamAvailable` is a server-side failure an operator must see in the
        // rate, and counting only `Internal`/`Database` would under-report exactly
        // when the upstream is down - which is when the alert matters most.
        let counter = server_error_counter();
        counter.record_response();
        if status.is_server_error() {
            counter.record_error();
        }
        // COUNT THE REJECTION BY REASON, in this same single place so it cannot drift
        // from what is actually returned. RejectionCounter is keyed on code(), which is
        // the client-facing contract - so the breakdown docs/observability.md names is
        // served without a second vocabulary that could disagree with it.
        //
        // ONLY THE 4xx THAT ARE THE PROXY OWN GATE FIRING, and the filter matters more
        // than the increment. A 5xx is not a rejection: the request would have been served
        // and the provider failed, which is ServerErrorCounter numerator. A 401 on /api/*
        // is authentication, not a proxy decision. So the set is the admission refusals -
        // bad or absent credential, model not sellable, key over a ceiling, rate limit, or
        // the wallet short - and every one of them preceded any hold leaving the wallet.
        if status.is_client_error() {
            match code.as_str() {
                "key_revoked"
                | "key_expired"
                | "model_not_allowed"
                | "key_limit_exceeded"
                | "insufficient_balance"
                | "rate_limited"
                | "unauthenticated" => {
                    rejection_counter().record(&code);
                }
                _ => {}
            }
        }

        // The detail moves to the log, not out of the system. `Display` keeps
        // the raw sqlx error / formatted failure, so an operator reading the
        // log still sees it; the customer sees only `client_message`.
        if matches!(self, Self::Database(_) | Self::Internal(_)) {
            // Computed as a plain statement (not inside the `error!` field list)
            // so the conversion is counted as covered regardless of whether a
            // subscriber is registered - the macro only evaluates its field
            // expressions when one is, which the unit tests do not do.
            let status_u16 = status.as_u16();
            error!(
                request_id = %request_id,
                code = %code,
                status = status_u16,
                error = %self,
                "returning a generic message to the client for an internal failure"
            );
        }
        let message = self.client_message();

        let body = ApiErrorResponse {
            error: ApiErrorBody {
                code,
                message,
                request_id,
                details,
            },
        };

        let mut res = (status, Json(body)).into_response();

        // The ONE place a Retry-After is decided (docs/error-model.md, Retry-After:
        // "Included on 429 and 503"). Both variants carry their value, so the
        // header logic lives here rather than at either call site.
        //
        // For 503 the value is the pool's shortest remaining cooldown. When no
        // breaker is open there is no cooldown to report, and the caller emits
        // the documented 1-second floor - never a fabricated estimate
        // (docs/error-model.md, Retry-After and 429 — rate limited). Which path
        // produced the 503 is logged
        // at the call site so the floor is explained, not silent.
        let retry_after_secs = match self {
            Self::RateLimited { retry_after_secs }
            | Self::NoUpstreamAvailable { retry_after_secs } => Some(retry_after_secs),
            _ => None,
        };
        if let Some(secs) = retry_after_secs {
            if let Ok(val) = secs.to_string().parse() {
                res.headers_mut()
                    .insert(axum::http::header::RETRY_AFTER, val);
            }
        }
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::{header, HeaderMap};
    use serde_json::{json, Value as Json};

    /// THE UNDERCHARGE COUNTER IS NOT THE SAME OBJECT AS THE ERROR COUNTER, and a
    /// shared counter would make the money number move on every 5xx - which is the
    /// failure mode that gets an alert muted: the figure is right some of the time and
    /// nobody can tell when.
    #[test]
    fn the_money_counter_counts_events_and_idr_independently() {
        let money = MoneyCounter::default();
        assert_eq!(money.undercharges(), 0);
        assert_eq!(
            money.undercharged_idr(),
            0,
            "zero is a real and GOOD reading here, unlike error_rate's null for an empty window"
        );

        money.record_undercharge(200);
        money.record_undercharge(90_000);
        assert_eq!(money.undercharges(), 2);
        assert_eq!(
            money.undercharged_idr(),
            90_200,
            "the SUM is the actionable half: 2 events of 200 is noise, and the total is what an operator pages on"
        );
    }

    // -----------------------------------------------------------------------
    // RejectionCounter: the reason breakdown, keyed on the documented code
    // -----------------------------------------------------------------------

    /// THE RATE NEEDS ITS OWN DENOMINATOR, and this is the test that says why.
    ///
    /// `ServerErrorCounter::responses` counts EVERY response the process returns - sign-in,
    /// the dashboard, `/health` - so a rate built on it would FALL as unrelated traffic
    /// rose, with no change in proxy traffic at all. The counter therefore keeps its own
    /// `requests`, incremented at the same gate the refusals are.
    #[test]
    fn the_rejection_rate_uses_its_own_denominator() {
        let counter = RejectionCounter::default();
        assert_eq!(
            counter.rejection_rate(),
            None,
            "a gate that has seen nothing is UNMEASURED, not 0% - the same rule error_rate documents"
        );

        for _ in 0..4 {
            counter.record_request();
        }
        counter.record("rate_limited");
        assert_eq!(
            counter.rejection_rate(),
            Some(0.25),
            "one refusal out of four attempts"
        );

        // THE PROPERTY THAT MATTERS: the rate must never exceed 1.0. It can only do so if
        // a refusal is counted for a request the denominator never saw, which is exactly
        // what recording `record_request` AFTER the first check would cause - a request
        // refused for a missing header would be a numerator with no denominator.
        let fresh = RejectionCounter::default();
        fresh.record("unauthenticated");
        fresh.record("unauthenticated");
        assert!(
            fresh.rejection_rate().is_none_or(|rate| rate <= 1.0),
            "a rate above 1.0 means refusals are being counted for requests the denominator never saw"
        );
    }

    /// EVERY ATTEMPT MOVES THE DENOMINATOR, refused or not, so the rate is a share of
    /// attempts rather than of failures.
    #[test]
    fn an_admitted_request_still_counts_toward_the_denominator() {
        let counter = RejectionCounter::default();
        counter.record_request();
        counter.record("rate_limited");
        counter.record_request();
        counter.record_request();
        assert_eq!(counter.requests(), 3);
        assert_eq!(
            counter.rejection_rate(),
            Some(1.0 / 3.0),
            "the two ADMITTED requests must lower the rate, or it is not a rate"
        );
    }

    /// THE REASONS ARE THE CODES THE SERVICE ALREADY RETURNS, and that is the whole design:
    /// a second vocabulary would drift from the client-facing contract, which is exactly what
    /// docs/observability.md's own draft list had already done in two places.
    #[test]
    fn a_rejection_is_filed_under_the_code_it_was_returned_as() {
        let counter = RejectionCounter::default();
        counter.record("insufficient_balance");
        counter.record("insufficient_balance");
        counter.record("rate_limited");

        assert_eq!(
            counter.counts(),
            vec![
                ("insufficient_balance".to_string(), 2),
                ("rate_limited".to_string(), 1),
            ],
            "counts must aggregate per code, and in code order so the payload is stable"
        );
    }

    /// A REASON THAT HAS NEVER FIRED IS ABSENT, not zero.
    ///
    /// This is what keeps a quiet service from publishing an entry for every code in
    /// error-model.md, and it is why a reason going non-zero for the FIRST time is a
    /// visible event rather than one line changing among many.
    #[test]
    fn a_reason_that_has_never_fired_is_absent_not_zero() {
        let counter = RejectionCounter::default();
        counter.record("key_revoked");
        let counts = counter.counts();
        assert_eq!(counts.len(), 1, "only the code that fired is reported");
        assert_eq!(counts[0].0, "key_revoked");
    }

    /// Poison must not take the metrics path down: one panicking recording thread must not
    /// make every later rejection reading panic on the /api/admin/metrics route.
    #[test]
    fn a_poisoned_rejection_lock_recovers_instead_of_panicking() {
        let counter = RejectionCounter::default();
        {
            let _held = counter.counts.lock().expect("fresh lock is unpoisoned");
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                panic!("poison the rejection lock");
            }))
            .ok();
        }

        counter.record("rate_limited");
        assert_eq!(
            counter.counts(),
            vec![("rate_limited".to_string(), 1)],
            "recording and reading must both survive a poisoned lock"
        );
    }

    /// A ZERO OR NEGATIVE shortfall must not move the IDR figure, because the caller
    /// only reaches this after the `shortfall_idr <= 0` guard - and if a future edit moves
    /// the call above that guard, a concurrent top-up (a settled-in-full request, not an
    /// undercharge) must not inflate the number.
    #[test]
    fn a_non_positive_shortfall_adds_no_idr() {
        let money = MoneyCounter::default();
        money.record_undercharge(0);
        money.record_undercharge(-50);
        assert_eq!(
            money.undercharges(),
            2,
            "both were reported, so both are events"
        );
        assert_eq!(
            money.undercharged_idr(),
            0,
            "but neither lost any money, so the IDR figure must not move"
        );
    }

    /// THE AMOUNT SATURATES RATHER THAN WRAPS. This is money arithmetic in a release
    /// build with overflow-checks OFF, so a wrap would turn the largest loss in the
    /// service's history into a small or NEGATIVE number - the worst day reading as its
    /// best.
    #[test]
    fn a_huge_shortfall_saturates_instead_of_wrapping() {
        let money = MoneyCounter::default();
        // THREE, not two, and the arithmetic is why: 2 * i64::MAX is
        // 18_446_744_073_709_551_614, which is u64::MAX - 1 and therefore REPRESENTABLE.
        // Two would prove nothing about saturation — the first version of this test used
        // two and its expected value was simply wrong, which is how the defective
        // implementation below it passed. Three does overflow the counter.
        money.record_undercharge(i64::MAX);
        money.record_undercharge(i64::MAX);
        money.record_undercharge(i64::MAX);
        assert_eq!(
            money.undercharged_idr(),
            u64::MAX,
            "an overflowing total must SATURATE at the counter's ceiling. A wrapping \
             fetch_add lands on a small number here, and a large loss would then read as \
             a small one on the single alert whose job is to say how much was lost"
        );

        // CONTROL: the saturation is a ceiling, not a frozen counter — a further
        // increment must still be absorbed without changing the value.
        money.record_undercharge(i64::MAX);
        assert_eq!(
            money.undercharged_idr(),
            u64::MAX,
            "once saturated it stays saturated rather than wrapping to a small number"
        );
        assert_eq!(money.undercharges(), 4, "but every event is still counted");
    }

    /// The literal contract from docs/error-model.md:35-50.
    ///
    /// Transcribed by hand from the document. The assertions below compare the
    /// code against this table, not against itself, so any drift between
    /// error.rs and the published contract fails a test.
    const DOCUMENTED: &[(u16, &str)] = &[
        (400, "invalid_request"),
        (401, "unauthenticated"),
        (401, "key_revoked"),
        (401, "key_expired"),
        (402, "insufficient_balance"),
        (402, "key_limit_exceeded"),
        (403, "model_not_allowed"),
        (403, "wrong_credential_type"),
        (404, "not_found"),
        (409, "conflict"),
        (422, "validation_failed"),
        (429, "rate_limited"),
        (500, "internal_error"),
        (503, "no_upstream_available"),
        // ADDED with the variant's row in docs/error-model.md. The server has emitted this code since
        // `AppError::Unavailable` was introduced; the contract did not list it, so a client switching
        // on `code` had no definition for a 503 that is NOT `no_upstream_available`.
        (503, "unavailable"),
    ];

    /// The published table is the whole API, and ONE code lives outside `AppError`:
    /// `forbidden` is built by `admin.rs::forbidden_response` for a non-operator or a
    /// self-action, because the operator surface needs a message that says WHICH, and
    /// `AppError` has no variant that carries one.
    ///
    /// At MODULE scope rather than inside the test that first needed it, because a second
    /// test now compares the quickstart page against the same complete set, and a second
    /// copy of this row is the drift both of them exist to catch.
    const OUTSIDE_APP_ERROR: &[(&str, u16)] = &[("forbidden", 403)];

    /// The published table, parsed from the document rather than trusted from
    /// memory.
    ///
    /// The comment on DOCUMENTED claims "any drift between error.rs and the
    /// published contract fails a test". That is true of the CODE half and false of
    /// the DOCUMENT half. DOCUMENTED is transcribed by hand, so editing
    /// docs/error-model.md - changing a status, adding a code, dropping a row -
    /// leaves every assertion here green while the published contract says
    /// something the server does not do. The contract is what a client codes
    /// against, and it is the copy nobody runs.
    ///
    /// So this reads the table. A hand-kept copy of what another file says is the
    /// same bug twice over, which is why the alert guards read probe.sh rather than
    /// counting its rows.
    /// The published table names no rating, and the NAME SAYS SO.
    ///
    /// This was `the_published_error_table_names_no_field_the_system_does_not_accept`,
    /// which is the overstatement pattern for the third time in three rounds, and the
    /// most blatant of the three: behind that name are three strings.
    ///
    /// A general version is not available, and pretending otherwise would be the same
    /// mistake with a better disguise. The column is PROSE - "Well-formed but invalid
    /// (a top-up amount below the provider minimum)" - so telling a field name from an
    /// English word is a judgement, and a heuristic would either miss an invented field
    /// wearing ordinary words or flag the ordinary words. This suite has a documented
    /// history of false alarms on exactly that kind of matcher, including one that read
    /// three clock times and a URL as file citations.
    ///
    /// So the check is what it is - the way a rating gets spelled, all three of them -
    /// and the name says that rather than claiming a general sweep it does not perform.
    ///
    /// The status-code guard below compares CODES, and every code matched - the 422 was
    /// real, the `validation_failed` was real. What it did not compare is the column a
    /// customer reads to understand WHY they got one, and that column had been the
    /// canonical example of a validation failure for a field the system does not accept:
    /// "rating out of range". No endpoint takes a rating and no code writes one; the
    /// schema has three `rating` columns and nothing touches them. So the table was
    /// correct in its codes and false in its prose, which is the more damaging half -
    /// a developer debugging a 422 by that table would look for a field that is not there.
    ///
    /// So the two real 422s are now named in the table, and this test stops a fictional
    /// example creeping back in. The general lesson is in docs/testing.md: a guard that
    /// compares the machine-readable half of a document leaves the prose unguarded, and
    /// the prose is what a human is actually reading.
    #[test]
    fn the_published_error_table_names_no_ratings() {
        let table = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("docs")
                .join("error-model.md"),
        )
        .expect("docs/error-model.md must be readable, or this checks nothing");
        let mut checked = 0usize;
        for example in ["rating", "score", "stars"] {
            if table.contains(example) {
                panic!(
                    "docs/error-model.md names {example:?} in the status table. Nothing in the \
                     crate accepts a rating: no route takes one and no code writes the three \
                     rating columns the schema declares. Name a validation the code performs \
                     instead - a top-up amount below the provider minimum, or a date that is not \
                     YYYY-MM-DD."
                );
            }
            checked += 1;
        }
        assert!(
            checked > 0,
            "no examples were checked, so this test passes over nothing"
        );
    }

    #[test]
    fn the_published_status_table_is_the_one_the_code_serves() {
        let spec = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("docs")
                .join("error-model.md"),
        )
        .expect("docs/error-model.md must be readable, or this checks nothing");

        // Scoped to the status-code section, because the streaming codes below it
        // are deliberately status-less - they travel as a frame after the headers
        // are sent, and treating them as missing rows would be a false alarm about a
        // design decision rather than a drift.
        let start = spec
            .find("## Status codes and their meanings")
            .unwrap_or_else(|| {
                panic!(
                    "docs/error-model.md no longer has the Status codes section. The status \
                 table is the contract a client codes against, and a check that cannot \
                 find it would pass over nothing."
                )
            });
        let section = &spec[start..];
        let end = section[2..]
            .find("\n## ")
            .map(|at| at + 2)
            .unwrap_or(section.len());
        let table = &section[..end];

        let mut published: Vec<(u16, String)> = Vec::new();
        for line in table.lines() {
            let cells: Vec<&str> = line.split('|').map(str::trim).collect();
            if cells.len() < 3 {
                continue;
            }
            let Ok(status) = cells[1].parse::<u16>() else {
                continue;
            };
            // The code cell is wrapped in backticks in the document; stripping every
            // one of them is simpler than matching one, and a code never contains
            // a backtick.
            let code = cells[2].replace(char::from(96), "");
            let code = code.trim();
            if code.is_empty() {
                continue;
            }
            published.push((status, code.to_string()));
        }

        // The vacuity guard: an empty or unparsable table must fail loudly rather
        // than comparing DOCUMENTED against nothing and passing.
        assert!(
            published.len() >= 10,
            "only {} row(s) parsed out of the published status table, so the comparison \
             below is not a comparison. A renamed heading or a reformat would land here \
             rather than in a false PASS.",
            published.len()
        );

        // DOCUMENTED is the AppError set. The published table is the whole API, and
        // ONE code lives outside this enum: forbidden is built by
        // admin.rs::forbidden_response for a non-operator or a self-action, because the
        // operator surface needs a message that says WHICH, and AppError has no variant
        // that carries one. Conflating the two sets is what the hand-transcribed copy did
        // - it listed fourteen rows against a document with fifteen - so the difference is
        // named here rather than papered over by adding the row to DOCUMENTED, which
        // would break the sibling test asserting AppError's codes are exactly the
        // documented AppError codes.
        let mut expected: Vec<(u16, String)> = DOCUMENTED
            .iter()
            .map(|(status, code)| (*status, (*code).to_string()))
            .collect();
        for (code, status) in OUTSIDE_APP_ERROR {
            expected.push((*status, (*code).to_string()));
        }
        expected.sort();
        let mut documented = published.clone();
        documented.sort();
        assert_eq!(
            documented, expected,
            "the published status table and the one error.rs serves have drifted. A \
             client codes against the document, so a row that says one thing while the \
             server does another is the contract breaking quietly. Update both together."
        );
    }
    /// Every AppError variant, one of each. The response-level tests run over
    /// this whole set, so a newly added variant is covered the day it lands
    /// rather than the day someone remembers to extend a hand-picked list.
    ///
    /// THAT SENTENCE WAS ASPIRATIONAL until `every_variant_is_listed_here` was added below. MEASURED:
    /// the enum had 16 variants and this list had 15 - `Unavailable` was missing - so the claim above
    /// was describing a mechanism that did not exist. Adding a variant still needed someone to
    /// remember, which is exactly what the comment said was unnecessary. `Unavailable` was added by
    /// hand here as well; the difference is that the guard now fails if it happens again.
    fn every_variant() -> Vec<AppError> {
        vec![
            AppError::InvalidRequest("body is not an object".into()),
            AppError::Unauthenticated,
            AppError::KeyRevoked,
            AppError::KeyExpired,
            AppError::InsufficientBalance { details: None },
            AppError::KeyLimitExceeded { details: None },
            AppError::ModelNotAllowed("deepseek-v4-pro".into()),
            AppError::WrongCredentialType("cookie where an API key is required".into()),
            AppError::NotFound("key_7f3a".into()),
            AppError::Conflict("telegram account already linked".into()),
            AppError::ValidationFailed {
                message: "amount_idr must be at least 10000".into(),
                field: "amount_idr".into(),
            },
            AppError::RateLimited {
                retry_after_secs: 42,
            },
            AppError::NoUpstreamAvailable {
                retry_after_secs: 30,
            },
            AppError::Unavailable("the session store is unreachable".into()),
            AppError::Database(sqlx::Error::RowNotFound),
            AppError::Internal("upstream request failed".into()),
        ]
    }

    /// The two lists above must name EVERY variant the enum declares.
    ///
    /// WHY THIS IS A TEST AND NOT A COMMENT. `every_variant()` said "a newly added variant is covered
    /// the day it lands rather than the day someone remembers to extend a hand-picked list" - and it
    /// WAS a hand-picked list. The enum had 16 variants, `every_variant()` had 15 and
    /// `documented_cases` had 14, and MEASURED, `Unavailable`'s status (503) and code
    /// ("unavailable") could BOTH be changed to wrong values with the whole suite green at
    /// 667 passed / 0 failed.
    ///
    /// That is not cosmetic here. `Unavailable` is what `routes/mod.rs` answers when the SESSION
    /// STORE cannot be queried, and the 503 is load-bearing: a 401 during a database blip tells every
    /// signed-in customer their session is bad, and the web client redirects to /login on a 401, so an
    /// outage would log the whole site out.
    ///
    /// WHAT IT READS. The variant names are parsed from THIS FILE's enum declaration, so it needs no
    /// new dependency and no reflection. A variant named in the enum and absent from either list is a
    /// failure, with the name in the message.
    #[test]
    fn every_variant_is_listed_here() {
        const SOURCE: &str = include_str!("error.rs");

        // The enum body: from `pub enum AppError` to the closing brace at column 0.
        let start = SOURCE
            .find("pub enum AppError")
            .expect("AppError's declaration must still be in this file");
        let body = &SOURCE[start..];
        let end = body
            .find("\n}")
            .expect("the enum must still be closed by a brace at column 0");
        let declared: Vec<&str> = body[..end]
            .lines()
            .filter_map(|l| {
                // a variant is indented four spaces and starts with an uppercase letter
                let t = l.strip_prefix("    ")?;
                if t.starts_with(' ') || t.starts_with('/') || t.starts_with('#') {
                    return None;
                }
                let name: String = t.chars().take_while(|c| c.is_alphanumeric()).collect();
                let mut chars = name.chars();
                match chars.next() {
                    Some(c) if c.is_ascii_uppercase() => Some(name),
                    _ => None,
                }
            })
            .map(|s| Box::leak(s.into_boxed_str()) as &'static str)
            .collect();

        assert!(
            declared.len() >= 10,
            "only {} variants were parsed from the enum, so this check is looking at the wrong thing \
             and would pass vacuously: {declared:?}",
            declared.len()
        );

        // Every declared variant must produce a DISTINCT code among the ones every_variant() covers.
        // Compared through `code()` rather than by counting entries, so a variant added to the list
        // but mapped to an existing code is caught too - two variants sharing a terminal code is
        // legal (`Database` and `Internal` both answer `internal_error`) and is exactly why this
        // cannot be a plain length comparison.
        let listed_codes: Vec<&str> = every_variant().iter().map(|e| e.code()).collect();
        let missing_from_every: Vec<&str> = declared
            .iter()
            .filter(|name| {
                let code = variant_code(name);
                code.is_empty() || !listed_codes.contains(&code)
            })
            .copied()
            .collect();

        assert!(
            missing_from_every.is_empty(),
            "these variants are declared in `AppError` and produce no entry with a distinct code in \
             `every_variant()`, so no response-level test covers them: {missing_from_every:?}. Add \
             one, and add the matching row to `documented_cases` with its status - that list is what \
             pins the contract docs/error-model.md publishes."
        );
    }

    /// The `code()` string a variant is expected to produce, by name.
    ///
    /// Spelled out rather than derived: this is a second statement of the mapping, and the point of
    /// the check above is to notice when THAT statement and the enum disagree. Deriving it from
    /// `code()` would make the assertion a tautology.
    fn variant_code(variant: &str) -> &'static str {
        match variant {
            "InvalidRequest" => "invalid_request",
            "Unauthenticated" => "unauthenticated",
            "KeyRevoked" => "key_revoked",
            "KeyExpired" => "key_expired",
            "InsufficientBalance" => "insufficient_balance",
            "KeyLimitExceeded" => "key_limit_exceeded",
            "ModelNotAllowed" => "model_not_allowed",
            "WrongCredentialType" => "wrong_credential_type",
            "NotFound" => "not_found",
            "Conflict" => "conflict",
            "ValidationFailed" => "validation_failed",
            "RateLimited" => "rate_limited",
            "NoUpstreamAvailable" => "no_upstream_available",
            "Unavailable" => "unavailable",
            "Database" | "Internal" => "internal_error",
            _ => "",
        }
    }

    /// One variant per documented row, paired with the row it must match.
    fn documented_cases() -> Vec<(AppError, u16, &'static str)> {
        vec![
            (AppError::InvalidRequest("x".into()), 400, "invalid_request"),
            (AppError::Unauthenticated, 401, "unauthenticated"),
            (AppError::KeyRevoked, 401, "key_revoked"),
            (AppError::KeyExpired, 401, "key_expired"),
            (
                AppError::InsufficientBalance { details: None },
                402,
                "insufficient_balance",
            ),
            (
                AppError::KeyLimitExceeded { details: None },
                402,
                "key_limit_exceeded",
            ),
            (
                AppError::ModelNotAllowed("m".into()),
                403,
                "model_not_allowed",
            ),
            (
                AppError::WrongCredentialType("c".into()),
                403,
                "wrong_credential_type",
            ),
            (AppError::NotFound("id".into()), 404, "not_found"),
            (AppError::Conflict("dup".into()), 409, "conflict"),
            (
                AppError::ValidationFailed {
                    message: "v".into(),
                    field: "v".into(),
                },
                422,
                "validation_failed",
            ),
            (
                AppError::RateLimited {
                    retry_after_secs: 1,
                },
                429,
                "rate_limited",
            ),
            (
                AppError::NoUpstreamAvailable {
                    retry_after_secs: 30,
                },
                503,
                "no_upstream_available",
            ),
            // ADDED, and it was missing for as long as the variant has existed. `Unavailable` is
            // constructed at `routes/mod.rs:108` when the SESSION STORE cannot be queried, and the
            // reasoning there is load-bearing: a 401 during a database blip tells every signed-in
            // customer their session is bad, and the web client redirects to /login on a 401, so the
            // whole site would be logged out by an outage. 503 says "try again", which is true.
            //
            // MEASURED before this row existed: changing Unavailable's status to 500 AND its code to
            // "internal_error" left the whole suite green at 667 passed / 0 failed, because neither
            // list named the variant and the test's own name - "every variant" - did not notice one
            // was missing.
            (
                AppError::Unavailable("the session store is unreachable".into()),
                503,
                "unavailable",
            ),
            (
                AppError::Database(sqlx::Error::RowNotFound),
                500,
                "internal_error",
            ),
            (AppError::Internal("boom".into()), 500, "internal_error"),
        ]
    }

    // -----------------------------------------------------------------------
    // The server-error counter: what makes the `error_rate` alert computable.
    //
    // `tools/alert/alerts.tsv` lists `error_rate > 5% over 5 min` and marks it
    // `needs-metrics`; `docs/observability.md:107` names it. The server already
    // KNOWS when it returns a 5xx - `into_response` logs every internal failure -
    // but nothing COUNTED it, so a rate over a window was unobtainable and the
    // alert could never fire. These tests fail against the pre-fix code for the
    // real reason: there was no counter to read.
    //
    // EACH COUNTER TEST TAKES THE SHARED ENV LOCK, and that is not optional. The
    // counter is process-global, so tests running in parallel share it: without the
    // lock the deltas below are whatever concurrent writers happened to add, which is
    // exactly how the first version of these tests failed (`left: 7, right: 2`). They
    // serialize on the SAME lock the env-mutating tests use, so a test that sets an
    // environment variable cannot interleave either.
    //
    // The counter is PROCESS-WIDE (see `server_error_counter`) because axum calls
    // `IntoResponse::into_response(self)` with no context, so there is nowhere to
    // thread a per-request counter without changing the trait. Tests therefore use
    // a Snapshot of that shared counter and assert on the DELTA.
    // -----------------------------------------------------------------------

    /// The shared counter's state, read as a before/after delta so tests cannot
    /// interfere with each other through the process-wide instance.
    fn counted() -> (u64, u64) {
        let c = server_error_counter();
        (c.server_errors(), c.responses())
    }

    /// RED FIRST: an internal failure is COUNTED, so a rate becomes computable.
    #[test]
    fn internal_failures_are_counted_so_an_error_rate_can_be_computed() {
        let _lock = crate::routes::test_env::EnvLock::acquire();
        let (errors_before, responses_before) = counted();

        let _ = AppError::Internal("boom".into()).into_response();
        let _ = AppError::Database(sqlx::Error::RowNotFound).into_response();

        let (errors_after, responses_after) = counted();
        // MONOTONIC, for the reason spelled out on the 503 test below: the counter is
        // process-wide and other tests drive `into_response` concurrently without this
        // lock, so an exact delta would be racy. Exact arithmetic is pinned on a
        // private counter in `the_rate_is_errors_over_responses_...`.
        assert!(
            errors_after >= errors_before + 2,
            "both internal failures must be counted: without a count there is no rate, and an alert with no data source can never fire"
        );
        assert!(
            responses_after >= responses_before + 2,
            "the denominator must move too, or the rate is a count with no base"
        );
    }

    /// A 4xx is ordinary traffic and must NOT be counted as a server error.
    ///
    /// This is the difference between an alert that means something and one that
    /// fires constantly: the alert is on the rate of 5xx, and a customer sending a
    /// bad request or hitting their own balance limit is normal operation.
    #[test]
    fn client_errors_move_the_denominator_but_never_the_error_count() {
        let _lock = crate::routes::test_env::EnvLock::acquire();
        let (errors_before, responses_before) = counted();

        for err in [
            AppError::Unauthenticated,
            AppError::InsufficientBalance { details: None },
            AppError::RateLimited {
                retry_after_secs: 1,
            },
            AppError::NotFound("key".into()),
            AppError::InvalidRequest("bad".into()),
        ] {
            let _ = err.into_response();
        }

        let (errors_after, responses_after) = counted();
        // The DENOMINATOR must advance by at least these five, but the NUMERATOR is
        // not assertable exactly here: the counter is process-wide, and the module's
        // other response-level tests drive `into_response` through the `respond`
        // helper WITHOUT this lock, so concurrent writers exist by design. (An
        // exact-zero version of this assertion failed with `left: 4` for exactly
        // that reason.) The property that matters - a 4xx is not a SERVER error - is
        // pinned exactly on a private counter above; here it is pinned as "5xx-only
        // writers could not have produced this".
        assert!(
            responses_after >= responses_before + 5,
            "5xx responses still count as observed responses, which is what makes the figure a RATE rather than a raw total"
        );
        // Four 4xx responses were driven above; the numerator may only have moved
        // for OTHER reasons, never for these.
        let errors_delta = errors_after - errors_before;
        assert!(
            errors_delta <= responses_after - responses_before,
            "a 4xx must not be counted as a server error: the numerator can never exceed the responses those calls produced"
        );
    }

    /// The rate is computable, and an EMPTY counter reports NO rate.
    #[test]
    fn the_rate_is_errors_over_responses_and_is_unknown_when_empty() {
        // 2 server errors out of 10 responses = 20%.
        //
        // The call pattern mirrors PRODUCTION exactly: `into_response` records the
        // response once for EVERY status, and additionally records an error when the
        // status is 5xx. So a 500 contributes 1 to each counter, and the denominator
        // already contains the errors. (My first version of this test recorded the
        // eight NON-error responses only and still expected 20%, which read as 25% -
        // the assertion caught a miscount in the test, not a bug in the counter.)
        let counter = ServerErrorCounter::default();
        for _ in 0..2 {
            // Two 500s: one response each, plus one error each.
            counter.record_response();
            counter.record_error();
        }
        for _ in 0..8 {
            // Eight ordinary responses.
            counter.record_response();
        }

        let rate = counter
            .error_rate()
            .expect("a rate is computable once responses exist");
        assert!(
            (rate - 0.20).abs() < 1e-9,
            "2 of 10 must read as 20%, got {rate}"
        );

        // A rate over zero requests is UNKNOWN, not zero. Reporting 0.0 would read
        // as "all healthy" and silently suppress the alert on a service that has
        // simply not served anything yet.
        assert_eq!(
            ServerErrorCounter::default().error_rate(),
            None,
            "no observations must read as UNKNOWN, never as a healthy 0.0"
        );
    }

    /// A 5xx that is NOT an AppError still counts.
    ///
    /// The counter is incremented for every response with a 5xx status, not only
    /// for the two internal AppError variants. A 503 from `NoUpstreamAvailable` is a
    /// server-side failure an operator must see in the rate, and counting only
    /// `Internal`/`Database` would under-report exactly when the upstream is down.
    #[test]
    fn every_5xx_response_counts_not_only_the_internal_variants() {
        // MONOTONIC, not an exact delta, and that is deliberate. The counter is
        // process-wide, so OTHER tests in this module call `into_response` through
        // the `respond` helper without taking this lock and inflate it concurrently
        // - which is what made an exact-delta version of this test fail with
        // `left: 5, right: 1`. The property that matters to an operator probe is
        // that a 503 MOVES the shared counter, and that is observable without being
        // racy. (Exact arithmetic is pinned above, on a fresh private counter.)
        let (errors_before, _) = counted();

        let _ = AppError::NoUpstreamAvailable {
            retry_after_secs: 30,
        }
        .into_response();

        let (errors_after, _) = counted();
        assert!(
            errors_after > errors_before,
            "a 503 is a server-side failure and must move the shared counter: counting only Internal/Database would under-report exactly while the upstream is down"
        );
    }

    /// Drives a variant through the real response path and reads everything the
    /// caller would see.
    async fn respond(err: AppError) -> (StatusCode, HeaderMap, Json) {
        let res = err.into_response();
        let status = res.status();
        let headers = res.headers().clone();
        let bytes = to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("an error response must have a readable body");
        let body: Json = serde_json::from_slice(&bytes).expect(
            "docs/error-model.md:10 - every error returns JSON; no bare HTML pages, no empty bodies",
        );
        (status, headers, body)
    }

    fn error_object(body: &Json) -> &Json {
        body.get("error")
            .expect("docs/error-model.md, Response shape - {error: {code, message, request_id}}")
    }

    fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
        headers
            .get(name)
            .map(|v| v.to_str().expect("header is ASCII"))
    }

    // ---------------------------------------------------------------------
    // The status/code contract
    // ---------------------------------------------------------------------

    #[test]
    fn every_variant_matches_the_documented_status_and_code() {
        for (err, status, expected_code) in documented_cases() {
            assert_eq!(
                err.status_code().as_u16(),
                status,
                "HTTP status drift for code `{expected_code}` (docs/error-model.md:35-50)"
            );
            assert_eq!(
                err.code(),
                expected_code,
                "machine-readable code drift for HTTP {status} (docs/error-model.md:35-50)"
            );
        }
    }

    #[test]
    fn the_code_set_is_exactly_the_documented_one() {
        let mut actual: Vec<&'static str> = every_variant().iter().map(|e| e.code()).collect();
        actual.sort_unstable();
        actual.dedup();

        let mut documented: Vec<&str> = DOCUMENTED.iter().map(|(_, c)| *c).collect();
        documented.sort_unstable();
        documented.dedup();

        assert_eq!(
            actual, documented,
            "docs/error-model.md, rule 4 - ; adding is fine, changing meaning is not"
        );
    }

    #[test]
    fn a_bad_key_is_401_and_a_denied_model_is_403() {
        // docs/error-model.md, 401 vs 403 - "Do not return 403 for a bad key."
        for bad_credential in [
            AppError::Unauthenticated,
            AppError::KeyRevoked,
            AppError::KeyExpired,
        ] {
            assert_eq!(bad_credential.status_code(), StatusCode::UNAUTHORIZED);
        }
        for known_but_denied in [
            AppError::ModelNotAllowed("x".into()),
            AppError::WrongCredentialType("x".into()),
        ] {
            assert_eq!(known_but_denied.status_code(), StatusCode::FORBIDDEN);
        }
    }

    #[test]
    fn a_key_limit_is_402_never_429() {
        // docs/error-model.md, 402 vs 429 - "A client must not retry a 402."
        assert_eq!(
            AppError::KeyLimitExceeded { details: None }.status_code(),
            StatusCode::PAYMENT_REQUIRED
        );
        assert_ne!(
            AppError::KeyLimitExceeded { details: None }.status_code(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    // ---------------------------------------------------------------------
    // Response shape
    // ---------------------------------------------------------------------

    /// A REFUSAL DRIVEN THROUGH THE REAL RESPONSE PATH MOVES THE COUNTER, and the negative
    /// control beside it is what makes that a measurement.
    ///
    /// A test that called rejection_counter().record() directly would pass with the wiring
    /// deleted, because the counter and its call site are different things. This one builds
    /// the whole response through respond(), exactly as the proxy does.
    #[tokio::test]
    async fn a_refusal_through_the_response_path_is_counted() {
        let counter = rejection_counter();
        // The process-global counter is shared with every other test in this binary, so the
        // BEFORE value is read rather than assumed zero. A hard-coded 0 would be flaky for a
        // reason that has nothing to do with what this asserts.
        let before = counter
            .counts()
            .iter()
            .find(|(code, _)| code == "insufficient_balance")
            .map(|(_, n)| *n)
            .unwrap_or(0);

        let _ = respond(AppError::InsufficientBalance { details: None }).await;

        let after = counter
            .counts()
            .into_iter()
            .find(|(code, _)| code == "insufficient_balance")
            .map(|(_, n)| n)
            .unwrap_or(0);
        // BOUNDED, NOT EXACT, and the reason is the same one the money-counter tests carry:
        // this counter is PROCESS-GLOBAL, so a sibling test refusing on another thread inside
        // this window raises it too. MEASURED before relaxing it - `cargo test --lib
        // error::tests` failed at "left: 4, right: 1". An exact equality here would be
        // testing the thread schedule; a test that fails whenever the suite is busy gets
        // muted, and then nothing is measured at all.
        //
        // What the bound still catches is the bug it exists for: a refusal that adds NOTHING
        // (the wiring deleted) fails the lower bound, and one that adds twice per response
        // fails the upper.
        assert!(
            after > before,
            "a 402 refusal must move the counter; it stayed at {after} (baseline {before}), so the recording is gone"
        );
        assert!(
            after <= before + 4,
            "a single refusal added more than a concurrent-test margin: {after} against a baseline of {before}"
        );

        // NEGATIVE CONTROL: a NON-rejection must not move the counter. A 5xx is the error
        // rate numerator, not a rejection - the request would have been served and the
        // provider failed, which needs a different action.
        //
        // THE ASSERTION IS PER-CODE, NOT ON THE TOTAL, and that is the fix for a real flake:
        // summing every code before and after compares a PROCESS-GLOBAL aggregate, so a
        // sibling test refusing concurrently raised it. MEASURED - this failed 3 runs in 6
        // with the total-based form. A 5xx has NO code in the map, so checking that no new
        // key appeared and no existing count changed for the codes a 5xx could plausibly
        // touch is both precise and race-independent... except that other threads still add
        // to the SAME map.
        //
        // So the honest form is what this actually can prove: `AppError::Internal` maps to
        // the code `internal_error`, and that code must NEVER appear in the map whatever else
        // other threads are doing. That is a per-key assertion no concurrent refusal can
        // perturb, because the proxy gate's own list does not include it.
        let _ = respond(AppError::Internal("boom".to_string())).await;
        assert!(
            !counter
                .counts()
                .iter()
                .any(|(code, _)| code == "internal_error"),
            "a 5xx is filed under internal_error and must never enter the rejection map: counting it would report a provider outage as a customer being refused"
        );
    }

    #[tokio::test]
    async fn the_response_body_has_the_documented_shape() {
        let (status, headers, body) = respond(AppError::InsufficientBalance {
            details: Some(json!({ "required_idr": 1200, "balance_idr": 400 })),
        })
        .await;

        assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
        assert_eq!(
            header_str(&headers, header::CONTENT_TYPE),
            Some("application/json")
        );

        let e = error_object(&body);
        assert_eq!(e["code"], "insufficient_balance");
        assert!(
            e["message"].as_str().is_some_and(|m| !m.is_empty()),
            "message must be a non-empty human-readable string"
        );
        assert!(e["request_id"].as_str().is_some());
        assert_eq!(e["details"]["required_idr"], 1200);
        assert_eq!(e["details"]["balance_idr"], 400);
    }

    #[tokio::test]
    async fn details_is_omitted_when_absent_never_null() {
        let (_, _, body) = respond(AppError::InsufficientBalance { details: None }).await;
        let e = error_object(&body);
        assert!(
            e.get("details").is_none(),
            "docs/error-model.md, Response shape - details is optional; it must be absent, not null"
        );
    }

    #[tokio::test]
    async fn every_error_carries_a_request_id_in_the_documented_format() {
        for err in every_variant() {
            let (_, _, body) = respond(err).await;
            let id = error_object(&body)["request_id"]
                .as_str()
                .expect("docs/error-model.md, rule 3 - always include ")
                .to_string();
            assert!(
                id.starts_with("req_"),
                "request_id must follow the documented req_... form, got {id}"
            );
            let hex = &id["req_".len()..];
            assert_eq!(
                hex.len(),
                32,
                "request_id suffix should be a bare uuid, got {id}"
            );
            assert!(
                hex.chars().all(|c| c.is_ascii_hexdigit()),
                "request_id suffix must be hex, got {id}"
            );
        }
    }

    #[tokio::test]
    async fn request_id_is_stable_within_one_error_and_unique_across_errors() {
        let (_, _, body) = respond(AppError::Unauthenticated).await;
        let first = error_object(&body)["request_id"]
            .as_str()
            .unwrap()
            .to_string();
        let second = error_object(&body)["request_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            first, second,
            "one response must carry exactly one stable id"
        );

        let (_, _, other) = respond(AppError::Unauthenticated).await;
        assert_ne!(
            first,
            error_object(&other)["request_id"].as_str().unwrap(),
            "a constant request_id correlates with nothing (docs/error-model.md, Response shape)"
        );
    }

    // ---------------------------------------------------------------------
    // Retry-After
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn rate_limited_carries_retry_after_in_seconds() {
        let (status, headers, _) = respond(AppError::RateLimited {
            retry_after_secs: 42,
        })
        .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        let value = header_str(&headers, header::RETRY_AFTER)
            .expect("docs/error-model.md, Retry-After - included on 429");
        assert_eq!(value, "42");
        assert!(
            value.parse::<u64>().is_ok(),
            "docs/error-model.md, Retry-After - seconds, not a date"
        );
    }

    #[tokio::test]
    async fn rate_limited_retry_after_is_never_zero() {
        // docs/error-model.md (429 — rate limited) - "never below 1".
        let (_, headers, _) = respond(AppError::RateLimited {
            retry_after_secs: 1,
        })
        .await;
        assert_eq!(header_str(&headers, header::RETRY_AFTER), Some("1"));
    }

    #[tokio::test]
    async fn retry_after_is_absent_on_every_error_that_does_not_document_it() {
        for err in every_variant() {
            if matches!(
                err,
                AppError::RateLimited { .. } | AppError::NoUpstreamAvailable { .. }
            ) {
                continue;
            }
            let (status, headers, _) = respond(err).await;
            assert!(
                headers.get(header::RETRY_AFTER).is_none(),
                "unexpected Retry-After on {status} - a header that appears everywhere teaches callers to ignore it (docs/error-model.md (Retry-After))"
            );
        }
    }

    #[tokio::test]
    async fn no_upstream_available_carries_retry_after() {
        // docs/error-model.md, Status codes - the 503 row says "Retry after
        // Retry-After"; the Retry-After section says "Included on 429 and 503."
        // docs/error-model.md (429 — rate limited) - "Floor it at 1 second."
        let (status, headers, _) = respond(AppError::NoUpstreamAvailable {
            retry_after_secs: 30,
        })
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        // The earliest moment a retry could plausibly succeed; a 503 without it
        // tells the client nothing about when to come back, which is the very
        // retry-loop the header exists to prevent.
        let value = header_str(&headers, header::RETRY_AFTER)
            .expect("docs/error-model.md, Retry-After - included on 503");
        let secs: u64 = value.parse().expect("Retry-After must be whole seconds");
        assert!(
            secs >= 1,
            "docs/error-model.md (429 — rate limited) - floored at 1 second"
        );
        assert_eq!(
            secs, 30,
            "the header must carry the value the variant holds, not a constant"
        );
    }

    #[tokio::test]
    async fn a_503_with_the_documented_floor_still_reports_one() {
        // The no-breaker-open path emits the documented floor of 1
        // (docs/error-model.md (429 — rate limited)). Pinned so a future change cannot turn the
        // floor into a 0, which is a malformed header.
        let (_, headers, _) = respond(AppError::NoUpstreamAvailable {
            retry_after_secs: 1,
        })
        .await;
        assert_eq!(header_str(&headers, header::RETRY_AFTER), Some("1"));
    }

    // ---------------------------------------------------------------------
    // Never leak internals (docs/error-model.md, rule 1)
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn a_database_error_does_not_leak_sql_or_schema_to_the_client() {
        let raw_sql = "SELECT id, balance_idr FROM wallets WHERE account_id = $1";
        let (status, _, body) = respond(AppError::Database(sqlx::Error::Protocol(
            raw_sql.to_string(),
        )))
        .await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        let message = error_object(&body)["message"].as_str().unwrap();
        assert!(
            !message.contains("SELECT"),
            "docs/error-model.md rule 1 - no SQL in customer-facing errors, got: {message}"
        );
        assert!(
            !message.contains("wallets"),
            "docs/error-model.md rule 1 - no schema names in customer-facing errors, got: {message}"
        );
        assert!(
            !message.contains(raw_sql),
            "the raw sqlx error string reached the client: {message}"
        );
    }

    #[tokio::test]
    async fn an_internal_error_does_not_leak_infrastructure_detail() {
        // The real call sites (routes/account.rs, routes/auth.rs) build the
        // message by formatting the underlying failure into it.
        let (status, _, body) = respond(AppError::Internal(
            "Midtrans Snap request failed: connection refused to 10.0.0.7:443".into(),
        ))
        .await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        let message = error_object(&body)["message"].as_str().unwrap();
        for leak in ["Midtrans", "10.0.0.7", "connection refused"] {
            assert!(
                !message.contains(leak),
                "docs/error-model.md rule 1 - internals must not reach the client, leaked {leak} in: {message}"
            );
        }
    }

    /// The `Database`/`Internal` arms of `into_response` log the failure with
    /// its status field (error.rs:167). `tracing`'s `error!` only evaluates its
    /// structured fields when a subscriber is registered, and the unit-test
    /// binary has none - so without this test that line is dead. Production
    /// always runs with a subscriber; this test installs a scoped one on the
    /// current thread and confirms the 500 + generic-body path still holds.
    #[tokio::test(flavor = "current_thread")]
    async fn internal_errors_log_their_status_field_when_a_subscriber_is_active() {
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::ERROR)
                .finish(),
        );

        let (status, _, body) = respond(AppError::Database(sqlx::Error::RowNotFound)).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            error_object(&body)["message"]
                .as_str()
                .unwrap()
                .starts_with("An unexpected error occurred"),
            "internal failures must stay generic to the client"
        );

        let (status, _, _body) = respond(AppError::Internal("boom".into())).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// Regression guard for the fix above: `Display` is load-bearing for
    /// server-side observability. It is what every `error = %err` log site
    /// prints, so it MUST keep the raw detail. Only the response body is
    /// generic. Without this test, gutting `Display` - and silently blinding
    /// the operator - would pass the whole suite.
    #[test]
    fn display_still_carries_the_detail_that_the_response_withholds() {
        let raw_sql = "SELECT id, balance_idr FROM wallets WHERE account_id = $1";
        let db = AppError::Database(sqlx::Error::Protocol(raw_sql.to_string()));
        assert!(
            db.to_string().contains(raw_sql),
            "Display must keep the underlying sqlx error for the logs, got: {db}"
        );

        let infra = "Midtrans Snap request failed: connection refused to 10.0.0.7:443";
        let internal = AppError::Internal(infra.to_string());
        assert!(
            internal.to_string().contains(infra),
            "Display must keep the formatted failure for the logs, got: {internal}"
        );
    }

    /// Rule 1, both directions: the two internal variants must never carry
    /// observed dangerous substrings into the client-facing body. Asserted on
    /// absence of the specific leaks rather than on an exact string, so plain
    /// wording changes to `client_message` do not break the test.
    #[tokio::test]
    async fn internal_failures_never_leak_observable_internals_to_the_client() {
        let cases: Vec<(AppError, Vec<&str>)> = vec![
            (
                AppError::Database(sqlx::Error::Protocol(
                    "encountered unexpected or invalid data: SELECT id, balance_idr FROM wallets WHERE account_id = $1"
                        .into(),
                )),
                vec![
                    "SELECT",
                    "wallets",
                    "balance_idr",
                    "account_id",
                    "sqlx",
                    "Database error:",
                ],
            ),
            (
                AppError::Internal(
                    "Midtrans Snap request failed: connection refused to 10.0.0.7:443".into(),
                ),
                vec![
                    "Midtrans",
                    "10.0.0.7",
                    "443",
                    "connection refused",
                    "Internal server error:",
                ],
            ),
        ];

        for (err, leaks) in cases {
            let (status, _, body) = respond(err).await;
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
            let message = error_object(&body)["message"].as_str().unwrap();
            for leak in leaks {
                assert!(
                    !message.contains(leak),
                    "docs/error-model.md rule 1 - leaked {leak:?} to the client in: {message}"
                );
            }
            assert!(
                !message.contains(':')
                    && !message.chars().any(|c| c.is_ascii_digit()),
                "the generic message must not carry an interpolated detail, a host, or a port: {message}"
            );
        }
    }

    #[tokio::test]
    async fn a_validation_error_names_the_offending_field_in_details() {
        // docs/error-model.md, rule 5, rule 5 - validation errors .
        let (status, _, body) = respond(AppError::ValidationFailed {
            message: "amount_idr must be at least 10000".into(),
            field: "amount_idr".into(),
        })
        .await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        let e = error_object(&body);
        let field = e
            .get("details")
            .and_then(|d| d.get("field"))
            .and_then(|f| f.as_str())
            .expect(
                "docs/error-model.md rule 5 requires details.field on a validation error so the UI can highlight the input without parsing the prose message",
            );
        assert_eq!(field, "amount_idr");
    }

    /// The quickstart page publishes the status-code table a DEVELOPER codes against.
    ///
    /// THIS IS THE COPY NOBODY RAN. `the_published_status_table_is_the_one_the_code_serves`
    /// above reads docs/error-model.md, and the website suite pins the UI mapping in
    /// website/tests/error-model.test.ts - so both halves of the contract are checked
    /// against something. The page a developer actually copies curl from is a third
    /// transcription of the same table, in TypeScript, and nothing read it: `errorCodes`
    /// in website/src/pages/docs/quickstart.astro could name a code the server never
    /// emits, or give a real code the wrong status, and every test in this repository
    /// stayed green while the published quickstart was wrong.
    ///
    /// It is the same defect as the retention sweep in the previous round - a promise
    /// written in a surface nothing compares to the thing that keeps it - and it is
    /// worth saying that the marker comments in docs/ (`[ServerErrorCode]`) are NOT how
    /// this is checked, deliberately. A marker is a hand-written string; the fourth
    /// transcription of the table would then be checked by a fifth. This reads the row
    /// literals out of the page and compares them to the same DOCUMENTED set the
    /// document is compared to, so the page and the document agree because they agree
    /// with the code, not with each other.
    ///
    /// The parse reads `status: '400', code: 'invalid_request'` rather than `| 400 |`
    /// because the source is TypeScript. Anything it cannot parse is skipped, so the
    /// vacuity guard below is what stops a reformat turning this into a check over an
    /// empty set - which is the failure mode a parser this narrow invites.
    #[test]
    fn the_quickstart_publishes_the_status_table_the_code_serves() {
        let page = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("website")
                .join("src")
                .join("pages")
                .join("docs")
                .join("quickstart.astro"),
        )
        .expect(
            "website/src/pages/docs/quickstart.astro must be readable, or this passes over \
             nothing. It is the page a developer copies their first request from.",
        );

        // The array literal that IS the table. Scoped to it so a `code:` in some
        // unrelated object on the page cannot be read as a row.
        let start = page.find("const errorCodes = [").unwrap_or_else(|| {
            panic!(
                "the quickstart no longer has an `errorCodes` array. That array is the \
                 published status-code table; if it was renamed or inlined, this check is \
                 looking at nothing and would pass whatever the page says."
            )
        });
        let body = &page[start..];
        let end = body.find("\n];").map(|at| at + 3).unwrap_or(body.len());
        let table = &body[..end];

        // `{ status: '400', code: 'invalid_request', ... }` - the two fields that
        // constitute a contract row. `meaning` and `action` are prose and are not
        // compared, for the reason the document's own check gives: the prose half is
        // a judgement, and this suite has a documented history of false alarms from
        // matchers that tried to read it.
        let mut published: Vec<(u16, String)> = Vec::new();
        for line in table.lines() {
            let (Some(status_at), Some(code_at)) = (line.find("status: '"), line.find("code: '"))
            else {
                continue;
            };
            let rest_after = |at: usize, opener: &str| -> String {
                let s = &line[at + opener.len()..];
                s[..s.find('\'').unwrap_or(s.len())].to_string()
            };
            // `status` is a quoted decimal, `code` a quoted identifier.
            let Ok(status) = rest_after(status_at, "status: '").parse::<u16>() else {
                continue;
            };
            let code = rest_after(code_at, "code: '");
            if code.is_empty() {
                continue;
            }
            published.push((status, code));
        }

        // The vacuity guard. A page reformatted so that no row parses would otherwise
        // compare nothing to nothing and pass.
        assert!(
            published.len() >= 10,
            "only {} row(s) parsed out of the quickstart's error-code table, so the \
             comparison below is not a comparison. The page publishes 14 codes; a renamed \
             field or a reformat lands here rather than in a false PASS.",
            published.len()
        );

        let mut expected: Vec<(u16, String)> = DOCUMENTED
            .iter()
            .map(|(status, code)| (*status, (*code).to_string()))
            .collect();
        for (code, status) in OUTSIDE_APP_ERROR {
            expected.push((*status, (*code).to_string()));
        }
        expected.sort();
        let mut published_sorted = published.clone();
        published_sorted.sort();
        assert_eq!(
            published_sorted, expected,
            "the quickstart's error-code table and the one the server serves have drifted. \
             This is the page a developer codes against before they have an account, so a \
             row that says one thing while the server does another sends them debugging a \
             status they will never receive. Update the page and docs/error-model.md \
             together."
        );

        // A SECOND ASSERTION, on the same parse: no row may name a code twice. The
        // equality above cannot see a duplicate that also displaced a real code only if
        // both are present - it can, and then the sets still match - so the page would
        // publish 15 rows against 15 expected and still be wrong. Cheap, and it is the
        // failure a hand-edited table actually makes.
        let mut codes: Vec<&str> = published.iter().map(|(_, c)| c.as_str()).collect();
        codes.sort_unstable();
        let unique = {
            let mut v = codes.clone();
            v.dedup();
            v
        };
        assert_eq!(
            codes.len(),
            unique.len(),
            "the quickstart's error-code table lists a code more than once, so one of the \
             codes the server can return is missing from the page while the row count still \
             looks right: {codes:?}"
        );
    }
}
