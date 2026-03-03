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
#[serde(tag = "type")]
pub enum PubSubInput {
    #[serde(rename = "prompt")]
    Prompt { content: String },
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
                tool_calls.push(name.to_owned());
            }
            _ => {}
        }
    }

    if !tool_calls.is_empty() {
        Some(PubSubEvent {
            kind: PubSubKind::ToolCall,
            message: tool_calls.join(", "),
            metadata: None,
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
            results.push(tool_id.to_owned());
        }
    }

    if results.is_empty() {
        None
    } else {
        Some(PubSubEvent {
            kind: PubSubKind::ToolResult,
            message: format!("Tool results: {}", results.join(", ")),
            metadata: None,
        })
    }
}

fn convert_result(r: &ResultMessage) -> PubSubEvent {
    let message = if r.is_error {
        r.result
            .as_deref()
            .unwrap_or("Agent completed with error")
            .to_owned()
    } else {
        r.result
            .as_deref()
            .unwrap_or("Agent completed successfully")
            .to_owned()
    };

    let kind = if r.is_error {
        PubSubKind::Error
    } else {
        PubSubKind::Completed
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
pub async fn dispatch_input(
    transport: &crate::transport::SubprocessTransport,
    input: PubSubInput,
) -> Result<(), crate::error::CliError> {
    match input {
        PubSubInput::Prompt { content } => transport.send_message(&content).await,
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
            PubSubInput::Prompt { content } => assert_eq!(content, "fix bug"),
            _ => panic!("expected Prompt"),
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
        assert_eq!(event.kind, PubSubKind::Completed);
        assert_eq!(event.message, "Done.");
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
            PubSubInput::Prompt { content } => assert_eq!(content, "hello"),
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
