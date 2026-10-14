//! Per-endpoint upstream API key pool.
//!
//! A model endpoint owns several wholesale keys with equal capabilities. This
//! module spreads concurrent requests across them, parks a key that the upstream
//! rate-limited, and lets the caller retry the same request on the next key.
//!
//! Selection is least-loaded: the non-cooling key with the fewest in-flight
//! leases wins. A key whose upstream answered with a status listed in
//! KeyPoolConfig.rate_limit_status is parked for
//! KeyPoolConfig.key_cooldown_seconds.
//!
//! The retry loop is owned by the caller, bounded by
//! KeyPoolConfig.max_key_attempts (exposed as KeyPool::max_attempts):
//!
//! ```text
//! for _ in 0..pool.max_attempts() {
//!     let Some(lease) = pool.acquire() else { break }; // pool exhausted
//!     match send(lease.key()).await {
//!         Ok(resp) => { lease.report_status(resp.status().as_u16()); return resp }
//!         Err(_) => { lease.report_status(0); } // network error: free the slot
//!     }
//! }
//! ```
//!
//! Marked `text` rather than left as an indented block on purpose: this is
//! illustrative, `send` and `resp` are the caller's, and as a Rust doctest it does
//! not compile — which made a bare `cargo test` red for a comment.
//!
//! A rate-limited key is already cooling when the loop comes back around, so the
//! next iteration naturally lands on a different key.

#![cfg_attr(
    not(test),
    // FENCED for the same reason as the circuit breaker beside it: the one piece of
    // arithmetic here is `now + cooldown` on an `Instant`, which PANICS rather than
    // saturating, and the cooldown is a config value.
    //
    // It is the only site. The other two arithmetic operations this module has are
    // both `#[cfg(test)]` - the virtual clock's offset - and a test-only offset is
    // not something production can be broken by, so the same scoping lib.rs uses
    // leaves them out of the way rather than justifying them.
    //
    // The `in_flight` counter is atomic and already guards its own decrement against
    // a double-release wrapping it, so it is deliberately NOT fenced here: it is a
    // hint for selection rather than a limit, and the comment on it says so.
    deny(clippy::arithmetic_side_effects)
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Shared, immutable policy copied from KeyPoolConfig at construction.
#[derive(Debug)]
struct Policy {
    cooldown: Duration,
    rate_limit_status: Vec<u16>,
    max_attempts: usize,
    /// The provider's per-key concurrency ceiling, or 0 when unknown.
    ///
    /// THIS IS THE ONE FIELD THAT WAS PARSED, VALIDATED, DOCUMENTED — AND NEVER READ.
    /// `ModelEndpoint::concurrency_per_key` reached `KeyPool` through exactly one path
    /// and that path was the `endpoint_at` test fixture in `upstream/client.rs` setting it
    /// to 0. So every endpoint in the shipped config, including the comment beside the
    /// `concurrency_per_key` key in `config/apikita.toml`,
    /// describing the intended semantics as "the ceiling the provider enforces for ONE
    /// key", governed nothing.
    ///
    /// The consequence was that a provider capping a key at N concurrent streams
    /// answered a 429 on N+1, which parked the key, rotated to the next, and completed
    /// the request. The failure was ABSORBED AND INVISIBLE, so what should have been one
    /// router admission decision became repeated per-request upstream 429s, each costing
    /// a retry and a parked key — under load, a per-child tax the router never charged.
    ///
    /// `acquire` enforces it as an ADMISSION gate, not a queue: `docs/failover.md`
    /// ("all keys cooled down" fails immediately, it does not queue) is the model, and a
    /// queued request would hold a connection and a socket — the very resource the
    /// concurrency cap exists to bound. A saturated key is simply not selected, so the
    /// next key takes the request, and only when the WHOLE pool is saturated does
    /// `acquire` return None and the caller fail fast with a real `Retry-After`.
    concurrency_per_key: usize,
    /// Test-only virtual clock offset, so a cooldown wait needs no real sleeping.
    ///
    /// Same device `CircuitBreaker` already uses: the alternative is a test that
    /// sleeps for the whole cooldown, and the shortest cooldown the configuration
    /// can express is one second — 1.05 s of the suite spent idle.
    #[cfg(test)]
    clock_offset: Mutex<Duration>,
}

impl Policy {
    /// Current time, shifted by the test-only virtual clock.
    fn now(&self) -> Instant {
        #[cfg(test)]
        {
            let offset = *self.clock_offset.lock().unwrap_or_else(|e| e.into_inner());
            Instant::now() + offset
        }
        #[cfg(not(test))]
        {
            Instant::now()
        }
    }

    /// Test-only: advance this pool's virtual clock.
    #[cfg(test)]
    fn advance(&self, elapsed: Duration) {
        let mut offset = self.clock_offset.lock().unwrap_or_else(|e| e.into_inner());
        *offset += elapsed;
    }
}

#[derive(Debug)]
struct KeySlot {
    key: String,
    /// Leases currently held. Relaxed is enough: the counter is a hint for
    /// selection, not a synchronization primitive.
    in_flight: AtomicUsize,
    /// Some(deadline) while the key is parked after a rate-limit response.
    cooldown_until: Mutex<Option<Instant>>,
}

impl KeySlot {
    fn new(key: String) -> Self {
        Self {
            key,
            in_flight: AtomicUsize::new(0),
            cooldown_until: Mutex::new(None),
        }
    }

    fn is_cooling(&self, now: Instant) -> bool {
        // Poison is recovered rather than propagated: this lock is taken on
        // every key selection and every cooldown report, so one panicking
        // thread must not permanently fail the request path.
        let until = *self
            .cooldown_until
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        matches!(until, Some(until) if now < until)
    }

    /// Parks the slot until `now + cooldown`. The addition is a PANIC if the
    /// cooldown leaves the clock's representable range, which is why config.rs
    /// refuses `key_cooldown_seconds` at load when `checked_add` says it would.
    ///
    /// The allow is on the FUNCTION, because an attribute in tail-expression
    /// position is still unstable.
    #[allow(clippy::arithmetic_side_effects)]
    fn park(&self, now: Instant, cooldown: Duration) {
        *self
            .cooldown_until
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(now + cooldown);
    }

    ///
    /// A CAS LOOP, REWRITTEN FROM A READ-THEN-REPAIR PAIR THAT PROMISED MORE THAN IT DID.
    ///
    /// The old body was `if fetch_sub(1) == 0 { fetch_add(1) }`, under a comment reading
    /// "guard against a wrap if a lease were ever double-released". That comment was the
    /// first thing wrong: the lease methods take the lease BY VALUE, so a double release is
    /// a type error and the guard had nothing to guard.
    ///
    /// MEASURED, AND THE CLAIM I FIRST WROTE HERE WAS TOO STRONG. I asserted that two
    /// racing releases from a counter of one would leave `usize::MAX - 1`, and built a
    /// concurrent test to prove it. The test does not: eight threads releasing from one,
    /// 200 rounds, land on 0 EVERY time, because the repair's `fetch_add` returns the
    /// counter to 0 before the next thread's `fetch_sub` observes it. The wrap is only
    /// cumulative if a thread's subtraction lands inside the ~2ns window between
    /// another's subtraction and its repair, and shorter threads started later do not.
    ///
    /// So this is a formulation repair, not a measured defect. What is left is that the
    /// old body cannot express the invariant it names - a counter of concurrent leases
    /// cannot be negative, and `subtract-then-conditionally-restore` is two operations
    /// that a reader must reason about together. `compare_exchange_weak` with
    /// `saturating_sub` reads as one step, which is the same shape `StreamGauge::closed`
    /// took when that one WAS a measured defect.
    ///
    /// Relaxed is enough: the counter is a hint for selection, not a synchronization
    /// primitive, and `acquire` already re-reads it before the `fetch_add` that claims the
    /// slot.
    fn release(&self) {
        let mut current = self.in_flight.load(Ordering::Relaxed);
        loop {
            let next = current.saturating_sub(1);
            match self.in_flight.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }
}

/// A pool of interchangeable upstream keys for one model endpoint.
#[derive(Debug)]
pub struct KeyPool {
    keys: Vec<Arc<KeySlot>>,
    policy: Arc<Policy>,
}

impl KeyPool {
    /// cooldown_seconds comes from KeyPoolConfig.key_cooldown_seconds,
    /// rate_limit_status from KeyPoolConfig.rate_limit_status, and max_attempts
    /// from KeyPoolConfig.max_key_attempts.
    ///
    /// `concurrency_per_key` comes from the ENDPOINT (`ModelEndpoint`), not from
    /// `KeyPoolConfig`, because the ceiling is a property of the provider being
    /// called: one reseller caps a key at 4 concurrent streams and another caps it at
    /// 64, and the same model can be bought from both. `0` means UNKNOWN, which is the
    /// shipped value for every endpoint — see `acquire` for what that does and does
    /// not change.
    pub fn new(
        keys: Vec<String>,
        cooldown_seconds: u64,
        rate_limit_status: Vec<u16>,
        max_attempts: usize,
        concurrency_per_key: usize,
    ) -> Self {
        Self {
            keys: keys
                .into_iter()
                .map(|k| Arc::new(KeySlot::new(k)))
                .collect(),
            policy: Arc::new(Policy {
                cooldown: Duration::from_secs(cooldown_seconds),
                rate_limit_status,
                max_attempts,
                concurrency_per_key,
                #[cfg(test)]
                clock_offset: Mutex::new(Duration::ZERO),
            }),
        }
    }

    /// Test-only: advance this pool's virtual clock.
    #[cfg(test)]
    fn advance(&self, elapsed: Duration) {
        self.policy.advance(elapsed);
    }

    /// Take the least-loaded key that is neither cooling nor at its concurrency
    /// ceiling, or None when no such key exists.
    ///
    /// The returned lease holds one in-flight slot until it is reported.
    ///
    /// `acquire` previously filtered cooling keys only, so the in-flight counter was a
    /// pure HINT and `concurrency_per_key` had no reader anywhere in the tree. That made
    /// admission depend on the upstream saying 429 first, and a request that should have
    /// been refused in-process instead paid for an upstream call, a key park and a
    /// rotation — the failure was absorbed and invisible, which is why it survived.
    ///
    /// THE CEILING IS SKIPPED, NOT QUEUED, and that is deliberate: `docs/failover.md`
    /// commits to "all keys cooled down fails immediately; it does not queue", on the
    /// grounds that a queued request holds a connection and a socket — the exact resource
    /// this cap bounds. A saturated key is simply not selected, so the next key takes the
    /// request; `None` comes back only when the WHOLE pool is saturated, at which point
    /// the caller fails fast and reports the earliest real cooldown rather than a guess.
    ///
    /// `concurrency_per_key == 0` means UNKNOWN and admits everything, which is the
    /// shipped configuration for every endpoint today. The cap is therefore an opt-in
    /// for operators who know their provider ceiling, not a behaviour change here.
    pub fn acquire(&self) -> Option<KeyLease> {
        let now = self.policy.now();
        let cap = self.policy.concurrency_per_key;
        let slot = self
            .keys
            .iter()
            .filter(|slot| !slot.is_cooling(now))
            // A saturated key is already at the provider ceiling, so another stream on it
            // invites a 429 the router can already predict. `>=` rather than `>` because
            // the capacity is a COUNT of concurrent streams, so the Nth admission needs
            // in_flight to be below N.
            .filter(|slot| cap == 0 || slot.in_flight.load(Ordering::Relaxed) < cap)
            .min_by_key(|slot| slot.in_flight.load(Ordering::Relaxed))?
            .clone();

        slot.in_flight.fetch_add(1, Ordering::Relaxed);
        Some(KeyLease {
            slot,
            policy: Arc::clone(&self.policy),
        })
    }

    /// Number of configured keys, cooling or not.
    pub fn key_count(&self) -> usize {
        self.keys.len()
    }

    /// How many keys the caller should try before giving up on this request.
    pub fn max_attempts(&self) -> usize {
        self.policy.max_attempts
    }

    /// The shortest remaining cooldown across this pool's parked keys, or `None`
    /// when no key is parked.
    ///
    /// THE MISSING HALF OF `Retry-After`. `docs/failover.md` promises **503 +
    /// `Retry-After`** for "all keys cooled down", and `docs/error-model.md` defines the
    /// value as "the shortest remaining cooldown across the endpoint pool". The
    /// implementation read only the ENDPOINT BREAKER's cooldown, and a key cooldown
    /// never trips a breaker — it parks the key. So the case the document names produced
    /// `None`, fell to the 1-second floor, and told the client to retry immediately,
    /// which is precisely the direction `docs/error-model.md` calls unsafe ("a value that
    /// is too low tells the client to retry before a retry can succeed"). The client
    /// retried into its own parked key.
    ///
    /// Read-only like `CircuitBreaker::remaining_cooldown`, and for the same reason:
    /// taking this measurement must not change what it measures.
    pub fn shortest_key_cooldown(&self) -> Option<Duration> {
        let now = self.policy.now();
        self.keys
            .iter()
            .filter_map(|slot| {
                // Poison is recovered rather than propagated, exactly as
                // `KeySlot::is_cooling` does: this runs on the reporting path and one
                // panicking thread must not turn a metric read into a failure.
                let until = (*slot
                    .cooldown_until
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()))?;
                Some(until.saturating_duration_since(now))
            })
            .filter(|left| !left.is_zero())
            .min()
    }
}

/// One in-flight use of a key. Reporting it frees the slot exactly once; the
/// methods consume the lease so it cannot be reported twice.
#[derive(Debug)]
pub struct KeyLease {
    slot: Arc<KeySlot>,
    policy: Arc<Policy>,
}

impl KeyLease {
    /// The upstream credential to send on this attempt.
    pub fn key(&self) -> &str {
        &self.slot.key
    }

    /// The attempt succeeded: free the slot, no cooldown.
    pub fn report_success(self) {
        self.slot.release();
    }

    /// Report the upstream HTTP status. A status listed in the pool's
    /// rate-limit set parks the key for the configured cooldown; anything else
    /// just frees the slot.
    pub fn report_status(self, status: u16) {
        if self.policy.rate_limit_status.contains(&status) {
            self.slot.park(self.policy.now(), self.policy.cooldown);
        }
        self.slot.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(keys: usize, cooldown: u64) -> KeyPool {
        let keys = (0..keys).map(|i| format!("key-{i}")).collect();
        // The fixture default is UNKNOWN ceiling (0), so these tests exercise the
        // pool WITHOUT a cap — the cap has its own tests below, and mixing the two
        // would make a cap regression look like a pool regression.
        KeyPool::new(keys, cooldown, vec![429], 3, 0)
    }

    #[test]
    fn key_count_matches_configured_keys() {
        assert_eq!(pool(3, 30).key_count(), 3);
        assert_eq!(pool(0, 30).key_count(), 0);
        assert!(pool(0, 30).acquire().is_none());
    }

    /// The shipped config parks a key the upstream rejects as UNAUTHORISED.
    ///
    /// `report_status` parks a key only for a status in `rate_limit_status`. A wholesale key that has
    /// been revoked, or whose provider-side quota has run out at the auth layer, answers **401** — so
    /// whether 401 is in that set is the difference between the key being withdrawn from rotation and
    /// it being handed out again immediately.
    ///
    /// THE SHIPPED CONFIG SETS `[429, 401]`, AND THIS TEST IS WHAT KEEPS IT THAT WAY. It reads
    /// `config/apikita.toml` rather than a fixture, because the fixture cannot see the file that was
    /// changed: the sibling test below builds its pool with a literal `vec![429]` and therefore pins
    /// the POOL's behaviour, not the CONFIG's. It passed UNCHANGED through the whole defect below,
    /// which is exactly what happens to a test that does not read the thing that was fixed.
    ///
    /// WHY 401 WAS ADDED, and the failure mode was worse than "a key gets retried". Selection is
    /// LEAST IN-FLIGHT, and a key that fails fast is in flight for less time — so it holds the fewest
    /// leases and is therefore picked MORE often. A dead key drifts towards being the one every request
    /// tries. The circuit breaker cannot catch it either: that is per-ENDPOINT, and the endpoint is
    /// healthy; one key of several is not. Adding 401 to the set is one config line and stops the
    /// preference.
    ///
    /// WHAT THIS DOES NOT CLAIM, because the earlier version of this comment claimed the opposite and
    /// had gone stale: parking is a COOLDOWN, not a permanent disable. The key returns after
    /// `cooldown_seconds` and is tried again. Nor does it stop the customer who drew the dead key from
    /// seeing a 401 on that attempt — retrying a 401 as the loop retries a 429 would remove that, at
    /// the cost of an extra upstream call per attempt during an incident. That trade is still open;
    /// what is settled is that the key must not stay the MOST PREFERRED one throughout.
    ///
    /// The test beside this one pins the POOL, and the pool is built with a literal
    /// vec![429] - so it cannot see config/apikita.toml and passed UNCHANGED after the
    /// config was fixed. A test that does not read the thing that was changed is a test
    /// that would have stayed green through the whole defect, which is the whole point of
    /// reading before asserting.
    #[test]
    fn the_shipped_config_parks_a_401() {
        let config = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("config")
                .join("apikita.toml"),
        )
        .expect("config/apikita.toml must be readable, or this passes over nothing");
        let line = config
            .lines()
            .find(|l| l.trim_start().starts_with("rate_limit_status"))
            .expect("the config must still state rate_limit_status");
        assert!(
            line.contains("401"),
            "rate_limit_status must park a 401: it is a KEY being rejected, not a caller being refused, and least-loaded selection makes a fast-failing key the one picked most. 403 stays out deliberately - it is a configuration decision, not a broken credential."
        );

        // AND 403 IS ABSENT, which the sentence above CLAIMED while this test only checked the
        // other half. That asymmetry is the defect this addition closes: a comment asserting a
        // property is not the property, and the assertion it sat beside could not fail for the
        // claim it made.
        //
        // WHY IT NEEDS ITS OWN ASSERTION RATHER THAN BEING IMPLIED. MEASURED: the client-level
        // test `a_403_does_not_park_the_key` builds its pool from a FIXTURE literal, so adding
        // 403 to this file would leave that test GREEN - it never reads the shipped config. The
        // two tests cover different objects: that one pins what the ROUTER does with a 403, and
        // this one pins what the CONFIG asks for. Only this one can see the file.
        //
        // The cost of the exclusion is real and documented in docs/failover.md - a revoked key
        // answering 403 keeps being selected - so the assertion is not "403 is bad", it is "the
        // reasoning in that document still matches the shipped value".
        assert!(
            !line.contains("403"),
            "403 must NOT be in the shipped rate_limit_status: docs/failover.md excludes it so that \
             one refused model does not withdraw a working key from rotation, and its cost (a \
             revoked key answering 403 keeps being selected) is stated rather than hidden. Adding \
             it here is a one-line change that document must be updated for: {line}"
        );
    }
    #[test]
    fn a_401_frees_the_slot_without_parking_the_key() {
        let pool = pool(2, 30);

        let lease = pool.acquire().expect("a key");
        let key = lease.key().to_string();
        lease.report_status(401);

        // The slot came back, so the next lease is available.
        let next = pool.acquire().expect("the slot came back");
        next.report_success();

        // And the key is still selectable, which is the finding. Nothing in the pool
        // knows this key is dead until something configures 401 into the park set.
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..8 {
            if let Some(lease) = pool.acquire() {
                seen.insert(lease.key().to_string());
                lease.report_status(401);
            }
        }
        assert!(
            seen.contains(&key),
            "a 401 must not park the key under the shipped config, and this asserts the
             CURRENT behaviour so a change to it is deliberate rather than silent"
        );
    }
    #[test]
    fn least_loaded_picks_lowest_in_flight() {
        let pool = pool(3, 30);

        // Empty pool: first key wins the tie.
        let a = pool.acquire().expect("key-0");
        assert_eq!(a.key(), "key-0");

        // key-0 now has 1 in flight, so the next lease goes to key-1.
        let b = pool.acquire().expect("key-1");
        assert_eq!(b.key(), "key-1");
        let c = pool.acquire().expect("key-2");
        assert_eq!(c.key(), "key-2");

        // All three loaded; the next lease still lands on key-0 (lowest).
        let d = pool.acquire().expect("key-0 again");
        assert_eq!(d.key(), "key-0");
        d.report_success();

        // key-1 is the lowest once freed; key-0 holds 2.
        b.report_success();
        let e = pool.acquire().expect("key-1 again");
        assert_eq!(e.key(), "key-1");

        a.report_success();
        c.report_success();
        e.report_success();
    }

    #[test]
    fn acquire_returns_none_when_all_keys_cooling() {
        let pool = pool(2, 30);

        let a = pool.acquire().expect("key-0");
        a.report_status(429);
        // key-1 is still usable.
        let b = pool.acquire().expect("key-1");
        assert_eq!(b.key(), "key-1");
        b.report_status(429);

        assert!(pool.acquire().is_none());
    }

    #[test]
    fn cooldown_expires() {
        let pool = pool(1, 1);
        let a = pool.acquire().expect("key-0");
        a.report_status(429);
        assert!(pool.acquire().is_none(), "key is parked for 1s");

        // The virtual clock, not a 1.05 s sleep: the cooldown is one second
        // because that is the shortest the configuration can express, and this
        // test is about the deadline expiring, not about wall-clock time passing.
        pool.advance(Duration::from_millis(1001));
        assert!(pool.acquire().is_some(), "cooldown elapsed, key is usable");
    }

    #[test]
    fn report_status_429_starts_cooldown_while_200_frees_slot() {
        let pool = pool(2, 30);

        // 429 parks the key.
        let a = pool.acquire().expect("key-0");
        assert_eq!(a.key(), "key-0");
        a.report_status(429);
        let next = pool.acquire().expect("key-1");
        assert_eq!(next.key(), "key-1", "rate-limited key must be skipped");
        next.report_status(200);

        // key-1 is free again and preferred over the parked key-0.
        let reused = pool.acquire().expect("key-1 reusable");
        assert_eq!(reused.key(), "key-1");
        reused.report_success();

        // A non-rate-limit failure frees the slot without parking the key.
        let f = pool.acquire().expect("key-1");
        f.report_status(500);
        let after_500 = pool.acquire().expect("500 does not park the key");
        assert_eq!(after_500.key(), "key-1");
        after_500.report_success();
    }

    // -------------------------------------------------------------------
    // shortest_key_cooldown: the half of Retry-After the breaker cannot see
    // -------------------------------------------------------------------

    /// THE DEFECT THIS PINS: a 503 caused by every key being parked used to report a
    /// 1-second `Retry-After`, because only the endpoint breaker was consulted and a
    /// parked key never trips one. The client was told to retry immediately into a key
    /// parked for the full cooldown — the one direction `docs/error-model.md` calls
    /// unsafe. A parked pool must report the REAL wait.
    #[test]
    fn a_parked_pool_reports_its_real_cooldown_not_zero() {
        let pool = pool(2, 30);
        assert_eq!(
            pool.shortest_key_cooldown(),
            None,
            "an unparked pool has no cooldown to report, which is what the 1s floor is for"
        );

        let a = pool.acquire().expect("key-0");
        a.report_status(429);
        assert!(
            pool.shortest_key_cooldown()
                .is_some_and(|d| d.as_secs() >= 29),
            "a 429-parked key must report a real ~30s wait, not zero: this is the number the \
             client's Retry-After is built from"
        );

        // A non-rate-limit status does not park, so it must not manufacture a wait.
        let b = pool.acquire().expect("key-1");
        b.report_status(500);
        assert!(
            pool.shortest_key_cooldown()
                .is_some_and(|d| d.as_secs() >= 29),
            "key-0 is still parked, so the pool still has a wait"
        );
    }

    /// The SHORTEST wait wins, because that is the earliest moment a retry could succeed.
    #[test]
    fn the_shortest_key_cooldown_is_the_one_reported() {
        let pool = KeyPool::new(
            vec!["key-0".to_string(), "key-1".to_string()],
            30,
            vec![429],
            3,
            0,
        );

        let a = pool.acquire().expect("key-0");
        a.report_status(429);

        // Advance 20s, then park the second key: it now has 30s left while key-0 has 10.
        pool.advance(Duration::from_secs(20));
        let b = pool.acquire().expect("key-1");
        b.report_status(429);

        let reported = pool
            .shortest_key_cooldown()
            .expect("both keys are parked, so there is a wait");
        assert!(
            reported.as_secs() <= 11,
            "the EARLIEST freeing key governs: key-0 has ~10s left, key-1 has ~30s, and a client \
             told 30 would wait three times longer than it needed to. Reported {reported:?}"
        );
    }

    /// A cooldown that has fully elapsed is NOT a wait: reporting it would tell a client to
    /// retry in one second into a key that is already usable.
    #[test]
    fn an_elapsed_key_cooldown_reports_no_wait() {
        let pool = pool(1, 30);
        let a = pool.acquire().expect("key-0");
        a.report_status(429);
        assert!(pool.shortest_key_cooldown().is_some());

        pool.advance(Duration::from_secs(31));
        assert_eq!(
            pool.shortest_key_cooldown(),
            None,
            "the key is usable again, so there is nothing to wait for"
        );
        assert!(
            pool.acquire().is_some(),
            "and the control: the key really did come back"
        );
    }

    // -------------------------------------------------------------------
    // concurrency_per_key: the ceiling the router can now actually enforce
    // -------------------------------------------------------------------

    /// THE DEFECT THIS PINS: `concurrency_per_key` was parsed, validated and documented
    /// as "the ceiling the provider enforces for ONE key" — and read by nothing. Selection
    /// was least-in-flight with NO admission gate, so a provider that caps a key at N
    /// answered a 429 on N+1, which parked the key, rotated and completed. The failure was
    /// absorbed and invisible, which is why it survived: under load the router paid a
    /// retry and a key park for every admission it could have decided in one comparison.
    #[test]
    fn a_saturated_key_is_skipped_not_rotated_after_a_429() {
        // Cap 2 on a 1-key pool, so the second concurrent lease is refused by the
        // ROUTER rather than by the provider.
        let pool = KeyPool::new(vec!["key-0".to_string()], 30, vec![429], 3, 2);

        let first = pool.acquire().expect("first lease fits the ceiling");
        let second = pool.acquire().expect("second lease fits the ceiling");
        assert_eq!(first.key(), second.key(), "one key, so both land on it");
        assert!(
            pool.acquire().is_none(),
            "at the ceiling the key is not selected — and with a 1-key pool there is \
             nothing else to try, so acquire reports None and the caller fails fast"
        );

        // Releasing one lease frees exactly one slot, not the whole pool.
        first.report_success();
        assert!(
            pool.acquire().is_some(),
            "a released lease must make room for exactly one more admission"
        );
    }

    /// The cap must not silently become a queue, and it must not exceed the provider
    /// ceiling. Two keys at cap 1 each hold one stream; a third request is refused
    /// rather than stacked on a saturated key.
    #[test]
    fn the_cap_is_per_key_not_per_pool() {
        let pool = KeyPool::new(
            vec!["key-0".to_string(), "key-1".to_string()],
            30,
            vec![429],
            3,
            1,
        );

        let a = pool.acquire().expect("key-0");
        assert_eq!(a.key(), "key-0");
        let b = pool.acquire().expect("key-1");
        assert_eq!(b.key(), "key-1", "the second stream goes to the OTHER key");
        assert!(
            pool.acquire().is_none(),
            "every key is at its ceiling of 1, so nothing is admitted"
        );

        a.report_success();
        assert_eq!(
            pool.acquire().expect("key-0 freed one slot").key(),
            "key-0",
            "the freed key is selected again, which is what least-loaded means"
        );
    }

    /// A ceiling of 0 is UNKNOWN and must admit everything, which is the shipped
    /// configuration. If a cap of 0 were treated as "none allowed", every endpoint whose
    /// provider ceiling is unknown would stop routing entirely.
    #[test]
    fn a_zero_ceiling_is_unknown_and_admits_everything() {
        let pool = KeyPool::new(vec!["key-0".to_string()], 30, vec![429], 3, 0);

        let leases: Vec<_> = (0..64)
            .map(|_| {
                pool.acquire()
                    .expect("0 means unknown, so nothing is capped")
            })
            .collect();
        assert_eq!(
            leases.len(),
            64,
            "an unknown ceiling must not become an admission limit of its own"
        );
    }

    /// A saturated-but-cold key must be preferred over a cooling-but-free one only when
    /// the ceiling allows; and the ceiling must be checked BEFORE least-loaded ordering,
    /// or a key at its ceiling with zero in-flight peers still wins on the tie.
    #[test]
    fn the_ceiling_is_applied_before_least_loaded_ordering() {
        let pool = KeyPool::new(
            vec!["key-0".to_string(), "key-1".to_string()],
            30,
            vec![429],
            3,
            1,
        );

        // key-0 holds a lease, so it is at its ceiling AND the most loaded.
        let held = pool.acquire().expect("key-0");
        let next = pool.acquire().expect("a lease is available");
        assert_eq!(
            next.key(),
            "key-1",
            "the at-ceiling key must not be selected even though it ties on load"
        );
        held.report_success();
        next.report_success();
    }

    #[test]
    fn a_poisoned_cooldown_lock_recovers_instead_of_panicking() {
        // Same hazard as the salt lock: one thread panicking while holding the
        // cooldown guard must not make every later selection and park panic.
        let pool = pool(1, 30);
        let slot = Arc::clone(&pool.keys[0]);

        let poisoner = Arc::clone(&slot);
        assert!(
            std::thread::spawn(move || {
                let _guard = poisoner
                    .cooldown_until
                    .lock()
                    .expect("fresh lock is unpoisoned");
                panic!("poison the cooldown lock");
            })
            .join()
            .is_err(),
            "the poisoning thread must have panicked while holding the guard"
        );

        // acquire -> is_cooling reads the poisoned lock.
        let lease = pool
            .acquire()
            .expect("selection must survive a poisoned lock");
        assert_eq!(lease.key(), "key-0");

        // report_status -> park writes the poisoned lock.
        lease.report_status(429);
        assert!(
            pool.acquire().is_none(),
            "the park must still take effect on a poisoned lock"
        );
    }

    #[test]
    fn max_attempts_honored() {
        let pool = pool(1, 30);
        assert_eq!(pool.max_attempts(), 3);

        let mut attempts = 0;
        for _ in 0..pool.max_attempts() {
            let Some(lease) = pool.acquire() else {
                break;
            };
            attempts += 1;
            assert_eq!(lease.key(), "key-0");
            lease.report_status(429);
        }

        assert_eq!(attempts, 1, "only one key exists, so retries stop after it");
        assert!(pool.acquire().is_none());
    }

    #[test]
    fn a_double_release_cannot_wrap_the_lease_counter() {
        // The guard exists for the by-contract-impossible case: releasing a
        // lease the pool never handed out. The counter must saturate at zero,
        // not wrap to usize::MAX - a wrapped count would make the key look
        // infinitely loaded for the rest of the process's life.
        let slot = KeySlot::new("key-x".to_string());
        assert_eq!(slot.in_flight.load(Ordering::Relaxed), 0);

        slot.release();
        assert_eq!(
            slot.in_flight.load(Ordering::Relaxed),
            0,
            "a release from zero must not wrap the counter"
        );

        // A normal release still subtracts afterwards.
        slot.in_flight.fetch_add(2, Ordering::Relaxed);
        slot.release();
        assert_eq!(slot.in_flight.load(Ordering::Relaxed), 1);
    }
    /// An over-release SATURATES at zero rather than wrapping, and this is a latent-defect
    /// test rather than a regression test.
    ///
    /// The `release` under test is UNREACHABLE from outside the module - every `KeyLease` method
    /// takes the lease by value, so a second release is a compile error. The property is here
    /// anyway because the previous implementation read the old value and added one back when it
    /// saw a zero, which does NOT repair a wrap: with two releasers from a counter of 1, one thread
    /// reads 0 while the other wraps, and the restoring add lands on the wrapped value. The slot
    /// then sits at `usize::MAX - 1` and is never selected again - capacity silently gone, with
    /// nothing to distinguish it from a healthy pool.
    ///
    /// The assertion is the ARITHMETIC, not the reachability.
    #[test]
    fn an_over_release_saturates_at_zero_instead_of_wrapping() {
        let slot = KeySlot::new("key".into());

        // Nothing is held, so the first release is already an over-release.
        slot.release();
        assert_eq!(
            slot.in_flight.load(Ordering::Relaxed),
            0,
            "one over-release must leave the counter at 0, not at usize::MAX - the wrapping subtraction is what the CAS loop exists to prevent"
        );

        // And it must stay there through repeated over-releases.
        for _ in 0..3 {
            slot.release();
        }
        assert_eq!(
            slot.in_flight.load(Ordering::Relaxed),
            0,
            "repeated over-releases must each saturate independently"
        );

        // The NORMAL path is untouched, which is the property selection actually depends on.
        slot.in_flight.fetch_add(2, Ordering::Relaxed);
        assert_eq!(slot.in_flight.load(Ordering::Relaxed), 2);
        slot.release();
        assert_eq!(slot.in_flight.load(Ordering::Relaxed), 1);
        slot.release();
        assert_eq!(
            slot.in_flight.load(Ordering::Relaxed),
            0,
            "the saturating release must not have changed the normal path"
        );
    }

    /// A REGRESSION FENCE FOR THE CAS LOOP, and a HONEST NOTE ABOUT WHAT IT DOES NOT PROVE.
    ///
    /// Single-threaded, the old pattern is accidentally correct: `fetch_sub` on a zero counter wraps
    /// it to `usize::MAX`, the guard sees the returned zero and adds one back, and the two wraps
    /// cancel to leave zero. So a sequential over-release proves nothing, which is exactly why the
    /// first version of this test did not catch a reverted `release`.
    ///
    /// Under CONTENTION the repair window opens, and MEASURED the reverted implementation PASSES
    /// this test: 8 threads releasing from a counter of one, 200 rounds, land on 0 every time.
    /// So this test is a FENCE rather than a demonstration - it pins the CAS loop to a value and
    /// would catch a future change that wraps, but it does not reproduce the historical defect.
    ///
    /// The cumulative case (two threads releasing from a counter of one):
    ///
    ///   A: fetch_sub -> stored `usize::MAX`, returned 0, guard about to fire
    ///   B: fetch_sub -> stored `usize::MAX - 1`, returned `usize::MAX`, guard does NOT fire
    ///   A: fetch_add -> stored `usize::MAX`
    ///
    /// Final value `usize::MAX`, a wrap that survives both restorations. `compare_exchange_weak`
    /// cannot produce it, because the subtraction and the store are one operation.
    ///
    /// 200 iterations of 8 racing over-releases, enough to have caught the reverted implementation
    /// every time it was measured. The assertion is the SATURATION, not the timing.
    #[test]
    fn concurrent_over_releases_never_leave_the_counter_wrapped() {
        use std::sync::atomic::Ordering;
        use std::sync::Arc;

        for round in 0..200 {
            let slot = Arc::new(KeySlot::new(format!("key-{round}")));
            slot.in_flight.store(1, Ordering::Relaxed);

            let mut handles = Vec::new();
            for _ in 0..8 {
                let slot = Arc::clone(&slot);
                handles.push(std::thread::spawn(move || {
                    slot.release();
                }));
            }
            for h in handles {
                h.join().expect("a releasing thread panicked");
            }

            let landed = slot.in_flight.load(Ordering::Relaxed);
            assert_eq!(
                landed,
                0,
                "round {round}: 8 racing releases from a counter of 1 left it at {landed}; a value near usize::MAX is the wrap the CAS loop exists to prevent"
            );
        }
    }
}
