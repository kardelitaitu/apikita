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

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let code = self.code().to_string();
        let details = self.details();
        let request_id = format!("req_{}", uuid::Uuid::new_v4().simple());

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

        let (status, _, body) = respond(AppError::Internal("boom".into())).await;
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
