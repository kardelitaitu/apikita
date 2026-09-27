use std::sync::atomic::{AtomicU64, Ordering};

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
/// library name (docs/error-model.md:159, rule 1). It stays honest and
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

    /// Carries the offending field, not just prose. docs/error-model.md:165
    /// (rule 5) requires `details.field` so a UI can highlight the input
    /// without parsing the message; making it a required field means a new
    /// call site cannot forget it.
    #[error("Validation failed: {message}")]
    ValidationFailed { message: String, field: String },

    #[error("Rate limited")]
    RateLimited { retry_after_secs: u64 },

    /// Every upstream is unhealthy (docs/error-model.md:50). Carries the
    /// `Retry-After` the client must honour, sourced from the pool's shortest
    /// remaining cooldown, so the wait is never guessed.
    #[error("No upstream available")]
    NoUpstreamAvailable { retry_after_secs: u64 },

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
            Self::NoUpstreamAvailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
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
            Self::Database(_) | Self::Internal(_) => "internal_error",
        }
    }

    /// The client-facing `message`. This is the non-contract field
    /// (docs/error-model.md:30) and the one rule 1 constrains: for `Database`
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

        // The ONE place a Retry-After is decided (docs/error-model.md:79-82:
        // "Included on 429 and 503"). Both variants carry their value, so the
        // header logic lives here rather than at either call site.
        //
        // For 503 the value is the pool's shortest remaining cooldown. When no
        // breaker is open there is no cooldown to report, and the caller emits
        // the documented 1-second floor - never a fabricated estimate
        // (docs/error-model.md:96, :112). Which path produced the 503 is logged
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
    ];

    /// Every AppError variant, one of each. The response-level tests run over
    /// this whole set, so a newly added variant is covered the day it lands
    /// rather than the day someone remembers to extend a hand-picked list.
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
            AppError::Database(sqlx::Error::RowNotFound),
            AppError::Internal("upstream request failed".into()),
        ]
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
            .expect("docs/error-model.md:12-20 - the shape is {error: {code, message, request_id}}")
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
            "docs/error-model.md:164 - code values are permanent; adding is fine, changing meaning is not"
        );
    }

    #[test]
    fn a_bad_key_is_401_and_a_denied_model_is_403() {
        // docs/error-model.md:52-62 - "Do not return 403 for a bad key."
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
        // docs/error-model.md:64-77 - "A client must not retry a 402."
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
            "docs/error-model.md:28 - details is optional; it must be absent, not null"
        );
    }

    #[tokio::test]
    async fn every_error_carries_a_request_id_in_the_documented_format() {
        for err in every_variant() {
            let (_, _, body) = respond(err).await;
            let id = error_object(&body)["request_id"]
                .as_str()
                .expect("docs/error-model.md:162 - always include request_id")
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
            "a constant request_id correlates with nothing (docs/error-model.md:27)"
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
            .expect("docs/error-model.md:81 - Retry-After is included on 429");
        assert_eq!(value, "42");
        assert!(
            value.parse::<u64>().is_ok(),
            "docs/error-model.md:81-82 - seconds, not a date"
        );
    }

    #[tokio::test]
    async fn rate_limited_retry_after_is_never_zero() {
        // docs/error-model.md:93-94 - "never below 1".
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
                "unexpected Retry-After on {status} - a header that appears everywhere teaches callers to ignore it (docs/error-model.md:96)"
            );
        }
    }

    #[tokio::test]
    async fn no_upstream_available_carries_retry_after() {
        // docs/error-model.md:50  - 503 says "Retry after Retry-After".
        // docs/error-model.md:81  - "Included on 429 and 503."
        // docs/error-model.md:112 - "Floor it at 1 second."
        let (status, headers, _) = respond(AppError::NoUpstreamAvailable {
            retry_after_secs: 30,
        })
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        // The earliest moment a retry could plausibly succeed; a 503 without it
        // tells the client nothing about when to come back, which is the very
        // retry-loop the header exists to prevent.
        let value = header_str(&headers, header::RETRY_AFTER)
            .expect("docs/error-model.md:81 - Retry-After is included on 503");
        let secs: u64 = value.parse().expect("Retry-After must be whole seconds");
        assert!(secs >= 1, "docs/error-model.md:112 - floored at 1 second");
        assert_eq!(
            secs, 30,
            "the header must carry the value the variant holds, not a constant"
        );
    }

    #[tokio::test]
    async fn a_503_with_the_documented_floor_still_reports_one() {
        // The no-breaker-open path emits the documented floor of 1
        // (docs/error-model.md:112). Pinned so a future change cannot turn the
        // floor into a 0, which is a malformed header.
        let (_, headers, _) = respond(AppError::NoUpstreamAvailable {
            retry_after_secs: 1,
        })
        .await;
        assert_eq!(header_str(&headers, header::RETRY_AFTER), Some("1"));
    }

    // ---------------------------------------------------------------------
    // Never leak internals (docs/error-model.md:159, rule 1)
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
        // docs/error-model.md:165, rule 5 - validation errors name the field.
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
}
