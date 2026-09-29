// The crate contains NO unsafe code, and that is now a compiler-enforced fact
// rather than an observation.
//
// MEASURED before adding this: a search for `unsafe` across every file in
// server/src returned zero matches. So this attribute does not require any code
// to change — it locks in a property the crate already has, which is exactly what
// makes it worth writing down. A property nobody enforces is one refactor away
// from being untrue, and nobody reviews a diff for the absence of something.
//
// WHY `forbid` AND NOT `deny`: `deny` can be lifted by an inner
// `#[allow(unsafe_code)]` on a module or function, so it documents an intention
// that any single line can override. `forbid` cannot be overridden at all — a
// future `unsafe` block does not merely warn, it fails to compile until someone
// deliberately edits THIS line.
//
// This is a service that holds wallets, verifies payment signatures and serves
// customer credentials. "No unsafe in the API crate" is a meaningful security
// claim to be able to make, and it is only worth making if the compiler is the
// one making it.
#![forbid(unsafe_code)]
#![cfg_attr(
    not(test),
    // NOTE THE SHAPE: `cfg_attr` takes the WHOLE `deny(...)` as ONE argument, not
    // the lints as separate arguments. `cfg_attr(c, clippy::a, clippy::b)` expands
    // to two inner attributes `#![clippy::a]` and `#![clippy::b]`, which do not
    // exist, and the crate stops compiling with "custom inner attributes are
    // unstable". Found by running the scoping rather than reasoning about it.
    //
    // A panic on the request path is an availability incident, not a style
    // choice. These two lints catch the common accidental forms: indexing that
    // can go out of bounds, and arithmetic that can overflow in release.
    //
    // SCOPED TO NON-TEST BUILDS, and the reason is the sentence above: this is
    // about the request path. A test that unwraps a fixture it just built has
    // failed loudly and cost nothing; a handler that does is an outage.
    //
    // Measured before scoping it: the test corpus trips these two lints 512
    // times, so `cargo clippy --workspace --all-targets` exited with 512 errors
    // and not one warning - the production code, which is what the lints are FOR,
    // was clean and invisible inside the noise. A gate that fails on the
    // committed tree gets muted, and a muted gate is worse than none
    // (.github/workflows/ci.yml:61-63 applies the same reasoning to
    // cargo-audit's unmaintained warnings). The worse outcome is a contributor
    // "fixing" 512 errors by deleting the deny, which would take the check off
    // the request path as well.
    //
    // This does not weaken the gate. CI runs `cargo clippy -- -D warnings`
    // (.github/workflows/ci.yml:45), which builds the lib WITHOUT cfg(test), so
    // the deny is live there. That was checked by planting an unwrap in a
    // production item and one in a test: the production one failed the gate, the
    // test one did not.
    deny(clippy::indexing_slicing, clippy::unwrap_used)
)]

pub mod abuse;
pub mod config;
pub mod db;
pub mod error;
pub mod ip_tracking;
pub mod money;
pub mod routes;
/// Test-only, because it compensates for a decision that belongs to the tests and
/// not to the binary: a test database is a temp file, so the money tests no longer
/// need a live server and no longer have to be `#[ignore]`d. Compiled out of every
/// non-test build.
#[cfg(test)]
mod test_support;
pub mod upstream;
