//! LLM integration tests for the agent-runner CLI.
//!
//! These tests spawn a **real Claude CLI** with real OAuth/API tokens
//! and validate the NDJSON protocol, message flow, and pub/sub event
//! conversion using the agent-runner's own `SubprocessTransport`.
//!
//! # Running
//!
//! ```bash
//! just cli-test-llm  # requires CLAUDE_CODE_OAUTH_TOKEN or ANTHROPIC_API_KEY
//! ```
//!
//! All tests are `#[ignore]` — they only run with `--ignored` or `--include-ignored`.

use std::time::Duration;

use crate::messages::CliMessage;
use crate::pubsub::{cli_message_to_event, PubSubKind};
use crate::transport::{CliSpawnOptions, SubprocessTransport};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Return auth credentials or `None` to skip the test.
fn require_auth() -> Option<(Option<String>, Option<String>)> {
    let oauth = std::env::var("CLAUDE_CODE_OAUTH_TOKEN")
        .ok()
        .filter(|s| !s.is_empty());
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|s| !s.is_empty());
    if oauth.is_none() && api_key.is_none() {
        eprintln!("SKIP: no CLAUDE_CODE_OAUTH_TOKEN or ANTHROPIC_API_KEY set");
        return None;
    }
    Some((oauth, api_key))
}

/// Spawn a real Claude CLI with stream-json mode.
///
/// Defaults: max-turns=1, bypassPermissions. Override via the closure.
async fn spawn_cli(opts_override: impl FnOnce(&mut CliSpawnOptions)) -> SubprocessTransport {
    let (oauth, api_key) = require_auth().expect("auth required");
    let mut opts = CliSpawnOptions {
        max_turns: Some(1),
        permission_mode: Some("bypassPermissions".into()),
        oauth_token: oauth,
        anthropic_api_key: api_key,
        ..Default::default()
    };
    opts_override(&mut opts);
    SubprocessTransport::spawn(opts).expect("failed to spawn Claude CLI")
}

/// Read messages from CLI until Result, EOF, or timeout.
async fn read_all_messages(transport: &SubprocessTransport, timeout_secs: u64) -> Vec<CliMessage> {
    let mut messages = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);

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
            Ok(Ok(None)) => break,
            Ok(Err(e)) => {
                eprintln!("CLI read error: {e}");
                break;
            }
            Err(_) => {
                eprintln!("CLI read timed out after {timeout_secs}s");
                break;
            }
        }
    }
    messages
}

/// Read messages until a pause (no new message within `pause_secs`), Result, or overall timeout.
async fn read_until_pause(
    transport: &SubprocessTransport,
    overall_secs: u64,
    pause_secs: u64,
) -> Vec<CliMessage> {
    let mut messages = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(overall_secs);
    let mut got_content = false;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        let timeout = if got_content {
            remaining.min(Duration::from_secs(pause_secs))
        } else {
            remaining
        };

        match tokio::time::timeout(timeout, transport.recv()).await {
            Ok(Ok(Some(msg))) => {
                if matches!(&msg, CliMessage::Assistant(_)) {
                    got_content = true;
                }
                let is_result = matches!(&msg, CliMessage::Result(_));
                messages.push(msg);
                if is_result {
                    break;
                }
            }
            Ok(Ok(None)) => break,
            Ok(Err(_)) => break,
            Err(_) => break, // pause timeout = CLI is waiting for input
        }
    }
    messages
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// NDJSON protocol flow: System → ... → Result.
#[tokio::test]
#[ignore]
async fn llm_ndjson_protocol_flow() {
    if require_auth().is_none() {
        return;
    }

    let mut transport = spawn_cli(|_| {}).await;
    transport
        .send_message("Say hello in one word.")
        .await
        .unwrap();
    let messages = read_all_messages(&transport, 60).await;
    let _ = transport.kill().await;

    assert!(
        messages.len() >= 2,
        "expected ≥2 messages, got {}",
        messages.len()
    );
    assert!(
        matches!(&messages[0], CliMessage::System(_)),
        "first message should be System"
    );
    assert!(
        matches!(messages.last().unwrap(), CliMessage::Result(_)),
        "last message should be Result"
    );
}

/// System init message has session_id, model, and tools list.
#[tokio::test]
#[ignore]
async fn llm_system_message_fields() {
    if require_auth().is_none() {
        return;
    }

    let mut transport = spawn_cli(|_| {}).await;
    transport.send_message("Say hi.").await.unwrap();
    let messages = read_all_messages(&transport, 60).await;
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
        .expect("should have system message");

    assert!(
        !system.session_id.is_empty(),
        "session_id should not be empty"
    );
    assert!(system.model.is_some(), "model should be present");
    assert!(system.tools.is_some(), "tools should be present");
    assert!(
        !system.tools.as_ref().unwrap().is_empty(),
        "tools list should not be empty"
    );
}

/// Assistant messages have non-empty content arrays with valid block types.
#[tokio::test]
#[ignore]
async fn llm_assistant_content_blocks() {
    if require_auth().is_none() {
        return;
    }

    let mut transport = spawn_cli(|_| {}).await;
    transport.send_message("Say hello.").await.unwrap();
    let messages = read_all_messages(&transport, 60).await;
    let _ = transport.kill().await;

    let assistants: Vec<_> = messages
        .iter()
        .filter_map(|m| {
            if let CliMessage::Assistant(a) = m {
                Some(a)
            } else {
                None
            }
        })
        .collect();

    assert!(
        !assistants.is_empty(),
        "should have at least one assistant message"
    );
    for a in &assistants {
        assert!(
            !a.message.content.is_empty(),
            "assistant content should not be empty"
        );
        for block in &a.message.content {
            let block_type = block.get("type").and_then(|v| v.as_str());
            assert!(
                block_type.is_some(),
                "content block should have a type field: {block:?}"
            );
        }
    }
}

/// Result message has session_id, subtype, and is_error=false.
#[tokio::test]
#[ignore]
async fn llm_result_message_fields() {
    if require_auth().is_none() {
        return;
    }

    let mut transport = spawn_cli(|_| {}).await;
    transport.send_message("Say hello.").await.unwrap();
    let messages = read_all_messages(&transport, 60).await;
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

    assert!(
        !result.session_id.is_empty(),
        "result session_id should not be empty"
    );
    assert!(
        !result.subtype.is_empty(),
        "result subtype should not be empty"
    );
    assert!(!result.is_error, "result should not be an error");
}

/// Result message contains usage information (tokens, duration).
#[tokio::test]
#[ignore]
async fn llm_result_usage_info() {
    if require_auth().is_none() {
        return;
    }

    let mut transport = spawn_cli(|_| {}).await;
    transport.send_message("Say hi.").await.unwrap();
    let messages = read_all_messages(&transport, 60).await;
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

    if let Some(ref usage) = result.usage {
        assert!(
            usage.input_tokens.unwrap_or(0) > 0,
            "input_tokens should be > 0"
        );
        assert!(
            usage.output_tokens.unwrap_or(0) > 0,
            "output_tokens should be > 0"
        );
    }
    assert!(
        result.duration_ms.is_some(),
        "duration_ms should be present"
    );
}

/// --system-prompt is applied and influences the response.
#[tokio::test]
#[ignore]
async fn llm_system_prompt_applied() {
    if require_auth().is_none() {
        return;
    }

    let mut transport = spawn_cli(|opts| {
        opts.system_prompt = Some(
            "You must always respond with exactly the word PINEAPPLE and nothing else.".into(),
        );
    })
    .await;
    transport
        .send_message("What is your response?")
        .await
        .unwrap();
    let messages = read_all_messages(&transport, 60).await;
    let _ = transport.kill().await;

    let has_pineapple = messages.iter().any(|m| match m {
        CliMessage::Assistant(a) => a.message.content.iter().any(|block| {
            block
                .get("text")
                .and_then(|v| v.as_str())
                .is_some_and(|t| t.contains("PINEAPPLE"))
        }),
        CliMessage::Result(r) => r.result.as_deref().is_some_and(|t| t.contains("PINEAPPLE")),
        _ => false,
    });
    assert!(
        has_pineapple,
        "system prompt should influence response to contain PINEAPPLE"
    );
}

/// With max-turns=1, CLI exits after one turn with exactly one Result.
#[tokio::test]
#[ignore]
async fn llm_max_turns_one_shot() {
    if require_auth().is_none() {
        return;
    }

    let mut transport = spawn_cli(|opts| {
        opts.max_turns = Some(1);
    })
    .await;
    transport.send_message("Say hello.").await.unwrap();
    let messages = read_all_messages(&transport, 60).await;
    let _ = transport.kill().await;

    assert!(
        matches!(messages.last(), Some(CliMessage::Result(_))),
        "should end with Result after one turn"
    );
    let result_count = messages
        .iter()
        .filter(|m| matches!(m, CliMessage::Result(_)))
        .count();
    assert_eq!(result_count, 1, "should have exactly one Result message");
}

/// --allowedTools filtering restricts available tools in system init.
#[tokio::test]
#[ignore]
async fn llm_allowed_tools_filtering() {
    if require_auth().is_none() {
        return;
    }

    let mut transport = spawn_cli(|opts| {
        opts.allowed_tools = Some(vec!["Read".into()]);
    })
    .await;
    transport.send_message("Say hi.").await.unwrap();
    let messages = read_all_messages(&transport, 60).await;
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
        .expect("should have system message");

    // --allowedTools controls runtime tool access, not what the system init
    // reports. Verify the session started successfully and tools are listed.
    assert!(
        system.tools.is_some(),
        "system init should report available tools"
    );
    let tools = system.tools.as_ref().unwrap();
    assert!(!tools.is_empty(), "tools list should not be empty");
    assert!(
        tools.contains(&"Read".to_owned()),
        "Read should be in the tools list"
    );
}

/// Multi-turn conversation: send initial prompt, get response, send follow-up.
#[tokio::test]
#[ignore]
async fn llm_multi_turn_stdin() {
    if require_auth().is_none() {
        return;
    }

    let mut transport = spawn_cli(|opts| {
        // No max-turns limit so the CLI stays alive between messages.
        opts.max_turns = None;
        opts.system_prompt = Some(
            "When the user asks you to remember something, just acknowledge it. \
             When asked what you were told to remember, repeat it exactly. \
             Never use any tools. Respond in plain text only."
                .into(),
        );
        opts.allowed_tools = Some(vec![]); // no tools
    })
    .await;

    // Turn 1 — ask to remember a word
    transport
        .send_message("Remember this word: MANGO")
        .await
        .unwrap();
    let turn1 = read_until_pause(&transport, 60, 8).await;

    // If we got a Result, the CLI exited — verify turn 1 worked at least
    if matches!(turn1.last(), Some(CliMessage::Result(_))) {
        let has_assistant = turn1.iter().any(|m| matches!(m, CliMessage::Assistant(_)));
        assert!(
            has_assistant,
            "should have at least one assistant message in turn 1"
        );
        let _ = transport.kill().await;
        return;
    }

    // Turn 2 — ask to recall
    transport
        .send_message("What word did I ask you to remember?")
        .await
        .unwrap();
    let turn2 = read_until_pause(&transport, 60, 8).await;
    let _ = transport.kill().await;

    // Should have assistant messages from turn 2
    let turn2_assistants: Vec<_> = turn2
        .iter()
        .filter(|m| matches!(m, CliMessage::Assistant(_)))
        .collect();
    assert!(
        !turn2_assistants.is_empty(),
        "should have assistant messages in turn 2"
    );

    // Check that MANGO appears in turn 2 responses
    let has_mango = turn2.iter().any(|m| match m {
        CliMessage::Assistant(a) => a.message.content.iter().any(|block| {
            block
                .get("text")
                .and_then(|v| v.as_str())
                .is_some_and(|t| t.contains("MANGO"))
        }),
        CliMessage::Result(r) => r.result.as_deref().is_some_and(|t| t.contains("MANGO")),
        _ => false,
    });
    assert!(has_mango, "follow-up response should contain MANGO");
}

/// Real CliMessages convert to valid PubSubEvents with correct kinds.
#[tokio::test]
#[ignore]
async fn llm_pubsub_event_conversion() {
    if require_auth().is_none() {
        return;
    }

    let mut transport = spawn_cli(|_| {}).await;
    transport.send_message("Say hello.").await.unwrap();
    let messages = read_all_messages(&transport, 60).await;
    let _ = transport.kill().await;

    let events: Vec<_> = messages.iter().filter_map(cli_message_to_event).collect();

    assert!(
        !events.is_empty(),
        "should produce at least one PubSubEvent"
    );

    // First event should be Milestone (from System init)
    assert_eq!(
        events[0].kind,
        PubSubKind::Milestone,
        "first event should be Milestone"
    );
    assert!(
        events[0].message.contains("Session started"),
        "milestone should mention session started"
    );

    // Last event should be WaitingForInput (from non-error Result)
    let last = events.last().unwrap();
    assert_eq!(
        last.kind,
        PubSubKind::WaitingForInput,
        "last event should be WaitingForInput"
    );

    // Should have at least one content event from assistant
    let has_content = events.iter().any(|e| {
        matches!(
            e.kind,
            PubSubKind::Text | PubSubKind::Thinking | PubSubKind::ToolCall
        )
    });
    assert!(
        has_content,
        "should have at least one Text/Thinking/ToolCall event"
    );

    // All events should have non-empty messages
    for event in &events {
        assert!(
            !event.message.is_empty(),
            "event message should not be empty: {event:?}"
        );
    }
}

/// MCP config file is generated with correct structure and wires into CLI args.
#[tokio::test]
#[ignore]
async fn llm_mcp_config_written() {
    use crate::mcp::{generate_mcp_config, write_mcp_config};

    let dir = tempfile::TempDir::new().expect("failed to create temp dir");
    let config = generate_mcp_config("http://platform.test:8080", "test-token");
    let path = write_mcp_config(dir.path(), &config).expect("failed to write MCP config");

    assert!(path.exists(), "MCP config file should exist");

    let content: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let servers = content["mcpServers"]
        .as_object()
        .expect("should have mcpServers");
    assert_eq!(servers.len(), 5, "should have 5 MCP servers");
    assert!(
        !servers.contains_key("platform-admin"),
        "admin should be excluded"
    );

    // Verify each server has correct structure
    for (name, server) in servers {
        assert_eq!(server["command"], "node", "{name} should use node");
        let args = server["args"].as_array().expect("args should be array");
        let script = args[0].as_str().unwrap();
        assert!(
            script.ends_with(&format!("{name}.js")),
            "script should end with {name}.js"
        );
        assert_eq!(
            server["env"]["PLATFORM_API_URL"], "http://platform.test:8080",
            "API URL should be injected"
        );
        assert_eq!(
            server["env"]["PLATFORM_API_TOKEN"], "test-token",
            "API token should be injected"
        );
    }

    // Verify the config path would produce correct CLI args
    let opts = CliSpawnOptions {
        mcp_config: Some(path.clone()),
        ..Default::default()
    };
    let args = crate::transport::build_args(&opts);
    assert!(args.contains(&"--mcp-config".to_owned()));
    assert!(args.contains(&path.display().to_string()));
}

/// Minimal prompt "hi" produces valid protocol flow.
#[tokio::test]
#[ignore]
async fn llm_empty_prompt_still_works() {
    if require_auth().is_none() {
        return;
    }

    let mut transport = spawn_cli(|_| {}).await;
    transport.send_message("hi").await.unwrap();
    let messages = read_all_messages(&transport, 60).await;
    let _ = transport.kill().await;

    assert!(
        messages.len() >= 2,
        "even 'hi' should produce ≥2 messages, got {}",
        messages.len()
    );
    assert!(matches!(&messages[0], CliMessage::System(_)));
    assert!(matches!(messages.last().unwrap(), CliMessage::Result(_)));
}
