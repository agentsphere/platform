use fred::clients::Client;
use fred::interfaces::{ClientLike, EventInterface, PubsubInterface};
use fred::types::config::Config;
use serde::{Deserialize, Serialize};

use crate::control::{ControlPayload, ControlRequest};
use crate::messages::{AssistantMessage, CliMessage, ResultMessage, SystemMessage, UserMessage};

// ---------------------------------------------------------------------------
// Pub/sub event types
// ---------------------------------------------------------------------------

/// Event kind — matches the platform's `ProgressKind` enum exactly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PubSubKind {
    Text,
    Thinking,
    ToolCall,
    ToolResult,
    Milestone,
    Error,
    Completed,
    WaitingForInput,
    ProgressUpdate,
}

/// Event published by agent-runner to `session:{id}:events`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PubSubEvent {
    pub kind: PubSubKind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Input received from platform via `session:{id}:input`.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // Fields read via serde deserialization + tests
#[serde(tag = "type")]
pub enum PubSubInput {
    #[serde(rename = "prompt")]
    Prompt {
        content: String,
        #[serde(default)]
        source: Option<String>,
    },
    #[serde(rename = "control")]
    Control { control: ControlPayload },
}

// ---------------------------------------------------------------------------
// CliMessage → PubSubEvent conversion
// ---------------------------------------------------------------------------

/// Convert a CLI NDJSON message to a publishable `PubSubEvent`.
///
/// Returns `None` for message types that don't map to events (e.g. empty content).
pub fn cli_message_to_event(msg: &CliMessage) -> Option<PubSubEvent> {
    match msg {
        CliMessage::System(sys) => Some(convert_system(sys)),
        CliMessage::Assistant(a) => convert_assistant(a),
        CliMessage::User(u) => convert_user(u),
        CliMessage::Result(r) => Some(convert_result(r)),
    }
}

fn convert_system(sys: &SystemMessage) -> PubSubEvent {
    PubSubEvent {
        kind: PubSubKind::Milestone,
        message: format!(
            "Session started (model: {})",
            sys.model.as_deref().unwrap_or("default")
        ),
        metadata: Some(serde_json::json!({
            "session_id": sys.session_id,
            "claude_code_version": sys.claude_code_version,
        })),
    }
}

/// Convert an assistant message to a pub/sub event.
///
/// Emits ONE event per message using priority: thinking > tool_calls > text.
/// If thinking is present, it returns immediately (matching the platform's
/// `cli_message_to_progress()` behavior). This means text/tool_use blocks
/// in the same message are not emitted. This is intentional — the platform
/// sends one progress event per CLI message.
fn convert_assistant(a: &AssistantMessage) -> Option<PubSubEvent> {
    let content = &a.message.content;
    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();

    for block in content {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    text_parts.push(t.to_owned());
                }
            }
            Some("thinking") => {
                if let Some(t) = block.get("thinking").and_then(|v| v.as_str()) {
                    return Some(PubSubEvent {
                        kind: PubSubKind::Thinking,
                        message: t.to_owned(),
                        metadata: None,
                    });
                }
            }
            Some("tool_use") => {
                let name = block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let summary = extract_tool_summary(name, block.get("input"));
                tool_calls.push((name.to_owned(), summary));
            }
            _ => {}
        }
    }

    if !tool_calls.is_empty() {
        let names: Vec<&str> = tool_calls.iter().map(|(n, _)| n.as_str()).collect();
        let tools_meta: Vec<serde_json::Value> = tool_calls
            .iter()
            .map(|(name, summary)| {
                let mut obj = serde_json::json!({"name": name});
                if let Some(s) = summary {
                    obj["summary"] = serde_json::Value::String(s.clone());
                }
                obj
            })
            .collect();
        Some(PubSubEvent {
            kind: PubSubKind::ToolCall,
            message: names.join(", "),
            metadata: Some(serde_json::json!({"tools": tools_meta})),
        })
    } else if !text_parts.is_empty() {
        Some(PubSubEvent {
            kind: PubSubKind::Text,
            message: text_parts.join(""),
            metadata: None,
        })
    } else {
        None
    }
}

fn convert_user(u: &UserMessage) -> Option<PubSubEvent> {
    let content = &u.message.content;
    let mut results = Vec::new();

    for block in content {
        if let Some("tool_result") = block.get("type").and_then(|t| t.as_str()) {
            let tool_id = block
                .get("tool_use_id")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let preview = extract_result_preview(block);
            results.push((tool_id.to_owned(), preview));
        }
    }

    if results.is_empty() {
        None
    } else {
        let ids: Vec<&str> = results.iter().map(|(id, _)| id.as_str()).collect();
        let results_meta: Vec<serde_json::Value> = results
            .iter()
            .map(|(id, preview)| {
                let mut obj = serde_json::json!({"tool_use_id": id});
                if let Some(p) = preview {
                    obj["preview"] = serde_json::Value::String(p.clone());
                }
                obj
            })
            .collect();
        Some(PubSubEvent {
            kind: PubSubKind::ToolResult,
            message: format!("Tool results: {}", ids.join(", ")),
            metadata: Some(serde_json::json!({"results": results_meta})),
        })
    }
}

/// Extract a short summary from tool input based on tool name.
fn extract_tool_summary(tool_name: &str, input: Option<&serde_json::Value>) -> Option<String> {
    let input = input?;
    let raw = match tool_name {
        "Read" | "Write" => input.get("file_path").and_then(|v| v.as_str()),
        "Edit" => input.get("file_path").and_then(|v| v.as_str()),
        "Bash" => input.get("command").and_then(|v| v.as_str()),
        "Grep" => input.get("pattern").and_then(|v| v.as_str()),
        "Glob" => input.get("pattern").and_then(|v| v.as_str()),
        "Agent" => input.get("prompt").and_then(|v| v.as_str()),
        _ => None,
    }?;
    Some(truncate(raw, 150))
}

/// Extract a preview from a tool result content block.
fn extract_result_preview(block: &serde_json::Value) -> Option<String> {
    let content = block.get("content")?;
    let text = if let Some(s) = content.as_str() {
        s.to_owned()
    } else if let Some(arr) = content.as_array() {
        // Content can be an array of blocks; take first text block
        arr.iter()
            .find_map(|b| {
                if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                    b.get("text").and_then(|t| t.as_str()).map(|s| s.to_owned())
                } else {
                    None
                }
            })?
    } else {
        return None;
    };
    Some(truncate(&text, 200))
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_owned()
    } else {
        let mut end = max_len;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }
}

fn convert_result(r: &ResultMessage) -> PubSubEvent {
    let message = if r.is_error {
        r.result
            .as_deref()
            .unwrap_or("Agent completed with error")
            .to_owned()
    } else {
        // Non-error: turn completed, session stays open for follow-ups
        "Turn completed — waiting for input".to_owned()
    };

    // Non-error Result means the turn is done but session stays alive.
    // Publish WaitingForInput so persistence subscriber and SSE don't exit.
    // Completed is only published when the REPL truly exits (CLI process dies).
    let kind = if r.is_error {
        PubSubKind::Error
    } else {
        PubSubKind::WaitingForInput
    };

    PubSubEvent {
        kind,
        message,
        metadata: Some(serde_json::json!({
            "total_cost_usd": r.total_cost_usd,
            "duration_ms": r.duration_ms,
            "num_turns": r.num_turns,
            "is_error": r.is_error,
        })),
    }
}

// ---------------------------------------------------------------------------
// PubSubClient — Valkey pub/sub connection
// ---------------------------------------------------------------------------

/// Maximum message size accepted from pub/sub input (1 MB).
const MAX_INPUT_MESSAGE_SIZE: usize = 1_048_576;

/// Valkey pub/sub client for a single agent session.
pub struct PubSubClient {
    client: Client,
    session_id: String,
}

impl PubSubClient {
    /// Connect to Valkey and return a new `PubSubClient`.
    pub async fn connect(url: &str, session_id: &str) -> anyhow::Result<Self> {
        let config = Config::from_url(url)?;
        let client = Client::new(config, None, None, None);
        client.init().await?;

        Ok(Self {
            client,
            session_id: session_id.to_owned(),
        })
    }

    /// Channel name for receiving input from the platform.
    pub fn input_channel(&self) -> String {
        format!("session:{}:input", self.session_id)
    }

    /// Channel name for publishing events to the platform.
    pub fn events_channel(&self) -> String {
        format!("session:{}:events", self.session_id)
    }

    /// Publish a progress event to the events channel.
    pub async fn publish_event(&self, event: &PubSubEvent) -> anyhow::Result<()> {
        let json = serde_json::to_string(event)?;
        let channel = self.events_channel();
        self.client.publish::<(), _, _>(&channel, &json).await?;
        Ok(())
    }

    /// Subscribe to the input channel and return a receiver for parsed messages.
    ///
    /// Spawns a background task that:
    /// 1. Creates a dedicated subscriber client (clone_new)
    /// 2. Subscribes to `session:{id}:input`
    /// 3. Parses incoming JSON into `PubSubInput` (rejects messages > 1 MB)
    /// 4. Forwards via mpsc channel
    pub async fn subscribe_input(
        &self,
    ) -> anyhow::Result<tokio::sync::mpsc::Receiver<PubSubInput>> {
        let (tx, rx) = tokio::sync::mpsc::channel::<PubSubInput>(32);
        let channel = self.input_channel();

        // Dedicated subscriber connection
        let subscriber = self.client.clone_new();
        subscriber.init().await?;
        subscriber.subscribe(&channel).await?;

        let mut message_rx = subscriber.message_rx();

        tokio::spawn(async move {
            while let Ok(message) = message_rx.recv().await {
                let payload = match message.value.convert::<String>() {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[warn] pub/sub message not a string: {e}");
                        continue;
                    }
                };

                if payload.len() > MAX_INPUT_MESSAGE_SIZE {
                    eprintln!(
                        "[warn] pub/sub message too large ({} bytes, max {})",
                        payload.len(),
                        MAX_INPUT_MESSAGE_SIZE
                    );
                    continue;
                }

                match serde_json::from_str::<PubSubInput>(&payload) {
                    Ok(input) => {
                        if tx.send(input).await.is_err() {
                            break; // receiver dropped
                        }
                    }
                    Err(e) => {
                        eprintln!("[warn] invalid pub/sub input: {e}");
                    }
                }
            }
            eprintln!("[warn] pub/sub subscriber disconnected");
            // Keep subscriber alive until task ends
            let _subscriber = subscriber;
        });

        Ok(rx)
    }
}

/// Dispatch a `PubSubInput` to the CLI transport.
///
/// Prompt messages are prefixed based on their `source` field so the agent
/// can distinguish who sent the message:
/// - `"manager"` → `[From manager agent] ...`
/// - `"user"` → `[From user] ...`
/// - `None` → no prefix (backward compatible)
///
/// Note: In per-turn spawn mode, this is not used (prompts go via `-p` arg).
/// Kept for local REPL mode and tests.
#[allow(dead_code)]
pub async fn dispatch_input(
    transport: &crate::transport::SubprocessTransport,
    input: PubSubInput,
) -> Result<(), crate::error::CliError> {
    match input {
        PubSubInput::Prompt { content, source } => {
            let prefixed = match source.as_deref() {
                Some("manager") => format!("[From manager agent] {content}"),
                Some("user") => format!("[From user] {content}"),
                _ => content,
            };
            transport.send_message(&prefixed).await
        }
        PubSubInput::Control { control } => {
            let req = ControlRequest {
                msg_type: "control",
                control,
            };
            transport.send_control(req).await
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::{
        AssistantContent, AssistantMessage, ResultMessage, SystemMessage, UserContent, UserMessage,
    };

    // -- PubSubEvent serialization tests --

    #[test]
    fn pubsub_event_serialize_milestone() {
        let event = PubSubEvent {
            kind: PubSubKind::Milestone,
            message: "Session started (model: opus)".into(),
            metadata: Some(serde_json::json!({"session_id": "abc"})),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["kind"], "milestone");
        assert_eq!(json["message"], "Session started (model: opus)");
        assert_eq!(json["metadata"]["session_id"], "abc");
    }

    #[test]
    fn pubsub_event_serialize_text() {
        let event = PubSubEvent {
            kind: PubSubKind::Text,
            message: "Hello world".into(),
            metadata: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["kind"], "text");
        assert_eq!(json["message"], "Hello world");
        assert!(json.get("metadata").is_none());
    }

    #[test]
    fn pubsub_event_serialize_thinking() {
        let event = PubSubEvent {
            kind: PubSubKind::Thinking,
            message: "Let me analyze...".into(),
            metadata: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["kind"], "thinking");
    }

    #[test]
    fn pubsub_event_serialize_tool_call() {
        let event = PubSubEvent {
            kind: PubSubKind::ToolCall,
            message: "Read".into(),
            metadata: Some(serde_json::json!({"tool": "Read"})),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["kind"], "tool_call");
        assert_eq!(json["metadata"]["tool"], "Read");
    }

    #[test]
    fn pubsub_event_serialize_tool_result() {
        let event = PubSubEvent {
            kind: PubSubKind::ToolResult,
            message: "Tool results: t1".into(),
            metadata: Some(serde_json::json!({"tool_use_id": "t1"})),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["kind"], "tool_result");
    }

    #[test]
    fn pubsub_event_serialize_completed() {
        let event = PubSubEvent {
            kind: PubSubKind::Completed,
            message: "Done".into(),
            metadata: Some(
                serde_json::json!({"total_cost_usd": 0.05, "num_turns": 3, "duration_ms": 5000}),
            ),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["kind"], "completed");
        assert_eq!(json["metadata"]["total_cost_usd"], 0.05);
    }

    #[test]
    fn pubsub_event_serialize_progress_update() {
        let event = PubSubEvent {
            kind: PubSubKind::ProgressUpdate,
            message: "## Status: working\n## Tasks\n- [x] Done".into(),
            metadata: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["kind"], "progress_update");
        assert!(json["message"].as_str().unwrap().contains("Status: working"));
    }

    #[test]
    fn pubsub_event_serialize_error() {
        let event = PubSubEvent {
            kind: PubSubKind::Error,
            message: "Rate limit exceeded".into(),
            metadata: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["kind"], "error");
    }

    #[test]
    fn pubsub_event_serialize_no_metadata() {
        let event = PubSubEvent {
            kind: PubSubKind::Text,
            message: "hi".into(),
            metadata: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("metadata"));
    }

    // -- PubSubInput deserialization tests --

    #[test]
    fn pubsub_input_deserialize_prompt() {
        let json = r#"{"type":"prompt","content":"fix bug"}"#;
        let input: PubSubInput = serde_json::from_str(json).unwrap();
        match input {
            PubSubInput::Prompt { content, source } => {
                assert_eq!(content, "fix bug");
                assert!(source.is_none(), "source should be None for legacy format");
            }
            _ => panic!("expected Prompt"),
        }
    }

    #[test]
    fn pubsub_input_deserialize_prompt_with_source_manager() {
        let json = r#"{"type":"prompt","content":"check status","source":"manager"}"#;
        let input: PubSubInput = serde_json::from_str(json).unwrap();
        match input {
            PubSubInput::Prompt { content, source } => {
                assert_eq!(content, "check status");
                assert_eq!(source.as_deref(), Some("manager"));
            }
            _ => panic!("expected Prompt"),
        }
    }

    #[test]
    fn pubsub_input_deserialize_prompt_with_source_user() {
        let json = r#"{"type":"prompt","content":"hello","source":"user"}"#;
        let input: PubSubInput = serde_json::from_str(json).unwrap();
        match input {
            PubSubInput::Prompt { content, source } => {
                assert_eq!(content, "hello");
                assert_eq!(source.as_deref(), Some("user"));
            }
            _ => panic!("expected Prompt"),
        }
    }

    #[test]
    fn dispatch_prefix_manager_source() {
        let input = PubSubInput::Prompt {
            content: "do this".into(),
            source: Some("manager".into()),
        };
        // Extract the prefixed content (can't call dispatch_input without transport,
        // so test the prefix logic directly)
        match input {
            PubSubInput::Prompt { content, source } => {
                let prefixed = match source.as_deref() {
                    Some("manager") => format!("[From manager agent] {content}"),
                    Some("user") => format!("[From user] {content}"),
                    _ => content,
                };
                assert_eq!(prefixed, "[From manager agent] do this");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn dispatch_prefix_user_source() {
        let input = PubSubInput::Prompt {
            content: "help me".into(),
            source: Some("user".into()),
        };
        match input {
            PubSubInput::Prompt { content, source } => {
                let prefixed = match source.as_deref() {
                    Some("manager") => format!("[From manager agent] {content}"),
                    Some("user") => format!("[From user] {content}"),
                    _ => content,
                };
                assert_eq!(prefixed, "[From user] help me");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn dispatch_no_prefix_for_none_source() {
        let input = PubSubInput::Prompt {
            content: "raw message".into(),
            source: None,
        };
        match input {
            PubSubInput::Prompt { content, source } => {
                let prefixed = match source.as_deref() {
                    Some("manager") => format!("[From manager agent] {content}"),
                    Some("user") => format!("[From user] {content}"),
                    _ => content,
                };
                assert_eq!(prefixed, "raw message");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn pubsub_input_deserialize_control_interrupt() {
        let json = r#"{"type":"control","control":{"type":"interrupt"}}"#;
        let input: PubSubInput = serde_json::from_str(json).unwrap();
        match input {
            PubSubInput::Control { control } => {
                assert!(matches!(control, ControlPayload::Interrupt));
            }
            _ => panic!("expected Control"),
        }
    }

    #[test]
    fn pubsub_input_deserialize_control_set_model() {
        let json = r#"{"type":"control","control":{"type":"set_model","model":"opus"}}"#;
        let input: PubSubInput = serde_json::from_str(json).unwrap();
        match input {
            PubSubInput::Control { control } => match control {
                ControlPayload::SetModel { model } => assert_eq!(model, "opus"),
                _ => panic!("expected SetModel"),
            },
            _ => panic!("expected Control"),
        }
    }

    #[test]
    fn pubsub_input_deserialize_control_permission() {
        let json = r#"{"type":"control","control":{"type":"permission_response","id":"p1","granted":true}}"#;
        let input: PubSubInput = serde_json::from_str(json).unwrap();
        match input {
            PubSubInput::Control { control } => match control {
                ControlPayload::PermissionResponse { id, granted } => {
                    assert_eq!(id, "p1");
                    assert!(granted);
                }
                _ => panic!("expected PermissionResponse"),
            },
            _ => panic!("expected Control"),
        }
    }

    #[test]
    fn pubsub_input_deserialize_unknown_type() {
        let json = r#"{"type":"unknown"}"#;
        let result = serde_json::from_str::<PubSubInput>(json);
        assert!(result.is_err());
    }

    #[test]
    fn pubsub_input_deserialize_invalid_json() {
        let result = serde_json::from_str::<PubSubInput>("not json");
        assert!(result.is_err());
    }

    #[test]
    fn pubsub_input_deserialize_missing_content() {
        let json = r#"{"type":"prompt"}"#;
        let result = serde_json::from_str::<PubSubInput>(json);
        assert!(result.is_err());
    }

    // -- Channel name tests --

    #[test]
    fn input_channel_name() {
        let client = MockChannelHelper {
            session_id: "abc-123".into(),
        };
        assert_eq!(client.input_channel(), "session:abc-123:input");
    }

    #[test]
    fn events_channel_name() {
        let client = MockChannelHelper {
            session_id: "abc-123".into(),
        };
        assert_eq!(client.events_channel(), "session:abc-123:events");
    }

    // -- Round-trip test --

    #[test]
    fn pubsub_event_round_trip() {
        let event = PubSubEvent {
            kind: PubSubKind::ToolCall,
            message: "Read".into(),
            metadata: Some(serde_json::json!({"tool": "Read"})),
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: PubSubEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.kind, event.kind);
        assert_eq!(deserialized.message, event.message);
        assert_eq!(deserialized.metadata, event.metadata);
    }

    // -- CliMessage → PubSubEvent conversion tests --

    #[test]
    fn cli_message_to_event_system() {
        let msg = CliMessage::System(SystemMessage {
            subtype: "init".into(),
            session_id: "s1".into(),
            model: Some("opus".into()),
            tools: None,
            claude_code_version: Some("1.0".into()),
        });
        let event = cli_message_to_event(&msg).unwrap();
        assert_eq!(event.kind, PubSubKind::Milestone);
        assert!(event.message.contains("opus"));
    }

    #[test]
    fn cli_message_to_event_text() {
        let msg = CliMessage::Assistant(AssistantMessage {
            message: AssistantContent {
                content: vec![serde_json::json!({"type": "text", "text": "Hello world"})],
                model: None,
                usage: None,
            },
            session_id: None,
        });
        let event = cli_message_to_event(&msg).unwrap();
        assert_eq!(event.kind, PubSubKind::Text);
        assert_eq!(event.message, "Hello world");
    }

    #[test]
    fn cli_message_to_event_thinking() {
        let msg = CliMessage::Assistant(AssistantMessage {
            message: AssistantContent {
                content: vec![
                    serde_json::json!({"type": "thinking", "thinking": "Let me consider..."}),
                ],
                model: None,
                usage: None,
            },
            session_id: None,
        });
        let event = cli_message_to_event(&msg).unwrap();
        assert_eq!(event.kind, PubSubKind::Thinking);
        assert!(event.message.contains("Let me consider"));
    }

    #[test]
    fn cli_message_to_event_tool_call() {
        let msg = CliMessage::Assistant(AssistantMessage {
            message: AssistantContent {
                content: vec![
                    serde_json::json!({"type": "tool_use", "name": "Read", "id": "t1", "input": {"file_path": "/workspace/main.rs"}}),
                ],
                model: None,
                usage: None,
            },
            session_id: None,
        });
        let event = cli_message_to_event(&msg).unwrap();
        assert_eq!(event.kind, PubSubKind::ToolCall);
        assert!(event.message.contains("Read"));
        // Enriched metadata
        let meta = event.metadata.unwrap();
        let tools = meta["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "Read");
        assert_eq!(tools[0]["summary"], "/workspace/main.rs");
    }

    #[test]
    fn cli_message_to_event_tool_result() {
        let msg = CliMessage::User(UserMessage {
            message: UserContent {
                content: vec![
                    serde_json::json!({"type": "tool_result", "tool_use_id": "t1", "content": "file contents"}),
                ],
            },
            session_id: None,
        });
        let event = cli_message_to_event(&msg).unwrap();
        assert_eq!(event.kind, PubSubKind::ToolResult);
        // Enriched metadata
        let meta = event.metadata.unwrap();
        let results = meta["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["tool_use_id"], "t1");
        assert_eq!(results[0]["preview"], "file contents");
    }

    #[test]
    fn cli_message_to_event_result_success() {
        let msg = CliMessage::Result(ResultMessage {
            subtype: "success".into(),
            session_id: "s1".into(),
            is_error: false,
            result: Some("Done.".into()),
            total_cost_usd: Some(0.05),
            duration_ms: Some(1234),
            num_turns: Some(3),
            usage: None,
        });
        let event = cli_message_to_event(&msg).unwrap();
        assert_eq!(event.kind, PubSubKind::WaitingForInput);
        assert_eq!(event.message, "Turn completed — waiting for input");
    }

    #[test]
    fn cli_message_to_event_result_error() {
        let msg = CliMessage::Result(ResultMessage {
            subtype: "error".into(),
            session_id: "s1".into(),
            is_error: true,
            result: Some("Rate limit exceeded".into()),
            total_cost_usd: None,
            duration_ms: None,
            num_turns: None,
            usage: None,
        });
        let event = cli_message_to_event(&msg).unwrap();
        assert_eq!(event.kind, PubSubKind::Error);
        assert!(event.message.contains("Rate limit"));
    }

    #[test]
    fn cli_message_to_event_empty_content() {
        let msg = CliMessage::Assistant(AssistantMessage {
            message: AssistantContent {
                content: vec![],
                model: None,
                usage: None,
            },
            session_id: None,
        });
        assert!(cli_message_to_event(&msg).is_none());
    }

    #[test]
    fn cli_message_to_event_system_no_model() {
        let msg = CliMessage::System(SystemMessage {
            subtype: "init".into(),
            session_id: "s1".into(),
            model: None,
            tools: None,
            claude_code_version: None,
        });
        let event = cli_message_to_event(&msg).unwrap();
        assert_eq!(event.kind, PubSubKind::Milestone);
        assert!(event.message.contains("default"));
        // metadata should have session_id and null version
        let meta = event.metadata.unwrap();
        assert_eq!(meta["session_id"], "s1");
        assert!(meta["claude_code_version"].is_null());
    }

    #[test]
    fn cli_message_to_event_thinking_takes_priority_over_text() {
        // When thinking + text blocks are in the same message, thinking wins
        let msg = CliMessage::Assistant(AssistantMessage {
            message: AssistantContent {
                content: vec![
                    serde_json::json!({"type": "thinking", "thinking": "reasoning..."}),
                    serde_json::json!({"type": "text", "text": "Hello"}),
                ],
                model: None,
                usage: None,
            },
            session_id: None,
        });
        let event = cli_message_to_event(&msg).unwrap();
        assert_eq!(event.kind, PubSubKind::Thinking);
        assert_eq!(event.message, "reasoning...");
    }

    #[test]
    fn cli_message_to_event_multiple_tool_calls() {
        let msg = CliMessage::Assistant(AssistantMessage {
            message: AssistantContent {
                content: vec![
                    serde_json::json!({"type": "tool_use", "name": "Read", "id": "t1", "input": {"file_path": "/a.rs"}}),
                    serde_json::json!({"type": "tool_use", "name": "Write", "id": "t2", "input": {"file_path": "/b.rs"}}),
                ],
                model: None,
                usage: None,
            },
            session_id: None,
        });
        let event = cli_message_to_event(&msg).unwrap();
        assert_eq!(event.kind, PubSubKind::ToolCall);
        assert!(event.message.contains("Read"));
        assert!(event.message.contains("Write"));
        let meta = event.metadata.unwrap();
        let tools = meta["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
    }

    #[test]
    fn cli_message_to_event_tool_use_no_name() {
        let msg = CliMessage::Assistant(AssistantMessage {
            message: AssistantContent {
                content: vec![serde_json::json!({"type": "tool_use", "id": "t1", "input": {}})],
                model: None,
                usage: None,
            },
            session_id: None,
        });
        let event = cli_message_to_event(&msg).unwrap();
        assert_eq!(event.kind, PubSubKind::ToolCall);
        assert!(event.message.contains("unknown"));
    }

    #[test]
    fn cli_message_to_event_user_no_tool_results() {
        // User message with non-tool_result blocks → None
        let msg = CliMessage::User(UserMessage {
            message: UserContent {
                content: vec![serde_json::json!({"type": "text", "text": "hi"})],
            },
            session_id: None,
        });
        assert!(cli_message_to_event(&msg).is_none());
    }

    #[test]
    fn cli_message_to_event_user_empty_content() {
        let msg = CliMessage::User(UserMessage {
            message: UserContent { content: vec![] },
            session_id: None,
        });
        assert!(cli_message_to_event(&msg).is_none());
    }

    #[test]
    fn cli_message_to_event_user_multiple_tool_results() {
        let msg = CliMessage::User(UserMessage {
            message: UserContent {
                content: vec![
                    serde_json::json!({"type": "tool_result", "tool_use_id": "t1", "content": "ok"}),
                    serde_json::json!({"type": "tool_result", "tool_use_id": "t2", "content": "done"}),
                ],
            },
            session_id: None,
        });
        let event = cli_message_to_event(&msg).unwrap();
        assert_eq!(event.kind, PubSubKind::ToolResult);
        assert!(event.message.contains("t1"));
        assert!(event.message.contains("t2"));
        let meta = event.metadata.unwrap();
        let results = meta["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["preview"], "ok");
        assert_eq!(results[1]["preview"], "done");
    }

    #[test]
    fn cli_message_to_event_user_tool_result_no_id() {
        let msg = CliMessage::User(UserMessage {
            message: UserContent {
                content: vec![serde_json::json!({"type": "tool_result", "content": "ok"})],
            },
            session_id: None,
        });
        let event = cli_message_to_event(&msg).unwrap();
        assert!(event.message.contains("unknown"));
    }

    #[test]
    fn cli_message_to_event_result_no_result_text_success() {
        let msg = CliMessage::Result(ResultMessage {
            subtype: "success".into(),
            session_id: "s1".into(),
            is_error: false,
            result: None,
            total_cost_usd: None,
            duration_ms: None,
            num_turns: None,
            usage: None,
        });
        let event = cli_message_to_event(&msg).unwrap();
        assert_eq!(event.kind, PubSubKind::WaitingForInput);
        assert_eq!(event.message, "Turn completed — waiting for input");
    }

    #[test]
    fn cli_message_to_event_result_no_result_text_error() {
        let msg = CliMessage::Result(ResultMessage {
            subtype: "error".into(),
            session_id: "s1".into(),
            is_error: true,
            result: None,
            total_cost_usd: None,
            duration_ms: None,
            num_turns: None,
            usage: None,
        });
        let event = cli_message_to_event(&msg).unwrap();
        assert_eq!(event.kind, PubSubKind::Error);
        assert_eq!(event.message, "Agent completed with error");
    }

    #[test]
    fn cli_message_to_event_result_metadata_fields() {
        let msg = CliMessage::Result(ResultMessage {
            subtype: "success".into(),
            session_id: "s1".into(),
            is_error: false,
            result: Some("Done".into()),
            total_cost_usd: Some(0.123),
            duration_ms: Some(4567),
            num_turns: Some(8),
            usage: None,
        });
        let event = cli_message_to_event(&msg).unwrap();
        let meta = event.metadata.unwrap();
        assert_eq!(meta["total_cost_usd"], 0.123);
        assert_eq!(meta["duration_ms"], 4567);
        assert_eq!(meta["num_turns"], 8);
        assert_eq!(meta["is_error"], false);
    }

    #[test]
    fn cli_message_to_event_assistant_text_with_missing_text_field() {
        // text block but with missing "text" key → skipped
        let msg = CliMessage::Assistant(AssistantMessage {
            message: AssistantContent {
                content: vec![serde_json::json!({"type": "text"})],
                model: None,
                usage: None,
            },
            session_id: None,
        });
        assert!(cli_message_to_event(&msg).is_none());
    }

    #[test]
    fn cli_message_to_event_assistant_thinking_missing_thinking_field() {
        // thinking block but missing "thinking" key → falls through
        let msg = CliMessage::Assistant(AssistantMessage {
            message: AssistantContent {
                content: vec![serde_json::json!({"type": "thinking"})],
                model: None,
                usage: None,
            },
            session_id: None,
        });
        // No thinking text, no other blocks → None
        assert!(cli_message_to_event(&msg).is_none());
    }

    #[test]
    fn cli_message_to_event_assistant_unknown_block_type() {
        let msg = CliMessage::Assistant(AssistantMessage {
            message: AssistantContent {
                content: vec![serde_json::json!({"type": "server_tool_use", "name": "x"})],
                model: None,
                usage: None,
            },
            session_id: None,
        });
        // Unknown type skipped → None
        assert!(cli_message_to_event(&msg).is_none());
    }

    #[test]
    fn cli_message_to_event_assistant_block_with_no_type() {
        let msg = CliMessage::Assistant(AssistantMessage {
            message: AssistantContent {
                content: vec![serde_json::json!({"data": "something"})],
                model: None,
                usage: None,
            },
            session_id: None,
        });
        assert!(cli_message_to_event(&msg).is_none());
    }

    #[test]
    fn cli_message_to_event_tool_calls_take_priority_over_text() {
        // When tool_use + text blocks are in the same message, tool_use wins
        let msg = CliMessage::Assistant(AssistantMessage {
            message: AssistantContent {
                content: vec![
                    serde_json::json!({"type": "text", "text": "I'll read the file"}),
                    serde_json::json!({"type": "tool_use", "name": "Read", "id": "t1", "input": {}}),
                ],
                model: None,
                usage: None,
            },
            session_id: None,
        });
        let event = cli_message_to_event(&msg).unwrap();
        assert_eq!(event.kind, PubSubKind::ToolCall);
        assert!(event.message.contains("Read"));
    }

    #[test]
    fn cli_message_to_event_concatenates_text_parts() {
        let msg = CliMessage::Assistant(AssistantMessage {
            message: AssistantContent {
                content: vec![
                    serde_json::json!({"type": "text", "text": "Hello "}),
                    serde_json::json!({"type": "text", "text": "world"}),
                ],
                model: None,
                usage: None,
            },
            session_id: None,
        });
        let event = cli_message_to_event(&msg).unwrap();
        assert_eq!(event.kind, PubSubKind::Text);
        assert_eq!(event.message, "Hello world");
    }

    // -- extract_tool_summary tests --

    #[test]
    fn extract_summary_read_file_path() {
        let input = serde_json::json!({"file_path": "/workspace/src/main.rs"});
        let result = extract_tool_summary("Read", Some(&input));
        assert_eq!(result.as_deref(), Some("/workspace/src/main.rs"));
    }

    #[test]
    fn extract_summary_bash_command() {
        let input = serde_json::json!({"command": "npm install"});
        let result = extract_tool_summary("Bash", Some(&input));
        assert_eq!(result.as_deref(), Some("npm install"));
    }

    #[test]
    fn extract_summary_grep_pattern() {
        let input = serde_json::json!({"pattern": "fn main"});
        let result = extract_tool_summary("Grep", Some(&input));
        assert_eq!(result.as_deref(), Some("fn main"));
    }

    #[test]
    fn extract_summary_unknown_tool() {
        let input = serde_json::json!({"foo": "bar"});
        let result = extract_tool_summary("UnknownTool", Some(&input));
        assert!(result.is_none());
    }

    #[test]
    fn extract_summary_no_input() {
        let result = extract_tool_summary("Read", None);
        assert!(result.is_none());
    }

    #[test]
    fn extract_summary_truncates_long_input() {
        let long_path = format!("/workspace/{}", "a".repeat(200));
        let input = serde_json::json!({"file_path": long_path});
        let result = extract_tool_summary("Read", Some(&input)).unwrap();
        assert!(result.len() <= 153); // 150 + "..."
        assert!(result.ends_with("..."));
    }

    #[test]
    fn extract_result_preview_string_content() {
        let block = serde_json::json!({"type": "tool_result", "tool_use_id": "t1", "content": "hello world"});
        let result = extract_result_preview(&block);
        assert_eq!(result.as_deref(), Some("hello world"));
    }

    #[test]
    fn extract_result_preview_array_content() {
        let block = serde_json::json!({
            "type": "tool_result",
            "tool_use_id": "t1",
            "content": [{"type": "text", "text": "result data"}]
        });
        let result = extract_result_preview(&block);
        assert_eq!(result.as_deref(), Some("result data"));
    }

    #[test]
    fn extract_result_preview_no_content() {
        let block = serde_json::json!({"type": "tool_result", "tool_use_id": "t1"});
        let result = extract_result_preview(&block);
        assert!(result.is_none());
    }

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_exact_length() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn truncate_long_string() {
        let result = truncate("hello world", 5);
        assert_eq!(result, "hello...");
    }

    // -- Ignored integration tests (require real Valkey) --

    #[tokio::test]
    #[ignore]
    async fn pubsub_connect_and_publish() {
        let url = std::env::var("VALKEY_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
        let client = PubSubClient::connect(&url, "test-session").await.unwrap();
        let event = PubSubEvent {
            kind: PubSubKind::Milestone,
            message: "test".into(),
            metadata: None,
        };
        client.publish_event(&event).await.unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn pubsub_subscribe_and_receive() {
        let url = std::env::var("VALKEY_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
        let client = PubSubClient::connect(&url, "test-sub-session")
            .await
            .unwrap();

        let mut rx = client.subscribe_input().await.unwrap();

        // Publish from a separate connection
        let pub_client = PubSubClient::connect(&url, "test-sub-session")
            .await
            .unwrap();
        let channel = pub_client.input_channel();

        // Small delay for subscription to establish
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Publish a prompt
        let config = Config::from_url(&url).unwrap();
        let publisher = Client::new(config, None, None, None);
        publisher.init().await.unwrap();
        publisher
            .publish::<(), _, _>(&channel, r#"{"type":"prompt","content":"hello"}"#)
            .await
            .unwrap();

        // Receive
        let input = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        match input {
            PubSubInput::Prompt { content, source } => {
                assert_eq!(content, "hello");
                assert!(source.is_none());
            }
            _ => panic!("expected Prompt"),
        }
    }

    /// Helper for channel name tests without needing a real Valkey connection.
    struct MockChannelHelper {
        session_id: String,
    }

    impl MockChannelHelper {
        fn input_channel(&self) -> String {
            format!("session:{}:input", self.session_id)
        }
        fn events_channel(&self) -> String {
            format!("session:{}:events", self.session_id)
        }
    }
}
