use crate::agent::provider::{ProgressEvent, ProgressKind};

/// Parse a single line of Claude Code's `--output-format stream-json` output
/// into a structured `ProgressEvent`.
///
/// Claude Code emits one JSON object per line. Event types include:
///   `{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"..."}]}}`
///   `{"type":"assistant","message":{"content":[{"type":"text","text":"..."}]}}`
///   `{"type":"assistant","message":{"content":[{"type":"tool_use","name":"...","input":{...}}]}}`
///   `{"type":"assistant","message":{"content":[{"type":"tool_result",...}]}}`
///   `{"type":"result","result":"...","cost":{...},"usage":{...}}`
///   `{"type":"error","error":{"message":"..."}}`
pub fn parse_line(line: &str) -> Option<ProgressEvent> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let event_type = v.get("type")?.as_str()?;

    match event_type {
        "assistant" => parse_assistant_event(&v),
        "result" => Some(parse_result_event(&v)),
        "error" => {
            let message = v
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            Some(ProgressEvent {
                kind: ProgressKind::Error,
                message: message.to_owned(),
                metadata: None,
            })
        }
        _ => None,
    }
}

fn parse_assistant_event(v: &serde_json::Value) -> Option<ProgressEvent> {
    let content = v.get("message")?.get("content")?.as_array()?;
    let block = content.first()?;
    let block_type = block.get("type")?.as_str()?;

    match block_type {
        "thinking" => {
            let thinking = block.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
            // Truncate thinking for WebSocket delivery
            let truncated: String = thinking.chars().take(200).collect();
            Some(ProgressEvent {
                kind: ProgressKind::Thinking,
                message: truncated,
                metadata: None,
            })
        }
        "text" => {
            let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
            Some(ProgressEvent {
                kind: ProgressKind::Text,
                message: text.to_owned(),
                metadata: None,
            })
        }
        "tool_use" => {
            let tool_name = block
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("unknown");
            Some(ProgressEvent {
                kind: ProgressKind::ToolCall,
                message: format!("Using tool: {tool_name}"),
                metadata: Some(serde_json::json!({ "tool": tool_name })),
            })
        }
        "tool_result" => Some(ProgressEvent {
            kind: ProgressKind::ToolResult,
            message: "Tool completed".to_owned(),
            metadata: None,
        }),
        _ => None,
    }
}

fn parse_result_event(v: &serde_json::Value) -> ProgressEvent {
    let cost = v.get("cost").cloned();
    let usage = v.get("usage").cloned();

    ProgressEvent {
        kind: ProgressKind::Completed,
        message: "Agent session completed".to_owned(),
        metadata: Some(serde_json::json!({
            "cost": cost,
            "usage": usage,
        })),
    }
}

/// Extract total token usage from a result event for cost tracking.
/// Used by the reaper to update `cost_tokens` on session completion.
#[allow(dead_code)]
pub fn extract_tokens(line: &str) -> Option<i64> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("type")?.as_str()? != "result" {
        return None;
    }
    v.get("usage")?.get("total_tokens")?.as_i64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_thinking_event() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"Let me analyze the code..."}]}}"#;
        let event = parse_line(line).unwrap();
        assert_eq!(event.kind, ProgressKind::Thinking);
        assert_eq!(event.message, "Let me analyze the code...");
        assert!(event.metadata.is_none());
    }

    #[test]
    fn parse_thinking_truncates_long_content() {
        let long_thinking = "a".repeat(500);
        let line = format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"thinking","thinking":"{long_thinking}"}}]}}}}"#
        );
        let event = parse_line(&line).unwrap();
        assert_eq!(event.kind, ProgressKind::Thinking);
        assert_eq!(event.message.len(), 200);
    }

    #[test]
    fn parse_text_event() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Here is the solution."}]}}"#;
        let event = parse_line(line).unwrap();
        assert_eq!(event.kind, ProgressKind::Text);
        assert_eq!(event.message, "Here is the solution.");
    }

    #[test]
    fn parse_tool_use_event() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"write_file","input":{"path":"src/main.rs"}}]}}"#;
        let event = parse_line(line).unwrap();
        assert_eq!(event.kind, ProgressKind::ToolCall);
        assert_eq!(event.message, "Using tool: write_file");
        let meta = event.metadata.unwrap();
        assert_eq!(meta["tool"], "write_file");
    }

    #[test]
    fn parse_tool_result_event() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_result","content":"File written."}]}}"#;
        let event = parse_line(line).unwrap();
        assert_eq!(event.kind, ProgressKind::ToolResult);
        assert_eq!(event.message, "Tool completed");
    }

    #[test]
    fn parse_result_event() {
        let line = r#"{"type":"result","result":"Done","cost":{"input_tokens":100,"output_tokens":50},"usage":{"total_tokens":150}}"#;
        let event = parse_line(line).unwrap();
        assert_eq!(event.kind, ProgressKind::Completed);
        assert_eq!(event.message, "Agent session completed");
        let meta = event.metadata.unwrap();
        assert_eq!(meta["usage"]["total_tokens"], 150);
    }

    #[test]
    fn parse_error_event() {
        let line = r#"{"type":"error","error":{"message":"API key invalid"}}"#;
        let event = parse_line(line).unwrap();
        assert_eq!(event.kind, ProgressKind::Error);
        assert_eq!(event.message, "API key invalid");
    }

    #[test]
    fn parse_invalid_json_returns_none() {
        assert!(parse_line("not json at all").is_none());
        assert!(parse_line("").is_none());
        assert!(parse_line("{invalid}").is_none());
    }

    #[test]
    fn parse_unknown_type_returns_none() {
        let line = r#"{"type":"unknown_event","data":"something"}"#;
        assert!(parse_line(line).is_none());
    }

    #[test]
    fn parse_empty_content_array_returns_none() {
        let line = r#"{"type":"assistant","message":{"content":[]}}"#;
        assert!(parse_line(line).is_none());
    }

    #[test]
    fn extract_tokens_from_result() {
        let line = r#"{"type":"result","usage":{"total_tokens":1500}}"#;
        assert_eq!(extract_tokens(line), Some(1500));
    }

    #[test]
    fn extract_tokens_from_non_result_returns_none() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#;
        assert!(extract_tokens(line).is_none());
    }
}
