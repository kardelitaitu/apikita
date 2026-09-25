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
//! ```no_run
//! # async fn example(pool: &apikita_server::upstream::key_pool::KeyPool) {
//! for _ in 0..pool.max_attempts() {
//!     let Some(lease) = pool.acquire() else { break }; // pool exhausted
//!     // match send(lease.key()).await { ... }
//! }
//! # }
//! ```
//!
//! A rate-limited key is already cooling when the loop comes back around, so the
//! next iteration naturally lands on a different key.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Shared, immutable policy copied from KeyPoolConfig at construction.
#[derive(Debug)]
struct Policy {
    cooldown: Duration,
    rate_limit_status: Vec<u16>,
    max_attempts: usize,
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
        matches!(*self.cooldown_until.lock().unwrap(), Some(until) if now < until)
    }

    fn park(&self, now: Instant, cooldown: Duration) {
        *self.cooldown_until.lock().unwrap() = Some(now + cooldown);
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
            keys: keys.into_iter().map(|k| Arc::new(KeySlot::new(k))).collect(),
            policy: Arc::new(Policy {
                cooldown: Duration::from_secs(cooldown_seconds),
                rate_limit_status,
                max_attempts,
            }),
        }
    }

    /// Take the least-loaded key that is not cooling, or None when every key is
    /// parked. The returned lease holds one in-flight slot until it is reported.
    pub fn acquire(&self) -> Option<KeyLease> {
        let now = Instant::now();
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
            self.slot.park(Instant::now(), self.policy.cooldown);
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

        std::thread::sleep(Duration::from_millis(1050));
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
}
