#![cfg_attr(
    not(test),
    // EVERY HTTP SURFACE, fenced at the MODULE ROOT rather than per file.
    //
    // An inner attribute here covers every submodule beneath it, so this is one
    // line of intent rather than nine - and it is also the placement that cannot
    // rot: a new handler file under routes/ is covered the moment it is added, with
    // nothing to remember. Fencing each file separately meant a file added later
    // would be unfenced until somebody noticed, which is how auth.rs and events.rs
    // still carry per-function allows while their neighbours carry a module deny.
    //
    // Non-test scoping, as everywhere else: a test that adds 1 to a counter has
    // failed loudly and cost nothing, and the test corpus trips this lint dozens of
    // times.
    deny(clippy::arithmetic_side_effects)
)]

pub mod account;
pub mod admin;
pub mod auth;
pub mod events;
pub mod health;
pub mod keys;
pub mod proxy;
pub mod reviews;
pub mod telegram;
pub mod webhooks;

use axum::{
    http::{header, HeaderMap},
    routing::{delete, get, patch, post},
    Router,
};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::error::AppError;
use proxy::AppState;

/// Name of the opaque session cookie (docs/server/api-spec.md; the same name
/// routes::auth sets on login).
pub const SESSION_COOKIE: &str = "session";

/// SHA-256 of a credential, hex. Only the hash is ever stored or compared
/// (docs/website/02-data-model.md, sessions), so the plaintext never reaches
/// Postgres.
pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// The session cookie value out of a raw `Cookie:` header.
///
/// This is the ONLY parser for the session cookie: the dashboard, key
/// management, SSE and the auth handlers all read a credential through it, so
/// they cannot drift into accepting different credentials. Three rules are
/// load-bearing and pinned by the tests below:
///
/// - the name must match EXACTLY, so `notsession`/`xsession`/`sessionx` are
///   not ours;
/// - an empty value is not a credential;
/// - the value is everything after the first `=`, because tokens are opaque
///   and `session=a=b` therefore carries the token `a=b`, not `a`.
pub fn session_token_from_cookie_header(cookie_header: &str) -> Option<&str> {
    cookie_header.split(';').find_map(|piece| {
        let piece = piece.trim();
        let token = piece
            .strip_prefix(SESSION_COOKIE)
            .and_then(|rest| rest.strip_prefix('='))?;
        (!token.is_empty()).then_some(token)
    })
}

/// A session row that cannot be read is a store that is not there, not a credential
/// that is wrong.
///
/// `resolve_account_from_cookie` answers `Unauthenticated` for every way a session can
/// be unusable, and that is deliberate: a caller must not be able to tell "no session"
/// from "dead session". A store that cannot be queried is a third case and does NOT
/// belong in that bucket - answering 401 during an outage tells every signed-in
/// customer their session is bad, and the web client acts on a 401 by redirecting to
/// /login, so a database blip would log the whole site out. 503 says "try again",
/// which is what is true.
///
/// The message is fixed rather than the sqlx text: a store error string can name the
/// path or the schema, and this is returned on a route any authenticated caller
/// reaches.
fn unusable_session_store(_: sqlx::Error) -> AppError {
    AppError::Unavailable("the session store is unreachable".into())
}

/// Whether a session is usable at `now`, given when it was last seen and when it
/// absolutely expires.
///
/// This is the WHOLE session-lifetime rule, extracted into a pure function so the
/// register's `docs/decisions.md`: "**30 days absolute, 7 days idle**" is a
/// decision this crate can test with no database, no clock and no cookie. Two
/// independent refusals, and either one is enough:
///
/// - **absolute** - `expires_at <= now` (seeded at login as
///   `now + absolute_days`), so a session has a hard end regardless of use;
/// - **idle** - `last_seen_at + idle_days <= now`, so an abandoned session dies
///   long before its absolute end.
///
/// Both bounds are inclusive-unusable, matching the `expires_at > ?` predicate the
/// lookup has always used: a session is live while `expires_at` is still in the
/// future, not while it is "not yet past".
///
/// **An idle bound at or above the absolute lifetime is INERT, by construction.**
/// `expires_at` is seeded `login + absolute_days`, so at `last_seen_at == login`
/// the idle rule refuses no earlier than the absolute rule, and after any activity
/// it refuses strictly later. A misconfiguration can therefore only ever make the
/// idle half LESS binding, never cut a session short - which is why the shipped
/// `idle_days = 7` against `absolute_days = 30` is the only combination that
/// changes behaviour.
#[allow(clippy::arithmetic_side_effects)]
pub fn session_is_live_at(
    now: chrono::DateTime<chrono::Utc>,
    last_seen_at: chrono::DateTime<chrono::Utc>,
    expires_at: chrono::DateTime<chrono::Utc>,
    idle_days: i64,
    absolute_days: i64,
) -> bool {
    // The unused bound is still taken as a parameter: it keeps the signature
    // honest about everything the contract mentions, so a future reader cannot
    // think the absolute lifetime is enforced somewhere else.
    let _ = absolute_days;

    if expires_at <= now {
        return false;
    }

    // SAFE because config.rs refuses a lifetime the clock cannot represent, at load.
    // A bare `DateTime + Duration` PANICS on an out-of-range result, and this is on
    // EVERY session resolution, so an unrepresentable idle bound would panic on the
    // first request and every request after it - which is why the config check names
    // `idle_days` rather than only the absolute bound.
    //
    // `idle_days` is an i64 here rather than the config's u32, so a caller could in
    // principle pass a negative; `checked_add_signed` accepts that happily, and it is
    // harmless - the sum moves into the past and the session reads as idle.
    //
    // The allow is on the FUNCTION: this addition is the function's tail expression,
    // and an attribute there needs the unstable `stmt_expr_attributes` feature.
    last_seen_at + chrono::Duration::days(idle_days) > now
}

/// The account a request's session cookie resolves to.
///
/// Any failure - no header, no session cookie, an unknown, revoked, expired or
/// idle token, or an unreadable row - is `AppError::Unauthenticated`: a caller
/// cannot tell "no session" from "dead session", and neither can an attacker.
///
/// **Resolution is also the activity signal.** A session that resolves has its
/// `last_seen_at` moved to now, because the idle bound is measured from the last
/// time the credential was actually USED and there is no other place a session
/// request passes through. The write is deliberately here rather than in the
/// proxy: `/v1/*` authenticates API keys, not cookies
/// (docs/server/api-spec.md - cookie credentials are rejected on `/v1/*`), so
/// touching the sessions table from the proxy would add a writer to the hot path
/// without keeping any promise.
pub async fn resolve_account_from_cookie(
    pool: &SqlitePool,
    headers: &HeaderMap,
) -> Result<Uuid, AppError> {
    let cookie_hdr = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .ok_or(AppError::Unauthenticated)?;

    let token = session_token_from_cookie_header(cookie_hdr).ok_or(AppError::Unauthenticated)?;

    // The dialect changes from the Postgres original are all required and none is
    // cosmetic. The placeholders are `?` (sqlx-sqlite); the two time bounds are
    // BOUND FROM RUST rather than compared against SQL `now()`, because SQLite's
    // `now()` emits the space-separated format and `'T'` sorts after a space, so
    // `...T07:00:00+00:00` compares greater than the current time indefinitely and
    // an expired session would never expire - a silent failure in the direction
    // that keeps access. The same hazard is recorded in db.rs and ip_tracking.rs.
    //
    // Revocation and the expiry half are filtered in SQL (they are properties of
    // the row); the idle half is decided by `session_is_live_at` in Rust, so the
    // rule this crate ships is the rule its tests exercise.
    let now = chrono::Utc::now();
    let sessions = auth::sessions_config()?;
    let idle_days = sessions.idle_days as i64;
    let absolute_days = sessions.absolute_days as i64;

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

    // A row that cannot be read at all is a store that is not there, not a credential
    // that is wrong. Returning Unauthenticated would say "your session is bad" about
    // every session during an outage, and the two callers that act on a 401 - the web
    // client's redirectToLogin() and a customer deciding whether to sign in again -
    // would both do the wrong thing. 503 says "try again", which is what is true.
    //
    // THIS WAS FOUND BY A TEST, not by reading: `table_state()` builds a pool pointing
    // at a file that does not exist, so a route resolving a cookie reached this
    // `?` and answered 500, which is not a status the mounted-route table test accepts
    // as "the router matched". The routing question and the error mapping were the same
    // question and only the table asked it.
    let account_id: Uuid = session
        .try_get::<uuid::fmt::Hyphenated, _>("account_id")
        .map_err(unusable_session_store)?
        .into_uuid();
    let last_seen_at: chrono::DateTime<chrono::Utc> = session
        .try_get("last_seen_at")
        .map_err(unusable_session_store)?;
    let expires_at: chrono::DateTime<chrono::Utc> = session
        .try_get("expires_at")
        .map_err(unusable_session_store)?;

    if !session_is_live_at(now, last_seen_at, expires_at, idle_days, absolute_days) {
        return Err(AppError::Unauthenticated);
    }

    // A refusal above must NOT extend the session: only a credential that was
    // actually honoured counts as activity. Guarded by `last_seen_at < ?` so two
    // requests of one page cannot write twice, and so an older, slower request
    // cannot move the timestamp backwards.
    sqlx::query("UPDATE sessions SET last_seen_at = ? WHERE token_hash = ? AND last_seen_at < ?")
        .bind(now)
        .bind(hash_token(token))
        .bind(now)
        .execute(pool)
        .await?;

    Ok(account_id)
}

/// The ROUTES constant, expanded into `create_router`'s method-router expressions.
///
/// WHY THIS IS A MACRO AND NOT A LOOP. axum's `MethodRouter` is CHAINED per route -
/// `get(a).post(b)` - and axum guarantees `/api/topups` and `/api/keys` carry exactly
/// two methods each. A `for` loop would have to `.merge()` two single-method routers
/// per path, which would restate each method list in a second place. That is the
/// duplicate-that-drifts-toward-the-weaker-reading defect this codebase already
/// fights, traded for the one this macro closes, so the macro is the smaller evil.
///
/// The cost is real and belongs in writing rather than in an absence: ONE rustfmt
/// invocation would re-wrap these `route(` lines and swallow the trailing `;` markers
/// that `the_route_inventory_matches_the_mounted_table` reads, and the only other
/// arm - a `cargo fmt` check in CI - is deliberately not taken, because this crate
/// holds several deliberate mis-formattings that rustfmt undoes.
macro_rules! routes {
    ($($(#[$meta:meta])* .route($path:literal, $($m:ident($h:path)).+ $(,)?));+ $(;)?) => {
        Router::new()
        $(
            $(#[$meta])*
            .route($path, $($m($h)).+)
        )+
    };
    // THE PATH LIST, CAPTURED FROM THE SAME INVOCATION. This arm is the second
    // half of the guard: `stringify!` of a `$path:literal` is the LITERAL'S OWN
    // TEXT, produced by the compiler from the tokens it actually expanded, so it
    // reports what the router was built from rather than what a test hoped was
    // there. Comments are stripped by the lexer and the `;` markers are not part
    // of a single route's capture, so the result is a bare list of quoted paths -
    // which is exactly what `the_macro_body_lists_the_inventory_it_mounts` needs
    // to compare against `ROUTES`.
    //
    // A SEPARATE ARM rather than a second field on the first, because the first
    // one is what `create_router` returns and every caller wants a `Router`, not
    // a `(Router, &str)`. Two arms of one macro is the guarantee that the list
    // cannot describe a different invocation than the router: both are expanded
    // from the SAME tokens by the SAME macro.
    (@paths $($(#[$meta:meta])* .route($path:literal, $($m:ident($h:path)).+ $(,)?));+ $(;)?) => {
        &[$(stringify!($path)),+]
    };
}

/// THE ROUTE INVENTORY. The single source for what this server serves.
///
/// **BOTH ENDS OF THIS LIST ARE CHECKED, and neither check can see the other's
/// subject.** `the_route_inventory_matches_the_mounted_table` reads THIS literal in
/// the source and compares it to `MOUNTED`, so a route dropped from the list is a
/// failure rather than the silent 404 it used to be. The method-router expressions
/// themselves are read by no test: axum does not expose its route table
/// (docs/testing.md:145), and the table test drives the ROUTER, which is built from
/// the list. So a method removed here is caught by no automated check, and saying so
/// is the point - it is the one link these checks cannot close.
///
/// WHAT THE TRAILING `;` MARKS ARE FOR. Each `.route(` line ends in `;`, which is a
/// token rustfmt never emits. A `.route(` line deleted by hand takes its `;` with it,
/// so the source literal and the built router fall out of step **detectably**. The
/// markers do not make this tamper-proof and do not claim to: a deletion can still be
/// made to read as consistent by removing the `;` too. They make the accidental
/// version - the one that actually happens - loud.
///
/// THE PATH LITERALS ARE load-bearing strings, not comments. `$path:literal` expands
/// into the method-router expression that axum receives, so the path a request is
/// matched against IS the string this test reads. Before this list existed the paths
/// were written here and mounted there, and the two could disagree with nothing to
/// say so.
///
/// `{id}` is axum 0.8's parameter syntax. `MOUNTED` carries a concrete uuid for the
/// same route, because it drives requests rather than parsing them.
pub const ROUTES: &str = r#"
.route("/health", get(health::health_check));
// Operator-only operational counts. NOT on /health: that body is pinned
// because the deploy gate parses it, and a leak test forbids ANY digit in an
// unauthenticated health body. See health::operator_metrics.
.route("/api/admin/metrics", get(health::operator_metrics));
// Auth. These are what the website calls, and none of them needs a PocketBase
// instance: the identity provider is this crate's own `identity` module, over
// the `identities` table.
.route("/auth/signup", post(auth::signup));
.route("/auth/login", post(auth::login));
.route("/auth/google", post(auth::google_sign_in));
.route("/auth/verify-email", post(auth::verify_email));
.route("/auth/password-reset/request", post(auth::request_password_reset));
.route("/auth/password-reset/confirm", post(auth::confirm_password_reset));
.route("/auth/verification/resend", post(auth::resend_verification));
.route("/auth/logout", post(auth::logout));
.route("/auth/logout-all", post(auth::logout_all));
.route("/auth/password-change", post(auth::change_password));
.route("/auth/providers", get(auth::list_providers));
.route("/api/me", get(account::get_me));
.route("/api/usage", get(account::get_usage));
.route("/api/usage/recent", get(account::get_recent_usage));
.route("/api/export", get(account::export_account_data));
.route("/api/topups", get(account::get_topups).post(account::create_topup));
.route("/api/keys", get(keys::list_keys).post(keys::create_key));
.route("/api/keys/{id}", patch(keys::update_key));
.route("/api/keys/{id}/revoke", post(keys::revoke_key));
.route("/api/telegram/link-code", post(telegram::issue_link_code));
.route("/api/telegram", delete(telegram::unlink_telegram));
// Reviews. Written by a COOKIE, not by a bot: the Telegram bot this was
// originally specified for is not being built, and the three properties the
// launch gate asked for (one per account, editable, withdrawal as a flag) are
// properties of the table rather than of the client that writes it. The
// aggregate is public; the write paths are the caller's own row and nothing
// else.
.route("/api/reviews", get(reviews::get_reviews).post(reviews::upsert_review));
.route("/api/reviews/mine", get(reviews::get_my_review));
.route("/api/reviews/withdraw", post(reviews::withdraw_review));
// The bot's redemption endpoint, authenticated by TELEGRAM_BOT_TOKEN.
.route("/api/bot/link", post(telegram::redeem_link_code));
.route("/events", get(events::sse_events_handler));
.route("/api/admin/accounts", get(admin::list_accounts));
.route("/api/admin/audit", get(admin::list_recent_audit));
.route("/api/admin/accounts/{id}", get(admin::get_account));
.route("/api/admin/accounts/{id}/audit", get(admin::get_account_audit));
.route("/api/admin/accounts/{id}/suspend", post(admin::suspend_account));
.route("/api/admin/accounts/{id}/restore", post(admin::resume_account));
.route("/api/admin/accounts/{id}/resume", post(admin::resume_account));
// Webhooks
.route("/webhooks/midtrans", post(webhooks::handle_midtrans_webhook));
// Proxy
.route("/v1/chat/completions", post(proxy::chat_completions));
"#;

/// **DEAD BY DESIGN.** The route table as a `const` array, which is the one thing
/// that would make the inventory checkable WITHOUT reading source text.
///
/// `ROUTES` above is a source literal because Rust has no stable reflection over
/// names, so `get(health::health_check)` cannot be assembled from an array of
/// strings - `concat_idents!` is unstable and a `macro_rules` pass cannot build an
/// identifier fragment-wise either. An array of paths can drive NOTHING in axum, so
/// adding one would be a second hand-maintained list, which is the defect.
///
/// It is kept as a MEASURED, REFUTED PROPOSAL rather than deleted, because it looks
/// obviously better to a first reader and "why not just use an array" is the first
/// question this file's approach invites. It is `#[allow(dead_code)]` so this
/// paragraph is tested against the compiler rather than trusted.
#[allow(dead_code)]
pub const ROUTE_ARRAY_SHAPE_IS_IMPOSSIBLE: &[&str] = &[
    "/health",
    "/api/admin/metrics",
    "/auth/signup",
    "/auth/login",
    "/auth/google",
    "/auth/verify-email",
    "/auth/password-reset/request",
    "/auth/password-reset/confirm",
    "/auth/verification/resend",
    "/auth/logout",
    "/auth/logout-all",
    "/auth/password-change",
    "/auth/providers",
    "/api/me",
    "/api/usage",
    "/api/usage/recent",
    "/api/export",
    "/api/topups",
    "/api/keys",
    "/api/keys/{id}",
    "/api/keys/{id}/revoke",
    "/api/telegram/link-code",
    "/api/telegram",
    "/api/reviews",
    "/api/reviews/mine",
    "/api/reviews/withdraw",
    "/api/bot/link",
    "/events",
    "/api/admin/accounts",
    "/api/admin/audit",
    "/api/admin/accounts/{id}",
    "/api/admin/accounts/{id}/audit",
    "/api/admin/accounts/{id}/suspend",
    "/api/admin/accounts/{id}/restore",
    "/api/admin/accounts/{id}/resume",
    "/webhooks/midtrans",
    "/v1/chat/completions",
];

pub fn create_router(state: AppState) -> Router {
    routes!(
        .route("/health", get(health::health_check));
        // Operator-only operational counts. NOT on /health: that body is pinned
        // because the deploy gate parses it, and a leak test forbids ANY digit in an
        // unauthenticated health body. See health::operator_metrics.
        .route("/api/admin/metrics", get(health::operator_metrics));
        // Auth. These are what the website calls, and none of them needs a
        // PocketBase instance: the identity provider is this crate's own
        // `identity` module, over the `identities` table.
        .route("/auth/signup", post(auth::signup));
        .route("/auth/login", post(auth::login));
        .route("/auth/google", post(auth::google_sign_in));
        .route("/auth/verify-email", post(auth::verify_email));
        .route("/auth/password-reset/request", post(auth::request_password_reset));
        .route("/auth/password-reset/confirm", post(auth::confirm_password_reset));
        .route("/auth/verification/resend", post(auth::resend_verification));
        .route("/auth/logout", post(auth::logout));
        .route("/auth/logout-all", post(auth::logout_all));
        // The two the settings panel needs, and the reason each exists rather than a
        // PocketBase SDK call: `change_password` replaces `users.update` with a verb
        // that verifies the CURRENT password and kills every session, and
        // `list_providers` replaces `listExternalAuths` by reporting the account's own
        // identity rows and taking no address or id.
        .route("/auth/password-change", post(auth::change_password));
        .route("/auth/providers", get(auth::list_providers));
        // Account & Wallet
        .route("/api/me", get(account::get_me));
        .route("/api/usage", get(account::get_usage));
        .route("/api/usage/recent", get(account::get_recent_usage));
        .route("/api/export", get(account::export_account_data));
        .route("/api/topups", get(account::get_topups).post(account::create_topup));
        // API Keys
        .route("/api/keys", get(keys::list_keys).post(keys::create_key));
        .route("/api/keys/{id}", patch(keys::update_key));
        .route("/api/keys/{id}/revoke", post(keys::revoke_key));
        // Telegram
        .route("/api/telegram/link-code", post(telegram::issue_link_code));
        .route("/api/telegram", delete(telegram::unlink_telegram));
        // Reviews. Cookie-authenticated: the bot these were specified for is not
        // being built, and the gate's three properties live in the table.
        .route("/api/reviews", get(reviews::get_reviews).post(reviews::upsert_review));
        .route("/api/reviews/mine", get(reviews::get_my_review));
        .route("/api/reviews/withdraw", post(reviews::withdraw_review));
        // The bot's redemption endpoint, authenticated by TELEGRAM_BOT_TOKEN
        .route("/api/bot/link", post(telegram::redeem_link_code));
        // Live updates (SSE)
        .route("/events", get(events::sse_events_handler));
        // Admin (cookie + operator flag)
        .route("/api/admin/accounts", get(admin::list_accounts));
        .route("/api/admin/audit", get(admin::list_recent_audit));
        .route("/api/admin/accounts/{id}", get(admin::get_account));
        .route("/api/admin/accounts/{id}/audit", get(admin::get_account_audit));
        .route("/api/admin/accounts/{id}/suspend", post(admin::suspend_account));
        .route("/api/admin/accounts/{id}/restore", post(admin::resume_account));
        .route("/api/admin/accounts/{id}/resume", post(admin::resume_account));
        // Webhooks
        .route("/webhooks/midtrans", post(webhooks::handle_midtrans_webhook));
        // Proxy
        .route("/v1/chat/completions", post(proxy::chat_completions))
    )
    // The router-level fallback, so the 404 for an UNROUTED path is the documented JSON
    // rather than axum's empty-bodied default.
    //
    // docs/error-model.md:10 promises "Every error returns the same JSON. No bare HTML
    // error pages, no empty bodies." Measured against the running binary, everything the
    // HANDLERS answer already met that - eleven of fourteen error paths carried
    // code/message/request_id - but a request that matches no route never reaches a
    // handler, so axum's own 404 (empty body) was what a client actually got. That is the
    // commonest client mistake there is, and the api-spec ADVERTISES two 404s of exactly
    // this kind (the DESIGNED-NOT-BUILT routes), so the contract was broken on paths the
    // documentation deliberately points at.
    //
    // NOT the same as METHOD_NOT_ALLOWED, which axum raises for a mounted path with the
    // wrong verb; that keeps its own status and is asserted separately in the route table.
    .fallback(unrouted)
    .with_state(state)
}

/// The path literals `create_router`'s `routes!` invocation actually expanded.
///
/// THE SECOND HALF OF THE INVENTORY GUARD, and it exists because the first half
/// cannot see the macro body. `the_route_inventory_matches_the_mounted_table`
/// compares `ROUTES` to `MOUNTED`, and BOTH of those are hand-written text: a
/// route added to the macro and forgotten in `ROUTES` left every check green
/// while the router served a path the inventory denied. The `;` markers catch a
/// HAND-DELETED line; nothing caught an ADDED one.
///
/// This array is not written by hand. It is the same invocation `create_router`
/// passes to `routes!`, run through the `@paths` arm, whose `stringify!($path)`
/// is the compiler's rendering of the literal the router was built from - so the
/// two cannot describe different invocations, because they are the same tokens.
/// `the_macro_body_lists_the_inventory_it_mounts` compares this to `ROUTES`.
///
/// WHAT THIS STILL DOES NOT COVER: the METHOD. `@paths` captures `$path` only,
/// so a `.route("/api/keys/{id}", get(...))` changed to `post(...)` moves in
/// neither list. That gap is stated on `ROUTES` already ("the method-router
/// expressions themselves are read by no test") and closing it is not possible
/// from a `macro_rules` arm without capturing `$m` too - which would make this
/// array carry method names no caller can check against anything.
#[allow(dead_code)]
pub const MOUNTED_BY_THE_MACRO: &[&str] = routes!(@paths
    .route("/health", get(health::health_check));
    // Operator-only operational counts. NOT on /health: that body is pinned
    // because the deploy gate parses it, and a leak test forbids ANY digit in an
    // unauthenticated health body. See health::operator_metrics.
    .route("/api/admin/metrics", get(health::operator_metrics));
    // Auth. These are what the website calls, and none of them needs a PocketBase
    // instance: the identity provider is this crate's own `identity` module, over
    // the `identities` table.
    .route("/auth/signup", post(auth::signup));
    .route("/auth/login", post(auth::login));
    .route("/auth/google", post(auth::google_sign_in));
    .route("/auth/verify-email", post(auth::verify_email));
    .route("/auth/password-reset/request", post(auth::request_password_reset));
    .route("/auth/password-reset/confirm", post(auth::confirm_password_reset));
    .route("/auth/verification/resend", post(auth::resend_verification));
    .route("/auth/logout", post(auth::logout));
    .route("/auth/logout-all", post(auth::logout_all));
    // The two the settings panel needs, and the reason each exists rather than a
    // PocketBase SDK call: `change_password` replaces `users.update` with a verb
    // that verifies the CURRENT password and kills every session, and
    // `list_providers` replaces `listExternalAuths` by reporting the account's own
    // identity rows and taking no address or id.
    .route("/auth/password-change", post(auth::change_password));
    .route("/auth/providers", get(auth::list_providers));
    // Account & Wallet
    .route("/api/me", get(account::get_me));
    .route("/api/usage", get(account::get_usage));
    .route("/api/usage/recent", get(account::get_recent_usage));
    .route("/api/export", get(account::export_account_data));
    .route("/api/topups", get(account::get_topups).post(account::create_topup));
    // API Keys
    .route("/api/keys", get(keys::list_keys).post(keys::create_key));
    .route("/api/keys/{id}", patch(keys::update_key));
    .route("/api/keys/{id}/revoke", post(keys::revoke_key));
    // Telegram
    .route("/api/telegram/link-code", post(telegram::issue_link_code));
    .route("/api/telegram", delete(telegram::unlink_telegram));
    // Reviews. This mirror is compared against ROUTES by
    // `the_macro_body_lists_the_inventory_it_mounts`, so a route added to one and
    // not the other fails there rather than silently drifting.
    .route("/api/reviews", get(reviews::get_reviews).post(reviews::upsert_review));
    .route("/api/reviews/mine", get(reviews::get_my_review));
    .route("/api/reviews/withdraw", post(reviews::withdraw_review));
    // The bot's redemption endpoint, authenticated by TELEGRAM_BOT_TOKEN
    .route("/api/bot/link", post(telegram::redeem_link_code));
    // Live updates (SSE)
    .route("/events", get(events::sse_events_handler));
    // Admin (cookie + operator flag)
    .route("/api/admin/accounts", get(admin::list_accounts));
    .route("/api/admin/audit", get(admin::list_recent_audit));
    .route("/api/admin/accounts/{id}", get(admin::get_account));
    .route("/api/admin/accounts/{id}/audit", get(admin::get_account_audit));
    .route("/api/admin/accounts/{id}/suspend", post(admin::suspend_account));
    .route("/api/admin/accounts/{id}/restore", post(admin::resume_account));
    .route("/api/admin/accounts/{id}/resume", post(admin::resume_account));
    // Webhooks
    .route("/webhooks/midtrans", post(webhooks::handle_midtrans_webhook));
    // Proxy
    .route("/v1/chat/completions", post(proxy::chat_completions))
);

/// The documented JSON for a path that matches no route.
///
/// Reuses `AppError::NotFound` rather than building a body here, so the shape, the `code`
/// vocabulary and the `request_id` all come from the ONE place that defines them - which is
/// what keeps this from drifting away from the handlers.
async fn unrouted() -> AppError {
    AppError::NotFound("no route matches this path".into())
}

/// Process-wide serialisation of environment-variable mutation in tests.
///
/// std::env is PROCESS-GLOBAL and Rust runs the tests of one binary in parallel
/// threads inside a single process, so a variable one test sets is visible to
/// every other test - and a save/restore pair is NOT enough on its own: two
/// tests can interleave between the save and the restore, each seeing the
/// other's value. Both halves are therefore required, and this module owns both
/// so that every test in the crate that touches these variables shares ONE lock:
///
/// - ENV_LOCK is that one lock. EnvGuard::set takes it and holds it until the
///   guard drops, so two guarded tests cannot overlap - including across modules
///   (routes::account and routes::webhooks both write MIDTRANS_SERVER_KEY, and
///   two private mutexes would not exclude each other);
/// - EnvGuard captures the PREVIOUS value with var_os and puts it back on Drop:
///   on the success path, on an assertion panic and on an early return alike. A
///   variable that was UNSET before is REMOVED again rather than left holding
///   the test's value.
///
/// The guard is deliberately not Send (it holds a MutexGuard): acquire it in the
/// test body and keep it there, outside any tokio::spawn, which is where the
/// assertions run.
#[cfg(test)]
pub mod test_env {
    use std::ffi::{OsStr, OsString};
    use std::sync::{Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// The ONE process-wide lock over environment mutation in tests.
    ///
    /// Acquire it at the start of every test that reads or writes a shared
    /// environment variable and keep it alive for the whole test: it is the half
    /// that stops two tests from INTERLEAVING their writes. It is deliberately
    /// not Send (it holds a MutexGuard), so it belongs in the test body and must
    /// not cross a tokio::spawn - a spawned assertion task may write a variable
    /// with EnvGuard instead, which is safe precisely because this lock is held
    /// by the test that spawned it.
    pub struct EnvLock {
        _lock: MutexGuard<'static, ()>,
    }

    impl EnvLock {
        pub fn acquire() -> Self {
            // A panicking test must not poison the lock for every later test:
            // the state a panic leaves behind is exactly what EnvGuard restores.
            Self {
                _lock: ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner()),
            }
        }
    }

    /// Sets environment variables and puts the PREVIOUS values back on Drop - on
    /// the success path, on an assertion panic and on an early return alike. A
    /// variable that was UNSET before is REMOVED again rather than left holding
    /// the test value.
    ///
    /// Takes no lock (so it is Send and may be used inside a spawned task):
    /// it is only safe while the test's EnvLock is alive.
    pub struct EnvGuard {
        previous: Vec<(&'static str, Option<OsString>)>,
    }

    impl EnvGuard {
        pub fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
            let mut guard = Self {
                previous: Vec::new(),
            };
            guard.also(key, value);
            guard
        }

        /// Sets another variable under the same guard.
        pub fn also(&mut self, key: &'static str, value: impl AsRef<OsStr>) {
            self.previous.push((key, std::env::var_os(key)));
            std::env::set_var(key, value);
        }

        /// Removes a variable for the duration of the guard, putting the
        /// PREVIOUS value - or its absence - back on Drop. This is how a test
        /// exercises the "required configuration is MISSING" path rather than
        /// merely an invalid value.
        pub fn remove(key: &'static str) -> Self {
            let mut guard = Self {
                previous: Vec::new(),
            };
            guard.also_remove(key);
            guard
        }

        /// Removes another variable under the same guard.
        pub fn also_remove(&mut self, key: &'static str) {
            self.previous.push((key, std::env::var_os(key)));
            std::env::remove_var(key);
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // Reverse order, so a variable written twice ends at its original.
            for (key, previous) in self.previous.drain(..).rev() {
                match previous {
                    Some(previous) => std::env::set_var(key, previous),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{EnvGuard, EnvLock};
        use std::panic::AssertUnwindSafe;

        // Probe names no other test in the crate touches, so these assertions
        // read the process environment without racing a sibling.
        const RESTORED: &str = "APK_TEST_ENV_GUARD_PROBE_RESTORED";
        const REMOVED: &str = "APK_TEST_ENV_GUARD_PROBE_REMOVED";
        const PANICKED: &str = "APK_TEST_ENV_GUARD_PROBE_PANICKED";
        const SET_FOR_REMOVAL: &str = "APK_TEST_ENV_GUARD_PROBE_REMOVAL";
        const SERIALISED: &str = "APK_TEST_ENV_GUARD_PROBE_SERIALISED";

        #[test]
        fn drop_restores_the_previous_value() {
            std::env::set_var(RESTORED, "the-original-value");
            {
                let _guard = EnvGuard::set(RESTORED, "the-test-value");
                assert_eq!(
                    std::env::var(RESTORED).as_deref(),
                    Ok("the-test-value"),
                    "the guard must install the value it was given"
                );
            }
            assert_eq!(
                std::env::var(RESTORED).as_deref(),
                Ok("the-original-value"),
                "the guard must put the PREVIOUS value back when it drops"
            );
            std::env::remove_var(RESTORED);
        }

        #[test]
        fn drop_removes_a_variable_that_was_unset() {
            std::env::remove_var(REMOVED);
            assert!(std::env::var_os(REMOVED).is_none());

            {
                let _guard = EnvGuard::set(REMOVED, "leaked-if-not-removed");
                assert_eq!(
                    std::env::var(REMOVED).as_deref(),
                    Ok("leaked-if-not-removed")
                );
            }

            assert!(
                std::env::var_os(REMOVED).is_none(),
                "a variable that was UNSET before the guard must be unset again, not left holding the test value"
            );
        }

        #[test]
        fn remove_hides_a_variable_and_restores_its_previous_state_on_drop() {
            let _lock = EnvLock::acquire();
            std::env::set_var(SET_FOR_REMOVAL, "the-original-value");
            {
                let _guard = EnvGuard::remove(SET_FOR_REMOVAL);
                assert!(std::env::var_os(SET_FOR_REMOVAL).is_none());
            }
            assert_eq!(
                std::env::var(SET_FOR_REMOVAL).as_deref(),
                Ok("the-original-value"),
                "a removed variable must come back with its previous value"
            );
            std::env::remove_var(SET_FOR_REMOVAL);
        }

        /// The classic bug this type exists to prevent: a guard that restores on
        /// the success path only, leaving the value installed when an assertion
        /// unwinds through it.
        #[test]
        fn drop_restores_on_the_panic_path_too() {
            std::env::set_var(PANICKED, "the-original-value");

            let unwound = std::panic::catch_unwind(AssertUnwindSafe(|| {
                let _guard = EnvGuard::set(PANICKED, "the-test-value");
                panic!("unwind straight through the guard");
            }));

            assert!(
                unwound.is_err(),
                "the probe closure must actually have panicked"
            );
            assert_eq!(
                std::env::var(PANICKED).as_deref(),
                Ok("the-original-value"),
                "Drop must run on the unwinding path, not only on success"
            );
            std::env::remove_var(PANICKED);
        }

        /// The OTHER half: a guard that restores but does not exclude would
        /// still let two tests interleave their writes.
        #[test]
        fn a_second_lock_cannot_enter_while_the_first_is_alive() {
            use std::sync::atomic::{AtomicBool, Ordering};
            use std::sync::Arc;
            use std::time::Duration;

            std::env::remove_var(SERIALISED);
            let entered = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&entered);

            let held = EnvLock::acquire();
            let contender = std::thread::spawn(move || {
                let _lock = EnvLock::acquire();
                let _guard = EnvGuard::set(SERIALISED, "held-by-the-other-thread");
                flag.store(true, Ordering::SeqCst);
            });

            // 50 ms, down from 200 ms. The property is "the contender did not get
            // in during a window in which it demonstrably would have": an
            // unblocked thread reaches that store in microseconds, so a 10,000x
            // margin is still a proof and the wait is not the point of the test.
            std::thread::sleep(Duration::from_millis(50));
            assert!(
                !entered.load(Ordering::SeqCst),
                "a second test must NOT get in while the first holds the env lock - that mutual exclusion is what stops two tests interleaving writes to a process-global variable"
            );

            drop(held);
            contender
                .join()
                .expect("the contender thread must not panic");
            assert!(
                entered.load(Ordering::SeqCst),
                "the contender must proceed once the first lock has dropped"
            );

            // The contender restored what IT saw - the value the first lock-holder
            // had written - so the probe is cleaned up explicitly. It is a name
            // nothing else in the crate reads.
            std::env::remove_var(SERIALISED);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{self, TestDb};

    /// The whole contract of the one session-cookie parser. Every endpoint that
    /// accepts a session credential goes through it, so these cases are the
    /// behaviour of all four surfaces at once.
    #[test]
    fn session_cookie_parsing_contract() {
        // The plain case.
        assert_eq!(session_token_from_cookie_header("session=abc"), Some("abc"));

        // Among other cookies, with the whitespace browsers actually send.
        for header in [
            "a=1; session=abc; b=2",
            "a=1;session=abc;b=2",
            "a=1;  session=abc  ",
        ] {
            assert_eq!(
                session_token_from_cookie_header(header),
                Some("abc"),
                "header: {header}"
            );
        }

        // A cookie whose name merely ends in, or merely starts with, "session"
        // is not ours. Accepting one would let another site's cookie act as a
        // credential.
        for header in ["notsession=1", "xsession=1", "sessionx=1"] {
            assert_eq!(
                session_token_from_cookie_header(header),
                None,
                "header: {header}"
            );
        }

        // An empty value is not a credential: it must never resolve an account.
        assert_eq!(session_token_from_cookie_header("session="), None);

        // Some other cookie, no session cookie at all.
        assert_eq!(session_token_from_cookie_header("other=1"), None);

        // Empty or whitespace-only header.
        assert_eq!(session_token_from_cookie_header(""), None);
        assert_eq!(session_token_from_cookie_header("   "), None);
        assert_eq!(session_token_from_cookie_header(" ; "), None);

        // Tokens are opaque, so the value is everything after the FIRST '='.
        // Pinned deliberately: a parser that split on '=' or took the last
        // segment would silently truncate the credential.
        assert_eq!(session_token_from_cookie_header("session=a=b"), Some("a=b"));

        // Duplicate names: the FIRST piece carrying a non-empty value wins and
        // the rest are ignored. Pinned because it decides which credential a
        // second same-named cookie cannot override.
        assert_eq!(
            session_token_from_cookie_header("session=one; session=two"),
            Some("one")
        );
        // An empty-valued cookie is SKIPPED, not fatal: the search continues to
        // the next piece. This is what all four former copies did, and it is
        // pinned here rather than changed. It is not an escalation - both
        // cookies come from the same origin and the token that wins is one the
        // client itself presented - but it is the one place the "empty value is
        // not a credential" rule is per-cookie rather than per-header, so a
        // future reader must not assume the empty value aborts the search.
        assert_eq!(
            session_token_from_cookie_header("session=; session=two"),
            Some("two")
        );
    }

    #[test]
    fn session_token_hash_is_sha256_hex_and_never_the_token() {
        // Only the hash reaches Postgres; the cookie value never does.
        assert_eq!(
            hash_token("apk_sess_abc"),
            "c943c9214781fe698239bd2827dda2ed0fd7c0c746cd3ab44785da20083a1a9e"
        );
        assert_eq!(
            hash_token(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_ne!(hash_token("apk_sess_abc"), hash_token("apk_sess_abd"));
    }

    /// auth.rs's parser was FOLDED INTO this module, so there is no second
    /// implementation to compare against - the login path now resolves the
    /// cookie through the function tested above. What can still drift is the
    /// pair of halves that produce and consume the cookie, so this pins the
    /// round trip: whatever `Set-Cookie` login emits, the shared parser reads
    /// back as the same token.
    #[test]
    fn the_set_cookie_login_writes_is_the_one_the_parser_reads() {
        for token in ["apk_sess_deadbeef", "apk_sess_a=b", "x"] {
            let headers = crate::routes::auth::session_cookie(token.to_string(), 30)
                .expect("login sets a cookie");
            let set_cookie = headers
                .get(header::SET_COOKIE)
                .and_then(|v| v.to_str().ok())
                .expect("login sets a cookie");
            assert_eq!(
                session_token_from_cookie_header(set_cookie),
                Some(token),
                "set-cookie: {set_cookie}"
            );
        }
    }

    /// A fixture account, with one session row this test chose the state of, in
    /// its OWN migrated SQLite database.
    ///
    /// Ported from the Postgres original, which read `DATABASE_URL`, inserted an
    /// account and a session, and then deleted the account in a fire-and-forget
    /// task so its `ON DELETE CASCADE` cleaned the session up even when the
    /// assertions panicked. SQLite makes all of that unnecessary: the database is
    /// a file, so `TestDb` builds one per fixture and `close()` removes it - there
    /// is no shared state to delete rows out of and therefore no teardown that has
    /// to be panic-safe. What the original `Drop` was protecting is preserved by
    /// `close()` being awaited rather than trusted to drop order.
    ///
    /// The INSERT carries every NOT NULL column: the SQLite schema has no DEFAULT
    /// for `id`, `created_at` or `last_seen_at` (plan section 4.1, correction 1),
    /// so the Postgres `INSERT INTO sessions (account_id, ...)` shape would fail at
    /// runtime with a NOT NULL constraint error.
    struct SessionFixture {
        db: TestDb,
        account_id: Uuid,
        token: String,
    }

    impl SessionFixture {
        /// A live session: not revoked, expiring far beyond the idle bound, seeded
        /// as the login path seeds one.
        async fn live() -> Self {
            Self::new(
                &format!("apk_sess_{}", Uuid::new_v4().simple()),
                false,
                chrono::Utc::now() + chrono::Duration::days(30),
            )
            .await
        }

        async fn new(
            token: &str,
            revoked: bool,
            expires_at: chrono::DateTime<chrono::Utc>,
        ) -> Self {
            let db = TestDb::new().await;
            let account_id = test_support::account(&db.pool).await;
            let now = chrono::Utc::now();

            sqlx::query(SESSION_INSERT)
                .bind(Uuid::new_v4().hyphenated())
                .bind(account_id.hyphenated())
                .bind(hash_token(token))
                .bind(expires_at)
                .bind(now)
                .bind(revoked.then(chrono::Utc::now))
                .bind(now)
                .execute(&db.pool)
                .await
                .expect("create session");

            Self {
                db,
                account_id,
                token: token.to_string(),
            }
        }

        fn cookie_header(&self) -> HeaderMap {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::COOKIE,
                format!("a=1; session={}; b=2", self.token).parse().unwrap(),
            );
            headers
        }

        async fn close(self) {
            self.db.close().await;
        }
    }

    /// Every NOT NULL sessions column, so the fixture cannot silently depend on a
    /// column default the strict schema does not have.
    const SESSION_INSERT: &str = "INSERT INTO sessions (id, account_id, token_hash, expires_at, last_seen_at, revoked_at, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)";

    /// Moves a session's stored `last_seen_at` back in time.
    ///
    /// This is the only honest way to age a session: the idle rule reads the
    /// STORED instant, so a test that wanted "8 days idle" without writing the row
    /// would be asserting against a clock it does not own.
    ///
    /// Updated through `token_hash` rather than by session id, so the test touches
    /// exactly the row its own cookie resolves - and it asserts that one row moved,
    /// because an UPDATE that silently matched nothing would leave the session live
    /// and turn the refusal assertion into a lie about a fixture that never aged.
    async fn set_last_seen_days_ago(pool: &SqlitePool, token: &str, days: i64) {
        let result = sqlx::query("UPDATE sessions SET last_seen_at = ? WHERE token_hash = ?")
            .bind(chrono::Utc::now() - chrono::Duration::days(days))
            .bind(hash_token(token))
            .execute(pool)
            .await
            .expect("age the session");
        assert_eq!(
            result.rows_affected(),
            1,
            "the fixture must age exactly its own session row"
        );
    }

    /// The lookup half: a live token resolves to its account, and every other
    /// shape is unauthenticated - never a different account.
    #[tokio::test]
    async fn live_session_lookup_resolves_the_account_or_refuses() {
        let live = SessionFixture::new(
            &format!("apk_sess_{}", Uuid::new_v4().simple()),
            false,
            chrono::Utc::now() + chrono::Duration::days(30),
        )
        .await;
        let pool = live.db.pool.clone();

        // A valid session resolves to its own account.
        assert_eq!(
            resolve_account_from_cookie(&pool, &live.cookie_header())
                .await
                .expect("a live session resolves"),
            live.account_id
        );

        // A different cookie but no session cookie.
        let mut no_session = HeaderMap::new();
        no_session.insert(header::COOKIE, "a=1; b=2".parse().unwrap());
        assert!(resolve_account_from_cookie(&pool, &no_session)
            .await
            .is_err());

        // No Cookie header at all.
        assert!(resolve_account_from_cookie(&pool, &HeaderMap::new())
            .await
            .is_err());

        // An empty session value never resolves an account.
        let mut empty = HeaderMap::new();
        empty.insert(header::COOKIE, "session=".parse().unwrap());
        assert!(resolve_account_from_cookie(&pool, &empty).await.is_err());

        // An unknown token is refused.
        let mut unknown = HeaderMap::new();
        unknown.insert(
            header::COOKIE,
            format!("session=apk_sess_{}", Uuid::new_v4().simple())
                .parse()
                .unwrap(),
        );
        assert!(resolve_account_from_cookie(&pool, &unknown).await.is_err());

        live.close().await;
    }

    /// A revoked or expired session is refused even though the row exists: this
    /// is what logout relies on.
    #[tokio::test]
    async fn live_revoked_and_expired_sessions_are_refused() {
        let revoked = SessionFixture::new(
            &format!("apk_sess_{}", Uuid::new_v4().simple()),
            true,
            chrono::Utc::now() + chrono::Duration::days(30),
        )
        .await;
        assert!(
            resolve_account_from_cookie(&revoked.db.pool, &revoked.cookie_header())
                .await
                .is_err()
        );
        revoked.close().await;

        let expired = SessionFixture::new(
            &format!("apk_sess_{}", Uuid::new_v4().simple()),
            false,
            chrono::Utc::now() - chrono::Duration::days(1),
        )
        .await;
        assert!(
            resolve_account_from_cookie(&expired.db.pool, &expired.cookie_header())
                .await
                .is_err()
        );
        expired.close().await;
    }

    /// The 7-day idle bound, which `docs/decisions.md:96` settles as the other half
    /// of "30 days absolute, 7 days idle" and which the config has always carried
    /// (`config/apikita.toml` `idle_days = 7`) without anything reading it.
    ///
    /// RED FIRST, deliberately: this test was written BEFORE the idle predicate
    /// existed, and it failed for the real reason - an 8-day-idle session resolved
    /// to its account. A test added after the fix proves only that the code does
    /// what the code does.
    ///
    /// The three cases are the whole contract, and the middle one is the trap:
    /// a session idle for LONGER than the bound is refused, a session idle inside it
    /// resolves, and a session whose row is stale but whose absolute expiry has also
    /// passed is refused by the pre-existing rule rather than by this one. The last
    /// case is the positive control for "the idle check did not replace the expiry
    /// check".
    #[tokio::test]
    async fn live_a_session_idle_past_the_bound_is_refused_though_its_row_is_far_from_expiry() {
        let live = SessionFixture::live().await;
        let pool = live.db.pool.clone();

        // Seeded at the LOGIN instant, then moved back in time - the only way this
        // can be driven is a real stored `last_seen_at`, because the idle test is
        // against the stored value, never against `now`.
        assert_eq!(
            resolve_account_from_cookie(&pool, &live.cookie_header())
                .await
                .expect("a freshly-seen session resolves"),
            live.account_id
        );

        // 8 days idle, 30-day absolute expiry: the row is not close to expiring,
        // so ONLY the idle bound can refuse it.
        set_last_seen_days_ago(&pool, &live.token, 8).await;
        assert!(
            resolve_account_from_cookie(&pool, &live.cookie_header())
                .await
                .is_err(),
            "a session idle for 8 days must be refused: docs/decisions.md settles the lifetime as 30 days absolute AND 7 days idle"
        );

        // The boundary is INSIDE the bound at exactly 6 days, so the refusal above
        // is the bound and not "any old session is refused".
        set_last_seen_days_ago(&pool, &live.token, 6).await;
        assert_eq!(
            resolve_account_from_cookie(&pool, &live.cookie_header())
                .await
                .expect("a session idle for 6 days is still inside a 7-day bound"),
            live.account_id
        );

        live.close().await;

        // Positive control on the OTHER axis: an absolute expiry in the past is
        // still refused, so adding the idle rule did not displace the existing one.
        let stale = SessionFixture::new(
            &format!("apk_sess_{}", Uuid::new_v4().simple()),
            false,
            chrono::Utc::now() + chrono::Duration::days(30),
        )
        .await;
        set_last_seen_days_ago(&stale.db.pool, &stale.token, 40).await;
        assert!(
            resolve_account_from_cookie(&stale.db.pool, &stale.cookie_header())
                .await
                .is_err(),
            "idle past the bound is refused even when the absolute expiry is far away"
        );
        stale.close().await;
    }

    /// Both bounds are INCLUSIVE-UNUSABLE, pinned at the exact instant.
    ///
    /// The function's own doc states the rule - "Both bounds are inclusive-unusable,
    /// matching the expires_at > ? predicate the lookup has always used" - and the
    /// test above cannot catch a change to it. It compares two CALLS to the same
    /// function, so a boundary that moved on both sides at once still passes. What a
    /// customer meets at the boundary is the difference between a session that is
    /// gone and one that is not, and it is a single instant wide.
    ///
    /// Each bound is isolated by making the other comfortably satisfied, so a failure
    /// names which of the two moved rather than that "something did".
    #[test]
    fn both_session_bounds_are_inclusive_unusable_at_the_exact_instant() {
        let now = chrono::Utc::now();
        const IDLE_DAYS: i64 = 7;
        const ABSOLUTE_DAYS: i64 = 30;
        let live = |last_seen_at, expires_at| {
            session_is_live_at(now, last_seen_at, expires_at, IDLE_DAYS, ABSOLUTE_DAYS)
        };

        // (1) ABSOLUTE. last_seen_at is now, so the idle rule is satisfied by a
        // fortnight and cannot be what refuses these.
        assert!(
            !live(now, now),
            "expires_at EXACTLY now must already be unusable: the rule is a session is
             live while its expiry is still in the future, and a boundary flipped to
             `<` would keep it alive for one more instant"
        );
        assert!(
            live(now, now + chrono::Duration::microseconds(1)),
            "one microsecond of headroom must be enough: a session whose expiry has
             not yet passed is live, and pinning only the dead side would let a
             boundary move in the direction that refuses a valid session"
        );

        // (2) IDLE. expires_at is a year out, so the absolute rule cannot be what
        // refuses these.
        let far = now + chrono::Duration::days(365);
        let at_bound = now - chrono::Duration::days(IDLE_DAYS);
        assert!(
            !live(at_bound, far),
            "last_seen_at EXACTLY idle_days ago must already be unusable: the rule is
             last_seen_at + idle_days > now, so the instant itself is out"
        );
        assert!(
            live(at_bound + chrono::Duration::microseconds(1), far),
            "one microsecond of activity inside the bound must be enough, and pinning
             only the dead side would let a boundary move in the direction that cuts a
             session short while it is still in use"
        );

        // (3) BOTH AT ONCE, which is the shape a real expiry produces: the bounds
        // are independent refusals and either is enough, so the two dead rungs above
        // must not depend on the other rule being slack.
        assert!(
            !live(at_bound, now),
            "a session at both bounds at once is refused, and it would be refused by
             either rule alone - neither is leaning on the other"
        );
    }
    /// The idle bound must be INERT unless it is STRICTER than the absolute
    /// lifetime, and that is a property of the code rather than of a particular
    /// config value.
    ///
    /// The hazard this pins: a session row's `expires_at` is seeded
    /// `now + absolute_days` at login (auth.rs), so an idle value at or above the
    /// absolute lifetime can never refuse a session that the expiry rule would have
    /// admitted. If a future edit ever moved the expiry to somewhere else - or
    /// applied the idle rule to something other than `last_seen_at` - this is where
    /// it would show up as a session that dies earlier than the register promises.
    ///
    /// Driven at the pure level so it holds with NO database and NO clock: the
    /// decision is a function of three instants.
    #[test]
    fn idle_bound_is_inert_unless_it_is_stricter_than_the_absolute_lifetime() {
        let now = chrono::Utc::now();
        let login = now - chrono::Duration::days(1);

        for absolute_days in [1u32, 7, 30, 365] {
            let expires_at = login + chrono::Duration::days(absolute_days as i64);

            for idle_days in [absolute_days, absolute_days + 1, absolute_days + 30] {
                let last_seen_at = login;
                assert_eq!(
                    session_is_live_at(now, last_seen_at, expires_at, idle_days as i64, absolute_days as i64),
                    session_is_live_at(now, last_seen_at, expires_at, absolute_days as i64, absolute_days as i64),
                    "an idle bound of {idle_days} days cannot change the outcome when the session's own expiry was seeded {absolute_days} days after login: both rules admit exactly the same sessions"
                );
            }
        }
    }

    use std::sync::Arc;

    use axum::http::StatusCode;

    // ---------------------------------------------------------------------
    // The route TABLE itself.
    //
    // `create_router` above is the product's entire HTTP surface, and nothing
    // asserted the table AS a table: keys.rs drove one request through
    // /v1/chat/completions, so a route that was deleted, renamed, moved to the
    // wrong method, or mounted without `with_state` would leave the whole suite
    // green while the endpoint was gone.
    //
    // The discriminating signal is decided by the ROUTER, before any handler
    // runs, which is what makes the table testable with no database at all:
    //
    //   404 NOT_FOUND          - no route matches the path; the handler never runs;
    //   405 METHOD_NOT_ALLOWED - the path is mounted, this method is not; the
    //                            handler never runs and no query is issued;
    //   anything else          - the request reached the handler, so the path AND
    //                            the method are mounted. WHICH non-404/405 status
    //                            it is (401 without a credential, 415/400 for the
    //                            body, 503 when the database is unreachable) is
    //                            the handler's business, not the router's, so it
    //                            is deliberately not pinned here.
    //
    // The negative controls are what give the positive half meaning: without them
    // the positive half would also pass on a router that mounted nothing.

    /// A pool that never dials. A lazy pool opens no connection and issues no
    /// query until a handler asks for one, and every request below stops at the
    /// router or at the first credential check - so none of these tests needs a
    /// database and all of them run in the default (non-ignored) suite. That is
    /// the point: a route-table regression is caught without a database.
    ///
    /// Ported from the Postgres original, whose lazy DSN pointed at a closed TCP
    /// port. There is no host to be unreachable under SQLite, so the equivalent is
    /// a filename that does not exist: `init_pool` deliberately does not set
    /// `create_if_missing`, so the first query fails with `unable to open database
    /// file` - the same "the database is not there" answer, for the same reason.
    /// The file is under the system temp directory and is never created.
    fn lazy_pool() -> SqlitePool {
        let absent = std::env::temp_dir().join(format!(
            "apikita_route_table_absent_{}.db",
            Uuid::new_v4().simple()
        ));
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(absent)
            .busy_timeout(std::time::Duration::from_millis(500));

        sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            // The default acquire timeout is 30s, which would make the one test
            // that lets the health handler dial (to prove it used THIS state's
            // pool) take half a minute. The file is absent, so the attempt fails
            // immediately; this only bounds the pathological case.
            .acquire_timeout(std::time::Duration::from_millis(500))
            .connect_lazy_with(options)
    }

    /// The real application state, built the way main.rs builds it.
    fn table_state() -> AppState {
        let config = Arc::new(
            crate::config::AppConfig::load_from_file("../config/apikita.toml")
                .expect("the shipped config parses"),
        );
        let events = Arc::new(crate::routes::events::RealtimeHub::new(&config.realtime));
        let trusted_proxies: Arc<[crate::ip_tracking::IpCidr]> = Arc::from(
            crate::ip_tracking::parse_cidrs(&config.network.trusted_proxy_cidrs)
                .expect("the shipped config CIDRs parse")
                .into_boxed_slice(),
        );
        AppState {
            pool: lazy_pool(),
            config,
            http_client: reqwest::Client::new(),
            events,
            ip_salt: Arc::new(crate::ip_tracking::DailySalt::new()),
            trusted_proxies,
        }
    }

    /// The whole router as a tower service, reached the way a caller reaches it.
    ///
    /// MockConnectInfo supplies the peer address that proxy::chat_completions
    /// extracts: without a ConnectInfo layer that extractor fails with 500, which
    /// would tell us nothing about whether the route matched.
    fn table_app() -> Router {
        use axum::extract::connect_info::MockConnectInfo;
        use std::net::SocketAddr;

        create_router(table_state())
            .layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))))
    }

    /// Sends one credential-free request through the real router and returns the
    /// status. The body is supplied where a handler parses one BEFORE checking a
    /// credential (auth::login validates its payload first): a 400 there still
    /// proves the route matched, which is all this asserts.
    async fn route_status(app: &Router, method: &str, uri: &str, body: &str) -> StatusCode {
        route_status_with_body(app, method, uri, body).await.0
    }

    /// The status AND the response body, for the callers that have to tell two
    /// 404s apart.
    ///
    /// A 404 alone does not mean "not mounted". axum answers 404 for an unmatched
    /// path AND for a request whose extractor rejected the body ("Failed to
    /// deserialize the JSON body into the target type" carries `NOT_FOUND` in axum's
    /// `JsonRejection`), so the status by itself cannot separate "this route is gone"
    /// from "this row sends a body the handler refuses". The body text is what
    /// separates them, and `POST /auth/password-change` sat on that ambiguity until
    /// the response was read rather than the code.
    async fn route_status_with_body(
        app: &Router,
        method: &str,
        uri: &str,
        body: &str,
    ) -> (StatusCode, String) {
        use axum::http::Request;
        use tower::ServiceExt;

        let mut req = Request::builder().method(method).uri(uri);
        if !body.is_empty() {
            req = req.header(header::CONTENT_TYPE, "application/json");
        }
        let req = req
            .body(axum::body::Body::from(body.to_string()))
            .expect("build the request");

        let res = app
            .clone()
            .oneshot(req)
            .await
            .expect("the router must respond");
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("the response body must be readable");
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// A concrete path parameter, used to prove {id} is a PARAMETER.
    const SOME_KEY_ID: &str = "00000000-0000-0000-0000-000000000000";

    /// Every route `create_router` mounts: (method, path, body). One row per route
    /// CALL, so /api/topups and /api/keys - each a SINGLE `.route()` with two
    /// methods chained - appear per method. **26 route calls, 28 rows.**
    ///
    /// THIS COUNT WAS WRONG FOR SIX ROUTES AND NOTHING SAID SO, which is the gap
    /// `the_route_inventory_matches_the_mounted_table` now closes. The header used to
    /// read "13 route calls, 15 rows" against a router mounting 26 calls, and
    /// `GET /api/admin/metrics`, `GET /api/usage/recent`, `GET /api/export`,
    /// `GET /api/admin/accounts`, `GET /api/admin/audit` and
    /// `GET /api/admin/accounts/{id}/audit` were mounted, documented in the api-spec,
    /// and ABSENT here. Nothing failed: every check below starts from this table, so a
    /// route missing from it was invisible. The direction is now checked in both
    /// places - this table against the spec below, and this table against the route
    /// inventory above.
    ///
    /// ROWS, not routes. The first attempt at this header said "30 rows" - a figure
    /// arrived at by assuming each dual-method path contributes an extra row. It does
    /// not: `/api/topups` and `/api/keys` are ONE `.route(` call each carrying both
    /// `get` and `post`, and this table lists a request per METHOD, so each
    /// contributes two rows in total, not three. 40 is the number the test pins.
    ///
    /// The reviews routes add three paths and FOUR rows: `/api/reviews` is the
    /// third dual-method path in the table (GET for the public aggregate, POST to
    /// write), so it contributes two, while `/api/reviews/mine` and
    /// `/api/reviews/withdraw` contribute one each.
    ///
    /// The native identity routes each contribute ONE row: they are all
    /// single-method (POST), so `/auth/logout` and `/auth/logout-all` are not the
    /// dual-method paths the previous note named - only `/api/topups` and
    /// `/api/keys` are. `/auth/password-change` (POST) and `/auth/providers` (GET)
    /// are likewise one row each, and they are the two the settings panel needs:
    /// the password change the retired SDK did with `users.update`, and the linked
    /// provider list it did with `listExternalAuths`.
    ///
    /// THE COUNT MOVED FROM 32 TO 36 IN TWO DIRECTIONS AT ONCE, which is why the
    /// header names the intermediate number rather than only the endpoints. It was
    /// 34, then 32 when the identity port deleted `/auth/exchange`, then 33 when
    /// `/auth/google` was added to the inventory it had been missing from, and 36
    /// now. A reader comparing two of those numbers without the sequence in
    /// between would conclude a route was dropped that never existed.
    const MOUNTED: &[(&str, &str, &str)] = &[
        ("GET", "/health", ""),
        ("GET", "/api/admin/metrics", ""),
        // The bodies are the smallest shape each handler accepts, so a
        // credential-free request reaches the handler and stops at its own
        // validation rather than at axum's extractor - which is all this table
        // asserts (the router matched).
        (
            "POST",
            "/auth/signup",
            r#"{"email":"a@b.example","password":"correct-horse-battery"}"#,
        ),
        (
            "POST",
            "/auth/login",
            r#"{"email":"a@b.example","password":"correct-horse-battery"}"#,
        ),
        ("POST", "/auth/google", r#"{"id_token":"probe"}"#),
        ("POST", "/auth/verify-email", r#"{"token":"probe"}"#),
        (
            "POST",
            "/auth/password-reset/request",
            r#"{"email":"a@b.example"}"#,
        ),
        (
            "POST",
            "/auth/password-reset/confirm",
            r#"{"token":"probe","email":"a@b.example","password":"correct-horse-battery"}"#,
        ),
        (
            "POST",
            "/auth/verification/resend",
            r#"{"email":"a@b.example"}"#,
        ),
        ("POST", "/auth/logout", ""),
        ("POST", "/auth/logout-all", ""),
        (
            // HANDLED BEFORE THE GATE, which is why no extractor can turn it into a
            // 404. `POST /auth/password-change` does this and NOTHING ELSE in the table
            // does: every other handler takes `Json<T>` directly and therefore lets
            // axum decide the status when a body does not parse, which axum reports as
            // 404 - indistinguishable, from a status alone, from a route that is not
            // mounted at all. Take a `Result<Json<T>, JsonRejection>` and map the
            // rejection yourself; the table can then assert what it means to assert.
            "POST",
            "/auth/password-change",
            r#"{"current_password":"probe","new_password":"probe"}"#,
        ),
        ("GET", "/auth/providers", ""),
        ("GET", "/api/me", ""),
        ("GET", "/api/usage", ""),
        ("GET", "/api/usage/recent", ""),
        ("GET", "/api/export", ""),
        ("GET", "/api/topups", ""),
        ("POST", "/api/topups", "{}"),
        ("GET", "/api/keys", ""),
        ("POST", "/api/keys", "{}"),
        (
            "PATCH",
            "/api/keys/00000000-0000-0000-0000-000000000000",
            "{}",
        ),
        (
            "POST",
            "/api/keys/00000000-0000-0000-0000-000000000000/revoke",
            "",
        ),
        ("GET", "/events", ""),
        // Telegram: cookie for issuing/unlinking, bot token for redemption. A
        // credential-free request stops at the guard (401), which is not 404/405 -
        // so the router matched, which is all this table asserts.
        ("POST", "/api/telegram/link-code", ""),
        ("DELETE", "/api/telegram", ""),
        // Reviews. GET is public, so a bodyless request must reach the handler
        // and answer 200. The write paths are session-scoped and stop at the
        // guard (401) - not 404/405, which is what says the router matched.
        ("GET", "/api/reviews", ""),
        ("POST", "/api/reviews", r#"{"rating":5}"#),
        ("GET", "/api/reviews/mine", ""),
        ("POST", "/api/reviews/withdraw", ""),
        (
            "POST",
            "/api/bot/link",
            r#"{"code":"000000","telegram_id":"1"}"#,
        ),
        // Admin: cookie + operator flag. A credential-free request stops at the
        // guard (401), which is not 404/405 - so the router matched, which is all
        // this table asserts.
        ("GET", "/api/admin/accounts", ""),
        ("GET", "/api/admin/audit", ""),
        (
            "GET",
            "/api/admin/accounts/00000000-0000-0000-0000-000000000000",
            "",
        ),
        (
            "GET",
            "/api/admin/accounts/00000000-0000-0000-0000-000000000000/audit",
            "",
        ),
        (
            "POST",
            "/api/admin/accounts/00000000-0000-0000-0000-000000000000/suspend",
            "",
        ),
        (
            "POST",
            "/api/admin/accounts/00000000-0000-0000-0000-000000000000/restore",
            "",
        ),
        (
            "POST",
            "/api/admin/accounts/00000000-0000-0000-0000-000000000000/resume",
            "",
        ),
        ("POST", "/webhooks/midtrans", "{}"),
        ("POST", "/v1/chat/completions", ""),
    ];

    /// Near misses that MUST be 405: the PATH is mounted, the method is not.
    const WRONG_METHOD: &[(&str, &str)] = &[
        ("POST", "/health"),
        ("GET", "/auth/logout"),
        ("GET", "/auth/logout-all"),
        ("DELETE", "/api/keys/00000000-0000-0000-0000-000000000000"),
        // A {id} path IS mounted (as PATCH), so a GET on it is 405 - which is
        // also the evidence that the parameter route exists for the METHOD as
        // well as the path.
        ("GET", "/api/keys/00000000-0000-0000-0000-000000000000"),
        (
            "GET",
            "/api/keys/00000000-0000-0000-0000-000000000000/revoke",
        ),
        ("GET", "/webhooks/midtrans"),
        ("GET", "/v1/chat/completions"),
        ("GET", "/api/telegram/link-code"),
        ("GET", "/api/telegram"),
        ("GET", "/api/bot/link"),
    ];

    /// Near misses that MUST be 404: no route matches the path at all. Includes
    /// paths one segment away from a real one, a real path with an extra segment
    /// appended, and the trailing-slash spellings that axum does NOT treat as the
    /// same path.
    ///
    /// Deliberately NOT in this list: `GET /api/keys/{a-uuid}`. That path IS
    /// mounted (as PATCH), so axum answers 405, not 404 - measured, not assumed.
    /// It belongs with the wrong-method controls below.
    const ABSENT_PATH: &[(&str, &str)] = &[
        ("GET", "/api/nope"),
        ("GET", "/api/mex"),
        // A NEAR MISS FOR THE PATH THAT REPLACED IT. `/auth/exchange` is gone from
        // the crate entirely, so a plural `s` tests nothing about it; the useful
        // sibling now is `/auth/password-change`, whose path is long enough to be
        // mistyped and mounted only as POST.
        ("POST", "/auth/exchanges"),
        ("POST", "/auth/password-changes"),
        ("GET", "/healt"),
        ("GET", "/api/me/"),
        ("GET", "/api/ME"),
        ("GET", "/health/"),
        (
            "POST",
            "/api/keys/00000000-0000-0000-0000-000000000000/revoked",
        ),
        ("POST", "/api/topups/extra"),
        ("GET", "/v1/chat/completion"),
    ];

    /// THE TABLE. Every mounted route must be dispatched by the router, and every
    /// near miss must be refused BY THE ROUTER - 405 for a real path with the
    /// wrong method, 404 for a path that is not mounted. Delete a route, rename a
    /// path, or move a handler to another method and this fails naming the exact
    /// pair.
    // -----------------------------------------------------------------------
    // THE SPEC MUST NOT PROMISE A ROUTE THE SERVER DOES NOT SERVE.
    // -----------------------------------------------------------------------
    //
    // `docs/server/api-spec.md:14-25` is presented as the CURRENT surface. W30 found
    // FOUR rows in it with no handler and no mount at all - GET/POST /api/reviews,
    // GET /api/bot/account, GET /api/bot/reviews/mine, POST /api/bot/notify-topup -
    // while the tables they need have existed since the initial migration.
    //
    // They are not bugs to fix (the whole reviews/feed flow is Telegram-BOT driven,
    // and the launch checklist records that the bot is still design-only),
    // but an integrator reading that table would call /api/reviews, get a 404, and
    // conclude the SERVER was broken. So the spec now marks them DESIGNED-NOT-BUILT,
    // and this test keeps the two documents from drifting apart again.
    //
    // It cannot parse free-form prose, so it pins the CONTRACT instead: every route
    // the spec calls implemented must appear in MOUNTED. When a new endpoint is
    // documented, either it is mounted (and added to MOUNTED) or it is marked
    // designed-only - and the list below is the mechanical record of which is which.
    #[test]
    fn the_spec_marks_exactly_the_designed_but_unbuilt_routes_as_designed() {
        // Routes the api-spec table mentions that are NOT mounted, each of which must
        // therefore carry a designed-not-built marker in the spec. If one of these is
        // ever mounted, this test fails and forces the spec to be updated with it -
        // which is the point.
        const SPEC_ONLY: &[&str] = &[
            "/api/bot/account",
            "/api/bot/reviews/mine",
            "/api/bot/notify-topup",
        ];

        let mounted_paths: Vec<&str> = MOUNTED.iter().map(|(_, path, _)| *path).collect();

        for path in SPEC_ONLY {
            assert!(
                !mounted_paths.contains(path),
                "{path} is mounted now, so it is no longer designed-only: remove it from SPEC_ONLY AND from the designed-not-built marker in docs/server/api-spec.md",
            );
        }

        // POSITIVE CONTROL: the list above is only meaningful if MOUNTED is the real
        // set. A path that IS mounted must be found, or `contains` is answering the
        // wrong question.
        assert!(
            mounted_paths.contains(&"/api/bot/link"),
            "the mounted table must contain a route that really exists, or the check above is vacuous"
        );
    }

    /// The SPEC must actually say so, which the test above never checked.
    ///
    /// `the_spec_marks_exactly_the_designed_but_unbuilt_routes_as_designed` asserts that
    /// the four designed-only paths are NOT MOUNTED. It never opened the spec. So the
    /// claim in its own comment - that each "must therefore carry a designed-not-built
    /// marker in the spec" - was a comment, and the W30 defect it exists to prevent
    /// could return by editing the spec alone: delete the markers, leave the routes
    /// unmounted, and the suite stays green while the table promises four endpoints
    /// that 404.
    ///
    /// That is the API CONTRACT, which is the one document a client codes against, and
    /// a promise the server does not keep is worse here than silence: an integrator
    /// calls /api/reviews, gets a 404, and concludes the SERVER is broken.
    ///
    /// Read from the spec rather than a list kept here, for the same reason the alert
    /// guards read probe.sh: a hand-maintained copy of what another file says is the
    /// same bug twice over.
    #[test]
    fn the_spec_names_every_route_it_does_not_serve() {
        let spec = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("docs")
                .join("server")
                .join("api-spec.md"),
        )
        .expect("docs/server/api-spec.md must be readable, or this checks nothing");

        // The paragraph that carries the promise, located by its own heading rather
        // than by line: a line citation here would be the very defect this test
        // exists to catch, and the paragraph moves whenever a route is added.
        let start = spec
            .find("MARKED ROUTES ARE DESIGNED, NOT BUILT")
            .unwrap_or_else(|| {
                panic!(
                    "docs/server/api-spec.md no longer carries the MARKED ROUTES ARE \
                     DESIGNED, NOT BUILT paragraph. Without it a reader has no way to \
                     tell a documented route from a served one."
                )
            });
        // A generous window: the paragraph names all three and may grow a sentence.
        let end = (start + 700).min(spec.len());
        let promise = &spec[start..end];

        const SPEC_ONLY: &[&str] = &[
            "/api/bot/account",
            "/api/bot/reviews/mine",
            "/api/bot/notify-topup",
        ];
        for path in SPEC_ONLY {
            assert!(
                promise.contains(path),
                "docs/server/api-spec.md does not say that {path} is designed and not \
                 built, while the route table presents it. An integrator who calls it \
                 gets a 404 and concludes the server is broken - which is exactly the \
                 defect this paragraph was added to prevent."
            );
        }

        // The table carries the marker too, and that is the half a scanner sees: the
        // paragraph is four paragraphs below the table, so a reader skimming the table
        // meets the row before the warning.
        //
        // The marker MOVED to the Bot row when the reviews routes were mounted. It
        // used to sit on Reviews, which was correct while the review endpoints were
        // designed-only; now they are served, so the same marker on that row would be
        // the opposite defect - a served route advertised as unbuilt. The three
        // /api/bot/* routes are still designed-only, so the marker still has a row to
        // live on and the test still asserts something true.
        //
        // The expected substring is the whole cell, `| **Bot** ⚠ |`, not a fragment of
        // it: the row's label is bold because the bot row groups several routes and the
        // others name one each. Asserting a loosely-matched fragment would pass on a
        // row that merely CONTAINS those characters - including a future row where the
        // marker no longer means what this test says it means.
        assert!(
            spec.contains("| **Bot** ⚠ |"),
            "the api-spec route table no longer marks ANY row as designed-not-built, while the \
             MARKED ROUTES ARE DESIGNED, NOT BUILT paragraph still names three routes. The \
             warning paragraph is further down; the marker is what someone reading the table \
             actually sees."
        );
    }

    /// And the OTHER direction: every route the server MOUNTS is in the spec.
    ///
    /// The companion to the test above, and the half nobody checks. That one asks
    /// whether the spec is honest about what it does not serve; this asks whether it
    /// describes what it does. An endpoint that is mounted and undocumented is a
    /// route an integrator cannot find and a maintainer cannot account for - and the
    /// spec is a CONTRACT, so a silent addition is a contract that grew without
    /// anyone agreeing to it.
    ///
    /// The spec writes a parameterised path with :id while the router needs a
    /// concrete one, so the mounted path is normalised before the comparison. That
    /// normalisation is asserted to be EXERCISED below: a test that quietly compared
    /// nothing because every path failed to match would pass.
    #[test]
    fn every_mounted_route_is_documented_in_the_api_spec() {
        let spec = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("docs")
                .join("server")
                .join("api-spec.md"),
        )
        .expect("docs/server/api-spec.md must be readable, or this checks nothing");

        const PLACEHOLDER: &str = "00000000-0000-0000-0000-000000000000";
        let mut parameterised = 0usize;
        for (_, path, _) in MOUNTED {
            let documented = path.replace(PLACEHOLDER, ":id");
            if documented.contains(":id") {
                parameterised += 1;
            }
            assert!(
                spec.contains(&documented),
                "{path} is mounted by create_router but docs/server/api-spec.md does not \
                 document it. An endpoint the contract does not mention is a route an \
                 integrator cannot find, and a contract that grew without anyone \
                 agreeing to it."
            );
        }

        // The vacuity guards. A MOUNTED list that lost its entries, or a table that
        // stopped using the placeholder, would make the loop above answer nothing.
        assert!(
            MOUNTED.len() >= 30,
            "MOUNTED has {} entries, far fewer than the router mounts, so the comparison \
             above is checking a list that no longer describes the surface",
            MOUNTED.len()
        );
        assert!(
            parameterised >= 4,
            "only {parameterised} mounted paths contain the id placeholder, so the \
             normalisation above is barely exercised and a mismatch in it would go \
             unnoticed"
        );
    }
    #[tokio::test]
    async fn every_mounted_route_dispatches_and_every_near_miss_is_refused() {
        let app = table_app();

        for (method, path, body) in MOUNTED {
            let (status, response) = route_status_with_body(&app, method, path, body).await;
            assert_ne!(
                status,
                StatusCode::METHOD_NOT_ALLOWED,
                "{method} {path} is mounted by create_router but the router refused the method \
                 (405): the handler was moved to a different method"
            );
            // A 404 IS NOT PROOF THAT THE PATH IS MISSING. axum answers 404 for an
            // unmatched path AND for a rejected extractor, so a row whose body the
            // handler will not accept is indistinguishable, from the status alone, from
            // a route that was deleted. The RESPONSE TEXT is what separates the two, so
            // it travels in the message: without it the next reader gets a 404 and no
            // way to tell which of two very different repairs is the right one.
            assert_ne!(
                status,
                StatusCode::NOT_FOUND,
                "{method} {path} answered 404 against a router built from create_router. Either \
                 the route was deleted or renamed, or the request was rejected before it reached \
                 the handler - axum reports both as 404. Body sent: {body}. Response: {response}"
            );
        }

        for (method, path) in WRONG_METHOD {
            assert_eq!(
                route_status(&app, method, path, "").await,
                StatusCode::METHOD_NOT_ALLOWED,
                "{method} {path} is NOT mounted, but the router did not answer 405 - either the \
                 path stopped being mounted, or a method the table does not declare was accepted"
            );
        }

        for (method, path) in ABSENT_PATH {
            assert_eq!(
                route_status(&app, method, path, "").await,
                StatusCode::NOT_FOUND,
                "{method} {path} is NOT mounted, but the router did not answer 404 - a route the \
                 table does not declare is being matched (prefix matching, or an extra route)"
            );
        }
    }

    /// The number of DISTINCT paths in a MOUNTED-shaped list, for the count
    /// cross-check in `the_route_inventory_matches_the_mounted_table`.
    ///
    /// `MOUNTED` is a table of REQUESTS, so a dual-method path appears twice. `ROUTES`
    /// is a list of MOUNTS, so it appears once. Comparing `MOUNTED.len()` to the
    /// inventory count was wrong by exactly the number of extra rows the dual-method
    /// routes contribute - which is how this helper came to exist.
    fn mounted_paths_distinct(mounted_set: &[&str]) -> usize {
        let mut seen: Vec<&str> = Vec::new();
        for path in mounted_set {
            if !seen.contains(path) {
                seen.push(path);
            }
        }
        seen.len()
    }

    ///
    /// Every other test in this module starts from `MOUNTED`. That is the direction
    /// that happens to be safe against a route being *deleted* - the dispatch test
    /// would fail - but it is blind in the direction a route is *added*: mount a new
    /// route, forget the table, and all seven checks stay green while nothing
    /// exercises the new endpoint. This test was written because that had ALREADY
    /// happened four times (`GET /api/admin/metrics`, `GET /api/usage/recent`,
    /// `GET /api/export`, `GET /api/admin/accounts/{id}/audit` were mounted,
    /// documented, and absent from the table), which is why the fix is a check
    /// rather than a correction.
    ///
    /// HOW IT CAN WORK AT ALL, given that axum exposes no route table and the naive
    /// source scan is wrong (docs/testing.md:129-150). `create_router` no longer
    /// writes its paths inline: it expands the `routes!` macro over an inventory
    /// whose PATH LITERALS ARE THE STRINGS THE MSVC/AXUM ROUTER IS BUILT FROM.
    /// So this test reads the same bytes the router consumed, and the two cannot
    /// disagree by accident.
    ///
    /// THE TRAP IT MUST NOT FALL INTO is the one docs/testing.md documents: a
    /// regex over `create_router` matches twenty of twenty-six `.route(` calls and
    /// falsely reports `/webhooks/midtrans` as unmounted, because some calls put the
    /// path on the following line. The parse below is line-oriented and requires the
    /// path on the SAME line as `.route(`, and `ROUTES` is written so that is always
    /// true - the assertion on the parsed count is what stops a future multi-line
    /// entry from silently dropping out.
    #[test]
    fn the_route_inventory_matches_the_mounted_table() {
        // The rendered inventory, as Rust sees it. `include_str!` would be a second
        // copy of the bytes; `ROUTES` IS the bytes create_router expanded.
        let inventory = ROUTES;

        // A route line in ROUTES is exactly:  .route("<path>", <expr>);
        // Anchored at both ends so a COMMENT that mentions `.route("...")` cannot be
        // mistaken for a route - the comment above `/api/admin/metrics` mentions
        // `/health`, and a looser match would collect it.
        let mut declared: Vec<&str> = Vec::new();
        for line in inventory.lines() {
            let line = line.trim();
            let Some(rest) = line.strip_prefix(".route(\"") else {
                continue;
            };
            let Some(close) = rest.find('"') else {
                continue;
            };
            let path = &rest[..close];
            // The trailing `;` is the marker described on ROUTES: rustfmt never emits
            // it, so its presence is evidence a human wrote this line and its absence
            // is evidence something rewrote it.
            assert!(
                line.ends_with(");"),
                "the route line for {path} does not end in `);`. That trailing `;` is what \
                 makes a deleted route detectable: without it this test can no longer tell a \
                 mounted route from a removed one. Restore the `;` (an editor or a formatter \
                 removed it)."
            );
            declared.push(path);
        }

        // The count cross-check, placed before the set comparison because its failure
        // message explains the mismatch the set loops would otherwise report as a
        // mystery: a route added to `routes!` and not to MOUNTED is a COUNT difference
        // before it is a set difference.
        //
        // NOT `MOUNTED.len()`: MOUNTED is a table of REQUESTS, so a dual-method path
        // (/api/topups, /api/keys) appears twice while `ROUTES` lists it once. The
        // first version of this test compared the two lengths and failed purely
        // because 28 rows != 26 route calls - a false positive, which is the failure
        // mode docs/testing.md:152-171 says costs more than a miss.
        let mounted_set: Vec<&str> = MOUNTED.iter().map(|(_, path, _)| *path).collect();

        // The parser must find the routes, not a fraction of them. If a route is
        // ever written multi-line, or a line's shape changes, this count drops and
        // the message names the cause rather than reporting a mystery mismatch
        // below. The number itself is pinned separately at the end of this test;
        // this assertion is the cross-check against the table.
        assert_eq!(
            declared.len(),
            mounted_paths_distinct(&mounted_set),
            "the inventory parse found {} route lines but MOUNTED declares {} distinct paths. \
             Either a route was added to `routes!` and not to MOUNTED (which is the defect this \
             test exists for), or the ROUTES line was rewritten into a shape this parser does \
             not recognise (it requires the path on the same line as `.route(`). Check both: \
             MOUNTED is the by-request table, `routes!` is what the router mounts.",
            declared.len(),
            mounted_paths_distinct(&mounted_set)
        );

        // The real comparison. MOUNTED uses a concrete uuid where the router uses {id},
        // so the inventory path is normalised the same way
        // `every_mounted_route_is_documented_in_the_api_spec` normalises MOUNTED.
        let declared_set: Vec<String> = declared
            .iter()
            .map(|p| p.replace("{id}", SOME_KEY_ID))
            .collect();

        for path in &declared_set {
            assert!(
                mounted_set.contains(&path.as_str()),
                "{path} is mounted by create_router and does NOT appear in MOUNTED, so no test \
                 exercises it. Add a (method, path, body) row to MOUNTED - this is exactly the \
                 hole the four admin/account routes sat in."
            );
        }
        for path in &mounted_set {
            assert!(
                declared_set.iter().any(|d| d == path),
                "{path} is in MOUNTED but create_router does NOT mount it. Either mount it or \
                 delete the row: a row for a route that does not exist makes the dispatch test \
                 assert something about a 404."
            );
        }

        // POSITIVE CONTROL, because both loops above pass trivially against two empty
        // lists and a parse that collected nothing would report a clean bill of health.
        assert!(
            declared.contains(&"/api/bot/link"),
            "the inventory parse did not find /api/bot/link, so it is not reading the route \
             list it thinks it is and every assertion above is vacuous"
        );
        // And the count the router actually expands, pinned: a hand-edit that drops a
        // route is caught above; this catches a rewrite that drops one from BOTH and
        // looks self-consistent.
        assert_eq!(
            declared.len(),
            37,
            "create_router mounts a different number of routes than the 37 this test was \
             last reconciled against. If that is deliberate, update this number AND the \
             MOUNTED doc comment that states the row count - both, or the next reader \
             trusts a stale one."
        );
        assert_eq!(
            mounted_set.len(),
            40,
            "MOUNTED declares a different number of ROWS than the 40 this test was last \
             reconciled against. Rows, not routes: /api/topups, /api/keys and /api/reviews each \
             carry GET+POST, so each contributes two rows from one `.route(` call, while the rest \
             including /auth/logout and /auth/logout-all are single-method. A mismatch here \
             means a row was added or removed without updating this number."
        );
    }

    /// THE MACRO BODY AND THE INVENTORY LIST THE SAME PATHS, AND NOTHING ELSE DID.
    ///
    /// THE HOLE THIS CLOSES. `ROUTES` and the `routes!(...)` invocation inside
    /// `create_router` are two hand-written copies of the same list, and every
    /// existing check reads ONE of them against `MOUNTED`:
    ///
    /// - `the_route_inventory_matches_the_mounted_table` reads `ROUTES` and compares
    ///   it to `MOUNTED`. Both are text. A route present in the macro body and in
    ///   `MOUNTED` but MISSING from `ROUTES` fails that test - correctly - but a
    ///   route added to the MACRO and to `MOUNTED` while `ROUTES` was left alone
    ///   fails nothing, and the inventory then denies a path the router serves.
    /// - The `;` markers catch a line DELETED by hand. Nothing caught a line ADDED
    ///   to one copy and not the other.
    ///
    /// The test's own history is the evidence this mattered: when the password-change
    /// and provider routes were mounted, `ROUTES` was edited FIRST and the inventory
    /// test passed while every request to those paths answered 404, because
    /// `create_router` had not been touched. Four iterations were spent before the
    /// response body, not the status, revealed it.
    ///
    /// `MOUNTED_BY_THE_MACRO` is not a third hand-written list - it is this same
    /// invocation run through the macro's `@paths` arm, so its entries are the
    /// compiler's rendering of the literals the router expanded. Comparing it to
    /// `ROUTES` is therefore a real cross-check rather than two copies of one
    /// mistake.
    #[test]
    fn the_macro_body_lists_the_inventory_it_mounts() {
        // `stringify!` renders a STRING LITERAL as its source text INCLUDING the
        // quotes, so this array holds `"\"/health\""` rather than `/health`. The
        // quotes are stripped on the READING side because the macro arm cannot do
        // it: `macro_rules` has no way to take `$path` and emit its value without
        // the literal syntax, and `concat!`/`stringify!` combinations keep the
        // quotes too. Stripping here still compares the COMPILER'S rendering of the
        // literal rather than a re-typed copy of it, which is the property that
        // makes this cross-check worth having.
        let from_the_macro: Vec<&str> = MOUNTED_BY_THE_MACRO
            .iter()
            .map(|quoted| quoted.trim_matches('"'))
            .collect();
        let from_the_inventory: Vec<&str> = ROUTES
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                let rest = line.strip_prefix(".route(\"")?;
                let close = rest.find('"')?;
                Some(&rest[..close])
            })
            .collect();

        // POSITIVE CONTROL. `stringify!` returning an empty-ish array, or the parse
        // finding nothing, would make the comparison below pass against nothing at
        // all - the same vacuity the sibling test guards against.
        assert!(
            from_the_macro.contains(&"/v1/chat/completions"),
            "the macro capture did not find the last route, so it is not reading the \
             invocation it thinks it is and every assertion here is vacuous"
        );

        // Every route the ROUTER expands must be in the inventory the test above
        // trusts. This is the direction the missing-check bug took.
        for path in &from_the_macro {
            assert!(
                from_the_inventory.contains(path),
                "{path} is mounted by create_router's `routes!` invocation but is NOT in \
                 the `ROUTES` inventory. The inventory is what `the_route_inventory_matches_\
                 the_mounted_table` and `every_mounted_route_is_documented_in_the_api_spec` \
                 both read, so this path is mounted, undocumented and unlisted - add it to \
                 `ROUTES` (with its trailing `;`), and to MOUNTED, and to the api-spec."
            );
        }

        // And the other direction, so the inventory cannot advertise a route the
        // router does not serve - which is how a 404 gets documented as a 200.
        for path in &from_the_inventory {
            assert!(
                from_the_macro.contains(path),
                "{path} is in the `ROUTES` inventory but create_router does NOT mount it. \
                 Every check downstream reads `ROUTES`, so the inventory claims a path a \
                 client would get a 404 from - add the `.route(` line to the `routes!` \
                 invocation in create_router, or remove this one from `ROUTES`."
            );
        }

        assert_eq!(
            from_the_macro.len(),
            from_the_inventory.len(),
            "the macro expands {} paths and `ROUTES` lists {}. The two loops above should \
             already have named the offender; this count is the cross-check that they were \
             not both empty or both truncated.",
            from_the_macro.len(),
            from_the_inventory.len()
        );
    }

    /// The dual-method routes: /api/topups and /api/keys are ONE `.route()` call
    /// each with `get(..).post(..)` chained, so both methods must be mounted and
    /// a THIRD method must still be 405. That last assertion is what makes this
    /// more than a duplicate of the table test: it pins that the route carries
    /// exactly the two methods the table declares.
    #[tokio::test]
    async fn the_dual_method_routes_accept_both_methods_and_only_those() {
        let app = table_app();

        for path in ["/api/topups", "/api/keys"] {
            for method in ["GET", "POST"] {
                let status = route_status(&app, method, path, "{}").await;
                assert_ne!(
                    status,
                    StatusCode::METHOD_NOT_ALLOWED,
                    "{method} {path} is declared on one route call with the other method, but the \
                     router does not accept it (405)"
                );
                assert_ne!(
                    status,
                    StatusCode::NOT_FOUND,
                    "{method} {path} is mounted by create_router but not matched (404)"
                );
            }

            // A method the table does NOT declare on these paths stays refused,
            // so the dual-method route cannot silently widen.
            for method in ["PUT", "DELETE"] {
                assert_eq!(
                    route_status(&app, method, path, "").await,
                    StatusCode::METHOD_NOT_ALLOWED,
                    "{method} {path} is not declared by the table and must be 405"
                );
            }
        }
    }

    /// `/api/keys/{id}` is a PATH PARAMETER, not the literal string "{id}".
    ///
    /// Two different ids are both routed - a literal route could only ever match
    /// one spelling - and a non-UUID segment is answered by the `Path<Uuid>`
    /// extractor (400) rather than by the router (404), which is only possible if
    /// the segment was matched as a parameter and then handed to the extractor.
    #[tokio::test]
    async fn the_key_id_route_matches_a_parameter_not_a_literal() {
        let app = table_app();

        for id in [SOME_KEY_ID, "11111111-2222-3333-4444-555555555555"] {
            let uri = format!("/api/keys/{id}");
            let status = route_status(&app, "PATCH", &uri, "{}").await;
            assert_ne!(
                status,
                StatusCode::NOT_FOUND,
                "PATCH {uri} must be routed: /api/keys/{{id}} is a parameter route (404)"
            );
            assert_ne!(
                status,
                StatusCode::METHOD_NOT_ALLOWED,
                "PATCH {uri} must be routed: the table declares patch() on /api/keys/{{id}} (405)"
            );
        }

        // Matched as a parameter, then rejected by Path<Uuid>: a 400 proves the
        // router dispatched the request, a 404 would prove it did not.
        let status = route_status(&app, "PATCH", "/api/keys/not-a-uuid", "{}").await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "a non-UUID id must reach the Path<Uuid> extractor and fail THERE (400); a 404 would \
             mean the router never matched the parameter route"
        );
    }

    /// `with_state` is part of the table: the state handed to `create_router` is
    /// the state the handlers actually run on.
    ///
    /// What this pins, and why it is DB-free: `GET /health` is 503 ONLY if the
    /// handler ran and read the pool out of the state it was given - that state's
    /// pool is lazy and points at a closed port, so the 503 is produced by the
    /// handler dialling THIS state's pool. A router whose handlers did not receive
    /// the state could not produce it.
    ///
    /// The second half is the negative control for a table that is only served for
    /// one particular state INSTANCE: two routers built from two independently
    /// constructed states must both serve every route. (Dropping `.with_state`
    /// outright is a compile error, not a runtime one, so this test cannot and
    /// does not claim to cover that - see the module note above.)
    #[tokio::test]
    async fn the_state_handed_to_the_router_is_the_state_its_handlers_run_on() {
        let first = table_app();
        let second = table_app();

        for app in [&first, &second] {
            let status = route_status(app, "GET", "/health", "").await;
            assert_eq!(
                status,
                StatusCode::SERVICE_UNAVAILABLE,
                "GET /health must run the handler against THIS state's (unreachable) pool and \
                 report degraded; any other status means the handler did not use the state the \
                 router was given"
            );
        }

        for (method, path, body) in MOUNTED {
            for app in [&first, &second] {
                let status = route_status(app, method, path, body).await;
                assert_ne!(
                    status,
                    StatusCode::NOT_FOUND,
                    "{method} {path} is mounted but one of two independently built states did not \
                     serve it (404): the table is not served per-Router from the given state"
                );
                assert_ne!(
                    status,
                    StatusCode::METHOD_NOT_ALLOWED,
                    "{method} {path} is mounted but one of two independently built states refused \
                     the method (405)"
                );
            }
        }
    }
}
