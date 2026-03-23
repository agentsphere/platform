//! Custom tracing layer that captures platform's own log events and forwards
//! them to the observe pipeline so admins can see platform logs in the
//! Observe > Logs UI with `service = "platform"`.
//!
//! **Key constraint**: This layer must NEVER log itself (infinite recursion).
//! Uses `try_send()` only — drops events if the channel is full.

use chrono::Utc;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::Level;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use uuid::Uuid;

use super::store::LogEntryRecord;

/// Create platform self-observe channel. Returns (sender for the layer, receiver for the bridge).
pub fn create_channel() -> (mpsc::Sender<LogEntryRecord>, mpsc::Receiver<LogEntryRecord>) {
    mpsc::channel(10_000)
}

/// Tracing layer that captures platform log events at the configured level and above.
pub struct PlatformLogLayer {
    tx: mpsc::Sender<LogEntryRecord>,
    min_level: Level,
}

impl PlatformLogLayer {
    pub fn new(tx: mpsc::Sender<LogEntryRecord>, min_level: Level) -> Self {
        Self { tx, min_level }
    }
}

// ---------------------------------------------------------------------------
// Span context storage
// ---------------------------------------------------------------------------

/// Well-known fields extracted from span attributes and stored in extensions.
#[derive(Default, Clone)]
struct SpanFields {
    project_id: Option<Uuid>,
    session_id: Option<Uuid>,
    user_id: Option<Uuid>,
    user_type: Option<String>,
    trace_id: Option<String>,
    task_name: Option<String>,
    source: Option<String>,
}

impl SpanFields {
    /// Merge non-None fields from `other` into self (only fills gaps).
    fn merge(&mut self, other: &SpanFields) {
        if self.project_id.is_none() {
            self.project_id = other.project_id;
        }
        if self.session_id.is_none() {
            self.session_id = other.session_id;
        }
        if self.user_id.is_none() {
            self.user_id = other.user_id;
        }
        if self.user_type.is_none() {
            self.user_type.clone_from(&other.user_type);
        }
        if self.trace_id.is_none() {
            self.trace_id.clone_from(&other.trace_id);
        }
        if self.task_name.is_none() {
            self.task_name.clone_from(&other.task_name);
        }
        if self.source.is_none() {
            self.source.clone_from(&other.source);
        }
    }
}

/// Visitor that records well-known fields from span attributes into `SpanFields`.
#[derive(Default)]
struct SpanFieldVisitor {
    fields: SpanFields,
}

impl tracing::field::Visit for SpanFieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let s = format!("{value:?}");
        self.record_str(field, &s);
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        match field.name() {
            "project_id" => self.fields.project_id = Uuid::parse_str(value).ok(),
            "session_id" => self.fields.session_id = Uuid::parse_str(value).ok(),
            "user_id" => self.fields.user_id = Uuid::parse_str(value).ok(),
            "user_type" => self.fields.user_type = Some(value.to_owned()),
            "trace_id" => self.fields.trace_id = Some(value.to_owned()),
            "task_name" => self.fields.task_name = Some(value.to_owned()),
            "source" => self.fields.source = Some(value.to_owned()),
            _ => {}
        }
    }

    fn record_u64(&mut self, _field: &tracing::field::Field, _value: u64) {}
    fn record_i64(&mut self, _field: &tracing::field::Field, _value: i64) {}
    fn record_bool(&mut self, _field: &tracing::field::Field, _value: bool) {}
}

// ---------------------------------------------------------------------------
// Layer implementation
// ---------------------------------------------------------------------------

impl<S> Layer<S> for PlatformLogLayer
where
    S: tracing::Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        let mut visitor = SpanFieldVisitor::default();
        attrs.record(&mut visitor);
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(visitor.fields);
        }
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: Context<'_, S>,
    ) {
        let mut visitor = SpanFieldVisitor::default();
        values.record(&mut visitor);
        if let Some(span) = ctx.span(id) {
            let mut ext = span.extensions_mut();
            if let Some(existing) = ext.get_mut::<SpanFields>() {
                existing.merge(&visitor.fields);
            } else {
                ext.insert(visitor.fields);
            }
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let meta = event.metadata();
        if meta.level() > &self.min_level {
            return;
        }

        // Extract event fields (message + arbitrary fields)
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);

        let message = visitor.message.unwrap_or_default();
        let level = meta.level().as_str().to_lowercase();
        let target = meta.target().to_owned();

        // Walk span chain to collect context
        let mut collected = SpanFields::default();
        if let Some(span) = ctx.event_span(event) {
            walk_span_chain(span, &mut collected);
        }

        // Build attributes
        let mut attrs = visitor.fields;
        attrs.insert("target".to_owned(), json!(target));
        if let Some(file) = meta.file() {
            attrs.insert("file".to_owned(), json!(file));
        }
        if let Some(line) = meta.line() {
            attrs.insert("line".to_owned(), json!(line));
        }
        if let Some(ref task_name) = collected.task_name {
            attrs.insert("task_name".to_owned(), json!(task_name));
        }
        if let Some(ref user_type) = collected.user_type {
            attrs.insert("user_type".to_owned(), json!(user_type));
        }

        // Determine source: span context > target heuristic
        let source = collected
            .source
            .unwrap_or_else(|| classify_source_from_target(&target));

        let record = LogEntryRecord {
            timestamp: Utc::now(),
            trace_id: collected.trace_id,
            span_id: None,
            project_id: collected.project_id,
            session_id: collected.session_id,
            user_id: collected.user_id,
            service: "platform".into(),
            level,
            source,
            message,
            attributes: Some(json!(attrs)),
        };

        // Non-blocking send — drop if channel is full (no backpressure on platform)
        let _ = self.tx.try_send(record);
    }
}

/// Walk up the span chain collecting the first non-None value for each field.
fn walk_span_chain<S>(span: tracing_subscriber::registry::SpanRef<'_, S>, out: &mut SpanFields)
where
    S: tracing::Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    let mut current = Some(span);
    while let Some(s) = current {
        let ext = s.extensions();
        if let Some(fields) = ext.get::<SpanFields>() {
            out.merge(fields);
        }
        current = s.parent();
    }
}

/// Classify log source from the tracing target path when no explicit source is set.
fn classify_source_from_target(target: &str) -> String {
    if target.contains("::api::") || target.contains("::auth::") {
        "api".into()
    } else {
        "system".into()
    }
}

// ---------------------------------------------------------------------------
// Event field visitor (message extraction)
// ---------------------------------------------------------------------------

/// Visitor that extracts the message field and all other fields from a tracing event.
#[derive(Default)]
struct FieldVisitor {
    message: Option<String>,
    fields: serde_json::Map<String, serde_json::Value>,
}

impl tracing::field::Visit for FieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{value:?}"));
        } else {
            self.fields
                .insert(field.name().to_owned(), json!(format!("{value:?}")));
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_owned());
        } else {
            self.fields.insert(field.name().to_owned(), json!(value));
        }
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.fields.insert(field.name().to_owned(), json!(value));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.fields.insert(field.name().to_owned(), json!(value));
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.fields.insert(field.name().to_owned(), json!(value));
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.fields.insert(field.name().to_owned(), json!(value));
    }
}

// ---------------------------------------------------------------------------
// Bridge + parse helpers
// ---------------------------------------------------------------------------

/// Spawn the bridge task that reads from the platform log channel and forwards
/// to the observe pipeline's logs channel.
pub fn spawn_bridge(
    mut platform_rx: mpsc::Receiver<LogEntryRecord>,
    logs_tx: mpsc::Sender<LogEntryRecord>,
) {
    tokio::spawn(async move {
        while let Some(record) = platform_rx.recv().await {
            // Non-blocking — drop if observe pipeline is full
            let _ = logs_tx.try_send(record);
        }
    });
}

/// Parse a level string (e.g. "warn", "error") into a tracing Level.
pub fn parse_level(s: &str) -> Level {
    match s.to_lowercase().as_str() {
        "trace" => Level::TRACE,
        "debug" => Level::DEBUG,
        "error" => Level::ERROR,
        "warn" => Level::WARN,
        _ => Level::INFO,
    }
}
