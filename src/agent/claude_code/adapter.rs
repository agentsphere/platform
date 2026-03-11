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
        }))
    }

    fn parse_progress(&self, line: &str) -> Option<ProgressEvent> {
        progress::parse_line(line)
    }

    fn name(&self) -> &'static str {
        "claude-code"
    }
}
