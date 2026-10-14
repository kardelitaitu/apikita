//! Identity: who a person is, and how they prove it.
//!
//! This module is what replaces PocketBase. Before it, `routes/auth.rs` handed a
//! token to an external service and asked whether it was real; the account row it
//! created was keyed on that service's id (`accounts.pb_user_id`). After it, the
//! crate owns the whole loop: it hashes the password, mints the token, sends the
//! mail, verifies the Google ID token and decides which account a sign-in belongs
//! to.
//!
//! ## The four submodules and why they are separate
//!
//! | module | owns | its hard part |
//! |---|---|---|
//! | [`password`] | Argon2id, the password policy | password hashing is CPU-bound and must not block a runtime worker |
//! | [`tokens`] | the single-use email links | a link travels through mailboxes, so it must be single-use and stored only as a hash |
//! | [`google`] | ID token verification | a JWT from a third party is a claim until its signature and its audience are checked |
//! | [`accounts`] | the identity rows and the linking rule | the four pre-hijacking vectors, which are all about ORDER |
//!
//! They are separate files rather than one because each has a decision in it that
//! a reader needs to be able to find without reading the other three, and because
//! `accounts` is the only one that needs the others.
//!
//! ## The rule that outranks convenience
//!
//! `email_verified` is written in a CLOSED SET of places. Two of them can write `1`, and the
//! register (`docs/architecture/identity.md`) fixes both:
//!
//! 1. the Google identity is inserted with `1`, and only after `verify_id_token` has
//!    checked the token's own `email_verified` claim;
//! 2. `mark_verified` writes `1`, and its only production caller is `verify_email`, which
//!    reaches it by redeeming a token DELIVERED to that address.
//!
//! Signup writes `0` (`create_password_account`) — nothing in that path proves the address.
//!
//! **THE RESET PATH DELIBERATELY WRITES NOTHING.** `set_password` updates `password_hash`
//! and no verification column, and `upsert_password_identity` takes a `verified` flag whose
//! only production caller passes `false`. A reset proves the mailbox was reachable at that
//! moment, but the verified transition is its own claim; marking it here would let a reset
//! launder an unverified address into a linkable one.
//!
//! THIS PARAGRAPH USED TO LIST THREE WRITERS AND THE THIRD WAS THE RESET, which was the
//! opposite of the code and of `identity.md` — a reader following it would have added the
//! write the security decision forbids. It is corrected above rather than deleted, because
//! the wrong version names the exact mistake to avoid.
//!
//! Nothing else sets it — not an admin endpoint, not a migration, not a call from
//! a route that has decided it would be convenient. The pre-hijacking defences
//! depend on it: the linking rule in [`accounts`] asks whether the PASSWORD
//! identity's address was verified (and verified before the Google identity
//! existed), so a `1` written by anything but proof of the address turns an
//! attacker's unverified row into an authorisation.
//!
//! ## Why these rules are Rust and not SQL triggers
//!
//! `identities.email_verified` has a CHECK constraint, and the tempting design is
//! to enforce the ordering in a trigger so no code path can violate it. It was
//! rejected, for three reasons that are worth restating because the idea recurs:
//!
//! * `routes/mod.rs` decides session liveness in Rust "so the rule this crate
//!   ships is the rule its tests exercise". The same argument applies here: a
//!   trigger's rule is exercised by whichever test remembers to provoke it.
//! * `doc_claims` guards read Rust source. A guard cannot see a trigger, so the
//!   discipline that keeps every other claim honest would go blind exactly here.
//! * A trigger fires on paths nobody reviews. Identity is the one area where an
//!   unreviewed writer is the whole threat model.

pub mod accounts;
pub mod email;
pub mod google;
pub mod password;
pub mod tokens;
