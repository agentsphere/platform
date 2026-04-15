// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! OTLP ingest pipeline: channels, record builders, flush loops, and helpers.
//!
//! HTTP handler wiring (axum extractors, `AppState`) stays in the main binary.
//! This module provides the core logic callable from any binary.

use std::collections::HashSet;
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Bytes;
use axum::http::HeaderMap;
use sqlx::PgPool;
use tokio::sync::mpsc;
use uuid::Uuid;

use platform_types::{ApiError, AuthUser, Permission, PermissionChecker};

use crate::correlation::{self, CorrelationEnvelope};
use crate::proto;
use crate::types::{LogEntryRecord, LogTailMessage, MetricRecord, SpanRecord};

/// Buffer capacity per signal type.
const BUFFER_CAPACITY: usize = 10_000;

/// Dropped-record counter for rate-limited logging of buffer-full events.
static BUFFER_FULL_DROPS: AtomicU64 = AtomicU64::new(0);
/// Epoch-second of the last buffer-full warning log.
static BUFFER_FULL_LAST_LOG: AtomicU64 = AtomicU64::new(0);

/// Log a buffer-full warning at most once per 30 seconds to avoid log spam.
fn warn_buffer_full(signal: &str) {
    let dropped = BUFFER_FULL_DROPS.fetch_add(1, Ordering::Relaxed) + 1;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let last = BUFFER_FULL_LAST_LOG.load(Ordering::Relaxed);
    if now.saturating_sub(last) >= 30
        && BUFFER_FULL_LAST_LOG
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        tracing::warn!(signal, dropped, "ingest buffer full, dropping records");
    }
}

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
/// `buffer_capacity` overrides the default `BUFFER_CAPACITY` when non-zero.
pub fn create_channels_with_capacity(
    buffer_capacity: usize,
) -> (
    IngestChannels,
    mpsc::Receiver<SpanRecord>,
    mpsc::Receiver<LogEntryRecord>,
    mpsc::Receiver<MetricRecord>,
) {
    let cap = if buffer_capacity > 0 {
        buffer_capacity
    } else {
        BUFFER_CAPACITY
    };
    let (spans_tx, spans_rx) = mpsc::channel(cap);
    let (logs_tx, logs_rx) = mpsc::channel(cap);
    let (metrics_tx, metrics_rx) = mpsc::channel(cap);
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

/// Create ingest channels with default buffer capacity (used in tests).
#[cfg(test)]
pub fn create_channels() -> (
    IngestChannels,
    mpsc::Receiver<SpanRecord>,
    mpsc::Receiver<LogEntryRecord>,
    mpsc::Receiver<MetricRecord>,
) {
    create_channels_with_capacity(BUFFER_CAPACITY)
}

// ---------------------------------------------------------------------------
// Gzip decompression
// ---------------------------------------------------------------------------

/// Decompress the request body if `Content-Encoding: gzip` is set.
pub fn maybe_decompress(headers: &HeaderMap, body: Bytes) -> Result<Bytes, ApiError> {
    let is_gzip = headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("gzip"));

    if !is_gzip {
        return Ok(body);
    }

    let mut decoder = flate2::read::GzDecoder::new(&body[..]);
    let mut decompressed = Vec::with_capacity(body.len() * 2);
    decoder
        .read_to_end(&mut decompressed)
        .map_err(|e| ApiError::BadRequest(format!("gzip decompression failed: {e}")))?;
    Ok(Bytes::from(decompressed))
}

// ---------------------------------------------------------------------------
// Per-project OTLP auth
// ---------------------------------------------------------------------------

/// Check that the authenticated user has `ObserveWrite` permission for every
/// `project_id` present in the OTLP payload.
pub async fn check_otlp_project_auth(
    auth: &AuthUser,
    checker: &impl PermissionChecker,
    resource_attrs_list: &[&[proto::KeyValue]],
) -> Result<(), ApiError> {
    let mut project_ids = HashSet::new();
    let mut has_system_metrics = false;

    for attrs in resource_attrs_list {
        match proto::get_string_attr(attrs, "platform.project_id") {
            Some(pid_str) => {
                let pid = Uuid::parse_str(&pid_str).map_err(|_| {
                    ApiError::BadRequest(format!(
                        "invalid platform.project_id: '{pid_str}' is not a valid UUID"
                    ))
                })?;
                project_ids.insert(pid);
            }
            None => {
                has_system_metrics = true;
            }
        }
    }

    // System-level metrics (no project_id) require observe:write at global scope
    if has_system_metrics {
        let allowed = checker
            .has_permission(auth.user_id, None, Permission::ObserveWrite)
            .await
            .map_err(|e| ApiError::Internal(e.context("OTLP system auth check")))?;

        if !allowed {
            return Err(ApiError::Forbidden);
        }
    }

    for pid in &project_ids {
        auth.check_project_scope(*pid)?;

        let allowed = checker
            .has_permission_scoped(
                auth.user_id,
                Some(*pid),
                Permission::ObserveWrite,
                auth.token_scopes.as_deref(),
            )
            .await
            .map_err(|e| ApiError::Internal(e.context("OTLP project auth check")))?;

        if !allowed {
            return Err(ApiError::NotFound("project".into()));
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Record conversion helpers
// ---------------------------------------------------------------------------

/// Build a span record from an OTLP span + resource attributes.
pub async fn build_span_record(
    span: &proto::Span,
    resource_attrs: &[proto::KeyValue],
    pool: &PgPool,
) -> SpanRecord {
    let mut env = correlation::extract_correlation(resource_attrs, &span.attributes);
    env.trace_id = Some(proto::trace_id_to_hex(&span.trace_id));
    env.span_id = Some(proto::span_id_to_hex(&span.span_id));
    let _ = correlation::resolve_session(pool, &mut env).await;

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

/// Build a log entry record from an OTLP log record + resource attributes.
pub async fn build_log_record(
    log: &proto::LogRecord,
    resource_attrs: &[proto::KeyValue],
    pool: &PgPool,
) -> LogEntryRecord {
    let mut env = correlation::extract_correlation(resource_attrs, &log.attributes);
    if !log.trace_id.is_empty() {
        env.trace_id = Some(proto::trace_id_to_hex(&log.trace_id));
    }
    if !log.span_id.is_empty() {
        env.span_id = Some(proto::span_id_to_hex(&log.span_id));
    }
    let _ = correlation::resolve_session(pool, &mut env).await;

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

    let source = if env.session_id.is_some() {
        "session".into()
    } else {
        "external".into()
    };

    LogEntryRecord {
        timestamp,
        trace_id: env.trace_id,
        span_id: env.span_id,
        project_id: env.project_id,
        session_id: env.session_id,
        user_id: env.user_id,
        service: env.service,
        level,
        source,
        message,
        attributes: json_opt(&log.attributes),
    }
}

/// Build metric records from an OTLP metric + resource attributes.
pub async fn build_metric_records(
    metric: &proto::Metric,
    resource_attrs: &[proto::KeyValue],
    pool: &PgPool,
) -> Vec<MetricRecord> {
    let mut records = Vec::new();
    let env = build_metric_envelope(resource_attrs, pool).await;
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
    pool: &PgPool,
) -> CorrelationEnvelope {
    let mut env = correlation::extract_correlation(resource_attrs, &[]);
    let _ = correlation::resolve_session(pool, &mut env).await;
    env
}

/// Convert a `NumberDataPoint` to a `MetricRecord`.
pub fn number_point_to_record(
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

/// Convert proto attributes to JSON, returning None for empty.
pub fn json_opt(attrs: &[proto::KeyValue]) -> Option<serde_json::Value> {
    let v = proto::attrs_to_json(attrs);
    if v.is_null() { None } else { Some(v) }
}

/// Convert span events to JSON array.
pub fn events_to_json(events: &[proto::SpanEvent]) -> Option<serde_json::Value> {
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

/// Attempt to send a span record to the channel; returns error on buffer full.
pub fn try_send_span(channels: &IngestChannels, record: SpanRecord) -> Result<(), ApiError> {
    if channels.spans_tx.try_send(record).is_err() {
        warn_buffer_full("traces");
        return Err(ApiError::ServiceUnavailable("ingest buffer full".into()));
    }
    Ok(())
}

/// Attempt to send a log record to the channel; returns error on buffer full.
pub fn try_send_log(channels: &IngestChannels, record: LogEntryRecord) -> Result<(), ApiError> {
    if channels.logs_tx.try_send(record).is_err() {
        warn_buffer_full("logs");
        return Err(ApiError::ServiceUnavailable("ingest buffer full".into()));
    }
    Ok(())
}

/// Attempt to send a metric record to the channel; returns error on buffer full.
pub fn try_send_metric(channels: &IngestChannels, record: MetricRecord) -> Result<(), ApiError> {
    if channels.metrics_tx.try_send(record).is_err() {
        warn_buffer_full("metrics");
        return Err(ApiError::ServiceUnavailable("ingest buffer full".into()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Background flush tasks
// ---------------------------------------------------------------------------

/// Drain the spans channel, batch-write to Postgres, and XADD matching
/// samples to the alert stream for span/trace alert rules.
pub async fn flush_spans(
    pool: sqlx::PgPool,
    valkey: fred::clients::Pool,
    alert_router: std::sync::Arc<tokio::sync::RwLock<crate::alert::AlertRouter>>,
    mut rx: mpsc::Receiver<SpanRecord>,
    cancel: tokio_util::sync::CancellationToken,
) {
    let mut buffer = Vec::with_capacity(128);
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));

    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    drain_spans(&pool, &valkey, &alert_router, &mut rx, &mut buffer),
                ).await;
                break;
            }
            _ = interval.tick() => {
                drain_spans(&pool, &valkey, &alert_router, &mut rx, &mut buffer).await;
            }
        }
    }
}

async fn drain_spans(
    pool: &sqlx::PgPool,
    valkey: &fred::clients::Pool,
    alert_router: &tokio::sync::RwLock<crate::alert::AlertRouter>,
    rx: &mut mpsc::Receiver<SpanRecord>,
    buffer: &mut Vec<SpanRecord>,
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

    if let Err(e) = crate::store::write_spans(pool, buffer).await {
        tracing::error!(error = %e, count = buffer.len(), "failed to flush spans");
        buffer.clear();
        return;
    }

    xadd_span_alert_samples(valkey, alert_router, buffer).await;
    buffer.clear();
}

/// Drain the logs channel, batch-write, publish to Valkey for live tail,
/// and XADD matching samples to the alert stream for log alert rules.
pub async fn flush_logs(
    pool: sqlx::PgPool,
    valkey: fred::clients::Pool,
    alert_router: std::sync::Arc<tokio::sync::RwLock<crate::alert::AlertRouter>>,
    mut rx: mpsc::Receiver<LogEntryRecord>,
    cancel: tokio_util::sync::CancellationToken,
) {
    let mut buffer = Vec::with_capacity(128);
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));

    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    drain_and_publish_logs(&pool, &valkey, &alert_router, &mut rx, &mut buffer),
                ).await;
                break;
            }
            _ = interval.tick() => {
                drain_and_publish_logs(&pool, &valkey, &alert_router, &mut rx, &mut buffer).await;
            }
        }
    }
}

async fn drain_and_publish_logs(
    pool: &sqlx::PgPool,
    valkey: &fred::clients::Pool,
    alert_router: &tokio::sync::RwLock<crate::alert::AlertRouter>,
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
                source: log.source.clone(),
                message: log.message.clone(),
                trace_id: log.trace_id.clone(),
            };
            if let Ok(json) = serde_json::to_string(&msg) {
                let channel = format!("logs:{pid}");
                let _ = platform_types::valkey::publish(valkey, &channel, &json).await;
            }
        }
    }

    if let Err(e) = crate::store::write_logs(pool, buffer).await {
        tracing::error!(error = %e, count = buffer.len(), "failed to flush logs");
        buffer.clear();
        return;
    }

    xadd_log_alert_samples(valkey, alert_router, buffer).await;
    buffer.clear();
}

/// Drain the metrics channel and batch-write to Postgres.
///
/// After writing to Postgres, matching samples are also published to the
/// `alert:samples` Valkey stream for the stream alert evaluator.
pub async fn flush_metrics(
    pool: sqlx::PgPool,
    valkey: fred::clients::Pool,
    alert_router: std::sync::Arc<tokio::sync::RwLock<crate::alert::AlertRouter>>,
    mut rx: mpsc::Receiver<MetricRecord>,
    cancel: tokio_util::sync::CancellationToken,
) {
    let mut buffer = Vec::with_capacity(128);
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));

    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    drain_metrics(&pool, &valkey, &alert_router, &mut rx, &mut buffer),
                ).await;
                break;
            }
            _ = interval.tick() => {
                drain_metrics(&pool, &valkey, &alert_router, &mut rx, &mut buffer).await;
            }
        }
    }
}

async fn drain_metrics(
    pool: &sqlx::PgPool,
    valkey: &fred::clients::Pool,
    alert_router: &tokio::sync::RwLock<crate::alert::AlertRouter>,
    rx: &mut mpsc::Receiver<MetricRecord>,
    buffer: &mut Vec<MetricRecord>,
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

    // Write to Postgres first
    if let Err(e) = crate::store::write_metrics(pool, buffer).await {
        tracing::error!(error = %e, count = buffer.len(), "failed to flush metrics");
        buffer.clear();
        return;
    }

    // XADD matching samples to alert:samples (best-effort)
    xadd_alert_samples(valkey, alert_router, buffer).await;

    buffer.clear();
}

/// Route metric records through the `AlertRouter` and XADD matching samples
/// to the `alert:samples` Valkey stream. Best-effort — failures are logged.
async fn xadd_alert_samples(
    valkey: &fred::clients::Pool,
    alert_router: &tokio::sync::RwLock<crate::alert::AlertRouter>,
    buffer: &[MetricRecord],
) {
    use fred::interfaces::StreamsInterface;

    let router = alert_router.read().await;
    if router.is_empty() {
        return;
    }

    let pipeline = valkey.next().pipeline();
    let mut has_matches = false;

    for record in buffer {
        let matching = router.matching_rules(&record.name, &record.labels, record.project_id);
        if matching.is_empty() {
            continue;
        }
        has_matches = true;

        let rules_str: String = matching
            .iter()
            .map(Uuid::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let ts_ms = record.timestamp.timestamp_millis().to_string();
        let value = record.value.to_string();

        // XADD alert:samples * r <rule_ids> t <ts_ms> v <value>
        let fields: Vec<(&str, &str)> = vec![
            ("r", rules_str.as_str()),
            ("t", ts_ms.as_str()),
            ("v", value.as_str()),
        ];
        let _: Result<(), _> = pipeline
            .xadd::<(), _, _, _, _>(
                crate::alert::ALERT_STREAM_KEY,
                false, // no NOMKSTREAM
                None::<()>,
                "*",
                fields,
            )
            .await;
    }

    if has_matches && let Err(e) = pipeline.all::<()>().await {
        tracing::debug!(error = %e, "failed to XADD alert samples (best-effort)");
    }
}

/// Route log records through the `AlertRouter` and XADD matching samples
/// to the `alert:samples` Valkey stream. Best-effort — failures are logged.
async fn xadd_log_alert_samples(
    valkey: &fred::clients::Pool,
    alert_router: &tokio::sync::RwLock<crate::alert::AlertRouter>,
    buffer: &[LogEntryRecord],
) {
    use fred::interfaces::StreamsInterface;

    let router = alert_router.read().await;
    if !router.has_log_rules() {
        return;
    }

    let pipeline = valkey.next().pipeline();
    let mut has_matches = false;

    for record in buffer {
        let matches = router.matching_log_rules(
            &record.level,
            &record.message,
            &record.service,
            record.project_id,
        );

        for (rule_id, value) in matches {
            has_matches = true;
            let rule_str = rule_id.to_string();
            let ts_ms = record.timestamp.timestamp_millis().to_string();
            let val_str = value.to_string();
            let fields: Vec<(&str, &str)> = vec![
                ("r", rule_str.as_str()),
                ("t", ts_ms.as_str()),
                ("v", val_str.as_str()),
            ];
            let _: Result<(), _> = pipeline
                .xadd::<(), _, _, _, _>(
                    crate::alert::ALERT_STREAM_KEY,
                    false,
                    None::<()>,
                    "*",
                    fields,
                )
                .await;
        }
    }

    if has_matches && let Err(e) = pipeline.all::<()>().await {
        tracing::debug!(error = %e, "failed to XADD log alert samples (best-effort)");
    }
}

/// Route span records through the `AlertRouter` and XADD matching samples
/// to the `alert:samples` Valkey stream. Best-effort — failures are logged.
async fn xadd_span_alert_samples(
    valkey: &fred::clients::Pool,
    alert_router: &tokio::sync::RwLock<crate::alert::AlertRouter>,
    buffer: &[SpanRecord],
) {
    use fred::interfaces::StreamsInterface;

    let router = alert_router.read().await;
    if !router.has_span_rules() {
        return;
    }

    let pipeline = valkey.next().pipeline();
    let mut has_matches = false;

    for record in buffer {
        let input = crate::alert::SpanAlertInput {
            name: &record.name,
            service: &record.service,
            status: &record.status,
            duration_ms: record.duration_ms.map(f64::from),
            is_root: record.parent_span_id.is_none(),
        };

        let matches = router.matching_span_rules(&input, record.project_id);
        for (rule_id, value) in matches {
            has_matches = true;
            let rule_str = rule_id.to_string();
            let ts_ms = record.started_at.timestamp_millis().to_string();
            let val_str = value.to_string();
            let fields: Vec<(&str, &str)> = vec![
                ("r", rule_str.as_str()),
                ("t", ts_ms.as_str()),
                ("v", val_str.as_str()),
            ];
            let _: Result<(), _> = pipeline
                .xadd::<(), _, _, _, _>(
                    crate::alert::ALERT_STREAM_KEY,
                    false,
                    None::<()>,
                    "*",
                    fields,
                )
                .await;
        }
    }

    if has_matches && let Err(e) = pipeline.all::<()>().await {
        tracing::debug!(error = %e, "failed to XADD span alert samples (best-effort)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto;

    // -- number_point_to_record ----------------------------------------

    fn empty_envelope() -> correlation::CorrelationEnvelope {
        correlation::CorrelationEnvelope {
            project_id: None,
            session_id: None,
            user_id: None,
            trace_id: None,
            span_id: None,
            service: "test-svc".into(),
        }
    }

    #[test]
    fn number_point_double_value() {
        let dp = proto::NumberDataPoint {
            value: Some(proto::number_data_point::Value::AsDouble(42.5)),
            time_unix_nano: 1_700_000_000_000_000_000,
            attributes: vec![],
        };
        let env = empty_envelope();
        let rec = number_point_to_record(&dp, "cpu", "gauge", Some("percent"), &env).unwrap();
        assert_eq!(rec.name, "cpu");
        assert_eq!(rec.metric_type, "gauge");
        assert!((rec.value - 42.5).abs() < f64::EPSILON);
        assert_eq!(rec.unit.as_deref(), Some("percent"));
    }

    #[test]
    fn number_point_int_value() {
        let dp = proto::NumberDataPoint {
            value: Some(proto::number_data_point::Value::AsInt(100)),
            time_unix_nano: 1_700_000_000_000_000_000,
            attributes: vec![],
        };
        let env = empty_envelope();
        let rec = number_point_to_record(&dp, "requests", "counter", None, &env).unwrap();
        assert!((rec.value - 100.0).abs() < f64::EPSILON);
        assert_eq!(rec.unit, None);
    }

    #[test]
    fn number_point_none_value_returns_none() {
        let dp = proto::NumberDataPoint {
            value: None,
            time_unix_nano: 1_700_000_000_000_000_000,
            attributes: vec![],
        };
        let env = empty_envelope();
        assert!(number_point_to_record(&dp, "x", "gauge", None, &env).is_none());
    }

    // -- json_opt -------------------------------------------------------

    #[test]
    fn json_opt_non_empty_returns_some() {
        let attrs = vec![proto::KeyValue {
            key: "foo".into(),
            value: Some(proto::AnyValue {
                value: Some(proto::any_value::Value::StringValue("bar".into())),
            }),
        }];
        let result = json_opt(&attrs);
        assert!(result.is_some());
        assert_eq!(result.unwrap()["foo"], "bar");
    }

    #[test]
    fn json_opt_empty_returns_none() {
        assert!(json_opt(&[]).is_none());
    }

    // -- events_to_json -------------------------------------------------

    #[test]
    fn events_to_json_empty_returns_none() {
        assert!(events_to_json(&[]).is_none());
    }

    #[test]
    fn events_to_json_single_event() {
        let events = vec![proto::SpanEvent {
            time_unix_nano: 1_700_000_000_000_000_000,
            name: "exception".into(),
            attributes: vec![],
        }];
        let json = events_to_json(&events).unwrap();
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "exception");
    }

    #[test]
    fn events_to_json_multiple_events() {
        let events = vec![
            proto::SpanEvent {
                time_unix_nano: 1_700_000_000_000_000_000,
                name: "e1".into(),
                attributes: vec![],
            },
            proto::SpanEvent {
                time_unix_nano: 1_700_000_001_000_000_000,
                name: "e2".into(),
                attributes: vec![],
            },
        ];
        let json = events_to_json(&events).unwrap();
        assert_eq!(json.as_array().unwrap().len(), 2);
    }

    // -- BUFFER_CAPACITY constant --

    #[test]
    fn buffer_capacity_is_10k() {
        assert_eq!(BUFFER_CAPACITY, 10_000);
    }

    // -- create_channels capacity --

    #[test]
    fn create_channels_have_expected_capacity() {
        let (channels, spans_rx, logs_rx, metrics_rx) = create_channels();
        assert_eq!(channels.spans_tx.capacity(), BUFFER_CAPACITY);
        assert_eq!(channels.logs_tx.capacity(), BUFFER_CAPACITY);
        assert_eq!(channels.metrics_tx.capacity(), BUFFER_CAPACITY);
        drop(spans_rx);
        drop(logs_rx);
        drop(metrics_rx);
    }

    // -- IngestChannels Clone --

    #[test]
    fn ingest_channels_clone() {
        let (channels, _spans_rx, _logs_rx, _metrics_rx) = create_channels();
        let cloned = channels.clone();
        assert_eq!(channels.spans_tx.capacity(), cloned.spans_tx.capacity());
        assert_eq!(channels.logs_tx.capacity(), cloned.logs_tx.capacity());
        assert_eq!(channels.metrics_tx.capacity(), cloned.metrics_tx.capacity());
    }

    // -- events_to_json preserves time format --

    #[test]
    fn events_to_json_time_is_rfc3339() {
        let events = vec![proto::SpanEvent {
            time_unix_nano: 1_700_000_000_000_000_000,
            name: "time-check".into(),
            attributes: vec![],
        }];
        let json = events_to_json(&events).unwrap();
        let time_str = json[0]["time"].as_str().unwrap();
        assert!(
            chrono::DateTime::parse_from_rfc3339(time_str).is_ok(),
            "time should be RFC3339, got: {time_str}"
        );
    }

    // -- maybe_decompress -------------------------------------------------

    #[test]
    fn decompress_plain_body_passes_through() {
        let headers = HeaderMap::new();
        let body = Bytes::from_static(b"hello");
        let result = maybe_decompress(&headers, body.clone()).unwrap();
        assert_eq!(result, body);
    }

    #[test]
    fn decompress_gzip_body() {
        use flate2::write::GzEncoder;
        use std::io::Write;

        let original = b"hello world protobuf data";
        let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(original).unwrap();
        let compressed = encoder.finish().unwrap();

        let mut headers = HeaderMap::new();
        headers.insert("content-encoding", "gzip".parse().unwrap());
        let result = maybe_decompress(&headers, Bytes::from(compressed)).unwrap();
        assert_eq!(&result[..], original);
    }

    #[test]
    fn decompress_invalid_gzip_returns_error() {
        let mut headers = HeaderMap::new();
        headers.insert("content-encoding", "gzip".parse().unwrap());
        let result = maybe_decompress(&headers, Bytes::from_static(b"not gzip"));
        assert!(result.is_err());
    }

    // -- number_point edge cases -------

    #[test]
    fn number_point_envelope_project_propagated() {
        let dp = proto::NumberDataPoint {
            value: Some(proto::number_data_point::Value::AsDouble(1.0)),
            time_unix_nano: 1_700_000_000_000_000_000,
            attributes: vec![],
        };
        let env = correlation::CorrelationEnvelope {
            project_id: Some(Uuid::nil()),
            session_id: None,
            user_id: None,
            trace_id: None,
            span_id: None,
            service: "svc".into(),
        };
        let rec = number_point_to_record(&dp, "m", "gauge", None, &env).unwrap();
        assert_eq!(rec.project_id, Some(Uuid::nil()));
    }

    #[test]
    fn number_point_timestamp_propagated() {
        let dp = proto::NumberDataPoint {
            value: Some(proto::number_data_point::Value::AsDouble(1.0)),
            time_unix_nano: 1_700_000_000_000_000_000,
            attributes: vec![],
        };
        let env = empty_envelope();
        let rec = number_point_to_record(&dp, "ts-test", "gauge", None, &env).unwrap();
        let expected = proto::nanos_to_datetime(1_700_000_000_000_000_000);
        assert_eq!(rec.timestamp, expected);
    }

    // -- create_channels_with_capacity -----------------------------------

    #[test]
    fn create_channels_with_zero_capacity_uses_default() {
        let (channels, spans_rx, logs_rx, metrics_rx) = create_channels_with_capacity(0);
        assert_eq!(channels.spans_tx.capacity(), BUFFER_CAPACITY);
        assert_eq!(channels.logs_tx.capacity(), BUFFER_CAPACITY);
        assert_eq!(channels.metrics_tx.capacity(), BUFFER_CAPACITY);
        drop((spans_rx, logs_rx, metrics_rx));
    }

    #[test]
    fn create_channels_with_custom_capacity() {
        let (channels, spans_rx, logs_rx, metrics_rx) = create_channels_with_capacity(42);
        assert_eq!(channels.spans_tx.capacity(), 42);
        assert_eq!(channels.logs_tx.capacity(), 42);
        assert_eq!(channels.metrics_tx.capacity(), 42);
        drop((spans_rx, logs_rx, metrics_rx));
    }

    // -- try_send_span / try_send_log / try_send_metric ------------------

    #[test]
    fn try_send_span_success() {
        let (channels, _rx_s, _rx_l, _rx_m) = create_channels_with_capacity(10);
        let span = SpanRecord {
            trace_id: "t1".into(),
            span_id: "s1".into(),
            parent_span_id: None,
            name: "test".into(),
            service: "svc".into(),
            kind: "internal".into(),
            status: "ok".into(),
            attributes: None,
            events: None,
            duration_ms: None,
            started_at: chrono::Utc::now(),
            finished_at: None,
            project_id: None,
            session_id: None,
            user_id: None,
        };
        assert!(try_send_span(&channels, span).is_ok());
    }

    #[test]
    fn try_send_span_buffer_full() {
        let (channels, _rx_s, _rx_l, _rx_m) = create_channels_with_capacity(1);
        let make = || SpanRecord {
            trace_id: "t".into(),
            span_id: "s".into(),
            parent_span_id: None,
            name: "test".into(),
            service: "svc".into(),
            kind: "internal".into(),
            status: "ok".into(),
            attributes: None,
            events: None,
            duration_ms: None,
            started_at: chrono::Utc::now(),
            finished_at: None,
            project_id: None,
            session_id: None,
            user_id: None,
        };
        assert!(try_send_span(&channels, make()).is_ok());
        let err = try_send_span(&channels, make()).unwrap_err();
        assert!(matches!(err, ApiError::ServiceUnavailable(_)));
    }

    #[test]
    fn try_send_log_success() {
        let (channels, _rx_s, _rx_l, _rx_m) = create_channels_with_capacity(10);
        let log = LogEntryRecord {
            timestamp: chrono::Utc::now(),
            trace_id: None,
            span_id: None,
            project_id: None,
            session_id: None,
            user_id: None,
            service: "svc".into(),
            level: "info".into(),
            source: "external".into(),
            message: "msg".into(),
            attributes: None,
        };
        assert!(try_send_log(&channels, log).is_ok());
    }

    #[test]
    fn try_send_log_buffer_full() {
        let (channels, _rx_s, _rx_l, _rx_m) = create_channels_with_capacity(1);
        let make = || LogEntryRecord {
            timestamp: chrono::Utc::now(),
            trace_id: None,
            span_id: None,
            project_id: None,
            session_id: None,
            user_id: None,
            service: "svc".into(),
            level: "info".into(),
            source: "external".into(),
            message: "msg".into(),
            attributes: None,
        };
        assert!(try_send_log(&channels, make()).is_ok());
        let err = try_send_log(&channels, make()).unwrap_err();
        assert!(matches!(err, ApiError::ServiceUnavailable(_)));
    }

    #[test]
    fn try_send_metric_success() {
        let (channels, _rx_s, _rx_l, _rx_m) = create_channels_with_capacity(10);
        let metric = MetricRecord {
            name: "m".into(),
            labels: serde_json::json!({}),
            metric_type: "gauge".into(),
            unit: None,
            project_id: None,
            timestamp: chrono::Utc::now(),
            value: 1.0,
        };
        assert!(try_send_metric(&channels, metric).is_ok());
    }

    #[test]
    fn try_send_metric_buffer_full() {
        let (channels, _rx_s, _rx_l, _rx_m) = create_channels_with_capacity(1);
        let make = || MetricRecord {
            name: "m".into(),
            labels: serde_json::json!({}),
            metric_type: "gauge".into(),
            unit: None,
            project_id: None,
            timestamp: chrono::Utc::now(),
            value: 1.0,
        };
        assert!(try_send_metric(&channels, make()).is_ok());
        let err = try_send_metric(&channels, make()).unwrap_err();
        assert!(matches!(err, ApiError::ServiceUnavailable(_)));
    }

    // -- warn_buffer_full ------------------------------------------------

    #[test]
    fn warn_buffer_full_does_not_panic() {
        warn_buffer_full("test-signal");
        warn_buffer_full("test-signal");
    }

    // -- check_otlp_project_auth -----------------------------------------

    struct AllowAllChecker;
    impl PermissionChecker for AllowAllChecker {
        async fn has_permission(
            &self,
            _: Uuid,
            _: Option<Uuid>,
            _: Permission,
        ) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn has_permission_scoped(
            &self,
            _: Uuid,
            _: Option<Uuid>,
            _: Permission,
            _: Option<&[String]>,
        ) -> anyhow::Result<bool> {
            Ok(true)
        }
    }

    struct DenyAllChecker;
    impl PermissionChecker for DenyAllChecker {
        async fn has_permission(
            &self,
            _: Uuid,
            _: Option<Uuid>,
            _: Permission,
        ) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn has_permission_scoped(
            &self,
            _: Uuid,
            _: Option<Uuid>,
            _: Permission,
            _: Option<&[String]>,
        ) -> anyhow::Result<bool> {
            Ok(false)
        }
    }

    fn kv_string(key: &str, val: &str) -> proto::KeyValue {
        proto::KeyValue {
            key: key.into(),
            value: Some(proto::AnyValue {
                value: Some(proto::any_value::Value::StringValue(val.into())),
            }),
        }
    }

    fn make_auth(user_id: Uuid) -> AuthUser {
        AuthUser {
            user_id,
            user_name: "test_user".into(),
            user_type: platform_types::UserType::Human,
            ip_addr: Some("127.0.0.1".into()),
            token_scopes: None,
            boundary_workspace_id: None,
            boundary_project_id: None,
            session_id: None,
            session_token_hash: None,
        }
    }

    fn make_auth_with_project_scope(user_id: Uuid, project_id: Uuid) -> AuthUser {
        AuthUser {
            user_id,
            user_name: "test_user".into(),
            user_type: platform_types::UserType::Human,
            ip_addr: Some("127.0.0.1".into()),
            token_scopes: None,
            boundary_workspace_id: None,
            boundary_project_id: Some(project_id),
            session_id: None,
            session_token_hash: None,
        }
    }

    #[tokio::test]
    async fn check_otlp_auth_valid_project_allowed() {
        let pid = Uuid::new_v4();
        let auth = make_auth(Uuid::new_v4());
        let attrs = vec![kv_string("platform.project_id", &pid.to_string())];
        let result = check_otlp_project_auth(&auth, &AllowAllChecker, &[attrs.as_slice()]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn check_otlp_auth_system_metrics_observe_write_allowed() {
        let auth = make_auth(Uuid::new_v4());
        let attrs: Vec<proto::KeyValue> = vec![]; // no project_id → system metrics
        let result = check_otlp_project_auth(&auth, &AllowAllChecker, &[attrs.as_slice()]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn check_otlp_auth_system_metrics_no_observe_write_denied() {
        let auth = make_auth(Uuid::new_v4());
        let attrs: Vec<proto::KeyValue> = vec![];
        let result = check_otlp_project_auth(&auth, &DenyAllChecker, &[attrs.as_slice()]).await;
        assert!(matches!(result, Err(ApiError::Forbidden)));
    }

    #[tokio::test]
    async fn check_otlp_auth_invalid_uuid_returns_bad_request() {
        let auth = make_auth(Uuid::new_v4());
        let attrs = vec![kv_string("platform.project_id", "not-a-uuid")];
        let result = check_otlp_project_auth(&auth, &AllowAllChecker, &[attrs.as_slice()]).await;
        assert!(matches!(result, Err(ApiError::BadRequest(_))));
    }

    #[tokio::test]
    async fn check_otlp_auth_project_not_allowed_returns_not_found() {
        let pid = Uuid::new_v4();
        let auth = make_auth(Uuid::new_v4());
        let attrs = vec![kv_string("platform.project_id", &pid.to_string())];
        let result = check_otlp_project_auth(&auth, &DenyAllChecker, &[attrs.as_slice()]).await;
        assert!(matches!(result, Err(ApiError::NotFound(_))));
    }

    #[tokio::test]
    async fn check_otlp_auth_scope_mismatch_returns_not_found() {
        let pid = Uuid::new_v4();
        let other_pid = Uuid::new_v4();
        let auth = make_auth_with_project_scope(Uuid::new_v4(), other_pid);
        let attrs = vec![kv_string("platform.project_id", &pid.to_string())];
        let result = check_otlp_project_auth(&auth, &AllowAllChecker, &[attrs.as_slice()]).await;
        assert!(matches!(result, Err(ApiError::NotFound(_))));
    }

    #[tokio::test]
    async fn check_otlp_auth_empty_resource_list() {
        let auth = make_auth(Uuid::new_v4());
        let result = check_otlp_project_auth(&auth, &AllowAllChecker, &[]).await;
        assert!(result.is_ok());
    }
}
