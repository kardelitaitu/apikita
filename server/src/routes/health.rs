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
}
