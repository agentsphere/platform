//! Mesh CA error types.

use crate::error::ApiError;

#[derive(Debug, thiserror::Error)]
pub enum MeshError {
    #[error("mesh CA not enabled")]
    NotEnabled,

    #[error("invalid SPIFFE identity: {0}")]
    InvalidSpiffeId(String),

    #[error("certificate generation failed: {0}")]
    CertGeneration(String),

    #[error("CA initialization failed: {0}")]
    CaInit(String),

    #[error(transparent)]
    Db(#[from] sqlx::Error),

    #[error(transparent)]
    Secrets(#[from] anyhow::Error),
}

impl From<MeshError> for ApiError {
    fn from(err: MeshError) -> Self {
        match err {
            MeshError::NotEnabled => Self::ServiceUnavailable("mesh CA not enabled".into()),
            MeshError::InvalidSpiffeId(msg) => Self::BadRequest(msg),
            MeshError::CertGeneration(msg) => {
                tracing::error!(error = %msg, "mesh certificate generation failed");
                Self::Internal(anyhow::anyhow!(msg))
            }
            MeshError::CaInit(msg) => {
                tracing::error!(error = %msg, "mesh CA initialization failed");
                Self::Internal(anyhow::anyhow!(msg))
            }
            MeshError::Db(e) => Self::from(e),
            MeshError::Secrets(e) => Self::Internal(e),
        }
    }
}
