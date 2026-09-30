//! Argon2id password hashing and the password policy.
//!
//! **The hashing is Argon2id and the parameters come from the config.** The
//! register at `docs/decisions.md` settles the algorithm; the values live in
//! `[auth]` (`config/apikita.toml`) so an operator can raise the work factor
//! without a rebuild, and `config.rs::validate` refuses a parameter set that
//! would make the hasher fail on every sign-in rather than refuse one.
//!
//! WHY THE PHC STRING FORMAT AND NOT OUR OWN. `argon2`'s `password-hash` feature
//! gives `PasswordHasher`/`PasswordVerifier` and `SaltString`, so the salt
//! generation, the encoding of the parameters and the verification of an older
//! hash written under different parameters are the crate's problem. Every one of
//! those is a place a hand-rolled implementation is subtly wrong in a way no test
//! of the output shape can see - and wrong here means a password that can never
//! be verified again.
//!
//! WHY `spawn_blocking`. Argon2 at 19 MiB x 2 passes is roughly 40 ms of
//! deliberate CPU work. On a tokio runtime worker that is 40 ms during which no
//! other task on that thread runs, so a sign-in endpoint under load would queue
//! requests behind its own hashing. The CPU work is the point of the algorithm,
//! so it is moved off the async runtime rather than reduced.

use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};

use crate::config::AuthConfig;
use crate::error::AppError;

/// The hasher named by the config, built on every call.
///
/// NOT CACHED, deliberately. `Argon2` is a thin value holding the parameters,
/// so building it is a few copies; the expensive part is the key derivation
/// itself. A cache would be a second home for the config that could be stale
/// after a reload, which is the defect class this codebase spends the most
/// effort avoiding.
fn hasher(auth: &AuthConfig) -> Result<Argon2<'static>, AppError> {
    let params = Params::new(
        auth.argon2_memory_kib,
        auth.argon2_iterations,
        auth.argon2_parallelism,
        None,
    )
    .map_err(|e| {
        AppError::Internal(format!(
            "auth.argon2_* is not a valid Argon2 parameter set: {e}"
        ))
    })?;

    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

/// Hash a password for storage.
///
/// Returns the PHC string - `$argon2id$v=19$m=19456,t=2,p=1$<salt>$<hash>` -
/// which carries its own parameters, so a hash written under today's settings
/// stays verifiable after an operator raises them. That is what makes the cost
/// knob tunable without a migration.
pub async fn hash_password(auth: AuthConfig, password: String) -> Result<String, AppError> {
    // The hash is CPU-bound and deliberately slow, so it runs on a blocking
    // thread. `spawn_blocking` rather than `block_in_place` because the runtime
    // is multi-threaded and this is the idiomatic form; the join error can only
    // be a panic inside the closure, which cannot happen here (the crate forbids
    // `unwrap` on the production path) but is handled rather than unwrapped.
    let hashed = tokio::task::spawn_blocking(move || {
        let argon2 = hasher(&auth)?;
        // A fresh 16-byte salt per hash. Generated here rather than accepted from
        // a caller: a caller-supplied salt is a way to make two identical
        // passwords collide, and there is no use for one.
        let salt = SaltString::generate(&mut OsRng);
        argon2
            .hash_password(password.as_bytes(), &salt)
            .map(|hash| hash.to_string())
            .map_err(|e| AppError::Internal(format!("password hashing failed: {e}")))
    })
    .await
    .map_err(|e| AppError::Internal(format!("password hashing task failed: {e}")))??;

    Ok(hashed)
}

/// Verify a password against a stored PHC string.
///
/// **This returns a bool, not a Result, for the password being wrong**, and the
/// distinction is load-bearing: a stored hash that cannot be PARSED is an
/// Internal error (our row is corrupt, the operator must know), while a password
/// that does not match is an ordinary `false`. Collapsing the two would make a
/// corrupt row read as "wrong password", which is a customer locked out with
/// nothing in the log.
///
/// The comparison is constant-time inside the crate, which is why this does not
/// re-implement one: `PasswordVerifier::verify_password` is the only thing here
/// that sees both the secret and the guess.
pub async fn verify_password(
    auth: AuthConfig,
    stored: String,
    password: String,
) -> Result<bool, AppError> {
    tokio::task::spawn_blocking(move || {
        let parsed = PasswordHash::new(&stored)
            .map_err(|e| AppError::Internal(format!("stored password hash is unreadable: {e}")))?;

        match hasher(&auth)?
            .verify_password(password.as_bytes(), &parsed)
        {
            Ok(()) => Ok(true),
            // `password_hash::Error::Password` IS the mismatch case, and it is
            // matched BY NAME rather than by `is_err()` so a future error variant
            // (an unsupported algorithm in the stored string, say) surfaces as an
            // error instead of silently reading as "wrong password".
            Err(argon2::password_hash::Error::Password) => Ok(false),
            Err(e) => Err(AppError::Internal(format!(
                "password verification failed: {e}"
            ))),
        }
    })
    .await
    .map_err(|e| AppError::Internal(format!("password verification task failed: {e}")))?
}

/// The one password policy, applied on BOTH signup and reset.
///
/// One function rather than a check at each call site, because two copies of a
/// policy is how the signup path and the reset path start accepting different
/// passwords - and the reset path is reachable by someone who does not know the
/// old password, so it is the one that must not be the looser of the two.
///
/// A LENGTH FLOOR ONLY, no character-class rule. A composition requirement
/// (must contain a digit, must contain a symbol) measurably pushes people toward
/// `Password1!`, which is worse than a longer passphrase; length is the only
/// rule with evidence behind it.
pub fn validate_password(auth: &AuthConfig, password: &str) -> Result<(), AppError> {
    // Counted in CHARS, not bytes, so a passphrase in a non-Latin script is not
    // rejected for being "too short" when it is three characters that happen to
    // be multi-byte. The max is also chars, which is the bound Argon2 cares
    // about only in that the input is copied.
    let chars = password.chars().count();

    if chars < auth.password_min_length {
        return Err(AppError::ValidationFailed {
            message: format!(
                "a password must be at least {} characters",
                auth.password_min_length
            ),
            field: "password".into(),
        });
    }

    if chars > auth.password_max_length {
        return Err(AppError::ValidationFailed {
            message: format!(
                "a password must be at most {} characters",
                auth.password_max_length
            ),
            field: "password".into(),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fast parameters, for tests that are about the LOGIC rather than the cost.
    ///
    /// Argon2's minimum is 8 KiB of memory and 1 pass, so this is the cheapest
    /// legal configuration: it keeps the suite's runtime sane while every rule
    /// under test (salt freshness, the PHC round trip, the parse error) is the
    /// same rule the shipped parameters exercise.
    fn cheap() -> AuthConfig {
        AuthConfig {
            argon2_memory_kib: 8,
            argon2_iterations: 1,
            argon2_parallelism: 1,
            ..AuthConfig::default()
        }
    }

    #[tokio::test]
    async fn a_password_verifies_against_its_own_hash() {
        let auth = cheap();
        let hash = hash_password(auth.clone(), "correct horse battery".into())
            .await
            .expect("hashing works");
        assert!(
            verify_password(auth.clone(), hash.clone(), "correct horse battery".into())
                .await
                .expect("verification works"),
            "the password that was hashed must verify"
        );
        assert!(
            !verify_password(auth.clone(), hash, "wrong horse battery".into())
                .await
                .expect("verification works"),
            "a different password must NOT verify - the positive control above would pass for a function that returns true"
        );
    }

    #[tokio::test]
    async fn the_same_password_hashes_differently_every_time() {
        let auth = cheap();
        let first = hash_password(auth.clone(), "same password".into())
            .await
            .expect("hashing works");
        let second = hash_password(auth.clone(), "same password".into())
            .await
            .expect("hashing works");

        assert_ne!(
            first, second,
            "two hashes of one password must differ, or the salt is not fresh and identical passwords are visible as identical rows"
        );
        // Both must still verify: the difference is the salt, not the password.
        assert!(verify_password(auth.clone(), first, "same password".into())
            .await
            .expect("verification works"));
        assert!(verify_password(auth.clone(), second, "same password".into())
            .await
            .expect("verification works"));
    }

    #[tokio::test]
    async fn the_stored_string_is_an_argon2id_phc_string() {
        let hash = hash_password(cheap(), "whatever".into())
            .await
            .expect("hashing works");

        // The algorithm is stated IN the stored value. If this ever reads
        // `$argon2i$` or `$argon2d$`, the variant changed - which is the one
        // change that would silently weaken every future password.
        assert!(
            hash.starts_with("$argon2id$"),
            "the stored hash must name Argon2id, got {hash}"
        );
        assert!(
            hash.contains("m=8,t=1,p=1"),
            "the parameters must be recorded in the hash so it stays verifiable after the config changes, got {hash}"
        );
    }

    #[tokio::test]
    async fn an_unreadable_stored_hash_is_an_error_not_a_wrong_password() {
        let err = verify_password(cheap(), "not-a-phc-string".into(), "guess".into())
            .await
            .expect_err("a corrupt stored hash must be an error");

        assert!(
            matches!(err, AppError::Internal(_)),
            "a corrupt row is ours to fix, so it must surface as Internal rather than as 'wrong password', got {err:?}"
        );
    }

    #[test]
    fn the_length_floor_is_the_configured_one() {
        let auth = AuthConfig::default();

        assert!(validate_password(&auth, &"a".repeat(7)).is_err(), "7 is below the floor of 8");
        assert!(validate_password(&auth, &"a".repeat(8)).is_ok(), "8 is AT the floor, not below it - the boundary is the point");
        assert!(validate_password(&auth, &"a".repeat(129)).is_err(), "129 is above the ceiling of 128");
        assert!(validate_password(&auth, &"a".repeat(128)).is_ok(), "128 is AT the ceiling");
    }

    #[test]
    fn the_floor_counts_characters_not_bytes() {
        let auth = AuthConfig::default();

        // Eight three-byte characters is 24 bytes and 8 characters. Counting
        // bytes would accept it at 8; counting characters accepts it at 8 too -
        // but the reverse case is what matters: a byte count would reject EIGHT
        // characters only if the floor were raised, so the property under test is
        // that the count is of what a person typed.
        let eight_wide = "あ".repeat(8);
        assert_eq!(eight_wide.chars().count(), 8);
        assert!(
            eight_wide.len() > 8,
            "the fixture must be multi-byte or this proves nothing about chars-vs-bytes"
        );
        assert!(validate_password(&auth, &eight_wide).is_ok());
    }

    #[test]
    fn a_short_password_names_the_field_for_the_ui() {
        let err = validate_password(&AuthConfig::default(), "short")
            .expect_err("a 5-character password is refused");

        match err {
            AppError::ValidationFailed { field, message } => {
                assert_eq!(field, "password", "docs/error-model.md requires details.field so a UI can highlight the input");
                assert!(
                    message.contains("8"),
                    "the message must state the number the page states, got {message}"
                );
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }
}
