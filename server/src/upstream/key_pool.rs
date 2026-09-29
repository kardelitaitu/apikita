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

    fn release(&self) {
        // Guard against a wrap if a lease were ever double-released.
        if self.in_flight.fetch_sub(1, Ordering::Relaxed) == 0 {
            self.in_flight.fetch_add(1, Ordering::Relaxed);
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
    pub fn new(
        keys: Vec<String>,
        cooldown_seconds: u64,
        rate_limit_status: Vec<u16>,
        max_attempts: usize,
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

    /// Take the least-loaded key that is not cooling, or None when every key is
    /// parked. The returned lease holds one in-flight slot until it is reported.
    pub fn acquire(&self) -> Option<KeyLease> {
        let now = self.policy.now();
        let slot = self
            .keys
            .iter()
            .filter(|slot| !slot.is_cooling(now))
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
        KeyPool::new(keys, cooldown, vec![429], 3)
    }

    #[test]
    fn key_count_matches_configured_keys() {
        assert_eq!(pool(3, 30).key_count(), 3);
        assert_eq!(pool(0, 30).key_count(), 0);
        assert!(pool(0, 30).acquire().is_none());
    }

    #[test]
    /// A key the upstream rejects as UNAUTHORISED is never parked, and that is
    /// currently a hole rather than a decision.
    ///
    /// `report_status` parks only a status in `rate_limit_status`, which the shipped
    /// config sets to `[429]`. A wholesale key that has been revoked, or whose
    /// provider-side quota has run out at the auth layer, answers 401 — which is not in
    /// the set, so the slot is freed and the key stays selectable.
    ///
    /// WORSE, IT IS SELF-REINFORCING. Selection is least in-flight, and a key that
    /// fails fast is in flight for less time, so it has the fewest leases and is
    /// therefore picked MORE often. The dead key drifts towards being the one every
    /// request tries, and the caller's loop returns any HTTP response to the customer
    /// rather than retrying, so a share of requests answers 401 from our own wholesale
    /// layer. The circuit breaker does not catch it either: it is per-ENDPOINT, and the
    /// endpoint is healthy - one key of several is not.
    ///
    /// This test PINS the behaviour rather than changing it, because both fixes are
    /// product decisions rather than bugs. Adding 401 to `rate_limit_status` is one
    /// config line and stops the key being preferred, but the customer who drew it
    /// still sees a 401. Retrying on 401 as the loop already retries on 429 removes that,
    /// and costs an extra upstream call per attempt during an incident. Nobody has
    /// chosen between them, so the honest state is recorded and visible.
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
}
