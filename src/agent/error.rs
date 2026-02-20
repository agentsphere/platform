use crate::error::ApiError;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("session not found")]
    SessionNotFound,

    #[error("session not running")]
    SessionNotRunning,

    #[error("invalid provider: {0}")]
    InvalidProvider(String),

    #[error("pod creation failed: {0}")]
    PodCreationFailed(String),

    #[error("pod attach failed: {0}")]
    AttachFailed(String),

    #[error(transparent)]
    Db(#[from] sqlx::Error),

    #[error(transparent)]
    Kube(#[from] kube::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<AgentError> for ApiError {
    fn from(err: AgentError) -> Self {
        match err {
            AgentError::SessionNotFound => Self::NotFound("session".into()),
            AgentError::SessionNotRunning => Self::BadRequest("session not running".into()),
            AgentError::InvalidProvider(msg) => Self::BadRequest(msg),
            AgentError::PodCreationFailed(_)
            | AgentError::AttachFailed(_)
            | AgentError::Db(_)
            | AgentError::Kube(_)
            | AgentError::Other(_) => Self::Internal(err.into()),
        }
    }
}
