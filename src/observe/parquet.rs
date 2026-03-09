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
    state.task_registry.register("parquet_rotation", 1800);

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                tracing::info!("parquet rotation shutting down");
                break;
            }
            () = tokio::time::sleep(std::time::Duration::from_secs(900)) => {
                let mut had_error = false;
                if let Err(e) = rotate_logs(&state).await {
                    state.task_registry.report_error("parquet_rotation", &e.to_string());
                    tracing::error!(error = %e, "log rotation failed");
                    had_error = true;
                }
                if let Err(e) = rotate_spans(&state).await {
                    state.task_registry.report_error("parquet_rotation", &e.to_string());
                    tracing::error!(error = %e, "span rotation failed");
                    had_error = true;
                }
                if let Err(e) = rotate_metrics(&state).await {
                    state.task_registry.report_error("parquet_rotation", &e.to_string());
                    tracing::error!(error = %e, "metric rotation failed");
                    had_error = true;
                }
                if !had_error {
                    state.task_registry.heartbeat("parquet_rotation");
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

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::datatypes::{DataType, TimeUnit};
    use chrono::Utc;

    // ── Schema tests ────────────────────────────────────────────────

    #[test]
    fn log_schema_has_10_fields() {
        let schema = log_schema();
        assert_eq!(schema.fields().len(), 10);
        assert_eq!(schema.field(0).name(), "id");
        assert_eq!(
            *schema.field(1).data_type(),
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
        );
        assert!(schema.field(2).is_nullable()); // trace_id
        assert!(!schema.field(0).is_nullable()); // id
    }

    #[test]
    fn span_schema_has_10_fields() {
        let schema = span_schema();
        assert_eq!(schema.fields().len(), 10);
        assert_eq!(schema.field(0).name(), "trace_id");
        assert!(!schema.field(0).is_nullable());
        assert!(schema.field(2).is_nullable()); // parent_span_id
        assert!(schema.field(7).is_nullable()); // duration_ms
    }

    #[test]
    fn metric_schema_has_4_fields() {
        let schema = metric_schema();
        assert_eq!(schema.fields().len(), 4);
        assert_eq!(schema.field(0).name(), "name");
        assert_eq!(*schema.field(3).data_type(), DataType::Float64);
    }

    // ── build_log_batch ─────────────────────────────────────────────

    fn sample_log_row() -> LogQueryRow {
        LogQueryRow {
            id: Uuid::new_v4(),
            timestamp: Utc::now(),
            trace_id: Some("trace-abc".into()),
            span_id: Some("span-def".into()),
            project_id: Some(Uuid::new_v4()),
            session_id: None,
            service: "my-svc".into(),
            level: "info".into(),
            message: "hello".into(),
            attributes: Some(serde_json::json!({"key": "val"})),
        }
    }

    #[test]
    fn build_log_batch_single_row() {
        let rows = vec![sample_log_row()];
        let batch = build_log_batch(&rows).unwrap();
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(batch.num_columns(), 10);
    }

    #[test]
    fn build_log_batch_multiple_rows() {
        let rows = vec![sample_log_row(), sample_log_row(), sample_log_row()];
        let batch = build_log_batch(&rows).unwrap();
        assert_eq!(batch.num_rows(), 3);
    }

    #[test]
    fn build_log_batch_nullable_fields() {
        let row = LogQueryRow {
            id: Uuid::new_v4(),
            timestamp: Utc::now(),
            trace_id: None,
            span_id: None,
            project_id: None,
            session_id: None,
            service: "svc".into(),
            level: "error".into(),
            message: "fail".into(),
            attributes: None,
        };
        let batch = build_log_batch(&[row]).unwrap();
        assert_eq!(batch.num_rows(), 1);
    }

    // ── build_span_batch ────────────────────────────────────────────

    fn sample_span_row() -> SpanQueryRow {
        SpanQueryRow {
            trace_id: "trace-001".into(),
            span_id: "span-001".into(),
            parent_span_id: Some("span-000".into()),
            name: "GET /api".into(),
            service: "api-svc".into(),
            kind: "server".into(),
            status: "ok".into(),
            duration_ms: Some(42),
            started_at: Utc::now(),
            attributes: Some(serde_json::json!({"http.status_code": 200})),
        }
    }

    #[test]
    fn build_span_batch_single_row() {
        let batch = build_span_batch(&[sample_span_row()]).unwrap();
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(batch.num_columns(), 10);
    }

    #[test]
    fn build_span_batch_optional_parent_none() {
        let mut row = sample_span_row();
        row.parent_span_id = None;
        let batch = build_span_batch(&[row]).unwrap();
        assert_eq!(batch.num_rows(), 1);
    }

    #[test]
    fn build_span_batch_optional_duration_none() {
        let mut row = sample_span_row();
        row.duration_ms = None;
        let batch = build_span_batch(&[row]).unwrap();
        assert_eq!(batch.num_rows(), 1);
    }

    // ── build_metric_batch ──────────────────────────────────────────

    fn sample_metric_row() -> MetricSampleRow {
        MetricSampleRow {
            name: "cpu_usage".into(),
            labels: serde_json::json!({"host": "node1"}),
            series_id: Uuid::new_v4(),
            timestamp: Utc::now(),
            value: 73.5,
        }
    }

    #[test]
    fn build_metric_batch_single_row() {
        let batch = build_metric_batch(&[sample_metric_row()]).unwrap();
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(batch.num_columns(), 4);
    }

    #[test]
    fn build_metric_batch_preserves_float_values() {
        let mut row = sample_metric_row();
        row.value = std::f64::consts::PI;
        let batch = build_metric_batch(&[row]).unwrap();
        let col = batch
            .column(3)
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        assert!((col.value(0) - std::f64::consts::PI).abs() < f64::EPSILON);
    }

    // ── write_parquet_buffer ────────────────────────────────────────

    #[test]
    fn write_parquet_produces_valid_bytes() {
        let batch = build_log_batch(&[sample_log_row()]).unwrap();
        let bytes = write_parquet_buffer(&batch).unwrap();
        assert!(!bytes.is_empty());
        // Parquet magic number: PAR1
        assert_eq!(&bytes[..4], b"PAR1");
    }
}
