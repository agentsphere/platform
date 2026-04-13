// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

use platform_types::ApiError;

#[derive(Debug, thiserror::Error)]
pub enum ObserveError {
    #[error("invalid OTLP payload: {0}")]
    InvalidPayload(String),

    #[error("ingest buffer full")]
    BackpressureFull,

    #[error("invalid alert rule: {0}")]
    InvalidAlertRule(String),

    #[error(transparent)]
    Db(#[from] sqlx::Error),

    #[error(transparent)]
    Storage(#[from] opendal::Error),

    #[error(transparent)]
    Arrow(#[from] arrow::error::ArrowError),

    #[error(transparent)]
    Parquet(#[from] parquet::errors::ParquetError),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<ObserveError> for ApiError {
    fn from(err: ObserveError) -> Self {
        match err {
            ObserveError::InvalidPayload(msg) | ObserveError::InvalidAlertRule(msg) => {
                Self::BadRequest(msg)
            }
            ObserveError::BackpressureFull => Self::ServiceUnavailable("ingest buffer full".into()),
            ObserveError::Db(e) => Self::from(e),
            ObserveError::Storage(e) => Self::from(e),
            _ => Self::Internal(err.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    #[test]
    fn invalid_payload_maps_to_bad_request() {
        let err = ObserveError::InvalidPayload("bad data".into());
        let api_err: ApiError = err.into();
        assert_eq!(api_err.into_response().status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn backpressure_maps_to_service_unavailable() {
        let err = ObserveError::BackpressureFull;
        let api_err: ApiError = err.into();
        assert_eq!(
            api_err.into_response().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn invalid_alert_rule_maps_to_bad_request() {
        let err = ObserveError::InvalidAlertRule("bad rule".into());
        let api_err: ApiError = err.into();
        assert_eq!(api_err.into_response().status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn db_row_not_found_maps_to_not_found() {
        let err = ObserveError::Db(sqlx::Error::RowNotFound);
        let api_err: ApiError = err.into();
        assert_eq!(api_err.into_response().status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn arrow_error_maps_to_internal() {
        let err = ObserveError::Arrow(arrow::error::ArrowError::InvalidArgumentError(
            "test".into(),
        ));
        let api_err: ApiError = err.into();
        assert_eq!(
            api_err.into_response().status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn parquet_error_maps_to_internal() {
        let err = ObserveError::Parquet(parquet::errors::ParquetError::General("test".into()));
        let api_err: ApiError = err.into();
        assert_eq!(
            api_err.into_response().status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn other_error_maps_to_internal() {
        let err = ObserveError::Other(anyhow::anyhow!("something failed"));
        let api_err: ApiError = err.into();
        assert_eq!(
            api_err.into_response().status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
