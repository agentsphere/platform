use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use prost::Message;
use tokio::sync::mpsc;

use crate::auth::middleware::AuthUser;
use crate::error::ApiError;
use crate::store::AppState;

use super::correlation::{self, CorrelationEnvelope};
use super::proto;
use super::store::{LogEntryRecord, LogTailMessage, MetricRecord, SpanRecord};

/// Buffer capacity per signal type.
const BUFFER_CAPACITY: usize = 10_000;

// ---------------------------------------------------------------------------
// Ingest channels
// ---------------------------------------------------------------------------

/// Shared sender handles for the ingest buffer channels.
#[derive(Clone)]
#[allow(clippy::struct_field_names)]
pub struct IngestChannels {
    pub spans_tx: mpsc::Sender<SpanRecord>,
    pub logs_tx: mpsc::Sender<LogEntryRecord>,
    pub metrics_tx: mpsc::Sender<MetricRecord>,
}

/// Create ingest channels and return (senders, receivers).
pub fn create_channels() -> (
    IngestChannels,
    mpsc::Receiver<SpanRecord>,
    mpsc::Receiver<LogEntryRecord>,
    mpsc::Receiver<MetricRecord>,
) {
    let (spans_tx, spans_rx) = mpsc::channel(BUFFER_CAPACITY);
    let (logs_tx, logs_rx) = mpsc::channel(BUFFER_CAPACITY);
    let (metrics_tx, metrics_rx) = mpsc::channel(BUFFER_CAPACITY);
    (
        IngestChannels {
            spans_tx,
            logs_tx,
            metrics_tx,
        },
        spans_rx,
        logs_rx,
        metrics_rx,
    )
}

// ---------------------------------------------------------------------------
// OTLP ingest handlers
// ---------------------------------------------------------------------------

/// `POST /v1/traces` — receive OTLP trace protobuf.
#[tracing::instrument(skip(state, channels, body), err)]
pub async fn ingest_traces(
    State(state): State<AppState>,
    auth: AuthUser,
    axum::Extension(channels): axum::Extension<IngestChannels>,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    crate::auth::rate_limit::check_rate(&state.valkey, "otlp", &auth.user_id.to_string(), 1000, 60)
        .await?;

    let request = proto::ExportTraceServiceRequest::decode(body)
        .map_err(|e| ApiError::BadRequest(format!("invalid protobuf: {e}")))?;

    for rs in &request.resource_spans {
        let resource_attrs = rs.resource.as_ref().map_or(&[][..], |r| &r.attributes);
        for ss in &rs.scope_spans {
            for span in &ss.spans {
                let record = build_span_record(span, resource_attrs, &state).await;
                if channels.spans_tx.try_send(record).is_err() {
                    return Err(ApiError::ServiceUnavailable("ingest buffer full".into()));
                }
            }
        }
    }

    let response_bytes = proto::ExportTraceServiceResponse {}.encode_to_vec();
    Ok((
        StatusCode::OK,
        [("content-type", "application/x-protobuf")],
        response_bytes,
    ))
}

/// `POST /v1/logs` — receive OTLP log protobuf.
#[tracing::instrument(skip(state, channels, body), err)]
pub async fn ingest_logs(
    State(state): State<AppState>,
    auth: AuthUser,
    axum::Extension(channels): axum::Extension<IngestChannels>,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    crate::auth::rate_limit::check_rate(&state.valkey, "otlp", &auth.user_id.to_string(), 1000, 60)
        .await?;

    let request = proto::ExportLogsServiceRequest::decode(body)
        .map_err(|e| ApiError::BadRequest(format!("invalid protobuf: {e}")))?;

    for rl in &request.resource_logs {
        let resource_attrs = rl.resource.as_ref().map_or(&[][..], |r| &r.attributes);
        for sl in &rl.scope_logs {
            for log in &sl.log_records {
                let record = build_log_record(log, resource_attrs, &state).await;
                if channels.logs_tx.try_send(record).is_err() {
                    return Err(ApiError::ServiceUnavailable("ingest buffer full".into()));
                }
            }
        }
    }

    let response_bytes = proto::ExportLogsServiceResponse {}.encode_to_vec();
    Ok((
        StatusCode::OK,
        [("content-type", "application/x-protobuf")],
        response_bytes,
    ))
}

/// `POST /v1/metrics` — receive OTLP metric protobuf.
#[tracing::instrument(skip(state, channels, body), err)]
pub async fn ingest_metrics(
    State(state): State<AppState>,
    auth: AuthUser,
    axum::Extension(channels): axum::Extension<IngestChannels>,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    crate::auth::rate_limit::check_rate(&state.valkey, "otlp", &auth.user_id.to_string(), 1000, 60)
        .await?;

    let request = proto::ExportMetricsServiceRequest::decode(body)
        .map_err(|e| ApiError::BadRequest(format!("invalid protobuf: {e}")))?;

    for rm in &request.resource_metrics {
        let resource_attrs = rm.resource.as_ref().map_or(&[][..], |r| &r.attributes);
        for sm in &rm.scope_metrics {
            for metric in &sm.metrics {
                let records = build_metric_records(metric, resource_attrs, &state).await;
                for record in records {
                    if channels.metrics_tx.try_send(record).is_err() {
                        return Err(ApiError::ServiceUnavailable("ingest buffer full".into()));
                    }
                }
            }
        }
    }

    let response_bytes = proto::ExportMetricsServiceResponse {}.encode_to_vec();
    Ok((
        StatusCode::OK,
        [("content-type", "application/x-protobuf")],
        response_bytes,
    ))
}

// ---------------------------------------------------------------------------
// Record conversion helpers
// ---------------------------------------------------------------------------

async fn build_span_record(
    span: &proto::Span,
    resource_attrs: &[proto::KeyValue],
    state: &AppState,
) -> SpanRecord {
    let mut env = correlation::extract_correlation(resource_attrs, &span.attributes);
    env.trace_id = Some(proto::trace_id_to_hex(&span.trace_id));
    env.span_id = Some(proto::span_id_to_hex(&span.span_id));
    let _ = correlation::resolve_session(&state.pool, &mut env).await;

    let started_at = proto::nanos_to_datetime(span.start_time_unix_nano);
    let finished_at = if span.end_time_unix_nano > 0 {
        Some(proto::nanos_to_datetime(span.end_time_unix_nano))
    } else {
        None
    };
    let duration_ms =
        finished_at.map(|f| i32::try_from((f - started_at).num_milliseconds()).unwrap_or(i32::MAX));

    let status_code = span.status.as_ref().map_or(0, |s| s.code);

    let parent = if span.parent_span_id.is_empty() {
        None
    } else {
        Some(proto::span_id_to_hex(&span.parent_span_id))
    };

    SpanRecord {
        trace_id: env.trace_id.unwrap_or_default(),
        span_id: env.span_id.unwrap_or_default(),
        parent_span_id: parent,
        name: span.name.clone(),
        service: env.service,
        kind: proto::span_kind_to_str(span.kind).into(),
        status: proto::status_code_to_str(status_code).into(),
        attributes: json_opt(&span.attributes),
        events: events_to_json(&span.events),
        duration_ms,
        started_at,
        finished_at,
        project_id: env.project_id,
        session_id: env.session_id,
        user_id: env.user_id,
    }
}

async fn build_log_record(
    log: &proto::LogRecord,
    resource_attrs: &[proto::KeyValue],
    state: &AppState,
) -> LogEntryRecord {
    let mut env = correlation::extract_correlation(resource_attrs, &log.attributes);
    if !log.trace_id.is_empty() {
        env.trace_id = Some(proto::trace_id_to_hex(&log.trace_id));
    }
    if !log.span_id.is_empty() {
        env.span_id = Some(proto::span_id_to_hex(&log.span_id));
    }
    let _ = correlation::resolve_session(&state.pool, &mut env).await;

    let timestamp = if log.time_unix_nano > 0 {
        proto::nanos_to_datetime(log.time_unix_nano)
    } else {
        chrono::Utc::now()
    };

    let level = if log.severity_text.is_empty() {
        proto::severity_to_level(log.severity_number).into()
    } else {
        log.severity_text.to_lowercase()
    };

    let message = log
        .body
        .as_ref()
        .map(|v| match &v.value {
            Some(proto::any_value::Value::StringValue(s)) => s.clone(),
            Some(other) => {
                let json = proto::any_value_to_json(&proto::AnyValue {
                    value: Some(other.clone()),
                });
                json.to_string()
            }
            None => String::new(),
        })
        .unwrap_or_default();

    LogEntryRecord {
        timestamp,
        trace_id: env.trace_id,
        span_id: env.span_id,
        project_id: env.project_id,
        session_id: env.session_id,
        user_id: env.user_id,
        service: env.service,
        level,
        message,
        attributes: json_opt(&log.attributes),
    }
}

async fn build_metric_records(
    metric: &proto::Metric,
    resource_attrs: &[proto::KeyValue],
    state: &AppState,
) -> Vec<MetricRecord> {
    let mut records = Vec::new();
    let env = build_metric_envelope(resource_attrs, state).await;
    let unit = if metric.unit.is_empty() {
        None
    } else {
        Some(metric.unit.clone())
    };

    match &metric.data {
        Some(proto::metric_data::Data::Gauge(g)) => {
            for dp in &g.data_points {
                if let Some(rec) =
                    number_point_to_record(dp, &metric.name, "gauge", unit.as_deref(), &env)
                {
                    records.push(rec);
                }
            }
        }
        Some(proto::metric_data::Data::Sum(s)) => {
            let mtype = if s.is_monotonic { "counter" } else { "gauge" };
            for dp in &s.data_points {
                if let Some(rec) =
                    number_point_to_record(dp, &metric.name, mtype, unit.as_deref(), &env)
                {
                    records.push(rec);
                }
            }
        }
        Some(proto::metric_data::Data::Histogram(h)) => {
            for dp in &h.data_points {
                if let Some(sum) = dp.sum {
                    let labels = proto::attrs_to_json(&dp.attributes);
                    records.push(MetricRecord {
                        name: metric.name.clone(),
                        labels,
                        metric_type: "histogram".into(),
                        unit: unit.clone(),
                        project_id: env.project_id,
                        timestamp: proto::nanos_to_datetime(dp.time_unix_nano),
                        value: sum,
                    });
                }
            }
        }
        None => {}
    }

    records
}

async fn build_metric_envelope(
    resource_attrs: &[proto::KeyValue],
    state: &AppState,
) -> CorrelationEnvelope {
    let mut env = correlation::extract_correlation(resource_attrs, &[]);
    let _ = correlation::resolve_session(&state.pool, &mut env).await;
    env
}

fn number_point_to_record(
    dp: &proto::NumberDataPoint,
    name: &str,
    metric_type: &str,
    unit: Option<&str>,
    env: &CorrelationEnvelope,
) -> Option<MetricRecord> {
    let value = match &dp.value {
        Some(proto::number_data_point::Value::AsDouble(d)) => *d,
        #[allow(clippy::cast_precision_loss)]
        Some(proto::number_data_point::Value::AsInt(i)) => *i as f64,
        None => return None,
    };
    let labels = proto::attrs_to_json(&dp.attributes);
    Some(MetricRecord {
        name: name.into(),
        labels,
        metric_type: metric_type.into(),
        unit: unit.map(String::from),
        project_id: env.project_id,
        timestamp: proto::nanos_to_datetime(dp.time_unix_nano),
        value,
    })
}

fn json_opt(attrs: &[proto::KeyValue]) -> Option<serde_json::Value> {
    let v = proto::attrs_to_json(attrs);
    if v.is_null() { None } else { Some(v) }
}

fn events_to_json(events: &[proto::SpanEvent]) -> Option<serde_json::Value> {
    if events.is_empty() {
        return None;
    }
    let arr: Vec<serde_json::Value> = events
        .iter()
        .map(|e| {
            serde_json::json!({
                "time": proto::nanos_to_datetime(e.time_unix_nano).to_rfc3339(),
                "name": e.name,
                "attributes": proto::attrs_to_json(&e.attributes),
            })
        })
        .collect();
    Some(serde_json::Value::Array(arr))
}

// ---------------------------------------------------------------------------
// Background flush tasks
// ---------------------------------------------------------------------------

/// Drain the spans channel and batch-write to Postgres.
pub async fn flush_spans(
    pool: sqlx::PgPool,
    mut rx: mpsc::Receiver<SpanRecord>,
    mut shutdown: tokio::sync::watch::Receiver<()>,
) {
    let mut buffer = Vec::with_capacity(128);
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                drain_spans(&pool, &mut rx, &mut buffer).await;
                break;
            }
            _ = interval.tick() => {
                drain_spans(&pool, &mut rx, &mut buffer).await;
            }
        }
    }
}

async fn drain_spans(
    pool: &sqlx::PgPool,
    rx: &mut mpsc::Receiver<SpanRecord>,
    buffer: &mut Vec<SpanRecord>,
) {
    while buffer.len() < 500 {
        match rx.try_recv() {
            Ok(record) => buffer.push(record),
            Err(_) => break,
        }
    }
    if !buffer.is_empty() {
        if let Err(e) = super::store::write_spans(pool, buffer).await {
            tracing::error!(error = %e, count = buffer.len(), "failed to flush spans");
        }
        buffer.clear();
    }
}

/// Drain the logs channel, batch-write, and publish to Valkey for live tail.
pub async fn flush_logs(
    pool: sqlx::PgPool,
    valkey: fred::clients::Pool,
    mut rx: mpsc::Receiver<LogEntryRecord>,
    mut shutdown: tokio::sync::watch::Receiver<()>,
) {
    let mut buffer = Vec::with_capacity(128);
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                drain_and_publish_logs(&pool, &valkey, &mut rx, &mut buffer).await;
                break;
            }
            _ = interval.tick() => {
                drain_and_publish_logs(&pool, &valkey, &mut rx, &mut buffer).await;
            }
        }
    }
}

async fn drain_and_publish_logs(
    pool: &sqlx::PgPool,
    valkey: &fred::clients::Pool,
    rx: &mut mpsc::Receiver<LogEntryRecord>,
    buffer: &mut Vec<LogEntryRecord>,
) {
    while buffer.len() < 500 {
        match rx.try_recv() {
            Ok(record) => buffer.push(record),
            Err(_) => break,
        }
    }
    if buffer.is_empty() {
        return;
    }

    // Publish to Valkey for live tail before writing to DB
    for log in buffer.iter() {
        if let Some(pid) = log.project_id {
            let msg = LogTailMessage {
                timestamp: log.timestamp,
                service: log.service.clone(),
                level: log.level.clone(),
                message: log.message.clone(),
                trace_id: log.trace_id.clone(),
            };
            if let Ok(json) = serde_json::to_string(&msg) {
                let channel = format!("logs:{pid}");
                let _ = crate::store::valkey::publish(valkey, &channel, &json).await;
            }
        }
    }

    if let Err(e) = super::store::write_logs(pool, buffer).await {
        tracing::error!(error = %e, count = buffer.len(), "failed to flush logs");
    }
    buffer.clear();
}

/// Drain the metrics channel and batch-write to Postgres.
pub async fn flush_metrics(
    pool: sqlx::PgPool,
    mut rx: mpsc::Receiver<MetricRecord>,
    mut shutdown: tokio::sync::watch::Receiver<()>,
) {
    let mut buffer = Vec::with_capacity(128);
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                drain_metrics(&pool, &mut rx, &mut buffer).await;
                break;
            }
            _ = interval.tick() => {
                drain_metrics(&pool, &mut rx, &mut buffer).await;
            }
        }
    }
}

async fn drain_metrics(
    pool: &sqlx::PgPool,
    rx: &mut mpsc::Receiver<MetricRecord>,
    buffer: &mut Vec<MetricRecord>,
) {
    while buffer.len() < 500 {
        match rx.try_recv() {
            Ok(record) => buffer.push(record),
            Err(_) => break,
        }
    }
    if !buffer.is_empty() {
        if let Err(e) = super::store::write_metrics(pool, buffer).await {
            tracing::error!(error = %e, count = buffer.len(), "failed to flush metrics");
        }
        buffer.clear();
    }
}
