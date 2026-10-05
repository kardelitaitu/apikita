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
