use std::sync::OnceLock;

use axum::{
    extract::{ConnectInfo, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Json},
};
use axum_extra::extract::cookie::{Cookie, SameSite};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};
use tracing::{error, warn};
use uuid::fmt::Hyphenated;
use uuid::Uuid;

use crate::auth_attempts;
use crate::config::{AppConfig, AuthConfig, EmailConfig, LimitsConfig, SessionsConfig};
use crate::error::AppError;
use crate::identity;
use crate::ip_tracking::resolve_client_ip;
use crate::routes::proxy::AppState;
use crate::routes::{session_token_from_cookie_header, SESSION_COOKIE};

/// SHA-256 hex of a session token, the value the sessions row stores.
/// The account a request's session cookie resolves to, against SQLite.
///
/// # This is a faithful mirror of production, and it is checked as one
///
/// The shared resolver in `crate::routes` takes a `SqlitePool` and works for the
/// handlers; this test-side copy exists because the auth tests below drive the
/// handler AND the resolver against the same pool.
///
/// **IT USED TO BE A DIFFERENT RULE, and this comment said it was the same one.**
/// The doc here claimed "Both are the same three lines, so a divergence would
/// fail a test rather than pass silently" - while this copy filtered only on
/// `revoked_at IS NULL AND expires_at > ?` and production applied the idle half
/// too, through `session_is_live_at`, and refreshed `last_seen_at`. So the
/// divergence the comment promised a test would catch was, for the idle rule,
/// invisible to every test in this file: the two disagreed and no assertion
/// compared them. A comment asserting a property is not the property.
///
/// It now calls the SAME `session_is_live_at` the production resolver calls, with
/// the same config, so the rule is shared rather than transcribed. The
/// `last_seen_at` refresh below is the other half of that parity.
#[cfg(test)]
async fn resolve_account_from_cookie(
    pool: &SqlitePool,
    headers: &HeaderMap,
) -> Result<Uuid, AppError> {
    let cookie_hdr = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .ok_or(AppError::Unauthenticated)?;

    let token = session_token_from_cookie_header(cookie_hdr).ok_or(AppError::Unauthenticated)?;

    let now = chrono::Utc::now();
    let sessions = sessions_config()?;
    let idle_days = sessions.idle_days as i64;
    let absolute_days = sessions.absolute_days as i64;

    // THREE CLAUSES, TWO INDEPENDENT CHECKS, and the redundancy is deliberate rather than untested.
    //
    // The `expires_at > ?` predicate here is checked AGAIN in this same function, by
    // `session_is_live_at` at the call below. MEASURED: deleting the SQL clause alone leaves the
    // library suite at 664 passed / 0 failed, while deleting the Rust check is caught by 2 tests - and
    // forcing it to fire always is caught by 100. A mutation sweep will therefore find the SQL copy
    // "untested" and the Rust copy guarded, which is the correct reading: the rule lives in Rust, and
    // the query predicate keeps the common case from doing the work.
    //
    // The `revoked_at IS NULL` clause is NOT duplicated in Rust - `session_is_live_at` takes no
    // revocation parameter - and deleting it IS caught (2 tests). So the two clauses have different
    // standing, which is why they are called out separately rather than as "the WHERE clause".
    //
    // THIS QUERY APPEARS THREE TIMES - here, in `logout_all` below, and in
    // `crate::routes::resolve_account_from_cookie` - and the three DO NOT have the same standing.
    // All three are followed by a `session_is_live_at` call, so the `expires_at` half of the reading
    // above applies to all three. The `revoked_at IS NULL` half does NOT, and the difference is
    // measured rather than assumed:
    //
    //   this copy (the test-side resolver)  deleting `revoked_at IS NULL` -> CAUGHT (2 tests)
    //   crate::routes::resolve_account_...  deleting it                   -> CAUGHT (6 tests)
    //   logout_all (auth.rs)                deleting it                   -> 664 passed / 0 failed
    //
    // So `logout_all`'s copy is the one nothing guards, and the clause analysis above - written once
    // "because the duplication means the same reading applies to both" - was the reason nobody
    // looked. MEASURED, then corrected rather than left as an assumption:
    //
    //   A REVOKED cookie whose timestamps are still in the FUTURE passes every Rust-side check,
    //   because `session_is_live_at` takes no revocation parameter. With this clause gone from
    //   `logout_all`, such a cookie reaches the global revoke and signs every other device on the
    //   account out. That is the same denial-of-service the doc-comment on `logout_all` describes
    //   for the idle half, arriving through the half that IS in its SQL.
    //
    // The neighbouring tests cover an EXPIRED cookie (`logout_all_ignores_dead_sessions`), which
    // fails on `expires_at` regardless of this clause - which is why they pass either way. What is
    // missing is the case this clause exists for: revoked, not yet expired.
    let session = sqlx::query(
        "SELECT account_id, last_seen_at, expires_at FROM sessions \
         WHERE token_hash = ? AND revoked_at IS NULL AND expires_at > ?",
    )
    .bind(hash_token(token))
    .bind(now)
    .fetch_optional(pool)
    .await?;

    let Some(session) = session else {
        return Err(AppError::Unauthenticated);
    };

    let account_id: Uuid = session
        .try_get::<Hyphenated, _>("account_id")
        .map_err(|_| AppError::Unauthenticated)?
        .into_uuid();
    let last_seen_at: chrono::DateTime<chrono::Utc> = session
        .try_get("last_seen_at")
        .map_err(|_| AppError::Unauthenticated)?;
    let expires_at: chrono::DateTime<chrono::Utc> = session
        .try_get("expires_at")
        .map_err(|_| AppError::Unauthenticated)?;

    if !crate::routes::session_is_live_at(now, last_seen_at, expires_at, idle_days, absolute_days) {
        return Err(AppError::Unauthenticated);
    }

    // A refusal above must NOT extend the session, so this write sits after the
    // liveness check - the same ordering, and the same guard, as production.
    sqlx::query("UPDATE sessions SET last_seen_at = ? WHERE token_hash = ? AND last_seen_at < ?")
        .bind(now)
        .bind(hash_token(token))
        .bind(now)
        .execute(pool)
        .await?;

    Ok(account_id)
}

fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// The session body the native sign-in endpoints return.
///
/// It is EMPTY, and that is a correction rather than an oversight. It used to
/// carry `account_id` and `balance_idr`, on the theory that "the website's client
/// reads this". It did not: `website/src/lib/auth-api.ts` declared the matching
/// `SessionResult`, passed it as the type argument to `postAuth`, and then every
/// caller in `website/src/pages/login.astro` threw the value away —
/// `.then(googleSignIn).then(() => window.location.assign(destination))`. Nothing
/// read either field, on either side, since the response was written.
///
/// The contract the endpoints actually have is the `Set-Cookie: session=...`
/// beside this body. That is what signs the caller in, and it is HttpOnly, so the
/// body was never the credential. `account_id` is not a fact the caller needs to
/// be told — it can ask `GET /api/me`. `balance_idr` was the more expensive
/// mistake: it made the login path run a wallet SELECT whose result no client
/// consumed, and the balance has a first-class delivery path that clients DO
/// read, the SSE `balance` event
/// (`server/src/routes/events.rs:455` -> `website/src/lib/live.ts:158`).
///
/// A field that is published but unread is not harmless: it is a second
/// definition of a fact that can drift from the one in use, and the next reader
/// of `docs/server/api-spec.md` writes a client against it. Making the body empty
/// means there is nothing there to drift.
#[derive(Debug, Serialize)]
pub struct AuthSessionResponse {}

#[derive(Debug, Deserialize)]
pub struct SignupRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct GoogleSignInRequest {
    pub id_token: String,
}

/// A single-use token, from the link in a mail.
#[derive(Debug, Deserialize)]
pub struct TokenRequest {
    pub token: String,
}

#[derive(Debug, Deserialize)]
pub struct PasswordResetRequest {
    pub token: String,
    pub email: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct EmailRequest {
    pub email: String,
}

/// `POST /auth/password-change`. The current password is required; see the handler.
#[derive(Debug, Deserialize)]
pub struct PasswordChangeRequest {
    pub current_password: String,
    pub new_password: String,
}

/// `GET /auth/providers` — the providers this account can sign in with.
///
/// A LIST rather than two booleans (`google`, `password`): the two providers that
/// exist today are not the set that will exist, and a boolean per provider turns
/// every addition into a schema change the client has to learn.
#[derive(Debug, Serialize)]
pub struct ProvidersResponse {
    pub providers: Vec<String>,
}

// ---------------------------------------------------------------------------
// Session config
// ---------------------------------------------------------------------------

static SESSIONS_CONFIG: OnceLock<SessionsConfig> = OnceLock::new();

/// SessionsConfig is not part of the router state (State<SqlitePool>), so the config
/// file is read once per process and cached. Same resolution order as main.rs:
/// APIKITA_CONFIG_PATH, then config/, then ../config/.
pub(crate) fn sessions_config() -> Result<&'static SessionsConfig, AppError> {
    if let Some(config) = SESSIONS_CONFIG.get() {
        return Ok(config);
    }

    let path =
        std::env::var("APIKITA_CONFIG_PATH").unwrap_or_else(|_| "config/apikita.toml".into());
    let loaded = AppConfig::load_from_file(&path)
        .or_else(|_| AppConfig::load_from_file("../config/apikita.toml"))
        .map_err(|e| AppError::Internal(format!("failed to load session config: {e}")))?;

    Ok(SESSIONS_CONFIG.get_or_init(|| loaded.sessions))
}

static LIMITS_CONFIG: OnceLock<LimitsConfig> = OnceLock::new();

/// A test-only escape hatch, checked before the cache.
///
/// The cap is what these tests are ABOUT, and `config/apikita.toml` ships
/// `login_per_hour_per_ip = 20` - a number no test can reach without twenty round
/// trips, and one that would leave the throttle path itself untested. The
/// alternative was to reset the `OnceLock` above, which needs `unsafe` and this
/// crate is `#![forbid(unsafe_code)]` - `forbid` rather than `deny` precisely so
/// that no single line can lift it (see `lib.rs`).
///
/// So the seam is here instead: a plain `Mutex<Option<LimitsConfig>>` that the
/// accessor consults FIRST. Production never writes it, so it stays `None` and the
/// accessor behaves exactly as `sessions_config` does. It is a `Mutex` rather than
/// another `OnceLock` because a test has to be able to put it back.
///
/// WHO CLEARS IT: `TestLimitsGuard`, which only test code can construct. It is a
/// guard rather than a plain setter because the window has to cover the WHOLE
/// test, not one request - and it has to close on the assertion-panic path too,
/// or a failing test would leave its cap behind for every test that ran after it.
///
/// Not a `#[cfg(test)]` item: the accessor below reads it on every path, and a
/// field that exists only in test builds would need a second copy of the accessor.
static TEST_LIMITS_OVERRIDE: std::sync::Mutex<Option<LimitsConfig>> = std::sync::Mutex::new(None);

/// Same shape as `sessions_config`, and for the same reason: `[limits]` is not
/// part of the router state, so the file is read once per process and cached.
///
/// A SECOND cache rather than one `AppConfig` for both, because each helper
/// caches only the section it hands out. Caching the whole config and returning
/// a field would make the two share a lifetime, and a section that is never
/// asked for would then be loaded anyway - which is how a config file gets a
/// key nobody reads without anyone noticing.
pub(crate) fn limits_config() -> Result<&'static LimitsConfig, AppError> {
    // READ, not taken: a test that wants to observe a cap being exhausted needs
    // the cap to hold for EVERY request it sends, not just the first. Taking it
    // meant the override covered exactly one call, so a test that sent three
    // requests against a cap of two was really testing a cap of two followed by
    // two calls against the shipped twenty - and it passed or failed for reasons
    // that had nothing to do with the cap. A guard handle (`TestLimitsGuard`)
    // clears it instead, on the success path and on a panic alike.
    let overridden = TEST_LIMITS_OVERRIDE
        .lock()
        .expect("the test override lock is never poisoned")
        .clone();
    if let Some(config) = overridden {
        return Ok(Box::leak(Box::new(config)));
    }

    if let Some(config) = LIMITS_CONFIG.get() {
        return Ok(config);
    }

    let path =
        std::env::var("APIKITA_CONFIG_PATH").unwrap_or_else(|_| "config/apikita.toml".into());
    let loaded = AppConfig::load_from_file(&path)
        .or_else(|_| AppConfig::load_from_file("../config/apikita.toml"))
        .map_err(|e| AppError::Internal(format!("failed to load limits config: {e}")))?;

    Ok(LIMITS_CONFIG.get_or_init(|| loaded.limits))
}

// ---------------------------------------------------------------------------
// Cookie helpers
// ---------------------------------------------------------------------------

/// The opaque session cookie. Attributes per docs/server/api-spec.md:
/// HttpOnly; Secure; SameSite=Lax.
///
/// Returns a `Result` rather than panicking on a parse failure. The value was
/// `.unwrap()`d, which meant a malformed header would panic INSIDE the login
/// handler — a 500 on the endpoint every customer uses to sign in, and one that
/// would be reported as an application crash rather than as the malformed cookie
/// it is. The parse cannot fail for a token this function generates, but the
/// failure is now a returned error the caller can answer honestly.
pub(crate) fn session_cookie(value: String, max_age_days: i64) -> Result<HeaderMap, AppError> {
    let cookie = Cookie::build((SESSION_COOKIE, value))
        .path("/")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::days(max_age_days))
        .build();

    let header_value = cookie
        .to_string()
        .parse()
        .map_err(|_| AppError::Internal("session cookie is not a valid header value".into()))?;

    let mut headers = HeaderMap::new();
    headers.insert(header::SET_COOKIE, header_value);
    Ok(headers)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------
// Native identity handlers
// ---------------------------------------------------------------------------

/// Shared per-request facts the credential endpoints all need.
struct AttemptContext {
    /// The salted hash of the client address, for the per-IP budget.
    client_key: String,
    now: DateTime<Utc>,
}

/// Resolve the attempt context ONCE per request.
///
/// The address is resolved and the salt read at this moment, because the key is
/// derived for a specific DAY: a key computed after an `await` could be the hash
/// of a different day's salt than the rows it is compared against, which would
/// silently split one attacker's attempts across two counters.
fn attempt_context(
    state: &AppState,
    peer: std::net::SocketAddr,
    headers: &HeaderMap,
) -> AttemptContext {
    let ip = resolve_client_ip(peer.ip(), headers, &state.trusted_proxies);
    AttemptContext {
        client_key: auth_attempts::ip_key(
            ip,
            &state.ip_salt.salt_for_day(crate::ip_tracking::today_utc()),
        ),
        now: Utc::now(),
    }
}

/// Build the `[auth]` and `[email]` config for this request's process.
fn auth_config() -> Result<&'static AuthConfig, AppError> {
    Ok(&app_config()?.auth)
}

/// The `[auth]` section, for tests in OTHER route modules that need to hash a
/// password with the shipped Argon2 parameters.
///
/// `pub` rather than `pub(crate)` for the same reason the handlers are: the
/// admin tests create a real password identity through the production helper and
/// then log in with it, and a hash written under a second, test-only parameter
/// set would not be the hash the verifier has to accept. Returning a clone keeps
/// the `OnceLock` cache private to this module.
#[cfg(test)]
pub fn auth_config_for_tests() -> Result<AuthConfig, AppError> {
    Ok(auth_config()?.clone())
}

fn email_config() -> Result<&'static EmailConfig, AppError> {
    Ok(&app_config()?.email)
}

/// The whole config, loaded once and cached.
///
/// `sessions_config` and `limits_config` below predate this and keep their own
/// caches; they are left alone because each hands out one section and a caller
/// that asks for `[limits]` should not force `[email]` and its environment
/// lookups to be read. This one exists for the identity endpoints, which need
/// `[auth]` and `[email]` together on every request.
static APP_CONFIG: OnceLock<AppConfig> = OnceLock::new();

fn app_config() -> Result<&'static AppConfig, AppError> {
    if let Some(config) = APP_CONFIG.get() {
        return Ok(config);
    }

    let path =
        std::env::var("APIKITA_CONFIG_PATH").unwrap_or_else(|_| "config/apikita.toml".into());
    let loaded = AppConfig::load_from_file(&path)
        .or_else(|_| AppConfig::load_from_file("../config/apikita.toml"))
        .map_err(|e| AppError::Internal(format!("failed to load config: {e}")))?;

    Ok(APP_CONFIG.get_or_init(|| loaded))
}

/// The URL a verification or reset link points at.
///
/// Env, not config, for the reason `identity::email::EMAIL_BASE_URL_ENV` gives:
/// the deployment's public origin is a property of where it runs, and a test
/// needs to point links at a local listener without rebuilding.
///
/// A RESET LINK ALSO CARRIES THE ADDRESS, because `/auth/password-reset/confirm`
/// requires the token and the address to agree and the two may be minutes apart in
/// two different browsers. The address is not a secret — it is the one the mail was
/// sent to — and carrying it is what lets that page submit without asking the user
/// to retype what they just typed. A verification link has no such requirement, so
/// it carries only the token.
fn mail_link(purpose: identity::tokens::Purpose, raw: &str, email: &str) -> String {
    let base = std::env::var("APIKITA_PUBLIC_URL")
        .unwrap_or_else(|_| "https://apikita.example".to_string());
    let base = base.trim_end_matches('/');
    match purpose {
        identity::tokens::Purpose::Verification => format!("{base}/verify?token={raw}"),
        // Percent-encoding a whole address is the safe form: a `+`, `&` or `#` in
        // a local part would otherwise truncate the query string or split it into
        // extra parameters, and `%40` is what the page decodes back to `@`.
        identity::tokens::Purpose::Reset => {
            format!(
                "{base}/reset/confirm?token={raw}&email={}",
                urlencode(email)
            )
        }
    }
}

/// Percent-encode a query-string value with the RFC 3986 unreserved set.
///
/// Hand-rolled rather than pulled from a crate because this is the only place the
/// crate needs it, and the set is short enough to be obviously correct: everything
/// that is not `A-Za-z0-9-._~` becomes `%XX` over the UTF-8 bytes.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(byte));
            }
            other => {
                out.push('%');
                // Uppercase hex, the form every decoder accepts. `%` then `{:02X}`
                // rather than a lookup into a hex table: the table form needs an
                // INDEX, and indexing is denied crate-wide (lib.rs:115) because a
                // slice index is a panic path. `{:02X}` of a `u8` is total - it
                // cannot be out of range for the type it formats.
                out.push_str(&format!("{other:02X}"));
            }
        }
    }
    out
}

/// The neutral reply every signup-shaped branch returns.
///
/// ONE constant, used by the created branch, the already-registered branch and
/// the failed-mail branch, because the only thing that makes the enumeration
/// defence real is that there is no code path that could word it differently.
const NEUTRAL_SIGNUP_REPLY: &str =
    "If that address can be registered, a confirmation link is on its way.";

const NEUTRAL_RESET_REPLY: &str = "If that address has an account, a reset link is on its way.";

/// Render the bodies for the two link-bearing mails.
fn verification_body(link: &str) -> String {
    format!(
        "Confirm your apikita address by opening this link:\n\n{link}\n\n\
         If you did not sign up, ignore this message: the account stays unverified \
         and cannot hold a balance."
    )
}

fn reset_body(link: &str) -> String {
    format!(
        "Choose a new apikita password by opening this link:\n\n{link}\n\n\
         If you did not ask for this, ignore it. Nothing changes until the link is used."
    )
}

/// Send a link mail, logging the failure without failing the request.
///
/// A MISSING RELAY IS NOT AN ERROR HERE, and neither is a relay that refuses the
/// message. Two reasons, and both are about not letting mail become a dependency
/// of an account:
///
/// - The account is inert without confirmation. An unverified account cannot hold
///   a balance, so a mail failure costs the user a retry, not their data.
/// - An error shaped like "we could not mail THAT address" is an enumeration
///   oracle the moment it is distinguishable from the neutral reply. Collapsing
///   it into the same reply is what keeps the reply meaningful.
///
/// The account id is logged so an operator can find the account that never got
/// its link; the address is not, because the log is not the place for it.
/// Builds the message a verification or reset flow sends.
///
/// A PURE FUNCTION, split out of [`send_link_mail`] so a test can assert on the
/// message THIS ROUTE BUILDS rather than on one the test builds itself. That
/// distinction is the entire point: the flow was dead for as long as it existed
/// because the recipient was `String::new()`, and every test of the mailer
/// supplied its own address, so the module was covered and the only place in the
/// program that assembles a message for a real customer was not. A test that
/// constructs its own `Email` cannot fail when this function does.
///
/// The address is taken as `&str` and passed in already normalized. It is
/// deliberately not logged anywhere downstream - see the comment below.
fn link_mail(
    purpose: identity::tokens::Purpose,
    raw_token: &str,
    email: &str,
) -> identity::email::Email {
    let link = mail_link(purpose, raw_token, email);
    let (subject, body) = match purpose {
        identity::tokens::Purpose::Verification => {
            ("Confirm your apikita address", verification_body(&link))
        }
        identity::tokens::Purpose::Reset => ("Reset your apikita password", reset_body(&link)),
    };

    // THE RECIPIENT IS THE WHOLE POINT OF THE PARAMETER, AND IT WAS EMPTY.
    // This read `to: String::new()` from the commit that introduced the native
    // identity flows and never worked: `EmailSender::send` parses the field as a
    // `Mailbox` (identity/email.rs), an empty string does not parse, and the
    // builder returns `EmailError::Build` before anything is dialled. Every call
    // logged "a verification or reset link could not be sent" against an account
    // id and no address, so the failure looked like a relay problem - and no test
    // saw it, because every test in email.rs builds its own `Email` with a real
    // address rather than going through the route that builds this one.
    identity::email::Email {
        to: email.to_string(),
        subject: subject.to_string(),
        body,
    }
}

/// Build the message, then hand the SEND to a detached task.
///
/// **THE WAIT IS THE ORACLE.** Signup, `request_password_reset` and `resend_verification` all answer
/// the same neutral reply whether or not the address has an account, and all three call this only on
/// the branch where the account EXISTS - a new account, a known address, a known unverified identity.
/// Awaiting the SMTP conversation here therefore charged a REGISTERED address a full relay round trip
/// and charged an unregistered one nothing, and the reply bodies matched the whole time.
///
/// MEASURED against a listener that accepts and never sends a banner: the send side took ~1001ms
/// against ~0.1ms for the path that skips it. The relay timeout has a 10s floor
/// (`identity::email::MIN_TIMEOUT_SECONDS`), so the gap is bounded by the RELAY, not by this client -
/// and it is three orders of magnitude larger than the Argon2 timing difference the hash-ordering
/// comments in `signup` and `login` were written to close. Nothing in the code or the docs recorded
/// this one.
///
/// Spawning is the fix that matches the shape the rest of this file already uses
/// (`ReservationGuard::drop` releases through `tokio::spawn` for the same reason: the request path
/// must not wait on something whose outcome the caller cannot use). The caller has nothing to do
/// with the result - the reply is neutral either way - so waiting bought no behaviour, only a
/// measurement channel.
///
/// WHAT IS NOT SPAWNED: building the message. `link_mail` is a pure function whose output a test
/// asserts on directly, and the token is issued by the CALLER on the request path, so the link the
/// customer eventually clicks is committed before this is called. Only the transport wait moves.
///
/// **WHAT THE DETACH MAKES WORSE, stated because it is a real cost and this comment is the only
/// place it is written down.** An awaited send was tracked by the request: if the process went away
/// mid-send, so did the thing that was waiting on it. A detached task is not tracked by anything.
/// `main.rs` calls `axum::serve` with no shutdown signal and the process runs under `tini`, so a
/// SIGTERM drops the runtime rather than draining it - there is no graceful drain in this service
/// and no task tracker to add one to (`JoinSet` appears nowhere outside two test stubs). A send
/// caught in the window between the response and the relay therefore vanishes with no row, no log
/// and no retry: the customer is told to check their inbox and nothing is coming.
///
/// The window is small and the recovery is cheap and already built - the link expired with the
/// process, so `resend_verification` and `request_password_reset` are exactly the paths for it, and
/// both are this same function. That is why the trade is worth making: the alternative is a request
/// path that leaks whether an address is registered. But "small" is not "closed", and a shutdown
/// during a burst of signups is the shape where it would be felt. Writing it here rather than
/// leaving it implied is the point - the next reader gets to weigh it with the number attached.
///
/// The `state` parameter was REMOVED here. It was already unused before this change (the previous
/// body carried `let _ = state;` to silence the warning) and the shorter body made that obvious.
/// `email_config` is a free function, so nothing in this path needed the app state at all.
async fn send_link_mail(
    account_id: Uuid,
    purpose: identity::tokens::Purpose,
    raw_token: &str,
    email: &str,
) {
    let sender = match email_config() {
        Ok(config) => identity::email::EmailSender::new(config),
        Err(e) => {
            error!(error = %e, "the [email] section could not be read; no mail will be sent");
            return;
        }
    };

    // Built here so a malformed address is still caught on the request path where it can be logged
    // with the request's own context, rather than surfacing as a mystery in a detached task.
    let message = link_mail(purpose, raw_token, email);

    // Fire-and-forget: the reply does not depend on the relay, and waiting is what made a
    // registered address answer on a different clock from an unregistered one. See the doc above
    // for what the detach costs on shutdown.
    tokio::spawn(async move {
        let outcome = sender.send(message).await;

        // A failure to reach the relay is reported against the ACCOUNT, never against
        // the address: this path is reached for an address that may not have an
        // account, and the log is not the place for it - which is the other half of
        // why an empty recipient survived so long.
        if let Err(e) = outcome {
            error!(
                account_id = %account_id,
                purpose = purpose.as_str(),
                error = %e,
                "a verification or reset link could not be sent"
            );
        }
    });
}

/// `POST /auth/signup` - create an account from an address and a password.
///
/// THE REPLY DOES NOT DEPEND ON WHETHER THE ADDRESS WAS ALREADY REGISTERED. That
/// is the whole of the enumeration defence: the endpoint is unauthenticated, so
/// any difference in status, body or observable timing between "created" and
/// "already exists" turns it into a membership test for the address space.
///
/// What happens on the already-registered branch is deliberately the SAME WORK,
/// not an early return: the previous credential is left alone, nothing is
/// overwritten, and the reply is byte-identical.
pub async fn signup(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    payload: Result<Json<SignupRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<impl IntoResponse, AppError> {
    let Json(payload) = payload.map_err(|_| {
        AppError::InvalidRequest("the request body is not valid JSON for this endpoint".into())
    })?;

    let ctx = attempt_context(&state, peer, &headers);
    let limits = limits_config()?;

    // The signup cap first, before any hashing: hashing is the expensive part and
    // a cap that runs after it is a cap that lets an attacker spend our CPU.
    auth_attempts::record_and_check(
        &state.pool,
        auth_attempts::Kind::Signup,
        auth_attempts::Subject::Ip(&ctx.client_key),
        limits.signup_per_hour_per_ip,
        ctx.now,
    )
    .await?;

    let email = identity::accounts::normalize_email(&payload.email);
    if email.is_empty() || !email.contains('@') {
        return Err(AppError::ValidationFailed {
            message: "that is not an email address".into(),
            field: "email".into(),
        });
    }

    let auth = auth_config()?;
    identity::password::validate_password(auth, &payload.password)?;

    // Hashing happens BEFORE the existence check so both branches pay the same
    // cost. An early return here would make "already registered" measurably
    // faster than "created", which is an enumeration oracle in the timing domain
    // even though the bodies match.
    let hash = identity::password::hash_password(auth.clone(), payload.password.clone()).await?;

    // The question is "does this address already have a PASSWORD identity", not
    // "does any identity exist for it". `account_for_email` answers the broader
    // question and is the wrong one here: it now prefers the GOOGLE identity (see
    // its doc), so a Google-only address would read as "already registered" and a
    // password signup would create nothing - leaving the caller with an address
    // they cannot register and no account they can sign in to. The unique index is
    // `UNIQUE (provider, email)`, so creating the password row here is legal and is
    // the designed outcome: an unverified password identity coexisting with a
    // Google one on the same address.
    let existing = identity::accounts::password_identity(&state.pool, &email).await?;

    if existing.is_none() {
        let issued =
            identity::accounts::create_password_account(&state.pool, &email, &hash, ctx.now)
                .await?;

        let token = identity::tokens::issue(
            &state.pool,
            issued,
            identity::tokens::Purpose::Verification,
            Duration::minutes(auth.verification_ttl_minutes as i64),
            ctx.now,
        )
        .await?;

        // The address here is the one the caller typed, which is the one the
        // account was created with after normalization.
        send_link_mail(
            issued,
            identity::tokens::Purpose::Verification,
            &token.raw,
            &email,
        )
        .await;
    }

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "message": NEUTRAL_SIGNUP_REPLY })),
    ))
}

/// `POST /auth/login` - exchange an address and password for a session.
pub async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    payload: Result<Json<LoginRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<impl IntoResponse, AppError> {
    let Json(payload) = payload.map_err(|_| {
        AppError::InvalidRequest("the request body is not valid JSON for this endpoint".into())
    })?;

    let ctx = attempt_context(&state, peer, &headers);
    let limits = limits_config()?;

    let email = identity::accounts::normalize_email(&payload.email);
    let identity_row = identity::accounts::password_identity(&state.pool, &email).await?;

    // Both counters are spent on EVERY attempt, including one for an address that
    // does not exist: the per-IP cap has to bound a guesser walking the address
    // space, and those attempts never resolve an account.
    auth_attempts::record_and_check_login(
        &state.pool,
        Some(&ctx.client_key),
        identity_row.as_ref().map(|i| i.account_id),
        limits.login_per_hour_per_ip,
        limits.login_per_hour_per_account,
        ctx.now,
    )
    .await?;

    let auth = auth_config()?;

    // A MISSING ACCOUNT STILL PAYS FOR A HASH. Returning early here would make
    // "no such address" fast and "wrong password" slow, which is a membership
    // oracle in exactly the same way the signup reply would be.
    let Some(identity_row) = identity_row else {
        // Hash against a throwaway so the cost matches; the result is discarded.
        let _ = identity::password::hash_password(auth.clone(), payload.password.clone()).await?;
        return Err(AppError::Unauthenticated);
    };

    let matches = identity::password::verify_password(
        auth.clone(),
        identity_row.password_hash.clone(),
        payload.password.clone(),
    )
    .await?;

    if !matches {
        return Err(AppError::Unauthenticated);
    }

    // A suspended account is not a credential failure, but it is refused with the
    // same 401 so the endpoint does not confirm that a credential was correct on
    // an account the operator has closed.
    let status: String = sqlx::query_scalar("SELECT status FROM accounts WHERE id = ?")
        .bind(identity_row.account_id.hyphenated())
        .fetch_one(&state.pool)
        .await?;

    if status != "active" {
        return Err(AppError::Unauthenticated);
    }

    let sessions = sessions_config()?;

    #[allow(clippy::arithmetic_side_effects)]
    let expires_at = ctx.now + Duration::days(sessions.absolute_days as i64);

    let token = format!("apk_sess_{}", Uuid::new_v4().simple());
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    sqlx::query(
        "INSERT INTO sessions (id, account_id, token_hash, expires_at, last_seen_at, user_agent, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(Uuid::new_v4().hyphenated())
    .bind(identity_row.account_id.hyphenated())
    .bind(hash_token(&token))
    .bind(expires_at)
    .bind(ctx.now)
    .bind(user_agent)
    .bind(ctx.now)
    .execute(&state.pool)
    .await?;

    Ok((
        StatusCode::OK,
        session_cookie(token, sessions.absolute_days as i64)?,
        Json(AuthSessionResponse {}),
    ))
}

/// `POST /auth/google` - sign in with a Google ID token.
pub async fn google_sign_in(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    payload: Result<Json<GoogleSignInRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<impl IntoResponse, AppError> {
    let Json(payload) = payload.map_err(|_| {
        AppError::InvalidRequest("the request body is not valid JSON for this endpoint".into())
    })?;

    let ctx = attempt_context(&state, peer, &headers);
    let limits = limits_config()?;
    let auth = auth_config()?;

    // The token is verified BEFORE any budget is spent: a forged token must not
    // cost us a network round trip to Google's JWKS endpoint.
    let user = identity::google::verify_id_token(auth, &payload.id_token, ctx.now).await?;

    auth_attempts::record_and_check(
        &state.pool,
        auth_attempts::Kind::Login,
        auth_attempts::Subject::Ip(&ctx.client_key),
        limits.login_per_hour_per_ip,
        ctx.now,
    )
    .await?;

    let outcome = identity::accounts::resolve_google_sign_in(
        &state.pool,
        &user.subject,
        &user.email,
        ctx.now,
    )
    .await?;

    let account_id = match outcome {
        identity::accounts::GoogleSignIn::Existing(id)
        | identity::accounts::GoogleSignIn::Created(id) => id,
        identity::accounts::GoogleSignIn::Linked { account_id, .. } => account_id,
        identity::accounts::GoogleSignIn::CollisionCreated {
            account_id,
            colliding_identity_id,
        } => {
            // The address matched a password identity that was NOT verified before
            // this sign-in, so the two are kept apart and the person is warned.
            // WARN, not ERROR: this is a legitimate user hitting a defended case,
            // and an attacker hitting it repeatedly is what the cap above bounds.
            warn!(
                account_id = %account_id,
                colliding_identity_id = %colliding_identity_id,
                "auth.email_collision_unverified"
            );
            account_id
        }
    };

    let status: String = sqlx::query_scalar("SELECT status FROM accounts WHERE id = ?")
        .bind(account_id.hyphenated())
        .fetch_one(&state.pool)
        .await?;

    if status != "active" {
        return Err(AppError::Unauthenticated);
    }

    let sessions = sessions_config()?;

    #[allow(clippy::arithmetic_side_effects)]
    let expires_at = ctx.now + Duration::days(sessions.absolute_days as i64);

    let token = format!("apk_sess_{}", Uuid::new_v4().simple());
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    sqlx::query(
        "INSERT INTO sessions (id, account_id, token_hash, expires_at, last_seen_at, user_agent, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(Uuid::new_v4().hyphenated())
    .bind(account_id.hyphenated())
    .bind(hash_token(&token))
    .bind(expires_at)
    .bind(ctx.now)
    .bind(user_agent)
    .bind(ctx.now)
    .execute(&state.pool)
    .await?;

    Ok((
        StatusCode::OK,
        session_cookie(token, sessions.absolute_days as i64)?,
        Json(AuthSessionResponse {}),
    ))
}

/// `POST /auth/verify-email` - redeem a verification link.
pub async fn verify_email(
    State(state): State<AppState>,
    payload: Result<Json<TokenRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<impl IntoResponse, AppError> {
    let Json(payload) = payload.map_err(|_| {
        AppError::InvalidRequest("the request body is not valid JSON for this endpoint".into())
    })?;

    let now = Utc::now();

    // `consume` marks and checks in one statement, so a link cannot be redeemed
    // twice even by two simultaneous requests.
    let redeemed = identity::tokens::consume(
        &state.pool,
        &payload.token,
        identity::tokens::Purpose::Verification,
        now,
    )
    .await?;

    // Which identity to verify is decided by the ADDRESS on the token's account,
    // not by the token alone: the token proves control of a mailbox, and it is the
    // mailbox that is being verified.
    let identity_id: Option<String> = sqlx::query_scalar(
        "SELECT id FROM identities WHERE account_id = ? AND provider = ? LIMIT 1",
    )
    .bind(redeemed.account_id.hyphenated())
    .bind(identity::accounts::PASSWORD)
    .fetch_optional(&state.pool)
    .await?;

    let Some(identity_id) = identity_id else {
        return Err(AppError::Unauthenticated);
    };

    let identity_id = Uuid::parse_str(&identity_id)
        .map_err(|e| AppError::Internal(format!("identities.id is unreadable: {e}")))?;

    identity::accounts::mark_verified(&state.pool, identity_id, now).await?;

    // Every other token for the account goes too: the address is proven, so a
    // second link sitting in the mailbox is a credential with no purpose left.
    identity::tokens::clear_for_account(&state.pool, redeemed.account_id).await?;

    Ok(StatusCode::NO_CONTENT)
}

/// `POST /auth/password-reset/request` - mail a reset link.
///
/// Neutral reply, for the same reason signup has one.
pub async fn request_password_reset(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    payload: Result<Json<EmailRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<impl IntoResponse, AppError> {
    let Json(payload) = payload.map_err(|_| {
        AppError::InvalidRequest("the request body is not valid JSON for this endpoint".into())
    })?;

    let ctx = attempt_context(&state, peer, &headers);
    let limits = limits_config()?;
    let auth = auth_config()?;

    let email = identity::accounts::normalize_email(&payload.email);
    let account = identity::accounts::account_for_email(&state.pool, &email).await?;

    // The cap is PER ACCOUNT where there is one: the resource being protected is
    // the victim's mailbox, and a per-IP cap alone would let a distributed sender
    // flood one inbox. An unknown address spends the per-IP budget instead.
    let subject = match account {
        Some(id) => auth_attempts::Subject::Account(id),
        None => auth_attempts::Subject::Ip(&ctx.client_key),
    };

    auth_attempts::record_and_check(
        &state.pool,
        auth_attempts::Kind::PasswordReset,
        subject,
        limits.password_reset_per_hour_per_account,
        ctx.now,
    )
    .await?;

    if let Some(account_id) = account {
        let token = identity::tokens::issue(
            &state.pool,
            account_id,
            identity::tokens::Purpose::Reset,
            Duration::minutes(auth.reset_ttl_minutes as i64),
            ctx.now,
        )
        .await?;

        // The reset link carries the address so the confirmation page can submit
        // without making the user retype it; see `mail_link`.
        send_link_mail(
            account_id,
            identity::tokens::Purpose::Reset,
            &token.raw,
            &email,
        )
        .await;
    }

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "message": NEUTRAL_RESET_REPLY })),
    ))
}

/// `POST /auth/password-reset/confirm` - set a new password with a reset token.
pub async fn confirm_password_reset(
    State(state): State<AppState>,
    payload: Result<Json<PasswordResetRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<impl IntoResponse, AppError> {
    let Json(payload) = payload.map_err(|_| {
        AppError::InvalidRequest("the request body is not valid JSON for this endpoint".into())
    })?;

    let now = Utc::now();
    let auth = auth_config()?;
    identity::password::validate_password(auth, &payload.password)?;

    let redeemed = identity::tokens::consume(
        &state.pool,
        &payload.token,
        identity::tokens::Purpose::Reset,
        now,
    )
    .await?;

    let hash = identity::password::hash_password(auth.clone(), payload.password.clone()).await?;

    // Vector 4: the reset may land on an account with no password identity - a
    // Google-only signup, where the person has never set a password. Creating the
    // identity here is the point of the decision: completing a reset already
    // required control of the mailbox, so this grants nothing that was not already
    // proven, and the alternative is a support ticket.
    let identity_row = identity::accounts::password_identity(&state.pool, &payload.email).await?;

    match identity_row {
        Some(existing) if existing.account_id == redeemed.account_id => {
            identity::accounts::set_password(&state.pool, existing.identity_id, &hash, now).await?;
        }
        Some(_) => {
            // The token authorises one account and the body names an address on a
            // different one. There is no correct merge, so nothing happens.
            return Err(AppError::Unauthenticated);
        }
        None => {
            identity::accounts::upsert_password_identity(
                &state.pool,
                redeemed.account_id,
                &payload.email,
                &hash,
                // NOT verified by a reset. A reset proves control of the mailbox,
                // but R2 says the `email_verified` transition is a separate claim;
                // marking it here would let a reset silently upgrade an address
                // that was never confirmed.
                false,
                now,
            )
            .await?;
        }
    }

    // EVERY session dies with the password. A reset is what a person does when
    // they believe someone else has their credential, so leaving a session that
    // was opened with the old password alive would defeat the transaction.
    sqlx::query("UPDATE sessions SET revoked_at = ? WHERE account_id = ? AND revoked_at IS NULL")
        .bind(now)
        .bind(redeemed.account_id.hyphenated())
        .execute(&state.pool)
        .await?;

    identity::tokens::clear_for_account(&state.pool, redeemed.account_id).await?;

    Ok(StatusCode::NO_CONTENT)
}

/// `POST /auth/verification/resend` - mail a fresh verification link.
pub async fn resend_verification(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    payload: Result<Json<EmailRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<impl IntoResponse, AppError> {
    let Json(payload) = payload.map_err(|_| {
        AppError::InvalidRequest("the request body is not valid JSON for this endpoint".into())
    })?;

    let ctx = attempt_context(&state, peer, &headers);
    let limits = limits_config()?;
    let auth = auth_config()?;

    auth_attempts::record_and_check(
        &state.pool,
        auth_attempts::Kind::VerificationResend,
        auth_attempts::Subject::Ip(&ctx.client_key),
        limits.verification_resend_per_hour,
        ctx.now,
    )
    .await?;

    let email = identity::accounts::normalize_email(&payload.email);
    let identity = identity::accounts::password_identity(&state.pool, &email).await?;

    // Only an account that EXISTS and is still UNVERIFIED gets a mail. A verified
    // address asking again is not an error and does not get a link: a working
    // verification link for a proven address is a credential with nothing to do.
    if let Some(identity) = identity {
        if !identity.email_verified {
            let token = identity::tokens::issue(
                &state.pool,
                identity.account_id,
                identity::tokens::Purpose::Verification,
                Duration::minutes(auth.verification_ttl_minutes as i64),
                ctx.now,
            )
            .await?;

            send_link_mail(
                identity.account_id,
                identity::tokens::Purpose::Verification,
                &token.raw,
                &email,
            )
            .await;
        }
    }

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "message": NEUTRAL_SIGNUP_REPLY })),
    ))
}

/// Revoke the current session row. A failed revoke is reported rather than
/// swallowed: the cookie is only cleared once the row is actually dead.
pub async fn logout(
    State(pool): State<SqlitePool>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    if let Some(token) = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(session_token_from_cookie_header)
    {
        sqlx::query(
            "UPDATE sessions SET revoked_at = ? WHERE token_hash = ? AND revoked_at IS NULL",
        )
        .bind(Utc::now())
        .bind(hash_token(token))
        .execute(&pool)
        .await?;
    }

    Ok((StatusCode::NO_CONTENT, session_cookie(String::new(), 0)?))
}

/// Revoke every live session for the account - other devices are logged out
/// immediately (docs/server/api-spec.md, auth).
///
/// # The caller's own session is judged by the WHOLE rule, not by half of it
///
/// This endpoint is a destructive act on other devices, so the credential that
/// authorises it has to be one the rest of the server would also honour. The
/// lookup below used to filter on `revoked_at IS NULL AND expires_at > ?` only -
/// the two halves that are properties of the row - and omitted the idle half.
/// Both the production resolver (`crate::routes::resolve_account_from_cookie`)
/// and `session_is_live_at` apply all three, so an abandoned-but-unexpired
/// session was refused everywhere else and still accepted HERE.
///
/// The consequence is narrow but real, and it is the direction that matters: a
/// cookie that no longer authenticates anything could still sign every other
/// device on the account out. That is a denial of service against a customer, by
/// anyone holding a token they can no longer use - and it needs no password, no
/// second factor and no live session, only a stale cookie and the ability to
/// POST. The `expires_at` half has a test (`logout_all_ignores_dead_sessions`);
/// the idle half was never part of the predicate at all.
///
/// The idle comparison is done in Rust by `session_is_live_at`, NOT in SQL, for
/// the reason `crate::routes` gives for doing it that way in the resolver: the
/// rule this crate ships is then the rule its tests exercise, and the two callers
/// cannot drift into disagreeing about what "live" means.
pub async fn logout_all(
    State(pool): State<SqlitePool>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    if let Some(token) = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(session_token_from_cookie_header)
    {
        let now = Utc::now();
        let sessions = sessions_config()?;
        let idle_days = sessions.idle_days as i64;
        let absolute_days = sessions.absolute_days as i64;

        let session = sqlx::query(
            "SELECT account_id, last_seen_at, expires_at FROM sessions \
             WHERE token_hash = ? AND revoked_at IS NULL AND expires_at > ?",
        )
        .bind(hash_token(token))
        .bind(now)
        .fetch_optional(&pool)
        .await?;

        // The row's own timestamps decide whether this cookie is a credential at
        // all. An unreadable row is not a reason to revoke: this endpoint is
        // destructive, so its failures fall toward doing nothing. It answers 204
        // rather than 500, matching how it already treats a cookie that resolves
        // to nothing.
        let caller = session.and_then(|s| {
            let account_id: Uuid = s.get::<Hyphenated, _>("account_id").into_uuid();
            let last_seen_at: chrono::DateTime<chrono::Utc> = s.try_get("last_seen_at").ok()?;
            let expires_at: chrono::DateTime<chrono::Utc> = s.try_get("expires_at").ok()?;
            Some((account_id, last_seen_at, expires_at))
        });

        let caller = caller.filter(|(_, last_seen_at, expires_at)| {
            crate::routes::session_is_live_at(
                now,
                *last_seen_at,
                *expires_at,
                idle_days,
                absolute_days,
            )
        });

        if let Some((account_id, _, _)) = caller {
            sqlx::query(
                "UPDATE sessions SET revoked_at = ? WHERE account_id = ? AND revoked_at IS NULL",
            )
            .bind(Utc::now())
            .bind(account_id.hyphenated())
            .execute(&pool)
            .await?;
        }
    }

    Ok((StatusCode::NO_CONTENT, session_cookie(String::new(), 0)?))
}

/// `POST /auth/password-change` - change the password of a signed-in account.
///
/// THE CURRENT PASSWORD IS REQUIRED, and that is the point of the endpoint rather
/// than an annoyance: without it a stolen session cookie would be enough to take
/// permanent ownership of the account. A cookie proves the caller can use this
/// browser now; the password proves they are the person the account belongs to.
///
/// EVERY OTHER SESSION DIES with the change, and this one too. The reasoning is the
/// reset path's, from the other direction: someone changing their password may be
/// doing it precisely because they suspect another device holds a session, and
/// leaving those alive would defeat the gesture. The caller is signed out and told
/// to sign in again rather than handed a fresh cookie, because a change is exactly
/// the moment a silent re-issue would be least welcome.
pub async fn change_password(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    payload: Result<Json<PasswordChangeRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<impl IntoResponse, AppError> {
    let Json(payload) = payload.map_err(|_| AppError::ValidationFailed {
        field: "body".into(),
        message: "expected { current_password, new_password }".into(),
    })?;

    // Authenticated FIRST, so an unauthenticated guesser does not get to spend the
    // account's rate-limit budget — the budget protects a signed-in account, and
    // there is no account to protect until the cookie resolves.
    let account_id = crate::routes::resolve_account_from_cookie(&state.pool, &headers).await?;

    let ctx = attempt_context(&state, peer, &headers);
    let limits = limits_config()?;
    auth_attempts::record_and_check(
        &state.pool,
        auth_attempts::Kind::Login,
        auth_attempts::Subject::Account(account_id),
        limits.login_per_hour_per_account,
        ctx.now,
    )
    .await?;

    let auth = auth_config()?;

    // Which identity's password is being changed: the one the session's account
    // actually has. Not taken from the body, or a caller could name an address on
    // another account.
    let identity = identity::accounts::first_password_identity(&state.pool, account_id).await?;
    let Some(identity) = identity else {
        // A Google-only account has no password to change. Telling the caller that
        // is not an enumeration leak — they are already signed in as this account.
        return Err(AppError::ValidationFailed {
            field: "current_password".into(),
            message: "this account signs in with Google and has no password to change".into(),
        });
    };

    // The current password is verified against the stored hash. A mismatch is a
    // 401 and is NOT written back as an attempt, like every other refused
    // credential check in this file.
    if !identity::password::verify_password(
        auth.clone(),
        identity.password_hash.clone(),
        payload.current_password.clone(),
    )
    .await?
    {
        return Err(AppError::Unauthenticated);
    }

    identity::password::validate_password(auth, &payload.new_password)?;
    let hash = identity::password::hash_password(auth.clone(), payload.new_password).await?;
    identity::accounts::set_password(&state.pool, identity.identity_id, &hash, ctx.now).await?;

    // Every session, INCLUDING the caller's. The cookie is cleared in the response
    // so the browser is not left holding a dead one.
    sqlx::query("UPDATE sessions SET revoked_at = ? WHERE account_id = ? AND revoked_at IS NULL")
        .bind(ctx.now)
        .bind(account_id.hyphenated())
        .execute(&state.pool)
        .await?;

    // AND every outstanding link, which is the OTHER half of the same idea. A reset
    // token lets its holder set a password without knowing the current one, so
    // leaving one alive after a password change would leave a live credential that
    // outranks the one just set - the change would revoke the sessions and not the
    // link that can replace the password again. `confirm_password_reset` already
    // clears them for the same reason; this handler did not, and the two now agree.
    //
    // This also clears VERIFICATION tokens, which `clear_for_account` does by
    // design. That is harmless here - the address is already verified for any
    // account that can sign in - and narrowing the delete by purpose would mean a
    // second query to maintain for no gain.
    identity::tokens::clear_for_account(&state.pool, account_id).await?;

    Ok((StatusCode::NO_CONTENT, session_cookie(String::new(), 0)?))
}

/// `GET /auth/providers` - which sign-in methods this account has.
///
/// Answers with the providers that have a row in `identities` for the signed-in
/// account, so the settings page can say "Linked" or "Not linked" without reading
/// anything it is not entitled to. It reports the ACCOUNT'S OWN providers and takes
/// no address or id, so it cannot be used to probe anyone else.
pub async fn list_providers(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let account_id = crate::routes::resolve_account_from_cookie(&state.pool, &headers).await?;

    let rows: Vec<String> =
        sqlx::query_scalar("SELECT DISTINCT provider FROM identities WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .fetch_all(&state.pool)
            .await?;

    Ok((StatusCode::OK, Json(ProvidersResponse { providers: rows })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_cookie_carries_expected_attributes() {
        let headers = session_cookie("apk_sess_abc".into(), 30).expect("a valid token parses");
        let value = headers
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .expect("set-cookie present");
        assert!(value.starts_with("session=apk_sess_abc"), "got {value}");
        assert!(value.contains("HttpOnly"), "got {value}");
        assert!(value.contains("Secure"), "got {value}");
        assert!(value.contains("SameSite=Lax"), "got {value}");
        assert!(value.contains("Max-Age=2592000"), "got {value}");

        // Clearing the cookie expires it immediately.
        let cleared = session_cookie(String::new(), 0).expect("a clear-cookie parses");
        let value = cleared
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .expect("set-cookie present");
        assert!(value.contains("Max-Age=0"), "got {value}");
    }

    /// The reset link must survive an address that would otherwise break the query
    /// string. `+` is the interesting one: it is legal in a local part, it means
    /// "space" to a naive query parser, and an `&` or `#` would truncate the link
    /// or split it into a parameter the page never expected.
    #[test]
    fn a_reset_link_carries_the_address_encoded() {
        let link = mail_link(
            identity::tokens::Purpose::Reset,
            "apk_rst_abc",
            "a+b&c#d@example.com",
        );
        assert!(
            link.starts_with("https://apikita.example/reset/confirm?token=apk_rst_abc&email="),
            "got {link}"
        );
        // Exactly the `@` and the three metacharacters are escaped, and nothing else.
        assert!(
            link.ends_with("email=a%2Bb%26c%23d%40example.com"),
            "got {link}"
        );
        // The address must not have leaked in raw form anywhere in the URL.
        assert!(
            !link.contains("+"),
            "a bare + would decode as a space: {link}"
        );
    }

    /// A verification link has no pair to name, so it carries the token alone.
    #[test]
    fn a_verification_link_does_not_carry_the_address() {
        let link = mail_link(
            identity::tokens::Purpose::Verification,
            "apk_vfy_abc",
            "a@b.example",
        );
        assert_eq!(link, "https://apikita.example/verify?token=apk_vfy_abc");
    }

    /// The encoder is the round trip the page performs. Unreserved bytes must pass
    /// through untouched — an encoder that escaped `example.com` to `example%2Ecom`
    /// would still "work", and would still be wrong.
    #[test]
    fn the_query_encoder_leaves_unreserved_bytes_alone() {
        assert_eq!(urlencode("a-b_c.d~e9Z"), "a-b_c.d~e9Z");
        assert_eq!(urlencode("@"), "%40");
        assert_eq!(urlencode("/"), "%2F");
        assert_eq!(urlencode(" "), "%20");
        assert_eq!(urlencode("é"), "%C3%A9");
    }

    /// THE MESSAGE THE ROUTE BUILDS MUST HAVE A RECIPIENT IN IT.
    ///
    /// This is the test for a link that never arrived. `send_link_mail` passed
    /// `to: String::new()`, so every verification and reset mail failed to build
    /// before anything was dialled - and `identity::email::send` reports that as
    /// `EmailError::Build`, which the caller logs against the ACCOUNT with no
    /// address. An operator reading those lines would look at the relay.
    ///
    /// It survived because every test in `identity::email` builds its own `Email`
    /// with a written-out address, so the module was covered and the ROUTE - the
    /// one place in the program that assembles the struct for a real customer -
    /// was not. A test of the mailer cannot see this; only a test of the caller
    /// can, which is why this one lives here rather than beside the mailer.
    ///
    /// IT CALLS `link_mail`, NOT A LOCAL COPY, and that is the whole reason that
    /// function was split out. The first version of this test built its own
    /// `Email` and passed with the defect reinstated - it asserted that a struct
    /// it had just written had the field it had just written. A guard that does
    /// not run the code under guard is a description, not a check.
    #[test]
    fn a_link_mail_is_addressed_to_the_account_it_is_about() {
        let mail = link_mail(
            identity::tokens::Purpose::Verification,
            "apk_vfy_abc",
            "customer@example.com",
        );

        // The recipient must PARSE, because a message whose recipient does not
        // parse is one `send` refuses to build - which is what "no mail is ever
        // sent" looked like from the outside.
        let parsed = mail
            .to
            .parse::<lettre::message::Mailbox>()
            .unwrap_or_else(|e| {
                panic!(
                    "the route addressed the mail to {:?}, which is not sendable: {e}",
                    mail.to
                )
            });
        assert_eq!(
            parsed.email.to_string(),
            "customer@example.com",
            "the recipient must be the account's own address, not an empty or other field"
        );

        // The link is built from the SAME address, so a message that reached the
        // right person could still carry a link for someone else. This is the
        // RESET purpose on purpose: a verification link carries the token alone
        // (see `a_verification_link_does_not_carry_the_address`), so the address
        // can only be checked on the reset side.
        let reset = link_mail(
            identity::tokens::Purpose::Reset,
            "apk_rst_abc",
            "customer@example.com",
        );
        assert!(
            reset.body.contains("email=customer%40example.com"),
            "the address in the link must match the envelope recipient: {}",
            reset.body
        );
        assert!(
            !reset.body.contains("customer@example.com"),
            "the address must not appear raw in a link: {}",
            reset.body
        );
    }

    /// The empty recipient the route used to build cannot be sent at all.
    ///
    /// This is the OTHER half, and it is the half that makes the fix testable: it
    /// pins WHY the bug was fatal rather than cosmetic. If `lettre` ever accepted
    /// an empty mailbox the first test would still pass while the real defect - an
    /// unaddressed message being dialled to a relay - went unnoticed, so the
    /// failure mode is asserted here rather than assumed.
    #[test]
    fn an_empty_recipient_cannot_be_built_and_that_is_why_it_was_fatal() {
        assert!(
            "".parse::<lettre::message::Mailbox>().is_err(),
            "an empty recipient parsed successfully; the guard above no longer \
             describes a fatal defect, and `send_link_mail`'s empty field would \
             have been survivable rather than a dead flow"
        );
    }

    // -----------------------------------------------------------------------
    // LIVE-DATABASE TESTS - the session handlers
    //
    // The tests below run the handlers against a real, migrated SQLite file, and
    // against a real loopback TCP peer where a peer is needed at all: nothing
    // here reaches for an external service, and nothing is #[ignore]d for
    // want of one. Sessions are minted directly in the database where the test
    // is about session handling rather than about logging in - a logout test
    // that had to sign up first would fail for reasons that have nothing to do
    // with logout.
    // -----------------------------------------------------------------------

    use crate::test_support::{self, TestDb};
    use axum::body::to_bytes;
    use chrono::DateTime;
    use serde_json::{json, Value};

    /// docs/observability.md's reconciliation, scoped to one account:
    /// wallets.balance_idr must equal SUM(ledger.delta_idr).
    ///
    /// FULL OUTER JOIN, matching `tools/reconcile/reconcile.sh` and the copies in
    /// `db.rs` and `routes/account.rs`. This used to be a `LEFT JOIN` driven from
    /// `wallets`, which asks a DIFFERENT and weaker question: it sees only accounts that
    /// have a wallet row, while `ledger.account_id` references `accounts(id)` - not
    /// `wallets` - so a ledger row with no wallet is permitted by the schema and was
    /// invisible to it. Measured: an account with a 5000 IDR ledger row and no wallet
    /// row reports drift=1 here and reported drift=0 before, so an assertion using the
    /// old form could pass on an account the shipped Gate 2 reconcile fails.
    ///
    /// THIS COPY HAD NO DETECTING TEST FOR THREE ROUNDS, while both siblings did. Measured:
    /// neutering this helper to report clean always leaves the whole suite GREEN, because its
    /// only two callers assert `== 0` with the message "the fixture must not have manufactured
    /// ledger drift" - a FIXTURE SANITY CHECK, which a helper that always returns 0 satisfies
    /// perfectly. So the shape above was correct and nothing held it there: the same weak-form
    /// regression the comment describes could return and every gate would stay green.
    ///
    /// `the_auth_drift_helper_sees_ledger_money_with_no_wallet_row` below is that detecting
    /// test. It asserts the statement this comment already makes - 5000 IDR of ledger money
    /// with no wallet row is drift=1 - which until now was prose with nothing checking it.
    async fn ledger_drift_rows(pool: &SqlitePool, account_id: Uuid) -> i64 {
        sqlx::query_scalar(
            r#"
            SELECT COUNT(*) FROM (
                SELECT COALESCE(w.account_id, l.account_id) AS account_id
                FROM wallets w
                FULL OUTER JOIN ledger l ON l.account_id = w.account_id
                WHERE COALESCE(w.account_id, l.account_id) = ?
                GROUP BY w.account_id, l.account_id, w.balance_idr
                HAVING w.account_id IS NULL
                    OR w.balance_idr <> COALESCE(SUM(l.delta_idr), 0)
            ) AS drift
            "#,
        )
        .bind(account_id.hyphenated())
        .fetch_one(pool)
        .await
        .expect("reconciliation query")
    }

    /// THE DETECTING TEST for `ledger_drift_rows` above, which had none.
    ///
    /// Both of its callers assert `ledger_drift_rows(...) == 0` as a fixture sanity check
    /// ("the fixture must not have manufactured ledger drift"). A helper that ALWAYS returns 0
    /// satisfies that perfectly, so neither caller can catch the helper weakening - MEASURED:
    /// replacing the `HAVING` clause with `HAVING 0` leaves all 632 tests green.
    ///
    /// This asserts the other direction, on the one case the schema permits and the old
    /// `LEFT JOIN` form could not see: a `ledger` row whose account has NO `wallets` row.
    /// `ledger.account_id` references `accounts(id)`, not `wallets`, so nothing forbids it -
    /// and it is real money with no cache holding it, which is exactly what
    /// `tools/reconcile/reconcile.sh` exists to find.
    ///
    /// Written as a direct assertion on the helper rather than through a handler, because no
    /// auth route can create this state - which is why the gap survived three rounds.
    #[tokio::test]
    async fn the_auth_drift_helper_sees_ledger_money_with_no_wallet_row() {
        let db = TestDb::new().await;
        let account = test_support::account(&db.pool).await;

        // A ledger row with no wallets row. The account exists (the FK needs it); the
        // wallet deliberately does not.
        sqlx::query(
            "INSERT INTO ledger (account_id, delta_idr, reason, balance_after, created_at) \
             VALUES (?, 5000, 'adjustment', 5000, '2026-01-01T00:00:00+00:00')",
        )
        .bind(account.hyphenated())
        .execute(&db.pool)
        .await
        .expect("insert the orphan ledger row");

        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM wallets WHERE account_id = ?")
                .bind(account.hyphenated())
                .fetch_one(&db.pool)
                .await
                .expect("count wallets"),
            0,
            "the fixture must have NO wallet row, or this test is not testing the case"
        );

        assert_eq!(
            ledger_drift_rows(&db.pool, account).await,
            1,
            "5000 IDR of ledger money with no wallet row is DRIFT, and the shipped gate \
             reports it (reconcile.sql is a FULL OUTER JOIN for this case). This helper \
             returning 0 here means it has silently become a weaker rule than the one that \
             ships, and both of its callers - which assert == 0 as a fixture check - would \
             not notice."
        );

        // And it stops reporting drift once a wallet row agrees with the ledger, so this is a
        // detector rather than a constant.
        sqlx::query(
            "INSERT INTO wallets (account_id, balance_idr, updated_at) \
             VALUES (?, 5000, '2026-01-01T00:00:00+00:00')",
        )
        .bind(account.hyphenated())
        .execute(&db.pool)
        .await
        .expect("insert the wallet row");

        assert_eq!(
            ledger_drift_rows(&db.pool, account).await,
            0,
            "once the wallet holds the ledger's 5000 IDR there is no drift, so the helper \
             must report 0 - otherwise this test would pass on a helper that always says 1"
        );

        db.close().await;
    }

    /// Sessions that are actually usable: unrevoked AND unexpired. Both bounds
    /// matter - an expired-but-unrevoked row is not a credential, and counting
    /// it as "live" would make an expired cookie look like a valid session.
    ///
    /// `settle_topup` used to sit just above this. It was DELETED rather than left
    /// as dead code: the one `live_*` PocketBase fixture that called it went with
    /// the provider it exercised, and none of the surviving tests here fund a
    /// wallet. `routes/account.rs` and `test_support` each keep their own funding
    /// fixture because their tests do fund accounts; a third copy here would
    /// compile, read as load-bearing, and silently drift from the ones that run.
    async fn live_sessions(pool: &SqlitePool, account_id: Uuid) -> i64 {
        // Both bounds are bound: an earlier port bound only the account, leaving the
        // expiry comparison against NULL - which is never TRUE, so every count came
        // back 0 and the assertions below read as "logout deleted rows".
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM sessions WHERE account_id = ? AND revoked_at IS NULL AND expires_at > ?",
        )
        .bind(account_id.hyphenated())
        .bind(Utc::now())
        .fetch_one(pool)
        .await
        .expect("count live sessions")
    }

    /// A live session for an existing account, created the way exchange_token
    /// creates one: the row holds only the SHA-256 of the token.
    async fn add_live_session(
        pool: &SqlitePool,
        account_id: Uuid,
        expires_at: DateTime<Utc>,
    ) -> String {
        let token = format!("apk_sess_{}", Uuid::new_v4().simple());
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO sessions (id, account_id, token_hash, expires_at, last_seen_at, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(Uuid::new_v4().hyphenated())
        .bind(account_id.hyphenated())
        .bind(hash_token(&token))
        .bind(expires_at)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .expect("create session");
        token
    }

    /// A session with an EXPLICIT `last_seen_at`, which `add_live_session` cannot
    /// express because it always stamps "now".
    ///
    /// That helper's freshness is what made the idle rule untestable: every session
    /// any test built was freshly touched, so `session_is_live_at`'s idle branch was
    /// never the deciding one and `logout_all`'s missing idle check could not show.
    /// Being able to say "this session has not been used for N days" is the whole
    /// point.
    async fn add_session_with_last_seen(
        pool: &SqlitePool,
        account_id: Uuid,
        expires_at: DateTime<Utc>,
        last_seen_at: DateTime<Utc>,
    ) -> String {
        let token = format!("apk_sess_{}", Uuid::new_v4().simple());
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO sessions (id, account_id, token_hash, expires_at, last_seen_at, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(Uuid::new_v4().hyphenated())
        .bind(account_id.hyphenated())
        .bind(hash_token(&token))
        .bind(expires_at)
        .bind(last_seen_at)
        .bind(now)
        .execute(pool)
        .await
        .expect("create session");
        token
    }

    /// An account with a live session, built the way the login path builds one:
    /// an accounts row, the zero-balance wallets row, and a sessions row.
    struct LiveAccount {
        account_id: Uuid,
        token: String,
    }

    async fn live_account(pool: &SqlitePool) -> LiveAccount {
        // Ported: the Postgres original leaned on column DEFAULTS for `accounts.id`,
        // `accounts.created_at` and `wallets.updated_at`; the strict SQLite schema has
        // none, so the shared fixture binds them (plan section 4.1, correction 1).
        let account_id = test_support::account(pool).await;
        test_support::wallet(pool, account_id).await;

        let token = add_live_session(pool, account_id, Utc::now() + Duration::days(30)).await;

        LiveAccount { account_id, token }
    }

    fn cookie_header(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            format!("a=1; session={token}; b=2").parse().unwrap(),
        );
        headers
    }

    /// The fixed client address the tests drive through `ConnectInfo`.
    ///
    /// `exchange_token` resolves its client address from the TCP peer, so a test
    /// that calls the handler directly has to supply one. It is TEST-NET-3
    /// (`203.0.113.0/24`, RFC 5737) rather than loopback, because loopback is
    /// exactly what a trusted-proxy test would use and the two must not be
    /// confusable when reading a failure.
    const TEST_IP: &str = "203.0.113.40";

    fn peer() -> ConnectInfo<std::net::SocketAddr> {
        ConnectInfo(format!("{TEST_IP}:5555").parse().unwrap())
    }

    /// An `AppState` over the database under test.
    ///
    /// `login` takes the whole state, not just the pool, because the sign-in caps
    /// need the daily salt and the trusted-proxy list - the same reason
    /// `proxy::chat_completions` does. The salt is built fresh per test and never
    /// persisted, matching production (`main.rs` builds one per process).
    ///
    /// **CALL IT ONCE PER TEST AND REUSE THE STATE.** The salt is what turns a
    /// client address into the `auth_attempts.ip_hash` a cap counts against, so
    /// two states built by two calls are two salts, hence two keys, hence two
    /// separate budgets - and a test that built a state per request would watch a
    /// cap of two allow a hundred attempts while looking like it measured the cap.
    /// That is not hypothetical: it is exactly how the first version of the
    /// login-cap tests below passed while asserting nothing. Production has one
    /// salt per process, so one state per test is the faithful fixture.
    fn state_for(pool: &SqlitePool) -> AppState {
        let config = std::sync::Arc::new(
            AppConfig::load_from_file("../config/apikita.toml")
                .expect("the shipped config parses; every route module's fixture loads it"),
        );
        AppState {
            pool: pool.clone(),
            events: std::sync::Arc::new(crate::routes::events::RealtimeHub::new(&config.realtime)),
            http_client: reqwest::Client::new(),
            ip_salt: std::sync::Arc::new(crate::ip_tracking::DailySalt::new()),
            trusted_proxies: std::sync::Arc::from(Vec::new()),
            config,
        }
    }

    /// Everything the assertions need out of a handler's response. The raw body
    /// is kept as bytes because logout answers 204 with no body at all, so the
    /// body cannot be assumed to be JSON.
    struct LiveResponse {
        status: StatusCode,
        headers: HeaderMap,
        body: Vec<u8>,
    }

    /// Drives a handler exactly the way the router does.
    async fn call<F, T>(result: F) -> LiveResponse
    where
        F: std::future::Future<Output = Result<T, AppError>>,
        T: IntoResponse,
    {
        let response = match result.await {
            Ok(ok) => ok.into_response(),
            Err(err) => err.into_response(),
        };
        let status = response.status();
        let headers = response.headers().clone();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("every response must have a readable body")
            .to_vec();
        LiveResponse {
            status,
            headers,
            body,
        }
    }

    impl LiveResponse {
        fn body_text(&self) -> String {
            String::from_utf8_lossy(&self.body).to_string()
        }

        /// The JSON body. Only called where one is expected: docs/error-model.md
        /// makes every error a JSON body, and the success paths under test here
        /// are 200 (JSON) or 204 (empty).
        fn json(&self) -> Value {
            assert!(!self.body.is_empty(), "expected a JSON body, got none");
            serde_json::from_slice(&self.body)
                .unwrap_or_else(|e| panic!("response body is not JSON ({e}): {}", self.body_text()))
        }

        /// The session cookie the handler set. All three handlers always set one.
        fn set_cookie(&self) -> String {
            self.headers
                .get(header::SET_COOKIE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_else(|| panic!("no Set-Cookie header on a {}", self.status))
                .to_string()
        }

        /// The session cookie the handler cleared, as the credential it carries
        /// - which must be nothing.
        fn cleared_token(&self) -> Option<String> {
            session_token_from_cookie_header(&self.set_cookie()).map(|t| t.to_string())
        }
    }

    /// The session's absolute expiry is taken from config, not from a literal.
    #[tokio::test]
    async fn live_the_session_expiry_honours_the_configured_absolute_days() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();

        let email = "expiry-check@example.com";
        let password = "a-real-enough-password";
        let hash = identity::password::hash_password(
            auth_config().expect("auth config").clone(),
            password.to_string(),
        )
        .await
        .expect("hash the fixture password");
        let account_id = identity::accounts::create_password_account(
            &pool,
            email,
            &hash,
            Utc::now() - Duration::minutes(5),
        )
        .await
        .expect("seed the account the login will find");

        let response = call(login(
            State(state_for(&pool)),
            peer(),
            HeaderMap::new(),
            Ok(Json(LoginRequest {
                email: email.into(),
                password: password.into(),
            })),
        ))
        .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "the fixture login must succeed: {}",
            response.body_text()
        );

        let expires_at: DateTime<Utc> =
            sqlx::query_scalar("SELECT expires_at FROM sessions WHERE account_id = ?")
                .bind(account_id.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("read the expiry");

        let configured = sessions_config()
            .expect("the shipped config loads")
            .absolute_days as i64;
        let expected_max = Utc::now() + Duration::days(configured) + Duration::minutes(1);
        let expected_min = Utc::now() + Duration::days(configured) - Duration::minutes(1);
        assert!(
            expires_at <= expected_max && expires_at >= expected_min,
            "expiry {expires_at} must be ~{configured} days out, from config rather than a literal"
        );

        db.close().await;
    }

    // The former live-PocketBase identity fixtures and the eight tests that drove
    // `exchange_token` against a loopback stub stood here. They are DELETED rather
    // than kept: the identity provider they exercised is gone from this crate, and
    // a test that stubs a service the code no longer talks to is not coverage, it
    // is a description of a program that is not running. The properties those
    // tests asserted that are STILL true of the native endpoints - a fresh signup
    // creates the account and the wallet, the balance comes from the ledger, a
    // refused credential is a 401 and not a 500 - now belong to the signup/login
    // tests, and are asserted there.

    #[tokio::test]
    async fn live_logout_revokes_exactly_this_session_and_clears_the_cookie() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let victim = live_account(&pool).await;
        let sibling = live_account(&pool).await;

        let outcome = tokio::spawn(logout_assertions(
            pool.clone(),
            victim.account_id,
            victim.token.clone(),
            sibling.account_id,
            sibling.token.clone(),
        ));
        let outcome = outcome.await;

        db.close().await;
        outcome.expect("the logout assertions panicked");
    }

    async fn logout_assertions(
        pool: SqlitePool,
        victim_id: Uuid,
        victim_token: String,
        sibling_id: Uuid,
        sibling_token: String,
    ) {
        // A request with no cookie at all is still a 204 that clears the cookie.
        let anonymous = call(logout(State(pool.clone()), HeaderMap::new())).await;
        assert_eq!(anonymous.status, StatusCode::NO_CONTENT);
        assert!(
            anonymous.body.is_empty(),
            "docs/server/api-spec.md: logout is a 204 with no body, got {}",
            anonymous.body_text()
        );
        assert_eq!(
            anonymous.cleared_token(),
            None,
            "the cleared cookie must carry no credential: {}",
            anonymous.set_cookie()
        );
        assert!(
            anonymous.set_cookie().contains("Max-Age=0"),
            "the cookie must be expired immediately: {}",
            anonymous.set_cookie()
        );

        let live_before: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sessions WHERE account_id = ? AND revoked_at IS NULL",
        )
        .bind(victim_id.hyphenated())
        .fetch_one(&pool)
        .await
        .expect("count live sessions");
        assert_eq!(
            live_before, 1,
            "the fixture must start with one live session"
        );

        // --- the real logout ---
        let response = call(logout(State(pool.clone()), cookie_header(&victim_token))).await;
        assert_eq!(response.status, StatusCode::NO_CONTENT);
        assert!(response.body.is_empty(), "logout answers 204, no body");
        assert_eq!(
            response.cleared_token(),
            None,
            "logout must clear the cookie it just revoked: {}",
            response.set_cookie()
        );

        // The row is REVOKED, not deleted: the audit trail survives.
        let revoked_at: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT revoked_at FROM sessions WHERE token_hash = ?")
                .bind(hash_token(&victim_token))
                .fetch_one(&pool)
                .await
                .expect("the session row must still exist");
        assert!(
            revoked_at.is_some(),
            "logout must set revoked_at, not delete the row"
        );

        // The revoked session stops resolving - this is what "real revocation"
        // means for every authenticated endpoint, which all share this reader.
        assert!(
            resolve_account_from_cookie(&pool, &cookie_header(&victim_token))
                .await
                .is_err(),
            "a revoked session must not resolve an account"
        );

        // The sibling session on the OTHER account is untouched, so the UPDATE
        // cannot be revoking more than the row it names.
        assert_eq!(
            resolve_account_from_cookie(&pool, &cookie_header(&sibling_token))
                .await
                .expect("the other account's session must survive"),
            sibling_id
        );
        let sibling_live: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sessions WHERE account_id = ? AND revoked_at IS NULL",
        )
        .bind(sibling_id.hyphenated())
        .fetch_one(&pool)
        .await
        .expect("count the sibling's live sessions");
        assert_eq!(sibling_live, 1);

        // A second logout is idempotent AND must not move the revocation
        // timestamp: that is the "AND revoked_at IS NULL" guard doing its job.
        let second = call(logout(State(pool.clone()), cookie_header(&victim_token))).await;
        assert_eq!(second.status, StatusCode::NO_CONTENT);
        let revoked_again: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT revoked_at FROM sessions WHERE token_hash = ?")
                .bind(hash_token(&victim_token))
                .fetch_one(&pool)
                .await
                .expect("read revoked_at again");
        assert_eq!(
            revoked_again, revoked_at,
            "a second logout must not rewrite the original revocation time"
        );

        // The victim account is otherwise intact.
        let sessions_left: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE account_id = ?")
                .bind(victim_id.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("count the victim's sessions");
        assert_eq!(sessions_left, 1, "logout must not delete rows");
        assert_eq!(
            ledger_drift_rows(&pool, victim_id).await,
            0,
            "the fixture must not have manufactured ledger drift"
        );
    }

    // -----------------------------------------------------------------------
    // 3. logout_all
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn live_logout_all_revokes_every_session_for_the_account_only() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let device_a = live_account(&pool).await;
        // device_b is a SECOND session on the SAME account, which is the whole
        // point of "sign out everywhere": two accounts prove nothing.
        let device_b =
            add_live_session(&pool, device_a.account_id, Utc::now() + Duration::days(30)).await;
        let other = live_account(&pool).await;

        let outcome = tokio::spawn(logout_all_assertions(
            pool.clone(),
            device_a.account_id,
            device_a.token.clone(),
            device_b,
            other.account_id,
            other.token.clone(),
        ));
        let outcome = outcome.await;

        db.close().await;
        outcome.expect("the logout_all assertions panicked");
    }

    async fn logout_all_assertions(
        pool: SqlitePool,
        account_id: Uuid,
        token_a: String,
        token_b: String,
        other_id: Uuid,
        other_token: String,
    ) {
        // A third device for the same account: logout-all must reach sessions it
        // was not told about.
        let token_c = add_live_session(&pool, account_id, Utc::now() + Duration::days(30)).await;

        let live_before = live_sessions(&pool, account_id).await;
        assert_eq!(
            live_before, 3,
            "the fixture must start with three live sessions"
        );

        // --- sign out everywhere, driven by ONE of them ---
        let response = call(logout_all(State(pool.clone()), cookie_header(&token_a))).await;
        assert_eq!(response.status, StatusCode::NO_CONTENT);
        assert!(
            response.body.is_empty(),
            "docs/server/api-spec.md: logout-all is a 204 with no body"
        );
        assert_eq!(
            response.cleared_token(),
            None,
            "logout-all must clear the caller's cookie: {}",
            response.set_cookie()
        );

        let live_after = live_sessions(&pool, account_id).await;
        assert_eq!(
            live_after, 0,
            "docs/architecture/identity.md: sign out everywhere revokes EVERY session for the account"
        );

        // Revoked, not deleted: all three rows are still there.
        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .fetch_one(&pool)
            .await
            .expect("count all sessions");
        assert_eq!(total, 3, "revocation must not delete the session rows");

        // Every one of the account's sessions - including the one that asked -
        // is now dead.
        for (label, token) in [("a", &token_a), ("b", &token_b), ("c", &token_c)] {
            assert!(
                resolve_account_from_cookie(&pool, &cookie_header(token))
                    .await
                    .is_err(),
                "session {label} must be revoked"
            );
        }

        // The OTHER account's session is untouched: the UPDATE is scoped by
        // account_id, not global.
        assert_eq!(
            resolve_account_from_cookie(&pool, &cookie_header(&other_token))
                .await
                .expect("another account's session must survive logout-all"),
            other_id
        );

        // A second logout-all has nothing left to revoke and is still a 204.
        let second = call(logout_all(State(pool.clone()), cookie_header(&token_a))).await;
        assert_eq!(
            second.status,
            StatusCode::NO_CONTENT,
            "logout-all must be idempotent: {}",
            second.body_text()
        );
        assert_eq!(live_sessions(&pool, account_id).await, 0);

        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            0,
            "the fixture must not have manufactured ledger drift"
        );
    }

    /// THE IDLE HALF OF THE SAME RULE, which the `expires_at` test above cannot
    /// reach.
    ///
    /// An idle session is not expired - its `expires_at` is comfortably in the
    /// future - so it passes the SQL predicate and, until this round, passed
    /// `logout_all`'s check too. It has been refused by every other route the whole
    /// time, because the shared resolver applies `session_is_live_at`. So the
    /// cookie could not authenticate anything, and could still sign every other
    /// device on the account out: a customer-facing denial of service available to
    /// anyone holding a token they can no longer use.
    ///
    /// WHY NO EXISTING TEST COULD SEE IT: `add_live_session` always writes
    /// `last_seen_at = now`, so every session any test builds is freshly touched.
    /// The defect was unreachable by construction from the fixture - the third time
    /// in this run of rounds that a fixture which always supplies a value could not
    /// observe a defect consisting of that value being wrong. The helper below
    /// exists to make the idle case expressible at all.
    #[tokio::test]
    async fn live_logout_all_ignores_an_idle_session_that_still_has_time_left() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account = live_account(&pool).await;

        // Expires in 30 days: NOT expired. Last seen long before the idle window.
        let idle_days = sessions_config().expect("session config").idle_days as i64;
        let idle_token = add_session_with_last_seen(
            &pool,
            account.account_id,
            Utc::now() + Duration::days(30),
            Utc::now() - Duration::days(idle_days + 1),
        )
        .await;

        let outcome = tokio::spawn(logout_all_ignores_idle_sessions(
            pool.clone(),
            account.account_id,
            account.token.clone(),
            idle_token,
            idle_days,
        ));
        let outcome = outcome.await;

        db.close().await;
        outcome.expect("the logout_all idle-session assertions panicked");
    }

    async fn logout_all_ignores_idle_sessions(
        pool: SqlitePool,
        account_id: Uuid,
        live_token: String,
        idle_token: String,
        idle_days: i64,
    ) {
        // The fixture must be the case that matters: unexpired, but idle.
        assert!(
            resolve_account_from_cookie(&pool, &cookie_header(&idle_token))
                .await
                .is_err(),
            "the fixture's idle session must not resolve: if it does, this test is \
             not exercising the idle rule at all"
        );
        let expires_at: String =
            sqlx::query_scalar("SELECT expires_at FROM sessions WHERE token_hash = ?")
                .bind(hash_token(&idle_token))
                .fetch_one(&pool)
                .await
                .expect("read the idle session's expiry");
        assert!(
            expires_at.as_str() > Utc::now().to_rfc3339().as_str(),
            "the idle session must still be UNEXPIRED ({expires_at}), or this is just \
             the expires_at test again"
        );

        let idle = call(logout_all(State(pool.clone()), cookie_header(&idle_token))).await;
        assert_eq!(
            idle.status,
            StatusCode::NO_CONTENT,
            "an idle cookie is not an error, it is just not a credential: {}",
            idle.body_text()
        );

        // `live_sessions()` counts unrevoked-and-unexpired rows, and it does NOT
        // apply the idle rule - an idle session is still "live" by that query. So
        // the expected count is BOTH rows, and anything lower would mean the idle
        // cookie revoked something it had no right to.
        assert_eq!(
            live_sessions(&pool, account_id).await,
            2,
            "an IDLE session (last seen {} days ago, expires_at in the future) must not sign \
             the account's live sessions out. It cannot authenticate anything else - the shared \
             resolver refuses it - so accepting it here would let a token that no longer works \
             act destructively on every other device.",
            idle_days + 1
        );

        // Neither row may be revoked, checked by name rather than by count, so this
        // cannot pass because the two errors cancelled out.
        for (label, token) in [
            ("the idle caller's own", &idle_token),
            ("the live one", &live_token),
        ] {
            let revoked_at: Option<String> =
                sqlx::query_scalar("SELECT revoked_at FROM sessions WHERE token_hash = ?")
                    .bind(hash_token(token))
                    .fetch_one(&pool)
                    .await
                    .expect("read revoked_at");
            assert_eq!(
                revoked_at, None,
                "{label} session was revoked by an idle cookie. That cookie resolves to no \
                 account anywhere else in the server, so this would be a denial of service \
                 available to anyone holding a stale token."
            );
        }

        // ...and the live session still works, so nothing was silently killed.
        assert_eq!(
            resolve_account_from_cookie(&pool, &cookie_header(&live_token))
                .await
                .expect("the live session must survive"),
            account_id
        );
    }

    /// The "expires_at > ?" clause in logout_all's SELECT. Without it, an
    /// expired-but-unrevoked cookie would be a credential that can sign every
    /// other device out - the one thing a dead session must not be able to do.
    #[tokio::test]
    async fn live_logout_all_ignores_a_session_that_is_not_live() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account = live_account(&pool).await;
        let expired_token =
            add_live_session(&pool, account.account_id, Utc::now() - Duration::days(1)).await;

        let outcome = tokio::spawn(logout_all_ignores_dead_sessions(
            pool.clone(),
            account.account_id,
            account.token.clone(),
            expired_token,
        ));
        let outcome = outcome.await;

        db.close().await;
        outcome.expect("the logout_all dead-session assertions panicked");
    }

    /// The "revoked_at IS NULL" clause in logout_all's SELECT - the half its neighbour above does
    /// NOT cover.
    ///
    /// `live_logout_all_ignores_a_session_that_is_not_live` tests the `expires_at > ?` clause with an
    /// EXPIRED cookie. An expired row fails `session_is_live_at` on `expires_at` whatever the SQL
    /// says, so that test passes with either clause removed and does not reach this one.
    ///
    /// MEASURED: deleting `revoked_at IS NULL` from logout_all's SELECT - and only from that copy,
    /// leaving the two resolver copies intact - left the whole suite at 664 passed / 0 failed.
    ///
    /// WHY IT MATTERS. `session_is_live_at` takes NO revocation parameter, so it cannot catch a
    /// revoked row. The only thing standing between a REVOKED-but-unexpired cookie and the global
    /// revoke below is this SQL clause. Without it, a token the server no longer honours anywhere
    /// else can still sign every other device on the account out - the same denial of service the
    /// doc-comment on `logout_all` describes for the idle half, arriving through the half that is
    /// present in its SQL.
    ///
    /// The fixture is the case the clause exists for and no test had: revoked, not yet expired.
    #[tokio::test]
    async fn live_logout_all_ignores_a_revoked_but_unexpired_session() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account = live_account(&pool).await;

        // Revoked, but future-dated: `session_is_live_at` will call this live.
        let revoked_token =
            add_live_session(&pool, account.account_id, Utc::now() + Duration::days(30)).await;
        // A second live session that must SURVIVE. If logout_all fires, this is revoked too.
        let survivor_token = account.token.clone();

        sqlx::query("UPDATE sessions SET revoked_at = ? WHERE token_hash = ?")
            .bind(Utc::now())
            .bind(hash_token(&revoked_token))
            .execute(&pool)
            .await
            .expect("revoke the first session");

        let outcome = tokio::spawn(logout_all_ignores_revoked_sessions(
            pool.clone(),
            account.account_id,
            revoked_token,
            survivor_token,
        ));
        let outcome = outcome.await;

        db.close().await;
        outcome.expect("the logout_all revoked-session assertions panicked");
    }

    async fn logout_all_ignores_revoked_sessions(
        pool: SqlitePool,
        account_id: Uuid,
        revoked_token: String,
        survivor_token: String,
    ) {
        assert_eq!(
            live_sessions(&pool, account_id).await,
            1,
            "the fixture must leave exactly one live session, so a global revoke is visible"
        );

        // A revoked session must not resolve; it is no longer a credential anywhere.
        assert!(
            resolve_account_from_cookie(&pool, &cookie_header(&revoked_token))
                .await
                .is_err(),
            "the fixture's revoked session must not resolve"
        );

        let response = call(logout_all(
            State(pool.clone()),
            cookie_header(&revoked_token),
        ))
        .await;
        assert_eq!(
            response.status,
            StatusCode::NO_CONTENT,
            "a revoked cookie is not an error, it is just not a credential: {}",
            response.body_text()
        );

        assert_eq!(
            live_sessions(&pool, account_id).await,
            1,
            "a REVOKED-but-unexpired cookie must not sign the account's other sessions out. If this \
             is 0, logout_all's SELECT returned a revoked row - its `revoked_at IS NULL` clause is \
             gone or bypassed, and `session_is_live_at` cannot catch it because it takes no \
             revocation parameter."
        );

        // ...and the surviving session still works, so nothing was silently killed.
        assert_eq!(
            resolve_account_from_cookie(&pool, &cookie_header(&survivor_token))
                .await
                .expect("the surviving session must still resolve"),
            account_id
        );
    }

    async fn logout_all_ignores_dead_sessions(
        pool: SqlitePool,
        account_id: Uuid,
        live_token: String,
        expired_token: String,
    ) {
        // An expired session cannot resolve an account, so it must not be able
        // to revoke anything.
        assert!(
            resolve_account_from_cookie(&pool, &cookie_header(&expired_token))
                .await
                .is_err(),
            "the fixture's expired session must not resolve"
        );

        let expired = call(logout_all(
            State(pool.clone()),
            cookie_header(&expired_token),
        ))
        .await;
        assert_eq!(
            expired.status,
            StatusCode::NO_CONTENT,
            "an expired cookie is not an error, it is just not a credential: {}",
            expired.body_text()
        );
        let live_after_expired = live_sessions(&pool, account_id).await;
        assert_eq!(
            live_after_expired, 1,
            "an EXPIRED session must not sign the account's live sessions out"
        );

        // No cookie at all: same answer.
        let anonymous = call(logout_all(State(pool.clone()), HeaderMap::new())).await;
        assert_eq!(anonymous.status, StatusCode::NO_CONTENT);
        let live_after_anonymous = live_sessions(&pool, account_id).await;
        assert_eq!(
            live_after_anonymous, 1,
            "a cookie-less logout-all must revoke nothing"
        );

        // ...and the live session still works, so nothing was silently killed.
        assert_eq!(
            resolve_account_from_cookie(&pool, &cookie_header(&live_token))
                .await
                .expect("the live session must be untouched"),
            account_id
        );
    }

    // -----------------------------------------------------------------------
    // The login cap. `config/apikita.toml` ships `login_per_hour_per_ip = 20`,
    // which no test can reach without twenty round trips, so both tests below
    // state a lower cap through `set_test_limits_override` rather than by writing
    // a config file - the accessor consults that override before its cache.
    //
    // The cap is the only ceiling in this module whose KEY is read by production
    // code, so these tests are also what keeps
    // `every_config_field_is_read_by_production_code_or_explained` honest: without
    // them the key would be "read" by the handler in a way no test ever exercises.
    // -----------------------------------------------------------------------

    /// THE CAP FIRES, and it fires on FAILED attempts.
    ///
    /// The second attempt below presents a password no account has - so the
    /// refusal under test is not "the budget ran out on a success", it is "the
    /// budget is spent by guessing", which is the only property that makes this
    /// cap worth having. A cap that counted successes would let a guesser try
    /// forever and never fire.
    #[tokio::test]
    async fn live_the_login_cap_refuses_a_guesser_after_the_configured_attempts() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();

        let _lock = crate::routes::test_env::EnvLock::acquire();
        // The cap is what this test is about, and the shipped config sets it to 20
        // - unreachable without twenty round trips. The override below is how a
        // test states a different cap; see the note on it near `limits_config`.
        //
        // The cap is TWO, not one, and that is not a softening. `login` records
        // the attempt BEFORE it checks the budget (see `record_and_check_login`),
        // so the first attempt's own row is already on the books when the count is
        // read - a cap of one is spent by the very attempt it is counting and
        // refuses it, which would make the 401 below unreachable and this test a
        // statement about the wrong property. With a cap of two the attempt that
        // is refused is unambiguously the one that arrived after the budget was
        // full, which is what "the cap refuses a guesser" means.
        let _caps = TestLimitsGuard::set(2, 100);
        // ONE state for the whole test. See the note on `state_for`: the salt it
        // carries is the key the per-IP counter is stored under, so a state per
        // request would put every attempt in its own budget and the cap below
        // could never fire however many attempts were sent.
        let state = state_for(&pool);

        // Attempt 1: the credential is wrong, so this is a 401 - a guesser's first
        // try is answered, not throttled. The IP budget is now one spent.
        let first = call(login(
            State(state.clone()),
            peer(),
            HeaderMap::new(),
            Ok(Json(LoginRequest {
                email: "nobody@example.com".into(),
                password: "wrong-password".into(),
            })),
        ))
        .await;
        assert_eq!(
            first.status,
            StatusCode::UNAUTHORIZED,
            "a rejected credential is a 401, not a throttle: {}",
            first.body_text()
        );

        // Attempt 2 from the same address: the second and last of the budget, still
        // answered as a credential failure.
        let second = call(login(
            State(state.clone()),
            peer(),
            HeaderMap::new(),
            Ok(Json(LoginRequest {
                email: "nobody@example.com".into(),
                password: "wrong-password".into(),
            })),
        ))
        .await;
        assert_eq!(
            second.status,
            StatusCode::UNAUTHORIZED,
            "the last attempt inside the budget is still a credential answer: {}",
            second.body_text()
        );

        // Attempt 3: out of budget. The handler refuses on the count BEFORE it
        // looks the credential up, so this answer is the throttle and not a third
        // 401 - which is what the status pins.
        let third = call(login(
            State(state.clone()),
            peer(),
            HeaderMap::new(),
            Ok(Json(LoginRequest {
                email: "nobody@example.com".into(),
                password: "wrong-password".into(),
            })),
        ))
        .await;
        assert_eq!(
            third.status,
            StatusCode::TOO_MANY_REQUESTS,
            "the attempt after the budget is spent must be throttled: {}",
            third.body_text()
        );
        assert_eq!(third.json()["error"]["code"], json!("rate_limited"));

        // The attempt rows are the audit trail, and there are THREE of them - every
        // attempt, including the one the cap refused. That is not an oversight:
        // `record_and_check_login` writes both counters BEFORE it checks either,
        // because a caller already over the per-IP cap must still move the
        // per-account counter, or a distributed guesser - over the per-IP cap by
        // construction - would never touch the counter written to catch them. The
        // cost is that a throttled caller does grow the table; the bound is that
        // the window is an hour and the growth is one row per REFUSED request,
        // which is the price of the two counters staying independent.
        let recorded: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM auth_attempts WHERE kind = 'login'")
                .fetch_one(&pool)
                .await
                .expect("count attempts");
        assert_eq!(
            recorded, 3,
            "every attempt must be on the books, the refused one included - the failure \
             stream IS the signal"
        );

        // No raw address anywhere, and the key is the salted hash.
        let keys: Vec<String> =
            sqlx::query_scalar("SELECT DISTINCT ip_hash FROM auth_attempts WHERE kind = 'login'")
                .fetch_all(&pool)
                .await
                .expect("read the attempt keys");
        assert_eq!(keys.len(), 1, "one address, one key");
        assert_eq!(keys[0].len(), 64, "a SHA-256 hex digest, not an address");
        assert!(
            !keys[0].contains(TEST_IP),
            "the raw address must never appear in the stored key"
        );

        db.close().await;
    }

    /// A DIFFERENT address is a different budget: the cap is per client, not
    /// global. Without this the first test would also pass if the limiter were
    /// counting every login in the process.
    #[tokio::test]
    async fn live_the_login_cap_is_per_address_and_does_not_lock_everyone_out() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();

        let _lock = crate::routes::test_env::EnvLock::acquire();
        let _caps = TestLimitsGuard::set(2, 100);
        // ONE state for the whole test. See the note on `state_for`: the salt it
        // carries is the key the per-IP counter is stored under, so a state per
        // request would put every attempt in its own budget and the cap below
        // could never fire however many attempts were sent.
        let state = state_for(&pool);

        // The first address spends its whole budget: the two attempts below are
        // both answered as credential failures, and a third one would be the
        // throttle. Pinning the exhausted state is what makes the next assertion
        // about the OTHER address rather than about a budget nothing spent.
        for _ in 0..2 {
            let spent = call(login(
                State(state.clone()),
                peer(),
                HeaderMap::new(),
                Ok(Json(LoginRequest {
                    email: "nobody@example.com".into(),
                    password: "wrong-password".into(),
                })),
            ))
            .await;
            assert_eq!(spent.status, StatusCode::UNAUTHORIZED);
        }

        let exhausted = call(login(
            State(state.clone()),
            peer(),
            HeaderMap::new(),
            Ok(Json(LoginRequest {
                email: "nobody@example.com".into(),
                password: "wrong-password".into(),
            })),
        ))
        .await;
        assert_eq!(
            exhausted.status,
            StatusCode::TOO_MANY_REQUESTS,
            "the first address must be out of budget, or the assertion below is vacuous"
        );

        // Same database, same instant, DIFFERENT peer address. If the limiter were
        // counting every login in the process rather than per client, this one
        // would be throttled too.
        let other = ConnectInfo("198.51.100.7:5555".parse().unwrap());

        let second = call(login(
            State(state.clone()),
            other,
            HeaderMap::new(),
            Ok(Json(LoginRequest {
                email: "nobody@example.com".into(),
                password: "wrong-password".into(),
            })),
        ))
        .await;
        assert_eq!(
            second.status,
            StatusCode::UNAUTHORIZED,
            "a different address has its own budget and must reach the credential check: {}",
            second.body_text()
        );

        db.close().await;
    }

    // --- POST /auth/password-change -----------------------------------------
    //
    // Six tests. This endpoint's whole reason to exist is the pair of guards a
    // caller can try to skip - the current password, and the session revocation -
    // so each test below fails if exactly one of them is removed. The set is a
    // specification, not a smoke test.

    /// A password identity for an account, written the way signup writes one.
    async fn password_identity_for(
        pool: &SqlitePool,
        account_id: Uuid,
        email: &str,
        password: &str,
    ) {
        let hash = identity::password::hash_password(
            auth_config().expect("auth config").clone(),
            password.to_string(),
        )
        .await
        .expect("hash the fixture password");
        identity::accounts::upsert_password_identity(
            pool,
            account_id,
            email,
            &hash,
            true,
            Utc::now(),
        )
        .await
        .expect("create the password identity");
    }

    /// A Google identity row, written the way the Google path writes one. The
    /// schema's CHECK refuses a google row whose flag is clear, so the flag is set
    /// here rather than left to a column default that does not exist.
    async fn google_identity_for(pool: &SqlitePool, account_id: Uuid, subject: &str, email: &str) {
        sqlx::query(
            "INSERT INTO identities (id, account_id, provider, subject, email, email_verified, \
             created_at, updated_at) VALUES (?,?,?,?,?,?,?,?)",
        )
        .bind(Uuid::new_v4().hyphenated().to_string())
        .bind(account_id.hyphenated())
        .bind("google")
        .bind(subject)
        .bind(email)
        .bind(1_i64)
        .bind(Utc::now())
        .bind(Utc::now())
        .execute(pool)
        .await
        .expect("create the google identity");
    }

    fn change_request(
        current: &str,
        next: &str,
    ) -> Result<Json<PasswordChangeRequest>, axum::extract::rejection::JsonRejection> {
        Ok(Json(PasswordChangeRequest {
            current_password: current.into(),
            new_password: next.into(),
        }))
    }

    async fn stored_password_hash(pool: &SqlitePool, account_id: Uuid) -> String {
        sqlx::query_scalar(
            "SELECT password_hash FROM identities WHERE account_id = ? AND provider = 'password'",
        )
        .bind(account_id.hyphenated())
        .fetch_one(pool)
        .await
        .expect("read the stored password hash")
    }

    /// The happy path, and the two things it must actually do: store a hash of the
    /// NEW password (not merely answer 204), and revoke EVERY session.
    ///
    /// The old password is checked to have stopped working. That is the assertion
    /// which fails if `set_password` were ever handed the old hash: a change that
    /// answered 204 while leaving the credential untouched would pass every
    /// status-code assertion and leave the account on the compromised password.
    #[tokio::test]
    async fn live_password_change_replaces_the_hash_and_kills_every_session() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();

        let victim = live_account(&pool).await;
        let old_password = "the-original-password";
        password_identity_for(&pool, victim.account_id, "change@example.com", old_password).await;

        // A SECOND session on the same account, so the assertion below is about
        // EVERY session rather than about the one cookie the change already had to
        // kill to be a change at all.
        let other_token =
            add_live_session(&pool, victim.account_id, Utc::now() + Duration::days(30)).await;

        let state = state_for(&pool);
        let response = call(change_password(
            State(state.clone()),
            peer(),
            cookie_header(&victim.token),
            change_request(old_password, "the-replacement-password"),
        ))
        .await;

        assert_eq!(
            response.status,
            StatusCode::NO_CONTENT,
            "a correct current password must be accepted: {}",
            response.body_text()
        );
        assert!(
            response.body.is_empty(),
            "api-spec.md: the change is a 204 with no body, got {}",
            response.body_text()
        );
        assert_eq!(
            response.cleared_token(),
            None,
            "the response must clear the cookie rather than hand back a live one"
        );
        assert_eq!(
            live_sessions(&pool, victim.account_id).await,
            0,
            "EVERY session must be revoked, including the caller's and the sibling built above"
        );

        // Asserted through the VERIFIER rather than by comparing hashes: Argon2id is
        // salted, so two hashes of one password differ, and a string comparison
        // could only ever say the column changed, never what it holds.
        let auth = auth_config().expect("auth config").clone();
        let stored = stored_password_hash(&pool, victim.account_id).await;
        assert!(
            identity::password::verify_password(
                auth.clone(),
                stored.clone(),
                "the-replacement-password".to_string(),
            )
            .await
            .expect("verify the new password"),
            "the new password must verify against the stored hash"
        );
        assert!(
            !identity::password::verify_password(auth, stored, old_password.to_string())
                .await
                .expect("verify the old password"),
            "THE OLD PASSWORD MUST STOP WORKING - this is the assertion that fails if the \
             handler stored nothing and only revoked sessions"
        );

        // The sibling token is dead at the RESOLVER, not merely marked. That is the
        // difference between a revocation and a column write.
        assert!(
            crate::routes::resolve_account_from_cookie(&pool, &cookie_header(&other_token))
                .await
                .is_err(),
            "a revoked sibling session must no longer resolve to an account"
        );

        db.close().await;
    }

    /// THE TAKEOVER GUARD. A valid session cookie with a WRONG current password
    /// must change nothing at all.
    ///
    /// This is the test that fails if the `verify_password` check is deleted: the
    /// endpoint would then be reachable with a stolen cookie alone, which is the
    /// exact takeover the handler's doc comment says it exists to prevent. Three
    /// things are asserted UNCHANGED rather than only the status, because a handler
    /// that verified and then proceeded anyway - or that revoked first and verified
    /// second - would still earn a 401 while having already done the damage.
    #[tokio::test]
    async fn live_password_change_refuses_a_wrong_current_password_and_changes_nothing() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();

        let victim = live_account(&pool).await;
        let old_password = "the-original-password";
        password_identity_for(&pool, victim.account_id, "guard@example.com", old_password).await;

        let before = stored_password_hash(&pool, victim.account_id).await;

        let state = state_for(&pool);
        let response = call(change_password(
            State(state.clone()),
            peer(),
            cookie_header(&victim.token),
            change_request("not-the-current-password", "the-replacement-password"),
        ))
        .await;

        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "a wrong current password is a 401, the same answer any refused credential earns: {}",
            response.body_text()
        );

        let after = stored_password_hash(&pool, victim.account_id).await;
        assert_eq!(
            before, after,
            "a refused change must not touch the stored hash, not even to re-hash the same password"
        );
        assert_eq!(
            live_sessions(&pool, victim.account_id).await,
            1,
            "a refused change must not revoke the caller's session - being wrong once is not a reason to sign someone out"
        );

        // The customer-visible half: a failed change leaves the account exactly as
        // it was found.
        let auth = auth_config().expect("auth config").clone();
        assert!(
            identity::password::verify_password(auth, after, old_password.to_string())
                .await
                .expect("verify the original password"),
            "the original password must still work after a refused change"
        );

        db.close().await;
    }

    /// A GOOGLE-ONLY account has no password to change, and is TOLD so rather than
    /// given a 500 or a misleading 401.
    ///
    /// The distinction is not cosmetic: a 401 would tell a signed-in Google user
    /// their credential was wrong when they have no credential, and the settings
    /// panel's next step for a 401 is the reset flow - which would CREATE a password.
    /// That is a different action from the one they asked for and one they did not
    /// consent to.
    #[tokio::test]
    async fn live_password_change_tells_a_google_only_account_it_has_no_password() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();

        let victim = live_account(&pool).await;
        google_identity_for(
            &pool,
            victim.account_id,
            "google-subject-1",
            "g@example.com",
        )
        .await;

        let state = state_for(&pool);
        let response = call(change_password(
            State(state.clone()),
            peer(),
            cookie_header(&victim.token),
            change_request("anything-at-all", "the-replacement-password"),
        ))
        .await;

        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "a validation failure, not a 401: there is no password to have got wrong. 422 is \
             what `ValidationFailed` carries (error.rs:116, pinned by its own test there): {}",
            response.body_text()
        );
        assert_eq!(
            response.json()["error"]["details"]["field"],
            "current_password",
            "the field named must be the one the caller can act on: {}",
            response.body_text()
        );
        assert_eq!(
            live_sessions(&pool, victim.account_id).await,
            1,
            "the session must survive a request that changed nothing"
        );

        db.close().await;
    }

    /// NO SESSION, NO BUDGET. An unauthenticated caller is refused BEFORE the
    /// account's rate-limit budget is consulted.
    ///
    /// The ordering is the reason this is pinned: the budget protects a SIGNED-IN
    /// account, so spending it before the cookie resolves would let an anonymous
    /// caller exhaust a stranger's login allowance and turn the throttle into the
    /// denial of service it exists to prevent.
    #[tokio::test]
    async fn live_password_change_refuses_an_anonymous_caller_without_spending_the_budget() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();

        let _lock = crate::routes::test_env::EnvLock::acquire();
        // A budget of ONE for the account path. If the refusal below spent it, the
        // signed-in attempt afterwards would be throttled and this test would see a
        // 429 where it expects a 204.
        let _caps = TestLimitsGuard::set(100, 1);
        let state = state_for(&pool);

        let victim = live_account(&pool).await;
        let old_password = "the-original-password";
        password_identity_for(&pool, victim.account_id, "anon@example.com", old_password).await;

        let anonymous = call(change_password(
            State(state.clone()),
            peer(),
            HeaderMap::new(),
            change_request(old_password, "the-replacement-password"),
        ))
        .await;
        assert_eq!(
            anonymous.status,
            StatusCode::UNAUTHORIZED,
            "no cookie is a 401 before anything else happens: {}",
            anonymous.body_text()
        );

        let recorded: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM auth_attempts WHERE kind = 'login' AND account_id = ?",
        )
        .bind(victim.account_id.hyphenated())
        .fetch_one(&pool)
        .await
        .expect("count the recorded attempts");
        assert_eq!(
            recorded, 0,
            "an unauthenticated request must not have written an attempt against the account"
        );

        // The budget is intact, so a signed-in caller still reaches the credential
        // check rather than the throttle. This is the assertion that fails if the
        // handler records the attempt before resolving the cookie.
        let signed_in = call(change_password(
            State(state.clone()),
            peer(),
            cookie_header(&victim.token),
            change_request(old_password, "the-replacement-password"),
        ))
        .await;
        assert_eq!(
            signed_in.status,
            StatusCode::NO_CONTENT,
            "the account's one attempt must still be available to the account itself: {}",
            signed_in.body_text()
        );

        db.close().await;
    }

    /// A body that does not match the shape is a VALIDATION failure naming the
    /// field, not a 404.
    ///
    /// This is why the handler takes `Result<Json<T>, JsonRejection>` instead of
    /// `Json<T>`: axum answers `NOT_FOUND` for a rejected body, and a 404 from a
    /// mounted route is indistinguishable from an unmounted one - which is exactly
    /// how `POST /auth/password-change` stayed invisible while it was being mounted.
    #[tokio::test]
    async fn live_password_change_reports_a_malformed_body_as_validation_not_404() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let victim = live_account(&pool).await;

        let state = state_for(&pool);
        let rejected = call(change_password(
            State(state.clone()),
            peer(),
            cookie_header(&victim.token),
            Err(
                axum::extract::rejection::JsonRejection::MissingJsonContentType(
                    axum::extract::rejection::MissingJsonContentType::default(),
                ),
            ),
        ))
        .await;

        assert_eq!(
            rejected.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "a malformed body is a 422, NEVER a 404 - that is the whole reason the handler \
             maps the rejection itself. axum's own answer for a rejected body is NOT_FOUND, \
             which is indistinguishable from an unmounted route: {}",
            rejected.body_text()
        );
        assert_eq!(
            rejected.json()["error"]["details"]["field"],
            "body",
            "the rejection must name the body, not a credential field: {}",
            rejected.body_text()
        );

        db.close().await;
    }

    /// The two body shapes the CI shape probe sends at this route, pinned here so
    /// the shell probe asserts something a Rust test already agrees with.
    ///
    /// REACHED THROUGH THE REAL ROUTER, not by handing the handler a `JsonRejection`:
    /// `JsonDataError::from_err` and `JsonSyntaxError::from_err` are `pub(crate)` in
    /// axum-core, so a test outside that crate can name the two variants and still
    /// not construct them. Sending the bytes is the better question anyway - it is
    /// what CI does, and it exercises the extractor order as well as the map.
    ///
    /// THE ORDER IS THE POINT. `Result<Json<T>, JsonRejection>` is the LAST extractor
    /// in the signature, so it runs last, and the body is mapped to a 422 before
    /// `resolve_account_from_cookie` is ever reached. These requests carry NO COOKIE,
    /// so a handler that authenticated first would answer 401 and this test would say
    /// so - which is why the assertion is a status equality and not a body match.
    ///
    /// Three different axum statuses collapse into one here, and that is the whole
    /// reason the handler takes the rejection `Result` instead of `Json<T>`: a body
    /// that is not JSON is axum's 400, a body whose shape does not match is its 422,
    /// and a missing content type is its 415. Left alone, one caller mistake would
    /// earn three statuses depending on how far it happened to get.
    #[tokio::test]
    async fn live_password_change_body_rejections_all_carry_the_documented_shape() {
        use axum::extract::connect_info::MockConnectInfo;
        use std::net::SocketAddr;

        let db = TestDb::new().await;

        // The real router, reached the way a caller reaches it. MockConnectInfo
        // supplies the peer address `change_password` extracts: without it that
        // extractor fails with 500, which would tell us nothing about the body map.
        let app = crate::routes::create_router(state_for(&db.pool)).layer(MockConnectInfo(
            SocketAddr::from(([203, 0, 113, 40], 44321)),
        ));

        async fn send(app: &axum::Router, body: &str) -> (StatusCode, String) {
            use axum::http::Request;
            use tower::ServiceExt;

            let request = Request::builder()
                .method("POST")
                .uri("/auth/password-change")
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .expect("the request must build");
            let response = app
                .clone()
                .oneshot(request)
                .await
                .expect("the router must respond");
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("the response body must be readable");
            (status, String::from_utf8_lossy(&bytes).into_owned())
        }

        // Both shapes CI sends, in one pass, so the assertions below cannot drift
        // apart from each other.
        let mut responses = Vec::new();
        for body in ["{}", "{,}"] {
            let (status, text) = send(&app, body).await;
            responses.push((body, status, text));
        }

        for (body, status, text) in &responses {
            assert_eq!(
                *status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "the body {body:?} must be a 422 whatever axum calls it: {text}"
            );
            assert!(
                text.contains("\"code\""),
                "every error carries a code (docs/error-model.md): {text}"
            );
            assert!(
                text.contains("\"request_id\""),
                "every error carries a request_id, and CI greps for this one: {text}"
            );
            assert!(
                !text.contains("not_found"),
                "a rejected body must not be reported as an unmounted route: {text}"
            );
        }

        // The rejection names the BODY and not a credential field, on both. That is
        // the field the settings page keys off to decide whether it is worth
        // re-reading what the customer typed, and naming `current_password` here
        // would send the panel looking at the wrong input.
        for (body, _, text) in &responses {
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(text)
                    .expect("the error body must be JSON")["error"]["details"]["field"],
                "body",
                "the rejection for {body:?} must name the body: {text}"
            );
        }

        db.close().await;
    }
    /// A password change kills outstanding reset links, not only sessions.
    ///
    /// THIS IS THE OTHER HALF OF "every session dies". A reset token is a credential
    /// that sets a password without knowing the current one, so a change that
    /// revoked the sessions and left the link alive would still have a live way back
    /// in - and the customer's reason for changing the password is usually that the
    /// old one, or a mailbox, was not theirs alone any more.
    ///
    /// The reset token is ISSUED through `identity::tokens::issue`, the production
    /// path, and redeemed through `identity::tokens::consume` afterwards: asserting
    /// the row is gone by counting rows would pass against a delete that cleared the
    /// wrong account, and what matters is whether the LINK still works.
    #[tokio::test]
    async fn live_password_change_kills_an_outstanding_reset_link() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account = live_account(&pool).await;
        let state = state_for(&pool);

        let address = "reset-me@example.com";
        let _ = password_identity_for(&pool, account.account_id, address, "old-password-123").await;

        let issued = crate::identity::tokens::issue(
            &pool,
            account.account_id,
            crate::identity::tokens::Purpose::Reset,
            chrono::Duration::hours(1),
            Utc::now(),
        )
        .await
        .expect("the reset token is issued");

        let changed = call(change_password(
            State(state.clone()),
            peer(),
            cookie_header(&account.token),
            change_request("old-password-123", "new-password-456"),
        ))
        .await;
        assert_eq!(
            changed.status,
            StatusCode::NO_CONTENT,
            "the change itself must succeed: {}",
            changed.body_text()
        );

        // The link is dead: redeeming it now fails. This is the assertion that fails
        // against a handler that only revoked the sessions.
        let redeemed = crate::identity::tokens::consume(
            &pool,
            &issued.raw,
            crate::identity::tokens::Purpose::Reset,
            Utc::now(),
        )
        .await;
        assert!(
            redeemed.is_err(),
            "a reset link issued before a password change must not redeem after it"
        );

        db.close().await;
    }
    // --- GET /auth/providers ------------------------------------------------

    /// The list reports the ACCOUNT'S OWN identity rows: a password-only account and
    /// a Google-only account get different answers over the same database.
    ///
    /// TWO ACCOUNTS IN ONE DATABASE is the point. With a single account, a query
    /// that forgot its `WHERE account_id = ?` would return the same one-element list
    /// and the test would pass while the endpoint reported every identity in the
    /// system to anyone holding a cookie.
    #[tokio::test]
    async fn live_providers_lists_only_this_accounts_identity_rows() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();

        let password_account = live_account(&pool).await;
        password_identity_for(
            &pool,
            password_account.account_id,
            "pw@example.com",
            "any-password",
        )
        .await;

        let google_account = live_account(&pool).await;
        google_identity_for(
            &pool,
            google_account.account_id,
            "google-subject-2",
            "g@example.com",
        )
        .await;

        let state = state_for(&pool);

        let mine = call(list_providers(
            State(state.clone()),
            cookie_header(&password_account.token),
        ))
        .await;
        assert_eq!(mine.status, StatusCode::OK);
        assert_eq!(
            mine.json()["providers"],
            serde_json::json!(["password"]),
            "a password-only account must be told exactly that, and must NOT see the other account's google row"
        );

        let theirs = call(list_providers(
            State(state.clone()),
            cookie_header(&google_account.token),
        ))
        .await;
        assert_eq!(theirs.status, StatusCode::OK);
        assert_eq!(
            theirs.json()["providers"],
            serde_json::json!(["google"]),
            "the google account must report google, which it cannot do if the query is not scoped by account"
        );

        db.close().await;
    }

    /// An account with BOTH sign-in methods reports both, once each.
    ///
    /// `SELECT DISTINCT` is load-bearing and this is the test that catches its
    /// removal: an account that has signed in twice through one provider holds two
    /// rows for it, and the panel would then be shown a repeated entry.
    #[tokio::test]
    async fn live_providers_reports_each_provider_once_for_an_account_that_has_both() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();

        let account = live_account(&pool).await;
        password_identity_for(
            &pool,
            account.account_id,
            "both@example.com",
            "any-password",
        )
        .await;
        google_identity_for(
            &pool,
            account.account_id,
            "google-subject-3",
            "both@example.com",
        )
        .await;

        let state = state_for(&pool);
        let response = call(list_providers(
            State(state.clone()),
            cookie_header(&account.token),
        ))
        .await;

        assert_eq!(response.status, StatusCode::OK);
        let mut providers: Vec<String> =
            serde_json::from_value(response.json()["providers"].clone())
                .expect("a list of strings");
        providers.sort();
        assert_eq!(
            providers,
            vec!["google".to_string(), "password".to_string()],
            "an account with both sign-in methods must be told both - and this is the answer \
             the pair of booleans the response deliberately does not use could not express: {}",
            response.body_text()
        );

        db.close().await;
    }

    /// No session is a 401 and NOT an empty list.
    ///
    /// An empty list would be a lie with a useful shape: the settings panel renders
    /// "Not linked" for every provider, which reads as "this account has no sign-in
    /// methods" rather than "you are not signed in".
    #[tokio::test]
    async fn live_providers_refuses_an_anonymous_caller_rather_than_answering_empty() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = state_for(&pool);

        let anonymous = call(list_providers(State(state.clone()), HeaderMap::new())).await;
        assert_eq!(
            anonymous.status,
            StatusCode::UNAUTHORIZED,
            "an unauthenticated caller must be refused, not handed an empty provider list: {}",
            anonymous.body_text()
        );

        db.close().await;
    }
    /// States the `[limits]` caps a test wants, without touching the config file, and
    /// clears them again when the test ends.
    ///
    /// A GUARD rather than a setter, because the override has to hold for every
    /// request the test sends: a cap is only observable by exhausting it, and
    /// exhausting one takes more than one call. The earlier take-on-read version
    /// covered exactly one request, so a three-request test against a cap of two was
    /// really testing a cap of two followed by the shipped twenty, and its assertions
    /// were about the timing of the override rather than about the cap.
    ///
    /// BOTH HALVES ARE REQUIRED, exactly as `test_env`'s docs argue for environment
    /// variables: `EnvLock` stops two tests from interleaving, and `Drop` is what
    /// makes the window close on the success path and on an assertion panic alike. A
    /// panic that left a cap of two behind would silently throttle every auth test
    /// that ran afterwards in the same process, which is the failure mode this crate
    /// reserves for guards.
    #[cfg(test)]
    struct TestLimitsGuard;

    #[cfg(test)]
    impl TestLimitsGuard {
        fn set(per_ip: u32, per_account: u32) -> Self {
            let mut current = crate::routes::auth::TEST_LIMITS_OVERRIDE
                .lock()
                .expect("the test override lock is never poisoned");
            let mut limits = AppConfig::load_from_file("../config/apikita.toml")
                .expect("the shipped config parses; every route module's fixture loads it")
                .limits;
            limits.login_per_hour_per_ip = per_ip;
            limits.login_per_hour_per_account = per_account;
            limits.signup_per_hour_per_ip = per_ip;
            limits.password_reset_per_hour_per_account = per_account;
            limits.verification_resend_per_hour = per_ip;
            *current = Some(limits);
            Self
        }
    }

    #[cfg(test)]
    impl Drop for TestLimitsGuard {
        fn drop(&mut self) {
            *crate::routes::auth::TEST_LIMITS_OVERRIDE
                .lock()
                .unwrap_or_else(|err| err.into_inner()) = None;
        }
    }

    // -----------------------------------------------------------------------
    // The account lifecycle: signup, Google, verification, reset, resend.
    //
    // THE HANDLERS THESE COVER HAD NO TEST THAT INVOKED THEM. `login`, `logout`,
    // `logout_all`, `change_password` and `list_providers` were driven; the six
    // below were reachable only through `MOUNTED` rows in `routes/mod.rs` that
    // assert a STATUS, never a behaviour. They are the account lifecycle and the
    // recovery path - signup mints the account, verify activates it, the reset
    // pair is how a locked-out customer gets back in, and resend is the mailbox
    // half of it - so an untested branch here is an untested way into an account.
    //
    // EVERY TEST BELOW ASSERTS A STATE CHANGE, NOT ONLY A STATUS. The neutral
    // replies these endpoints return on purpose (see `NEUTRAL_SIGNUP_REPLY`) mean
    // the status is deliberately uninformative, so a status-only test would pass
    // against a handler that did nothing at all.
    // -----------------------------------------------------------------------

    fn signup_json(
        email: &str,
        password: &str,
    ) -> Result<Json<SignupRequest>, axum::extract::rejection::JsonRejection> {
        Ok(Json(SignupRequest {
            email: email.to_string(),
            password: password.to_string(),
        }))
    }

    fn email_json(
        email: &str,
    ) -> Result<Json<EmailRequest>, axum::extract::rejection::JsonRejection> {
        Ok(Json(EmailRequest {
            email: email.to_string(),
        }))
    }

    fn token_json(
        token: &str,
    ) -> Result<Json<TokenRequest>, axum::extract::rejection::JsonRejection> {
        Ok(Json(TokenRequest {
            token: token.to_string(),
        }))
    }

    fn reset_json(
        token: &str,
        email: &str,
        password: &str,
    ) -> Result<Json<PasswordResetRequest>, axum::extract::rejection::JsonRejection> {
        Ok(Json(PasswordResetRequest {
            token: token.to_string(),
            email: email.to_string(),
            password: password.to_string(),
        }))
    }

    /// How many identity rows an address has, counted through `normalize_email`
    /// so the fixture's casing cannot make the count wrong.
    async fn identity_count(pool: &SqlitePool, email: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM identities WHERE email = ?")
            .bind(identity::accounts::normalize_email(email))
            .fetch_one(pool)
            .await
            .expect("counting the identities must work")
    }

    /// How many outstanding verification links an account holds.
    async fn verification_token_count(pool: &SqlitePool, account_id: Uuid) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM identity_tokens WHERE account_id = ? AND purpose = 'verification'",
        )
        .bind(account_id.hyphenated())
        .fetch_one(pool)
        .await
        .expect("counting the tokens must work")
    }

    /// A SIGNUP FOR AN ADDRESS THAT A GOOGLE-ONLY ACCOUNT HOLDS STILL CREATES THE
    /// PASSWORD ACCOUNT.
    ///
    /// The caller-visible half of the ordering rule in `account_for_email`. Signup
    /// reads "an account exists for this address" as "this address is already
    /// registered" and creates nothing, so when the address is held by a Google-only
    /// account, a password signup used to leave the person with no account they could
    /// sign in to and no error to explain it.
    ///
    /// This is the R1 state the schema exists to allow: an unverified password
    /// identity may coexist with a Google identity on one address. The rule that a
    /// GOOGLE row wins is what makes this endpoint behave, because "the address is
    /// taken" is then answered by the one identity that is always verified.
    #[tokio::test]
    async fn live_signup_over_a_google_only_account_still_creates_a_password_account() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = state_for(&pool);
        let _lock = crate::routes::test_env::EnvLock::acquire();
        let now = Utc::now();

        let email = "google-holds-this@example.com";

        // A GOOGLE-ONLY account owns the address: one identity, no password.
        let google_account = match crate::identity::accounts::resolve_google_sign_in(
            &pool,
            "google-holds-this-sub",
            email,
            now,
        )
        .await
        .expect("google")
        {
            crate::identity::accounts::GoogleSignIn::Created(id) => id,
            other => panic!("expected Created, got {other:?}"),
        };

        let response = call(signup(
            State(state.clone()),
            peer(),
            HeaderMap::new(),
            signup_json(email, "a-real-enough-password"),
        ))
        .await;
        assert_eq!(
            response.status,
            StatusCode::ACCEPTED,
            "signup answers neutrally whatever it did: {}",
            response.body_text()
        );

        // The password identity must exist, on an account of its own: the Google
        // account has no password, so adopting it would hand the caller nothing.
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT provider, account_id FROM identities WHERE email = ? ORDER BY provider",
        )
        .bind(email)
        .fetch_all(&pool)
        .await
        .expect("rows");

        assert_eq!(
            rows.len(),
            2,
            "a password identity now joins the google one"
        );
        let password_row = rows
            .iter()
            .find(|(provider, _)| provider == "password")
            .expect("the password identity must exist, or the signup did nothing");
        let password_account = Uuid::parse_str(&password_row.1).expect("a uuid");

        assert_ne!(
            password_account, google_account,
            "the new password identity belongs to a NEW account; the google account is \
             not reachable with a password and must not be adopted"
        );

        // And the account_for_email answer that signup consulted is the GOOGLE one,
        // which is the rule that made this deterministic.
        assert_eq!(
            crate::identity::accounts::account_for_email(&pool, email)
                .await
                .expect("lookup"),
            Some(google_account),
            "the verified identity is the one the address resolves to"
        );
    }

    /// A FRESH SIGNUP CREATES THE ACCOUNT, AN UNVERIFIED IDENTITY AND A LINK.
    ///
    /// Three separate creates, and all three matter: an account with no identity
    /// cannot sign in, and an identity with no token can never be verified, which
    /// would lock the customer out of their own account the moment they made it.
    #[tokio::test]
    async fn live_signup_creates_the_account_an_unverified_identity_and_a_link() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = state_for(&pool);
        let _lock = crate::routes::test_env::EnvLock::acquire();

        let email = "freshly-signed-up@example.com";
        let response = call(signup(
            State(state.clone()),
            peer(),
            HeaderMap::new(),
            signup_json(email, "a-real-enough-password"),
        ))
        .await;
        assert_eq!(
            response.status,
            StatusCode::ACCEPTED,
            "signup accepts: {}",
            response.body_text()
        );

        let account_id = crate::identity::accounts::account_for_email(&pool, email)
            .await
            .expect("the lookup must work")
            .expect("signup created the account");
        assert_eq!(
            identity_count(&pool, email).await,
            1,
            "exactly one identity"
        );
        assert_eq!(
            verification_token_count(&pool, account_id).await,
            1,
            "and exactly one link to verify it with"
        );

        let flag: i64 =
            sqlx::query_scalar("SELECT email_verified FROM identities WHERE account_id = ?")
                .bind(account_id.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("reading the flag must work");
        assert_eq!(
            flag, 0,
            "a fresh signup is NOT verified: nothing has proven the address yet, and an \
             account that started verified would make the whole mail loop decorative"
        );

        db.close().await;
    }

    /// A SIGNUP FOR AN ADDRESS THAT ALREADY EXISTS REPLIES IDENTICALLY.
    ///
    /// THE ENUMERATION GUARD, and the test is written around the ONE thing that
    /// makes it work: the two calls must take DIFFERENT BRANCHES. Comparing a
    /// re-signup against the FIRST signup does not do that - both take the same
    /// branch, so a difference the handler introduced in that branch is invisible
    /// on both sides. (That is not hypothetical: it is how the first version of
    /// this test was written, and a mutation proved it blind.)
    ///
    /// So the order is: create the address, then sign up for it AGAIN (the
    /// existing branch), then sign up for an address that does not exist (the
    /// create branch), and compare the last two.
    #[tokio::test]
    async fn live_signup_answers_a_known_address_exactly_like_an_unknown_one() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = state_for(&pool);
        let _lock = crate::routes::test_env::EnvLock::acquire();

        let known = "already-registered@example.com";
        let first = call(signup(
            State(state.clone()),
            peer(),
            HeaderMap::new(),
            signup_json(known, "a-real-enough-password"),
        ))
        .await;
        assert_eq!(first.status, StatusCode::ACCEPTED);

        let already = call(signup(
            State(state.clone()),
            peer(),
            HeaderMap::new(),
            signup_json(known, "a-different-password"),
        ))
        .await;
        let fresh = call(signup(
            State(state.clone()),
            peer(),
            HeaderMap::new(),
            signup_json("brand-new@example.com", "a-real-enough-password"),
        ))
        .await;

        assert_eq!(
            already.status, fresh.status,
            "a signup for an address that ALREADY EXISTS must be byte-identical to one \
             for an address that does not, or the difference IS an enumeration oracle"
        );
        assert_eq!(
            already.body_text(),
            fresh.body_text(),
            "and the BODIES must match too: a status that agrees while the message does \
             not still tells an attacker which addresses are registered"
        );
        assert_eq!(
            already.body_text(),
            first.body_text(),
            "the reply is the same one every signup gets, so it cannot be used to probe"
        );

        // AND THE SECOND SIGNUP DID NOT TOUCH THE FIRST ACCOUNT. A re-signup is
        // refused by taking the neutral branch, NOT by overwriting the credential:
        // otherwise anyone could take over an account by signing up for it again
        // with their own password.
        let identity = crate::identity::accounts::password_identity(&pool, known)
            .await
            .expect("the lookup must work")
            .expect("the fixture address has an identity");
        assert!(
            !identity.email_verified,
            "a re-signup must not verify the address either"
        );
        let stored = stored_password_hash(
            &pool,
            crate::identity::accounts::account_for_email(&pool, known)
                .await
                .expect("the lookup must work")
                .expect("the account exists"),
        )
        .await;
        assert!(
            identity::password::verify_password(
                auth_config_for_tests().expect("the shipped [auth] section parses"),
                stored,
                "a-real-enough-password".to_string(),
            )
            .await
            .expect("verification must work"),
            "the ORIGINAL password must still be the one on file: the neutral reply is \
             what protects the account, not the reply plus an overwrite"
        );

        // THE COST, which the reply equality above cannot see. The comment on the hash call in
        // `signup` says hashing happens BEFORE the existence check "so both branches pay the same
        // cost", and an early return would make "already registered" measurably faster than
        // "created" - an oracle in the timing domain even though the bodies match.
        //
        // THE PROBE MUST USE AN ALREADY-REGISTERED ADDRESS, and getting that wrong is easy: an
        // UNKNOWN address takes the create-branch and hashes under BOTH orderings, so probing one
        // cannot tell them apart. MEASURED: with the hash moved inside `existing.is_none()`, a probe
        // on an unknown address still saw one call and the assertion passed - it was testing that
        // hashing HAPPENS, not that it happens FIRST. `known` already has a password identity from
        // the first signup in this test, so it is the branch the ordering actually protects.
        let before = identity::password::hash_calls();
        let _ = call(signup(
            State(state.clone()),
            peer(),
            HeaderMap::new(),
            signup_json(known, "a-real-enough-password"),
        ))
        .await;
        let calls = identity::password::hash_calls().saturating_sub(before);
        assert!(
            calls >= 1,
            "a signup for an address that ALREADY has a password identity made {calls} calls to \
             `hash_password`. The hasher must run BEFORE the existence check so both branches pay \
             the same cost; an early return makes 'already registered' measurably faster than \
             'created', which is a membership oracle in the timing domain even though the bodies \
             match. See the comment on the hash call in `signup`."
        );

        db.close().await;
    }

    /// One error body with its `request_id` blanked, for comparing two replies that are allowed to
    /// differ only in that one field.
    fn without_request_id(body: &str) -> String {
        body.split("\"request_id\":\"")
            .enumerate()
            .map(|(i, part)| {
                if i == 0 {
                    part.to_string()
                } else {
                    // Drop everything up to the closing quote of the id.
                    match part.find('"') {
                        Some(at) => format!("<id>{}", &part[at..]),
                        None => part.to_string(),
                    }
                }
            })
            .collect::<Vec<_>>()
            .join("\"request_id\":\"")
    }

    /// LOGIN ANSWERS AN UNKNOWN ADDRESS AND A WRONG PASSWORD IDENTICALLY - and pays the same work.
    ///
    /// The repository tests the neutral answer for signup, for password reset and for resend
    /// verification. LOGIN WAS THE ONE THAT HAD NO SUCH TEST, and it is the endpoint where an oracle
    /// is most directly useful: a 401 that arrives faster for an unregistered address than for a
    /// registered one with a wrong password tells an attacker which addresses have accounts, without
    /// ever guessing a password.
    ///
    /// The code already takes measures against that - a throwaway hash on the missing-account path,
    /// one `Unauthenticated` for both credential failures, and the same 401 for a suspended account.
    /// MEASURED before this test existed: deleting the throwaway hash left all 38 `routes::auth`
    /// tests green, so the RESPONSE was pinned and the COST was not.
    ///
    /// A HASH IS COUNTED, NOT TIMED. Two endpoints promise that an unknown account does the same
    /// WORK as a known one - `signup` hashes before it checks existence, and `login` hashes a
    /// throwaway - and both are claims about cost that a body comparison cannot see. A wall-clock
    /// assertion would be flaky, and a flaky guard gets muted; "did the hasher run on this path" is
    /// deterministic and is the fact that distinguishes them. See `identity::password::hash_calls`.
    #[tokio::test]
    async fn live_login_answers_an_unknown_address_exactly_like_a_wrong_password() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = state_for(&pool);
        let _lock = crate::routes::test_env::EnvLock::acquire();

        let config = auth_config_for_tests().expect("the shipped [auth] section parses");
        let known = "has-a-password@example.com";
        crate::identity::accounts::create_password_account(
            &pool,
            known,
            &identity::password::hash_password(config, "the-real-password".to_string())
                .await
                .expect("hashing must work"),
            Utc::now(),
        )
        .await
        .expect("creating the fixture account must work");

        // (1) THE REPLY. The two failures a membership oracle would separate.
        let unknown = call(login(
            State(state.clone()),
            peer(),
            HeaderMap::new(),
            Ok(Json(LoginRequest {
                email: "no-such-address@example.com".into(),
                password: "any-password-at-all".into(),
            })),
        ))
        .await;
        let wrong = call(login(
            State(state.clone()),
            peer(),
            HeaderMap::new(),
            Ok(Json(LoginRequest {
                email: known.into(),
                password: "not-the-real-password".into(),
            })),
        ))
        .await;

        assert_eq!(
            unknown.status,
            StatusCode::UNAUTHORIZED,
            "an unknown address is a credential failure, not a distinct code"
        );
        assert_eq!(
            unknown.status, wrong.status,
            "an unknown address and a wrong password must answer the same STATUS, or the \
             difference says which addresses are registered"
        );
        // The `request_id` is deliberately FRESH per error (see `every_error_event_carries_a_fresh_
        // documented_request_id`), so it is the one field two replies may differ in. Blanking it is
        // what makes the comparison a statement about the ORACLE rather than about a correlation id.
        assert_eq!(
            without_request_id(&unknown.body_text()),
            without_request_id(&wrong.body_text()),
            "and the BODIES must match apart from the fresh request id: the same status with \
             different copy still tells an attacker which addresses are registered"
        );

        // (2) THE WORK, which the reply equality cannot see. Run one login for an address with no
        // identity and count the hasher invocations it caused.
        let before = identity::password::hash_calls();
        let _ = call(login(
            State(state.clone()),
            peer(),
            HeaderMap::new(),
            Ok(Json(LoginRequest {
                email: "another-unknown@example.com".into(),
                password: "any-password-at-all".into(),
            })),
        ))
        .await;
        let calls = identity::password::hash_calls().saturating_sub(before);

        assert!(
            calls >= 1,
            "a login for an address with no identity made {calls} calls to `hash_password`, so \
             the missing-account path returns early and does NOT pay for a hash. That makes 'no \
             such address' measurably faster than 'wrong password', which is a membership oracle \
             in the timing domain even though the bodies above match. See the throwaway hash in \
             the `login` body."
        );

        db.close().await;
    }

    /// A BODY THAT IS NOT AN ADDRESS OR NOT A PASSWORD IS REFUSED BY NAME.
    #[tokio::test]
    async fn live_signup_refuses_a_body_that_is_not_a_password_or_an_address() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = state_for(&pool);
        let _lock = crate::routes::test_env::EnvLock::acquire();

        let bad_address = call(signup(
            State(state.clone()),
            peer(),
            HeaderMap::new(),
            signup_json("not-an-address", "a-real-enough-password"),
        ))
        .await;
        assert_eq!(
            bad_address.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "a body that is not an address is a validation failure: {}",
            bad_address.body_text()
        );
        assert_eq!(
            bad_address.json()["error"]["details"]["field"],
            json!("email"),
            "and it names the field, so the page can point at the right input"
        );

        let weak_password = call(signup(
            State(state.clone()),
            peer(),
            HeaderMap::new(),
            signup_json("a-real-address@example.com", "short"),
        ))
        .await;
        assert_eq!(
            weak_password.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "a password under the floor is refused: {}",
            weak_password.body_text()
        );
        assert_eq!(
            weak_password.json()["error"]["details"]["field"],
            json!("password")
        );

        assert_eq!(
            identity_count(&pool, "a-real-address@example.com").await,
            0,
            "and a refused signup creates nothing"
        );

        db.close().await;
    }

    /// THE SIGNUP CAP RUNS BEFORE THE HASH, NOT AFTER IT.
    ///
    /// The handler's own comment: "hashing is the expensive part and a cap that
    /// runs after it is a cap that lets an attacker spend our CPU." A test that
    /// only asserted 429 would pass against a cap placed after the hash, so the
    /// assertion that matters is that the REFUSED address left no account behind.
    #[tokio::test]
    async fn live_signup_spends_the_per_address_cap_before_hashing() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = state_for(&pool);
        let _lock = crate::routes::test_env::EnvLock::acquire();
        let _limits = TestLimitsGuard::set(1, 100);

        let first = call(signup(
            State(state.clone()),
            peer(),
            HeaderMap::new(),
            signup_json("cap-one@example.com", "a-real-enough-password"),
        ))
        .await;
        assert_eq!(first.status, StatusCode::ACCEPTED);

        let second = call(signup(
            State(state.clone()),
            peer(),
            HeaderMap::new(),
            signup_json("cap-two@example.com", "a-real-enough-password"),
        ))
        .await;
        assert_eq!(
            second.status,
            StatusCode::TOO_MANY_REQUESTS,
            "the second from one address is refused: {}",
            second.body_text()
        );
        assert_eq!(
            identity_count(&pool, "cap-two@example.com").await,
            0,
            "a refused signup must not create the account, which is what proves the cap \
             ran before the work rather than after it"
        );

        db.close().await;
    }

    /// VERIFYING A LINK ACTIVATES THE IDENTITY AND SPENDS THE TOKEN.
    ///
    /// The whole point of signup is that an account is inert until this runs, so the
    /// test asserts the transition rather than the status: the flag goes 0 to 1, and
    /// the same link cannot be redeemed twice.
    #[tokio::test]
    async fn live_verify_email_marks_the_identity_and_refuses_a_second_redemption() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = state_for(&pool);
        let _lock = crate::routes::test_env::EnvLock::acquire();

        let email = "needs-verifying@example.com";
        let created = call(signup(
            State(state.clone()),
            peer(),
            HeaderMap::new(),
            signup_json(email, "a-real-enough-password"),
        ))
        .await;
        assert_eq!(created.status, StatusCode::ACCEPTED);

        let account_id = crate::identity::accounts::account_for_email(&pool, email)
            .await
            .expect("the lookup must work")
            .expect("signup created the account");

        // The mail is not sent in tests, and the row stores only a HASH - which is
        // the design working, and it means the fixture has to issue its own link.
        let issued = identity::tokens::issue(
            &pool,
            account_id,
            identity::tokens::Purpose::Verification,
            Duration::hours(1),
            Utc::now(),
        )
        .await
        .expect("issuing a verification token must work");

        let verified = call(verify_email(State(state.clone()), token_json(&issued.raw))).await;
        assert_eq!(
            verified.status,
            StatusCode::NO_CONTENT,
            "redeeming a real link verifies the address: {}",
            verified.body_text()
        );

        let flag: i64 =
            sqlx::query_scalar("SELECT email_verified FROM identities WHERE account_id = ?")
                .bind(account_id.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("reading the flag must work");
        assert_eq!(flag, 1, "THE ADDRESS IS NOW VERIFIED, which is the point");

        // A SECOND REDEMPTION IS REFUSED. `consume` marks and checks in one
        // statement, so this is not a race the test can lose.
        let again = call(verify_email(State(state.clone()), token_json(&issued.raw))).await;
        assert_eq!(
            again.status,
            StatusCode::UNAUTHORIZED,
            "a link is single-use: {}",
            again.body_text()
        );

        db.close().await;
    }

    /// AN UNKNOWN VERIFICATION TOKEN IS REFUSED WITH THE ONE ANSWER.
    #[tokio::test]
    async fn live_verify_email_refuses_a_token_that_was_never_issued() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = state_for(&pool);
        let _lock = crate::routes::test_env::EnvLock::acquire();

        let response = call(verify_email(
            State(state.clone()),
            token_json("apk_verify_definitely-not-a-real-token"),
        ))
        .await;
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "every failure reason is one answer: {}",
            response.body_text()
        );

        db.close().await;
    }

    /// A RESET LINK LETS ITS HOLDER SET A PASSWORD WITHOUT THE OLD ONE.
    #[tokio::test]
    async fn live_confirm_password_reset_sets_the_password_and_kills_every_session() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = state_for(&pool);
        let _lock = crate::routes::test_env::EnvLock::acquire();

        let email = "wants-a-reset@example.com";
        let account_id = crate::identity::accounts::create_password_account(
            &pool,
            email,
            &identity::password::hash_password(
                auth_config_for_tests().expect("the shipped [auth] section parses"),
                "the-old-password".to_string(),
            )
            .await
            .expect("hashing must work"),
            Utc::now(),
        )
        .await
        .expect("creating the fixture account must work");

        // A live session, so "every session dies" has something to kill.
        let token = add_live_session(&pool, account_id, Utc::now() + Duration::days(1)).await;
        assert_eq!(live_sessions(&pool, account_id).await, 1);

        let issued = identity::tokens::issue(
            &pool,
            account_id,
            identity::tokens::Purpose::Reset,
            Duration::minutes(30),
            Utc::now(),
        )
        .await
        .expect("issuing a reset token must work");

        let response = call(confirm_password_reset(
            State(state.clone()),
            reset_json(&issued.raw, email, "the-new-password"),
        ))
        .await;
        assert_eq!(
            response.status,
            StatusCode::NO_CONTENT,
            "a real reset completes: {}",
            response.body_text()
        );

        // THE NEW PASSWORD WORKS AND THE OLD ONE DOES NOT. Compared through the
        // VERIFIER, not by string: Argon2id is salted, so two hashes of one password
        // differ and a string comparison would fail on a correct implementation.
        let stored = stored_password_hash(&pool, account_id).await;
        let config = auth_config_for_tests().expect("the shipped [auth] section parses");
        assert!(
            identity::password::verify_password(
                config.clone(),
                stored.clone(),
                "the-new-password".to_string(),
            )
            .await
            .expect("verification must work"),
            "the password the reset set must verify"
        );
        assert!(
            !identity::password::verify_password(config, stored, "the-old-password".to_string(),)
                .await
                .expect("verification must work"),
            "and the password it replaced must NOT - otherwise the reset did nothing"
        );

        // EVERY SESSION DIED with the password.
        assert_eq!(
            live_sessions(&pool, account_id).await,
            0,
            "a reset must revoke the sessions that were opened with the old password"
        );
        assert!(
            crate::routes::resolve_account_from_cookie(&pool, &cookie_header(&token))
                .await
                .is_err(),
            "and the specific token from before the reset must no longer resolve"
        );

        // AND THE RESET DID NOT VERIFY THE ADDRESS. This is the assertion that was missing, and
        // its absence was MEASURED rather than suspected: making `set_password` also write
        // `email_verified = 1` - precisely the laundering `docs/architecture/identity.md`
        // forbids - left the whole suite green at 646 passed.
        //
        // Why it matters rather than being bookkeeping. The pre-hijacking defences gate on whether
        // the PASSWORD identity's address was proven and WHEN: `identities` is unique on
        // `(provider, email)`, so an unverified password row and a Google row may coexist on one
        // address, and the linking rule reads the flag to decide whether a sign-in may join them.
        // A reset that set it would let an attacker who controls the mailbox FOR AN INSTANT - a
        // shared inbox, a forwarded alias, a briefly-held address - convert that into a verified
        // identity that a later Google sign-in links to. The collision index is what makes the
        // state reachable; this flag is what makes it safe.
        //
        // `create_password_account` writes 0 and nothing in this path proves the address, so 0 is
        // the only correct value here.
        let verified: i64 = sqlx::query_scalar(
            "SELECT email_verified FROM identities WHERE account_id = ? AND provider = 'password'",
        )
        .bind(account_id.hyphenated())
        .fetch_one(&pool)
        .await
        .expect("the password identity must exist");
        assert_eq!(
            verified, 0,
            "a password reset must NOT mark the address verified: a reset proves the mailbox was \
             reachable, and the verified transition is a separate claim. Setting it here would let \
             a reset launder an unverified address into one a later Google sign-in links to"
        );

        db.close().await;
    }

    /// THE TOKEN AND THE ADDRESS MUST AGREE, AND A MISMATCH CHANGES NOTHING.
    ///
    /// This is the decision `confirm_password_reset` documents: "The token authorises
    /// one account and the body names an address on a different one. There is no
    /// correct merge, so nothing happens." The assertion is that the OTHER account's
    /// password is untouched, which a status-only check would miss.
    #[tokio::test]
    async fn live_confirm_password_reset_refuses_a_token_for_another_address() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = state_for(&pool);
        let _lock = crate::routes::test_env::EnvLock::acquire();

        let victim = "the-real-owner@example.com";
        let other = "somebody-else@example.com";
        let config = auth_config_for_tests().expect("the shipped [auth] section parses");
        let victim_id = crate::identity::accounts::create_password_account(
            &pool,
            victim,
            &identity::password::hash_password(config.clone(), "the-old-password".to_string())
                .await
                .expect("hashing must work"),
            Utc::now(),
        )
        .await
        .expect("creating the fixture account must work");
        crate::identity::accounts::create_password_account(
            &pool,
            other,
            &identity::password::hash_password(config.clone(), "the-other-password".to_string())
                .await
                .expect("hashing must work"),
            Utc::now(),
        )
        .await
        .expect("creating the second fixture account must work");

        let issued = identity::tokens::issue(
            &pool,
            victim_id,
            identity::tokens::Purpose::Reset,
            Duration::minutes(30),
            Utc::now(),
        )
        .await
        .expect("issuing a reset token must work");

        let response = call(confirm_password_reset(
            State(state.clone()),
            reset_json(&issued.raw, other, "a-stolen-new-password"),
        ))
        .await;
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "a token for one address used on another is refused: {}",
            response.body_text()
        );

        // THE OTHER ACCOUNT IS UNCHANGED. This is load-bearing: a handler that
        // verified the token, set the password and THEN noticed the mismatch would
        // answer 401 with the damage already done.
        let other_id = crate::identity::accounts::account_for_email(&pool, other)
            .await
            .expect("the lookup must work")
            .expect("the second account exists");
        let other_hash = stored_password_hash(&pool, other_id).await;
        assert!(
            identity::password::verify_password(
                config,
                other_hash,
                "the-other-password".to_string(),
            )
            .await
            .expect("verification must work"),
            "the address named in the body must keep its own password"
        );

        db.close().await;
    }

    /// THE RESET REQUEST ANSWERS THE SAME WAY WHETHER OR NOT THE ADDRESS EXISTS.
    ///
    /// Same rule as signup, and the same failure mode: an asymmetric reply turns the
    /// endpoint into "which addresses have accounts here?".
    #[tokio::test]
    async fn live_request_password_reset_answers_known_and_unknown_alike() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = state_for(&pool);
        let _lock = crate::routes::test_env::EnvLock::acquire();

        let known = "has-an-account@example.com";
        let config = auth_config_for_tests().expect("the shipped [auth] section parses");
        crate::identity::accounts::create_password_account(
            &pool,
            known,
            &identity::password::hash_password(config, "a-real-enough-password".to_string())
                .await
                .expect("hashing must work"),
            Utc::now(),
        )
        .await
        .expect("creating the fixture account must work");

        let for_known = call(request_password_reset(
            State(state.clone()),
            peer(),
            HeaderMap::new(),
            email_json(known),
        ))
        .await;
        let for_unknown = call(request_password_reset(
            State(state.clone()),
            peer(),
            HeaderMap::new(),
            email_json("nobody-here@example.com"),
        ))
        .await;

        assert_eq!(
            for_known.status,
            StatusCode::ACCEPTED,
            "the reply is neutral and positive: {}",
            for_known.body_text()
        );
        assert_eq!(
            for_known.body_text(),
            for_unknown.body_text(),
            "an address WITH an account must get byte-identical copy to one WITHOUT, \
             or the reply says which addresses are registered"
        );

        db.close().await;
    }

    /// A RESEND GOES ONLY TO AN UNVERIFIED ADDRESS.
    ///
    /// "Only an account that EXISTS and is still UNVERIFIED gets a mail. A verified
    /// address asking again is not an error and does not get a link." The observable
    /// is a NEW TOKEN ROW: the neutral reply means the status cannot tell the
    /// difference, so the table is what proves the branch was taken.
    #[tokio::test]
    async fn live_resend_verification_issues_a_link_only_for_an_unverified_address() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = state_for(&pool);
        let _lock = crate::routes::test_env::EnvLock::acquire();

        let unverified = "not-yet-verified@example.com";
        let config = auth_config_for_tests().expect("the shipped [auth] section parses");
        let hash = identity::password::hash_password(config, "a-real-enough-password".to_string())
            .await
            .expect("hashing must work");
        let pending = crate::identity::accounts::create_password_account(
            &pool,
            unverified,
            &hash,
            Utc::now(),
        )
        .await
        .expect("creating the fixture account must work");

        let before = verification_token_count(&pool, pending).await;
        let response = call(resend_verification(
            State(state.clone()),
            peer(),
            HeaderMap::new(),
            email_json(unverified),
        ))
        .await;
        assert_eq!(response.status, StatusCode::ACCEPTED);
        assert_eq!(
            verification_token_count(&pool, pending).await,
            before + 1,
            "an UNVERIFIED address gets a fresh link"
        );

        // AND A VERIFIED ONE DOES NOT. The account is marked verified through the
        // production helper, then asked again.
        let identity = crate::identity::accounts::password_identity(&pool, unverified)
            .await
            .expect("the lookup must work")
            .expect("the fixture has an identity");
        crate::identity::accounts::mark_verified(&pool, identity.identity_id, Utc::now())
            .await
            .expect("marking verified must work");

        let settled = verification_token_count(&pool, pending).await;
        let response = call(resend_verification(
            State(state.clone()),
            peer(),
            HeaderMap::new(),
            email_json(unverified),
        ))
        .await;
        assert_eq!(
            response.status,
            StatusCode::ACCEPTED,
            "the reply stays neutral even when nothing is sent"
        );
        assert_eq!(
            verification_token_count(&pool, pending).await,
            settled,
            "a VERIFIED address gets NO new link: a working verification link for a \
             proven address is a credential with nothing to do"
        );

        db.close().await;
    }

    /// A RESEND FOR AN UNKNOWN ADDRESS IS NEUTRAL AND WRITES NOTHING.
    #[tokio::test]
    async fn live_resend_verification_answers_an_unknown_address_neutrally() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = state_for(&pool);
        let _lock = crate::routes::test_env::EnvLock::acquire();

        let response = call(resend_verification(
            State(state.clone()),
            peer(),
            HeaderMap::new(),
            email_json("never-registered@example.com"),
        ))
        .await;
        assert_eq!(
            response.status,
            StatusCode::ACCEPTED,
            "an unknown address is not an error: {}",
            response.body_text()
        );
        assert_eq!(
            identity_count(&pool, "never-registered@example.com").await,
            0,
            "and nothing is created for it"
        );

        db.close().await;
    }

    /// A TOKEN THAT IS NOT FROM GOOGLE IS REFUSED, AND IT COSTS NO BUDGET.
    ///
    /// THE ORDERING IS THE ASSERTION. `google_sign_in` verifies the token BEFORE it
    /// records an attempt, so a caller who cannot possibly sign in does not spend the
    /// address's rate-limit budget - and, more expensively, does not cost the server a
    /// round trip to Google's JWKS endpoint. A handler that recorded first would pass
    /// a status-only test and fail this one.
    ///
    /// HONEST LIMIT: the happy path of this handler CANNOT be driven from a test.
    /// `identity::google::verify_id_token` offers no seam to inject claims - it always
    /// fetches Google's key set - so no test here covers "a valid token creates or
    /// finds the account and opens a session". What is covered is every branch that
    /// runs before the signature is checked, which is all the code that does not depend
    /// on live Google.
    #[tokio::test]
    async fn live_google_sign_in_refuses_a_forged_token_without_spending_the_budget() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = state_for(&pool);
        let _lock = crate::routes::test_env::EnvLock::acquire();

        // A budget of ONE per address, so a single recorded attempt is detectable.
        let _limits = TestLimitsGuard::set(1, 1);

        let response = call(google_sign_in(
            State(state.clone()),
            peer(),
            HeaderMap::new(),
            Ok(Json(GoogleSignInRequest {
                id_token: "not-a-jwt".to_string(),
            })),
        ))
        .await;
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "a caller's rubbish token is 401, not 500: {}",
            response.body_text()
        );

        let spent: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM auth_attempts WHERE kind = 'login' AND account_id IS NULL",
        )
        .fetch_one(&pool)
        .await
        .expect("counting the attempts must work");
        assert_eq!(
            spent, 0,
            "a token that was never verified must not have spent the login budget: the \
             check runs BEFORE the counter, so a forged token cannot exhaust a real \
             user's allowance"
        );

        db.close().await;
    }

    /// A FORGED TOKEN LEAVES THE ROUTER AS 401, WITH THE DOCUMENTED SHAPE.
    ///
    /// This runs through the REAL router on purpose. `google_sign_in` is the one auth
    /// handler whose failure the caller cannot act on (`verify_id_token` refuses before
    /// the account is even looked up), and the contract pinned here is the HTTP
    /// surface: a credential error is 401, it carries `code` and `request_id` the way
    /// `docs/error-model.md` promises, and it is not a 500.
    ///
    /// HONEST LIMIT, and it is a real one: the UNCONFIGURED-CLIENT-ID branch is not
    /// reachable from a test either. `auth_config()` (`:282`) reads the PROCESS-WIDE
    /// `APP_CONFIG` `OnceLock`, not `state.config`, so a test cannot blank it without a
    /// seam that does not exist. The coverage of this handler is its refusal path,
    /// which is the branch a stranger can reach.
    #[tokio::test]
    async fn live_google_sign_in_refuses_a_forged_token_through_the_router() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let _lock = crate::routes::test_env::EnvLock::acquire();

        use axum::body::Body;
        use axum::http::Request;
        use std::net::SocketAddr;
        use tower::ServiceExt;

        let app = crate::routes::create_router(state_for(&pool)).layer(
            axum::extract::connect_info::MockConnectInfo(SocketAddr::from((
                [203, 0, 113, 40],
                44321,
            ))),
        );
        let request = Request::builder()
            .method("POST")
            .uri("/auth/google")
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"id_token":"not-a-jwt"}"#))
            .expect("the request must build");
        let response = app.oneshot(request).await.expect("the router must respond");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("the body must be readable");
        let body = String::from_utf8_lossy(&bytes).into_owned();

        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "a token that is not from Google is a credential error, and the router must \
             not turn it into a 500: {body}"
        );
        for required in ["\"code\"", "\"request_id\""] {
            assert!(
                body.contains(required),
                "the error must carry {required} the way docs/error-model.md promises: {body}"
            );
        }

        db.close().await;
    }
}
