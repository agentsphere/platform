use crate::error::ApiError;

#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
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
