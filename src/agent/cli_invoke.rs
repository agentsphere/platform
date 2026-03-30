#![allow(dead_code)]
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

/// Timeout for the CLI to emit its first NDJSON message (system init).
/// Detects startup hangs (auth, config) in 30s instead of 300s.
const CLI_FIRST_MESSAGE_TIMEOUT: Duration = Duration::from_secs(30);

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
    /// Extra environment variables from custom LLM provider config.
    pub extra_env: Vec<(String, String)>,
    /// Model override from custom LLM provider config.
    pub model_override: Option<String>,
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
        cwd: Some(std::path::PathBuf::from("/tmp")),
        extra_env: params.extra_env,
        model: params.model_override,
        ..Default::default()
    };

    let mut transport = SubprocessTransport::spawn(opts)
        .map_err(|e| AgentError::Other(anyhow::anyhow!("CLI spawn failed: {e}")))?;

    // Close stdin — in -p mode the prompt is an argument, not piped input.
    // Without closing, the CLI may block on stdin reads during startup.
    transport.close_stdin().await;

    let result = read_cli_output(&mut transport, params.session_id, valkey).await;

    // Collect stderr before killing (helps diagnose failures)
    let stderr = collect_stderr(&mut transport).await;
    let _ = transport.kill().await;

    if let Err(ref e) = result
        && !stderr.is_empty()
    {
        tracing::error!(
            session_id = %params.session_id,
            stderr = %stderr,
            "CLI subprocess stderr on failure: {e}"
        );
    }

    let (result_msg, cli_session_id, assistant_structured) = result?;

    // Parse structured output from result message (with assistant fallback)
    let structured = parse_structured_output(&result_msg, assistant_structured.as_ref());

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
///
/// Uses a two-phase timeout: 30s for the first message (startup hang detection),
/// then 300s for subsequent messages (normal operation).
pub(crate) async fn read_cli_output(
    transport: &mut SubprocessTransport,
    session_id: Uuid,
    valkey: &fred::clients::Pool,
) -> Result<(ResultMessage, Option<String>, Option<serde_json::Value>), AgentError> {
    let mut result_msg: Option<ResultMessage> = None;
    let mut cli_session_id: Option<String> = None;
    let mut assistant_structured: Option<serde_json::Value> = None;
    let mut first_message = true;

    loop {
        let timeout_dur = if first_message {
            CLI_FIRST_MESSAGE_TIMEOUT
        } else {
            CLI_INVOKE_TIMEOUT
        };

        let msg = if let Ok(result) = tokio::time::timeout(timeout_dur, transport.recv()).await {
            result.map_err(|e| AgentError::Other(anyhow::anyhow!("CLI read error: {e}")))?
        } else if first_message {
            return Err(AgentError::Other(anyhow::anyhow!(
                "CLI startup timed out — no output within {}s (check stderr logs)",
                CLI_FIRST_MESSAGE_TIMEOUT.as_secs()
            )));
        } else {
            return Err(AgentError::Other(anyhow::anyhow!(
                "CLI invocation timed out ({}s)",
                CLI_INVOKE_TIMEOUT.as_secs()
            )));
        };

        let Some(msg) = msg else {
            // Process exited (stdout EOF)
            break;
        };

        first_message = false;

        // Track CLI session ID from system init
        if let CliMessage::System(ref sys) = msg {
            cli_session_id = Some(sys.session_id.clone());
        }

        // Capture structured output from assistant tool_use blocks.
        // Two cases to handle:
        //  1. StructuredOutput tool: input is a valid StructuredResponse
        //     (normal --json-schema flow)
        //  2. Direct tool call: the LLM calls one of our tools (create_project,
        //     spawn_coding_agent, etc.) directly instead of via StructuredOutput.
        //     Wrap it into a StructuredResponse so the create-app loop can
        //     execute it.
        if let CliMessage::Assistant(ref a) = msg {
            let mut direct_tool_calls: Vec<serde_json::Value> = Vec::new();

            for block in &a.message.content {
                if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                    continue;
                }
                if let Some(input) = block.get("input") {
                    // Case 1: StructuredOutput tool (input parses as StructuredResponse)
                    if serde_json::from_value::<StructuredResponse>(input.clone()).is_ok() {
                        assistant_structured = Some(input.clone());
                    }
                    // Case 2: Direct tool call (one of our known tool names)
                    else if let Some(name) = block.get("name").and_then(|v| v.as_str())
                        && matches!(
                            name,
                            "create_project"
                                | "spawn_coding_agent"
                                | "send_message_to_session"
                                | "check_session_progress"
                        )
                    {
                        direct_tool_calls.push(serde_json::json!({
                            "name": name,
                            "parameters": input
                        }));
                    }
                }
            }

            // If no StructuredOutput was captured but we have direct tool calls,
            // wrap them into a StructuredResponse.
            if assistant_structured.is_none() && !direct_tool_calls.is_empty() {
                assistant_structured = Some(serde_json::json!({
                    "text": "",
                    "tools": direct_tool_calls
                }));
            }
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

    let result = result_msg.ok_or_else(|| {
        AgentError::Other(anyhow::anyhow!(
            "CLI process exited without a result message (check stderr logs)"
        ))
    })?;

    Ok((result, cli_session_id, assistant_structured))
}

/// Collect stderr output from the transport's background task.
async fn collect_stderr(transport: &mut SubprocessTransport) -> String {
    if let Some(task) = transport.stderr_task.take() {
        match tokio::time::timeout(Duration::from_secs(2), task).await {
            Ok(Ok(stderr)) => stderr,
            _ => String::new(),
        }
    } else {
        String::new()
    }
}

/// Parse the structured output from a `ResultMessage`.
///
/// Falls back to using `result` as text with empty tools if `structured_output`
/// is absent or malformed.
fn parse_structured_output(
    result: &ResultMessage,
    assistant_fallback: Option<&serde_json::Value>,
) -> StructuredResponse {
    if let Some(ref structured) = result.structured_output {
        if let Ok(response) = serde_json::from_value::<StructuredResponse>(structured.clone()) {
            return response;
        }
        tracing::warn!("failed to parse structured_output, falling back to result text");
    }

    // Fallback: recover from assistant tool_use block (error_max_turns case)
    if let Some(fallback) = assistant_fallback
        && let Ok(response) = serde_json::from_value::<StructuredResponse>(fallback.clone())
    {
        tracing::info!(
            subtype = %result.subtype,
            tools_count = response.tools.len(),
            "recovered structured output from assistant tool_use block"
        );
        return response;
    }

    // Final fallback: use result text with no tools
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
///
/// Uses a structured, readable format with labeled key-value pairs and
/// next-step hints to guide the LLM on what to do with return values.
pub fn format_tool_results(results: &[(String, Result<serde_json::Value, String>)]) -> String {
    let mut parts = Vec::new();
    for (name, result) in results {
        match result {
            Ok(value) => {
                let mut lines = vec![format!("[{name}] OK")];
                // Format return values as labeled key-value pairs
                if let Some(obj) = value.as_object() {
                    for (k, v) in obj {
                        if k.starts_with('_') {
                            continue; // skip hint fields in the data section
                        }
                        match v {
                            serde_json::Value::String(s) => {
                                lines.push(format!("  {k}: {s}"));
                            }
                            _ => {
                                lines.push(format!("  {k}: {v}"));
                            }
                        }
                    }
                    // Show hint fields last, clearly labeled
                    if let Some(hint) = obj.get("_next") {
                        lines.push(format!("  → {}", hint.as_str().unwrap_or("")));
                    }
                } else {
                    lines.push(format!("  result: {value}"));
                }
                parts.push(lines.join("\n"));
            }
            Err(err) => {
                parts.push(format!("[{name}] ERROR: {err}"));
            }
        }
    }
    format!("TOOL RESULTS:\n{}", parts.join("\n\n"))
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
                        "name": { "type": "string", "enum": ["create_project", "spawn_coding_agent", "send_message_to_session", "check_session_progress"] },
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
        assert!(names.contains(&"send_message_to_session"));
        assert!(names.contains(&"check_session_progress"));
        assert_eq!(names.len(), 4);
    }

    #[test]
    fn format_tool_results_success() {
        let results = vec![(
            "create_project".to_owned(),
            Ok(serde_json::json!({"project_id": "abc"})),
        )];
        let formatted = format_tool_results(&results);
        assert!(formatted.contains("[create_project] OK"));
        assert!(formatted.contains("project_id: abc"));
    }

    #[test]
    fn format_tool_results_error() {
        let results = vec![("create_project".to_owned(), Err("name taken".into()))];
        let formatted = format_tool_results(&results);
        assert!(formatted.contains("[create_project] ERROR: name taken"));
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
        assert!(formatted.contains("[create_project] OK"));
        assert!(formatted.contains("[spawn_coding_agent] ERROR: failed"));
    }

    #[test]
    fn format_tool_results_empty() {
        let results: Vec<(String, Result<serde_json::Value, String>)> = Vec::new();
        let formatted = format_tool_results(&results);
        assert!(formatted.contains("TOOL RESULTS:"));
    }

    #[test]
    fn format_tool_results_with_hint() {
        let results = vec![(
            "create_project".to_owned(),
            Ok(serde_json::json!({"project_id": "abc", "_next": "Now call spawn_coding_agent"})),
        )];
        let formatted = format_tool_results(&results);
        assert!(formatted.contains("[create_project] OK"));
        assert!(formatted.contains("project_id: abc"));
        assert!(formatted.contains("→ Now call spawn_coding_agent"));
        // _next should NOT appear as a regular key-value pair
        assert!(!formatted.contains("_next:"));
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
        let resp = parse_structured_output(&result, None);
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
        let resp = parse_structured_output(&result, None);
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
        let resp = parse_structured_output(&result, None);
        assert_eq!(resp.text, "Fallback text");
        assert!(resp.tools.is_empty());
    }

    #[test]
    fn parse_structured_output_from_assistant_fallback() {
        // error_max_turns: no structured_output in result, but assistant had tool_use
        let result = ResultMessage {
            subtype: "error_max_turns".into(),
            session_id: "s1".into(),
            is_error: true,
            result: Some("Max turns reached".into()),
            total_cost_usd: None,
            duration_ms: None,
            num_turns: Some(1),
            usage: None,
            structured_output: None,
        };
        let fallback = serde_json::json!({
            "text": "I'll create the project.",
            "tools": [{"name": "create_project", "parameters": {"name": "my-app"}}]
        });
        let resp = parse_structured_output(&result, Some(&fallback));
        assert_eq!(resp.text, "I'll create the project.");
        assert_eq!(resp.tools.len(), 1);
        assert_eq!(resp.tools[0].name, "create_project");
    }

    #[test]
    fn parse_structured_output_prefers_result_over_fallback() {
        // Both result.structured_output and assistant fallback present — result wins
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
                "text": "From result",
                "tools": [{"name": "create_project", "parameters": {"name": "winner"}}]
            })),
        };
        let fallback = serde_json::json!({
            "text": "From assistant",
            "tools": [{"name": "spawn_coding_agent", "parameters": {"prompt": "loser"}}]
        });
        let resp = parse_structured_output(&result, Some(&fallback));
        assert_eq!(resp.text, "From result");
        assert_eq!(resp.tools[0].name, "create_project");
    }

    #[test]
    fn parse_structured_output_ignores_invalid_fallback() {
        // Invalid fallback JSON (not a StructuredResponse) → text fallback
        let result = ResultMessage {
            subtype: "error_max_turns".into(),
            session_id: "s1".into(),
            is_error: true,
            result: Some("Max turns reached".into()),
            total_cost_usd: None,
            duration_ms: None,
            num_turns: Some(1),
            usage: None,
            structured_output: None,
        };
        let fallback = serde_json::json!({"invalid": "not a structured response"});
        let resp = parse_structured_output(&result, Some(&fallback));
        assert_eq!(resp.text, "Max turns reached");
        assert!(resp.tools.is_empty());
    }

    #[test]
    fn parse_structured_output_no_fallback_no_structured_output() {
        // Neither structured_output nor fallback → text fallback
        let result = ResultMessage {
            subtype: "error_max_turns".into(),
            session_id: "s1".into(),
            is_error: true,
            result: Some("Max turns reached".into()),
            total_cost_usd: None,
            duration_ms: None,
            num_turns: Some(1),
            usage: None,
            structured_output: None,
        };
        let resp = parse_structured_output(&result, None);
        assert_eq!(resp.text, "Max turns reached");
        assert!(resp.tools.is_empty());
    }

    #[test]
    fn parse_structured_output_direct_tool_call_wrapped() {
        // When the LLM calls our tool directly (not via StructuredOutput),
        // the fallback is wrapped: {text: "", tools: [{name, parameters}]}
        let result = ResultMessage {
            subtype: "success".into(),
            session_id: "s1".into(),
            is_error: false,
            result: None,
            total_cost_usd: None,
            duration_ms: None,
            num_turns: None,
            usage: None,
            structured_output: None,
        };
        let fallback = serde_json::json!({
            "text": "",
            "tools": [{"name": "create_project", "parameters": {"name": "hello-llm"}}]
        });
        let resp = parse_structured_output(&result, Some(&fallback));
        assert_eq!(resp.tools.len(), 1);
        assert_eq!(resp.tools[0].name, "create_project");
        assert_eq!(resp.tools[0].parameters["name"], "hello-llm");
    }

    #[test]
    fn first_message_timeout_shorter_than_invoke_timeout() {
        assert!(
            CLI_FIRST_MESSAGE_TIMEOUT < CLI_INVOKE_TIMEOUT,
            "startup timeout must be shorter than full invocation timeout"
        );
    }

    #[test]
    fn first_message_timeout_is_30s() {
        assert_eq!(CLI_FIRST_MESSAGE_TIMEOUT.as_secs(), 30);
    }

    #[test]
    fn cli_invoke_timeout_is_300s() {
        assert_eq!(CLI_INVOKE_TIMEOUT.as_secs(), 300);
    }

    #[test]
    fn format_tool_results_non_object_value() {
        // When tool result is a plain string/number (not an object), should use "result: " format
        let results = vec![(
            "create_project".to_owned(),
            Ok(serde_json::json!("simple string result")),
        )];
        let formatted = format_tool_results(&results);
        assert!(formatted.contains("[create_project] OK"));
        assert!(formatted.contains("result: \"simple string result\""));
    }

    #[test]
    fn format_tool_results_array_value() {
        let results = vec![("list_items".to_owned(), Ok(serde_json::json!([1, 2, 3])))];
        let formatted = format_tool_results(&results);
        assert!(formatted.contains("[list_items] OK"));
        assert!(formatted.contains("result: [1,2,3]"));
    }

    #[test]
    fn format_tool_results_boolean_value() {
        let results = vec![("check".to_owned(), Ok(serde_json::json!(true)))];
        let formatted = format_tool_results(&results);
        assert!(formatted.contains("[check] OK"));
        assert!(formatted.contains("result: true"));
    }

    #[test]
    fn format_tool_results_null_value() {
        let results = vec![("noop".to_owned(), Ok(serde_json::json!(null)))];
        let formatted = format_tool_results(&results);
        assert!(formatted.contains("[noop] OK"));
        assert!(formatted.contains("result: null"));
    }

    #[test]
    fn format_tool_results_with_underscore_prefix_key_skipped() {
        // Keys starting with _ (other than _next) should be skipped in the data section
        let results = vec![(
            "create_project".to_owned(),
            Ok(
                serde_json::json!({"project_id": "abc", "_internal": "skip this", "_next": "Do next thing"}),
            ),
        )];
        let formatted = format_tool_results(&results);
        assert!(formatted.contains("project_id: abc"));
        assert!(!formatted.contains("_internal"));
        assert!(formatted.contains("→ Do next thing"));
    }

    #[test]
    fn format_tool_results_hint_non_string() {
        // _next that is not a string should produce empty hint
        let results = vec![(
            "tool1".to_owned(),
            Ok(serde_json::json!({"id": "123", "_next": 42})),
        )];
        let formatted = format_tool_results(&results);
        assert!(formatted.contains("id: 123"));
        assert!(formatted.contains("→ "));
    }

    #[test]
    fn format_tool_results_multiple_success_entries() {
        let results = vec![
            ("tool_a".to_owned(), Ok(serde_json::json!({"a": 1}))),
            ("tool_b".to_owned(), Ok(serde_json::json!({"b": 2}))),
            ("tool_c".to_owned(), Ok(serde_json::json!({"c": 3}))),
        ];
        let formatted = format_tool_results(&results);
        assert!(formatted.contains("[tool_a] OK"));
        assert!(formatted.contains("[tool_b] OK"));
        assert!(formatted.contains("[tool_c] OK"));
        assert!(formatted.contains("a: 1"));
        assert!(formatted.contains("b: 2"));
        assert!(formatted.contains("c: 3"));
    }

    #[test]
    fn format_tool_results_object_with_nested_json() {
        let results = vec![(
            "complex".to_owned(),
            Ok(serde_json::json!({"nested": {"key": "value"}})),
        )];
        let formatted = format_tool_results(&results);
        assert!(formatted.contains("[complex] OK"));
        assert!(formatted.contains("nested: {\"key\":\"value\"}"));
    }

    #[test]
    fn parse_structured_output_no_result_text() {
        // Neither result text nor structured_output — should use default message
        let result = ResultMessage {
            subtype: "error_max_turns".into(),
            session_id: "s1".into(),
            is_error: true,
            result: None,
            total_cost_usd: None,
            duration_ms: None,
            num_turns: None,
            usage: None,
            structured_output: None,
        };
        let resp = parse_structured_output(&result, None);
        assert_eq!(resp.text, "No response from agent");
        assert!(resp.tools.is_empty());
    }

    #[test]
    fn parse_structured_output_malformed_structured_with_valid_fallback() {
        // structured_output is malformed but assistant fallback is valid
        let result = ResultMessage {
            subtype: "success".into(),
            session_id: "s1".into(),
            is_error: false,
            result: Some("Text fallback".into()),
            total_cost_usd: None,
            duration_ms: None,
            num_turns: None,
            usage: None,
            structured_output: Some(serde_json::json!("not an object")),
        };
        let fallback = serde_json::json!({
            "text": "From fallback",
            "tools": [{"name": "spawn_coding_agent", "parameters": {"prompt": "do it"}}]
        });
        let resp = parse_structured_output(&result, Some(&fallback));
        assert_eq!(resp.text, "From fallback");
        assert_eq!(resp.tools.len(), 1);
        assert_eq!(resp.tools[0].name, "spawn_coding_agent");
    }

    #[test]
    fn structured_response_serialize_roundtrip() {
        let resp = StructuredResponse {
            text: "Hello world".into(),
            tools: vec![
                ToolRequest {
                    name: "create_project".into(),
                    parameters: serde_json::json!({"name": "test"}),
                },
                ToolRequest {
                    name: "spawn_coding_agent".into(),
                    parameters: serde_json::json!({"prompt": "build it"}),
                },
            ],
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: StructuredResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.text, "Hello world");
        assert_eq!(back.tools.len(), 2);
    }

    #[test]
    fn create_app_schema_has_tool_descriptions() {
        let schema = create_app_schema();
        let tools_desc = schema["properties"]["tools"]["description"].as_str();
        assert!(tools_desc.is_some(), "tools should have a description");
        let text_desc = schema["properties"]["text"]["description"].as_str();
        assert!(text_desc.is_some(), "text should have a description");
    }

    #[test]
    fn format_tool_results_error_preserves_message() {
        let results = vec![(
            "spawn_coding_agent".to_owned(),
            Err("pod creation timed out after 60s".into()),
        )];
        let formatted = format_tool_results(&results);
        assert!(formatted.contains("[spawn_coding_agent] ERROR: pod creation timed out after 60s"));
    }

    #[test]
    fn format_tool_results_header_always_present() {
        let results: Vec<(String, Result<serde_json::Value, String>)> = Vec::new();
        let formatted = format_tool_results(&results);
        assert!(formatted.starts_with("TOOL RESULTS:\n"));
    }

    #[test]
    fn format_tool_results_string_field_in_object() {
        // String values in objects should not have extra quotes
        let results = vec![(
            "tool1".to_owned(),
            Ok(serde_json::json!({"name": "my-project", "id": "uuid-here"})),
        )];
        let formatted = format_tool_results(&results);
        assert!(formatted.contains("name: my-project"));
        assert!(formatted.contains("id: uuid-here"));
        // Should NOT contain extra quotes around string values
        assert!(!formatted.contains("name: \"my-project\""));
    }

    #[test]
    fn format_tool_results_with_string_value() {
        let results = vec![("my_tool".to_owned(), Ok(serde_json::json!("just a string")))];
        let formatted = format_tool_results(&results);
        assert!(formatted.contains("[my_tool] OK"));
        assert!(formatted.contains("result: \"just a string\""));
    }

    #[test]
    fn format_tool_results_with_number_value() {
        let results = vec![("count_tool".to_owned(), Ok(serde_json::json!(42)))];
        let formatted = format_tool_results(&results);
        assert!(formatted.contains("[count_tool] OK"));
        assert!(formatted.contains("result: 42"));
    }

    #[test]
    fn format_tool_results_with_array_value() {
        let results = vec![(
            "list_tool".to_owned(),
            Ok(serde_json::json!(["a", "b", "c"])),
        )];
        let formatted = format_tool_results(&results);
        assert!(formatted.contains("[list_tool] OK"));
        assert!(formatted.contains("result:"));
    }

    #[test]
    fn format_tool_results_with_null_value() {
        let results = vec![("null_tool".to_owned(), Ok(serde_json::json!(null)))];
        let formatted = format_tool_results(&results);
        assert!(formatted.contains("[null_tool] OK"));
        assert!(formatted.contains("result: null"));
    }

    #[test]
    fn format_tool_results_hint_field_only_in_arrow() {
        // _next should appear as → hint, not as a key-value pair
        let results = vec![(
            "tool_a".to_owned(),
            Ok(serde_json::json!({"name": "x", "_next": "Do Y next"})),
        )];
        let formatted = format_tool_results(&results);
        assert!(formatted.contains("→ Do Y next"));
        assert!(!formatted.contains("_next:"));
    }

    #[test]
    fn format_tool_results_underscore_fields_skipped() {
        // Fields starting with _ (other than _next) should be skipped
        let results = vec![(
            "tool_b".to_owned(),
            Ok(serde_json::json!({"visible": "yes", "_internal": "hidden"})),
        )];
        let formatted = format_tool_results(&results);
        assert!(formatted.contains("visible: yes"));
        assert!(!formatted.contains("_internal"));
    }

    #[test]
    fn format_tool_results_numeric_value_in_object() {
        let results = vec![(
            "tool_c".to_owned(),
            Ok(serde_json::json!({"count": 42, "active": true})),
        )];
        let formatted = format_tool_results(&results);
        assert!(formatted.contains("count: 42"));
        assert!(formatted.contains("active: true"));
    }

    #[test]
    fn parse_structured_output_no_result_no_fallback_defaults() {
        let result = ResultMessage {
            subtype: "success".into(),
            session_id: "s1".into(),
            is_error: false,
            result: None,
            total_cost_usd: None,
            duration_ms: None,
            num_turns: None,
            usage: None,
            structured_output: None,
        };
        let resp = parse_structured_output(&result, None);
        assert_eq!(resp.text, "No response from agent");
        assert!(resp.tools.is_empty());
    }

    #[test]
    fn structured_response_missing_text_fails() {
        let json = r#"{"tools":[]}"#;
        let result = serde_json::from_str::<StructuredResponse>(json);
        assert!(result.is_err());
    }

    #[test]
    fn structured_response_missing_tools_fails() {
        let json = r#"{"text":"hello"}"#;
        let result = serde_json::from_str::<StructuredResponse>(json);
        assert!(result.is_err());
    }

    #[test]
    fn tool_request_debug() {
        let req = ToolRequest {
            name: "test".into(),
            parameters: serde_json::json!({}),
        };
        let debug = format!("{req:?}");
        assert!(debug.contains("test"));
    }

    #[test]
    fn structured_response_clone() {
        let resp = StructuredResponse {
            text: "hello".into(),
            tools: vec![ToolRequest {
                name: "create_project".into(),
                parameters: serde_json::json!({"name": "test"}),
            }],
        };
        let cloned = resp.clone();
        assert_eq!(cloned.text, "hello");
        assert_eq!(cloned.tools.len(), 1);
        assert_eq!(cloned.tools[0].name, "create_project");
    }

    #[test]
    fn create_app_schema_required_fields_on_tool_items() {
        let schema = create_app_schema();
        let required = &schema["properties"]["tools"]["items"]["required"];
        let arr = required.as_array().unwrap();
        assert!(arr.contains(&serde_json::json!("name")));
        assert!(arr.contains(&serde_json::json!("parameters")));
    }
}
