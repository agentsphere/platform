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
    pub project_id: Option<Uuid>,
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
    pub parent_session_id: Option<Uuid>,
    pub spawn_depth: i32,
    pub allowed_child_roles: Option<Vec<String>>,
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
    /// Agent role controls which MCP servers are loaded.
    /// One of: "dev" (default), "ops", "admin", "ui", "test".
    #[serde(default)]
    pub role: Option<String>,
    /// Container image override for this session.
    #[serde(default)]
    pub image: Option<String>,
    /// Shell commands to run after git clone but before the agent starts.
    #[serde(default)]
    pub setup_commands: Option<Vec<String>>,
    /// Browser sidecar configuration. When present, a headless Chromium sidecar
    /// is added to the agent pod and a Playwright MCP server is made available.
    /// Only allowed for roles: "ui", "test".
    #[serde(default)]
    pub browser: Option<BrowserConfig>,
}

/// Configuration for the headless browser sidecar.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserConfig {
    /// Allowed origin URLs the agent can navigate to via MCP tools.
    /// e.g. `["http://localhost:3000", "http://preview-myapp.platform-agents.svc:80"]`
    /// Validated by the MCP server before each navigation — the browser itself is unrestricted.
    pub allowed_origins: Vec<String>,
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

/// Parameters for building an agent pod at the provider trait boundary.
pub struct BuildPodParams<'a> {
    pub session: &'a AgentSession,
    pub config: &'a ProviderConfig,
    pub agent_api_token: &'a str,
    pub platform_api_url: &'a str,
    pub repo_clone_url: &'a str,
    pub namespace: &'a str,
    pub project_agent_image: Option<&'a str>,
    /// User-provided Anthropic API key. If set, used instead of the global K8s secret.
    pub anthropic_api_key: Option<&'a str>,
}

/// Trait for agent provider implementations.
/// Uses native async fn in trait (Rust 2024 edition).
pub trait AgentProvider: Send + Sync {
    /// Build the K8s Pod object for this agent session.
    fn build_pod(&self, params: BuildPodParams<'_>) -> Result<Pod, AgentError>;

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
        assert!(config.role.is_none());
        assert!(config.image.is_none());
        assert!(config.setup_commands.is_none());
        assert!(config.browser.is_none());
    }

    #[test]
    fn provider_config_full() {
        let config: ProviderConfig = serde_json::from_str(
            r#"{"model":"claude-sonnet-4-5-20250929","max_turns":10,"role":"ops"}"#,
        )
        .unwrap();
        assert_eq!(config.model.as_deref(), Some("claude-sonnet-4-5-20250929"));
        assert_eq!(config.max_turns, Some(10));
        assert_eq!(config.role.as_deref(), Some("ops"));
    }

    #[test]
    fn provider_config_ignores_unknown_fields() {
        let config: ProviderConfig =
            serde_json::from_str(r#"{"model":"opus","unknown_field":true}"#).unwrap();
        assert_eq!(config.model.as_deref(), Some("opus"));
    }

    #[test]
    fn provider_config_with_image_and_setup() {
        let config: ProviderConfig = serde_json::from_str(
            r#"{"image":"golang:1.23","setup_commands":["go mod download","go build ./..."]}"#,
        )
        .unwrap();
        assert_eq!(config.image.as_deref(), Some("golang:1.23"));
        let cmds = config.setup_commands.unwrap();
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0], "go mod download");
    }

    #[test]
    fn progress_kind_serializes_snake_case() {
        let json = serde_json::to_string(&ProgressKind::ToolCall).unwrap();
        assert_eq!(json, r#""tool_call""#);
    }

    #[test]
    fn provider_config_with_browser() {
        let config: ProviderConfig = serde_json::from_str(
            r#"{"role":"ui","browser":{"allowed_origins":["http://localhost:3000","http://myapp:8080"]}}"#,
        )
        .unwrap();
        assert_eq!(config.role.as_deref(), Some("ui"));
        let browser = config.browser.unwrap();
        assert_eq!(browser.allowed_origins.len(), 2);
        assert_eq!(browser.allowed_origins[0], "http://localhost:3000");
        assert_eq!(browser.allowed_origins[1], "http://myapp:8080");
    }

    #[test]
    fn provider_config_without_browser() {
        let config: ProviderConfig =
            serde_json::from_str(r#"{"role":"dev","model":"opus"}"#).unwrap();
        assert!(config.browser.is_none());
    }

    #[test]
    fn browser_config_roundtrip() {
        let config = BrowserConfig {
            allowed_origins: vec!["https://example.com".into()],
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: BrowserConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.allowed_origins, config.allowed_origins);
    }
}
