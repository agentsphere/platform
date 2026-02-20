use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value as JsonValue;
use sqlx::PgPool;
use uuid::Uuid;

use super::error::ObserveError;

// ---------------------------------------------------------------------------
// Internal record types (not API response types)
// ---------------------------------------------------------------------------

/// Span record ready for batch insertion.
pub struct SpanRecord {
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub name: String,
    pub service: String,
    pub kind: String,
    pub status: String,
    pub attributes: Option<JsonValue>,
    pub events: Option<JsonValue>,
    pub duration_ms: Option<i32>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub project_id: Option<Uuid>,
    pub session_id: Option<Uuid>,
    pub user_id: Option<Uuid>,
}

/// Log entry record ready for batch insertion.
pub struct LogEntryRecord {
    pub timestamp: DateTime<Utc>,
    pub trace_id: Option<String>,
    pub span_id: Option<String>,
    pub project_id: Option<Uuid>,
    pub session_id: Option<Uuid>,
    pub user_id: Option<Uuid>,
    pub service: String,
    pub level: String,
    pub message: String,
    pub attributes: Option<JsonValue>,
}

/// Metric sample record ready for batch insertion.
pub struct MetricRecord {
    pub name: String,
    pub labels: JsonValue,
    pub metric_type: String,
    pub unit: Option<String>,
    pub project_id: Option<Uuid>,
    pub timestamp: DateTime<Utc>,
    pub value: f64,
}

/// Lightweight log message for live tail pub/sub.
#[derive(Debug, Serialize)]
pub struct LogTailMessage {
    pub timestamp: DateTime<Utc>,
    pub service: String,
    pub level: String,
    pub message: String,
    pub trace_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Batch write functions
// ---------------------------------------------------------------------------

/// Batch insert spans using multi-row VALUES.
#[tracing::instrument(skip(pool, spans), fields(count = spans.len()), err)]
pub async fn write_spans(pool: &PgPool, spans: &[SpanRecord]) -> Result<(), ObserveError> {
    if spans.is_empty() {
        return Ok(());
    }

    for span in spans {
        upsert_trace(pool, span).await?;
    }

    let trace_ids: Vec<&str> = spans.iter().map(|s| s.trace_id.as_str()).collect();
    let span_ids: Vec<&str> = spans.iter().map(|s| s.span_id.as_str()).collect();
    let parent_ids: Vec<Option<&str>> = spans.iter().map(|s| s.parent_span_id.as_deref()).collect();
    let names: Vec<&str> = spans.iter().map(|s| s.name.as_str()).collect();
    let services: Vec<&str> = spans.iter().map(|s| s.service.as_str()).collect();
    let kinds: Vec<&str> = spans.iter().map(|s| s.kind.as_str()).collect();
    let statuses: Vec<&str> = spans.iter().map(|s| s.status.as_str()).collect();
    let attributes: Vec<Option<&JsonValue>> = spans.iter().map(|s| s.attributes.as_ref()).collect();
    let events: Vec<Option<&JsonValue>> = spans.iter().map(|s| s.events.as_ref()).collect();
    let durations: Vec<Option<i32>> = spans.iter().map(|s| s.duration_ms).collect();
    let started: Vec<DateTime<Utc>> = spans.iter().map(|s| s.started_at).collect();
    let finished: Vec<Option<DateTime<Utc>>> = spans.iter().map(|s| s.finished_at).collect();

    sqlx::query(
        r"
        INSERT INTO spans (trace_id, span_id, parent_span_id, name, service, kind, status,
                           attributes, events, duration_ms, started_at, finished_at)
        SELECT * FROM UNNEST(
            $1::text[], $2::text[], $3::text[], $4::text[], $5::text[],
            $6::text[], $7::text[], $8::jsonb[], $9::jsonb[], $10::int[],
            $11::timestamptz[], $12::timestamptz[]
        )
        ON CONFLICT (span_id) DO NOTHING
        ",
    )
    .bind(&trace_ids)
    .bind(&span_ids)
    .bind(&parent_ids)
    .bind(&names)
    .bind(&services)
    .bind(&kinds)
    .bind(&statuses)
    .bind(&attributes)
    .bind(&events)
    .bind(&durations)
    .bind(&started)
    .bind(&finished)
    .execute(pool)
    .await?;

    Ok(())
}

/// Upsert the trace row when a root span arrives.
async fn upsert_trace(pool: &PgPool, span: &SpanRecord) -> Result<(), ObserveError> {
    let is_root = span.parent_span_id.is_none();
    if !is_root {
        // Ensure trace row exists even for non-root spans
        sqlx::query(
            r"
            INSERT INTO traces (trace_id, root_span, service, status, started_at, project_id, session_id, user_id)
            VALUES ($1, $2, $3, 'unset', $4, $5, $6, $7)
            ON CONFLICT (trace_id) DO NOTHING
            ",
        )
        .bind(&span.trace_id)
        .bind(&span.name)
        .bind(&span.service)
        .bind(span.started_at)
        .bind(span.project_id)
        .bind(span.session_id)
        .bind(span.user_id)
        .execute(pool)
        .await?;
        return Ok(());
    }

    sqlx::query(
        r"
        INSERT INTO traces (trace_id, root_span, service, status, duration_ms, started_at, finished_at,
                            project_id, session_id, user_id)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        ON CONFLICT (trace_id) DO UPDATE SET
            root_span = EXCLUDED.root_span,
            status = EXCLUDED.status,
            duration_ms = EXCLUDED.duration_ms,
            finished_at = EXCLUDED.finished_at
        ",
    )
    .bind(&span.trace_id)
    .bind(&span.name)
    .bind(&span.service)
    .bind(&span.status)
    .bind(span.duration_ms)
    .bind(span.started_at)
    .bind(span.finished_at)
    .bind(span.project_id)
    .bind(span.session_id)
    .bind(span.user_id)
    .execute(pool)
    .await?;

    Ok(())
}

/// Batch insert log entries using UNNEST.
#[tracing::instrument(skip(pool, logs), fields(count = logs.len()), err)]
pub async fn write_logs(pool: &PgPool, logs: &[LogEntryRecord]) -> Result<(), ObserveError> {
    if logs.is_empty() {
        return Ok(());
    }

    let timestamps: Vec<DateTime<Utc>> = logs.iter().map(|l| l.timestamp).collect();
    let trace_ids: Vec<Option<&str>> = logs.iter().map(|l| l.trace_id.as_deref()).collect();
    let span_ids: Vec<Option<&str>> = logs.iter().map(|l| l.span_id.as_deref()).collect();
    let project_ids: Vec<Option<Uuid>> = logs.iter().map(|l| l.project_id).collect();
    let session_ids: Vec<Option<Uuid>> = logs.iter().map(|l| l.session_id).collect();
    let user_ids: Vec<Option<Uuid>> = logs.iter().map(|l| l.user_id).collect();
    let services: Vec<&str> = logs.iter().map(|l| l.service.as_str()).collect();
    let levels: Vec<&str> = logs.iter().map(|l| l.level.as_str()).collect();
    let messages: Vec<&str> = logs.iter().map(|l| l.message.as_str()).collect();
    let attributes: Vec<Option<&JsonValue>> = logs.iter().map(|l| l.attributes.as_ref()).collect();

    sqlx::query(
        r"
        INSERT INTO log_entries (timestamp, trace_id, span_id, project_id, session_id, user_id,
                                 service, level, message, attributes)
        SELECT * FROM UNNEST(
            $1::timestamptz[], $2::text[], $3::text[], $4::uuid[], $5::uuid[], $6::uuid[],
            $7::text[], $8::text[], $9::text[], $10::jsonb[]
        )
        ",
    )
    .bind(&timestamps)
    .bind(&trace_ids)
    .bind(&span_ids)
    .bind(&project_ids)
    .bind(&session_ids)
    .bind(&user_ids)
    .bind(&services)
    .bind(&levels)
    .bind(&messages)
    .bind(&attributes)
    .execute(pool)
    .await?;

    Ok(())
}

/// Batch upsert metric series and insert samples.
#[tracing::instrument(skip(pool, metrics), fields(count = metrics.len()), err)]
pub async fn write_metrics(pool: &PgPool, metrics: &[MetricRecord]) -> Result<(), ObserveError> {
    if metrics.is_empty() {
        return Ok(());
    }

    for m in metrics {
        write_single_metric(pool, m).await?;
    }

    Ok(())
}

async fn write_single_metric(pool: &PgPool, m: &MetricRecord) -> Result<(), ObserveError> {
    // Upsert series, get id
    let series_id: Uuid = sqlx::query_scalar(
        r"
        INSERT INTO metric_series (name, labels, metric_type, unit, project_id, last_value)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (name, labels)
        DO UPDATE SET last_value = EXCLUDED.last_value, updated_at = now()
        RETURNING id
        ",
    )
    .bind(&m.name)
    .bind(&m.labels)
    .bind(&m.metric_type)
    .bind(&m.unit)
    .bind(m.project_id)
    .bind(m.value)
    .fetch_one(pool)
    .await?;

    // Insert sample
    sqlx::query(
        r"
        INSERT INTO metric_samples (series_id, timestamp, value)
        VALUES ($1, $2, $3)
        ON CONFLICT (series_id, timestamp) DO UPDATE SET value = EXCLUDED.value
        ",
    )
    .bind(series_id)
    .bind(m.timestamp)
    .bind(m.value)
    .execute(pool)
    .await?;

    Ok(())
}
