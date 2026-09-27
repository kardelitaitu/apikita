use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json, Response},
};
use serde_json::json;
use sqlx::SqlitePool;
use tracing::error;

use crate::routes::admin::AdminError;
use crate::routes::proxy::AppState;

/// The fixed, generic `database` indicator for the failed probe.
///
/// What matters operationally is THAT the database is unreachable, not why. This
/// endpoint is unauthenticated (docs/server/api-spec.md:368), so serialising a
/// raw `sqlx::Error` Display into the body would hand driver messages, host and
/// port, errno, pool/connection state and possible SQL fragments to any caller
/// (docs/error-model.md:168, rule 1). Same model as `AppError::client_message`
/// in error.rs: the caller gets a fixed string, the operator keeps the detail -
/// logged below at `error!` level.
const DATABASE_UNAVAILABLE: &str = "database unavailable";

pub async fn health_check(State(pool): State<SqlitePool>) -> impl IntoResponse {
    match sqlx::query("SELECT 1").execute(&pool).await {
        Ok(_) => (
            StatusCode::OK,
            Json(json!({
                "status": "healthy",
                "database": "connected"
            })),
        ),
        Err(err) => {
            // The detail moves to the log, not out of the system: the operator
            // still sees the raw sqlx error, the caller never does.
            error!(
                error = %err,
                "health check: the database probe failed; returning a generic indicator"
            );
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "status": "degraded",
                    "database": DATABASE_UNAVAILABLE
                })),
            )
        }
    }
}

/// A count of responses and of the 5xx among them, as JSON, for an OPERATOR.
///
/// WHY THIS ROUTE EXISTS. `tools/alert/alerts.tsv` lists an `error_rate > 5%`
/// alert and marks it `needs-metrics`, because nothing could compute a rate. The
/// server has counted 5xx responses since `error.rs` gained `ServerErrorCounter`,
/// so the only missing piece was a read - this one. Both alert scripts still justify
/// the gap with "HTTP counters over a 5-minute window (same access logs)", which is
/// now STALE: the counters are in-process, so no access log and no metrics vendor is
/// needed.
///
/// WHY NOT ON `/health`, which would be the obvious home. Three independent reasons,
/// and the first two are enforced by existing tests:
///
/// 1. `/health`'s body is PINNED exactly (`{status, database}`) because the DEPLOY
///    GATE parses it. Adding a key would change a contract the deployment reads.
/// 2. A leak test forbids ANY ASCII digit in the unauthenticated `/health` body,
///    naming `count` explicitly - and this payload is nothing but digits.
/// 3. These numbers are operational data (how much traffic, how much is failing).
///    `docs/server/api-spec.md:466-470` fixes `/health` as the unauthenticated
///    liveness probe and nothing more.
///
/// So the counts sit behind the ONE operator guard, the same `cookie + operator
/// flag` scheme every admin route uses (`api-spec.md:515`), not a new credential.
///
/// **A MEASURED LIMITATION OF THE COUNTER, verified against a running server.** The
/// counter is incremented inside `AppError::into_response`, so it sees every error the
/// APPLICATION renders - and NOT the responses axum produces without reaching a
/// handler, such as the 404 for an unmatched path. Measured live: four requests to a
/// nonexistent path moved `responses` by ZERO, while two `/api/me` 401s moved it by
/// two. So the rate is over HANDLED requests, which is the right denominator for "is
/// my server failing" - an unmatched path is a scanner, not an outage - but it is not
/// "all HTTP traffic", and a future reader should not assume otherwise.
///
/// **It reads no table.** The counter is in-process, and an operator investigating
/// an incident wants these counts precisely when the database is ALSO unhealthy -
/// tying the two together would blind them at the worst moment. The only database
/// access is the guard's own identity check, which is unavoidable and correct.
/// The metrics payload, built from the process-wide counter.
///
/// Extracted from the handler so a test can exercise EXACTLY the code the handler
/// runs. The first version inlined this mapping and the test re-stated it instead, so
/// a mutation that reported `0.0` for an empty window SURVIVED - the test was
/// asserting its own copy of the logic, not the route. A function the handler calls
/// closes that gap: the two cannot drift.
pub(crate) fn metrics_payload(counter: &crate::error::ServerErrorCounter) -> serde_json::Value {
    let server_errors = counter.server_errors();
    let responses = counter.responses();

    // `None` for an empty window, never 0.0. A zero would read as "perfectly
    // healthy" and suppress the alert on a service that has simply served nothing
    // yet - a fresh deploy, a restart, or traffic that has dried up. The alert script
    // must treat null as "no data", which is a different thing from "no errors".
    let error_rate = counter.error_rate().map(|rate| {
        // Rounded to 4 decimals so the payload is stable to diff and log; the
        // threshold is 5%, so 4 decimals is far finer than the alert needs.
        (rate * 10_000.0).round() / 10_000.0
    });

    json!({
        "server_errors": server_errors,
        "responses": responses,
        "error_rate": error_rate,
    })
}

pub async fn operator_metrics(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AdminError> {
    // The guard runs FIRST, before a single counter is read, so a refused caller
    // cannot learn anything from this route.
    crate::routes::admin::require_operator(&state, &headers).await?;

    Ok(Json(metrics_payload(crate::error::server_error_counter())).into_response())
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::test_support::TestDb;
    use axum::body::to_bytes;
    use serde_json::{json, Value};
    use std::str::FromStr;
    use std::time::Duration;
    use uuid::Uuid;

    /// Drives the handler exactly the way the router does and reads its body.
    async fn probe(pool: SqlitePool) -> (StatusCode, Value) {
        let res = health_check(State(pool)).await.into_response();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("a health response must have a readable body");
        let body: Value = serde_json::from_slice(&bytes)
            .expect("docs/error-model.md:10 - every response is JSON");
        (status, body)
    }

    /// A pool pointed at a database that is genuinely unreachable, so the probe
    /// fails for real instead of being simulated. Lazy: it does not dial until the
    /// first query, which is exactly the moment the handler dials.
    ///
    /// Ported from the Postgres original, whose DSN named a closed TCP port. There
    /// is no host to be unreachable under SQLite, so the equivalent is a FILENAME
    /// that does not exist: `init_pool` deliberately does not set
    /// `create_if_missing`, so the first query fails with `unable to open database
    /// file` - the same honest "the database is not there", for the same reason.
    /// The path is under the system temp directory and is never created.
    fn unreachable_pool() -> SqlitePool {
        let absent = std::env::temp_dir().join(format!(
            "apikita_probe_absent_{}.db",
            Uuid::new_v4().simple()
        ));
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(absent)
            .busy_timeout(Duration::from_secs(5));

        sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(5))
            .connect_lazy_with(options)
    }

    /// docs/error-model.md:168 (rule 1) - never leak internals. The health
    /// endpoint is unauthenticated (docs/server/api-spec.md:368), so ANY caller
    /// can read this body: a raw sqlx error Display there hands out driver
    /// messages, host and port, errno, pool state and possible SQL fragments to
    /// the internet.
    ///
    /// Asserted on the ABSENCE of dangerous content, like the rule-1 tests in
    /// error.rs, so ordinary wording changes to the generic indicator do not
    /// break the test - only a leak does.
    #[tokio::test]
    async fn a_database_failure_reports_degraded_without_leaking_the_driver_error() {
        // The leak list is the Postgres original's, kept verbatim: the point is that
        // NO driver text reaches the caller, and the SQLite driver's own vocabulary
        // ("unable to open database file", "sqlite", the path) is what the handler
        // must not echo. "postgres" stays in the list deliberately - if the constant
        // ever regressed to naming an engine, the test still catches it.
        let (status, body) = probe(unreachable_pool()).await;

        // The shape monitoring parses stays stable (docs/deployment.md:134 gates
        // the deploy on this endpoint).
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "docs/server/api-spec.md:366 - 200 only when the process AND the database are reachable"
        );
        assert_eq!(
            body["status"], "degraded",
            "monitoring parses status; it must not be renamed or dropped"
        );

        let raw = serde_json::to_string(&body).expect("the body is JSON");
        let lower = raw.to_lowercase();
        for leak in [
            "apikita_probe",
            "sqlite",
            "unable to open",
            "database file",
            "secret",
            "127.0.0.1",
            "postgres",
            "sqlx",
            "connection",
            "connect",
            "refused",
            "pool",
            "timeout",
            "timed out",
            "os error",
            "select",
        ] {
            assert!(
                !lower.contains(&leak.to_lowercase()),
                "docs/error-model.md rule 1 - leaked {leak:?} to an unauthenticated caller in: {raw}"
            );
        }
        assert!(
            !raw.chars().any(|c| c.is_ascii_digit()),
            "an unauthenticated health body must carry no host, port, errno or count: {raw}"
        );
    }

    /// The other half of the contract: a reachable database is still 200 and
    /// still says healthy/connected. A regression here takes down monitoring and
    /// the deploy gate, so the status code AND the whole body are pinned.
    #[tokio::test]
    async fn a_reachable_database_is_200_healthy_and_connected() {
        // Ported: the Postgres original read DATABASE_URL and dialled a shared
        // server. `TestDb` builds and migrates its own file, so this runs by
        // default and no other test's rows can perturb it.
        let db = TestDb::new().await;
        let pool = db.pool.clone();

        let (status, body) = probe(pool).await;

        assert_eq!(
            status,
            StatusCode::OK,
            "docs/server/api-spec.md:366 - 200 only when the process AND the database are reachable"
        );
        assert_eq!(
            body,
            json!({ "status": "healthy", "database": "connected" }),
            "the healthy contract must not drift - the deploy gate parses this body"
        );

        db.close().await;
    }

    /// The LIVE half of the healthy contract: the first test that drives the
    /// handler against a REAL Postgres through a pool that has not dialled yet, so
    /// the 200 can only come from SELECT 1 actually executing.
    ///
    /// A lazy pool starts with zero connections, which makes the handler own query
    /// the thing that opens them: asserting pool.size() > 0 afterwards proves the
    /// handler reached the database rather than short-circuiting. That is the part
    /// no pure test can cover.
    ///
    /// Ported: the Postgres original read DATABASE_URL and was `#[ignore]`d because
    /// it needed a migrated server. `TestDb` builds and migrates one per test, so
    /// the ignore is gone - which is the whole point of the port (plan section 5.5).
    #[tokio::test]
    async fn live_probe_executes_select_1_against_the_real_database_and_is_200_healthy() {
        let db = TestDb::new().await;

        // A SECOND, lazy pool onto the same migrated file: nothing has been dialled
        // yet, so any connection that exists after the probe was opened BY the probe.
        let options = sqlx::sqlite::SqliteConnectOptions::from_str(&format!(
            "sqlite://{}",
            db.db_path().display()
        ))
        .expect("parse the migrated file as a sqlite url");
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(5))
            .connect_lazy_with(options);
        assert_eq!(
            pool.size(),
            0,
            "the pool must start empty, or this test cannot tell who dialled"
        );

        let (status, body) = probe(pool.clone()).await;

        assert_eq!(
            status,
            StatusCode::OK,
            "docs/server/api-spec.md:366 - 200 only when the process AND the database are reachable"
        );
        assert_eq!(
            body,
            json!({ "status": "healthy", "database": "connected" }),
            "the healthy contract must not drift - the deploy gate parses this body"
        );
        assert!(
            pool.size() > 0,
            "the handler must have executed its probe against the real database; an empty pool here means the 200 came from somewhere other than SELECT 1"
        );

        pool.close().await;
        db.close().await;
    }

    /// The LIVE unavailable branch: a pool that was REALLY connected to the
    /// database and then REALLY torn down, so the handler failure is the driver's
    /// own PoolClosed, not a hand-built error.
    ///
    /// The no-leak assertion compares the body against the driver error text this
    /// process ACTUALLY received, so it cannot go stale the way a hand-written
    /// fragment list does.
    ///
    /// Ported: `TestDb` supplies the migrated database the Postgres original took
    /// from DATABASE_URL, so the ignore is gone.
    #[tokio::test]
    async fn live_probe_on_a_closed_pool_reports_the_constant_and_no_driver_error_fragment() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();

        // Establish a real connection first, so closing the pool is a real teardown
        // of a real connection rather than a never-dialled no-op.
        let warmed: i32 = sqlx::query_scalar("SELECT 1")
            .fetch_one(&pool)
            .await
            .expect("the pool is reachable before it is closed");
        assert_eq!(warmed, 1, "the fixture must start from a working database");

        pool.close().await;
        assert!(pool.is_closed(), "close() must leave the pool unusable");

        // Capture the REAL driver error the handler is about to hit, from the same
        // pool and the same statement. If a closed pool ever starts serving queries
        // again, the unavailable branch is unreachable and this test is meaningless,
        // so that is a failure rather than a skip.
        let driver_error = match sqlx::query("SELECT 1").execute(&pool).await {
            Ok(_) => panic!(
                "a closed pool must not serve queries; the unavailable branch would never be reached"
            ),
            Err(err) => err.to_string(),
        };
        assert!(
            !driver_error.is_empty() && driver_error != DATABASE_UNAVAILABLE,
            "the captured failure must carry real detail, or the leak assertion below is vacuous: {driver_error:?}"
        );

        let (status, body) = probe(pool).await;

        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "docs/server/api-spec.md:366 - 200 only when the process AND the database are reachable"
        );
        assert_eq!(
            body["status"], "degraded",
            "monitoring parses status; it must not be renamed or dropped"
        );
        assert_eq!(
            body["database"], DATABASE_UNAVAILABLE,
            "docs/error-model.md:168 - the caller gets the fixed constant, never the driver error"
        );
        assert_eq!(
            body,
            json!({ "status": "degraded", "database": DATABASE_UNAVAILABLE }),
            "the degraded body is exactly the documented pair and nothing else"
        );

        let raw = serde_json::to_string(&body).expect("the body is JSON");
        assert!(
            !raw.contains(&driver_error),
            "docs/error-model.md rule 1 - the driver error {driver_error:?} reached an unauthenticated caller in: {raw}"
        );

        db.close().await;
    }

    // -----------------------------------------------------------------------
    // GET /api/admin/metrics - the read that makes `error_rate` alertable.
    //
    // `tools/alert/alerts.tsv` lists an `error_rate > 5%` alert marked
    // `needs-metrics`, and both alert scripts justify that with a now-STALE
    // premise: "HTTP counters over a 5-minute window (same access logs)".
    // `error.rs` counts 5xx responses in process, so no access log and no metrics
    // vendor is needed - only a surface an operator can read.
    //
    // It is NOT on /health, and that is deliberate: /health is unauthenticated, its
    // body is PINNED exactly because the DEPLOY GATE parses it, and a leak test
    // forbids ANY ASCII digit in it, naming `count` explicitly.
    // -----------------------------------------------------------------------

    /// A real account holding a REAL sessions row, so the operator guard is reached
    /// through the production authentication path rather than simulated.
    async fn account_with_session(pool: &SqlitePool, operator: bool) -> String {
        let id = Uuid::new_v4();
        let now = chrono::Utc::now();
        sqlx::query(
            "INSERT INTO accounts (id, pb_user_id, is_operator, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(id.hyphenated())
        .bind(format!("test_pb_{}", id.simple()))
        .bind(if operator { 1 } else { 0 })
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .expect("create account");

        // Every NOT NULL column is bound from Rust: the SQLite schema has no DEFAULT
        // for id, last_seen_at or created_at.
        let token = format!("apk_sess_{}", Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO sessions (id, account_id, token_hash, expires_at, last_seen_at, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(Uuid::new_v4().hyphenated())
        .bind(id.hyphenated())
        .bind(crate::routes::hash_token(&token))
        .bind(now + chrono::Duration::days(30))
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .expect("create session");

        token
    }

    fn session_cookie(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            format!("a=1; session={token}; b=2").parse().unwrap(),
        );
        headers
    }

    /// The real application state, built the way main.rs builds it.
    fn metrics_state(pool: SqlitePool) -> AppState {
        for path in ["../config/apikita.toml", "config/apikita.toml"] {
            if std::path::Path::new(path).exists() {
                let config = std::sync::Arc::new(
                    AppConfig::load_from_file(path).expect("parse apikita.toml"),
                );
                let events =
                    std::sync::Arc::new(crate::routes::events::RealtimeHub::new(&config.realtime));
                let trusted_proxies: std::sync::Arc<[crate::ip_tracking::IpCidr]> =
                    std::sync::Arc::from(
                        crate::ip_tracking::parse_cidrs(&config.network.trusted_proxy_cidrs)
                            .expect("config CIDRs parse")
                            .into_boxed_slice(),
                    );
                return AppState {
                    pool,
                    config,
                    http_client: reqwest::Client::new(),
                    events,
                    ip_salt: std::sync::Arc::new(crate::ip_tracking::DailySalt::new()),
                    trusted_proxies,
                };
            }
        }
        panic!("could not find apikita.toml for testing");
    }

    /// Renders the handler the way the router does.
    async fn metrics(state: AppState, headers: HeaderMap) -> (StatusCode, Value) {
        let response = match operator_metrics(State(state), headers).await {
            Ok(response) => response,
            Err(err) => err.into_response(),
        };
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("a metrics response must have a readable body");
        let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, body)
    }

    /// NO COOKIE -> 401, and no counter value is produced or leaked.
    ///
    /// The counts are operational data - how much traffic the service is taking and
    /// how much of it is failing - so an anonymous caller must not read them. This is
    /// the same 401 every other cookie endpoint gives.
    #[tokio::test]
    async fn the_metrics_route_refuses_an_anonymous_caller() {
        let db = TestDb::new().await;
        let state = metrics_state(db.pool.clone());

        let (status, body) = metrics(state, HeaderMap::new()).await;

        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "an anonymous caller must not read operational counts: {body}"
        );
        assert_eq!(body["error"]["code"], json!("unauthenticated"));
        assert!(
            body.get("server_errors").is_none() && body.get("responses").is_none(),
            "a refused response must not leak the counts it is refusing: {body}"
        );

        db.close().await;
    }

    /// A LIVE session on a NON-operator account -> 403, not 401.
    ///
    /// The distinction matters: the caller IS authenticated, we know exactly who they
    /// are, and they may not read this. A 401 would tell a user with a perfectly good
    /// session to log in again for a permissions problem (docs/error-model.md:52-59).
    #[tokio::test]
    async fn the_metrics_route_refuses_a_non_operator_with_403() {
        let db = TestDb::new().await;
        let token = account_with_session(&db.pool, false).await;
        let state = metrics_state(db.pool.clone());

        let (status, body) = metrics(state, session_cookie(&token)).await;

        assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");
        assert_eq!(body["error"]["code"], json!("forbidden"));

        db.close().await;
    }

    /// AN OPERATOR READS THE COUNTS the server actually writes.
    #[tokio::test]
    async fn an_operator_reads_the_error_rate_the_counter_recorded() {
        let db = TestDb::new().await;
        let token = account_with_session(&db.pool, true).await;
        let state = metrics_state(db.pool.clone());

        // Record a KNOWN amount through the real shared counter, then assert the route
        // reports it.
        let counter = crate::error::server_error_counter();
        let errors_before = counter.server_errors();
        let responses_before = counter.responses();
        for _ in 0..3 {
            counter.record_response();
        }
        counter.record_error();

        let (status, body) = metrics(state, session_cookie(&token)).await;

        assert_eq!(status, StatusCode::OK, "body: {body}");
        // MONOTONIC, not an exact value: the counter is process-wide and other tests
        // drive responses through `into_response` concurrently. A route reading a
        // DIFFERENT or stale source would still fail this.
        assert!(
            body["server_errors"].as_u64().unwrap_or(0) >= errors_before + 1,
            "the route must report the shared counter: {body}"
        );
        assert!(
            body["responses"].as_u64().unwrap_or(0) >= responses_before + 3,
            "the denominator must come from the same counter: {body}"
        );
        // The rate must be the QUOTIENT of the two numbers reported alongside it, not
        // a separately-computed figure that could disagree with them.
        let rate = body["error_rate"]
            .as_f64()
            .expect("a rate is reported once responses exist");
        let expected =
            body["server_errors"].as_f64().unwrap() / body["responses"].as_f64().unwrap();
        assert!(
            (rate - expected).abs() < 1e-4,
            "the reported rate must equal errors/responses: {rate} vs {expected}"
        );

        db.close().await;
    }

    /// AN EMPTY WINDOW REPORTS `null`, NEVER `0.0`.
    ///
    /// This is the difference between an alert that works and one that is worse than
    /// nothing. `0.0` reads as "perfectly healthy", so a service that has served
    /// NOTHING - a fresh deploy, a process that just restarted, a service whose
    /// traffic has dried up - would report a flawless error rate and SUPPRESS the
    /// alert. `null` says "no data", which an operator or the alert script must
    /// distinguish from "no errors".
    ///
    /// MUTATION-TESTED: reporting `0.0` for the empty case survives the two
    /// authorisation tests, because every one of them records responses first. This
    /// test is the one that pins the distinction.
    #[tokio::test]
    async fn an_empty_window_reports_unknown_rather_than_a_healthy_zero() {
        // A FRESH counter, so the assertion cannot be perturbed by the process-wide
        // writers other tests create concurrently. The HANDLER's mapping is asserted
        // separately below, on the shared counter, because the mutation that survived
        // was in that mapping - not in the counter.
        let counter = crate::error::ServerErrorCounter::default();

        assert_eq!(
            counter.error_rate(),
            None,
            "no responses must be UNKNOWN, not 0.0 - a zero reads as a healthy service and suppresses the alert"
        );

        // POSITIVE CONTROL: the same accessor DOES return a rate once a response
        // exists, so the assertion above is about the empty case and not about an
        // accessor that always returns None.
        counter.record_response();
        assert_eq!(
            counter.error_rate(),
            Some(0.0),
            "with one clean response the rate is a real 0.0 - which is why the EMPTY case must be distinguishable from it"
        );
    }

    /// The ROUTE's payload reports `null` for an empty window, never `0.0`.
    ///
    /// This calls `metrics_payload`, the SAME function the handler calls, so it cannot
    /// drift from the route's behaviour. That matters, because the first version of
    /// this test re-stated the mapping instead of exercising it, and a mutation that
    /// reported `0.0` for an empty window **survived**: every other test records
    /// responses first, so the process-wide counter is never empty by then.
    #[test]
    fn the_routes_payload_reports_unknown_rather_than_a_healthy_zero() {
        // An EMPTY counter: the fresh-deploy case.
        let payload = metrics_payload(&crate::error::ServerErrorCounter::default());

        assert_eq!(
            payload["server_errors"],
            json!(0),
            "an empty counter has recorded no failures"
        );
        assert_eq!(payload["responses"], json!(0));
        assert_eq!(
            payload["error_rate"],
            json!(null),
            "an empty window must be UNKNOWN - a 0.0 would read as a healthy service and suppress the alert"
        );

        // POSITIVE CONTROL: a genuine zero rate with responses present IS a number,
        // so the two cases are different payloads and the alert can tell them apart.
        let clean = crate::error::ServerErrorCounter::default();
        clean.record_response();
        let payload = metrics_payload(&clean);
        assert_eq!(
            payload["error_rate"],
            json!(0.0),
            "one clean response is a real 0.0 rate, distinguishable from the no-data null"
        );

        // And the rate really is the quotient, rounded, of the two counts beside it.
        // FOUR responses, one of them a failure = 0.25. Note that `record_error` does
        // NOT also record a response - production calls both explicitly for a 5xx
        // (`into_response` records the response, then records the error), so the two
        // counters here must be driven the same way. Getting this wrong is how the
        // first version of this test read 1/3 instead of 1/4.
        let mixed = crate::error::ServerErrorCounter::default();
        for _ in 0..4 {
            mixed.record_response();
        }
        mixed.record_error();
        let payload = metrics_payload(&mixed);
        assert_eq!(payload["server_errors"], json!(1));
        assert_eq!(payload["responses"], json!(4));
        assert_eq!(payload["error_rate"], json!(0.25));
    }
}
