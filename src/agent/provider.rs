use chrono::{DateTime, Utc};
use k8s_openapi::api::core::v1::Pod;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::error::AgentError;

// ---------------------------------------------------------------------------
// Agent session (internal DB model)
// ---------------------------------------------------------------------------

/// Represents an `agent_sessions` row for internal use.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AgentSession {
    pub id: Uuid,
    pub project_id: Uuid,
    pub user_id: Uuid,
    pub agent_user_id: Option<Uuid>,
    pub prompt: String,
    pub status: String,
    pub branch: Option<String>,
    pub pod_name: Option<String>,
    pub provider: String,
    pub provider_config: Option<serde_json::Value>,
    pub cost_tokens: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------------
// Provider configuration
// ---------------------------------------------------------------------------

/// Provider-specific configuration passed at session creation.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub max_turns: Option<i32>,
}

// ---------------------------------------------------------------------------
// Progress events
// ---------------------------------------------------------------------------

/// Structured progress event parsed from agent output.
#[derive(Debug, Clone, Serialize)]
pub struct ProgressEvent {
    pub kind: ProgressKind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProgressKind {
    Thinking,
    ToolCall,
    ToolResult,
    Milestone,
    Error,
    Completed,
    Text,
}

// ---------------------------------------------------------------------------
// AgentProvider trait
// ---------------------------------------------------------------------------

/// Trait for agent provider implementations.
/// Uses native async fn in trait (Rust 2024 edition).
pub trait AgentProvider: Send + Sync {
    /// Build the K8s Pod object for this agent session.
    fn build_pod(
        &self,
        session: &AgentSession,
        config: &ProviderConfig,
        agent_api_token: &str,
        platform_api_url: &str,
        repo_clone_url: &str,
        namespace: &str,
    ) -> Result<Pod, AgentError>;

    /// Parse a single line of streaming output into a structured progress event.
    fn parse_progress(&self, line: &str) -> Option<ProgressEvent>;

    /// Provider name identifier (e.g., "claude-code").
    #[allow(dead_code)]
    fn name(&self) -> &'static str;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_config_defaults() {
        let config: ProviderConfig = serde_json::from_str("{}").unwrap();
        assert!(config.model.is_none());
        assert!(config.max_turns.is_none());
    }

    #[test]
    fn provider_config_full() {
        let config: ProviderConfig =
            serde_json::from_str(r#"{"model":"claude-sonnet-4-5-20250929","max_turns":10}"#)
                .unwrap();
        assert_eq!(config.model.as_deref(), Some("claude-sonnet-4-5-20250929"));
        assert_eq!(config.max_turns, Some(10));
    }

    #[test]
    fn provider_config_ignores_unknown_fields() {
        let config: ProviderConfig =
            serde_json::from_str(r#"{"model":"opus","unknown_field":true}"#).unwrap();
        assert_eq!(config.model.as_deref(), Some("opus"));
    }

    #[test]
    fn progress_kind_serializes_snake_case() {
        let json = serde_json::to_string(&ProgressKind::ToolCall).unwrap();
        assert_eq!(json, r#""tool_call""#);
    }
}
