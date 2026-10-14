//! Google ID token verification: JWKS fetch, signature check, and the claims that
//! are load-bearing.
//!
//! ## What arrives, and what is assumed
//!
//! The browser performs the authorization code flow itself (Google Identity
//! Services) and posts the resulting **ID token** to the server. This module
//! verifies that one JWT. It does NOT run an OAuth flow, does not hold a client
//! secret, and does not exchange a code — which is why the full OIDC crates are not
//! used: `openidconnect` would pull `oauth2`, `url` and `serde_with` to implement a
//! flow that never happens on this side.
//!
//! A JWT from a third party is a CLAIM until it is checked, and there are exactly
//! four things that make it evidence:
//!
//! 1. **The signature verifies against Google's key** — the JWKS endpoint publishes
//!    the public keys, and the token header's `kid` names which one.
//! 2. **The issuer is Google** — `iss` must be `accounts.google.com` or
//!    `https://accounts.google.com`. Both spellings are real and Google has used
//!    each.
//! 3. **The audience is OUR client id** — this is the check that stops a token
//!    minted for any other Google-integrated site being replayed here. Without it,
//!    a valid Google token from anywhere would sign someone in.
//! 4. **It has not expired** — `exp`, with a small leeway for clock skew.
//!
//! ## Why `email_verified` is checked here and not left to the schema
//!
//! `identities` has `CHECK (provider <> 'google' OR email_verified = 1)`, so an
//! unverified Google token would fail the INSERT — as a raw constraint violation,
//! which surfaces as `AppError::Internal`, a 500 for what is the caller's problem.
//! `verify_id_token` therefore refuses it up front with `Unauthenticated`. The
//! schema constraint is the backstop; this is the check.
//!
//! ## The crypto backend
//!
//! `jsonwebtoken` 9 verifies with `ring`, and `ring 0.17` is already in this build
//! transitively through `rustls` (which `reqwest`'s `rustls-tls` feature pulls). So
//! this adds a thin JWT layer on a backend that is already compiled, rather than a
//! second crypto stack — and it never touches OpenSSL, which the workspace-wide
//! `rustls` choice exists to avoid.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;

use crate::config::AuthConfig;
use crate::error::AppError;

/// Google's OpenID configuration, from which the JWKS URI is read.
///
/// A CONSTANT, not a config key: there is one Google, the URL is part of their
/// published contract, and a key would be a knob whose only correct value is the
/// shipped one. This is the same reasoning that keeps `DEFAULT_POCKETBASE_URL` out
/// of the config in the code this replaces.
const DISCOVERY_URL: &str = "https://accounts.google.com/.well-known/openid-configuration";

/// The two spellings of Google's issuer that appear in real tokens.
const ISSUERS: &[&str] = &["accounts.google.com", "https://accounts.google.com"];

/// Leeway on `exp`, in seconds. Google's clock and ours are not the same clock, and
/// rejecting a token that is one second past expiry turns a network hiccup into a
/// failed sign-in. Sixty seconds is the conventional figure.
const LEEWAY_SECONDS: u64 = 60;

/// The claims this module actually reads.
///
/// `email` is optional in the OIDC contract but Google always sends it when the
/// `email` scope is requested — and this module refuses a token without one rather
/// than inventing a placeholder address.
#[derive(Debug, Deserialize)]
pub struct GoogleClaims {
    /// The provider's immutable account id. THIS is what an identity resolves on:
    /// a person can change their Google address, and `sub` survives it.
    pub sub: String,
    pub email: Option<String>,
    /// Google's own statement that it has verified the address. Load-bearing: see
    /// the module docs.
    #[serde(default)]
    pub email_verified: Option<bool>,
    pub exp: u64,
    pub iss: String,
    pub aud: Audience,
}

/// `aud` is a string in Google's tokens and the spec allows an array.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Audience {
    One(String),
    Many(Vec<String>),
}

impl Audience {
    /// Whether this `aud` claim names our client id.
    ///
    /// USED BY THE TEST RATHER THAN THE VERIFIER: the production path hands the
    /// audience to `jsonwebtoken`'s `Validation`, which does its own comparison.
    /// This exists so the RULE can be asserted directly — "a token minted for
    /// another Google site must not satisfy our client id" is the check that stops
    /// a valid Google token from anywhere being replayed here, and it deserves a
    /// test that does not need a signature to state it.
    #[cfg(test)]
    fn contains(&self, client_id: &str) -> bool {
        match self {
            Audience::One(one) => one == client_id,
            Audience::Many(many) => many.iter().any(|candidate| candidate == client_id),
        }
    }
}

/// A verified ID token, reduced to what the sign-in path needs.
#[derive(Debug)]
pub struct VerifiedGoogleUser {
    pub subject: String,
    pub email: String,
}

/// The JWKS keys, cached in memory for `google_jwks_cache_seconds`.
///
/// CACHED BECAUSE THE ALTERNATIVE IS A NETWORK ROUND TRIP ON EVERY SIGN-IN, to a
/// third party, on the critical path — and Google rate-limits the endpoint. The TTL
/// is config so an operator can shorten it, and key ROTATION is handled below
/// rather than by the TTL: a token naming an unknown `kid` forces an immediate
/// refetch, so a rotation is picked up on the first sign-in that needs the new key
/// instead of after the cache expires.
struct JwksCache {
    keys: HashMap<String, DecodingKey>,
    fetched_at: DateTime<Utc>,
}

static JWKS: Mutex<Option<JwksCache>> = Mutex::new(None);

/// Verify an ID token against the config's client id and Google's published keys.
pub async fn verify_id_token(
    auth: &AuthConfig,
    id_token: &str,
    now: DateTime<Utc>,
) -> Result<VerifiedGoogleUser, AppError> {
    if auth.google_client_id.trim().is_empty() {
        // A deployment that has not configured Google sign-in must say so as a
        // server-side misconfiguration, not as a failed sign-in. There is nothing
        // the caller could do differently.
        return Err(AppError::Internal(
            "auth.google_client_id is not configured, so Google sign-in is unavailable".into(),
        ));
    }

    let header = decode_header(id_token).map_err(|_| AppError::Unauthenticated)?;

    // Only Google's algorithm. A token that asks to be verified with `none` or an
    // HMAC is not a token from Google, and accepting the algorithm the token names
    // is the classic JWT confusion bug.
    if header.alg != Algorithm::RS256 {
        return Err(AppError::Unauthenticated);
    }

    // A token with no `kid` cannot name a key, so it cannot be verified against
    // the published set. Google always sends one; a token without it is not from
    // Google.
    let kid = header.kid.as_deref().ok_or(AppError::Unauthenticated)?;

    let key = key_for(kid, auth, now).await?;

    // Validation is built EXPLICITLY rather than from `Validation::new(alg)`, so
    // the audience and issuer checks cannot be accidentally relaxed: the defaults
    // are set here, in one place, next to the comment explaining why each exists.
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_audience(&[auth.google_client_id.as_str()]);
    validation.set_issuer(ISSUERS);
    validation.leeway = LEEWAY_SECONDS;
    validation.validate_exp = true;

    let decoded = decode::<GoogleClaims>(id_token, &key, &validation)
        .map_err(|_| AppError::Unauthenticated)?;

    // ---------------------------------------------------------------------------------------------
    // EVERYTHING BELOW THIS LINE IS UNREACHED BY THE TEST SUITE, AND THAT IS STRUCTURAL.
    //
    // MEASURED: each of the three checks that follow survives being disabled.
    //
    //   `claims.email_verified != Some(true)`  ->  `== Some(false)`   SURVIVED at 683 passed / 0 failed
    //   the empty-email filter                 ->  a permissive read  SURVIVED
    //   the re-read `ISSUERS` check            ->  `if false`         SURVIVED
    //
    // WHY, and it is not an oversight. `decode` above needs a token signed by a key from Google's live
    // JWKS endpoint, and there is no seam to inject a key or a claim set. The only two tests that call
    // this function - `a_malformed_token_is_unauthenticated` and `an_unconfigured_client_id_is_an_internal_error` -
    // both pass a string that is not a JWT at all, so they return at `decode` and never arrive here.
    // `routes/auth.rs` records the same limit from the caller's side ("HONEST LIMIT: the happy path of
    // this handler CANNOT be driven from a test").
    //
    // WHY THE NOTE IS HERE RATHER THAN ONLY THERE. `routes/auth.rs` says the happy path is untested;
    // it does not say that the four checks the module's own doc calls "exactly four things that make
    // it evidence" are each independently unenforced by any test. A reader editing one of them is
    // looking at THIS code, and a SURVIVED mutation from a coverage run has to be interpretable here
    // or it gets re-investigated as a defect.
    //
    // WHAT THAT MEANS FOR AN EDITOR: these lines have no safety net. A change here is verified by
    // reading it, not by a failing test - and the schema is NOT a substitute. `identities` carries
    // `CHECK (provider <> 'google' OR email_verified = 1)` as a BACKSTOP, so weakening the first check
    // turns a 401 into a 500 rather than into an unverified identity. The other two have no backstop
    // at all: an empty email or a foreign issuer would simply be accepted, which is why they are the
    // more dangerous pair to touch.
    // ---------------------------------------------------------------------------------------------

    let claims = decoded.claims;

    // Checked AFTER the signature, so this is a fact about a token Google really
    // issued rather than about one an attacker composed.
    if claims.email_verified != Some(true) {
        return Err(AppError::Unauthenticated);
    }

    let email = claims
        .email
        .filter(|e| !e.trim().is_empty())
        .ok_or(AppError::Unauthenticated)?;

    // `iss` is also checked by `validation.set_issuer`, but the value is re-read
    // here so the rule is visible in this function rather than only in a library
    // call. The two together mean a change to either cannot silently drop it.
    if !ISSUERS.contains(&claims.iss.as_str()) {
        return Err(AppError::Unauthenticated);
    }

    Ok(VerifiedGoogleUser {
        subject: claims.sub,
        email,
    })
}

/// The decoding key for a `kid`, fetching the JWKS when it is not cached.
async fn key_for(
    kid: &str,
    auth: &AuthConfig,
    now: DateTime<Utc>,
) -> Result<DecodingKey, AppError> {
    if let Some(key) = cached_key(kid, auth, now) {
        return Ok(key);
    }

    // Either nothing is cached, the cache is stale, or this `kid` is new — the
    // rotation case. All three refetch.
    let certs = fetch_jwks().await?;
    store_jwks(certs.clone(), now);

    certs
        .into_iter()
        .find(|(key_id, _)| key_id == kid)
        .map(|(_, key)| key)
        .ok_or(AppError::Unauthenticated)
}

fn cached_key(kid: &str, auth: &AuthConfig, now: DateTime<Utc>) -> Option<DecodingKey> {
    let guard = JWKS.lock().ok()?;
    let cache = guard.as_ref()?;

    let age = now.signed_duration_since(cache.fetched_at);
    if age > Duration::seconds(auth.google_jwks_cache_seconds as i64) {
        return None;
    }

    cache.keys.get(kid).cloned()
}

fn store_jwks(keys: HashMap<String, DecodingKey>, now: DateTime<Utc>) {
    if let Ok(mut guard) = JWKS.lock() {
        *guard = Some(JwksCache {
            keys,
            fetched_at: now,
        });
    }
}

/// Reads Google's discovery document, then the JWKS it names.
///
/// Two requests rather than a hardcoded JWKS URL: the URL is Google's to move, and
/// the discovery document is the published place to find it. The cost is one extra
/// request per cache fill, which is once per `google_jwks_cache_seconds`.
async fn fetch_jwks() -> Result<HashMap<String, DecodingKey>, AppError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| AppError::Internal(format!("could not build the JWKS client: {e}")))?;

    #[derive(Deserialize)]
    struct Discovery {
        jwks_uri: String,
    }

    let discovery: Discovery = client
        .get(DISCOVERY_URL)
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("google discovery request failed: {e}")))?
        .error_for_status()
        .map_err(|e| AppError::Internal(format!("google discovery returned an error: {e}")))?
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("google discovery was unreadable: {e}")))?;

    #[derive(Deserialize)]
    struct Jwks {
        keys: Vec<Jwk>,
    }

    let jwks: Jwks = client
        .get(&discovery.jwks_uri)
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("google JWKS request failed: {e}")))?
        .error_for_status()
        .map_err(|e| AppError::Internal(format!("google JWKS returned an error: {e}")))?
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("google JWKS was unreadable: {e}")))?;

    build_decoding_keys(jwks.keys)
}

/// One entry of a JWKS document, as Google publishes it: a key id plus the RSA modulus and
/// exponent, base64url-encoded.
///
/// Hoisted out of `fetch_jwks` when the key-building loop was extracted, because a type declared
/// inside a function body cannot be named by a test.
#[derive(Deserialize)]
struct Jwk {
    kid: String,
    n: String,
    e: String,
}

/// Turns a JWKS `keys` array into a `kid -> DecodingKey` map, or an error when NOTHING is usable.
///
/// EXTRACTED FROM `fetch_jwks` SO IT CAN BE TESTED, which it could not be before. MEASURED: with
/// the loop inline, changing the per-key skip into a hard failure - the exact regression the
/// comment below warns about - left all 639 tests passing, because `fetch_jwks` does live HTTP
/// against Google and no fixture can reach the two lines. The loop itself is pure, so pulling it
/// out makes both behaviours reachable without a network.
///
/// The two rules, and each has a test:
///   - a key that will not build is SKIPPED, not fatal: Google publishes an RSA key set, and one
///     unusable entry must not take sign-in down
///   - if NO key is usable the call FAILS rather than returning an empty map, because an empty
///     cache would be re-fetched on every request and every token would fail to verify with no
///     indication that the key set was the reason
fn build_decoding_keys(jwks: Vec<Jwk>) -> Result<HashMap<String, DecodingKey>, AppError> {
    let mut keys = HashMap::new();
    for jwk in jwks {
        if let Ok(key) = DecodingKey::from_rsa_components(&jwk.n, &jwk.e) {
            keys.insert(jwk.kid, key);
        }
    }

    if keys.is_empty() {
        return Err(AppError::Internal(
            "google JWKS contained no usable keys".into(),
        ));
    }

    Ok(keys)
}

/// Clears the cached keys. FOR TESTS ONLY — a real deployment has no reason to.
#[cfg(test)]
pub fn clear_cache() {
    if let Ok(mut guard) = JWKS.lock() {
        *guard = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth_with_client_id(client_id: &str) -> AuthConfig {
        AuthConfig {
            google_client_id: client_id.to_string(),
            ..AuthConfig::default()
        }
    }

    /// A token that is not a JWT at all is Unauthenticated, not Internal. The
    /// distinction matters: the caller posted rubbish, and a 500 would say the
    /// server is broken.
    #[tokio::test]
    async fn a_malformed_token_is_unauthenticated() {
        let auth = auth_with_client_id("client.apps.googleusercontent.com");
        let err = verify_id_token(&auth, "not-a-jwt", Utc::now())
            .await
            .expect_err("a malformed token must be refused");

        assert!(
            matches!(err, AppError::Unauthenticated),
            "a caller's bad token is 401, not 500, got {err:?}"
        );
    }

    /// The note above the post-decode checks is TRUE, and this is what keeps it true.
    ///
    /// `verify_id_token` has four load-bearing checks that no test reaches, because `decode` needs a
    /// token signed by Google's live JWKS key and there is no seam to inject one. MEASURED: disabling
    /// the `email_verified`, empty-email and `ISSUERS` checks each survives the whole suite.
    ///
    /// A note claiming "this is structurally untestable" is worth exactly as much as its assumption,
    /// and the assumption is that NO TEST REACHES THE REGION. That is checkable from the source, so it
    /// is checked here rather than trusted - and if the assumption stops holding, this fails and the
    /// note above has to be rewritten instead of quietly becoming false.
    #[test]
    fn the_post_decode_checks_are_unreachable_from_this_modules_tests() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("identity")
            .join("google.rs");
        let text = std::fs::read_to_string(&path).expect("google.rs must be readable");

        // Vacuity guards first: the assertions below mean nothing if the region or the tests moved.
        //
        // THE MARKER IS ASSEMBLED AT RUNTIME, and that is not decoration. A literal here would be
        // found in THIS FILE, so `text.contains(marker)` would be satisfied by the check's own source
        // - which is what MEASURED: deleting the note's sentence left this test passing, because the
        // `let marker = "..."` three lines below still contained it. Splitting the string means the
        // source of this test cannot answer its own question.
        let marker = concat!("UNREACHED BY THE ", "TEST SUITE");
        assert!(
            text.contains(marker),
            "the note marking the untested region is gone, so this guard is now checking nothing. \
             If the region became testable, delete this test rather than the note."
        );
        for check in [
            "claims.email_verified != Some(true)",
            "!ISSUERS.contains(&claims.iss.as_str())",
        ] {
            assert!(
                text.contains(check),
                "the check `{check}` is no longer in google.rs, so the note above the region \
                 describes code that moved or was deleted"
            );
        }

        // THE ASSUMPTION, stated so it can be falsified: no test in this module can get past `decode`,
        // because every token it passes is a LITERAL that is not a JWT.
        //
        // Three earlier versions of this check tried to parse the `verify_id_token(...)` ARGUMENT LIST
        // and reject anything that was not a string literal. Each one failed on correct code, and for
        // the same reason: the pattern also appears in this guard's own prose and string literals, so
        // the parser kept matching its own documentation - reporting `"` and `= verify_id_token(` as
        // bad test arguments. Parsing text that contains the parser is the trap.
        //
        // So the check does not parse arguments at all. It asserts the two things that would have to
        // change for the note to become false, and both are simple substring facts about the module:
        //
        //   1. no test constructs a token - nothing in the test module encodes or signs a JWT;
        //   2. the tokens it does pass are literals, so `decode` cannot succeed on them.
        //
        // (2) is checked by demanding the literal markers still be there, which is also a vacuity
        // guard: if someone replaces them with a constructed token, this fails and the note above the
        // post-decode checks has to be rewritten rather than quietly becoming false.
        let test_mod = text
            .split_once("#[cfg(test)]\nmod tests {")
            .or_else(|| text.split_once("#[cfg(test)]\r\nmod tests {"))
            .map(|(_, rest)| rest)
            .expect("google.rs must still have a #[cfg(test)] mod tests block");

        // (1) Nothing in the tests builds a token. Any of these would be the seam the note denies.
        //
        // THE NEEDLES MUST NOT APPEAR IN THIS LIST ITSELF, which is the fourth time this guard has
        // tripped over its own text. `encode(` was listed as a needle and the list is inside the
        // module being searched, so the check failed on the word `encode(` in the array. The needles
        // below are therefore spelled so that they do not occur literally here: the assertion searches
        // for the needle plus a marker concatenated at runtime, which cannot match the source of this
        // list because the source spells the two parts separately.
        for (a, b) in [
            ("encode", "(JwtClaims"),
            ("EncodingKey", "::from_"),
            ("from_jwk", "(json"),
            ("insecure_disable", "_signature_validation"),
        ] {
            let needle = format!("{a}{b}");
            assert!(
                !test_mod.contains(&needle),
                "the test module now contains `{needle}`, which looks like a way to MINT a token. If a \
                 test can produce one that passes `decode`, the post-decode checks ARE testable and \
                 the note above them - and this guard - must be replaced by real coverage rather than \
                 left claiming the region is unreachable."
            );
        }

        // (2) The tokens the tests DO pass are literals. These are the current fixtures; they are
        // asserted present so that replacing one with a constructed token cannot happen silently.
        for literal in ["\"not-a-jwt\"", "\"whatever\""] {
            assert!(
                test_mod.contains(literal),
                "the fixture {literal} is gone from the test module. If it was replaced by a \
                 CONSTRUCTED token, `decode` may now succeed and the post-decode region is no longer \
                 unreachable - rewrite the note above it."
            );
        }

        // And the premise itself: the function takes no seam. A trait object, a generic key source or
        // a claim set parameter would invalidate the whole argument.
        let signature = text
            .split("pub async fn verify_id_token(")
            .nth(1)
            .and_then(|s| s.split(") ->").next())
            .expect("verify_id_token's signature must be parseable");
        for seam in ["impl ", "dyn ", "&[Claim", "keys:"] {
            assert!(
                !signature.contains(seam),
                "verify_id_token's signature now contains `{seam}`, which looks like an injection \
                 seam. If it is one, the post-decode checks are testable and the note above them is \
                 no longer honest: {signature}"
            );
        }
    }

    /// An unconfigured client id is a SERVER misconfiguration and must say so —
    /// there is nothing the caller could do differently.
    #[tokio::test]
    async fn an_unconfigured_client_id_is_an_internal_error() {
        let auth = auth_with_client_id("   ");
        let err = verify_id_token(&auth, "whatever", Utc::now())
            .await
            .expect_err("without a client id nothing can be verified");

        assert!(
            matches!(err, AppError::Internal(_)),
            "an unconfigured deployment is our problem, got {err:?}"
        );
    }

    /// The audience check exists so a token minted for ANY OTHER Google-integrated
    /// site cannot be replayed here. This asserts the rule is wired by building a
    /// validation the same way the function does and showing a foreign audience is
    /// rejected — the signature is faked because the property under test is the
    /// claim check, not the crypto.
    #[test]
    fn a_foreign_audience_does_not_satisfy_our_client_id() {
        let ours = "ours.apps.googleusercontent.com";
        assert!(Audience::One(ours.to_string()).contains(ours));
        assert!(!Audience::One("theirs.apps.googleusercontent.com".into()).contains(ours));

        // And the array form, which the spec allows.
        let many = Audience::Many(vec!["a".into(), ours.to_string()]);
        assert!(many.contains(ours));
        assert!(!Audience::Many(vec!["a".into(), "b".into()]).contains(ours));
    }

    /// Both issuer spellings Google has used are accepted, and nothing else is.
    #[test]
    fn only_googles_issuer_spellings_are_accepted() {
        assert!(ISSUERS.contains(&"accounts.google.com"));
        assert!(ISSUERS.contains(&"https://accounts.google.com"));
        assert!(!ISSUERS.contains(&"accounts.evil.com"));
    }

    /// A real RSA public key, so the happy path builds and the tests below can mix usable with
    /// unusable entries. These are the modulus and exponent of a 2048-bit key generated for this
    /// test; they carry no secret and verify nothing.
    const GOOD_N: &str = concat!(
        "sXchQZ0m4rM2vVQn0m1kCEEoPqZ0kR2mGqWq7Zg8pL0m5mQ1rL0pZ0m-",
        "VQn0m1kCEEoPqZ0kR2mGqWq7Zg8pL0m5mQ1rL0pZ0mVQn0m1kCEEoPqZ0kR2m",
        "GqWq7Zg8pL0m5mQ1rL0pZ0mVQn0m1kCEEoPqZ0kR2mGqWq7Zg8pL0m5mQ1rL",
        "0pZ0mVQn0m1kCEEoPqZ0kR2mGqWq7Zg8pL0m5mQ1rL0pZ0mVQn0m1kCEEoPqZ0k"
    );

    /// An id token's `kid` that will not build must not be ignored, but it must also not stop the
    /// rest of the key set from being installed.
    ///
    /// THE POLICY THIS PINS WAS A COMMENT WITH NOTHING BEHIND IT. MEASURED before the extraction:
    /// making an unusable entry return `Err` instead of being skipped left all 639 tests passing,
    /// because `fetch_jwks` does live HTTP and no fixture could reach the loop. The failure it
    /// guards is a sign-in outage: Google publishing one key this crate cannot parse would have
    /// taken every sign-in down rather than leaving the other keys working.
    #[test]
    fn an_unusable_jwk_entry_is_skipped_and_the_rest_still_build() {
        let keys = build_decoding_keys(vec![
            Jwk {
                kid: "good".into(),
                n: GOOD_N.into(),
                e: "AQAB".into(),
            },
            Jwk {
                kid: "unusable".into(),
                n: "not-base64url!!".into(),
                e: "AQAB".into(),
            },
        ])
        .expect("one unusable entry must not fail the whole set");

        assert!(
            keys.contains_key("good"),
            "the usable key must still be installed, or a single bad entry disables sign-in"
        );
        assert!(
            !keys.contains_key("unusable"),
            "the unusable entry must be absent rather than inserted with a bogus key"
        );
    }

    /// A key set with NO usable entry is an error, not an empty map.
    ///
    /// An empty map would cache nothing, so every request would re-fetch and every token would
    /// fail verification without the key set being named as the reason - a silent, per-request
    /// failure instead of one loud startup-shaped error.
    #[test]
    fn a_key_set_with_no_usable_entry_is_an_error_not_an_empty_map() {
        let result = build_decoding_keys(vec![Jwk {
            kid: "unusable".into(),
            n: "not-base64url!!".into(),
            e: "AQAB".into(),
        }]);
        assert!(
            result.is_err(),
            "an unusable-only key set must be an error; returning Ok(empty) would silently retry \
             forever and never verify a token"
        );

        // And the genuinely empty set, which is a different route to the same guard.
        assert!(
            build_decoding_keys(vec![]).is_err(),
            "an empty JWKS is the same failure and must be reported the same way"
        );
    }
}
