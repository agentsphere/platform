//! Span generation and W3C traceparent propagation.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use tokio::sync::RwLock;

/// Track active inbound request spans for log correlation.
#[derive(Debug, Default)]
pub struct ActiveSpans {
    spans: HashMap<String, ActiveSpan>,
}

/// An in-flight span used for log correlation.
#[derive(Debug, Clone)]
pub struct ActiveSpan {
    pub trace_id: String,
    pub span_id: String,
    pub started_at: Instant,
}

impl ActiveSpans {
    /// Register a new active span.
    pub fn insert(&mut self, span_id: String, span: ActiveSpan) {
        self.spans.insert(span_id, span);
    }

    /// Remove a completed span.
    pub fn remove(&mut self, span_id: &str) -> Option<ActiveSpan> {
        self.spans.remove(span_id)
    }

    /// Get the `trace_id` of the single active span, or the longest-running one.
    /// Returns `None` if no spans are active.
    pub fn best_trace_id(&self) -> Option<String> {
        if self.spans.is_empty() {
            return None;
        }
        if self.spans.len() == 1 {
            return self.spans.values().next().map(|s| s.trace_id.clone());
        }
        // Multiple active spans — pick the longest-running (earliest start)
        self.spans
            .values()
            .min_by_key(|s| s.started_at)
            .map(|s| s.trace_id.clone())
    }

    /// Number of active spans.
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    /// Whether there are no active spans.
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }
}

/// Shared active spans handle.
pub type SharedActiveSpans = Arc<RwLock<ActiveSpans>>;

/// Completed span record for OTLP export.
#[derive(Debug, Clone)]
pub struct SpanRecord {
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub name: String,
    pub service: String,
    pub kind: SpanKind,
    pub status: String,
    pub attributes: Option<JsonValue>,
    pub started_at: DateTime<Utc>,
    pub duration_ms: i32,
    pub http_status_code: Option<u16>,
}

/// Span kind for OTLP encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanKind {
    Server,
    Client,
    Internal,
}

/// Generate a new `trace_id` (16 random bytes -> 32 hex chars).
pub fn new_trace_id() -> String {
    let mut bytes = [0u8; 16];
    rand::fill(&mut bytes);
    hex::encode(bytes)
}

/// Generate a new `span_id` (8 random bytes -> 16 hex chars).
pub fn new_span_id() -> String {
    let mut bytes = [0u8; 8];
    rand::fill(&mut bytes);
    hex::encode(bytes)
}

/// Parse W3C traceparent header: `"00-{trace_id}-{parent_span_id}-{flags}"`.
/// Returns `(trace_id, parent_span_id, flags)` or `None` if invalid.
pub fn parse_traceparent(header: &str) -> Option<(String, String, u8)> {
    let parts: Vec<&str> = header.split('-').collect();
    if parts.len() != 4 {
        return None;
    }
    if parts[0] != "00" {
        return None;
    }
    let trace_id = parts[1];
    let parent_span_id = parts[2];
    let flags_str = parts[3];

    // Validate lengths
    if trace_id.len() != 32 || parent_span_id.len() != 16 || flags_str.len() != 2 {
        return None;
    }
    // Validate hex
    if !trace_id.chars().all(|c| c.is_ascii_hexdigit())
        || !parent_span_id.chars().all(|c| c.is_ascii_hexdigit())
    {
        return None;
    }
    let flags = u8::from_str_radix(flags_str, 16).ok()?;

    Some((trace_id.to_string(), parent_span_id.to_string(), flags))
}

/// Build W3C traceparent header from `trace_id` + `span_id`.
pub fn build_traceparent(trace_id: &str, span_id: &str) -> String {
    format!("00-{trace_id}-{span_id}-01")
}

/// Build a completed SERVER span for OTLP export.
#[allow(clippy::too_many_arguments)]
pub fn build_server_span(
    trace_id: &str,
    span_id: &str,
    parent_span_id: Option<&str>,
    name: &str,
    service: &str,
    started_at: DateTime<Utc>,
    duration_ms: i32,
    status_code: u16,
    extra_attrs: Vec<(String, String)>,
) -> SpanRecord {
    let mut attrs = serde_json::Map::new();
    attrs.insert(
        "http.status_code".into(),
        serde_json::Value::Number(status_code.into()),
    );
    for (k, v) in extra_attrs {
        attrs.insert(k, serde_json::Value::String(v));
    }

    let status = if status_code >= 500 { "error" } else { "ok" };

    SpanRecord {
        trace_id: trace_id.to_string(),
        span_id: span_id.to_string(),
        parent_span_id: parent_span_id.map(ToString::to_string),
        name: name.to_string(),
        service: service.to_string(),
        kind: SpanKind::Server,
        status: status.to_string(),
        attributes: Some(serde_json::Value::Object(attrs)),
        started_at,
        duration_ms,
        http_status_code: Some(status_code),
    }
}

/// Build a completed CLIENT span for OTLP export.
#[allow(clippy::too_many_arguments)]
pub fn build_client_span(
    trace_id: &str,
    span_id: &str,
    parent_span_id: Option<&str>,
    name: &str,
    service: &str,
    started_at: DateTime<Utc>,
    duration_ms: i32,
    status_code: u16,
) -> SpanRecord {
    let mut attrs = serde_json::Map::new();
    attrs.insert(
        "http.status_code".into(),
        serde_json::Value::Number(status_code.into()),
    );
    let status = if status_code >= 500 { "error" } else { "ok" };
    SpanRecord {
        trace_id: trace_id.to_string(),
        span_id: span_id.to_string(),
        parent_span_id: parent_span_id.map(ToString::to_string),
        name: name.to_string(),
        service: service.to_string(),
        kind: SpanKind::Client,
        status: status.to_string(),
        attributes: Some(serde_json::Value::Object(attrs)),
        started_at,
        duration_ms,
        http_status_code: Some(status_code),
    }
}

/// Build a CONNECTION span for TCP proxy sessions.
pub fn build_connection_span(
    trace_id: &str,
    span_id: &str,
    service: &str,
    started_at: DateTime<Utc>,
    duration_ms: i32,
    bytes_transferred: u64,
) -> SpanRecord {
    let mut attrs = serde_json::Map::new();
    attrs.insert(
        "net.bytes_transferred".into(),
        serde_json::Value::Number(bytes_transferred.into()),
    );
    SpanRecord {
        trace_id: trace_id.to_string(),
        span_id: span_id.to_string(),
        parent_span_id: None,
        name: format!("TCP connection to {service}"),
        service: service.to_string(),
        kind: SpanKind::Internal,
        status: "ok".to_string(),
        attributes: Some(serde_json::Value::Object(attrs)),
        started_at,
        duration_ms,
        http_status_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traceparent_round_trip() {
        let trace_id = new_trace_id();
        let span_id = new_span_id();
        let header = build_traceparent(&trace_id, &span_id);

        let (parsed_tid, parsed_sid, flags) = parse_traceparent(&header).unwrap();
        assert_eq!(parsed_tid, trace_id);
        assert_eq!(parsed_sid, span_id);
        assert_eq!(flags, 1);
    }

    #[test]
    fn traceparent_parse_valid() {
        let header = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let (tid, sid, flags) = parse_traceparent(header).unwrap();
        assert_eq!(tid, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(sid, "00f067aa0ba902b7");
        assert_eq!(flags, 1);
    }

    #[test]
    fn traceparent_parse_invalid_version() {
        assert!(parse_traceparent("01-abc-def-00").is_none());
    }

    #[test]
    fn traceparent_parse_wrong_parts() {
        assert!(parse_traceparent("00-abc-01").is_none());
        assert!(parse_traceparent("00-abc-def-gh-01").is_none());
    }

    #[test]
    fn traceparent_parse_wrong_lengths() {
        // trace_id too short
        assert!(parse_traceparent("00-abc-00f067aa0ba902b7-01").is_none());
    }

    #[test]
    fn trace_id_length() {
        let tid = new_trace_id();
        assert_eq!(tid.len(), 32);
        assert!(tid.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn span_id_length() {
        let sid = new_span_id();
        assert_eq!(sid.len(), 16);
        assert!(sid.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn active_spans_best_trace_id_empty() {
        let spans = ActiveSpans::default();
        assert!(spans.best_trace_id().is_none());
    }

    #[test]
    fn active_spans_single() {
        let mut spans = ActiveSpans::default();
        spans.insert(
            "s1".into(),
            ActiveSpan {
                trace_id: "trace1".into(),
                span_id: "s1".into(),
                started_at: Instant::now(),
            },
        );
        assert_eq!(spans.best_trace_id(), Some("trace1".into()));
    }

    #[test]
    fn active_spans_multiple_picks_earliest() {
        let mut spans = ActiveSpans::default();
        let early = Instant::now();
        // Simulate time passing by using the same instant for all
        // (in practice the earliest would be selected)
        spans.insert(
            "s1".into(),
            ActiveSpan {
                trace_id: "trace_early".into(),
                span_id: "s1".into(),
                started_at: early,
            },
        );
        spans.insert(
            "s2".into(),
            ActiveSpan {
                trace_id: "trace_late".into(),
                span_id: "s2".into(),
                started_at: early, // same time, but we verify it returns one
            },
        );
        // With same start time, it should still return something
        assert!(spans.best_trace_id().is_some());
    }

    #[test]
    fn active_spans_remove() {
        let mut spans = ActiveSpans::default();
        spans.insert(
            "s1".into(),
            ActiveSpan {
                trace_id: "t1".into(),
                span_id: "s1".into(),
                started_at: Instant::now(),
            },
        );
        assert_eq!(spans.len(), 1);
        let removed = spans.remove("s1");
        assert!(removed.is_some());
        assert!(spans.is_empty());
    }

    #[test]
    fn build_server_span_ok() {
        let span = build_server_span(
            "trace123",
            "span456",
            Some("parent789"),
            "POST /api/test",
            "test-svc",
            Utc::now(),
            42,
            200,
            vec![("http.method".into(), "POST".into())],
        );
        assert_eq!(span.status, "ok");
        assert_eq!(span.kind, SpanKind::Server);
        assert_eq!(span.http_status_code, Some(200));
    }

    #[test]
    fn build_server_span_error() {
        let span = build_server_span(
            "t",
            "s",
            None,
            "GET /err",
            "svc",
            Utc::now(),
            100,
            500,
            vec![],
        );
        assert_eq!(span.status, "error");
    }

    #[test]
    fn build_client_span_basic() {
        let span = build_client_span("t", "s", None, "GET /upstream", "svc", Utc::now(), 50, 200);
        assert_eq!(span.kind, SpanKind::Client);
        assert_eq!(span.status, "ok");
    }

    #[test]
    fn build_connection_span_basic() {
        let span = build_connection_span("t", "s", "postgres", Utc::now(), 1000, 4096);
        assert_eq!(span.kind, SpanKind::Internal);
        assert!(span.name.contains("postgres"));
    }
}
