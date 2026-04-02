//! Stdout/stderr capture, log parsing, and trace correlation.

use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{ChildStderr, ChildStdout};
use tokio::sync::mpsc;

use super::traces::SharedActiveSpans;

/// A parsed log line ready for OTLP export.
#[derive(Debug, Clone)]
pub struct LogRecord {
    pub timestamp: DateTime<Utc>,
    pub level: String,
    pub message: String,
    pub trace_id: Option<String>,
    pub span_id: Option<String>,
    pub is_stderr: bool,
    pub attributes: Option<JsonValue>,
}

/// Parse a single line from stdout or stderr.
///
/// JSON detection: tries `serde_json::from_str` first. Recognizes well-known
/// fields: msg/message, level/severity, `trace_id`, `span_id`, props/properties/attributes.
pub fn parse_line(line: &str, is_stderr: bool) -> LogRecord {
    // Try JSON parsing
    if let Ok(obj) = serde_json::from_str::<JsonValue>(line)
        && let Some(map) = obj.as_object()
    {
        let message = map
            .get("msg")
            .or_else(|| map.get("message"))
            .and_then(|v| v.as_str())
            .unwrap_or(line)
            .to_string();

        let level = map
            .get("level")
            .or_else(|| map.get("severity"))
            .and_then(|v| v.as_str())
            .map_or_else(
                || if is_stderr { "WARN" } else { "INFO" }.to_string(),
                normalize_level,
            );

        let trace_id = map
            .get("trace_id")
            .and_then(|v| v.as_str())
            .map(ToString::to_string);

        let span_id = map
            .get("span_id")
            .and_then(|v| v.as_str())
            .map(ToString::to_string);

        let attributes = map
            .get("props")
            .or_else(|| map.get("properties"))
            .or_else(|| map.get("attributes"))
            .cloned();

        return LogRecord {
            timestamp: Utc::now(),
            level,
            message,
            trace_id,
            span_id,
            is_stderr,
            attributes,
        };
    }

    // Plain text
    LogRecord {
        timestamp: Utc::now(),
        level: if is_stderr { "WARN" } else { "INFO" }.to_string(),
        message: line.to_string(),
        trace_id: None,
        span_id: None,
        is_stderr,
        attributes: None,
    }
}

/// Normalize level strings to uppercase standard names.
fn normalize_level(level: &str) -> String {
    match level.to_lowercase().as_str() {
        "trace" | "trce" => "TRACE".to_string(),
        "debug" | "dbug" => "DEBUG".to_string(),
        "info" | "information" => "INFO".to_string(),
        "warn" | "warning" => "WARN".to_string(),
        "error" | "err" | "fatal" | "critical" | "crit" => "ERROR".to_string(),
        _ => level.to_uppercase(),
    }
}

/// Correlate a log record with active spans.
///
/// Priority:
/// 1. Log has explicit `trace_id` -> use it
/// 2. Exactly one active inbound span -> use its `trace_id`
/// 3. Multiple active spans -> use longest-running (earliest start)
/// 4. No active spans -> leave `None` (pod lifecycle trace assigned by caller)
pub async fn correlate(log: &mut LogRecord, active_spans: &SharedActiveSpans) {
    if log.trace_id.is_some() {
        return;
    }
    let spans = active_spans.read().await;
    if let Some(tid) = spans.best_trace_id() {
        log.trace_id = Some(tid);
    }
}

/// Read lines from stdout and stderr, parse, correlate, and send to the log channel.
#[tracing::instrument(skip_all)]
pub async fn run_log_pipeline(
    stdout: BufReader<ChildStdout>,
    stderr: BufReader<ChildStderr>,
    log_tx: mpsc::Sender<LogRecord>,
    active_spans: SharedActiveSpans,
    mut shutdown: tokio::sync::watch::Receiver<()>,
) {
    let mut stdout_lines = stdout.lines();
    let mut stderr_lines = stderr.lines();

    loop {
        tokio::select! {
            line = stdout_lines.next_line() => {
                match line {
                    Ok(Some(text)) => {
                        let mut record = parse_line(&text, false);
                        correlate(&mut record, &active_spans).await;
                        let _ = log_tx.try_send(record);
                    }
                    Ok(None) => break, // stdout closed
                    Err(e) => {
                        tracing::warn!(error = %e, "stdout read error");
                        break;
                    }
                }
            }
            line = stderr_lines.next_line() => {
                match line {
                    Ok(Some(text)) => {
                        let mut record = parse_line(&text, true);
                        correlate(&mut record, &active_spans).await;
                        let _ = log_tx.try_send(record);
                    }
                    Ok(None) => break, // stderr closed
                    Err(e) => {
                        tracing::warn!(error = %e, "stderr read error");
                        break;
                    }
                }
            }
            _ = shutdown.changed() => break,
        }
    }

    tracing::debug!("log pipeline exiting");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plain_text_stdout() {
        let record = parse_line("hello world", false);
        assert_eq!(record.message, "hello world");
        assert_eq!(record.level, "INFO");
        assert!(!record.is_stderr);
        assert!(record.trace_id.is_none());
    }

    #[test]
    fn parse_plain_text_stderr() {
        let record = parse_line("error occurred", true);
        assert_eq!(record.level, "WARN");
        assert!(record.is_stderr);
    }

    #[test]
    fn parse_json_with_message() {
        let line = r#"{"msg":"test message","level":"info","trace_id":"abc123"}"#;
        let record = parse_line(line, false);
        assert_eq!(record.message, "test message");
        assert_eq!(record.level, "INFO");
        assert_eq!(record.trace_id, Some("abc123".into()));
    }

    #[test]
    fn parse_json_with_message_key() {
        let line = r#"{"message":"another test","severity":"warning"}"#;
        let record = parse_line(line, false);
        assert_eq!(record.message, "another test");
        assert_eq!(record.level, "WARN");
    }

    #[test]
    fn parse_json_with_props() {
        let line = r#"{"msg":"test","level":"debug","props":{"key":"value"}}"#;
        let record = parse_line(line, false);
        assert_eq!(record.level, "DEBUG");
        assert!(record.attributes.is_some());
    }

    #[test]
    fn parse_json_with_span_id() {
        let line = r#"{"msg":"test","span_id":"abcdef12"}"#;
        let record = parse_line(line, false);
        assert_eq!(record.span_id, Some("abcdef12".into()));
    }

    #[test]
    fn parse_malformed_json() {
        let line = r#"{"incomplete": json"#;
        let record = parse_line(line, false);
        assert_eq!(record.message, line);
        assert_eq!(record.level, "INFO");
    }

    #[test]
    fn normalize_level_variants() {
        assert_eq!(normalize_level("trace"), "TRACE");
        assert_eq!(normalize_level("DEBUG"), "DEBUG");
        assert_eq!(normalize_level("info"), "INFO");
        assert_eq!(normalize_level("information"), "INFO");
        assert_eq!(normalize_level("warning"), "WARN");
        assert_eq!(normalize_level("error"), "ERROR");
        assert_eq!(normalize_level("fatal"), "ERROR");
        assert_eq!(normalize_level("critical"), "ERROR");
        assert_eq!(normalize_level("unknown"), "UNKNOWN");
    }

    #[test]
    fn json_stderr_defaults_to_warn() {
        // JSON log on stderr without explicit level -> WARN
        let line = r#"{"msg":"oops"}"#;
        let record = parse_line(line, true);
        assert_eq!(record.level, "WARN");
    }

    #[test]
    fn json_with_explicit_level_on_stderr() {
        // JSON log on stderr with explicit level -> uses explicit
        let line = r#"{"msg":"info log","level":"info"}"#;
        let record = parse_line(line, true);
        assert_eq!(record.level, "INFO");
    }

    #[tokio::test]
    async fn correlate_with_explicit_trace_id() {
        let active_spans = SharedActiveSpans::default();
        let mut record = LogRecord {
            timestamp: Utc::now(),
            level: "INFO".into(),
            message: "test".into(),
            trace_id: Some("explicit_trace".into()),
            span_id: None,
            is_stderr: false,
            attributes: None,
        };
        correlate(&mut record, &active_spans).await;
        assert_eq!(record.trace_id, Some("explicit_trace".into()));
    }

    #[tokio::test]
    async fn correlate_with_active_span() {
        use super::super::traces::{ActiveSpan, ActiveSpans};
        use std::sync::Arc;
        use std::time::Instant;
        use tokio::sync::RwLock;

        let mut spans = ActiveSpans::default();
        spans.insert(
            "s1".into(),
            ActiveSpan {
                trace_id: "from_span".into(),
                span_id: "s1".into(),
                started_at: Instant::now(),
            },
        );
        let active_spans = Arc::new(RwLock::new(spans));

        let mut record = LogRecord {
            timestamp: Utc::now(),
            level: "INFO".into(),
            message: "test".into(),
            trace_id: None,
            span_id: None,
            is_stderr: false,
            attributes: None,
        };
        correlate(&mut record, &active_spans).await;
        assert_eq!(record.trace_id, Some("from_span".into()));
    }
}
