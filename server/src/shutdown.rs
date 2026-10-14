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

    /// Module names `lib.rs` declares `#[cfg(test)]`, so a whole file that is test-only is not
    /// scanned for production spawns.
    ///
    /// READ FROM `lib.rs` RATHER THAN HARDCODED, and this was a real misclassification: the first
    /// run of this guard reported `money_adversarial.rs:118` and `:207` as UNTRACKED PRODUCTION
    /// spawns. They are neither - the file is `#[cfg(test)] pub mod money_adversarial` in
    /// `lib.rs`, so nothing in it is compiled into the server at all. A hardcoded skip list would
    /// have to be edited in step with that attribute; reading the attribute means the two cannot
    /// disagree.
    fn test_only_modules() -> Vec<String> {
        let lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs");
        let text = std::fs::read_to_string(&lib)
            .expect("src/lib.rs must be readable, or this guard checks nothing");
        let mut out = Vec::new();
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let Some(name) = line.trim().strip_prefix("pub mod ") else {
                continue;
            };
            let name = name.trim_end_matches(';').trim();
            let gated = i > 0 && lines[i - 1].trim() == "#[cfg(test)]";
            if gated {
                out.push(format!("{name}.rs"));
            }
        }
        out
    }

    /// Every `.rs` file under `dir`, recursively.
    fn collect_rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// Is `lines[i]` inside production code rather than the file's `mod tests`?
    ///
    /// BRACE DEPTH, not "the last `#[cfg(test)]` above it": a `#[cfg(test)]` also marks individual
    /// test-only FUNCTIONS in this crate, and treating one of those as the module boundary would
    /// report every production spawn after it as a test and make this guard vacuous for whole files.
    fn in_production_code(lines: &[&str], i: usize) -> bool {
        let mut depth: i32 = 0;
        let mut tests_at: Option<i32> = None;
        for (n, line) in lines.iter().enumerate().take(i + 1) {
            if tests_at.is_none() && line.trim_start().starts_with("mod tests") {
                tests_at = Some(depth);
            }
            depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
            if n == i {
                break;
            }
        }
        match tests_at {
            Some(open) => i == 0 || depth <= open,
            None => true,
        }
    }

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
    /// EVERY PRODUCTION `tokio::spawn` REGISTERS WITH THE TRACKER, AND THE COUNT IS THE ONE
    /// THE MODULE DOC CLAIMS.
    ///
    /// THE DEFECT THIS PREVENTS: the module doc says "this server has four production spawn sites
    /// on the money path" and names them. NOTHING CHECKED THAT. A fifth detached task that
    /// forgot `tasks.begin()` would be invisible - the drain would return while it was still
    /// running, which is the "reply sent, hold still open" shape the doc itself says this module
    /// exists to close. The count would also go stale silently, which is the smaller problem.
    ///
    /// THE CHECK IS A SOURCE SCAN, and it is deliberately crude about what it will not catch: it
    /// looks at each production spawn site and asks whether a `.begin()` appears within the few
    /// lines BEFORE it. A registration made somewhere else entirely - passed in as a field and
    /// begun by the callee, say - would read as missing. That is the same limitation the
    /// config-unwired guard documents, and it is stated rather than hidden because a guard whose
    /// blind spots are unknown is worse than none.
    ///
    /// `bin/` IS EXCLUDED for a reason rather than convenience: those are separate programs with
    /// their own runtimes and no graceful-shutdown drain, so a spawn there is not a task this
    /// module has ever promised to wait for.
    #[test]
    fn every_production_spawn_site_registers_with_the_tracker() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files: Vec<std::path::PathBuf> = Vec::new();
        collect_rs_files(&src, &mut files);
        assert!(
            files.len() > 20,
            "only {} source files were found, so this guard is scanning far less than the crate",
            files.len()
        );

        let test_only = test_only_modules();
        assert!(
            !test_only.is_empty(),
            "no `#[cfg(test)] pub mod` was found in lib.rs, so this scan is not reading what it thinks it is"
        );

        let mut untracked: Vec<String> = Vec::new();
        let mut tracked = 0usize;

        for path in &files {
            let rel = path
                .strip_prefix(&src)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            if rel.starts_with("bin/") {
                continue;
            }
            // A whole file gated behind `#[cfg(test)]` in lib.rs is not part of the server.
            let file_name = rel.rsplit('/').next().unwrap_or(&rel).to_string();
            if test_only.contains(&file_name) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            let lines: Vec<&str> = text.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") {
                    continue;
                }
                if !line.contains("tokio::spawn(") {
                    continue;
                }
                if !in_production_code(&lines, i) {
                    continue;
                }
                let from = i.saturating_sub(6);
                let registered = lines[from..i].iter().any(|l| l.contains(".begin()"));
                if registered {
                    tracked += 1;
                } else {
                    untracked.push(format!("{rel}:{}", i + 1));
                }
            }
        }

        assert!(
            untracked.is_empty(),
            "these production `tokio::spawn` sites do not register with the shutdown tracker, so a graceful shutdown can return before they finish - the 'reply sent, hold still open' shape this module exists to close. Take a guard with `tasks.begin()` immediately before the spawn and move it into the task. Sites: {untracked:?}"
        );

        // AND THE COUNT THE MODULE DOC CLAIMS IS STILL TRUE, pinned so a fifth site is a
        // deliberate edit to the doc rather than a silent drift.
        assert_eq!(
            tracked, 4,
            "shutdown.rs's module doc says 'four production spawn sites on the money path'. MEASURED: {tracked} register today. If that changed, update the doc in this commit - the count is what tells a reader how much this module carries."
        );
    }
}
