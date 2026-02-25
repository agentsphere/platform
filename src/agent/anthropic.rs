use std::collections::HashMap;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

use super::provider::{ProgressEvent, ProgressKind};

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MODEL: &str = "claude-sonnet-4-5-20250929";
const MAX_TOKENS: u32 = 8192;

// ---------------------------------------------------------------------------
// ChatMessage — supports both plain text and structured content blocks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: serde_json::Value,
}

impl ChatMessage {
    /// Create a user message with plain text content.
    pub fn user(text: &str) -> Self {
        Self {
            role: "user".into(),
            content: serde_json::Value::String(text.into()),
        }
    }

    /// Create an assistant message with plain text content.
    pub fn assistant_text(text: &str) -> Self {
        Self {
            role: "assistant".into(),
            content: serde_json::Value::String(text.into()),
        }
    }

    /// Create an assistant message with structured content blocks.
    pub fn assistant_blocks(blocks: Vec<serde_json::Value>) -> Self {
        Self {
            role: "assistant".into(),
            content: serde_json::Value::Array(blocks),
        }
    }

    /// Create a user message containing `tool_result` blocks.
    pub fn tool_results(results: Vec<serde_json::Value>) -> Self {
        Self {
            role: "user".into(),
            content: serde_json::Value::Array(results),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool use types
// ---------------------------------------------------------------------------

/// A tool call extracted from the Anthropic SSE stream.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

/// Result of a single conversation turn with tool support.
#[derive(Debug)]
pub enum TurnResult {
    /// Agent returned text only — turn is complete.
    Text(String),
    /// Agent wants to use tools. Contains any text emitted before tools,
    /// the tool calls, and the full assistant content blocks for history.
    ToolUse {
        text: String,
        tool_calls: Vec<ToolCall>,
        assistant_blocks: Vec<serde_json::Value>,
    },
}

// ---------------------------------------------------------------------------
// Streaming API — with tool use
// ---------------------------------------------------------------------------

/// Stream a conversation turn with tool support via the Anthropic Messages API.
///
/// Returns `TurnResult::Text` when the agent responds with text only, or
/// `TurnResult::ToolUse` when it wants to call tools. The caller should
/// execute tools and continue the conversation loop.
pub async fn stream_turn_with_tools(
    api_key: &str,
    model: Option<&str>,
    messages: &[ChatMessage],
    system: &str,
    tools: &[serde_json::Value],
    tx: &tokio::sync::broadcast::Sender<ProgressEvent>,
) -> Result<TurnResult, anyhow::Error> {
    let model = model.unwrap_or(DEFAULT_MODEL);

    let body = serde_json::json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        "stream": true,
        "system": system,
        "messages": messages,
        "tools": tools,
    });

    let response = send_anthropic_request(api_key, &body, tx).await?;

    let mut stream = response.bytes_stream();
    let mut buf = String::new();
    let mut state = StreamState::default();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buf.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(line_end) = buf.find('\n') {
            let line = buf[..line_end].trim_end_matches('\r').to_string();
            buf = buf[line_end + 1..].to_string();

            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            if data == "[DONE]" {
                continue;
            }
            process_sse_event(data, &mut state, tx);
        }
    }

    Ok(state.into_result())
}

// ---------------------------------------------------------------------------
// SSE stream processing
// ---------------------------------------------------------------------------

/// Mutable state accumulated during SSE stream processing.
#[derive(Default)]
struct StreamState {
    full_text: String,
    content_blocks: Vec<serde_json::Value>,
    /// `tool_use` blocks: index → (id, name, accumulated input JSON string)
    tool_blocks: HashMap<u64, (String, String, String)>,
}

impl StreamState {
    /// Convert accumulated state into a `TurnResult`.
    fn into_result(self) -> TurnResult {
        if self.tool_blocks.is_empty() {
            return TurnResult::Text(self.full_text);
        }

        let mut assistant_blocks = Vec::new();
        if !self.full_text.is_empty() {
            assistant_blocks.push(serde_json::json!({
                "type": "text",
                "text": self.full_text,
            }));
        }
        assistant_blocks.extend(self.content_blocks.iter().cloned());

        let tool_calls: Vec<ToolCall> = self
            .content_blocks
            .iter()
            .filter(|b| b.get("type").and_then(serde_json::Value::as_str) == Some("tool_use"))
            .map(|b| ToolCall {
                id: b
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                name: b
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                input: b.get("input").cloned().unwrap_or(serde_json::json!({})),
            })
            .collect();

        TurnResult::ToolUse {
            text: self.full_text,
            tool_calls,
            assistant_blocks,
        }
    }
}

/// Process a single SSE data payload, updating stream state.
fn process_sse_event(
    data: &str,
    state: &mut StreamState,
    tx: &tokio::sync::broadcast::Sender<ProgressEvent>,
) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
        return;
    };
    let Some(event_type) = v.get("type").and_then(serde_json::Value::as_str) else {
        return;
    };

    match event_type {
        "content_block_start" => process_block_start(&v, state, tx),
        "content_block_delta" => process_block_delta(&v, state, tx),
        "content_block_stop" => process_block_stop(&v, state),
        "error" => {
            let message = v
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown API error");
            let _ = tx.send(ProgressEvent {
                kind: ProgressKind::Error,
                message: message.to_owned(),
                metadata: None,
            });
        }
        _ => {}
    }
}

fn process_block_start(
    v: &serde_json::Value,
    state: &mut StreamState,
    tx: &tokio::sync::broadcast::Sender<ProgressEvent>,
) {
    let index = v
        .get("index")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let Some(cb) = v.get("content_block") else {
        return;
    };
    if cb.get("type").and_then(serde_json::Value::as_str) != Some("tool_use") {
        return;
    }
    let id = cb
        .get("id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_owned();
    let name = cb
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_owned();

    let _ = tx.send(ProgressEvent {
        kind: ProgressKind::ToolCall,
        message: name.clone(),
        metadata: Some(serde_json::json!({"tool_use_id": id})),
    });
    state.tool_blocks.insert(index, (id, name, String::new()));
}

fn process_block_delta(
    v: &serde_json::Value,
    state: &mut StreamState,
    tx: &tokio::sync::broadcast::Sender<ProgressEvent>,
) {
    let index = v
        .get("index")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let Some(delta) = v.get("delta") else { return };
    let delta_type = delta
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");

    match delta_type {
        "text_delta" => {
            if let Some(text) = delta.get("text").and_then(serde_json::Value::as_str) {
                state.full_text.push_str(text);
                let _ = tx.send(ProgressEvent {
                    kind: ProgressKind::Text,
                    message: text.to_owned(),
                    metadata: None,
                });
            }
        }
        "thinking_delta" => {
            if let Some(thinking) = delta.get("thinking").and_then(serde_json::Value::as_str) {
                let truncated: String = thinking.chars().take(200).collect();
                let _ = tx.send(ProgressEvent {
                    kind: ProgressKind::Thinking,
                    message: truncated,
                    metadata: None,
                });
            }
        }
        "input_json_delta" => {
            if let Some(partial) = delta
                .get("partial_json")
                .and_then(serde_json::Value::as_str)
                && let Some(entry) = state.tool_blocks.get_mut(&index)
            {
                entry.2.push_str(partial);
            }
        }
        _ => {}
    }
}

fn process_block_stop(v: &serde_json::Value, state: &mut StreamState) {
    let index = v
        .get("index")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if let Some((id, name, input_json)) = state.tool_blocks.get(&index) {
        let input: serde_json::Value =
            serde_json::from_str(input_json).unwrap_or(serde_json::json!({}));
        state.content_blocks.push(serde_json::json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": input,
        }));
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Send an HTTP request to the Anthropic API and check for errors.
async fn send_anthropic_request(
    api_key: &str,
    body: &serde_json::Value,
    tx: &tokio::sync::broadcast::Sender<ProgressEvent>,
) -> Result<reqwest::Response, anyhow::Error> {
    let client = reqwest::Client::new();
    let response = client
        .post(api_url())
        .header("x-api-key", api_key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let err_msg = format!("Anthropic API error ({status}): {body}");
        let _ = tx.send(ProgressEvent {
            kind: ProgressKind::Error,
            message: err_msg.clone(),
            metadata: None,
        });
        anyhow::bail!(err_msg);
    }

    Ok(response)
}

/// Get the Anthropic API URL. Configurable via `ANTHROPIC_API_URL` env var for testing.
fn api_url() -> &'static str {
    static URL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    URL.get_or_init(|| {
        std::env::var("ANTHROPIC_API_URL").unwrap_or_else(|_| ANTHROPIC_API_URL.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_message_user_constructor() {
        let msg = ChatMessage::user("hello");
        assert_eq!(msg.role, "user");
        assert_eq!(msg.content, serde_json::Value::String("hello".into()));
    }

    #[test]
    fn chat_message_assistant_text_constructor() {
        let msg = ChatMessage::assistant_text("reply");
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.content, serde_json::Value::String("reply".into()));
    }

    #[test]
    fn chat_message_assistant_blocks_constructor() {
        let blocks = vec![
            serde_json::json!({"type": "text", "text": "hi"}),
            serde_json::json!({"type": "tool_use", "id": "t1", "name": "foo", "input": {}}),
        ];
        let msg = ChatMessage::assistant_blocks(blocks.clone());
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.content, serde_json::Value::Array(blocks));
    }

    #[test]
    fn chat_message_tool_results_constructor() {
        let results = vec![serde_json::json!({
            "type": "tool_result",
            "tool_use_id": "t1",
            "content": "ok",
        })];
        let msg = ChatMessage::tool_results(results.clone());
        assert_eq!(msg.role, "user");
        assert_eq!(msg.content, serde_json::Value::Array(results));
    }

    #[test]
    fn chat_message_serializes_string_content() {
        let msg = ChatMessage::user("test");
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"], "test");
    }

    #[test]
    fn chat_message_serializes_array_content() {
        let msg = ChatMessage::tool_results(vec![serde_json::json!({
            "type": "tool_result",
            "tool_use_id": "abc",
            "content": "done",
        })]);
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "user");
        assert!(json["content"].is_array());
        assert_eq!(json["content"][0]["type"], "tool_result");
    }

    // --- SSE event processing tests ---

    fn make_tx() -> tokio::sync::broadcast::Sender<ProgressEvent> {
        let (tx, _) = tokio::sync::broadcast::channel(16);
        tx
    }

    #[test]
    fn process_text_delta_event() {
        let tx = make_tx();
        let mut rx = tx.subscribe();
        let mut state = StreamState::default();
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
        process_sse_event(data, &mut state, &tx);
        assert_eq!(state.full_text, "Hello");
        let event = rx.try_recv().unwrap();
        assert_eq!(event.kind, ProgressKind::Text);
        assert_eq!(event.message, "Hello");
    }

    #[test]
    fn process_thinking_delta_event() {
        let tx = make_tx();
        let mut rx = tx.subscribe();
        let mut state = StreamState::default();
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me think..."}}"#;
        process_sse_event(data, &mut state, &tx);
        assert!(
            state.full_text.is_empty(),
            "thinking should not affect full_text"
        );
        let event = rx.try_recv().unwrap();
        assert_eq!(event.kind, ProgressKind::Thinking);
        assert_eq!(event.message, "Let me think...");
    }

    #[test]
    fn process_tool_use_start_event() {
        let tx = make_tx();
        let mut rx = tx.subscribe();
        let mut state = StreamState::default();
        let data = r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_123","name":"create_project"}}"#;
        process_sse_event(data, &mut state, &tx);
        assert!(state.tool_blocks.contains_key(&1));
        let (id, name, json) = state.tool_blocks.get(&1).unwrap();
        assert_eq!(id, "toolu_123");
        assert_eq!(name, "create_project");
        assert!(json.is_empty());
        let event = rx.try_recv().unwrap();
        assert_eq!(event.kind, ProgressKind::ToolCall);
        assert_eq!(event.message, "create_project");
    }

    #[test]
    fn process_input_json_delta_accumulates() {
        let tx = make_tx();
        let mut state = StreamState::default();
        // First, start a tool_use block
        state
            .tool_blocks
            .insert(0, ("t1".into(), "tool".into(), String::new()));
        // Then send input_json_delta
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"na"}}"#;
        process_sse_event(data, &mut state, &tx);
        let data2 = r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"me\":\"test\"}"}}"#;
        process_sse_event(data2, &mut state, &tx);
        let (_, _, json) = state.tool_blocks.get(&0).unwrap();
        assert_eq!(json, r#"{"name":"test"}"#);
    }

    #[test]
    fn process_block_stop_finalizes_tool() {
        let tx = make_tx();
        let mut state = StreamState::default();
        state.tool_blocks.insert(
            0,
            (
                "t1".into(),
                "create_project".into(),
                r#"{"name":"my-app"}"#.into(),
            ),
        );
        let data = r#"{"type":"content_block_stop","index":0}"#;
        process_sse_event(data, &mut state, &tx);
        assert_eq!(state.content_blocks.len(), 1);
        let block = &state.content_blocks[0];
        assert_eq!(block["type"], "tool_use");
        assert_eq!(block["id"], "t1");
        assert_eq!(block["name"], "create_project");
        assert_eq!(block["input"]["name"], "my-app");
    }

    #[test]
    fn process_error_event() {
        let tx = make_tx();
        let mut rx = tx.subscribe();
        let mut state = StreamState::default();
        let data = r#"{"type":"error","error":{"message":"rate limit exceeded"}}"#;
        process_sse_event(data, &mut state, &tx);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.kind, ProgressKind::Error);
        assert_eq!(event.message, "rate limit exceeded");
    }

    #[test]
    fn process_unknown_event_ignored() {
        let tx = make_tx();
        let mut state = StreamState::default();
        let data = r#"{"type":"message_start","message":{"id":"msg_123"}}"#;
        process_sse_event(data, &mut state, &tx);
        assert!(state.full_text.is_empty());
        assert!(state.tool_blocks.is_empty());
        assert!(state.content_blocks.is_empty());
    }

    #[test]
    fn process_invalid_json_ignored() {
        let tx = make_tx();
        let mut state = StreamState::default();
        process_sse_event("not json", &mut state, &tx);
        assert!(state.full_text.is_empty());
    }

    #[test]
    fn process_text_block_stop_no_duplicate() {
        let tx = make_tx();
        let mut state = StreamState::default();
        state.full_text = "hello".into();
        // content_block_stop for a text block (not in tool_blocks) should not duplicate
        let data = r#"{"type":"content_block_stop","index":0}"#;
        process_sse_event(data, &mut state, &tx);
        assert!(state.content_blocks.is_empty());
    }

    #[test]
    fn process_block_start_non_tool_use_ignored() {
        let tx = make_tx();
        let mut state = StreamState::default();
        let data = r#"{"type":"content_block_start","index":0,"content_block":{"type":"text"}}"#;
        process_sse_event(data, &mut state, &tx);
        assert!(state.tool_blocks.is_empty());
    }

    #[test]
    fn stream_state_with_text_and_tools() {
        let mut state = StreamState::default();
        state.full_text = "Let me help.".into();
        state
            .tool_blocks
            .insert(1, ("t1".into(), "tool".into(), "{}".into()));
        state.content_blocks.push(serde_json::json!({
            "type": "tool_use", "id": "t1", "name": "tool", "input": {}
        }));
        let result = state.into_result();
        match result {
            TurnResult::ToolUse {
                text,
                tool_calls,
                assistant_blocks,
            } => {
                assert_eq!(text, "Let me help.");
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].name, "tool");
                // assistant_blocks should have text block + tool_use block
                assert_eq!(assistant_blocks.len(), 2);
                assert_eq!(assistant_blocks[0]["type"], "text");
            }
            _ => panic!("expected ToolUse"),
        }
    }

    // --- StreamState into_result tests ---

    #[test]
    fn stream_state_text_only() {
        let state = StreamState {
            full_text: "hello world".into(),
            ..Default::default()
        };
        let result = state.into_result();
        assert!(matches!(result, TurnResult::Text(t) if t == "hello world"));
    }

    #[test]
    fn stream_state_with_tool_blocks() {
        let mut state = StreamState::default();
        state
            .tool_blocks
            .insert(0, ("tid".into(), "tool".into(), "{}".into()));
        state.content_blocks.push(serde_json::json!({
            "type": "tool_use", "id": "tid", "name": "tool", "input": {}
        }));
        let result = state.into_result();
        assert!(matches!(result, TurnResult::ToolUse { tool_calls, .. } if tool_calls.len() == 1));
    }
}
