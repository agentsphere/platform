use crate::error::ApiError;

#[derive(Debug, thiserror::Error)]
#[allow(dead_code)] // NotFound, StepFailed used in executor error paths
pub enum PipelineError {
    #[error("invalid pipeline definition: {0}")]
    InvalidDefinition(String),

    #[error("pipeline not found")]
    NotFound,

    #[error("step failed: {name} (exit code {exit_code})")]
    StepFailed { name: String, exit_code: i32 },

    #[error(transparent)]
    Db(#[from] sqlx::Error),

    #[error(transparent)]
    Kube(#[from] kube::Error),

    #[error(transparent)]
    Storage(#[from] opendal::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<PipelineError> for ApiError {
    fn from(err: PipelineError) -> Self {
        match err {
            PipelineError::InvalidDefinition(msg) => Self::BadRequest(msg),
            PipelineError::NotFound => Self::NotFound("pipeline".into()),
            PipelineError::StepFailed { .. } => Self::Internal(err.into()),
            PipelineError::Db(e) => Self::from(e),
            PipelineError::Kube(e) => Self::from(e),
            PipelineError::Storage(e) => Self::from(e),
            PipelineError::Other(e) => Self::Internal(e),
        }
    }
}
