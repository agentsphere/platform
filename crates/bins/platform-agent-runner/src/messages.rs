// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

// Forked from src/agent/claude_cli/messages.rs — keep in sync manually

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// CLI stdout messages (received from CLI)
// ---------------------------------------------------------------------------

/// Top-level NDJSON message from CLI stdout.
///
/// The CLI emits one JSON object per line. The `type` field determines the
/// variant. Unknown types are captured by `Unknown` to allow forward compat.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum CliMessage {
    #[serde(rename = "system")]
    System(SystemMessage),

    #[serde(rename = "assistant")]
    Assistant(AssistantMessage),

    #[serde(rename = "user")]
    User(UserMessage),

    #[serde(rename = "result")]
    Result(ResultMessage),
}

/// System init message — first message emitted after CLI startup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemMessage {
    pub subtype: String,
    pub session_id: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub claude_code_version: Option<String>,
}

/// Assistant turn — contains content blocks (thinking, text, `tool_use`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub message: AssistantContent,
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantContent {
    pub content: Vec<serde_json::Value>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub usage: Option<UsageInfo>,
}

/// User turn (tool results from CLI back to assistant).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMessage {
    pub message: UserContent,
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserContent {
    pub content: Vec<serde_json::Value>,
}

/// Result message — final message when the CLI is done.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultMessage {
    pub subtype: String,
    pub session_id: String,
    pub is_error: bool,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub total_cost_usd: Option<f64>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub num_turns: Option<u32>,
    #[serde(default)]
    pub usage: Option<UsageInfo>,
}

/// Token usage information.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::struct_field_names)]
pub struct UsageInfo {
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_tokens: Option<u64>,
    #[serde(default)]
    pub cache_creation_tokens: Option<u64>,
}

// ---------------------------------------------------------------------------
// CLI stdin messages (sent to CLI)
// ---------------------------------------------------------------------------

/// Input message sent to CLI via stdin.
#[derive(Debug, Clone, Serialize)]
pub struct CliUserInput {
    #[serde(rename = "type")]
    pub msg_type: &'static str,
    pub message: CliUserInputContent,
}

#[derive(Debug, Clone, Serialize)]
pub struct CliUserInputContent {
    pub role: &'static str,
    pub content: serde_json::Value,
}

impl CliUserInput {
    /// Create a simple text user input.
    pub fn text(content: &str) -> Self {
        Self {
            msg_type: "user",
            message: CliUserInputContent {
                role: "user",
                content: serde_json::Value::String(content.to_owned()),
            },
        }
    }

    /// Create a structured (multi-part) user input.
    pub fn structured(content: serde_json::Value) -> Self {
        Self {
            msg_type: "user",
            message: CliUserInputContent {
                role: "user",
                content,
            },
        }
    }
}

/// Try to parse a single NDJSON line from CLI stdout.
/// Returns `None` for empty lines.
/// Returns `Err` for invalid JSON.
/// Returns `Ok(None)` for unknown message types (forward compat).
pub fn parse_cli_message(line: &str) -> Result<Option<CliMessage>, serde_json::Error> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    // First, check if this is a known type by peeking at the "type" field.
    let raw: serde_json::Value = serde_json::from_str(trimmed)?;
    let msg_type = raw.get("type").and_then(|v| v.as_str());

    match msg_type {
        Some("system" | "assistant" | "user" | "result") => {
            let msg: CliMessage = serde_json::from_value(raw)?;
            Ok(Some(msg))
        }
        // Unknown types (e.g. "stream_event") are silently skipped for forward compat.
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_init_deserialize() {
        let json = r#"{"type":"system","subtype":"init","session_id":"abc-123","tools":["Read","Write"],"model":"opus","claude_code_version":"1.0.0"}"#;
        let msg: CliMessage = serde_json::from_str(json).unwrap();
        match msg {
            CliMessage::System(s) => {
                assert_eq!(s.subtype, "init");
                assert_eq!(s.session_id, "abc-123");
                assert_eq!(s.model.as_deref(), Some("opus"));
                assert_eq!(s.tools.as_ref().unwrap().len(), 2);
                assert_eq!(s.claude_code_version.as_deref(), Some("1.0.0"));
            }
            _ => panic!("expected System"),
        }
    }

    #[test]
    fn system_init_with_optional_fields_null() {
        let json = r#"{"type":"system","subtype":"init","session_id":"abc-123"}"#;
        let msg: CliMessage = serde_json::from_str(json).unwrap();
        match msg {
            CliMessage::System(s) => {
                assert!(s.model.is_none());
                assert!(s.tools.is_none());
                assert!(s.claude_code_version.is_none());
            }
            _ => panic!("expected System"),
        }
    }

    #[test]
    fn assistant_message_deserialize() {
        let json = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello"}]},"session_id":"s1"}"#;
        let msg: CliMessage = serde_json::from_str(json).unwrap();
        match msg {
            CliMessage::Assistant(a) => {
                assert_eq!(a.message.content.len(), 1);
                assert_eq!(a.message.content[0]["type"], "text");
                assert_eq!(a.message.content[0]["text"], "hello");
            }
            _ => panic!("expected Assistant"),
        }
    }

    #[test]
    fn user_message_deserialize() {
        let json = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#;
        let msg: CliMessage = serde_json::from_str(json).unwrap();
        match msg {
            CliMessage::User(u) => {
                assert_eq!(u.message.content.len(), 1);
                assert_eq!(u.message.content[0]["type"], "tool_result");
            }
            _ => panic!("expected User"),
        }
    }

    #[test]
    fn result_success_deserialize() {
        let json = r#"{"type":"result","subtype":"success","session_id":"s1","is_error":false,"result":"done","total_cost_usd":0.05,"duration_ms":5000,"num_turns":3,"usage":{"input_tokens":100,"output_tokens":200}}"#;
        let msg: CliMessage = serde_json::from_str(json).unwrap();
        match msg {
            CliMessage::Result(r) => {
                assert_eq!(r.subtype, "success");
                assert!(!r.is_error);
                assert_eq!(r.result.as_deref(), Some("done"));
                assert_eq!(r.total_cost_usd, Some(0.05));
                assert_eq!(r.duration_ms, Some(5000));
                assert_eq!(r.num_turns, Some(3));
                let usage = r.usage.unwrap();
                assert_eq!(usage.input_tokens, Some(100));
                assert_eq!(usage.output_tokens, Some(200));
            }
            _ => panic!("expected Result"),
        }
    }

    #[test]
    fn result_error_deserialize() {
        let json =
            r#"{"type":"result","subtype":"error_max_turns","session_id":"s1","is_error":true}"#;
        let msg: CliMessage = serde_json::from_str(json).unwrap();
        match msg {
            CliMessage::Result(r) => {
                assert_eq!(r.subtype, "error_max_turns");
                assert!(r.is_error);
                assert!(r.result.is_none());
            }
            _ => panic!("expected Result"),
        }
    }

    #[test]
    fn unknown_type_returns_none() {
        let json = r#"{"type":"stream_event","event":"partial"}"#;
        let result = parse_cli_message(json).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn user_input_serialize_text() {
        let input = CliUserInput::text("hello");
        let json = serde_json::to_value(&input).unwrap();
        assert_eq!(json["type"], "user");
        assert_eq!(json["message"]["role"], "user");
        assert_eq!(json["message"]["content"], "hello");
    }

    #[test]
    fn user_input_structured_content() {
        let content = serde_json::json!([
            {"type": "text", "text": "analyze this"},
            {"type": "image", "source": {"type": "base64", "data": "..."}}
        ]);
        let input = CliUserInput::structured(content.clone());
        let json = serde_json::to_value(&input).unwrap();
        assert_eq!(json["type"], "user");
        assert_eq!(json["message"]["content"], content);
    }

    #[test]
    fn usage_info_deserialize() {
        let json = r#"{"input_tokens":500,"output_tokens":300,"cache_read_tokens":100}"#;
        let usage: UsageInfo = serde_json::from_str(json).unwrap();
        assert_eq!(usage.input_tokens, Some(500));
        assert_eq!(usage.output_tokens, Some(300));
        assert_eq!(usage.cache_read_tokens, Some(100));
        assert!(usage.cache_creation_tokens.is_none());
    }

    #[test]
    fn empty_json_object_rejected() {
        let result = serde_json::from_str::<CliMessage>("{}");
        assert!(result.is_err());
    }

    #[test]
    fn parse_cli_message_empty_line() {
        assert!(parse_cli_message("").unwrap().is_none());
        assert!(parse_cli_message("  ").unwrap().is_none());
    }

    #[test]
    fn parse_cli_message_invalid_json() {
        assert!(parse_cli_message("not json").is_err());
    }

    #[test]
    fn parse_cli_message_valid_system() {
        let json = r#"{"type":"system","subtype":"init","session_id":"s1"}"#;
        let msg = parse_cli_message(json).unwrap().unwrap();
        assert!(matches!(msg, CliMessage::System(_)));
    }
}
