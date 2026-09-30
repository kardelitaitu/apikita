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
    // choice. These two lints catch two of the common accidental forms:
    // indexing that can go out of bounds, and an unwrap that turns a Result into
    // a panic.
    //
    // THIS COMMENT ONCE ALSO CLAIMED THESE CATCH ARITHMETIC OVERFLOW. THEY DO NOT.
    // Measured, by planting each form in a production item and asking this gate:
    // out-of-bounds indexing is refused (indexing_slicing), a Result.expect() is
    // refused (unwrap_used), and i64 addition, multiplication and subtraction are
    // all ACCEPTED. None of the two says anything about arithmetic, and
    // [profile.release] below leaves overflow-checks at its default of OFF, so
    // an overflow in the build that ships wraps silently.
    //
    // The lint that does say it is clippy::arithmetic_side_effects. It is NOT
    // enabled here, because at crate level it reports 43 sites, most of them
    // arithmetic that cannot overflow in practice (a u8 counter, a Duration) and
    // where an overflow would cost a wrong counter rather than wrong money.
    //
    // IT IS enabled where the trade-off inverts, one module at a time. THE RULE,
    // which is the part that has to stay true: a module that computes a figure a
    // customer is charged, or a decision that controls access, carries its own deny
    // of this lint, scoped exactly as this one is. Every existing site is either
    // written `checked`/`saturating` or carries a `#[allow]` with the reason its
    // operands are bounded AT THE SITE - never a blanket allow at the top of a file,
    // which would silence the lint for everything added afterwards and so defeat the
    // point of adding it.
    //
    // The membership count is deliberately not written here. An earlier version said
    // "four modules", then "five", and each time it was stale within a round - which
    // is this repository's own most persistent defect class, and no reason to add
    // another instance of it inside the file whose job is to describe the gate. The
    // dated measurement below is enough: six as of this writing, out of 43 sites at
    // crate level.
    //
    // WHAT THE FENCE HAS PAID SO FAR, since the reason it is worth extending module
    // by module is that it finds things. The cheap modules cost nothing - zero sites
    // outside their tests, because the money arithmetic all lives in db.rs and the
    // others route to it. The expensive ones found real defects:
    //
    //   * db.rs, nine sites. One was bounded only by MAGNITUDE rather than by
    //     construction, so it became a checked_sub that refuses the settlement
    //     instead of writing a wrapped negative balance into an append-only log.
    //   * routes/keys, three sites. The 30-day spend total accumulated with a
    //     wrapping `+=`, so a total crossing i64::MAX came back NEGATIVE - and a
    //     negative spend is "nowhere near the limit", so the arithmetic computing a
    //     limit's own input was the one thing that could switch the limit off. It
    //     became saturating_add, because for a ceiling the safe direction is to read
    //     too high.
    //
    // Fencing a module is how you find out which of its sites were arguments and
    // which were bugs, and that is the argument for spending the budget on the
    // expensive ones first rather than sweeping the cheap ones for volume.
    //
    // So the honest statement of what protects money is now three things, not two:
    // the schema and the reconciliation gate below, plus a lint on the modules where
    // an overflow would be financial. Wallets carry CHECK (balance_idr >= 0) and
    // tools/reconcile compares SUM(ledger.delta_idr) to the balance per account, so a
    // wrapped NEGATIVE is refused by the CHECK and a wrapped ledger value is caught
    // by reconcile. What neither can catch is a wrap that lands on a plausible
    // POSITIVE figure - which is why the non-finite price guard in config.rs is a
    // hard validation rather than a lint too, and why a config that bills at zero
    // cannot be reconciled away.
    //
    // Extending the deny to the remaining modules is available and is not done: each
    // site there needs the same judgement, and doing 43 of them in one pass is how
    // a written argument ends up being a rubber stamp.
    //
    // SCOPED TO NON-TEST BUILDS, because a panic is an availability incident
    // only on the request path. A test that unwraps a fixture it just built has
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
pub mod auth_attempts;
pub mod config;
pub mod db;
/// Test-only: checks claims the OPERATIONAL documents make about this code, so a
/// citation cannot quietly re-point at a neighbour the way a line number does.
/// Compiled out of every non-test build, like `test_support`.
#[cfg(test)]
mod doc_claims;
/// Test-only: checks the SCHEMA against the promises the customer-facing pages make
/// about it — above all that no request-path table holds request or response
/// text, which is what lets the privacy page say we are a proxy and not a data
/// processor. Its own module because it reads a migration file rather than a
/// document, and `doc_claims` would then have been a name that lied.
#[cfg(test)]
mod doc_schema;
pub mod error;
pub mod identity;
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
