//! The authorization boundary for the admin account-control verbs, asserted THROUGH THE ROUTER.
//!
//! WHY THIS EXISTS. An audit measured that **most mounted routes are never exercised as an HTTP
//! path**: their wiring is proven only by a generic "this route matched - it is not a 404/405" table
//! row in `routes::tests`. That table is deliberate about its scope and says so, but the consequence is
//! that a handler can lose its authorization check and every test stays green.
//!
//! THE AUDIT FIGURE USED TO BE HERE AS "23 of 37", AND IT HAD GONE STALE. MEASURED against `ROUTES`
//! when this was written: **38** routes are mounted, not 37, and nine of them sit under `/api/admin`.
//! Nothing guarded the number - the inventory guard compares two LISTS rather than counting them - so
//! it drifted by one unnoticed the first time a route was added, which is exactly what `config.rs`
//! warns a count in a comment does. The test below pins the two counts that remain, so a reader now
//! gets the number from a check rather than from prose.
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
//! VERIFIED REACHABLE: `/restore` and `/resume` both map to `resume_account` (`routes/mod.rs:381-382`).
//! That is the documented `/restore` == `/resume` equivalence, asserted separately; here all three are
//! checked for authorization because all three are mounted and all three are callable.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use sqlx::SqlitePool;
use tower::ServiceExt;

use crate::ip_tracking::{parse_cidrs, DailySalt, IpCidr};
use crate::routes::events::RealtimeHub;
use crate::routes::proxy::AppState;
use crate::test_support as support;
use crate::test_support;

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

/// The real application state, built the way `main.rs` builds it, so the router under test is the one
/// that actually ships.
fn test_state(pool: SqlitePool) -> AppState {
    let config = crate::test_support::shipped_config();
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
    let app = test_support::app(test_state(db.pool.clone()));

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
    let token = test_support::issue_session(pool, account).await;

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

    let app = test_support::app(test_state(pool.clone()));
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
    let token = test_support::issue_session(pool, operator).await;

    let app = test_support::app(test_state(pool.clone()));
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

/// THE TWO COUNTS THE HEADER STATES, READ FROM `ROUTES` RATHER THAN FROM PROSE.
///
/// The header carried "23 of 37 mounted routes" as an unaudited figure, and the 37 had become 38 the
/// first time somebody added a route. The inventory guard
/// (`the_route_inventory_matches_the_mounted_table`) compares ROUTES against MOUNTED_BY_THE_MACRO -
/// two lists, not a count - so it passes either way and would not have noticed.
///
/// THIS PINS THE NUMBER RATHER THAN REPEATING IT. The header's argument is about SCALE - most routes
/// are never driven as an HTTP path, which is why the wiring gap matters - so a reader who takes "37"
/// for current will misjudge how much of the surface is unexercised.
///
/// A NEW ROUTE FLAILS HERE ON PURPOSE, and re-measuring is the fix: the count IS the audit, so a
/// route added without a test is a change to the number this round measured.
#[test]
fn the_admin_boundary_header_states_the_route_count_it_ships() {
    use crate::routes::ROUTES;

    let mounted = ROUTES
        .lines()
        .filter(|line| line.trim_start().starts_with(".route("))
        .count();
    assert_eq!(
        mounted,
        38,
        "the header says 38 routes are mounted; if ROUTES carries a different number now, the header is stale again - recompute it and the admin share, then edit the header"
    );

    let admin = ROUTES
        .lines()
        .filter(|line| line.trim_start().starts_with(".route("))
        .filter(|line| line.contains("/api/admin"))
        .count();
    assert_eq!(
        admin,
        9,
        "the header says nine routes sit under /api/admin; three of them are the account-control verbs this file asserts through the router"
    );
}
