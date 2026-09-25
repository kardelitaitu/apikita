use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
};
use serde_json::json;
use sqlx::SqlitePool;

pub async fn health_check(State(pool): State<SqlitePool>) -> impl IntoResponse {
    match sqlx::query("SELECT 1").execute(&pool).await {
        Ok(_) => (
            StatusCode::OK,
            Json(json!({
                "status": "healthy",
                "database": "connected"
            })),
        ),
        Err(err) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "status": "degraded",
                "database": err.to_string()
            })),
        ),
    }
}
