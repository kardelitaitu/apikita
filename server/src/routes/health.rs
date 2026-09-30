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
/// (docs/error-model.md, "Response shape": the extractor's own detail text is
/// deliberately NOT echoed into `message`). Same model as `AppError::client_message`
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
pub(crate) fn metrics_payload(
    counter: &crate::error::ServerErrorCounter,
    unhealthy_models: &[String],
    retention: &crate::db::RetentionLag,
    today: chrono::NaiveDate,
) -> serde_json::Value {
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
        // The all_providers_unhealthy condition (the "All providers unhealthy" row of
        // the Alerts table in docs/observability.md). A LIST, empty when every model has
        // at least one usable endpoint. Named rather than a boolean so an operator
        // learns WHICH model is down without a second lookup.
        //
        // CITED BY ROW, NOT BY LINE, and that is a correction rather than a style
        // choice: this comment read docs/observability.md:104, which is the "Relay
        // down" row. The row it meant had moved to 105 when that table gained an
        // entry, so the citation pointed a reader at a completely different alert and
        // still read as a plausible reference. The repo already knows this - it fails
        // if alerts.tsv cites observability.md:N, for the same reason - and the same
        // rule now applies here.
        "unhealthy_models": unhealthy_models,
        // The db_disk alert's REAL question (the "DB disk" row of the Alerts table in
        // docs/observability.md): its condition
        // column says "volume usage" but its ACTION says "check retention". This is the
        // age of the oldest row per age-based table, present ONLY when a row is past
        // that table's window. An empty object means retention is keeping up.
        //
        // Deliberately NOT a disk percentage: 80% full is normal for a working
        // database, while a sweep that stopped is an incident at any size, because it
        // breaks a published retention promise.
        "retention": retention_report(retention, today),
    })
}

/// The retention half of the metrics payload.
///
/// Split out so the JSON shape is testable without a database, the same reasoning that
/// produced `metrics_payload`. The window each table is measured against is included,
/// so a reader can see the promise without opening the source.
fn retention_report(
    retention: &crate::db::RetentionLag,
    today: chrono::NaiveDate,
) -> serde_json::Value {
    let windows = json!({
        "usage_events": crate::db::USAGE_EVENTS_RETENTION_DAYS,
        "usage_daily": crate::db::USAGE_DAILY_RETENTION_DAYS,
        "sessions": crate::db::SESSION_RETENTION_DAYS,
        "key_ip_seen": crate::ip_tracking::SEEN_RETENTION_DAYS,
        "key_ip_daily": crate::ip_tracking::DAILY_RETENTION_DAYS,
        "link_redemption_attempts": crate::db::LINK_ATTEMPT_RETENTION_DAYS,
        "auth_attempts": crate::ip_tracking::AUTH_ATTEMPT_RETENTION_DAYS,
        "link_code_issues": crate::ip_tracking::LINK_CODE_ISSUE_RETENTION_DAYS,
        // ZERO, and it reads oddly beside the others, which is why the reason is
        // here rather than only on the constant. A link is not kept for a period:
        // the sweep deletes it the moment it expires, so the honest window is "no
        // grace period", and the number that expresses that is 0.
        "identity_tokens": crate::db::IDENTITY_TOKEN_LAG_DAYS,
    });

    // ONLY the tables that are behind, so an empty object reads as "retention is
    // keeping up" and a non-empty one names exactly what is overdue.
    let mut oldest_behind = serde_json::Map::new();
    for (table, age) in retention.oldest_days_by_table() {
        if let Some(days) = age {
            oldest_behind.insert(table.to_string(), json!(days));
        }
    }

    json!({
        "behind": retention.anything_behind(),
        "oldest_days": serde_json::Value::Object(oldest_behind),
        "windows_days": windows,
        "checked_on": today.to_string(),
    })
}

pub async fn operator_metrics(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AdminError> {
    // The guard runs FIRST, before a single counter is read, so a refused caller
    // cannot learn anything from this route.
    crate::routes::admin::require_operator(&state, &headers).await?;

    // The retention read is the one part of this route that touches the database, and
    // it is deliberately NOT allowed to fail the whole response: an operator asking for
    // the counters during an incident should still get them if the retention query
    // errors. A failed read is reported as a distinct field, never as "no lag".
    let today = crate::ip_tracking::today_utc();
    let retention = match crate::db::retention_lag(&state.pool, today).await {
        Ok(lag) => lag,
        Err(err) => {
            tracing::error!(error = %err, "metrics: the retention-lag read failed");
            let counter = crate::error::server_error_counter();
            // Sentinel: "behind" is NULL rather than false, so a consumer can tell
            // "retention is fine" from "we could not tell".
            return Ok(Json(json!({
                "server_errors": counter.server_errors(),
                "responses": counter.responses(),
                "error_rate": counter.error_rate(),
                "unhealthy_models": crate::routes::proxy::models_with_no_healthy_endpoint(),
                "retention": { "behind": null, "error": "the retention read failed" },
            }))
            .into_response());
        }
    };

    Ok(Json(metrics_payload(
        crate::error::server_error_counter(),
        &crate::routes::proxy::models_with_no_healthy_endpoint(),
        &retention,
        today,
    ))
    .into_response())
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

    /// docs/error-model.md ("Response shape": the extractor's own detail text is
    /// deliberately NOT echoed into `message`) - never leak internals. The health
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
            "docs/error-model.md, Response shape - the caller gets the fixed constant, never the driver error"
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
            "INSERT INTO accounts (id, is_operator, created_at, updated_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(id.hyphenated())
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
    /// session to log in again for a permissions problem (docs/error-model.md, 401 vs 403).
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
            body["server_errors"].as_u64().unwrap_or(0) > errors_before,
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

    /// The retention half of the payload: empty when healthy, NAMED when behind.
    ///
    /// `db_disk`'s action is "check retention", so an operator needs to know WHICH
    /// table is overdue and by how long - a bare boolean would send them to the
    /// database to find out.
    #[test]
    fn the_retention_report_names_the_tables_that_are_behind() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();

        // Healthy: behind is false and the named list is EMPTY, not missing, so a
        // consumer can always index it.
        let healthy = crate::db::RetentionLag::default();
        let report = retention_report(&healthy, today);
        assert_eq!(report["behind"], json!(false));
        assert_eq!(report["oldest_days"], json!({}));
        assert_eq!(report["checked_on"], json!("2026-06-01"));

        // Behind on ONE table: only that table is named, with its real age.
        let behind = crate::db::RetentionLag {
            usage_events: Some(200),
            usage_daily: None,
            sessions: None,
            key_ip_seen: None,
            key_ip_daily: None,
            link_redemption_attempts: None,
            auth_attempts: None,
            identity_tokens: None,
            link_code_issues: None,
        };
        let report = retention_report(&behind, today);
        assert_eq!(report["behind"], json!(true));
        assert_eq!(
            report["oldest_days"],
            json!({ "usage_events": 200 }),
            "only the LAGGING table is named: listing a healthy table would suggest it is overdue"
        );

        // The window each table is measured against is visible, so the promise is not
        // a mystery number in the payload.
        assert_eq!(
            report["windows_days"]["usage_events"],
            json!(crate::db::USAGE_EVENTS_RETENTION_DAYS)
        );

        // EVERY MEASURED TABLE HAS A WINDOW, and this is the check that keeps the
        // `windows` map from silently falling behind the struct. The map is written
        // by hand and `oldest_days_by_table` is written by hand, and they were
        // allowed to disagree: `link_code_issues` and `identity_tokens` were added
        // to the struct while the map still named seven tables, so the payload would
        // have reported an age with no window to read it against - the operator sees
        // "identity_tokens is 400 days behind" and has to open the source to learn
        // that the answer is zero.
        //
        // Both lists are compared as sets, so a NEW field fails here until it is
        // given a window, which is the point: adding a measurement is not finished
        // until the promise it is measured against is in the payload too.
        let windows = report["windows_days"]
            .as_object()
            .expect("windows_days is an object");
        for (table, _) in crate::db::RetentionLag::default().oldest_days_by_table() {
            assert!(
                windows.contains_key(table),
                "`{table}` is measured by retention_lag but has no entry in `windows_days`, \
                 so the payload reports an age with no window to read it against"
            );
        }
        assert_eq!(
            windows.len(),
            crate::db::RetentionLag::default()
                .oldest_days_by_table()
                .len(),
            "the payload publishes {} windows and the sweep measures {} tables - one of the \
             two lists was edited without the other",
            windows.len(),
            crate::db::RetentionLag::default()
                .oldest_days_by_table()
                .len()
        );
    }

    /// The payload REPORTS the unhealthy models by NAME, and an empty list when all
    /// are healthy. This is the `all_providers_unhealthy` condition.
    ///
    /// Named rather than a boolean on purpose: an alert saying "a provider is down"
    /// without saying WHICH costs a second investigation, and the operator already has
    /// the route open.
    #[test]
    fn the_payload_names_every_model_with_no_usable_endpoint() {
        let counter = crate::error::ServerErrorCounter::default();

        // Healthy: an EMPTY list, not a missing key, so a consumer can always index it.
        let payload = metrics_payload(
            &counter,
            &[],
            &Default::default(),
            chrono::NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
        );
        assert_eq!(
            payload["unhealthy_models"],
            json!([]),
            "an all-healthy service reports an EMPTY list, never a missing key"
        );

        // Down: the names are carried through verbatim.
        let down = vec![
            "deepseek-v4-flash".to_string(),
            "deepseek-v4-pro".to_string(),
        ];
        let payload = metrics_payload(
            &counter,
            &down,
            &Default::default(),
            chrono::NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
        );
        assert_eq!(payload["unhealthy_models"], json!(down));
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
        let payload = metrics_payload(
            &crate::error::ServerErrorCounter::default(),
            &[],
            &Default::default(),
            chrono::NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
        );

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
        let payload = metrics_payload(
            &clean,
            &[],
            &Default::default(),
            chrono::NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
        );
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
        let payload = metrics_payload(
            &mixed,
            &[],
            &Default::default(),
            chrono::NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
        );
        assert_eq!(payload["server_errors"], json!(1));
        assert_eq!(payload["responses"], json!(4));
        assert_eq!(payload["error_rate"], json!(0.25));
    }
    /// A FAILED RETENTION READ IS NOT "RETENTION IS FINE".
    ///
    /// The handler deliberately does not fail the whole response when the retention
    /// query errors - "an operator asking for the counters during an incident should
    /// still get them". What it returns instead is a SENTINEL (health.rs:187-195):
    ///
    ///   "behind" is NULL rather than false, so a consumer can tell retention-is-fine
    ///   from we-could-not-tell."
    ///
    /// NOTHING PINNED THAT DISTINCTION. The healthy path is tested
    /// (the_retention_report_names_the_tables_that_are_behind) and the failure path
    /// was not, so a refactor could collapse null to false and every test would stay
    /// green. That collapse is the dangerous direction: false means fine and null
    /// means unknown, so a monitoring consumer treating a falsy value the same either
    /// way would read a BROKEN retention query as a HEALTHY one - a blind spot
    /// reported as an all-clear, the same shape as the alert cooldown that claimed to
    /// be in force (W52) and the gate whose failure could not fire (W43).
    #[tokio::test]
    async fn a_failed_retention_read_reports_unknown_not_fine() {
        let db = TestDb::new().await;
        let token = account_with_session(&db.pool, true).await;
        let state = metrics_state(db.pool.clone());

        // Force the retention read to fail the way it would in production - a broken
        // or half-migrated database - rather than by mocking it: retention_lag reads
        // these tables, so dropping one is the real error.
        sqlx::query("DROP TABLE usage_events")
            .execute(&db.pool)
            .await
            .expect("the fixture must be able to drop the table the lag query reads");

        let (status, body) = metrics(state, session_cookie(&token)).await;

        // Still a 200: the operator asked for counters during an incident and gets
        // them. Failing the request would hide the counters exactly when they matter.
        assert_eq!(
            status,
            StatusCode::OK,
            "a failed retention read must not fail the whole metrics response: {body}"
        );

        // THE SENTINEL. null and not false: the first says we could not tell, the
        // second says retention is fine, and only one of those is true here.
        assert!(
            body["retention"]["behind"].is_null(),
            "a failed retention read must report behind as null (unknown), NOT false (fine) - got {:?} in {body}",
            body["retention"]["behind"]
        );
        assert_ne!(
            body["retention"]["behind"],
            serde_json::json!(false),
            "false would read as retention-is-fine to a monitoring consumer, which is the opposite of what a failed read means: {body}"
        );

        // And the failure is NAMED, so an operator can see why the field is unknown.
        assert!(
            body["retention"]["error"].is_string(),
            "the failure must be reported in its own field, not left for an operator to infer from a null: {body}"
        );

        // The other half of the stated intent: the counters still arrive. Without this,
        // the assertions above would pass on a handler returning ONLY the sentinel.
        assert!(
            body["server_errors"].is_u64() && body["responses"].is_u64(),
            "the counters must still be present when the retention read fails - that is the whole reason the read is not allowed to fail the response: {body}"
        );
    }
}
