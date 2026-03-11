//! LLM integration tests for the create-app CLI flow.
//!
//! These tests use a **real Claude CLI** with real OAuth/API tokens.
//! They validate the actual NDJSON protocol, structured output, and session
//! management — but NOT specific LLM response content (non-deterministic).
//!
//! # Running
//!
//! ```bash
//! just test-llm  # requires CLAUDE_CODE_OAUTH_TOKEN or ANTHROPIC_API_KEY
//! ```
//!
//! All tests are `#[ignore]` — they only run with `--ignored` or `--include-ignored`.

use std::time::Duration;

use fred::interfaces::ClientLike;
use platform::agent::claude_cli::messages::{CliMessage, parse_cli_message};
use platform::agent::claude_cli::transport::{CliSpawnOptions, SubprocessTransport};
use platform::agent::cli_invoke::{CliInvokeParams, StructuredResponse, create_app_schema};

/// Skip the test gracefully if no auth credentials are available.
fn require_auth() -> (Option<String>, Option<String>) {
    let oauth = std::env::var("CLAUDE_CODE_OAUTH_TOKEN").ok();
    let api_key = std::env::var("ANTHROPIC_API_KEY").ok();
    if oauth.is_none() && api_key.is_none() {
        eprintln!("SKIP: no CLAUDE_CODE_OAUTH_TOKEN or ANTHROPIC_API_KEY set");
        return (None, None);
    }
    (oauth, api_key)
}

/// Helper: spawn a one-shot `claude -p` with structured output.
fn spawn_structured_cli(
    prompt: &str,
    session_id: Option<&str>,
    resume: Option<&str>,
) -> SubprocessTransport {
    let (oauth, api_key) = require_auth();

    let opts = CliSpawnOptions {
        prompt: Some(prompt.to_owned()),
        system_prompt: Some(
            "You are a helpful assistant. Always respond with valid JSON matching the schema."
                .into(),
        ),
        initial_session_id: session_id.map(String::from),
        resume_session: resume.map(String::from),
        json_schema: Some(serde_json::to_string(&create_app_schema()).unwrap()),
        disable_tools: true,
        max_turns: Some(1),
        permission_mode: Some("bypassPermissions".into()),
        oauth_token: oauth,
        anthropic_api_key: api_key,
        ..Default::default()
    };

    SubprocessTransport::spawn(opts).expect("failed to spawn Claude CLI")
}

/// Helper: spawn CLI in -p mode and immediately close stdin (not needed).
async fn spawn_and_prepare(
    prompt: &str,
    session_id: Option<&str>,
    resume: Option<&str>,
) -> SubprocessTransport {
    let transport = spawn_structured_cli(prompt, session_id, resume);
    transport.close_stdin().await;
    transport
}

/// Read all messages from CLI until result or timeout.
async fn read_all_messages(transport: &mut SubprocessTransport) -> Vec<CliMessage> {
    let mut messages = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        match tokio::time::timeout(remaining, transport.recv()).await {
            Ok(Ok(Some(msg))) => {
                let is_result = matches!(&msg, CliMessage::Result(_));
                messages.push(msg);
                if is_result {
                    break;
                }
            }
            Ok(Ok(None)) => break, // EOF
            Ok(Err(e)) => {
                eprintln!("CLI read error: {e}");
                break;
            }
            Err(_) => {
                eprintln!("CLI read timed out");
                break;
            }
        }
    }

    messages
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Structured output with empty tools (text-only response).
#[tokio::test]
#[ignore = "requires real Claude CLI"]
async fn llm_structured_output_text_only() {
    let (oauth, api_key) = require_auth();
    if oauth.is_none() && api_key.is_none() {
        return;
    }

    let mut transport = spawn_and_prepare("Say hello in one sentence.", None, None).await;
    let messages = read_all_messages(&mut transport).await;
    let _ = transport.kill().await;

    // Must have at least a system init and a result
    assert!(
        messages.len() >= 2,
        "expected at least 2 messages, got {}",
        messages.len()
    );

    // First message should be system init
    assert!(
        matches!(&messages[0], CliMessage::System(_)),
        "first message should be System"
    );

    // Last message should be a result
    let result = messages.last().unwrap();
    if let CliMessage::Result(r) = result {
        assert!(!r.is_error, "result should not be an error");
        assert!(
            r.structured_output.is_some(),
            "should have structured_output"
        );

        let so = r.structured_output.as_ref().unwrap();
        let resp: StructuredResponse = serde_json::from_value(so.clone())
            .expect("structured_output should be valid StructuredResponse");
        assert!(!resp.text.is_empty(), "text should not be empty");
        assert!(resp.tools.is_empty(), "tools should be empty for text-only");
    } else {
        panic!("last message should be Result");
    }
}

/// Structured output with a tool request.
#[tokio::test]
#[ignore = "requires real Claude CLI"]
async fn llm_structured_output_with_tool_request() {
    let (oauth, api_key) = require_auth();
    if oauth.is_none() && api_key.is_none() {
        return;
    }

    let mut transport = spawn_and_prepare(
        "Create a project called test-app with description 'A test application'",
        None,
        None,
    )
    .await;
    let messages = read_all_messages(&mut transport).await;
    let _ = transport.kill().await;

    let result = messages
        .iter()
        .find_map(|m| {
            if let CliMessage::Result(r) = m {
                Some(r)
            } else {
                None
            }
        })
        .expect("should have a result message");

    assert!(result.structured_output.is_some());
    let resp: StructuredResponse =
        serde_json::from_value(result.structured_output.clone().unwrap()).unwrap();
    assert!(!resp.text.is_empty());
    // The LLM should request create_project tool
    assert!(
        resp.tools.iter().any(|t| t.name == "create_project"),
        "expected create_project tool, got: {:?}",
        resp.tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );
}

/// Session ID and resume flow.
#[tokio::test]
#[ignore = "requires real Claude CLI"]
async fn llm_session_id_and_resume() {
    let (oauth, api_key) = require_auth();
    if oauth.is_none() && api_key.is_none() {
        return;
    }

    let session_id = uuid::Uuid::new_v4().to_string();

    // First call with --session-id
    let mut t1 = spawn_and_prepare("Remember the number 42.", Some(&session_id), None).await;
    let msgs1 = read_all_messages(&mut t1).await;
    let _ = t1.kill().await;

    let result1 = msgs1
        .iter()
        .find_map(|m| {
            if let CliMessage::Result(r) = m {
                Some(r)
            } else {
                None
            }
        })
        .expect("first call should have result");
    assert!(!result1.is_error);

    // Second call with --resume
    let mut t2 = spawn_and_prepare(
        "What number did I ask you to remember?",
        None,
        Some(&session_id),
    )
    .await;
    let msgs2 = read_all_messages(&mut t2).await;
    let _ = t2.kill().await;

    let result2 = msgs2
        .iter()
        .find_map(|m| {
            if let CliMessage::Result(r) = m {
                Some(r)
            } else {
                None
            }
        })
        .expect("second call should have result");
    assert!(!result2.is_error);

    // The response should mention 42 (context retained)
    let resp: StructuredResponse =
        serde_json::from_value(result2.structured_output.clone().unwrap()).unwrap();
    assert!(
        resp.text.contains("42"),
        "expected '42' in resumed response, got: {}",
        resp.text
    );
}

/// NDJSON stream format validation — all lines parse correctly.
#[tokio::test]
#[ignore = "requires real Claude CLI"]
async fn llm_ndjson_stream_format() {
    let (oauth, api_key) = require_auth();
    if oauth.is_none() && api_key.is_none() {
        return;
    }

    let mut transport = spawn_and_prepare("Say hello.", None, None).await;
    let messages = read_all_messages(&mut transport).await;
    let _ = transport.kill().await;

    assert!(
        !messages.is_empty(),
        "should have received at least one message"
    );

    // All messages should be valid CliMessage variants
    for msg in &messages {
        match msg {
            CliMessage::System(s) => {
                assert!(!s.session_id.is_empty());
                assert!(s.model.is_some());
            }
            CliMessage::Assistant(a) => {
                assert!(!a.message.content.is_empty());
            }
            CliMessage::Result(r) => {
                assert!(!r.session_id.is_empty());
            }
            CliMessage::User(_) => {} // valid
        }
    }
}

/// Result message contains `structured_output` matching our schema.
#[tokio::test]
#[ignore = "requires real Claude CLI"]
async fn llm_result_has_structured_output() {
    let (oauth, api_key) = require_auth();
    if oauth.is_none() && api_key.is_none() {
        return;
    }

    let mut transport = spawn_and_prepare("Say hello.", None, None).await;
    let messages = read_all_messages(&mut transport).await;
    let _ = transport.kill().await;

    let result = messages
        .iter()
        .find_map(|m| {
            if let CliMessage::Result(r) = m {
                Some(r)
            } else {
                None
            }
        })
        .expect("should have a result message");

    let so = result
        .structured_output
        .as_ref()
        .expect("result should have structured_output");

    // Must have "text" field (string)
    assert!(
        so["text"].is_string(),
        "structured_output.text should be a string"
    );

    // Must have "tools" field (array)
    assert!(
        so["tools"].is_array(),
        "structured_output.tools should be an array"
    );
}

/// `--tools ""` disables built-in tools — system init shows only `StructuredOutput`.
#[tokio::test]
#[ignore = "requires real Claude CLI"]
async fn llm_tools_empty_disables_builtins() {
    let (oauth, api_key) = require_auth();
    if oauth.is_none() && api_key.is_none() {
        return;
    }

    let mut transport = spawn_and_prepare("Say hello.", None, None).await;
    let messages = read_all_messages(&mut transport).await;
    let _ = transport.kill().await;

    let system = messages
        .iter()
        .find_map(|m| {
            if let CliMessage::System(s) = m {
                Some(s)
            } else {
                None
            }
        })
        .expect("should have a system message");

    if let Some(ref tools) = system.tools {
        // With --tools "", only StructuredOutput should be available
        for tool in tools {
            assert!(
                tool == "StructuredOutput" || tool.starts_with("mcp__"),
                "unexpected tool '{tool}' — --tools \"\" should disable builtins"
            );
        }
    }
}

/// `parse_cli_message` correctly handles real CLI output lines.
#[tokio::test]
#[ignore = "requires real Claude CLI"]
async fn llm_parse_cli_messages_roundtrip() {
    let (oauth, api_key) = require_auth();
    if oauth.is_none() && api_key.is_none() {
        return;
    }

    let mut transport = spawn_and_prepare("Say hello.", None, None).await;
    let messages = read_all_messages(&mut transport).await;
    let _ = transport.kill().await;

    // Serialize each message back to JSON and re-parse
    for msg in &messages {
        let json = serde_json::to_string(msg).expect("should serialize");
        let reparsed = parse_cli_message(&json)
            .expect("should parse")
            .expect("should not be None");
        // Verify same variant
        match (msg, &reparsed) {
            (CliMessage::System(_), CliMessage::System(_))
            | (CliMessage::Assistant(_), CliMessage::Assistant(_))
            | (CliMessage::Result(_), CliMessage::Result(_))
            | (CliMessage::User(_), CliMessage::User(_)) => {}
            _ => panic!("variant mismatch after roundtrip"),
        }
    }
}

/// End-to-end test: `invoke_cli()` completes with real Claude CLI.
///
/// This mirrors what the platform server does — same `CliSpawnOptions`,
/// same `close_stdin()` call, same timeout. Catches env/stdin/config
/// issues that unit tests miss.
#[tokio::test]
#[ignore = "requires real Claude CLI"]
async fn llm_invoke_cli_completes() {
    let (oauth, api_key) = require_auth();
    if oauth.is_none() && api_key.is_none() {
        return;
    }

    // Create a real Valkey pool (fire-and-forget pub/sub — OK if unavailable)
    let valkey_url =
        std::env::var("VALKEY_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
    let valkey_config =
        fred::types::config::Config::from_url(&valkey_url).expect("invalid VALKEY_URL");
    let valkey = fred::clients::Pool::new(valkey_config, None, None, None, 1)
        .expect("valkey pool creation failed");
    valkey.init().await.expect("valkey connection failed");

    let params = CliInvokeParams {
        session_id: uuid::Uuid::new_v4(),
        prompt: "Say hello in one sentence.".into(),
        is_resume: false,
        system_prompt: Some("You are a helpful assistant. Respond with valid JSON.".into()),
        oauth_token: oauth,
        anthropic_api_key: api_key,
        max_turns: Some(1),
    };

    let result = tokio::time::timeout(
        Duration::from_secs(60),
        platform::agent::cli_invoke::invoke_cli(params, &valkey),
    )
    .await
    .expect("invoke_cli should complete within 60s")
    .expect("invoke_cli should succeed");

    let (structured, result_msg) = result;
    assert!(!structured.text.is_empty(), "should have response text");
    assert!(result_msg.is_some(), "should have result message");
}
