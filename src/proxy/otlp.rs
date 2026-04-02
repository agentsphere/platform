//! OTLP protobuf serialization and batch HTTP export.

use std::time::Duration;

use chrono::{DateTime, Utc};
use prost::Message;
use tokio::sync::{mpsc, watch};

use crate::observe::proto;

use super::logs::LogRecord;
use super::metrics::MetricRecord;
use super::traces::{SpanKind, SpanRecord};

/// Batched OTLP exporter. Collects spans, logs, metrics and flushes
/// to the platform's `/v1/{traces,logs,metrics}` endpoints.
pub struct OtlpExporter {
    client: reqwest::Client,
    endpoint: String,
    token: String,
    project_id: Option<String>,
    service_name: String,
    session_id: Option<String>,
}

impl OtlpExporter {
    /// Create a new OTLP exporter.
    pub fn new(
        endpoint: String,
        token: String,
        project_id: Option<String>,
        service_name: String,
        session_id: Option<String>,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();

        Self {
            client,
            endpoint,
            token,
            project_id,
            service_name,
            session_id,
        }
    }

    /// Build the resource attributes common to all signals.
    fn resource_attrs(&self) -> Vec<proto::KeyValue> {
        let mut attrs = vec![proto::KeyValue {
            key: "service.name".into(),
            value: Some(proto::AnyValue {
                value: Some(proto::any_value::Value::StringValue(
                    self.service_name.clone(),
                )),
            }),
        }];

        if let Some(ref pid) = self.project_id {
            attrs.push(proto::KeyValue {
                key: "platform.project_id".into(),
                value: Some(proto::AnyValue {
                    value: Some(proto::any_value::Value::StringValue(pid.clone())),
                }),
            });
        }
        if let Some(ref sid) = self.session_id {
            attrs.push(proto::KeyValue {
                key: "platform.session_id".into(),
                value: Some(proto::AnyValue {
                    value: Some(proto::any_value::Value::StringValue(sid.clone())),
                }),
            });
        }

        attrs
    }

    /// Flush a batch of spans as `ExportTraceServiceRequest` protobuf.
    #[tracing::instrument(skip(self, spans), fields(count = spans.len()))]
    pub async fn flush_spans(&self, spans: Vec<SpanRecord>) -> anyhow::Result<()> {
        if spans.is_empty() {
            return Ok(());
        }

        let proto_spans: Vec<proto::Span> = spans.iter().map(span_to_proto).collect();

        let request = proto::ExportTraceServiceRequest {
            resource_spans: vec![proto::ResourceSpans {
                resource: Some(proto::Resource {
                    attributes: self.resource_attrs(),
                }),
                scope_spans: vec![proto::ScopeSpans {
                    scope: Some(proto::InstrumentationScope {
                        name: "platform-proxy".into(),
                        version: env!("CARGO_PKG_VERSION").into(),
                    }),
                    spans: proto_spans,
                }],
            }],
        };

        let body = request.encode_to_vec();
        self.post("/v1/traces", body).await
    }

    /// Flush a batch of logs as `ExportLogsServiceRequest` protobuf.
    #[tracing::instrument(skip(self, logs), fields(count = logs.len()))]
    pub async fn flush_logs(&self, logs: Vec<LogRecord>) -> anyhow::Result<()> {
        if logs.is_empty() {
            return Ok(());
        }

        let proto_logs: Vec<proto::LogRecord> = logs.iter().map(log_to_proto).collect();

        let request = proto::ExportLogsServiceRequest {
            resource_logs: vec![proto::ResourceLogs {
                resource: Some(proto::Resource {
                    attributes: self.resource_attrs(),
                }),
                scope_logs: vec![proto::ScopeLogs {
                    scope: Some(proto::InstrumentationScope {
                        name: "platform-proxy".into(),
                        version: env!("CARGO_PKG_VERSION").into(),
                    }),
                    log_records: proto_logs,
                }],
            }],
        };

        let body = request.encode_to_vec();
        self.post("/v1/logs", body).await
    }

    /// Flush metrics as `ExportMetricsServiceRequest` protobuf.
    #[tracing::instrument(skip(self, metrics), fields(count = metrics.len()))]
    pub async fn flush_metrics(&self, metrics: Vec<MetricRecord>) -> anyhow::Result<()> {
        if metrics.is_empty() {
            return Ok(());
        }

        let proto_metrics: Vec<proto::Metric> = metrics.iter().map(metric_to_proto).collect();

        let request = proto::ExportMetricsServiceRequest {
            resource_metrics: vec![proto::ResourceMetrics {
                resource: Some(proto::Resource {
                    attributes: self.resource_attrs(),
                }),
                scope_metrics: vec![proto::ScopeMetrics {
                    scope: Some(proto::InstrumentationScope {
                        name: "platform-proxy".into(),
                        version: env!("CARGO_PKG_VERSION").into(),
                    }),
                    metrics: proto_metrics,
                }],
            }],
        };

        let body = request.encode_to_vec();
        self.post("/v1/metrics", body).await
    }

    /// POST protobuf body to the platform endpoint.
    async fn post(&self, path: &str, body: Vec<u8>) -> anyhow::Result<()> {
        let url = format!("{}{path}", self.endpoint);
        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/x-protobuf")
            .header("Authorization", format!("Bearer {}", self.token))
            .body(body)
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => Ok(()),
            Ok(r) => {
                tracing::warn!(
                    status = r.status().as_u16(),
                    path,
                    "OTLP export got non-success response"
                );
                Ok(()) // Don't crash on export failure
            }
            Err(e) => {
                tracing::warn!(error = %e, path, "OTLP export failed");
                Ok(()) // Don't crash on export failure
            }
        }
    }
}

/// Convert a `SpanRecord` to a protobuf `Span`.
fn span_to_proto(span: &SpanRecord) -> proto::Span {
    let trace_id = hex::decode(&span.trace_id).unwrap_or_else(|_| vec![0; 16]);
    let span_id = hex::decode(&span.span_id).unwrap_or_else(|_| vec![0; 8]);
    let parent_span_id = span
        .parent_span_id
        .as_ref()
        .and_then(|s| hex::decode(s).ok())
        .unwrap_or_default();

    let start_ns = datetime_to_nanos(span.started_at);
    let end_ns = start_ns + u64::try_from(span.duration_ms).unwrap_or(0) * 1_000_000;

    let kind = match span.kind {
        SpanKind::Server => proto::SpanKind::Server as i32,
        SpanKind::Client => proto::SpanKind::Client as i32,
        SpanKind::Internal => proto::SpanKind::Internal as i32,
    };

    let (status_code, status_message) = if span.status == "error" {
        (proto::StatusCode::Error as i32, span.status.clone())
    } else {
        (proto::StatusCode::Ok as i32, String::new())
    };

    let mut attributes = Vec::new();
    if let Some(ref attrs) = span.attributes
        && let Some(map) = attrs.as_object()
    {
        for (k, v) in map {
            attributes.push(proto::KeyValue {
                key: k.clone(),
                value: Some(json_to_any_value(v)),
            });
        }
    }

    proto::Span {
        trace_id,
        span_id,
        parent_span_id,
        name: span.name.clone(),
        kind,
        start_time_unix_nano: start_ns,
        end_time_unix_nano: end_ns,
        attributes,
        events: vec![],
        status: Some(proto::SpanStatus {
            code: status_code,
            message: status_message,
        }),
    }
}

/// Convert a `LogRecord` to a protobuf `LogRecord`.
fn log_to_proto(log: &LogRecord) -> proto::LogRecord {
    let severity_number = match log.level.as_str() {
        "TRACE" => proto::SeverityNumber::Trace as i32,
        "DEBUG" => proto::SeverityNumber::Debug as i32,
        "WARN" => proto::SeverityNumber::Warn as i32,
        "ERROR" => proto::SeverityNumber::Error as i32,
        // "INFO" and anything else default to Info
        _ => proto::SeverityNumber::Info as i32,
    };

    let trace_id = log
        .trace_id
        .as_ref()
        .and_then(|s| hex::decode(s).ok())
        .unwrap_or_default();
    let span_id = log
        .span_id
        .as_ref()
        .and_then(|s| hex::decode(s).ok())
        .unwrap_or_default();

    let mut attributes = Vec::new();
    if log.is_stderr {
        attributes.push(proto::KeyValue {
            key: "log.source".into(),
            value: Some(proto::AnyValue {
                value: Some(proto::any_value::Value::StringValue("stderr".into())),
            }),
        });
    }
    if let Some(ref attrs) = log.attributes
        && let Some(map) = attrs.as_object()
    {
        for (k, v) in map {
            attributes.push(proto::KeyValue {
                key: k.clone(),
                value: Some(json_to_any_value(v)),
            });
        }
    }

    proto::LogRecord {
        time_unix_nano: datetime_to_nanos(log.timestamp),
        severity_number,
        severity_text: log.level.clone(),
        body: Some(proto::AnyValue {
            value: Some(proto::any_value::Value::StringValue(log.message.clone())),
        }),
        attributes,
        trace_id,
        span_id,
    }
}

/// Convert a `MetricRecord` to a protobuf `Metric`.
fn metric_to_proto(metric: &MetricRecord) -> proto::Metric {
    let mut attrs = Vec::new();
    if let Some(map) = metric.labels.as_object() {
        for (k, v) in map {
            attrs.push(proto::KeyValue {
                key: k.clone(),
                value: Some(json_to_any_value(v)),
            });
        }
    }

    let time_ns = datetime_to_nanos(metric.timestamp);

    let data_point = proto::NumberDataPoint {
        attributes: attrs,
        time_unix_nano: time_ns,
        value: Some(proto::number_data_point::Value::AsDouble(metric.value)),
    };

    let data = match metric.metric_type.as_str() {
        "sum" => Some(proto::metric_data::Data::Sum(proto::Sum {
            data_points: vec![data_point],
            is_monotonic: true,
        })),
        _ => Some(proto::metric_data::Data::Gauge(proto::Gauge {
            data_points: vec![data_point],
        })),
    };

    proto::Metric {
        name: metric.name.clone(),
        description: String::new(),
        unit: metric.unit.clone().unwrap_or_default(),
        data,
    }
}

/// Convert a `serde_json::Value` to an OTLP `AnyValue`.
fn json_to_any_value(v: &serde_json::Value) -> proto::AnyValue {
    match v {
        serde_json::Value::String(s) => proto::AnyValue {
            value: Some(proto::any_value::Value::StringValue(s.clone())),
        },
        serde_json::Value::Bool(b) => proto::AnyValue {
            value: Some(proto::any_value::Value::BoolValue(*b)),
        },
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                proto::AnyValue {
                    value: Some(proto::any_value::Value::IntValue(i)),
                }
            } else {
                proto::AnyValue {
                    value: Some(proto::any_value::Value::DoubleValue(
                        n.as_f64().unwrap_or(0.0),
                    )),
                }
            }
        }
        _ => proto::AnyValue {
            value: Some(proto::any_value::Value::StringValue(v.to_string())),
        },
    }
}

/// Convert a `DateTime<Utc>` to nanoseconds since epoch.
fn datetime_to_nanos(dt: DateTime<Utc>) -> u64 {
    u64::try_from(dt.timestamp_nanos_opt().unwrap_or(0)).unwrap_or(0)
}

/// Background flush loop. Collects from mpsc channels, flushes every
/// `flush_interval` or when batch reaches `batch_size`.
#[tracing::instrument(skip_all)]
pub async fn run_exporter(
    exporter: OtlpExporter,
    mut span_rx: mpsc::Receiver<SpanRecord>,
    mut log_rx: mpsc::Receiver<LogRecord>,
    mut metric_rx: mpsc::Receiver<MetricRecord>,
    flush_interval: Duration,
    batch_size: usize,
    mut shutdown: watch::Receiver<()>,
) {
    let mut span_buf: Vec<SpanRecord> = Vec::with_capacity(batch_size);
    let mut log_buf: Vec<LogRecord> = Vec::with_capacity(batch_size);
    let mut metric_buf: Vec<MetricRecord> = Vec::with_capacity(batch_size);

    let mut ticker = tokio::time::interval(flush_interval);

    loop {
        tokio::select! {
            span = span_rx.recv() => {
                if let Some(s) = span {
                    span_buf.push(s);
                    if span_buf.len() >= batch_size {
                        let batch = std::mem::replace(&mut span_buf, Vec::with_capacity(batch_size));
                        let _ = exporter.flush_spans(batch).await;
                    }
                }
            }
            log = log_rx.recv() => {
                if let Some(l) = log {
                    log_buf.push(l);
                    if log_buf.len() >= batch_size {
                        let batch = std::mem::replace(&mut log_buf, Vec::with_capacity(batch_size));
                        let _ = exporter.flush_logs(batch).await;
                    }
                }
            }
            metric = metric_rx.recv() => {
                if let Some(m) = metric {
                    metric_buf.push(m);
                    if metric_buf.len() >= batch_size {
                        let batch = std::mem::replace(&mut metric_buf, Vec::with_capacity(batch_size));
                        let _ = exporter.flush_metrics(batch).await;
                    }
                }
            }
            _ = ticker.tick() => {
                if !span_buf.is_empty() {
                    let batch = std::mem::replace(&mut span_buf, Vec::with_capacity(batch_size));
                    let _ = exporter.flush_spans(batch).await;
                }
                if !log_buf.is_empty() {
                    let batch = std::mem::replace(&mut log_buf, Vec::with_capacity(batch_size));
                    let _ = exporter.flush_logs(batch).await;
                }
                if !metric_buf.is_empty() {
                    let batch = std::mem::replace(&mut metric_buf, Vec::with_capacity(batch_size));
                    let _ = exporter.flush_metrics(batch).await;
                }
            }
            _ = shutdown.changed() => {
                // Final drain
                if !span_buf.is_empty() {
                    let _ = exporter.flush_spans(span_buf).await;
                }
                if !log_buf.is_empty() {
                    let _ = exporter.flush_logs(log_buf).await;
                }
                if !metric_buf.is_empty() {
                    let _ = exporter.flush_metrics(metric_buf).await;
                }
                break;
            }
        }
    }

    tracing::debug!("OTLP exporter exiting");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_to_proto_basic() {
        let span = SpanRecord {
            trace_id: "4bf92f3577b34da6a3ce929d0e0e4736".into(),
            span_id: "00f067aa0ba902b7".into(),
            parent_span_id: None,
            name: "POST /api/test".into(),
            service: "test-svc".into(),
            kind: SpanKind::Server,
            status: "ok".into(),
            attributes: Some(serde_json::json!({"http.status_code": 200})),
            started_at: Utc::now(),
            duration_ms: 42,
            http_status_code: Some(200),
        };

        let proto = span_to_proto(&span);
        assert_eq!(proto.name, "POST /api/test");
        assert_eq!(proto.kind, proto::SpanKind::Server as i32);
        assert_eq!(proto.trace_id.len(), 16);
        assert_eq!(proto.span_id.len(), 8);
    }

    #[test]
    fn span_to_proto_error_status() {
        let span = SpanRecord {
            trace_id: "4bf92f3577b34da6a3ce929d0e0e4736".into(),
            span_id: "00f067aa0ba902b7".into(),
            parent_span_id: None,
            name: "GET /error".into(),
            service: "svc".into(),
            kind: SpanKind::Server,
            status: "error".into(),
            attributes: None,
            started_at: Utc::now(),
            duration_ms: 100,
            http_status_code: Some(500),
        };

        let proto = span_to_proto(&span);
        let status = proto.status.unwrap();
        assert_eq!(status.code, proto::StatusCode::Error as i32);
    }

    #[test]
    fn log_to_proto_basic() {
        let log = LogRecord {
            timestamp: Utc::now(),
            level: "INFO".into(),
            message: "test log".into(),
            trace_id: Some("4bf92f3577b34da6a3ce929d0e0e4736".into()),
            span_id: None,
            is_stderr: false,
            attributes: None,
        };

        let proto = log_to_proto(&log);
        assert_eq!(proto.severity_text, "INFO");
        assert_eq!(proto.severity_number, proto::SeverityNumber::Info as i32);
        assert_eq!(proto.trace_id.len(), 16);
    }

    #[test]
    fn log_to_proto_stderr_attribute() {
        let log = LogRecord {
            timestamp: Utc::now(),
            level: "WARN".into(),
            message: "warning".into(),
            trace_id: None,
            span_id: None,
            is_stderr: true,
            attributes: None,
        };

        let proto = log_to_proto(&log);
        let has_stderr = proto.attributes.iter().any(|kv| kv.key == "log.source");
        assert!(has_stderr);
    }

    #[test]
    fn metric_to_proto_gauge() {
        let metric = MetricRecord {
            name: "process.memory.rss".into(),
            labels: serde_json::json!({"service": "test"}),
            metric_type: "gauge".into(),
            unit: Some("bytes".into()),
            timestamp: Utc::now(),
            value: 1024.0,
        };

        let proto = metric_to_proto(&metric);
        assert_eq!(proto.name, "process.memory.rss");
        assert!(matches!(
            proto.data,
            Some(proto::metric_data::Data::Gauge(_))
        ));
    }

    #[test]
    fn metric_to_proto_sum() {
        let metric = MetricRecord {
            name: "http.server.request.count".into(),
            labels: serde_json::json!({}),
            metric_type: "sum".into(),
            unit: Some("{request}".into()),
            timestamp: Utc::now(),
            value: 42.0,
        };

        let proto = metric_to_proto(&metric);
        assert!(matches!(proto.data, Some(proto::metric_data::Data::Sum(_))));
    }

    #[test]
    fn exporter_creation() {
        let exporter = OtlpExporter::new(
            "http://localhost:8080".into(),
            "test-token".into(),
            Some("project-id".into()),
            "test-svc".into(),
            None,
        );
        let attrs = exporter.resource_attrs();
        assert!(attrs.iter().any(|kv| kv.key == "service.name"));
        assert!(attrs.iter().any(|kv| kv.key == "platform.project_id"));
    }

    #[test]
    fn datetime_to_nanos_positive() {
        let dt = Utc::now();
        let ns = datetime_to_nanos(dt);
        assert!(ns > 0);
    }
}
