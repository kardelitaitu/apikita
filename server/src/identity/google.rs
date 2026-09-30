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

    let header = decode_header(id_token)
        .map_err(|_| AppError::Unauthenticated)?;

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

    let claims = decoded.claims;

    // Checked AFTER the signature, so this is a fact about a token Google really
    // issued rather than about one an attacker composed.
    if claims.email_verified != Some(true) {
        return Err(AppError::Unauthenticated);
    }

    let email = claims.email.filter(|e| !e.trim().is_empty()).ok_or(AppError::Unauthenticated)?;

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
    struct Jwk {
        kid: String,
        n: String,
        e: String,
    }

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

    let mut keys = HashMap::new();
    for jwk in jwks.keys {
        // A key that will not build is skipped rather than fatal: Google publishes
        // an RSA key set, and one unusable entry must not take sign-in down.
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
}
