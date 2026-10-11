//! Tracking for the detached work a graceful shutdown must not abandon.
//!
//! WHY THIS EXISTS. `axum::serve(...).with_graceful_shutdown(sig)` drains **connections**. It does
//! not await detached `tokio::spawn` tasks, because those are owned by the runtime rather than by the
//! connection that started them. MEASURED: this server has four production spawn sites on the money
//! path -
//!
//!   * `release_quietly` (`routes/proxy.rs`) - the compensating credit for a failed settlement
//!   * `settle_after_stream` (`routes/proxy.rs`) - the billing task fed by `MeteredStream`'s `Drop`
//!   * `record_key_ip` (`routes/proxy.rs`) - the source-address abuse signal
//!   * the mail send in `routes/auth.rs` - not money, but it is the one a user waits on
//!
//! - so a shutdown that drained only connections could tell a client its request completed while the
//! release or the settlement had not been written. That is the "reply sent, hold still open" shape
//! `db::unpaired_hold_rows` exists to detect, except it would be reachable without a process crash.
//!
//! WHAT IT DOES NOT CLAIM. `routes/proxy.rs` documents the process-kill window as **deliberate and
//! detected**: no ordering of statements removes it, the `stranded_hold` alert fires on it, and
//! `hold-sweep --release` is the operator action. This module does not close that window - a SIGKILL
//! still loses the task. It narrows it for the ordered shutdown the orchestrator actually sends, and
//! the sweep remains the backstop for the rest.
//!
//! THE COUNTING IS DELIBERATELY ASYMMETRIC. `begin` increments before the spawn and `Guard::drop`
//! decrements at the end, so the counter is only ever wrong in the direction of "still busy", which
//! makes a shutdown wait rather than proceed. A counter that could under-count would let the drain
//! finish early, which is the failure it exists to prevent.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;

/// Counts outstanding detached tasks and lets a shutdown wait for them to reach zero.
#[derive(Debug, Default)]
pub struct TaskTracker {
    outstanding: AtomicUsize,
    idle: Notify,
}

impl TaskTracker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register a task about to be spawned. The returned guard must be held for the task's lifetime.
    ///
    /// Take this BEFORE `tokio::spawn`, not inside it: between the spawn and the task's first poll is
    /// a window in which the task exists and this count does not, and a shutdown landing there would
    /// miss it.
    pub fn begin(self: &Arc<Self>) -> Guard {
        self.outstanding.fetch_add(1, Ordering::SeqCst);
        Guard {
            tracker: Arc::clone(self),
        }
    }

    /// How many detached tasks are outstanding.
    pub fn outstanding(&self) -> usize {
        self.outstanding.load(Ordering::SeqCst)
    }

    /// Wait until every registered task has finished, or `timeout` elapses.
    ///
    /// Returns the number STILL outstanding - `0` means every task completed. A non-zero return is
    /// not an error the caller can fix; it is the fact the caller must log, because money-path work
    /// is about to be abandoned and `hold-sweep --release` is what will find it.
    pub async fn drain(&self, timeout: Duration) -> usize {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let n = self.outstanding();
            if n == 0 {
                return 0;
            }
            // Re-check AFTER registering interest, or a task finishing between the load and the
            // notified() await leaves this waiting for a wakeup that already happened.
            let notified = self.idle.notified();
            if self.outstanding() == 0 {
                return 0;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.outstanding();
            }
        }
    }
}

/// Decrements the tracker when the task it guards finishes, including on panic.
///
/// A panic in a detached task would otherwise leave the count permanently raised and make every later
/// shutdown wait for the full timeout, which is why this is a Drop guard rather than a call at the
/// end of the task body.
#[derive(Debug)]
pub struct Guard {
    tracker: Arc<TaskTracker>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if self.tracker.outstanding.fetch_sub(1, Ordering::SeqCst) == 1 {
            // Last one out: wake every waiter.
            self.tracker.idle.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_empty_tracker_drains_immediately() {
        let t = TaskTracker::new();
        assert_eq!(t.drain(Duration::from_secs(5)).await, 0);
    }

    #[tokio::test]
    async fn drain_waits_for_a_task_and_reports_zero() {
        let t = TaskTracker::new();
        let guard = t.begin();
        assert_eq!(t.outstanding(), 1);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            drop(guard);
        });
        assert_eq!(t.drain(Duration::from_secs(5)).await, 0);
        assert_eq!(t.outstanding(), 0);
    }

    #[tokio::test]
    async fn drain_reports_what_is_still_outstanding_when_it_times_out() {
        let t = TaskTracker::new();
        let _held = t.begin();
        // Never dropped: the task is stuck, which is exactly the case the count must report.
        assert_eq!(t.drain(Duration::from_millis(60)).await, 1);
    }

    #[tokio::test]
    async fn a_panicking_task_still_decrements() {
        let t = TaskTracker::new();
        let guard = t.begin();
        let handle = tokio::spawn(async move {
            let _g = guard;
            panic!("a detached task panicked");
        });
        // The JoinHandle surfaces the panic; the tracker must be clean regardless.
        assert!(handle.await.is_err(), "the task must have panicked");
        assert_eq!(t.outstanding(), 0, "a panic must not strand the count");
        assert_eq!(t.drain(Duration::from_secs(5)).await, 0);
    }

    #[tokio::test]
    async fn drain_sees_a_task_that_finishes_before_it_registers_interest() {
        let t = TaskTracker::new();
        let guard = t.begin();
        drop(guard);
        // The counter is already zero; drain must not wait for a wakeup.
        assert_eq!(t.drain(Duration::from_secs(5)).await, 0);
    }
}
