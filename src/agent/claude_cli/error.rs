use crate::agent::error::AgentError;

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("claude CLI not found — install via: npm install -g @anthropic-ai/claude-code")]
    CliNotFound,

    #[error("CLI process exited with code {code}: {stderr}")]
    ProcessExit { code: i32, stderr: String },

    #[error("CLI spawn failed: {0}")]
    SpawnFailed(#[source] std::io::Error),

    #[error("stdin write failed: {0}")]
    StdinWrite(#[source] std::io::Error),

    #[error("stdout read failed: {0}")]
    StdoutRead(#[source] std::io::Error),

    #[error("invalid NDJSON: {0}")]
    InvalidJson(#[source] serde_json::Error),

    #[error("init timeout: CLI did not emit system init within {0}s")]
    InitTimeout(u64),

    #[error("CLI process not running")]
    NotRunning,

    #[error("control protocol error: {0}")]
    ControlError(String),

    #[error("session error: {0}")]
    SessionError(String),
}

impl From<CliError> for AgentError {
    fn from(err: CliError) -> Self {
        Self::Other(err.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ApiError;

    #[test]
    fn cli_error_display_messages() {
        assert_eq!(
            CliError::CliNotFound.to_string(),
            "claude CLI not found — install via: npm install -g @anthropic-ai/claude-code"
        );
        assert_eq!(
            CliError::ProcessExit {
                code: 1,
                stderr: "bad".into()
            }
            .to_string(),
            "CLI process exited with code 1: bad"
        );
        assert_eq!(
            CliError::InitTimeout(30).to_string(),
            "init timeout: CLI did not emit system init within 30s"
        );
        assert_eq!(CliError::NotRunning.to_string(), "CLI process not running");
        assert_eq!(
            CliError::ControlError("bad control".into()).to_string(),
            "control protocol error: bad control"
        );
        assert_eq!(
            CliError::SessionError("no session".into()).to_string(),
            "session error: no session"
        );
    }

    #[test]
    fn cli_error_to_agent_error() {
        let agent_err: AgentError = CliError::CliNotFound.into();
        assert!(matches!(agent_err, AgentError::Other(_)));
    }

    #[test]
    fn agent_cli_error_to_api_internal() {
        let agent_err: AgentError = CliError::NotRunning.into();
        let api_err: ApiError = agent_err.into();
        assert!(matches!(api_err, ApiError::Internal(_)));
    }
}
