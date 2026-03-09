//! Custom tracing layer that captures platform's own log events (warn+)
//! and forwards them to the observe pipeline so admins can see platform logs
//! in the Observe > Logs UI with `service = "platform"`.
//!
//! **Key constraint**: This layer must NEVER log itself (infinite recursion).
//! Uses `try_send()` only — drops events if the channel is full.

use chrono::Utc;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::Level;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

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

impl<S: tracing::Subscriber> Layer<S> for PlatformLogLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        if meta.level() > &self.min_level {
            return;
        }

        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);

        let message = visitor.message.unwrap_or_default();
        let level = meta.level().as_str().to_lowercase();
        let target = meta.target().to_owned();

        let mut attrs = visitor.fields;
        attrs.insert("target".to_owned(), json!(target));
        if let Some(file) = meta.file() {
            attrs.insert("file".to_owned(), json!(file));
        }
        if let Some(line) = meta.line() {
            attrs.insert("line".to_owned(), json!(line));
        }

        let record = LogEntryRecord {
            timestamp: Utc::now(),
            trace_id: None,
            span_id: None,
            project_id: None,
            session_id: None,
            user_id: None,
            service: "platform".into(),
            level,
            message,
            attributes: Some(json!(attrs)),
        };

        // Non-blocking send — drop if channel is full (no backpressure on platform)
        let _ = self.tx.try_send(record);
    }
}

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
        "info" => Level::INFO,
        "error" => Level::ERROR,
        _ => Level::WARN,
    }
}
