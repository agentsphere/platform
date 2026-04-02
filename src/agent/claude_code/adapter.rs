use k8s_openapi::api::core::v1::Pod;

use crate::agent::error::AgentError;
use crate::agent::provider::{AgentProvider, BuildPodParams, ProgressEvent};

use super::pod::{PodBuildParams, build_agent_pod};
use super::progress;

/// Claude Code agent provider implementation.
pub struct ClaudeCodeProvider;

impl AgentProvider for ClaudeCodeProvider {
    fn build_pod(&self, params: BuildPodParams<'_>) -> Result<Pod, AgentError> {
        Ok(build_agent_pod(&PodBuildParams {
            session: params.session,
            config: params.config,
            agent_api_token: params.agent_api_token,
            platform_api_url: params.platform_api_url,
            repo_clone_url: params.repo_clone_url,
            namespace: params.namespace,
            project_agent_image: params.project_agent_image,
            anthropic_api_key: params.anthropic_api_key,
            cli_oauth_token: params.cli_oauth_token,
            extra_env_vars: params.extra_env_vars,
            registry_url: params.registry_url,
            registry_secret_name: params.registry_secret_name,
            valkey_url: params.valkey_url,
            claude_cli_version: params.claude_cli_version,
            host_mount_path: params.host_mount_path,
            claude_cli_path: params.claude_cli_path,
            service_account_name: params.service_account_name,
            registry_push_secret_name: params.registry_push_secret_name,
            registry_push_url: params.registry_push_url,
            project_name: params.project_name,
            session_short_id: params.session_short_id,
            default_runner_image: params.default_runner_image,
            git_clone_image: params.git_clone_image,
            proxy_binary_path: params.proxy_binary_path,
        }))
    }

    fn parse_progress(&self, line: &str) -> Option<ProgressEvent> {
        progress::parse_line(line)
    }

    fn name(&self) -> &'static str {
        "claude-code"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_name_is_claude_code() {
        let provider = ClaudeCodeProvider;
        assert_eq!(provider.name(), "claude-code");
    }

    #[test]
    fn parse_progress_valid_assistant_text() {
        let provider = ClaudeCodeProvider;
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello"}]}}"#;
        let event = provider.parse_progress(line);
        assert!(event.is_some());
        let event = event.unwrap();
        assert_eq!(event.kind, crate::agent::provider::ProgressKind::Text);
        assert_eq!(event.message, "Hello");
    }

    #[test]
    fn parse_progress_invalid_json() {
        let provider = ClaudeCodeProvider;
        let result = provider.parse_progress("not json");
        assert!(result.is_none());
    }

    #[test]
    fn parse_progress_error_event() {
        let provider = ClaudeCodeProvider;
        let line = r#"{"type":"error","error":{"message":"rate limit"}}"#;
        let event = provider.parse_progress(line).unwrap();
        assert_eq!(event.kind, crate::agent::provider::ProgressKind::Error);
        assert!(event.message.contains("rate limit"));
    }

    #[test]
    fn parse_progress_result_success() {
        let provider = ClaudeCodeProvider;
        let line = r#"{"type":"result","subtype":"success","session_id":"s1","is_error":false,"result":"done"}"#;
        let event = provider.parse_progress(line).unwrap();
        assert_eq!(event.kind, crate::agent::provider::ProgressKind::Completed);
    }

    #[test]
    fn parse_progress_unknown_type() {
        let provider = ClaudeCodeProvider;
        let line = r#"{"type":"stream_partial","data":"chunk"}"#;
        let result = provider.parse_progress(line);
        assert!(result.is_none());
    }
}
