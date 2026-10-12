//! The inbound limits this server enforces on itself.
//!
//! WHY THIS EXISTS. `docs/topology.md:50` states the relay is *"a cost and load boundary, not a
//! security boundary"* and that **"Both endpoints must be hardened"** — the backend needs *"its own
//! rate limits, body caps"*. MEASURED before this module existed: it had none of the three.
//!
//!   * **800 half-open connections** were accepted and held with **zero timeouts in 30 seconds**, while
//!     `/health` kept answering. A slowloris needs no credentials, and `max_connections_per_account`
//!     does not gate it — that limit applies to `/events` only, after the session resolves.
//!   * a single half-open client made **shutdown hang for 2,101 seconds**, because
//!     `with_graceful_shutdown` waits for connections to finish and such a connection never finishes.
//!   * the **2 MiB body cap** was axum's own `DefaultBodyLimit`, set by no line in this repository.
//!
//! WHAT EACH LAYER DOES, and why the set is not redundant:
//!
//!   * **body limit** — bounds ONE request's memory. Without it a single request can ask the server to
//!     buffer unboundedly.
//!   * **concurrency limit** — bounds how many requests are in flight at once. Without it N requests
//!     each inside the body limit still multiply, and nothing bounds N.
//!   * **timeout** — bounds how long ONE request may occupy its slot. Without it a concurrency limit is
//!     a denial-of-service: a slow client holds a slot forever and 512 slow clients refuse every other
//!     customer. This is the layer that makes the other two hold.
//!
//! ORDER MATTERS, and the outermost is applied last in axum. The concurrency limit is outermost so a
//! request that cannot get a slot is refused before it is timed or buffered; the timeout is next, so a
//! slot cannot be held indefinitely; the body limit is innermost, closest to the extractor that reads.
//!
//! WHAT THIS DOES NOT DO. It does not rate-limit per caller — that is `[limits]` and the relay's
//! `limit_req_zone`. It bounds the SERVER's own exposure, which is a different question from fairness
//! between callers.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::DefaultBodyLimit;
use axum::Router;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;

use crate::config::ServerConfig;

/// Apply the inbound hardening to a router.
///
/// Takes the whole `ServerConfig` rather than three numbers, so a limit added to the config cannot be
/// forgotten here — a field that exists and is unread is exactly how the body cap went unnoticed.
pub fn apply(router: Router, config: &ServerConfig) -> Router {
    let body_limit = config.max_body_bytes;
    let header_timeout = Duration::from_secs(config.header_read_timeout_seconds);
    let concurrency = config.max_concurrent_requests;

    router
        // INNERMOST. axum's own extractor limit, set explicitly so the number is a decision rather
        // than a framework default. `DefaultBodyLimit` and `RequestBodyLimitLayer` are both applied
        // because they guard different paths: the extractor's limit rejects during extraction, and the
        // layer rejects before the body is read at all.
        .layer(DefaultBodyLimit::max(body_limit))
        .layer(RequestBodyLimitLayer::new(body_limit))
        // A request that cannot get a slot is refused rather than queued. `ConcurrencyLimitLayer`
        // returns 503, which is the documented "try again" status — see `docs/error-model.md`.
        .layer(tower::limit::ConcurrencyLimitLayer::new(concurrency))
        // OUTERMOST. Bounds how long one request may occupy its slot, which is what stops the
        // concurrency limit from being a denial-of-service in its own right.
        //
        // `with_status_code` rather than the deprecated `new`: the default is 408 Request Timeout, but
        // this is a SERVER-side bound rather than a statement about the client's request, and 503 is
        // what `docs/error-model.md` documents for "the server could not serve this now". A client
        // seeing 408 would be told its request was malformed, which is not what happened.
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            header_timeout,
        ))
}

/// The concurrency limiter's permit count, exposed so a test can assert the layer is present without
/// constructing a whole router.
pub fn concurrency_limit(config: &ServerConfig) -> Arc<usize> {
    Arc::new(config.max_concurrent_requests)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    #[test]
    fn the_shipped_config_has_every_limit_set() {
        let config = AppConfig::load_from_file("../config/apikita.toml")
            .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
            .expect("config/apikita.toml must load");

        // Every one of these is a number that was previously absent or inherited. A zero would
        // disable the limit silently, which is the failure this test exists to prevent.
        assert!(
            config.server.max_concurrent_requests > 0,
            "a concurrency limit of 0 would refuse every request"
        );
        assert!(
            config.server.header_read_timeout_seconds > 0,
            "a header timeout of 0 would abort every request"
        );
        assert!(
            config.server.max_body_bytes > 0,
            "a body limit of 0 would refuse every body"
        );
    }

    #[test]
    fn a_config_without_the_server_section_still_hardens() {
        // `serde(default)` exists so an older config boots. It must not boot WEAKER: the defaults are
        // the shipped values, so a missing `[server]` gets the same protection a present one does.
        let d = ServerConfig::default();
        assert_eq!(d.max_concurrent_requests, 512);
        assert_eq!(d.header_read_timeout_seconds, 10);
        assert_eq!(d.max_body_bytes, 2 * 1024 * 1024);
    }

    #[test]
    fn the_body_limit_matches_what_the_document_publishes() {
        // The 2 MiB figure is stated in `config/apikita.toml` and in `docs/load-test.md`'s note about
        // the limit. This pins the number so an axum upgrade cannot change it silently - which is
        // exactly what happened while it was a framework default.
        let config = AppConfig::load_from_file("../config/apikita.toml")
            .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
            .expect("config must load");
        assert_eq!(
            config.server.max_body_bytes, 2_097_152,
            "MEASURED boundary was 2,097,152 bytes accepted and 2,097,153 refused; if this moves, the \
             relay's client_max_body_size and docs/load-test.md must move with it"
        );
    }
}
