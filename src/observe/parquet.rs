use std::sync::Arc;

use arrow::array::{Float64Array, Int32Array, StringArray, TimestampMicrosecondArray};
use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use sqlx::Row;
use uuid::Uuid;

use crate::store::AppState;

use super::error::ObserveError;

// ---------------------------------------------------------------------------
// Rotation loop
// ---------------------------------------------------------------------------

/// Background task: rotate old data to Parquet every 15 minutes.
pub async fn rotation_loop(state: AppState, mut shutdown: tokio::sync::watch::Receiver<()>) {
    tracing::info!("parquet rotation started");

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                tracing::info!("parquet rotation shutting down");
                break;
            }
            () = tokio::time::sleep(std::time::Duration::from_secs(900)) => {
                if let Err(e) = rotate_logs(&state).await {
                    tracing::error!(error = %e, "log rotation failed");
                }
                if let Err(e) = rotate_spans(&state).await {
                    tracing::error!(error = %e, "span rotation failed");
                }
                if let Err(e) = rotate_metrics(&state).await {
                    tracing::error!(error = %e, "metric rotation failed");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Log rotation
// ---------------------------------------------------------------------------

/// Rotate log entries older than 48h to `MinIO` Parquet.
#[tracing::instrument(skip(state), err)]
pub async fn rotate_logs(state: &AppState) -> Result<u64, ObserveError> {
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(48);

    let rows = sqlx::query(
        r"
        SELECT id, timestamp, trace_id, span_id, project_id, session_id,
               service, level, message, attributes
        FROM log_entries
        WHERE timestamp < $1
        ORDER BY timestamp ASC
        LIMIT 10000
        ",
    )
    .bind(cutoff)
    .fetch_all(&state.pool)
    .await?;

    if rows.is_empty() {
        return Ok(0);
    }

    let count = rows.len() as u64;
    let ids: Vec<Uuid> = rows.iter().map(|r| r.get::<Uuid, _>("id")).collect();

    let typed: Vec<LogQueryRow> = rows
        .iter()
        .map(|r| LogQueryRow {
            id: r.get("id"),
            timestamp: r.get("timestamp"),
            trace_id: r.get("trace_id"),
            span_id: r.get("span_id"),
            project_id: r.get("project_id"),
            session_id: r.get("session_id"),
            service: r.get("service"),
            level: r.get("level"),
            message: r.get("message"),
            attributes: r.get("attributes"),
        })
        .collect();

    let batch = build_log_batch(&typed)?;
    let parquet_bytes = write_parquet_buffer(&batch)?;
    upload_and_delete_logs(state, &ids, parquet_bytes, cutoff).await?;

    tracing::info!(count, "rotated logs to parquet");
    Ok(count)
}

async fn upload_and_delete_logs(
    state: &AppState,
    ids: &[Uuid],
    parquet_bytes: Vec<u8>,
    cutoff: chrono::DateTime<chrono::Utc>,
) -> Result<(), ObserveError> {
    let date = cutoff.format("%Y-%m-%d");
    let batch_id = Uuid::new_v4();
    let path = format!("otel/logs/{date}/logs_{batch_id}.parquet");
    state.minio.write(&path, parquet_bytes).await?;

    sqlx::query("DELETE FROM log_entries WHERE id = ANY($1)")
        .bind(ids)
        .execute(&state.pool)
        .await?;
    Ok(())
}

fn log_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new(
            "timestamp",
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            false,
        ),
        Field::new("trace_id", DataType::Utf8, true),
        Field::new("span_id", DataType::Utf8, true),
        Field::new("project_id", DataType::Utf8, true),
        Field::new("session_id", DataType::Utf8, true),
        Field::new("service", DataType::Utf8, false),
        Field::new("level", DataType::Utf8, false),
        Field::new("message", DataType::Utf8, false),
        Field::new("attributes", DataType::Utf8, true),
    ]))
}

fn build_log_batch(rows: &[LogQueryRow]) -> Result<RecordBatch, ObserveError> {
    let len = rows.len();
    let mut ids = Vec::with_capacity(len);
    let mut timestamps = Vec::with_capacity(len);
    let mut trace_ids: Vec<Option<String>> = Vec::with_capacity(len);
    let mut span_ids: Vec<Option<String>> = Vec::with_capacity(len);
    let mut project_ids: Vec<Option<String>> = Vec::with_capacity(len);
    let mut session_ids: Vec<Option<String>> = Vec::with_capacity(len);
    let mut services = Vec::with_capacity(len);
    let mut levels = Vec::with_capacity(len);
    let mut messages = Vec::with_capacity(len);
    let mut attributes: Vec<Option<String>> = Vec::with_capacity(len);

    for row in rows {
        ids.push(row.id.to_string());
        timestamps.push(row.timestamp.timestamp_micros());
        trace_ids.push(row.trace_id.clone());
        span_ids.push(row.span_id.clone());
        project_ids.push(row.project_id.map(|u| u.to_string()));
        session_ids.push(row.session_id.map(|u| u.to_string()));
        services.push(row.service.clone());
        levels.push(row.level.clone());
        messages.push(row.message.clone());
        attributes.push(row.attributes.as_ref().map(ToString::to_string));
    }

    let schema = log_schema();
    let columns: Vec<Arc<dyn arrow::array::Array>> = vec![
        Arc::new(StringArray::from(ids)),
        Arc::new(TimestampMicrosecondArray::from(timestamps).with_timezone("UTC")),
        Arc::new(StringArray::from(trace_ids)),
        Arc::new(StringArray::from(span_ids)),
        Arc::new(StringArray::from(project_ids)),
        Arc::new(StringArray::from(session_ids)),
        Arc::new(StringArray::from(services)),
        Arc::new(StringArray::from(levels)),
        Arc::new(StringArray::from(messages)),
        Arc::new(StringArray::from(attributes)),
    ];

    Ok(RecordBatch::try_new(schema, columns)?)
}

struct LogQueryRow {
    id: Uuid,
    timestamp: chrono::DateTime<chrono::Utc>,
    trace_id: Option<String>,
    span_id: Option<String>,
    project_id: Option<Uuid>,
    session_id: Option<Uuid>,
    service: String,
    level: String,
    message: String,
    attributes: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Span rotation
// ---------------------------------------------------------------------------

/// Rotate spans older than 48h to `MinIO` Parquet.
#[tracing::instrument(skip(state), err)]
pub async fn rotate_spans(state: &AppState) -> Result<u64, ObserveError> {
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(48);

    let rows = sqlx::query(
        r"
        SELECT id, trace_id, span_id, parent_span_id, name, service, kind, status,
               attributes, duration_ms, started_at
        FROM spans
        WHERE started_at < $1
        ORDER BY started_at ASC
        LIMIT 10000
        ",
    )
    .bind(cutoff)
    .fetch_all(&state.pool)
    .await?;

    if rows.is_empty() {
        return Ok(0);
    }

    let count = rows.len() as u64;
    let ids: Vec<Uuid> = rows.iter().map(|r| r.get::<Uuid, _>("id")).collect();

    // Convert to typed rows for batch building
    let typed: Vec<SpanQueryRow> = rows
        .iter()
        .map(|r| SpanQueryRow {
            trace_id: r.get("trace_id"),
            span_id: r.get("span_id"),
            parent_span_id: r.get("parent_span_id"),
            name: r.get("name"),
            service: r.get("service"),
            kind: r.get("kind"),
            status: r.get("status"),
            duration_ms: r.get("duration_ms"),
            started_at: r.get("started_at"),
            attributes: r.get("attributes"),
        })
        .collect();

    let batch = build_span_batch(&typed)?;
    let parquet_bytes = write_parquet_buffer(&batch)?;

    let date = cutoff.format("%Y-%m-%d");
    let batch_id = Uuid::new_v4();
    let path = format!("otel/traces/{date}/spans_{batch_id}.parquet");
    state.minio.write(&path, parquet_bytes).await?;

    sqlx::query("DELETE FROM spans WHERE id = ANY($1)")
        .bind(&ids)
        .execute(&state.pool)
        .await?;

    tracing::info!(count, path = %path, "rotated spans to parquet");
    Ok(count)
}

fn span_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("trace_id", DataType::Utf8, false),
        Field::new("span_id", DataType::Utf8, false),
        Field::new("parent_span_id", DataType::Utf8, true),
        Field::new("name", DataType::Utf8, false),
        Field::new("service", DataType::Utf8, false),
        Field::new("kind", DataType::Utf8, false),
        Field::new("status", DataType::Utf8, false),
        Field::new("duration_ms", DataType::Int32, true),
        Field::new(
            "started_at",
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            false,
        ),
        Field::new("attributes", DataType::Utf8, true),
    ]))
}

struct SpanQueryRow {
    trace_id: String,
    span_id: String,
    parent_span_id: Option<String>,
    name: String,
    service: String,
    kind: String,
    status: String,
    duration_ms: Option<i32>,
    started_at: chrono::DateTime<chrono::Utc>,
    attributes: Option<serde_json::Value>,
}

fn build_span_batch(rows: &[SpanQueryRow]) -> Result<RecordBatch, ObserveError> {
    let len = rows.len();
    let mut trace_ids = Vec::with_capacity(len);
    let mut span_ids = Vec::with_capacity(len);
    let mut parent_ids: Vec<Option<String>> = Vec::with_capacity(len);
    let mut names = Vec::with_capacity(len);
    let mut services = Vec::with_capacity(len);
    let mut kinds = Vec::with_capacity(len);
    let mut statuses = Vec::with_capacity(len);
    let mut durations: Vec<Option<i32>> = Vec::with_capacity(len);
    let mut started_vec = Vec::with_capacity(len);
    let mut attrs: Vec<Option<String>> = Vec::with_capacity(len);

    for row in rows {
        trace_ids.push(row.trace_id.clone());
        span_ids.push(row.span_id.clone());
        parent_ids.push(row.parent_span_id.clone());
        names.push(row.name.clone());
        services.push(row.service.clone());
        kinds.push(row.kind.clone());
        statuses.push(row.status.clone());
        durations.push(row.duration_ms);
        started_vec.push(row.started_at.timestamp_micros());
        attrs.push(row.attributes.as_ref().map(ToString::to_string));
    }

    let schema = span_schema();
    let columns: Vec<Arc<dyn arrow::array::Array>> = vec![
        Arc::new(StringArray::from(trace_ids)),
        Arc::new(StringArray::from(span_ids)),
        Arc::new(StringArray::from(parent_ids)),
        Arc::new(StringArray::from(names)),
        Arc::new(StringArray::from(services)),
        Arc::new(StringArray::from(kinds)),
        Arc::new(StringArray::from(statuses)),
        Arc::new(Int32Array::from(durations)),
        Arc::new(TimestampMicrosecondArray::from(started_vec).with_timezone("UTC")),
        Arc::new(StringArray::from(attrs)),
    ];

    Ok(RecordBatch::try_new(schema, columns)?)
}

// ---------------------------------------------------------------------------
// Metric rotation
// ---------------------------------------------------------------------------

/// Rotate metric samples older than 1h to `MinIO` Parquet.
#[tracing::instrument(skip(state), err)]
pub async fn rotate_metrics(state: &AppState) -> Result<u64, ObserveError> {
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(1);

    let rows = sqlx::query(
        r"
        SELECT ms.series_id, ms.timestamp, ms.value,
               ser.name, ser.labels
        FROM metric_samples ms
        JOIN metric_series ser ON ser.id = ms.series_id
        WHERE ms.timestamp < $1
        ORDER BY ms.timestamp ASC
        LIMIT 10000
        ",
    )
    .bind(cutoff)
    .fetch_all(&state.pool)
    .await?;

    if rows.is_empty() {
        return Ok(0);
    }

    let count = rows.len() as u64;

    let typed: Vec<MetricSampleRow> = rows
        .iter()
        .map(|r| MetricSampleRow {
            name: r.get("name"),
            labels: r.get("labels"),
            series_id: r.get("series_id"),
            timestamp: r.get("timestamp"),
            value: r.get("value"),
        })
        .collect();

    let batch = build_metric_batch(&typed)?;
    let parquet_bytes = write_parquet_buffer(&batch)?;

    let date = cutoff.format("%Y-%m-%d");
    let batch_id = Uuid::new_v4();
    let path = format!("otel/metrics/{date}/metrics_{batch_id}.parquet");
    state.minio.write(&path, parquet_bytes).await?;

    // Delete rotated samples
    let series_ids: Vec<Uuid> = typed.iter().map(|r| r.series_id).collect();
    let timestamps: Vec<chrono::DateTime<chrono::Utc>> =
        typed.iter().map(|r| r.timestamp).collect();

    sqlx::query(
        r"
        DELETE FROM metric_samples ms
        USING (SELECT * FROM UNNEST($1::uuid[], $2::timestamptz[]) AS t(s, ts)) v
        WHERE ms.series_id = v.s AND ms.timestamp = v.ts
        ",
    )
    .bind(&series_ids)
    .bind(&timestamps)
    .execute(&state.pool)
    .await?;

    tracing::info!(count, path = %path, "rotated metrics to parquet");
    Ok(count)
}

fn metric_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("name", DataType::Utf8, false),
        Field::new("labels", DataType::Utf8, false),
        Field::new(
            "timestamp",
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            false,
        ),
        Field::new("value", DataType::Float64, false),
    ]))
}

struct MetricSampleRow {
    name: String,
    labels: serde_json::Value,
    series_id: Uuid,
    timestamp: chrono::DateTime<chrono::Utc>,
    value: f64,
}

fn build_metric_batch(rows: &[MetricSampleRow]) -> Result<RecordBatch, ObserveError> {
    let len = rows.len();
    let mut names = Vec::with_capacity(len);
    let mut labels = Vec::with_capacity(len);
    let mut timestamps = Vec::with_capacity(len);
    let mut values = Vec::with_capacity(len);

    for row in rows {
        names.push(row.name.clone());
        labels.push(row.labels.to_string());
        timestamps.push(row.timestamp.timestamp_micros());
        values.push(row.value);
    }

    let schema = metric_schema();
    let columns: Vec<Arc<dyn arrow::array::Array>> = vec![
        Arc::new(StringArray::from(names)),
        Arc::new(StringArray::from(labels)),
        Arc::new(TimestampMicrosecondArray::from(timestamps).with_timezone("UTC")),
        Arc::new(Float64Array::from(values)),
    ];

    Ok(RecordBatch::try_new(schema, columns)?)
}

// ---------------------------------------------------------------------------
// Parquet writer
// ---------------------------------------------------------------------------

fn write_parquet_buffer(batch: &RecordBatch) -> Result<Vec<u8>, ObserveError> {
    let mut buf = Vec::new();
    let props = parquet::file::properties::WriterProperties::builder()
        .set_compression(parquet::basic::Compression::SNAPPY)
        .build();
    let mut writer = ArrowWriter::try_new(&mut buf, batch.schema(), Some(props))?;
    writer.write(batch)?;
    writer.close()?;
    Ok(buf)
}
