//! Upstream circuit breaker.
//!
//! One breaker guards one upstream endpoint. After failure_threshold consecutive
//! failures the breaker trips Open; the cooldown starts at cooldown_seconds and
//! doubles on every failed recovery attempt, capped at cooldown_max_seconds.
//! When the cooldown elapses the breaker goes HalfOpen and admits exactly one
//! trial request; that trial's outcome either closes the breaker (reset) or
//! reopens it with a longer wait.
//!
//! Thread-safe and dependency-free: one Mutex, no I/O, no clock abstraction.

#![cfg_attr(
    not(test),
    // THIS MODULE'S FAILURE MODE IS A PANIC, NOT A WRONG NUMBER, and that is worth
    // recording because it is the opposite of the money modules.
    //
    // `Instant + Duration` does not saturate - a bare `+` that leaves the
    // representable range PANICS. The breaker holds a Mutex while it does this, so
    // the panic happens under a lock, on a request, with no field name in sight.
    //
    // It took a measurement to find the boundary, and it is further out than the
    // shape of the code suggests: a trillion seconds - 31,700 years - is fine, and
    // only a nineteen-digit value panics. So this module is not carrying a live bug.
    // What it carries is an unvalidated config value reaching an operation that
    // panics rather than degrading, which is why config.rs now refuses a cooldown
    // the clock cannot represent at LOAD, where the message can name the field.
    //
    // The remaining site here is the backoff multiplication, which is float and
    // already guarded: cooldown_multiplier is checked for finiteness at its use
    // site AND at config load, and the result is clamped to cooldown_max_seconds.
    deny(clippy::arithmetic_side_effects)
)]

use crate::config::CircuitBreakerConfig;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BreakerState {
    Closed,
    Open,
    HalfOpen,
}

#[derive(Debug)]
struct Inner {
    state: BreakerState,
    /// Consecutive failures since the last success. Reset by record_success.
    consecutive_failures: u32,
    /// Cooldown applied by the current Open window (grows on repeated failure).
    cooldown: Duration,
    /// When the Open window ends and a trial request may be admitted.
    open_until: Option<Instant>,
    /// True while the single HalfOpen trial request is in flight.
    trial_in_flight: bool,
    /// Test-only virtual clock offset, so cooldown waits need no real sleeping.
    #[cfg(test)]
    clock_offset: Duration,
}

pub struct CircuitBreaker {
    cfg: CircuitBreakerConfig,
    inner: Mutex<Inner>,
}

impl CircuitBreaker {
    pub fn new(cfg: CircuitBreakerConfig) -> Self {
        Self {
            cfg,
            inner: Mutex::new(Inner {
                state: BreakerState::Closed,
                consecutive_failures: 0,
                cooldown: Duration::ZERO,
                open_until: None,
                trial_in_flight: false,
                #[cfg(test)]
                clock_offset: Duration::ZERO,
            }),
        }
    }

    /// Current state. An Open window whose cooldown has elapsed reports
    /// HalfOpen: the breaker is ready to admit one trial request.
    pub fn state(&self) -> BreakerState {
        let mut inner = self.lock();
        self.expire_cooldown(&mut inner);
        inner.state
    }

    /// May a request be sent upstream right now?
    ///
    /// Closed admits everything; Open admits nothing until the cooldown
    /// elapses; HalfOpen admits exactly one request (the trial) and rejects
    /// every concurrent caller until that trial is resolved.
    pub fn allow_request(&self) -> bool {
        let mut inner = self.lock();
        self.expire_cooldown(&mut inner);
        match inner.state {
            BreakerState::Closed => true,
            BreakerState::Open => false,
            BreakerState::HalfOpen => {
                if inner.trial_in_flight {
                    false
                } else {
                    inner.trial_in_flight = true;
                    true
                }
            }
        }
    }

    /// How long until this breaker's Open window ends, or `None` when no Open
    /// window is in effect (the breaker is Closed, or already HalfOpen).
    ///
    /// READ-ONLY AND SIDE-EFFECT-FREE, deliberately. It reads `open_until` under
    /// the lock and returns; it does NOT call `expire_cooldown`, so asking this
    /// question can never promote an Open breaker to HalfOpen and can never
    /// change what `allow_request` will answer. A caller reporting a wait must
    /// not perturb the thing it is reporting on.
    ///
    /// A cooldown that has already elapsed still reports `Some(0)` while the
    /// breaker reads Open, because a retry is plausible immediately; the caller
    /// floors the reported value (docs/error-model.md (429 — rate limited)).
    pub fn remaining_cooldown(&self) -> Option<Duration> {
        let inner = self.lock();
        if inner.state != BreakerState::Open {
            return None;
        }
        let until = inner.open_until?;
        Some(until.saturating_duration_since(self.now(&inner)))
    }

    /// A request completed successfully. Closes the breaker and resets both the
    /// failure count and the backoff.
    pub fn record_success(&self) {
        let mut inner = self.lock();
        inner.state = BreakerState::Closed;
        inner.consecutive_failures = 0;
        inner.cooldown = self.base_cooldown();
        inner.open_until = None;
        inner.trial_in_flight = false;
    }

    /// A request failed (5xx, timeout, transport error). Trips the breaker once
    /// the consecutive-failure threshold is reached, and doubles the cooldown
    /// when a HalfOpen trial fails.
    /// The `Instant + Duration` below is SAFE only because config.rs refuses a
    /// cooldown the clock cannot represent, at LOAD, naming the field. A bare
    /// `Instant + Duration` PANICS rather than saturating, and this runs while the
    /// breaker lock is held. `inner.cooldown` is `base_cooldown()` or
    /// `next_cooldown(...)`, and both are bounded by the two cooldown settings that
    /// validation checks with `checked_add`.
    ///
    /// `checked_add` here as well would turn a missed validation into a tripped
    /// breaker rather than a panic - but the two would then disagree about what "too
    /// long" means, and the config check is the one that can name the operator's
    /// mistake. This allow records that the line depends on it.
    ///
    /// On the function, not the statement: an attribute on an expression-statement
    /// needs the unstable `stmt_expr_attributes` feature, and the compiler says so
    /// rather than accepting it.
    #[allow(clippy::arithmetic_side_effects)]
    pub fn record_failure(&self) {
        let mut inner = self.lock();
        inner.consecutive_failures = inner.consecutive_failures.saturating_add(1);
        inner.trial_in_flight = false;

        if inner.state == BreakerState::Open {
            // Stale report from a request admitted before the breaker tripped.
            return;
        }
        // A Closed breaker that trips starts at the base cooldown; a failed
        // HalfOpen trial doubles the cooldown it was already serving.
        inner.cooldown = if inner.state == BreakerState::HalfOpen {
            self.next_cooldown(inner.cooldown)
        } else {
            self.base_cooldown()
        };
        if inner.state == BreakerState::HalfOpen
            || inner.consecutive_failures >= self.cfg.failure_threshold
        {
            inner.state = BreakerState::Open;
            inner.open_until = Some(self.now(&inner) + inner.cooldown);
        }
    }

    /// Promote an Open breaker to HalfOpen once its cooldown has elapsed.
    fn expire_cooldown(&self, inner: &mut Inner) {
        if inner.state != BreakerState::Open {
            return;
        }
        let now = self.now(inner);
        if inner.open_until.is_some_and(|until| now >= until) {
            inner.state = BreakerState::HalfOpen;
            inner.open_until = None;
            inner.trial_in_flight = false;
        }
    }

    fn base_cooldown(&self) -> Duration {
        Duration::from_secs(self.cfg.cooldown_seconds)
    }

    /// Multiply the current cooldown by cooldown_multiplier, clamped to
    /// cooldown_max_seconds. A multiplier of 1.0 or less disables the backoff.
    ///
    /// SAFE, and the guard is the line below it: a non-finite multiplier falls back
    /// to 1.0 rather than producing a NaN duration, which the `.min` at the end
    /// would then have to reason about. `as u64` on a float that is already finite
    /// and non-negative saturates in Rust, and the result is clamped regardless.
    #[allow(clippy::arithmetic_side_effects)]
    fn next_cooldown(&self, current: Duration) -> Duration {
        let multiplier =
            if self.cfg.cooldown_multiplier.is_finite() && self.cfg.cooldown_multiplier > 1.0 {
                self.cfg.cooldown_multiplier
            } else {
                1.0
            };
        let millis = (current.as_millis() as f64 * multiplier).round() as u64;
        Duration::from_millis(millis).min(Duration::from_secs(self.cfg.cooldown_max_seconds))
    }

    /// Current time, shifted by the test-only virtual clock.
    fn now(&self, inner: &Inner) -> Instant {
        #[cfg(test)]
        {
            Instant::now() + inner.clock_offset
        }
        #[cfg(not(test))]
        {
            let _ = inner;
            Instant::now()
        }
    }

    /// Test-only: advance this breaker's virtual clock.
    #[cfg(test)]
    fn advance(&self, elapsed: Duration) {
        let mut inner = self.lock();
        inner.clock_offset += elapsed;
    }

    /// Poisoning is recovered rather than panicked on: a breaker that lost a
    /// thread must keep protecting the endpoint.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            failure_threshold: 3,
            cooldown_seconds: 30,
            cooldown_max_seconds: 900,
            cooldown_multiplier: 2.0,
            request_timeout_seconds: 120,
            health_check_interval_seconds: 30,
            health_check_failures: 2,
        }
    }

    fn advance(b: &CircuitBreaker, secs: u64) {
        b.advance(Duration::from_secs(secs));
    }

    /// Three consecutive failures trip the breaker.
    fn trip(b: &CircuitBreaker) {
        b.record_failure();
        b.record_failure();
        b.record_failure();
        assert_eq!(b.state(), BreakerState::Open);
    }

    #[test]
    fn opens_after_three_consecutive_failures() {
        let b = CircuitBreaker::new(cfg());
        assert_eq!(b.state(), BreakerState::Closed);

        b.record_failure();
        b.record_failure();
        assert_eq!(b.state(), BreakerState::Closed);
        assert!(b.allow_request());

        b.record_failure();
        assert_eq!(b.state(), BreakerState::Open);
    }

    // The cooldown report is READ-ONLY: asking for it must never promote an
    // Open breaker to HalfOpen nor change what allow_request answers.
    /// The virtual clock only SHIFTS `Instant::now()` (see `now`); real time
    /// still advances between a trip and the read, so an exact-equality
    /// assertion on a live countdown is flaky by construction. Compare against
    /// the expected value with a tolerance of the microseconds it takes to get
    /// from one line to the next: the property is "reports the cooldown", not
    /// "reports it to the nanosecond".
    fn assert_cooldown_is(actual: Option<Duration>, expected_secs: u64) {
        let actual = actual.expect("expected an open breaker with a cooldown");
        let expected = Duration::from_secs(expected_secs);
        let drift = actual.abs_diff(expected);
        assert!(
            drift < Duration::from_millis(100),
            "expected about {expected:?}, got {actual:?} (drift {drift:?})"
        );
    }

    #[test]
    fn remaining_cooldown_is_none_until_the_breaker_opens() {
        let b = CircuitBreaker::new(cfg());
        assert_eq!(
            b.remaining_cooldown(),
            None,
            "a Closed breaker has no cooldown"
        );

        b.record_failure();
        b.record_failure();
        assert_eq!(
            b.remaining_cooldown(),
            None,
            "still Closed below the threshold"
        );

        b.record_failure();
        assert_eq!(b.state(), BreakerState::Open);
        assert_cooldown_is(b.remaining_cooldown(), 30);
    }

    #[test]
    fn remaining_cooldown_counts_down_and_ends_with_the_open_window() {
        let b = CircuitBreaker::new(cfg());
        trip(&b);

        advance(&b, 10);
        assert_cooldown_is(b.remaining_cooldown(), 20);
        advance(&b, 19);
        assert_cooldown_is(b.remaining_cooldown(), 1);

        // The window has elapsed: the breaker is HalfOpen and there is no
        // cooldown left to report.
        advance(&b, 1);
        assert_eq!(b.state(), BreakerState::HalfOpen);
        assert_eq!(b.remaining_cooldown(), None);
    }

    #[test]
    fn asking_for_the_cooldown_does_not_disturb_the_breaker() {
        let b = CircuitBreaker::new(cfg());
        trip(&b);

        // Reading repeatedly must not consume the wait or admit a request.
        for _ in 0..5 {
            assert_cooldown_is(b.remaining_cooldown(), 30);
        }
        assert_eq!(b.state(), BreakerState::Open);
        assert!(
            !b.allow_request(),
            "a read-only report must not have promoted the breaker to HalfOpen"
        );
    }

    #[test]
    fn a_doubled_cooldown_is_reported_at_its_longer_value() {
        let b = CircuitBreaker::new(cfg());
        trip(&b);
        advance(&b, 30);
        assert!(b.allow_request(), "the HalfOpen trial is admitted");

        // The trial failed: the breaker reopens with a doubled cooldown.
        b.record_failure();
        assert_eq!(b.state(), BreakerState::Open);
        assert_cooldown_is(b.remaining_cooldown(), 60);
    }

    #[test]
    fn success_resets_the_failure_count() {
        let b = CircuitBreaker::new(cfg());
        b.record_failure();
        b.record_failure();
        b.record_success();

        // The threshold counts consecutive failures, so two more are not enough.
        b.record_failure();
        b.record_failure();
        assert_eq!(b.state(), BreakerState::Closed);
        b.record_failure();
        assert_eq!(b.state(), BreakerState::Open);
    }

    #[test]
    fn open_rejects_requests_until_the_cooldown_elapses() {
        let b = CircuitBreaker::new(cfg());
        trip(&b);

        assert!(!b.allow_request());
        advance(&b, 29);
        assert!(!b.allow_request());
        assert_eq!(b.state(), BreakerState::Open);
    }

    #[test]
    fn half_open_admits_exactly_one_trial_request() {
        let b = CircuitBreaker::new(cfg());
        trip(&b);
        advance(&b, 30);

        assert!(b.allow_request(), "the trial request must be admitted");
        assert_eq!(b.state(), BreakerState::HalfOpen);
        assert!(!b.allow_request(), "concurrent callers must be rejected");
        assert!(!b.allow_request());
    }

    #[test]
    fn success_in_half_open_closes_and_resets_the_backoff() {
        let b = CircuitBreaker::new(cfg());
        trip(&b);
        advance(&b, 30);
        assert!(b.allow_request());
        b.record_success();

        assert_eq!(b.state(), BreakerState::Closed);
        assert!(b.allow_request());

        // The cooldown is back to the base 30s, not the doubled 60s.
        trip(&b);
        advance(&b, 29);
        assert!(!b.allow_request());
        advance(&b, 1);
        assert!(b.allow_request());
    }

    #[test]
    fn failure_in_half_open_reopens_with_doubled_cooldown() {
        let b = CircuitBreaker::new(cfg());
        trip(&b);

        advance(&b, 30);
        assert!(b.allow_request());
        b.record_failure();
        assert_eq!(b.state(), BreakerState::Open);

        advance(&b, 59);
        assert!(!b.allow_request(), "the 60s cooldown is not over yet");
        advance(&b, 1);
        assert!(b.allow_request());
        b.record_failure();

        advance(&b, 119);
        assert!(!b.allow_request(), "the cooldown doubled to 120s");
        advance(&b, 1);
        assert!(b.allow_request());
    }

    #[test]
    fn cooldown_doubles_then_caps_at_the_maximum() {
        let b = CircuitBreaker::new(cfg());
        trip(&b);

        // 30 -> 60 -> 120 -> 240 -> 480 -> 900 -> 900
        let mut wait = 30;
        for next in [60u64, 120, 240, 480, 900, 900] {
            advance(&b, wait - 1);
            assert!(
                !b.allow_request(),
                "not admitted before the {wait}s cooldown"
            );
            advance(&b, 1);
            assert!(
                b.allow_request(),
                "the trial is admitted at exactly {wait}s"
            );

            b.record_failure();
            assert_eq!(b.state(), BreakerState::Open);
            wait = next;
        }

        // The cap holds: the next cooldown is 900s, never 1800s.
        advance(&b, 899);
        assert!(!b.allow_request());
        advance(&b, 1);
        assert!(b.allow_request(), "the cooldown is capped at 900s");
    }

    #[test]
    fn a_stale_failure_report_does_not_extend_an_already_open_breaker() {
        let b = CircuitBreaker::new(cfg());
        trip(&b);

        // A request admitted BEFORE the trip reports its failure now. The
        // breaker is already Open, so the report must change nothing: without
        // that guard every in-flight straggler would re-enter the cooldown
        // math and keep pushing the trial further out, indefinitely.
        b.record_failure();
        assert_eq!(b.state(), BreakerState::Open);
        assert_cooldown_is(b.remaining_cooldown(), 30);

        advance(&b, 30);
        assert!(
            b.allow_request(),
            "the stale report must not have pushed the trial further out"
        );
    }

    #[test]
    fn a_multiplier_of_one_or_less_disables_the_backoff() {
        let mut config = cfg();
        config.cooldown_multiplier = 1.0;
        let b = CircuitBreaker::new(config);
        trip(&b);

        advance(&b, 30);
        assert!(b.allow_request());
        b.record_failure();
        assert_eq!(b.state(), BreakerState::Open);

        // multiplier 1.0: the failed HalfOpen trial re-opens at the SAME 30s
        // cooldown instead of doubling - the documented "backoff disabled"
        // behaviour, not a silent 30 -> 60.
        assert_cooldown_is(b.remaining_cooldown(), 30);
    }
}
