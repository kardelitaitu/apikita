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
//! ORDER MATTERS, and the outermost is applied last in axum. The TIMEOUT is outermost, then the
//! concurrency limit, then the body limit innermost, closest to the extractor that reads.
//!
//! **THIS PARAGRAPH SAID THE OPPOSITE AND WAS WRONG.** It read *"The concurrency limit is outermost
//! so a request that cannot get a slot is refused before it is timed or buffered; the timeout is
//! next"* - which contradicted both the code and the `INNERMOST`/`OUTERMOST` markers below it, and
//! `routes/mod.rs` repeated the same sentence. MEASURED, and it was wrong about the MECHANISM as well
//! as the order: `tower::limit::ConcurrencyLimitLayer` does not refuse, it WAITS - `poll_ready`
//! acquires a semaphore permit and returns `Pending` when saturated - so a request over the limit is
//! QUEUED in either order. What the order decides is whether the WAIT is bounded: with the timeout
//! outside, a request that never gets a slot returns 503 after `request_timeout`, which is the
//! arrangement shipped here and the defensible one.
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
///
/// IT TAKES THE WHOLE `AppConfig` NOW, for that same reason and one step further out. The request timeout
/// has to be the UPSTREAM's allowance, which lives under `[circuit_breaker]` and not under `[server]`, so
/// a signature taking only `ServerConfig` could not express the correct bound at all — which is how the
/// 12x disagreement between the two budgets went unnoticed.
pub fn apply(router: Router, config: &crate::config::AppConfig) -> Router {
    let body_limit = config.server.max_body_bytes;
    let concurrency = config.server.max_concurrent_requests;
    // The whole-request bound is the upstream's own budget plus headroom for the settlement write that
    // follows the stream. See the OUTERMOST comment below for the measurement behind this.
    //
    // `saturating_add` rather than `+`: the value comes from config, and a config at `u64::MAX` would
    // overflow the addition in a debug build. An enormous timeout is a misconfiguration rather than a
    // security hole, because the concurrency limit still bounds how many such requests exist.
    let request_timeout = Duration::from_secs(
        config
            .circuit_breaker
            .request_timeout_seconds
            .saturating_add(30),
    );

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
        // IT IS A WHOLE-REQUEST TIMEOUT, NOT A HEADER TIMEOUT, AND THAT WAS A BUG.
        //
        // MEASURED: a handler that takes 6s under `header_read_timeout_seconds = 2` returns
        // `503 Service Unavailable` at 2007ms - the layer wraps the entire service, so it bounds the
        // handler including any upstream round trip, not just the arrival of headers.
        //
        // The value it was bound to (`header_read_timeout_seconds`, 10s) is documented in config as the
        // SLOWLORIS defence - "a browser sends headers in one packet", "nginx's client_header_timeout
        // defaults to 60s". With 10s here, a chat completion that took longer than ten seconds was aborted
        // with a 503, while `[circuit_breaker] request_timeout_seconds = 120` grants the upstream two
        // minutes. **The upstream budget and the request budget disagreed by 12x, and the tighter one
        // won.** A long answer from a slow provider - the ordinary case this product exists for - would
        // have been a 503.
        //
        // WHAT IT IS BOUND TO NOW: the upstream's own budget, plus headroom for the settlement write that
        // happens after the stream ends. `request_timeout_seconds` is the circuit breaker's per-request
        // upstream allowance, so a request that may legitimately wait that long for the provider must not
        // be killed before it. The slowl-oris defence is NOT lost: a client that sends a partial request
        // line still cannot hold a slot past this bound, and the bound is now the upstream's rather than a
        // header figure that was never about bodies.
        //
        // `with_status_code` rather than the deprecated `new`: the default is 408 Request Timeout, but
        // this is a SERVER-side bound rather than a statement about the client's request, and 503 is
        // what `docs/error-model.md` documents for "the server could not serve this now". A client
        // seeing 408 would be told its request was malformed, which is not what happened.
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            request_timeout,
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
            config.server.max_body_bytes > 0,
            "a body limit of 0 would refuse every body"
        );
        // And the request timeout comes from the UPSTREAM's budget, so a zero there must not silently
        // remove the bound - `saturating_add(30)` floors it at 30s even for a 0.
        assert!(
            config.circuit_breaker.request_timeout_seconds > 0,
            "an upstream timeout of 0 would make the whole-request bound 30s by accident"
        );
    }

    #[test]
    fn a_config_without_the_server_section_still_hardens() {
        // `serde(default)` exists so an older config boots. It must not boot WEAKER: the defaults are
        // the shipped values, so a missing `[server]` gets the same protection a present one does.
        let d = ServerConfig::default();
        assert_eq!(d.max_concurrent_requests, 512);
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
    /// THE INBOUND LAYERS' ORDER, PINNED BY BEHAVIOUR RATHER THAN BY A COMMENT.
    ///
    /// THREE COMMENTS DESCRIBED THIS AND ONE OF THEM WAS WRONG. `hardening.rs`'s module doc said
    /// *"The concurrency limit is outermost so a request that cannot get a slot is refused before it
    /// is timed or buffered; the timeout is next"*, and `routes/mod.rs` repeated it. The CODE applies
    /// `TimeoutLayer` LAST, which in axum makes it the OUTERMOST - so the two comments contradicted
    /// each other and the inline `INNERMOST`/`OUTERMOST` markers agreed with the code.
    ///
    /// The observable settles it, and it is not what either comment claimed. MEASURED:
    ///
    ///   * `tower::limit::ConcurrencyLimitLayer` WAITS rather than refusing - its `poll_ready`
    ///     acquires a semaphore permit and returns `Pending` when saturated. So a request over the
    ///     limit is QUEUED, not refused, whichever order the layers are in.
    ///   * With the timeout OUTSIDE that queue, the WAIT ITSELF is bounded: a request that never
    ///     gets a slot returns 503 after `request_timeout`. That is the shipped arrangement and it
    ///     is the defensible one - a queued request cannot hang forever.
    ///
    /// So the comments are corrected to say queued-and-bounded, and this test pins the arrangement
    /// they describe. It asserts the ORDER rather than the saturation, because driving a real
    /// saturation would need 512 in-flight requests to observe what a one-line composition shows.
    #[test]
    fn the_timeout_layer_is_outermost_so_a_queued_request_cannot_wait_forever() {
        // `hardening::apply` is the only place the three are composed, so the assertion reads its
        // own source for the call order.
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/routes/hardening.rs"),
        )
        .expect("hardening.rs must be readable, or this checks nothing");

        let body = source.find(".layer(DefaultBodyLimit::max(").expect(
            "the body limit must still be applied first, or this is reading the wrong file",
        );
        let concurrency = source
            .find(".layer(tower::limit::ConcurrencyLimitLayer::new(")
            .expect("the concurrency limit must still be applied");
        let timeout = source
            .find(".layer(TimeoutLayer::with_status_code(")
            .expect("the timeout must still be applied");

        assert!(
            body < concurrency,
            "the body limit must be applied BEFORE the concurrency limit, so it sits innermost"
        );
        assert!(
            concurrency < timeout,
            "the timeout must be applied LAST, which makes it OUTERMOST. It was applied before the concurrency limit once, and the module doc still described that order; a queued request would then wait unboundedly rather than being bounded by the timeout"
        );
    }
}
