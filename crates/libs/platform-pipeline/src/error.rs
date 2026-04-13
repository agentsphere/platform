// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Pipeline error types.
//!
//! Note: `From<PipelineError> for ApiError` stays in `src/pipeline/error.rs`
//! since `ApiError` belongs to the main binary's error module.

#[derive(Debug, thiserror::Error)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_invalid_definition() {
        let err = PipelineError::InvalidDefinition("bad yaml".into());
        assert_eq!(err.to_string(), "invalid pipeline definition: bad yaml");
    }

    #[test]
    fn display_not_found() {
        let err = PipelineError::NotFound;
        assert_eq!(err.to_string(), "pipeline not found");
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

    #[test]
    fn from_anyhow_creates_other() {
        let anyhow_err = anyhow::anyhow!("test error");
        let pipeline_err: PipelineError = anyhow_err.into();
        assert!(matches!(pipeline_err, PipelineError::Other(_)));
    }

    #[test]
    fn from_sqlx_creates_db() {
        let sqlx_err = sqlx::Error::RowNotFound;
        let pipeline_err: PipelineError = sqlx_err.into();
        assert!(matches!(pipeline_err, PipelineError::Db(_)));
    }

    #[test]
    fn from_opendal_creates_storage() {
        let opendal_err = opendal::Error::new(opendal::ErrorKind::Unexpected, "test");
        let pipeline_err: PipelineError = opendal_err.into();
        assert!(matches!(pipeline_err, PipelineError::Storage(_)));
    }
}
