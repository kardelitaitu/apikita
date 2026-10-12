//! The authorization boundary for the admin account-control verbs, asserted THROUGH THE ROUTER.
//!
//! WHY THIS EXISTS. An audit measured that **23 of 37 mounted routes are never exercised as an HTTP
//! path**: their wiring is proven only by a generic "this route matched - it is not a 404/405" table
//! row in `routes::tests`. That table is deliberate about its scope and says so, but the consequence is
//! that a handler can lose its authorization check and every test stays green.
//!
//! The highest-risk instance is `POST /api/admin/accounts/{id}/suspend` and its two siblings: the
//! security-critical account-control verbs, driven by nothing through the router.
//!
//! WHAT THIS ASSERTS, and why it is the right property. A request with no credential must be refused
//! BEFORE the handler runs, and an ordinary authenticated customer must be refused too. Both hold
//! regardless of whether the handler itself is correct, and both FAIL the moment the operator check is
//! dropped from the route - the mutation the existing table cannot see.
//!
//! It deliberately does NOT test handler behaviour; those have direct-call tests. This closes the
//! wiring gap and nothing else.
//!
//! VERIFIED REACHABLE: `/restore` and `/resume` both map to `resume_account` (`routes/mod.rs:377-378`).
//! That is the documented `/restore` == `/resume` equivalence, asserted separately; here all three are
//! checked for authorization because all three are mounted and all three are callable.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use sqlx::SqlitePool;
use tower::ServiceExt;
use uuid::Uuid;

use crate::config::AppConfig;
use crate::ip_tracking::{parse_cidrs, DailySalt, IpCidr};
use crate::routes::proxy::AppState;
use crate::routes::{events::RealtimeHub, hash_token};
use crate::test_support as support;

/// The three admin account-control verbs. All must be unreachable without an OPERATOR session.
const ADMIN_VERBS: &[(&str, &str)] = &[
    (
        "POST",
        "/api/admin/accounts/00000000-0000-0000-0000-000000000000/suspend",
    ),
    (
        "POST",
        "/api/admin/accounts/00000000-0000-0000-0000-000000000000/restore",
    ),
    (
        "POST",
        "/api/admin/accounts/00000000-0000-0000-0000-000000000000/resume",
    ),
];

fn live_config() -> Arc<AppConfig> {
    for path in ["../config/apikita.toml", "config/apikita.toml"] {
        if std::path::Path::new(path).exists() {
            return Arc::new(AppConfig::load_from_file(path).expect("parse apikita.toml"));
        }
    }
    panic!("could not find apikita.toml for testing");
}

/// The real application state, built the way `main.rs` builds it, so the router under test is the one
/// that actually ships.
fn test_state(pool: SqlitePool) -> AppState {
    let config = live_config();
    let events = Arc::new(RealtimeHub::new(&config.realtime));
    let trusted_proxies: Arc<[IpCidr]> = Arc::from(
        parse_cidrs(&config.network.trusted_proxy_cidrs)
            .expect("config CIDRs parse")
            .into_boxed_slice(),
    );
    AppState {
        pool,
        config,
        http_client: reqwest::Client::new(),
        events,
        ip_salt: Arc::new(DailySalt::new()),
        trusted_proxies,
        tasks: crate::shutdown::TaskTracker::new(),
    }
}

/// The whole router, reached the way a caller reaches it.
fn app(state: AppState) -> Router {
    use std::net::SocketAddr;
    crate::routes::create_router(state)
        .layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))))
}

/// A real sessions row plus the token its cookie resolves to, so the request below travels the
/// production authentication path rather than a shortcut.
async fn issue_session(pool: &SqlitePool, account_id: Uuid) -> String {
    let token = format!("apk_sess_{}", Uuid::new_v4().simple());
    let now = chrono::Utc::now();
    sqlx::query(
        "INSERT INTO sessions (id, account_id, token_hash, expires_at, last_seen_at, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(Uuid::new_v4().hyphenated())
    .bind(account_id.hyphenated())
    .bind(hash_token(&token))
    .bind(now + chrono::Duration::days(30))
    .bind(now)
    .bind(now)
    .execute(pool)
    .await
    .expect("create session");
    token
}

/// Drive a request through the real router, with or without a cookie.
async fn via_router(
    app: &Router,
    method: &str,
    uri: &str,
    cookie: Option<&str>,
) -> (StatusCode, String) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(c) = cookie {
        req = req.header(header::COOKIE, format!("session={c}"));
    }
    let req = req.body(Body::empty()).expect("build the request");

    let res = app
        .clone()
        .oneshot(req)
        .await
        .expect("the router must respond");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
        .await
        .expect("the body must be readable");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// With no credential at all, every admin verb must be refused before its handler runs.
#[tokio::test]
async fn every_admin_verb_refuses_a_request_with_no_credential() {
    let db = support::TestDb::new().await;
    let app = app(test_state(db.pool.clone()));

    for (method, uri) in ADMIN_VERBS {
        let (status, body) = via_router(&app, method, uri, None).await;

        assert_ne!(
            status,
            StatusCode::NOT_FOUND,
            "{method} {uri} is not mounted, so this test proves nothing about its authorization"
        );
        assert!(
            status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN,
            "{method} {uri} answered {status} to a request with NO credential. A security-critical \
             account-control verb must be refused before its handler runs. Body: {}",
            body.chars().take(120).collect::<String>()
        );
    }

    db.close().await;
}

/// The interesting case: a VALID session for an ordinary customer. A check that only asks "is there a
/// session?" would let this through, so this is what actually pins the operator requirement.
#[tokio::test]
async fn every_admin_verb_refuses_an_ordinary_authenticated_customer() {
    let db = support::TestDb::new().await;
    let pool = &db.pool;

    let account = support::account_with_wallet(pool).await;
    let token = issue_session(pool, account).await;

    // PRECONDITION, asserted rather than assumed: the fixture really is a non-operator, so a refusal
    // below is about authorization rather than a malformed fixture.
    let is_operator: i64 = sqlx::query_scalar("SELECT is_operator FROM accounts WHERE id = ?")
        .bind(account.hyphenated())
        .fetch_one(pool)
        .await
        .expect("read is_operator");
    assert_eq!(
        is_operator, 0,
        "PRECONDITION FAILED: the fixture account is an operator, so this test could not distinguish \
         'authorization refused' from 'authorization absent'"
    );

    let app = app(test_state(pool.clone()));
    for (method, uri) in ADMIN_VERBS {
        let (status, body) = via_router(&app, method, uri, Some(&token)).await;

        assert!(
            status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN,
            "{method} {uri} answered {status} for a NON-OPERATOR session. An ordinary authenticated \
             customer must not be able to control other accounts. Body: {}",
            body.chars().take(120).collect::<String>()
        );
        assert_ne!(
            status,
            StatusCode::NOT_FOUND,
            "{method} {uri} is not mounted, so this test proves nothing"
        );
    }

    db.close().await;
}

/// The counterweight, WITHOUT WHICH THE TESTS ABOVE ARE VACUOUS: a refusal is only evidence if a
/// permitted request would be treated differently. An operator session must get PAST the authorization
/// layer - proven by the status changing away from 401/403, whatever the handler then does with a
/// nonexistent target id.
#[tokio::test]
async fn an_operator_session_gets_past_the_authorization_layer() {
    let db = support::TestDb::new().await;
    let pool = &db.pool;

    let operator = support::account_with_wallet(pool).await;
    sqlx::query("UPDATE accounts SET is_operator = 1 WHERE id = ?")
        .bind(operator.hyphenated())
        .execute(pool)
        .await
        .expect("promote to operator");
    let token = issue_session(pool, operator).await;

    let app = app(test_state(pool.clone()));
    let mut got_past = 0;
    for (method, uri) in ADMIN_VERBS {
        let (status, _) = via_router(&app, method, uri, Some(&token)).await;
        if status != StatusCode::UNAUTHORIZED && status != StatusCode::FORBIDDEN {
            got_past += 1;
        }
    }

    assert!(
        got_past > 0,
        "NOT ONE admin verb let an OPERATOR through, so the two refusal tests above would pass even \
         if the routes refused EVERYONE. A 401/403 for an operator means the authorization layer is \
         rejecting the wrong subject, and the refusal tests are measuring nothing"
    );

    db.close().await;
}
