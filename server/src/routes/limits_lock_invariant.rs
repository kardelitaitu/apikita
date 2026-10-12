//! The invariant that makes `TEST_LIMITS_OVERRIDE`'s `.expect()` safe, enforced rather than inferred.
//!
//! `limits_config()` reads a process-wide `Mutex` on every request and calls
//! `.expect("the test override lock is never poisoned")` on the lock result. A poisoned `Mutex` makes
//! that `.expect()` **panic a request handler**, which is a 500 for a customer.
//!
//! It cannot happen today, and the reason is worth stating precisely because it is an INVARIANT rather
//! than a property of the line:
//!
//!   * A `Mutex` is poisoned only when a thread panics **while holding the guard**.
//!   * The read path holds the guard as a temporary in one expression -
//!     `.lock().expect(..).clone()` - and nothing between the lock and the drop can panic (the clone is
//!     of a struct of plain integers). So the reader cannot poison it.
//!   * The only WRITERS are `TestLimitsGuard::set` and its `Drop`, both `#[cfg(test)]`. Production
//!     never writes it, so the lock stays `None` and unpoisoned for the life of the process.
//!
//! The risk is therefore not present-but-broken, it is **FUTURE-fragile**: the day someone adds a
//! production writer whose critical section can panic, this `.expect()` starts taking down request
//! handlers - and nothing in the suite would say so until it happened in production.
//!
//! The repository is already inconsistent about exactly this. `routes/keys.rs:910` guards its cache lock
//! with `unwrap_or_else(|err| err.into_inner())` - the poisoned-recovery idiom, which treats a poisoned
//! lock as recoverable rather than fatal - and so does this file's own `TestLimitsGuard::drop`. Only the
//! read path uses `.expect()`.
//!
//! This test does not change the behaviour; it makes the assumption CHECKABLE. It reads the source and
//! fails if a production writer appears, which is the moment the `.expect()` becomes a real hazard.

use std::fs;
use std::path::Path;

/// The file holding the override, and the two facts that must stay true.
fn auth_source() -> String {
    for p in ["src/routes/auth.rs", "../src/routes/auth.rs"] {
        if Path::new(p).exists() {
            return fs::read_to_string(p).expect("read auth.rs");
        }
    }
    panic!("could not locate src/routes/auth.rs to check the lock invariant");
}

/// Every line that WRITES the override - i.e. appears on the left of an assignment or takes `&mut`.
///
/// Deliberately textual: the point is to catch a future edit, and a future edit is a source change.
fn writer_lines(src: &str) -> Vec<(usize, String)> {
    src.lines()
        .enumerate()
        .filter(|(_, l)| l.contains("TEST_LIMITS_OVERRIDE"))
        .filter(|(_, l)| {
            // A write is a lock whose guard is then assigned through, or bound as `mut`.
            l.contains("let mut current")
                || l.contains("*current =")
                || l.contains("= None;")
                || l.contains("= Some(")
        })
        .map(|(i, l)| (i + 1, l.trim().to_string()))
        .collect()
}

/// The invariant: every writer of the override is test-gated.
///
/// Not "no writer panics" - that is unprovable textually. This is the stronger, checkable
/// approximation: no writer exists in a production build at all, so the lock cannot be poisoned
/// by anything a customer can reach.
#[test]
fn the_test_override_lock_has_no_production_writer() {
    let src = auth_source();
    let lines: Vec<&str> = src.lines().collect();

    // Where the test module begins. A writer after this point is test-gated.
    let mod_tests_at = lines
        .iter()
        .position(|l| l.trim_start().starts_with("mod tests {"))
        .expect(
            "auth.rs no longer has a `mod tests` block. This check locates the test boundary to decide \
             whether a writer is production code; without it the check cannot tell, and assuming would \
             make it pass vacuously",
        );

    let writers = writer_lines(&src);
    assert!(
        !writers.is_empty(),
        "found NO writers of TEST_LIMITS_OVERRIDE. Either the override was removed - in which case this \
         test and the invariant it protects are both obsolete and should go together - or the detection \
         below no longer matches the code, which would make this check silently vacuous"
    );

    for (n, line) in &writers {
        assert!(
            *n > mod_tests_at,
            "TEST_LIMITS_OVERRIDE is WRITTEN at auth.rs:{n}, which is PRODUCTION code (the test module \
             starts at line {}). {} A production writer can panic while holding the guard and poison \
             this lock, and `limits_config` answers a poisoned lock by `.expect()`-ing - which PANICS A \
             REQUEST HANDLER. If a production writer is genuinely needed, change that `.expect()` to \
             `.unwrap_or_else(|err| err.into_inner())` first, matching the idiom already used at \
             routes/keys.rs:910 and in this file's own TestLimitsGuard::drop",
            mod_tests_at + 1,
            line
        );
    }
}

/// The other half: the READ path must not be able to poison the lock itself.
///
/// If a future edit held the guard across something that can panic - an I/O call, a parse, a `.unwrap()`
/// on user data - then even test-only writers would be enough to poison it, and the `.expect()` above
/// would become reachable from a test that panicked. This pins that the read is a single expression.
#[test]
fn the_read_path_holds_the_guard_only_for_a_clone() {
    let src = auth_source();
    let lines: Vec<&str> = src.lines().collect();

    // Find the READ: the line that binds the override, not a comment that merely mentions it. The
    // first version matched the doc comment above the binding and the window then missed the clone,
    // which is the check failing for its own reason rather than the code's.
    let at = lines
        .iter()
        .position(|l| {
            let t = l.trim_start();
            !t.starts_with("//") && l.contains("TEST_LIMITS_OVERRIDE") && l.contains("let ")
        })
        .expect(
            "the READ of TEST_LIMITS_OVERRIDE was not found as a `let` binding. If the read was \
             restructured, this check needs rewriting - it must not pass by failing to find anything",
        );

    // The whole expression, joined, since rustfmt splits it across lines.
    let window = lines[at..(at + 6).min(lines.len())].join(" ");

    assert!(
        window.contains(".clone()"),
        "the read at auth.rs:{} no longer clones inside one expression: `{window}`. The safety argument \
         is that the guard lives only for a clone of plain integers, so nothing can panic while it is \
         held. If the guard is now held across real work, a panic anywhere in that work poisons the \
         lock and the next request panics in `limits_config`",
        at + 1
    );
}
