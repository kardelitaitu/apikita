use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Serialize)]
pub struct ApiErrorResponse {
    pub error: ApiErrorBody,
}

#[derive(Debug, Serialize)]
pub struct ApiErrorBody {
    pub code: String,
    pub message: String,
    pub request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("Invalid request: {0}")]
    InvalidRequest(String),

    #[error("Unauthenticated")]
    Unauthenticated,

    #[error("Key revoked")]
    KeyRevoked,

    #[error("Key expired")]
    KeyExpired,

    #[error("Insufficient balance")]
    InsufficientBalance { details: Option<Value> },

    #[error("Key limit exceeded")]
    KeyLimitExceeded { details: Option<Value> },

    #[error("Model not allowed: {0}")]
    ModelNotAllowed(String),

    #[error("Wrong credential type: {0}")]
    WrongCredentialType(String),

    #[error("Resource not found: {0}")]
    NotFound(String),

    #[error("Conflict: {0}")]
    Conflict(String),

    #[error("Validation failed: {0}")]
    ValidationFailed(String),

    #[error("Rate limited")]
    RateLimited { retry_after_secs: u64 },

    #[error("No upstream available")]
    NoUpstreamAvailable,

    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("Internal server error: {0}")]
    Internal(String),
}

impl AppError {
    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::InvalidRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthenticated | Self::KeyRevoked | Self::KeyExpired => StatusCode::UNAUTHORIZED,
            Self::InsufficientBalance { .. } | Self::KeyLimitExceeded { .. } => {
                StatusCode::PAYMENT_REQUIRED
            }
            Self::ModelNotAllowed(_) | Self::WrongCredentialType(_) => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::ValidationFailed(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::NoUpstreamAvailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Database(_) | Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "invalid_request",
            Self::Unauthenticated => "unauthenticated",
            Self::KeyRevoked => "key_revoked",
            Self::KeyExpired => "key_expired",
            Self::InsufficientBalance { .. } => "insufficient_balance",
            Self::KeyLimitExceeded { .. } => "key_limit_exceeded",
            Self::ModelNotAllowed(_) => "model_not_allowed",
            Self::WrongCredentialType(_) => "wrong_credential_type",
            Self::NotFound(_) => "not_found",
            Self::Conflict(_) => "conflict",
            Self::ValidationFailed(_) => "validation_failed",
            Self::RateLimited { .. } => "rate_limited",
            Self::NoUpstreamAvailable => "no_upstream_available",
            Self::Database(_) | Self::Internal(_) => "internal_error",
        }
    }

    pub fn details(&self) -> Option<Value> {
        match self {
            Self::InsufficientBalance { details } | Self::KeyLimitExceeded { details } => {
                details.clone()
            }
            _ => None,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let code = self.code().to_string();
        let message = self.to_string();
        let details = self.details();
        let request_id = format!("req_{}", uuid::Uuid::new_v4().simple());

        let body = ApiErrorResponse {
            error: ApiErrorBody {
                code,
                message,
                request_id,
                details,
            },
        };

        let mut res = (status, Json(body)).into_response();
        if let Self::RateLimited { retry_after_secs } = self {
            if let Ok(val) = retry_after_secs.to_string().parse() {
                res.headers_mut().insert(axum::http::header::RETRY_AFTER, val);
            }
        }
        res
    }
}
