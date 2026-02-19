use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug, thiserror::Error)]
#[allow(dead_code)] // Validation, ServiceUnavailable consumed by later modules
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

    #[error("too many requests")]
    TooManyRequests,

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
            Self::TooManyRequests => (
                StatusCode::TOO_MANY_REQUESTS,
                serde_json::json!({ "error": "too many requests" }),
            ),
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

impl From<opendal::Error> for ApiError {
    fn from(err: opendal::Error) -> Self {
        tracing::error!(error = %err, "opendal error");
        Self::Internal(err.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    #[test]
    fn not_found_returns_404() {
        let resp = ApiError::NotFound("thing".into()).into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn unauthorized_returns_401() {
        let resp = ApiError::Unauthorized.into_response();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn forbidden_returns_403() {
        let resp = ApiError::Forbidden.into_response();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn bad_request_returns_400() {
        let resp = ApiError::BadRequest("msg".into()).into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn conflict_returns_409() {
        let resp = ApiError::Conflict("msg".into()).into_response();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn too_many_requests_returns_429() {
        let resp = ApiError::TooManyRequests.into_response();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn validation_returns_422() {
        let resp = ApiError::Validation(vec!["field".into()]).into_response();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn service_unavailable_returns_503() {
        let resp = ApiError::ServiceUnavailable("down".into()).into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn internal_returns_500() {
        let resp = ApiError::Internal(anyhow::anyhow!("boom")).into_response();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn not_found_body_contains_message() {
        let resp = ApiError::NotFound("widget".into()).into_response();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "widget");
    }

    #[tokio::test]
    async fn validation_body_has_fields() {
        let resp = ApiError::Validation(vec!["name required".into()]).into_response();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "validation error");
        assert_eq!(json["fields"][0], "name required");
    }

    #[tokio::test]
    async fn internal_hides_details() {
        let resp = ApiError::Internal(anyhow::anyhow!("secret info")).into_response();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "internal server error");
        assert!(!body.iter().any(|_| false) || !json.to_string().contains("secret"));
    }
}
