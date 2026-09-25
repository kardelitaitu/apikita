use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
};
use serde_json::json;
use sqlx::PgPool;
use tracing::error;

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

pub async fn health_check(State(pool): State<PgPool>) -> impl IntoResponse {
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use serde_json::{json, Value};
    use sqlx::postgres::PgPoolOptions;
    use std::time::Duration;

    /// Drives the handler exactly the way the router does and reads its body.
    async fn probe(pool: PgPool) -> (StatusCode, Value) {
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
    /// fails for real instead of being simulated. connect_lazy does not dial
    /// until the first query, which is exactly the moment the handler dials.
    fn unreachable_pool() -> PgPool {
        PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(5))
            .connect_lazy("postgres://apikita_probe:secret@127.0.0.1:1/apikita_absent")
            .expect("the probe DSN is a valid postgres url")
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
        let dsn = "postgres://apikita_probe:secret@127.0.0.1:1/apikita_absent";
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
            dsn,
            "apikita_probe",
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
        // Read-only (SELECT 1), so unlike the settlement fixtures in db.rs this
        // is safe to run against the shared local schema.
        let dsn = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
            // The documented local stack default (.env.example, docker-compose.yml).
            "postgres://postgres:dev@localhost:5432/apikita".to_string()
        });
        let pool = crate::db::init_pool(&dsn)
            .await
            .expect("set DATABASE_URL to a reachable Postgres instance");

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
    }

    /// The LIVE half of the healthy contract: the first test that drives the
    /// handler against a REAL Postgres through a pool that has not dialled yet, so
    /// the 200 can only come from SELECT 1 actually executing.
    ///
    /// connect_lazy starts with zero connections, which makes the handler own query
    /// the thing that opens them: asserting pool.size() > 0 afterwards proves the
    /// handler reached the server rather than short-circuiting. That is the part no
    /// pure test can cover, and the reason this one is #[ignore]d.
    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn live_probe_executes_select_1_against_real_postgres_and_is_200_healthy() {
        let dsn = std::env::var("DATABASE_URL")
            .expect("set DATABASE_URL to a migrated Postgres instance");

        // Lazy on purpose: nothing has been dialled yet, so any connection that
        // exists after the probe was opened BY the probe.
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(5))
            .connect_lazy(&dsn)
            .expect("DATABASE_URL is a valid postgres url");
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
            "the handler must have executed its probe against real Postgres; an empty pool here means the 200 came from somewhere other than SELECT 1"
        );
    }

    /// The LIVE unavailable branch: a pool that was REALLY connected to Postgres
    /// and then REALLY torn down, so the handler failure is the driver own
    /// PoolClosed, not a hand-built error.
    ///
    /// The no-leak assertion compares the body against the driver error text this
    /// process ACTUALLY received, so it cannot go stale the way a hand-written
    /// fragment list does.
    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn live_probe_on_a_closed_pool_reports_the_constant_and_no_driver_error_fragment() {
        let dsn = std::env::var("DATABASE_URL")
            .expect("set DATABASE_URL to a migrated Postgres instance");

        let pool = crate::db::init_pool(&dsn)
            .await
            .expect("connect to Postgres");

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
    }
}
