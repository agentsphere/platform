use colored::Colorize;

use crate::messages::{AssistantMessage, CliMessage, ResultMessage, SystemMessage, UserMessage};

/// Render a CLI message to the terminal.
///
/// - System → stderr, dimmed
/// - Assistant thinking → stderr, dimmed
/// - Assistant text → **stdout** (allows piping)
/// - Assistant tool_use → stderr, cyan
/// - User tool_result → stderr, blue (content truncated at 200 chars)
/// - Result success → stderr, green
/// - Result error → stderr, red
pub fn render_message(msg: &CliMessage) {
    match msg {
        CliMessage::System(sys) => render_system(sys),
        CliMessage::Assistant(a) => render_assistant(a),
        CliMessage::User(u) => render_user(u),
        CliMessage::Result(r) => render_result(r),
    }
}

fn render_system(sys: &SystemMessage) {
    let model = sys.model.as_deref().unwrap_or("default");
    let version = sys
        .claude_code_version
        .as_deref()
        .map(|v| format!(" v{v}"))
        .unwrap_or_default();
    eprintln!(
        "{}",
        format!("Session started (model: {model}{version})").dimmed()
    );
}

fn render_assistant(a: &AssistantMessage) {
    for block in &a.message.content {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("thinking") => {
                if let Some(text) = block.get("thinking").and_then(|v| v.as_str()) {
                    eprintln!("{}", text.dimmed());
                }
            }
            Some("text") => {
                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                    // Text goes to stdout (allows piping)
                    println!("{text}");
                }
            }
            Some("tool_use") => {
                let name = block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                eprintln!("{} {}", "tool:".cyan(), name.cyan());
            }
            _ => {}
        }
    }
}

fn render_user(u: &UserMessage) {
    for block in &u.message.content {
        if let Some("tool_result") = block.get("type").and_then(|t| t.as_str()) {
            let tool_id = block
                .get("tool_use_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let content = block.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let truncated = truncate_str(content, 200);
            eprintln!("{} {} {}", "result:".blue(), tool_id.blue(), truncated);
        }
    }
}

fn render_result(r: &ResultMessage) {
    if r.is_error {
        let msg = r.result.as_deref().unwrap_or("Agent completed with error");
        eprintln!("{} {}", "error:".red().bold(), msg.red());
    } else {
        let msg = r
            .result
            .as_deref()
            .unwrap_or("Agent completed successfully");
        let mut detail = String::new();
        if let Some(cost) = r.total_cost_usd {
            detail.push_str(&format!(" cost=${cost:.4}"));
        }
        if let Some(turns) = r.num_turns {
            detail.push_str(&format!(" turns={turns}"));
        }
        if let Some(ms) = r.duration_ms {
            detail.push_str(&format!(" duration={ms}ms"));
        }
        eprintln!("{} {}{}", "done:".green().bold(), msg, detail.dimmed());
    }
}

/// Send a desktop notification (terminal bell on all platforms).
///
/// SAFETY: `title` and `body` must be hardcoded string literals — they are
/// interpolated into an AppleScript command on macOS with minimal escaping.
/// Do NOT pass user-controlled input.
pub fn notify_desktop(title: &str, body: &str) {
    // Terminal bell as universal notification
    eprint!("\x07");

    // macOS: use osascript for native notification
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("osascript")
            .args([
                "-e",
                &format!(
                    "display notification \"{}\" with title \"{}\"",
                    body.replace('"', "\\\""),
                    title.replace('"', "\\\"")
                ),
            ])
            .spawn();
    }

    // Linux: use notify-send if available
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("notify-send")
            .args([title, body])
            .spawn();
    }
}

/// Truncate a string to `max_len` chars, appending "..." if truncated.
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_owned()
    } else {
        let truncated: String = s.chars().take(max_len).collect();
        format!("{truncated}...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::{AssistantContent, ResultMessage, SystemMessage, UserContent};

    fn make_system() -> CliMessage {
        CliMessage::System(SystemMessage {
            subtype: "init".into(),
            session_id: "s1".into(),
            model: Some("opus".into()),
            tools: None,
            claude_code_version: Some("1.0".into()),
        })
    }

    fn make_assistant_text(text: &str) -> CliMessage {
        CliMessage::Assistant(AssistantMessage {
            message: AssistantContent {
                content: vec![serde_json::json!({"type": "text", "text": text})],
                model: None,
                usage: None,
            },
            session_id: None,
        })
    }

    fn make_assistant_thinking(text: &str) -> CliMessage {
        CliMessage::Assistant(AssistantMessage {
            message: AssistantContent {
                content: vec![serde_json::json!({"type": "thinking", "thinking": text})],
                model: None,
                usage: None,
            },
            session_id: None,
        })
    }

    fn make_assistant_tool_use(name: &str) -> CliMessage {
        CliMessage::Assistant(AssistantMessage {
            message: AssistantContent {
                content: vec![
                    serde_json::json!({"type": "tool_use", "name": name, "id": "t1", "input": {}}),
                ],
                model: None,
                usage: None,
            },
            session_id: None,
        })
    }

    #[test]
    fn render_system_message() {
        render_message(&make_system()); // no panic
    }

    #[test]
    fn render_assistant_text() {
        render_message(&make_assistant_text("Hello world")); // no panic
    }

    #[test]
    fn render_assistant_thinking() {
        render_message(&make_assistant_thinking("Let me think...")); // no panic
    }

    #[test]
    fn render_assistant_tool_use() {
        render_message(&make_assistant_tool_use("Read")); // no panic
    }

    #[test]
    fn render_assistant_multiple_blocks() {
        let msg = CliMessage::Assistant(AssistantMessage {
            message: AssistantContent {
                content: vec![
                    serde_json::json!({"type": "text", "text": "First"}),
                    serde_json::json!({"type": "text", "text": "Second"}),
                ],
                model: None,
                usage: None,
            },
            session_id: None,
        });
        render_message(&msg); // no panic
    }

    #[test]
    fn render_assistant_mixed_content() {
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
        render_message(&msg); // no panic
    }

    #[test]
    fn render_assistant_empty_content() {
        let msg = CliMessage::Assistant(AssistantMessage {
            message: AssistantContent {
                content: vec![],
                model: None,
                usage: None,
            },
            session_id: None,
        });
        render_message(&msg); // no panic
    }

    #[test]
    fn render_user_tool_result_short() {
        let msg = CliMessage::User(UserMessage {
            message: UserContent {
                content: vec![
                    serde_json::json!({"type": "tool_result", "tool_use_id": "t1", "content": "ok"}),
                ],
            },
            session_id: None,
        });
        render_message(&msg); // no panic
    }

    #[test]
    fn render_user_tool_result_exact_200() {
        let content: String = "x".repeat(200);
        let truncated = truncate_str(&content, 200);
        assert_eq!(truncated.len(), 200);
        assert!(!truncated.ends_with("..."));
    }

    #[test]
    fn render_user_tool_result_201_truncated() {
        let content: String = "x".repeat(201);
        let truncated = truncate_str(&content, 200);
        assert_eq!(truncated, format!("{}...", "x".repeat(200)));
    }

    #[test]
    fn render_result_success() {
        let msg = CliMessage::Result(ResultMessage {
            subtype: "success".into(),
            session_id: "s1".into(),
            is_error: false,
            result: Some("Done.".into()),
            total_cost_usd: None,
            duration_ms: None,
            num_turns: None,
            usage: None,
        });
        render_message(&msg); // no panic
    }

    #[test]
    fn render_result_success_with_metadata() {
        let msg = CliMessage::Result(ResultMessage {
            subtype: "success".into(),
            session_id: "s1".into(),
            is_error: false,
            result: Some("Done.".into()),
            total_cost_usd: Some(0.05),
            duration_ms: Some(5000),
            num_turns: Some(3),
            usage: None,
        });
        render_message(&msg); // no panic
    }

    #[test]
    fn render_result_success_no_metadata() {
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
        render_message(&msg); // no panic — uses default "Agent completed successfully"
    }

    #[test]
    fn render_result_error() {
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
        render_message(&msg); // no panic
    }

    #[test]
    fn render_result_error_no_message() {
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
        render_message(&msg); // no panic — uses fallback "Agent completed with error"
    }

    // R20: truncate_str with empty string
    #[test]
    fn truncate_str_empty() {
        assert_eq!(truncate_str("", 200), "");
    }

    // R20: truncate_str with multi-byte Unicode characters
    #[test]
    fn truncate_str_unicode_chars() {
        // Each emoji is 1 char but multiple bytes
        let emojis = "🎉🎊🎈🎁";
        assert_eq!(emojis.chars().count(), 4);
        let truncated = truncate_str(emojis, 2);
        assert_eq!(truncated, "🎉🎊...");
    }
}
