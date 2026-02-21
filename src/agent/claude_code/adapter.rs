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
        }))
    }

    fn parse_progress(&self, line: &str) -> Option<ProgressEvent> {
        progress::parse_line(line)
    }

    fn name(&self) -> &'static str {
        "claude-code"
    }
}
