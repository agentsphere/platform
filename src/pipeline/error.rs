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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_definition_maps_to_bad_request() {
        let api: ApiError = PipelineError::InvalidDefinition("bad yaml".into()).into();
        assert!(matches!(api, ApiError::BadRequest(msg) if msg == "bad yaml"));
    }

    #[test]
    fn not_found_maps_to_not_found() {
        let api: ApiError = PipelineError::NotFound.into();
        assert!(matches!(api, ApiError::NotFound(msg) if msg == "pipeline"));
    }

    #[test]
    fn step_failed_maps_to_internal() {
        let api: ApiError = PipelineError::StepFailed {
            name: "build".into(),
            exit_code: 1,
        }
        .into();
        assert!(matches!(api, ApiError::Internal(_)));
    }

    #[test]
    fn other_maps_to_internal() {
        let api: ApiError = PipelineError::Other(anyhow::anyhow!("boom")).into();
        assert!(matches!(api, ApiError::Internal(_)));
    }

    #[test]
    fn display_step_failed() {
        let err = PipelineError::StepFailed {
            name: "build".into(),
            exit_code: 42,
        };
        let msg = err.to_string();
        assert!(msg.contains("build"));
        assert!(msg.contains("42"));
    }
}
