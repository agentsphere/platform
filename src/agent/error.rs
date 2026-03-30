use crate::error::ApiError;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("session not found")]
    SessionNotFound,

    #[error("session not running")]
    SessionNotRunning,

    #[error("invalid provider: {0}")]
    InvalidProvider(String),

    #[error("{0}")]
    ConfigurationRequired(String),

    #[error("pod creation failed: {0}")]
    PodCreationFailed(String),

    #[error("pod attach failed: {0}")]
    AttachFailed(String),

    #[error("too many concurrent manager sessions")]
    TooManySessions,

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
            AgentError::InvalidProvider(msg) | AgentError::ConfigurationRequired(msg) => {
                Self::BadRequest(msg)
            }
            AgentError::TooManySessions => {
                Self::BadRequest("too many concurrent manager sessions (max 5)".into())
            }
            AgentError::PodCreationFailed(_)
            | AgentError::AttachFailed(_)
            | AgentError::Db(_)
            | AgentError::Kube(_)
            | AgentError::Other(_) => Self::Internal(err.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_not_found_maps_to_not_found() {
        let api: ApiError = AgentError::SessionNotFound.into();
        assert!(matches!(api, ApiError::NotFound(msg) if msg == "session"));
    }

    #[test]
    fn session_not_running_maps_to_bad_request() {
        let api: ApiError = AgentError::SessionNotRunning.into();
        assert!(matches!(api, ApiError::BadRequest(msg) if msg.contains("not running")));
    }

    #[test]
    fn invalid_provider_maps_to_bad_request() {
        let api: ApiError = AgentError::InvalidProvider("bad provider".into()).into();
        assert!(matches!(api, ApiError::BadRequest(msg) if msg.contains("bad provider")));
    }

    #[test]
    fn configuration_required_maps_to_bad_request() {
        let api: ApiError = AgentError::ConfigurationRequired("no API key".into()).into();
        assert!(matches!(api, ApiError::BadRequest(msg) if msg.contains("no API key")));
    }

    #[test]
    fn too_many_sessions_maps_to_bad_request() {
        let api: ApiError = AgentError::TooManySessions.into();
        assert!(matches!(api, ApiError::BadRequest(msg) if msg.contains("too many")));
    }

    #[test]
    fn pod_creation_failed_maps_to_internal() {
        let api: ApiError = AgentError::PodCreationFailed("timeout".into()).into();
        assert!(matches!(api, ApiError::Internal(_)));
    }

    #[test]
    fn attach_failed_maps_to_internal() {
        let api: ApiError = AgentError::AttachFailed("no stdin".into()).into();
        assert!(matches!(api, ApiError::Internal(_)));
    }

    #[test]
    fn other_error_maps_to_internal() {
        let api: ApiError = AgentError::Other(anyhow::anyhow!("boom")).into();
        assert!(matches!(api, ApiError::Internal(_)));
    }

    #[test]
    fn display_messages_are_descriptive() {
        assert_eq!(AgentError::SessionNotFound.to_string(), "session not found");
        assert_eq!(
            AgentError::SessionNotRunning.to_string(),
            "session not running"
        );
        assert_eq!(
            AgentError::InvalidProvider("foo".into()).to_string(),
            "invalid provider: foo"
        );
        assert_eq!(
            AgentError::ConfigurationRequired("set your key".into()).to_string(),
            "set your key"
        );
        assert_eq!(
            AgentError::TooManySessions.to_string(),
            "too many concurrent manager sessions"
        );
        assert_eq!(
            AgentError::PodCreationFailed("timeout".into()).to_string(),
            "pod creation failed: timeout"
        );
        assert_eq!(
            AgentError::AttachFailed("no stdin".into()).to_string(),
            "pod attach failed: no stdin"
        );
    }
}
