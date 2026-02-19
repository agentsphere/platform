use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum ApiError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("unauthorized")]
    Unauthorized,

    #[error("forbidden")]
    Forbidden,

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("validation error")]
    Validation(Vec<String>),

    #[error("service unavailable: {0}")]
    ServiceUnavailable(String),

    #[error("internal error: {0}")]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, body) = match &self {
            Self::NotFound(msg) => (StatusCode::NOT_FOUND, serde_json::json!({ "error": msg })),
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                serde_json::json!({ "error": "unauthorized" }),
            ),
            Self::Forbidden => (
                StatusCode::FORBIDDEN,
                serde_json::json!({ "error": "forbidden" }),
            ),
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, serde_json::json!({ "error": msg })),
            Self::Conflict(msg) => (StatusCode::CONFLICT, serde_json::json!({ "error": msg })),
            Self::Validation(errors) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                serde_json::json!({ "error": "validation error", "fields": errors }),
            ),
            Self::ServiceUnavailable(msg) => (
                StatusCode::SERVICE_UNAVAILABLE,
                serde_json::json!({ "error": msg }),
            ),
            Self::Internal(err) => {
                tracing::error!(error = %err, "internal server error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({ "error": "internal server error" }),
                )
            }
        };

        (status, axum::Json(body)).into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(err: sqlx::Error) -> Self {
        match &err {
            sqlx::Error::RowNotFound => Self::NotFound("resource not found".into()),
            sqlx::Error::Database(db_err) => {
                if db_err.code().as_deref() == Some("23505") {
                    Self::Conflict("resource already exists".into())
                } else {
                    tracing::error!(error = %err, "database error");
                    Self::Internal(err.into())
                }
            }
            _ => {
                tracing::error!(error = %err, "database error");
                Self::Internal(err.into())
            }
        }
    }
}

impl From<fred::error::Error> for ApiError {
    fn from(err: fred::error::Error) -> Self {
        tracing::error!(error = %err, "valkey error");
        Self::Internal(err.into())
    }
}

impl From<kube::Error> for ApiError {
    fn from(err: kube::Error) -> Self {
        tracing::error!(error = %err, "kubernetes error");
        Self::Internal(err.into())
    }
}
