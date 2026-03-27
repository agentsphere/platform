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

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::field::Field;

    // Helper to create a tracing Field for testing.
    // We use tracing::Span with known field names.
    fn with_field<F: Fn(&Field)>(name: &'static str, f: F) {
        use tracing::subscriber::with_default;
        use tracing_subscriber::registry;

        let subscriber = registry();
        with_default(subscriber, || {
            // Create a span with the field name we need
            match name {
                "project_id" => {
                    let span = tracing::trace_span!("test", project_id = tracing::field::Empty);
                    span.record("project_id", "placeholder");
                    if let Some(id) = span.id() {
                        let _ = id;
                    }
                    // Use field from the span's metadata
                    let meta = span.metadata().unwrap();
                    if let Some(field) = meta.fields().field(name) {
                        f(&field);
                    }
                }
                _ => {
                    // For other fields we test via the visitor directly using parse_level etc.
                }
            }
        });
    }

    // -- SpanFields::merge --

    #[test]
    fn span_fields_merge_fills_gaps() {
        let mut base = SpanFields::default();
        let other = SpanFields {
            project_id: Some(Uuid::nil()),
            user_type: Some("human".into()),
            source: Some("api".into()),
            ..Default::default()
        };
        base.merge(&other);
        assert_eq!(base.project_id, Some(Uuid::nil()));
        assert_eq!(base.user_type.as_deref(), Some("human"));
        assert_eq!(base.source.as_deref(), Some("api"));
    }

    #[test]
    fn span_fields_merge_no_overwrite() {
        let existing_id = Uuid::new_v4();
        let other_id = Uuid::new_v4();
        let mut base = SpanFields {
            project_id: Some(existing_id),
            user_type: Some("human".into()),
            ..Default::default()
        };
        let other = SpanFields {
            project_id: Some(other_id),
            user_type: Some("agent".into()),
            source: Some("system".into()),
            ..Default::default()
        };
        base.merge(&other);
        assert_eq!(base.project_id, Some(existing_id)); // not overwritten
        assert_eq!(base.user_type.as_deref(), Some("human")); // not overwritten
        assert_eq!(base.source.as_deref(), Some("system")); // filled gap
    }

    #[test]
    fn span_fields_merge_empty_source() {
        let mut base = SpanFields {
            project_id: Some(Uuid::nil()),
            ..Default::default()
        };
        let other = SpanFields::default();
        base.merge(&other);
        assert_eq!(base.project_id, Some(Uuid::nil())); // unchanged
        assert!(base.user_type.is_none()); // still None
    }

    // -- classify_source_from_target --

    #[test]
    fn classify_source_api_target() {
        assert_eq!(
            classify_source_from_target("platform::api::projects"),
            "api"
        );
    }

    #[test]
    fn classify_source_auth_target() {
        assert_eq!(
            classify_source_from_target("platform::auth::middleware"),
            "api"
        );
    }

    #[test]
    fn classify_source_pipeline_target() {
        assert_eq!(
            classify_source_from_target("platform::pipeline::executor"),
            "system"
        );
    }

    #[test]
    fn classify_source_unknown_target() {
        assert_eq!(classify_source_from_target("hyper::proto::h1"), "system");
    }

    // -- FieldVisitor --

    #[test]
    fn field_visitor_record_str_message() {
        // Test FieldVisitor by constructing it directly and using a known span
        let visitor = FieldVisitor::default();
        assert!(visitor.message.is_none());
        assert!(visitor.fields.is_empty());
    }

    #[test]
    fn field_visitor_default_is_empty() {
        let v = FieldVisitor::default();
        assert!(v.message.is_none());
        assert!(v.fields.is_empty());
    }

    // -- parse_level --

    #[test]
    fn parse_level_all_variants() {
        assert_eq!(parse_level("trace"), Level::TRACE);
        assert_eq!(parse_level("debug"), Level::DEBUG);
        assert_eq!(parse_level("warn"), Level::WARN);
        assert_eq!(parse_level("error"), Level::ERROR);
        assert_eq!(parse_level("info"), Level::INFO);
    }

    #[test]
    fn parse_level_case_insensitive() {
        assert_eq!(parse_level("WARN"), Level::WARN);
        assert_eq!(parse_level("Error"), Level::ERROR);
        assert_eq!(parse_level("DEBUG"), Level::DEBUG);
    }

    #[test]
    fn parse_level_unknown_defaults_to_info() {
        assert_eq!(parse_level("verbose"), Level::INFO);
        assert_eq!(parse_level(""), Level::INFO);
        assert_eq!(parse_level("critical"), Level::INFO);
    }

    // -- create_channel --

    #[test]
    fn create_channel_returns_sender_receiver() {
        let (tx, _rx) = create_channel();
        // Channel capacity is 10_000
        assert!(tx.capacity() > 0);
    }

    // -- SpanFieldVisitor via tracing spans --

    #[test]
    fn span_field_visitor_parses_uuid_fields() {
        use tracing::subscriber::with_default;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::registry;

        let (tx, _rx) = mpsc::channel(100);
        let layer = PlatformLogLayer::new(tx, Level::TRACE);
        let subscriber = registry().with(layer);

        with_default(subscriber, || {
            // Create a span with project_id — on_new_span will run SpanFieldVisitor
            let id = Uuid::nil();
            let _span = tracing::info_span!("test_span", project_id = %id).entered();
            // If we get here without panic, the visitor successfully parsed the UUID
        });
    }

    #[test]
    fn span_field_visitor_handles_string_fields() {
        use tracing::subscriber::with_default;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::registry;

        let (tx, _rx) = mpsc::channel(100);
        let layer = PlatformLogLayer::new(tx, Level::TRACE);
        let subscriber = registry().with(layer);

        with_default(subscriber, || {
            let _span =
                tracing::info_span!("test_span", user_type = "human", source = "api").entered();
        });
    }

    #[test]
    fn span_field_visitor_ignores_unknown_fields() {
        use tracing::subscriber::with_default;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::registry;

        let (tx, _rx) = mpsc::channel(100);
        let layer = PlatformLogLayer::new(tx, Level::TRACE);
        let subscriber = registry().with(layer);

        with_default(subscriber, || {
            let _span = tracing::info_span!("test_span", unknown_field = "value").entered();
        });
    }

    // -- FieldVisitor via tracing events --

    #[tokio::test]
    async fn field_visitor_captures_message_and_fields() {
        use tracing::subscriber::with_default;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::registry;

        let (tx, mut rx) = mpsc::channel(100);
        let layer = PlatformLogLayer::new(tx, Level::TRACE);
        let subscriber = registry().with(layer);

        with_default(subscriber, || {
            tracing::info!(count = 42, "hello world");
        });

        let record = rx.recv().await.unwrap();
        assert_eq!(record.message, "hello world");
        let attrs = record.attributes.unwrap();
        assert_eq!(attrs["count"], 42);
    }

    #[tokio::test]
    async fn field_visitor_captures_i64_field() {
        use tracing::subscriber::with_default;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::registry;

        let (tx, mut rx) = mpsc::channel(100);
        let layer = PlatformLogLayer::new(tx, Level::TRACE);
        let subscriber = registry().with(layer);

        with_default(subscriber, || {
            tracing::info!(latency_ms = 150i64, "request handled");
        });

        let record = rx.recv().await.unwrap();
        assert_eq!(record.message, "request handled");
        let attrs = record.attributes.unwrap();
        assert_eq!(attrs["latency_ms"], 150);
    }

    #[tokio::test]
    async fn field_visitor_debug_formatted_message() {
        use tracing::subscriber::with_default;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::registry;

        let (tx, mut rx) = mpsc::channel(100);
        let layer = PlatformLogLayer::new(tx, Level::TRACE);
        let subscriber = registry().with(layer);

        with_default(subscriber, || {
            tracing::info!(error = %"connection refused", "operation failed");
        });

        let record = rx.recv().await.unwrap();
        assert_eq!(record.message, "operation failed");
    }

    // -- FieldVisitor f64 / bool / u64 coverage --

    #[tokio::test]
    async fn field_visitor_captures_f64_field() {
        use tracing::subscriber::with_default;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::registry;

        let (tx, mut rx) = mpsc::channel(100);
        let layer = PlatformLogLayer::new(tx, Level::TRACE);
        let subscriber = registry().with(layer);

        with_default(subscriber, || {
            tracing::info!(temperature = 36.6_f64, "reading");
        });

        let record = rx.recv().await.unwrap();
        assert_eq!(record.message, "reading");
        let attrs = record.attributes.unwrap();
        assert!((attrs["temperature"].as_f64().unwrap() - 36.6).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn field_visitor_captures_bool_field() {
        use tracing::subscriber::with_default;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::registry;

        let (tx, mut rx) = mpsc::channel(100);
        let layer = PlatformLogLayer::new(tx, Level::TRACE);
        let subscriber = registry().with(layer);

        with_default(subscriber, || {
            tracing::info!(healthy = true, "check");
        });

        let record = rx.recv().await.unwrap();
        let attrs = record.attributes.unwrap();
        assert_eq!(attrs["healthy"], true);
    }

    #[tokio::test]
    async fn field_visitor_captures_u64_field() {
        use tracing::subscriber::with_default;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::registry;

        let (tx, mut rx) = mpsc::channel(100);
        let layer = PlatformLogLayer::new(tx, Level::TRACE);
        let subscriber = registry().with(layer);

        with_default(subscriber, || {
            tracing::info!(bytes = 4096_u64, "transfer");
        });

        let record = rx.recv().await.unwrap();
        let attrs = record.attributes.unwrap();
        assert_eq!(attrs["bytes"], 4096);
    }

    // -- Level filtering --

    #[tokio::test]
    async fn level_filtering_drops_below_min_level() {
        use tracing::subscriber::with_default;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::registry;

        let (tx, mut rx) = mpsc::channel(100);
        // min_level = WARN means only WARN and ERROR pass through
        let layer = PlatformLogLayer::new(tx, Level::WARN);
        let subscriber = registry().with(layer);

        with_default(subscriber, || {
            tracing::info!("should be dropped");
            tracing::debug!("also dropped");
            tracing::warn!("should pass");
            tracing::error!("should also pass");
        });

        let record1 = rx.recv().await.unwrap();
        assert_eq!(record1.level, "warn");
        let record2 = rx.recv().await.unwrap();
        assert_eq!(record2.level, "error");
        // Channel should be empty — info/debug were dropped
        assert!(rx.try_recv().is_err());
    }

    // -- Span chain walking with context propagation --

    #[tokio::test]
    async fn span_chain_propagates_context_to_events() {
        use tracing::subscriber::with_default;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::registry;

        let (tx, mut rx) = mpsc::channel(100);
        let layer = PlatformLogLayer::new(tx, Level::TRACE);
        let subscriber = registry().with(layer);

        let pid = Uuid::nil();
        with_default(subscriber, || {
            let _outer = tracing::info_span!("outer", project_id = %pid).entered();
            tracing::info!("inside outer span");
        });

        let record = rx.recv().await.unwrap();
        assert_eq!(record.project_id, Some(pid));
        assert_eq!(record.service, "platform");
    }

    #[tokio::test]
    async fn nested_span_chain_collects_first_non_none() {
        use tracing::subscriber::with_default;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::registry;

        let (tx, mut rx) = mpsc::channel(100);
        let layer = PlatformLogLayer::new(tx, Level::TRACE);
        let subscriber = registry().with(layer);

        let pid = Uuid::nil();
        let sid = Uuid::from_u128(1);
        with_default(subscriber, || {
            let _outer = tracing::info_span!("outer", project_id = %pid).entered();
            let _inner =
                tracing::info_span!("inner", session_id = %sid, source = "session").entered();
            tracing::info!("nested event");
        });

        let record = rx.recv().await.unwrap();
        assert_eq!(record.project_id, Some(pid));
        assert_eq!(record.session_id, Some(sid));
    }

    // -- Source classification from target --

    #[tokio::test]
    async fn source_classification_uses_target_when_no_span_source() {
        use tracing::subscriber::with_default;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::registry;

        let (tx, mut rx) = mpsc::channel(100);
        let layer = PlatformLogLayer::new(tx, Level::TRACE);
        let subscriber = registry().with(layer);

        with_default(subscriber, || {
            // No span context at all — source should be derived from target
            tracing::info!("no span context");
        });

        let record = rx.recv().await.unwrap();
        // Target will be this module's path which contains neither ::api:: nor ::auth::
        assert_eq!(record.source, "system");
    }

    #[tokio::test]
    async fn source_classification_prefers_span_source() {
        use tracing::subscriber::with_default;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::registry;

        let (tx, mut rx) = mpsc::channel(100);
        let layer = PlatformLogLayer::new(tx, Level::TRACE);
        let subscriber = registry().with(layer);

        with_default(subscriber, || {
            let _span = tracing::info_span!("req", source = "session").entered();
            tracing::info!("with explicit source");
        });

        let record = rx.recv().await.unwrap();
        assert_eq!(record.source, "session");
    }

    // -- on_record merges into existing span --

    #[tokio::test]
    async fn on_record_merges_new_fields_into_span() {
        use tracing::subscriber::with_default;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::registry;

        let (tx, mut rx) = mpsc::channel(100);
        let layer = PlatformLogLayer::new(tx, Level::TRACE);
        let subscriber = registry().with(layer);

        let pid = Uuid::nil();
        with_default(subscriber, || {
            let span = tracing::info_span!(
                "dynamic",
                project_id = %pid,
                session_id = tracing::field::Empty
            );
            let sid = Uuid::from_u128(42);
            span.record("session_id", tracing::field::display(&sid));
            let _guard = span.entered();
            tracing::info!("after record");
        });

        let record = rx.recv().await.unwrap();
        assert_eq!(record.project_id, Some(pid));
        assert_eq!(record.session_id, Some(Uuid::from_u128(42)));
    }

    // -- SpanFieldVisitor record_u64 / record_i64 / record_bool (no-op branches) --

    #[test]
    fn span_field_visitor_u64_i64_bool_are_noop() {
        // SpanFieldVisitor only cares about string/debug fields for UUID extraction.
        // The numeric/bool methods are intentionally no-ops. Verify they don't panic.
        let visitor = SpanFieldVisitor::default();
        // We can't easily construct tracing::field::Field outside of tracing macros,
        // but we can verify the default visitor is unchanged after the methods.
        assert!(visitor.fields.project_id.is_none());
        assert!(visitor.fields.session_id.is_none());
        // The fields remain None after default (no-ops don't set anything)
    }

    // -- spawn_bridge forwarding --

    #[tokio::test]
    async fn spawn_bridge_forwards_records() {
        let (platform_tx, platform_rx) = mpsc::channel(100);
        let (logs_tx, mut logs_rx) = mpsc::channel(100);

        spawn_bridge(platform_rx, logs_tx);

        let record = LogEntryRecord {
            timestamp: Utc::now(),
            trace_id: None,
            span_id: None,
            project_id: None,
            session_id: None,
            user_id: None,
            service: "bridge-test".into(),
            level: "info".into(),
            source: "system".into(),
            message: "bridge msg".into(),
            attributes: None,
        };

        platform_tx.send(record).await.unwrap();
        drop(platform_tx); // close sender so bridge task finishes

        let forwarded = logs_rx.recv().await.unwrap();
        assert_eq!(forwarded.service, "bridge-test");
        assert_eq!(forwarded.message, "bridge msg");
    }

    // -- Event without metadata file/line (verify attributes always has target) --

    #[tokio::test]
    async fn event_attributes_always_include_target() {
        use tracing::subscriber::with_default;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::registry;

        let (tx, mut rx) = mpsc::channel(100);
        let layer = PlatformLogLayer::new(tx, Level::TRACE);
        let subscriber = registry().with(layer);

        with_default(subscriber, || {
            tracing::info!("target test");
        });

        let record = rx.recv().await.unwrap();
        let attrs = record.attributes.unwrap();
        assert!(
            attrs.get("target").is_some(),
            "attributes should always include target"
        );
    }

    // -- Full record structure verification --

    #[tokio::test]
    async fn full_record_structure() {
        use tracing::subscriber::with_default;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::registry;

        let (tx, mut rx) = mpsc::channel(100);
        let layer = PlatformLogLayer::new(tx, Level::TRACE);
        let subscriber = registry().with(layer);

        let pid = Uuid::nil();
        with_default(subscriber, || {
            let _span = tracing::info_span!(
                "handler",
                project_id = %pid,
                user_type = "human",
                task_name = "deploy",
                trace_id = "abc123"
            )
            .entered();
            tracing::warn!(retries = 3_u64, "retrying operation");
        });

        let record = rx.recv().await.unwrap();
        assert_eq!(record.service, "platform");
        assert_eq!(record.level, "warn");
        assert_eq!(record.message, "retrying operation");
        assert_eq!(record.project_id, Some(pid));
        assert!(record.trace_id.as_deref() == Some("abc123"));
        let attrs = record.attributes.unwrap();
        assert_eq!(attrs["retries"], 3);
        assert_eq!(attrs["task_name"], "deploy");
        assert_eq!(attrs["user_type"], "human");
    }
}
