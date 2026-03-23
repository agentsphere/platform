use std::collections::HashMap;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::claude_cli::transport::{CliSpawnOptions, SubprocessTransport};
use super::cli_invoke::StructuredResponse;
use super::error::AgentError;

/// Timeout for validation CLI invocations (30s — fast fail on auth/network).
const VALIDATION_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A single validation test result.
#[derive(Debug, Clone, Serialize)]
pub struct TestResult {
    pub test: u8,
    pub name: &'static str,
    pub status: TestStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TestStatus {
    Running,
    Passed,
    Failed,
}

/// Events sent over the SSE channel during validation.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ValidationEvent {
    Test(TestResult),
    Done { all_passed: bool },
}

// ---------------------------------------------------------------------------
// Provider env construction
// ---------------------------------------------------------------------------

/// Build the `(anthropic_api_key, extra_env)` tuple for a custom provider.
///
/// For `custom_endpoint`, extracts `ANTHROPIC_API_KEY` from `env_vars` into the
/// dedicated auth path (first return value). Auto-injects provider-specific vars.
pub fn build_provider_extra_env<S: std::hash::BuildHasher>(
    provider_type: &str,
    env_vars: &HashMap<String, String, S>,
) -> (Option<String>, Vec<(String, String)>) {
    let mut extra: Vec<(String, String)> = Vec::new();
    let mut api_key: Option<String> = None;

    for (key, value) in env_vars {
        // ANTHROPIC_API_KEY goes through the dedicated auth path, not extra_env
        if key == "ANTHROPIC_API_KEY" {
            api_key = Some(value.clone());
            continue;
        }
        extra.push((key.clone(), value.clone()));
    }

    // Auto-inject provider-specific env vars
    match provider_type {
        "bedrock" => {
            push_if_missing(&mut extra, "DISABLE_PROMPT_CACHING", "1");
        }
        "vertex" => {
            push_if_missing(&mut extra, "CLAUDE_CODE_USE_VERTEX", "1");
            push_if_missing(&mut extra, "CLOUD_ML_REGION", "global");
        }
        // azure_foundry and custom_endpoint: no auto-inject needed
        _ => {}
    }

    (api_key, extra)
}

fn push_if_missing(env: &mut Vec<(String, String)>, key: &str, value: &str) {
    if !env.iter().any(|(k, _)| k == key) {
        env.push((key.to_owned(), value.to_owned()));
    }
}

// ---------------------------------------------------------------------------
// Test implementations
// ---------------------------------------------------------------------------

/// Test 1: Basic connectivity — can we reach the endpoint and get a response?
async fn test_connection(
    api_key: Option<&str>,
    extra_env: &[(String, String)],
    model: Option<&str>,
    cancel: &CancellationToken,
) -> TestResult {
    let opts = CliSpawnOptions {
        prompt: Some("Reply with the single word hello".into()),
        disable_tools: true,
        max_turns: Some(1),
        anthropic_api_key: api_key.map(String::from),
        extra_env: extra_env.to_vec(),
        model: model.map(String::from),
        permission_mode: Some("bypassPermissions".into()),
        cwd: Some(std::path::PathBuf::from("/tmp")),
        ..Default::default()
    };

    match run_cli_with_cancel(opts, cancel).await {
        Ok(Some(text)) if !text.is_empty() => TestResult {
            test: 1,
            name: "connection",
            status: TestStatus::Passed,
            detail: "Endpoint reachable, got valid response".into(),
        },
        Ok(_) => TestResult {
            test: 1,
            name: "connection",
            status: TestStatus::Failed,
            detail: "Endpoint returned empty response".into(),
        },
        Err(e) => TestResult {
            test: 1,
            name: "connection",
            status: TestStatus::Failed,
            detail: format!("Connection failed: {e}"),
        },
    }
}

/// Test 2: Structured output + tool use format.
async fn test_output_format(
    api_key: Option<&str>,
    extra_env: &[(String, String)],
    model: Option<&str>,
    cancel: &CancellationToken,
) -> TestResult {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "text": { "type": "string" },
            "tools": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "parameters": { "type": "object" }
                    },
                    "required": ["name", "parameters"]
                }
            }
        },
        "required": ["text", "tools"]
    });

    let opts = CliSpawnOptions {
        prompt: Some(
            "You have tools: create_project and spawn_agent. Call create_project with name 'test'. \
             Return the tool call in the tools array."
                .into(),
        ),
        disable_tools: true,
        max_turns: Some(1),
        json_schema: Some(serde_json::to_string(&schema).unwrap_or_default()),
        anthropic_api_key: api_key.map(String::from),
        extra_env: extra_env.to_vec(),
        model: model.map(String::from),
        permission_mode: Some("bypassPermissions".into()),
        cwd: Some(std::path::PathBuf::from("/tmp")),
        ..Default::default()
    };

    match run_cli_structured_with_cancel(opts, cancel).await {
        Ok(resp) => {
            if resp.tools.is_empty() {
                TestResult {
                    test: 2,
                    name: "output_format",
                    status: TestStatus::Failed,
                    detail: "Structured output parsed but tools array is empty".into(),
                }
            } else {
                let has_name = resp.tools.iter().all(|t| !t.name.is_empty());
                if has_name {
                    TestResult {
                        test: 2,
                        name: "output_format",
                        status: TestStatus::Passed,
                        detail: "Valid structured JSON with tool calls".into(),
                    }
                } else {
                    TestResult {
                        test: 2,
                        name: "output_format",
                        status: TestStatus::Failed,
                        detail: "Tool calls missing name field".into(),
                    }
                }
            }
        }
        Err(e) => TestResult {
            test: 2,
            name: "output_format",
            status: TestStatus::Failed,
            detail: format!("Structured output test failed: {e}"),
        },
    }
}

/// Test 3: Multi-turn session memory (--resume preserves context).
async fn test_session_memory(
    api_key: Option<&str>,
    extra_env: &[(String, String)],
    model: Option<&str>,
    cancel: &CancellationToken,
) -> TestResult {
    let integer_schema = serde_json::json!({
        "type": "object",
        "properties": {
            "answer": { "type": "integer" }
        },
        "required": ["answer"]
    });
    let schema_str = serde_json::to_string(&integer_schema).unwrap_or_default();
    let session_id = Uuid::new_v4().to_string();

    // Turn 1: 10 + 10
    let opts1 = CliSpawnOptions {
        prompt: Some("what is 10+10? put the answer in the answer field".into()),
        disable_tools: true,
        max_turns: Some(1),
        json_schema: Some(schema_str.clone()),
        initial_session_id: Some(session_id.clone()),
        anthropic_api_key: api_key.map(String::from),
        extra_env: extra_env.to_vec(),
        model: model.map(String::from),
        permission_mode: Some("bypassPermissions".into()),
        cwd: Some(std::path::PathBuf::from("/tmp")),
        ..Default::default()
    };

    let turn1 = match run_cli_json_with_cancel(opts1, cancel).await {
        Ok(json) => json,
        Err(e) => {
            return TestResult {
                test: 3,
                name: "session_memory",
                status: TestStatus::Failed,
                detail: format!("Turn 1 failed: {e}"),
            };
        }
    };

    let answer1 = turn1.get("answer").and_then(serde_json::Value::as_i64);
    if answer1 != Some(20) {
        return TestResult {
            test: 3,
            name: "session_memory",
            status: TestStatus::Failed,
            detail: format!("Turn 1: expected answer=20, got {answer1:?}"),
        };
    }

    // Turn 2: subtract 7 from previous (should be 13)
    let opts2 = CliSpawnOptions {
        prompt: Some(
            "subtract 7 from the previous result, put the answer in the answer field".into(),
        ),
        disable_tools: true,
        max_turns: Some(1),
        json_schema: Some(schema_str),
        resume_session: Some(session_id),
        anthropic_api_key: api_key.map(String::from),
        extra_env: extra_env.to_vec(),
        model: model.map(String::from),
        permission_mode: Some("bypassPermissions".into()),
        cwd: Some(std::path::PathBuf::from("/tmp")),
        ..Default::default()
    };

    let turn2 = match run_cli_json_with_cancel(opts2, cancel).await {
        Ok(json) => json,
        Err(e) => {
            return TestResult {
                test: 3,
                name: "session_memory",
                status: TestStatus::Failed,
                detail: format!("Turn 2 failed: {e}"),
            };
        }
    };

    let answer2 = turn2.get("answer").and_then(serde_json::Value::as_i64);
    if answer2 == Some(13) {
        TestResult {
            test: 3,
            name: "session_memory",
            status: TestStatus::Passed,
            detail: "Multi-turn memory preserved (got 13)".into(),
        }
    } else {
        TestResult {
            test: 3,
            name: "session_memory",
            status: TestStatus::Failed,
            detail: format!("Turn 2: expected answer=13, got {answer2:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Test runner
// ---------------------------------------------------------------------------

/// Run all 3 validation tests sequentially, sending events via `tx`.
/// Updates the config's `validation_status` in DB after completion.
pub async fn run_validation(
    pool: &sqlx::PgPool,
    config_id: Uuid,
    api_key: Option<String>,
    extra_env: Vec<(String, String)>,
    model: Option<String>,
    tx: mpsc::Sender<ValidationEvent>,
    cancel: CancellationToken,
) {
    let test_defs: &[(u8, &str)] = &[
        (1, "connection"),
        (2, "output_format"),
        (3, "session_memory"),
    ];

    let mut all_passed = true;

    for &(num, name) in test_defs {
        if cancel.is_cancelled() {
            break;
        }

        // Send "running" event
        let running = ValidationEvent::Test(TestResult {
            test: num,
            name,
            status: TestStatus::Running,
            detail: String::new(),
        });
        if tx.send(running).await.is_err() {
            return; // Client disconnected
        }

        // Run the test
        let result = match num {
            1 => test_connection(api_key.as_deref(), &extra_env, model.as_deref(), &cancel).await,
            2 => {
                test_output_format(api_key.as_deref(), &extra_env, model.as_deref(), &cancel).await
            }
            3 => {
                test_session_memory(api_key.as_deref(), &extra_env, model.as_deref(), &cancel).await
            }
            _ => unreachable!(),
        };

        if !matches!(result.status, TestStatus::Passed) {
            all_passed = false;
        }

        if tx.send(ValidationEvent::Test(result)).await.is_err() {
            return; // Client disconnected
        }
    }

    // Send done event
    let _ = tx.send(ValidationEvent::Done { all_passed }).await;

    // Update DB validation status
    let status = if all_passed { "valid" } else { "invalid" };
    if let Err(e) =
        crate::secrets::llm_providers::update_validation_status(pool, config_id, status).await
    {
        tracing::warn!(error = %e, %config_id, "failed to update validation status");
    }
}

// ---------------------------------------------------------------------------
// CLI helpers (with cancellation)
// ---------------------------------------------------------------------------

/// Run a CLI invocation and return the raw text result. Respects cancellation.
async fn run_cli_with_cancel(
    opts: CliSpawnOptions,
    cancel: &CancellationToken,
) -> Result<Option<String>, AgentError> {
    let mut transport = SubprocessTransport::spawn(opts)
        .map_err(|e| AgentError::Other(anyhow::anyhow!("CLI spawn failed: {e}")))?;

    transport.close_stdin().await;

    let result = tokio::select! {
        r = read_result_text(&mut transport) => r,
        () = cancel.cancelled() => {
            let _ = transport.kill().await;
            return Err(AgentError::Other(anyhow::anyhow!("validation cancelled")));
        }
    };

    let _ = transport.kill().await;
    result
}

/// Run CLI with `--json-schema` and return parsed `StructuredResponse`.
async fn run_cli_structured_with_cancel(
    opts: CliSpawnOptions,
    cancel: &CancellationToken,
) -> Result<StructuredResponse, AgentError> {
    let mut transport = SubprocessTransport::spawn(opts)
        .map_err(|e| AgentError::Other(anyhow::anyhow!("CLI spawn failed: {e}")))?;

    transport.close_stdin().await;

    let result = tokio::select! {
        r = read_structured_result(&mut transport) => r,
        () = cancel.cancelled() => {
            let _ = transport.kill().await;
            return Err(AgentError::Other(anyhow::anyhow!("validation cancelled")));
        }
    };

    let _ = transport.kill().await;
    result
}

/// Run CLI with `--json-schema` and return raw JSON value.
async fn run_cli_json_with_cancel(
    opts: CliSpawnOptions,
    cancel: &CancellationToken,
) -> Result<serde_json::Value, AgentError> {
    let mut transport = SubprocessTransport::spawn(opts)
        .map_err(|e| AgentError::Other(anyhow::anyhow!("CLI spawn failed: {e}")))?;

    transport.close_stdin().await;

    let result = tokio::select! {
        r = read_json_result(&mut transport) => r,
        () = cancel.cancelled() => {
            let _ = transport.kill().await;
            return Err(AgentError::Other(anyhow::anyhow!("validation cancelled")));
        }
    };

    let _ = transport.kill().await;
    result
}

/// Read NDJSON from transport until `ResultMessage`, return the result text.
async fn read_result_text(
    transport: &mut SubprocessTransport,
) -> Result<Option<String>, AgentError> {
    use super::claude_cli::CliMessage;

    loop {
        let msg = tokio::time::timeout(VALIDATION_TIMEOUT, transport.recv())
            .await
            .map_err(|_| {
                AgentError::Other(anyhow::anyhow!(
                    "CLI timed out ({}s)",
                    VALIDATION_TIMEOUT.as_secs()
                ))
            })?
            .map_err(|e| AgentError::Other(anyhow::anyhow!("CLI read error: {e}")))?;

        let Some(msg) = msg else { break };

        if let CliMessage::Result(r) = msg {
            return Ok(r.result);
        }
    }

    Err(AgentError::Other(anyhow::anyhow!(
        "CLI exited without result message"
    )))
}

/// Read NDJSON until `ResultMessage`, parse structured output.
async fn read_structured_result(
    transport: &mut SubprocessTransport,
) -> Result<StructuredResponse, AgentError> {
    use super::claude_cli::CliMessage;

    let mut assistant_structured: Option<serde_json::Value> = None;

    loop {
        let msg = tokio::time::timeout(VALIDATION_TIMEOUT, transport.recv())
            .await
            .map_err(|_| AgentError::Other(anyhow::anyhow!("CLI timed out")))?
            .map_err(|e| AgentError::Other(anyhow::anyhow!("CLI read error: {e}")))?;

        let Some(msg) = msg else { break };

        if let CliMessage::Assistant(ref a) = msg {
            for block in &a.message.content {
                if block.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                    && let Some(input) = block.get("input")
                    && serde_json::from_value::<StructuredResponse>(input.clone()).is_ok()
                {
                    assistant_structured = Some(input.clone());
                }
            }
        }

        if let CliMessage::Result(r) = msg {
            // Try structured_output first
            if let Some(ref so) = r.structured_output
                && let Ok(resp) = serde_json::from_value::<StructuredResponse>(so.clone())
            {
                return Ok(resp);
            }
            // Fallback to assistant tool_use
            if let Some(ref fallback) = assistant_structured
                && let Ok(resp) = serde_json::from_value::<StructuredResponse>(fallback.clone())
            {
                return Ok(resp);
            }
            // Last resort: text with empty tools
            return Ok(StructuredResponse {
                text: r.result.unwrap_or_default(),
                tools: Vec::new(),
            });
        }
    }

    Err(AgentError::Other(anyhow::anyhow!(
        "CLI exited without result message"
    )))
}

/// Read NDJSON until `ResultMessage`, return raw JSON from `structured_output`.
async fn read_json_result(
    transport: &mut SubprocessTransport,
) -> Result<serde_json::Value, AgentError> {
    use super::claude_cli::CliMessage;

    loop {
        let msg = tokio::time::timeout(VALIDATION_TIMEOUT, transport.recv())
            .await
            .map_err(|_| AgentError::Other(anyhow::anyhow!("CLI timed out")))?
            .map_err(|e| AgentError::Other(anyhow::anyhow!("CLI read error: {e}")))?;

        let Some(msg) = msg else { break };

        if let CliMessage::Result(r) = msg {
            if let Some(so) = r.structured_output {
                return Ok(so);
            }
            if let Some(text) = r.result {
                // Try parsing the result text as JSON
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                    return Ok(json);
                }
            }
            return Err(AgentError::Other(anyhow::anyhow!(
                "no structured output in result"
            )));
        }
    }

    Err(AgentError::Other(anyhow::anyhow!(
        "CLI exited without result message"
    )))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_provider_extra_env_bedrock() {
        let vars = HashMap::from([
            ("AWS_ACCESS_KEY_ID".into(), "AKIA123".into()),
            ("AWS_SECRET_ACCESS_KEY".into(), "secret".into()),
        ]);
        let (api_key, extra) = build_provider_extra_env("bedrock", &vars);
        assert!(api_key.is_none());
        assert!(
            extra
                .iter()
                .any(|(k, v)| k == "AWS_ACCESS_KEY_ID" && v == "AKIA123")
        );
        assert!(
            extra
                .iter()
                .any(|(k, v)| k == "DISABLE_PROMPT_CACHING" && v == "1")
        );
    }

    #[test]
    fn build_provider_extra_env_vertex() {
        let vars = HashMap::from([("ANTHROPIC_VERTEX_PROJECT_ID".into(), "my-project".into())]);
        let (api_key, extra) = build_provider_extra_env("vertex", &vars);
        assert!(api_key.is_none());
        assert!(
            extra
                .iter()
                .any(|(k, v)| k == "CLAUDE_CODE_USE_VERTEX" && v == "1")
        );
        assert!(
            extra
                .iter()
                .any(|(k, v)| k == "CLOUD_ML_REGION" && v == "global")
        );
    }

    #[test]
    fn build_provider_extra_env_vertex_region_override() {
        let vars = HashMap::from([
            ("ANTHROPIC_VERTEX_PROJECT_ID".into(), "proj".into()),
            ("CLOUD_ML_REGION".into(), "us-central1".into()),
        ]);
        let (_, extra) = build_provider_extra_env("vertex", &vars);
        // User's region should be preserved, not overwritten
        let regions: Vec<_> = extra
            .iter()
            .filter(|(k, _)| k == "CLOUD_ML_REGION")
            .collect();
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].1, "us-central1");
    }

    #[test]
    fn build_provider_extra_env_custom_endpoint() {
        let vars = HashMap::from([
            ("ANTHROPIC_BASE_URL".into(), "https://litellm.local".into()),
            ("ANTHROPIC_API_KEY".into(), "sk-custom-123".into()),
        ]);
        let (api_key, extra) = build_provider_extra_env("custom_endpoint", &vars);
        assert_eq!(api_key.as_deref(), Some("sk-custom-123"));
        assert!(extra.iter().any(|(k, _)| k == "ANTHROPIC_BASE_URL"));
        // ANTHROPIC_API_KEY should NOT be in extra_env
        assert!(!extra.iter().any(|(k, _)| k == "ANTHROPIC_API_KEY"));
    }

    #[test]
    fn build_provider_extra_env_azure() {
        let vars = HashMap::from([("ANTHROPIC_FOUNDRY_API_KEY".into(), "foundry-key".into())]);
        let (api_key, extra) = build_provider_extra_env("azure_foundry", &vars);
        assert!(api_key.is_none());
        assert!(
            extra
                .iter()
                .any(|(k, v)| k == "ANTHROPIC_FOUNDRY_API_KEY" && v == "foundry-key")
        );
    }

    #[test]
    fn test_status_serialize() {
        let passed = serde_json::to_string(&TestStatus::Passed).unwrap();
        assert_eq!(passed, "\"passed\"");
        let failed = serde_json::to_string(&TestStatus::Failed).unwrap();
        assert_eq!(failed, "\"failed\"");
        let running = serde_json::to_string(&TestStatus::Running).unwrap();
        assert_eq!(running, "\"running\"");
    }

    #[test]
    fn test_result_serialize() {
        let result = TestResult {
            test: 1,
            name: "connection",
            status: TestStatus::Passed,
            detail: "ok".into(),
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"test\":1"));
        assert!(json.contains("\"connection\""));
        assert!(json.contains("\"passed\""));
    }

    #[test]
    fn validation_event_done_serialize() {
        let event = ValidationEvent::Done { all_passed: true };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"all_passed\":true"));
    }

    #[test]
    fn push_if_missing_does_not_overwrite() {
        let mut env = vec![("KEY".into(), "original".into())];
        push_if_missing(&mut env, "KEY", "new");
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].1, "original");
    }

    #[test]
    fn push_if_missing_adds_when_absent() {
        let mut env = vec![];
        push_if_missing(&mut env, "KEY", "value");
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].1, "value");
    }
}
