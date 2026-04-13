// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

// Forked from src/agent/claude_cli/error.rs — keep in sync manually

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

    #[error("pub/sub error: {0}")]
    PubSubError(String),
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn pubsub_error_display() {
        assert_eq!(
            CliError::PubSubError("connection refused".into()).to_string(),
            "pub/sub error: connection refused"
        );
    }

    #[test]
    fn cli_error_variants_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CliError>();
    }
}
