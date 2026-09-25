pub mod account;
pub mod auth;
pub mod events;
pub mod health;
pub mod keys;
pub mod proxy;
pub mod webhooks;

use axum::{
    http::{header, HeaderMap},
    routing::{get, patch, post},
    Router,
};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
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

/// The account a request's session cookie resolves to.
///
/// Any failure - no header, no session cookie, an unknown, revoked or expired
/// token, or an unreadable row - is `AppError::Unauthenticated`: a caller
/// cannot tell "no session" from "dead session", and neither can an attacker.
pub async fn resolve_account_from_cookie(
    pool: &PgPool,
    headers: &HeaderMap,
) -> Result<Uuid, AppError> {
    let cookie_hdr = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .ok_or(AppError::Unauthenticated)?;

    let token = session_token_from_cookie_header(cookie_hdr).ok_or(AppError::Unauthenticated)?;

    let session = sqlx::query(
        "SELECT account_id FROM sessions WHERE token_hash = $1 AND revoked_at IS NULL AND expires_at > now()",
    )
    .bind(hash_token(token))
    .fetch_optional(pool)
    .await?;

    match session {
        Some(s) => Ok(s.try_get("account_id")?),
        None => Err(AppError::Unauthenticated),
    }
}

pub fn create_router(state: AppState) -> Router {
    Router::new()
        // Ops
        .route("/health", get(health::health_check))
        // Auth
        .route("/auth/exchange", post(auth::exchange_token))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/logout-all", post(auth::logout_all))
        // Account & Wallet
        .route("/api/me", get(account::get_me))
        .route("/api/usage", get(account::get_usage))
        .route("/api/topups", get(account::get_topups).post(account::create_topup))
        // API Keys
        .route("/api/keys", get(keys::list_keys).post(keys::create_key))
        .route("/api/keys/{id}", patch(keys::update_key))
        .route("/api/keys/{id}/revoke", post(keys::revoke_key))
        // Live updates (SSE)
        .route("/events", get(events::sse_events_handler))
        // Webhooks
        .route("/webhooks/midtrans", post(webhooks::handle_midtrans_webhook))
        // Proxy
        .route("/v1/chat/completions", post(proxy::chat_completions))
        .with_state(state)
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
                assert_eq!(std::env::var(REMOVED).as_deref(), Ok("leaked-if-not-removed"));
            }

            assert!(
                std::env::var_os(REMOVED).is_none(),
                "a variable that was UNSET before the guard must be unset again, not left holding the test value: {:?}",
                std::env::var_os(REMOVED)
            );
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

            assert!(unwound.is_err(), "the probe closure must actually have panicked");
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

            std::thread::sleep(Duration::from_millis(200));
            assert!(
                !entered.load(Ordering::SeqCst),
                "a second test must NOT get in while the first holds the env lock - that mutual exclusion is what stops two tests interleaving writes to a process-global variable"
            );

            drop(held);
            contender.join().expect("the contender thread must not panic");
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
        assert_eq!(
            session_token_from_cookie_header("session=a=b"),
            Some("a=b")
        );

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
            let headers = crate::routes::auth::session_cookie(token.to_string(), 30);
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

    /// A fixture account whose session row is known to this test, deleted in FK
    /// order afterwards.
    struct SessionFixture {
        pool: PgPool,
        account_id: Uuid,
        token: String,
    }

    impl SessionFixture {
        async fn new(pool: &PgPool, token: &str, revoked: bool, expires_at: chrono::DateTime<chrono::Utc>) -> Self {
            let pb_user_id = format!("test_{}", Uuid::new_v4().simple());
            let account_id: Uuid =
                sqlx::query_scalar("INSERT INTO accounts (pb_user_id) VALUES ($1) RETURNING id")
                    .bind(&pb_user_id)
                    .fetch_one(pool)
                    .await
                    .expect("create account");

            sqlx::query(
                "INSERT INTO sessions (account_id, token_hash, expires_at, revoked_at) VALUES ($1, $2, $3, $4)",
            )
            .bind(account_id)
            .bind(hash_token(token))
            .bind(expires_at)
            .bind(revoked.then(chrono::Utc::now))
            .execute(pool)
            .await
            .expect("create session");

            Self {
                pool: pool.clone(),
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
    }

    impl Drop for SessionFixture {
        fn drop(&mut self) {
            // Deleting the account cascades to its sessions (schema: ON DELETE
            // CASCADE), so the fixture leaves nothing behind even if the
            // assertions panicked.
            let pool = self.pool.clone();
            let account_id = self.account_id;
            tokio::spawn(async move {
                let _ = sqlx::query("DELETE FROM accounts WHERE id = $1")
                    .bind(account_id)
                    .execute(&pool)
                    .await;
            });
        }
    }

    async fn live_pool() -> PgPool {
        let database_url = std::env::var("DATABASE_URL")
            .expect("set DATABASE_URL to a migrated Postgres instance");
        crate::db::init_pool(&database_url)
            .await
            .expect("connect to Postgres")
    }

    /// The lookup half: a live token resolves to its account, and every other
    /// shape is unauthenticated - never a different account.
    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn live_session_lookup_resolves_the_account_or_refuses() {
        let pool = live_pool().await;
        let live = SessionFixture::new(
            &pool,
            &format!("apk_sess_{}", Uuid::new_v4().simple()),
            false,
            chrono::Utc::now() + chrono::Duration::days(30),
        )
        .await;

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
        assert!(resolve_account_from_cookie(&pool, &no_session).await.is_err());

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
    }

    /// A revoked or expired session is refused even though the row exists: this
    /// is what logout relies on.
    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn live_revoked_and_expired_sessions_are_refused() {
        let pool = live_pool().await;

        let revoked = SessionFixture::new(
            &pool,
            &format!("apk_sess_{}", Uuid::new_v4().simple()),
            true,
            chrono::Utc::now() + chrono::Duration::days(30),
        )
        .await;
        assert!(resolve_account_from_cookie(&pool, &revoked.cookie_header())
            .await
            .is_err());

        let expired = SessionFixture::new(
            &pool,
            &format!("apk_sess_{}", Uuid::new_v4().simple()),
            false,
            chrono::Utc::now() - chrono::Duration::days(1),
        )
        .await;
        assert!(resolve_account_from_cookie(&pool, &expired.cookie_header())
            .await
            .is_err());
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

    /// A pool that never dials. `connect_lazy` opens no connection and issues no
    /// query until a handler asks for one, and every request below stops at the
    /// router or at the first credential check - so none of these tests needs
    /// DATABASE_URL and all of them run in the default (non-ignored) suite. That
    /// is the point: a route-table regression is caught without a database.
    fn lazy_pool() -> PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            // The default acquire timeout is 30s, which would make the one test
            // that lets the health handler dial (to prove it used THIS state's
            // pool) take half a minute. The port is closed, so the connection is
            // refused almost immediately; this only bounds the pathological case.
            .acquire_timeout(std::time::Duration::from_millis(500))
            .connect_lazy("postgres://apikita_route_table:unused@127.0.0.1:1/apikita_absent")
            .expect("the route-table DSN is a valid postgres url")
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

        create_router(table_state()).layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))))
    }

    /// Sends one credential-free request through the real router and returns the
    /// status. The body is supplied where a handler parses one BEFORE checking a
    /// credential (auth::exchange_token validates pb_token first): a 400 there
    /// still proves the route matched, which is all this asserts.
    async fn route_status(app: &Router, method: &str, uri: &str, body: &str) -> StatusCode {
        use axum::http::Request;
        use tower::ServiceExt;

        let mut req = Request::builder().method(method).uri(uri);
        if !body.is_empty() {
            req = req.header(header::CONTENT_TYPE, "application/json");
        }
        let req = req
            .body(axum::body::Body::from(body.to_string()))
            .expect("build the request");

        app.clone()
            .oneshot(req)
            .await
            .expect("the router must respond")
            .status()
    }

    /// A concrete path parameter, used to prove {id} is a PARAMETER.
    const SOME_KEY_ID: &str = "00000000-0000-0000-0000-000000000000";

    /// Every route `create_router` mounts: (method, path, body). One row per
    /// route CALL, so /api/topups and /api/keys - each a SINGLE `.route()` with
    /// two methods chained - appear once per method. 13 route calls, 16 rows.
    const MOUNTED: &[(&str, &str, &str)] = &[
        ("GET", "/health", ""),
        ("POST", "/auth/exchange", r#"{"pb_token":"probe"}"#),
        ("POST", "/auth/logout", ""),
        ("POST", "/auth/logout-all", ""),
        ("GET", "/api/me", ""),
        ("GET", "/api/usage", ""),
        ("GET", "/api/topups", ""),
        ("POST", "/api/topups", "{}"),
        ("GET", "/api/keys", ""),
        ("POST", "/api/keys", "{}"),
        ("PATCH", "/api/keys/00000000-0000-0000-0000-000000000000", "{}"),
        ("POST", "/api/keys/00000000-0000-0000-0000-000000000000/revoke", ""),
        ("GET", "/events", ""),
        ("POST", "/webhooks/midtrans", "{}"),
        ("POST", "/v1/chat/completions", ""),
    ];

    /// Near misses that MUST be 405: the PATH is mounted, the method is not.
    const WRONG_METHOD: &[(&str, &str)] = &[
        ("GET", "/auth/exchange"),
        ("POST", "/health"),
        ("GET", "/auth/logout"),
        ("GET", "/auth/logout-all"),
        ("DELETE", "/api/keys/00000000-0000-0000-0000-000000000000"),
        // A {id} path IS mounted (as PATCH), so a GET on it is 405 - which is
        // also the evidence that the parameter route exists for the METHOD as
        // well as the path.
        ("GET", "/api/keys/00000000-0000-0000-0000-000000000000"),
        ("GET", "/api/keys/00000000-0000-0000-0000-000000000000/revoke"),
        ("GET", "/webhooks/midtrans"),
        ("GET", "/v1/chat/completions"),
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
        ("POST", "/auth/exchanges"),
        ("GET", "/healt"),
        ("GET", "/api/me/"),
        ("GET", "/api/ME"),
        ("GET", "/health/"),
        ("POST", "/api/keys/00000000-0000-0000-0000-000000000000/revoked"),
        ("POST", "/api/topups/extra"),
        ("GET", "/v1/chat/completion"),
    ];

    /// THE TABLE. Every mounted route must be dispatched by the router, and every
    /// near miss must be refused BY THE ROUTER - 405 for a real path with the
    /// wrong method, 404 for a path that is not mounted. Delete a route, rename a
    /// path, or move a handler to another method and this fails naming the exact
    /// pair.
    #[tokio::test]
    async fn every_mounted_route_dispatches_and_every_near_miss_is_refused() {
        let app = table_app();

        for (method, path, body) in MOUNTED {
            let status = route_status(&app, method, path, body).await;
            assert_ne!(
                status,
                StatusCode::NOT_FOUND,
                "{method} {path} is mounted by create_router but the router did not match it \
                 (404): the route was deleted or the path renamed"
            );
            assert_ne!(
                status,
                StatusCode::METHOD_NOT_ALLOWED,
                "{method} {path} is mounted by create_router but the router refused the method \
                 (405): the handler was moved to a different method"
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

        for id in [
            SOME_KEY_ID,
            "11111111-2222-3333-4444-555555555555",
        ] {
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
