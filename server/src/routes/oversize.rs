//! The response the body limit owes a client.
//!
//! WHY THIS IS A LAYER RATHER THAN AN ERROR VARIANT. `DefaultBodyLimit` rejects during EXTRACTION, so
//! the handler never runs and no `AppError` is ever constructed. MEASURED on the running server before
//! this existed: a body over the limit got `413 text/plain`, body `Error` - five bytes, no envelope -
//! while `docs/error-model.md:10` promises "Every error returns the same JSON. No bare HTML error
//! pages, no empty bodies."
//!
//! Adding a variant to `AppError` was tried first and came out DEAD CODE: declared, given a status and
//! a code, and unreachable, because nothing on the extraction path can return it. A variant nothing
//! constructs is the appearance of a fix, which is why this is middleware.
//!
//! WHAT IT FIXES, and what it deliberately does not:
//!
//!   * fixes - the SHAPE. An oversized body now gets the envelope every other error uses, with a `code`
//!     a client can branch on and a `request_id` an operator can grep.
//!   * does NOT fix - the LIMIT. 2 MB is axum's own `DefaultBodyLimit` default, inherited rather than
//!     chosen, and MEASURED to be exact at 2,097,153 bytes. An axum upgrade or a
//!     `DefaultBodyLimit::disable()` would remove it with nothing failing, because nothing pins it.
//!     Making it a decision is a separate change; this one makes the response honest about it.

use axum::body::Body;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

/// The status axum uses for a body-limit rejection, and the one this layer rewrites.
const REJECTED: StatusCode = StatusCode::PAYLOAD_TOO_LARGE;

/// The limit this server actually applies, in bytes.
///
/// axum's `DefaultBodyLimit` default, MEASURED to the byte: 2,097,152 passes and 2,097,153 is
/// rejected. Restated here because the message has to name a number, and named rather than inlined so
/// that the one place it appears is greppable when the limit finally becomes an explicit decision.
const LIMIT_BYTES: usize = 2 * 1024 * 1024;

/// Rewrite a body-limit rejection into the documented error envelope.
///
/// Runs on EVERY response and passes everything else through, so the cost is one status comparison per
/// request.
pub async fn documented_oversize_response(response: Response) -> Response {
    if response.status() != REJECTED {
        return response;
    }

    // Only rewrite the framework's own rejection. A 413 this server produced deliberately already has
    // the envelope, and rewriting it would replace a specific message with a generic one.
    let is_framework_rejection = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/plain"));

    if !is_framework_rejection {
        return response;
    }

    let body = Json(serde_json::json!({
        "error": {
            "code": "payload_too_large",
            "message": format!(
                "The request body is larger than this server accepts ({} bytes).",
                LIMIT_BYTES
            ),
            // Same shape as every other error's id, so an operator's grep works across both.
            "request_id": format!("req_{}", uuid::Uuid::new_v4().simple()),
        }
    }));

    let out = (REJECTED, body).into_response();
    // `Body::empty()` from the original is dropped with `response`; the new body carries its own
    // length, so nothing has to be copied across.
    let _ = Body::empty();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    /// A response shaped the way axum's rejection is: 413 plus a plain-text body.
    fn framework_rejection() -> Response {
        (
            REJECTED,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "Failed to buffer the request body: length limit exceeded",
        )
            .into_response()
    }

    /// A 413 this server produced itself, already enveloped.
    fn ours() -> Response {
        (
            REJECTED,
            Json(serde_json::json!({
                "error": { "code": "payload_too_large", "message": "ours", "request_id": "req_x" }
            })),
        )
            .into_response()
    }

    async fn json_of(r: Response) -> serde_json::Value {
        let body = to_bytes(r.into_body(), 64 * 1024).await.expect("body");
        serde_json::from_slice(&body).expect("the rewritten body must be JSON")
    }

    #[tokio::test]
    async fn a_framework_rejection_becomes_the_documented_envelope() {
        let out = documented_oversize_response(framework_rejection()).await;
        assert_eq!(out.status(), REJECTED);

        let v = json_of(out).await;
        assert_eq!(
            v["error"]["code"], "payload_too_large",
            "the code is the contract field (docs/error-model.md)"
        );
        assert!(
            v["error"]["request_id"]
                .as_str()
                .is_some_and(|s| s.starts_with("req_")),
            "every error carries a request_id, and the framework's rejection carried none"
        );
        assert!(
            v["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("2097152")),
            "the message must name the limit rather than gesture at one"
        );
    }

    #[tokio::test]
    async fn a_413_this_server_produced_is_left_alone() {
        let v = json_of(documented_oversize_response(ours()).await).await;
        assert_eq!(
            v["error"]["message"], "ours",
            "rewriting our own 413 would replace a specific message with a generic one"
        );
    }

    #[tokio::test]
    async fn other_statuses_pass_through_untouched() {
        for status in [
            StatusCode::OK,
            StatusCode::UNAUTHORIZED,
            StatusCode::NOT_FOUND,
            StatusCode::INTERNAL_SERVER_ERROR,
        ] {
            let out = documented_oversize_response(status.into_response()).await;
            assert_eq!(out.status(), status, "a non-413 must not be rewritten");
        }
    }
}
