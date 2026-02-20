use k8s_openapi::api::core::v1::Pod;

use crate::agent::error::AgentError;
use crate::agent::provider::{AgentProvider, AgentSession, ProgressEvent, ProviderConfig};

use super::pod::{PodBuildParams, build_agent_pod};
use super::progress;

/// Claude Code agent provider implementation.
pub struct ClaudeCodeProvider;

impl AgentProvider for ClaudeCodeProvider {
    fn build_pod(
        &self,
        session: &AgentSession,
        config: &ProviderConfig,
        agent_api_token: &str,
        platform_api_url: &str,
        repo_clone_url: &str,
        namespace: &str,
    ) -> Result<Pod, AgentError> {
        Ok(build_agent_pod(&PodBuildParams {
            session,
            config,
            agent_api_token,
            platform_api_url,
            repo_clone_url,
            namespace,
        }))
    }

    fn parse_progress(&self, line: &str) -> Option<ProgressEvent> {
        progress::parse_line(line)
    }

    fn name(&self) -> &'static str {
        "claude-code"
    }
}
