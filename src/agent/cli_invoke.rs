use std::time::Duration;

use uuid::Uuid;

use super::claude_cli::CliMessage;
use super::claude_cli::messages::ResultMessage;
use super::claude_cli::session::cli_message_to_progress;
use super::claude_cli::transport::{CliSpawnOptions, SubprocessTransport};
use super::error::AgentError;
use super::pubsub_bridge;

/// Timeout for a single CLI invocation (5 minutes).
const CLI_INVOKE_TIMEOUT: Duration = Duration::from_secs(300);

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Structured output from a CLI invocation with `--json-schema`.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct StructuredResponse {
    pub text: String,
    pub tools: Vec<ToolRequest>,
}

/// A tool call requested by the LLM via structured output.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ToolRequest {
    pub name: String,
    pub parameters: serde_json::Value,
}

/// Parameters for a one-shot CLI invocation.
pub struct CliInvokeParams {
    pub session_id: Uuid,
    pub prompt: String,
    pub is_resume: bool,
    pub system_prompt: Option<String>,
    pub oauth_token: Option<String>,
    pub anthropic_api_key: Option<String>,
    pub max_turns: Option<u32>,
}

// ---------------------------------------------------------------------------
// invoke_cli
// ---------------------------------------------------------------------------

/// Spawn `claude -p` with structured output, read NDJSON, publish events.
///
/// Returns the parsed `StructuredResponse` (text + tool requests) and
/// the raw `ResultMessage` for cost tracking.
///
/// Publishes `ProgressEvent`s to Valkey pub/sub `session:{id}:events` in real-time.
#[tracing::instrument(skip(params, valkey), fields(session_id = %params.session_id), err)]
pub async fn invoke_cli(
    params: CliInvokeParams,
    valkey: &fred::clients::Pool,
) -> Result<(StructuredResponse, Option<ResultMessage>), AgentError> {
    let session_id_str = params.session_id.to_string();

    let opts = CliSpawnOptions {
        prompt: Some(params.prompt),
        system_prompt: params.system_prompt,
        resume_session: if params.is_resume {
            Some(session_id_str.clone())
        } else {
            None
        },
        initial_session_id: if params.is_resume {
            None
        } else {
            Some(session_id_str)
        },
        json_schema: Some(serde_json::to_string(&create_app_schema()).unwrap_or_default()),
        disable_tools: true,
        oauth_token: params.oauth_token,
        anthropic_api_key: params.anthropic_api_key,
        max_turns: params.max_turns.or(Some(1)),
        permission_mode: Some("bypassPermissions".into()),
        ..Default::default()
    };

    let mut transport = SubprocessTransport::spawn(opts)
        .map_err(|e| AgentError::Other(anyhow::anyhow!("CLI spawn failed: {e}")))?;

    let result = tokio::time::timeout(CLI_INVOKE_TIMEOUT, async {
        read_cli_output(&mut transport, params.session_id, valkey).await
    })
    .await
    .map_err(|_| {
        AgentError::Other(anyhow::anyhow!(
            "CLI invocation timed out ({}s)",
            CLI_INVOKE_TIMEOUT.as_secs()
        ))
    })?;

    // Always kill the process
    let _ = transport.kill().await;

    let (result_msg, cli_session_id) = result?;

    // Parse structured output from result message
    let structured = parse_structured_output(&result_msg);

    tracing::debug!(
        session_id = %params.session_id,
        cli_session = ?cli_session_id,
        tools_count = structured.tools.len(),
        "CLI invocation complete"
    );

    Ok((structured, Some(result_msg)))
}

/// Read NDJSON messages from the CLI subprocess, publish progress events,
/// and return the `ResultMessage`.
async fn read_cli_output(
    transport: &mut SubprocessTransport,
    session_id: Uuid,
    valkey: &fred::clients::Pool,
) -> Result<(ResultMessage, Option<String>), AgentError> {
    let mut result_msg: Option<ResultMessage> = None;
    let mut cli_session_id: Option<String> = None;

    while let Some(msg) = transport
        .recv()
        .await
        .map_err(|e| AgentError::Other(anyhow::anyhow!("CLI read error: {e}")))?
    {
        // Track CLI session ID from system init
        if let CliMessage::System(ref sys) = msg {
            cli_session_id = Some(sys.session_id.clone());
        }

        // Publish progress event to Valkey pub/sub
        if let Some(event) = cli_message_to_progress(&msg) {
            let _ = pubsub_bridge::publish_event(valkey, session_id, &event).await;
        }

        // Capture result message
        if let CliMessage::Result(r) = msg {
            result_msg = Some(r);
            break;
        }
    }

    let result = result_msg
        .ok_or_else(|| AgentError::Other(anyhow::anyhow!("CLI exited without a result message")))?;

    Ok((result, cli_session_id))
}

/// Parse the structured output from a `ResultMessage`.
///
/// Falls back to using `result` as text with empty tools if `structured_output`
/// is absent or malformed.
fn parse_structured_output(result: &ResultMessage) -> StructuredResponse {
    if let Some(ref structured) = result.structured_output {
        if let Ok(response) = serde_json::from_value::<StructuredResponse>(structured.clone()) {
            return response;
        }
        tracing::warn!("failed to parse structured_output, falling back to result text");
    }

    // Fallback: use result text with no tools
    StructuredResponse {
        text: result
            .result
            .as_deref()
            .unwrap_or("No response from agent")
            .to_owned(),
        tools: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Format tool execution results for feeding back via `--resume`.
pub fn format_tool_results(results: &[(String, Result<serde_json::Value, String>)]) -> String {
    let mut parts = Vec::new();
    for (name, result) in results {
        match result {
            Ok(value) => parts.push(format!("{name}: success — {value}")),
            Err(err) => parts.push(format!("{name}: error — {err}")),
        }
    }
    format!("Tool results:\n{}", parts.join("\n"))
}

/// The JSON schema for create-app structured output.
pub fn create_app_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "text": { "type": "string", "description": "Your response to the user" },
            "tools": {
                "type": "array",
                "description": "List of tools to execute. Empty array if no tools needed.",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "enum": ["create_project", "spawn_coding_agent"] },
                        "parameters": { "type": "object" }
                    },
                    "required": ["name", "parameters"]
                }
            }
        },
        "required": ["text", "tools"]
    })
}

/// Update `cost_tokens` on a session from a CLI result message.
pub async fn update_session_cost(
    pool: &sqlx::PgPool,
    session_id: Uuid,
    result: &ResultMessage,
) -> Result<(), AgentError> {
    if let Some(ref usage) = result.usage {
        let total = usage
            .input_tokens
            .unwrap_or(0)
            .saturating_add(usage.output_tokens.unwrap_or(0));
        sqlx::query!(
            "UPDATE agent_sessions SET cost_tokens = COALESCE(cost_tokens, 0) + $2 WHERE id = $1",
            session_id,
            i64::try_from(total).unwrap_or(i64::MAX),
        )
        .execute(pool)
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_response_deserialize() {
        let json = r#"{"text":"I'll create the project.","tools":[{"name":"create_project","parameters":{"name":"my-app"}}]}"#;
        let resp: StructuredResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.text, "I'll create the project.");
        assert_eq!(resp.tools.len(), 1);
        assert_eq!(resp.tools[0].name, "create_project");
        assert_eq!(resp.tools[0].parameters["name"], "my-app");
    }

    #[test]
    fn structured_response_no_tools() {
        let json = r#"{"text":"What framework do you want?","tools":[]}"#;
        let resp: StructuredResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.text, "What framework do you want?");
        assert!(resp.tools.is_empty());
    }

    #[test]
    fn structured_response_multiple_tools() {
        let json = r#"{"text":"Creating...","tools":[{"name":"create_project","parameters":{"name":"blog"}},{"name":"spawn_coding_agent","parameters":{"project_id":"abc","prompt":"build it"}}]}"#;
        let resp: StructuredResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.tools.len(), 2);
        assert_eq!(resp.tools[0].name, "create_project");
        assert_eq!(resp.tools[1].name, "spawn_coding_agent");
    }

    #[test]
    fn tool_request_roundtrip() {
        let req = ToolRequest {
            name: "create_project".into(),
            parameters: serde_json::json!({"name": "test"}),
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: ToolRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "create_project");
    }

    #[test]
    fn create_app_schema_valid() {
        let schema = create_app_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["text"].is_object());
        assert!(schema["properties"]["tools"].is_object());
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::json!("text")));
        assert!(required.contains(&serde_json::json!("tools")));
    }

    #[test]
    fn create_app_schema_tools_enum() {
        let schema = create_app_schema();
        let tool_enum = &schema["properties"]["tools"]["items"]["properties"]["name"]["enum"];
        let names: Vec<&str> = tool_enum
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(names.contains(&"create_project"));
        assert!(names.contains(&"spawn_coding_agent"));
    }

    #[test]
    fn format_tool_results_success() {
        let results = vec![(
            "create_project".to_owned(),
            Ok(serde_json::json!({"project_id": "abc"})),
        )];
        let formatted = format_tool_results(&results);
        assert!(formatted.contains("create_project: success"));
        assert!(formatted.contains("abc"));
    }

    #[test]
    fn format_tool_results_error() {
        let results = vec![("create_project".to_owned(), Err("name taken".into()))];
        let formatted = format_tool_results(&results);
        assert!(formatted.contains("create_project: error"));
        assert!(formatted.contains("name taken"));
    }

    #[test]
    fn format_tool_results_mixed() {
        let results = vec![
            (
                "create_project".to_owned(),
                Ok(serde_json::json!({"id": "1"})),
            ),
            ("spawn_coding_agent".to_owned(), Err("failed".into())),
        ];
        let formatted = format_tool_results(&results);
        assert!(formatted.contains("create_project: success"));
        assert!(formatted.contains("spawn_coding_agent: error"));
    }

    #[test]
    fn format_tool_results_empty() {
        let results: Vec<(String, Result<serde_json::Value, String>)> = Vec::new();
        let formatted = format_tool_results(&results);
        assert!(formatted.contains("Tool results:"));
    }

    #[test]
    fn parse_structured_output_valid() {
        let result = ResultMessage {
            subtype: "success".into(),
            session_id: "s1".into(),
            is_error: false,
            result: None,
            total_cost_usd: None,
            duration_ms: None,
            num_turns: None,
            usage: None,
            structured_output: Some(serde_json::json!({
                "text": "Hello",
                "tools": [{"name": "create_project", "parameters": {"name": "test"}}]
            })),
        };
        let resp = parse_structured_output(&result);
        assert_eq!(resp.text, "Hello");
        assert_eq!(resp.tools.len(), 1);
    }

    #[test]
    fn parse_structured_output_fallback_to_result() {
        let result = ResultMessage {
            subtype: "success".into(),
            session_id: "s1".into(),
            is_error: false,
            result: Some("Plain text response".into()),
            total_cost_usd: None,
            duration_ms: None,
            num_turns: None,
            usage: None,
            structured_output: None,
        };
        let resp = parse_structured_output(&result);
        assert_eq!(resp.text, "Plain text response");
        assert!(resp.tools.is_empty());
    }

    #[test]
    fn parse_structured_output_malformed_falls_back() {
        let result = ResultMessage {
            subtype: "success".into(),
            session_id: "s1".into(),
            is_error: false,
            result: Some("Fallback text".into()),
            total_cost_usd: None,
            duration_ms: None,
            num_turns: None,
            usage: None,
            structured_output: Some(serde_json::json!({"not_valid": true})),
        };
        let resp = parse_structured_output(&result);
        assert_eq!(resp.text, "Fallback text");
        assert!(resp.tools.is_empty());
    }
}
