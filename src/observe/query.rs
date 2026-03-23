use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use fred::interfaces::EventInterface;
use fred::interfaces::PubsubInterface;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tokio_stream::StreamExt;
use uuid::Uuid;

use ts_rs::TS;

use crate::api::helpers::ListResponse;
use crate::auth::middleware::AuthUser;
use crate::error::ApiError;
use crate::rbac::{Permission, resolver};
use crate::store::AppState;
use crate::validation;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

// --- Log types ---

#[derive(Debug, Deserialize)]
pub struct LogSearchParams {
    pub project_id: Option<Uuid>,
    pub session_id: Option<Uuid>,
    pub trace_id: Option<String>,
    pub level: Option<String>,
    pub service: Option<String>,
    pub source: Option<String>,
    pub task_name: Option<String>,
    pub q: Option<String>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    /// Relative time range like "1h", "6h", "24h", "7d". Converted to `from`.
    pub range: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize, TS)]
#[ts(export, rename = "LogEntry")]
pub struct LogEntryResponse {
    pub id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub trace_id: Option<String>,
    pub span_id: Option<String>,
    pub project_id: Option<Uuid>,
    pub session_id: Option<Uuid>,
    pub service: String,
    pub level: String,
    pub source: String,
    pub message: String,
    pub attributes: Option<serde_json::Value>,
}

// --- Trace types ---

#[derive(Debug, Deserialize)]
pub struct TraceListParams {
    pub project_id: Option<Uuid>,
    pub session_id: Option<Uuid>,
    pub service: Option<String>,
    pub status: Option<String>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize, TS)]
#[ts(export, rename = "TraceSummary")]
pub struct TraceSummaryResponse {
    pub trace_id: String,
    pub root_span: String,
    pub service: String,
    pub status: String,
    pub duration_ms: Option<i32>,
    pub started_at: DateTime<Utc>,
    pub project_id: Option<Uuid>,
}

#[derive(Debug, Serialize, TS)]
#[ts(export, rename = "TraceDetail")]
pub struct TraceDetailResponse {
    pub trace_id: String,
    pub root_span: String,
    pub service: String,
    pub status: String,
    pub duration_ms: Option<i32>,
    pub started_at: DateTime<Utc>,
    pub spans: Vec<SpanResponse>,
}

#[derive(Debug, Serialize, TS)]
#[ts(export, rename = "Span")]
pub struct SpanResponse {
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub name: String,
    pub service: String,
    pub kind: String,
    pub status: String,
    pub duration_ms: Option<i32>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    #[ts(type = "Record<string, any> | null")]
    pub attributes: Option<serde_json::Value>,
    #[ts(
        type = "Array<{name: string, timestamp: string, attributes?: Record<string, any>}> | null"
    )]
    pub events: Option<serde_json::Value>,
}

// --- Metric types ---

#[derive(Debug, Deserialize)]
pub struct MetricQueryParams {
    pub name: Option<String>,
    pub labels: Option<String>,
    pub project_id: Option<Uuid>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    /// Relative time range like "1h", "6h", "24h", "7d". Converted to `from`.
    pub range: Option<String>,
    #[serde(rename = "step")]
    _step: Option<i64>,
    #[serde(rename = "agg")]
    _agg: Option<String>,
    pub limit: Option<i64>,
    #[serde(rename = "offset")]
    _offset: Option<i64>,
}

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct MetricDataPoint {
    pub timestamp: DateTime<Utc>,
    pub value: f64,
}

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct MetricSeries {
    pub name: String,
    pub labels: std::collections::HashMap<String, String>,
    pub points: Vec<MetricDataPoint>,
}

#[derive(Debug, Serialize, TS)]
#[ts(export, rename = "MetricName")]
pub struct MetricNameResponse {
    pub name: String,
    pub labels: serde_json::Value,
    pub metric_type: String,
    pub unit: Option<String>,
}

// --- Session types ---

#[derive(Debug, Serialize)]
pub struct TimelineEntry {
    pub timestamp: DateTime<Utc>,
    pub kind: String,
    pub service: String,
    pub message: String,
    pub trace_id: Option<String>,
    pub span_id: Option<String>,
    pub level: Option<String>,
}

// --- Live tail types ---

#[derive(Debug, Deserialize)]
pub struct LiveTailParams {
    pub project_id: Option<Uuid>,
    pub level: Option<String>,
    pub service: Option<String>,
    pub source: Option<String>,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/observe/logs", get(search_logs))
        .route("/api/observe/logs/tail", get(live_tail_sse))
        .route("/api/observe/traces", get(list_traces))
        .route("/api/observe/traces/{trace_id}", get(get_trace))
        .route("/api/observe/metrics", get(query_metrics))
        .route("/api/observe/metrics/query", get(query_metrics))
        .route("/api/observe/metrics/names", get(list_metric_names))
        .route(
            "/api/observe/sessions/{session_id}/timeline",
            get(session_timeline),
        )
        .route("/api/projects/{project_id}/logs", get(project_logs))
}

// ---------------------------------------------------------------------------
// Permission helper
// ---------------------------------------------------------------------------

async fn require_observe_read(
    state: &AppState,
    auth: &AuthUser,
    project_id: Option<Uuid>,
) -> Result<(), ApiError> {
    let allowed = resolver::has_permission_scoped(
        &state.pool,
        &state.valkey,
        auth.user_id,
        None,
        Permission::ObserveRead,
        auth.token_scopes.as_deref(),
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::Forbidden);
    }

    if let Some(pid) = project_id {
        require_project_read(state, auth, pid).await?;
    }
    Ok(())
}

async fn require_project_read(
    state: &AppState,
    auth: &AuthUser,
    project_id: Uuid,
) -> Result<(), ApiError> {
    // Enforce hard project scope from API token
    auth.check_project_scope(project_id)?;

    let project = sqlx::query(
        "SELECT visibility, owner_id, workspace_id FROM projects WHERE id = $1 AND is_active = true",
    )
    .bind(project_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("project".into()))?;

    let visibility: String = project.get("visibility");
    let owner_id: Uuid = project.get("owner_id");

    // Enforce hard workspace boundary from API token
    if let Some(scope_wid) = auth.boundary_workspace_id {
        let ws_id: Uuid = project.get("workspace_id");
        if ws_id != scope_wid {
            return Err(ApiError::NotFound("project".into()));
        }
    }

    if visibility == "public" || visibility == "internal" || owner_id == auth.user_id {
        return Ok(());
    }

    let allowed = resolver::has_permission_scoped(
        &state.pool,
        &state.valkey,
        auth.user_id,
        Some(project_id),
        Permission::ProjectRead,
        auth.token_scopes.as_deref(),
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::NotFound("project".into()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Log search
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state), err)]
async fn search_logs(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(params): Query<LogSearchParams>,
) -> Result<Json<ListResponse<LogEntryResponse>>, ApiError> {
    search_logs_inner(&state, &auth, params).await
}

#[tracing::instrument(skip(state), err)]
async fn project_logs(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(project_id): Path<Uuid>,
    Query(mut params): Query<LogSearchParams>,
) -> Result<Json<ListResponse<LogEntryResponse>>, ApiError> {
    require_project_read(&state, &auth, project_id).await?;
    params.project_id = Some(project_id);
    search_logs_inner(&state, &auth, params).await
}

/// Resolve a relative time range string (e.g. "1h", "7d") to an absolute `from` timestamp.
fn resolve_range(
    explicit_from: Option<DateTime<Utc>>,
    range: Option<&str>,
) -> Option<DateTime<Utc>> {
    explicit_from.or_else(|| {
        range.and_then(|r| {
            let secs = match r {
                "1h" => 3600,
                "6h" => 21600,
                "12h" => 43200,
                "24h" | "1d" => 86400,
                "7d" => 604_800,
                "30d" => 2_592_000,
                _ => return None,
            };
            Some(Utc::now() - chrono::Duration::seconds(secs))
        })
    })
}

async fn search_logs_inner(
    state: &AppState,
    auth: &AuthUser,
    params: LogSearchParams,
) -> Result<Json<ListResponse<LogEntryResponse>>, ApiError> {
    require_observe_read(state, auth, params.project_id).await?;

    if let Some(ref q) = params.q {
        validation::check_length("q", q, 1, 1000)?;
    }

    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);
    let search_pattern = params.q.as_deref().map(|s| format!("%{s}%"));
    let from = resolve_range(params.from, params.range.as_deref());

    let total: i64 = sqlx::query_scalar(
        r"
        SELECT COUNT(*)
        FROM log_entries
        WHERE ($1::uuid IS NULL OR project_id = $1)
          AND ($2::uuid IS NULL OR session_id = $2)
          AND ($3::text IS NULL OR trace_id = $3)
          AND ($4::text IS NULL OR level = $4)
          AND ($5::text IS NULL OR service = $5)
          AND ($6::text IS NULL OR message ILIKE $6)
          AND ($7::timestamptz IS NULL OR timestamp >= $7)
          AND ($8::timestamptz IS NULL OR timestamp <= $8)
          AND ($9::text IS NULL OR source = $9)
          AND ($10::text IS NULL OR attributes->>'task_name' = $10)
        ",
    )
    .bind(params.project_id)
    .bind(params.session_id)
    .bind(params.trace_id.as_deref())
    .bind(params.level.as_deref())
    .bind(params.service.as_deref())
    .bind(search_pattern.as_deref())
    .bind(from)
    .bind(params.to)
    .bind(params.source.as_deref())
    .bind(params.task_name.as_deref())
    .fetch_one(&state.pool)
    .await?;

    let rows = sqlx::query(
        r"
        SELECT id, timestamp, trace_id, span_id, project_id, session_id,
               service, level, source, message, attributes
        FROM log_entries
        WHERE ($1::uuid IS NULL OR project_id = $1)
          AND ($2::uuid IS NULL OR session_id = $2)
          AND ($3::text IS NULL OR trace_id = $3)
          AND ($4::text IS NULL OR level = $4)
          AND ($5::text IS NULL OR service = $5)
          AND ($6::text IS NULL OR message ILIKE $6)
          AND ($7::timestamptz IS NULL OR timestamp >= $7)
          AND ($8::timestamptz IS NULL OR timestamp <= $8)
          AND ($9::text IS NULL OR source = $9)
          AND ($10::text IS NULL OR attributes->>'task_name' = $10)
        ORDER BY timestamp DESC
        LIMIT $11 OFFSET $12
        ",
    )
    .bind(params.project_id)
    .bind(params.session_id)
    .bind(params.trace_id.as_deref())
    .bind(params.level.as_deref())
    .bind(params.service.as_deref())
    .bind(search_pattern.as_deref())
    .bind(from)
    .bind(params.to)
    .bind(params.source.as_deref())
    .bind(params.task_name.as_deref())
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.pool)
    .await?;

    let items = rows
        .into_iter()
        .map(|r| LogEntryResponse {
            id: r.get("id"),
            timestamp: r.get("timestamp"),
            trace_id: r.get("trace_id"),
            span_id: r.get("span_id"),
            project_id: r.get("project_id"),
            session_id: r.get("session_id"),
            service: r.get("service"),
            level: r.get("level"),
            source: r.get("source"),
            message: r.get("message"),
            attributes: r.get("attributes"),
        })
        .collect();

    Ok(Json(ListResponse { items, total }))
}

// ---------------------------------------------------------------------------
// Trace list / detail
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state), err)]
async fn list_traces(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(params): Query<TraceListParams>,
) -> Result<Json<ListResponse<TraceSummaryResponse>>, ApiError> {
    require_observe_read(&state, &auth, params.project_id).await?;

    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);

    let total: i64 = sqlx::query_scalar(
        r"
        SELECT COUNT(*)
        FROM traces
        WHERE ($1::uuid IS NULL OR project_id = $1)
          AND ($2::uuid IS NULL OR session_id = $2)
          AND ($3::text IS NULL OR service = $3)
          AND ($4::text IS NULL OR status = $4)
          AND ($5::timestamptz IS NULL OR started_at >= $5)
          AND ($6::timestamptz IS NULL OR started_at <= $6)
        ",
    )
    .bind(params.project_id)
    .bind(params.session_id)
    .bind(params.service.as_deref())
    .bind(params.status.as_deref())
    .bind(params.from)
    .bind(params.to)
    .fetch_one(&state.pool)
    .await?;

    let rows = sqlx::query(
        r"
        SELECT trace_id, root_span, service, status, duration_ms, started_at, project_id
        FROM traces
        WHERE ($1::uuid IS NULL OR project_id = $1)
          AND ($2::uuid IS NULL OR session_id = $2)
          AND ($3::text IS NULL OR service = $3)
          AND ($4::text IS NULL OR status = $4)
          AND ($5::timestamptz IS NULL OR started_at >= $5)
          AND ($6::timestamptz IS NULL OR started_at <= $6)
        ORDER BY started_at DESC
        LIMIT $7 OFFSET $8
        ",
    )
    .bind(params.project_id)
    .bind(params.session_id)
    .bind(params.service.as_deref())
    .bind(params.status.as_deref())
    .bind(params.from)
    .bind(params.to)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.pool)
    .await?;

    let items = rows
        .into_iter()
        .map(|r| TraceSummaryResponse {
            trace_id: r.get("trace_id"),
            root_span: r.get("root_span"),
            service: r.get("service"),
            status: r.get("status"),
            duration_ms: r.get("duration_ms"),
            started_at: r.get("started_at"),
            project_id: r.get("project_id"),
        })
        .collect();

    Ok(Json(ListResponse { items, total }))
}

#[tracing::instrument(skip(state), err)]
async fn get_trace(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(trace_id): Path<String>,
) -> Result<Json<TraceDetailResponse>, ApiError> {
    let trace = sqlx::query(
        r"
        SELECT trace_id, root_span, service, status, duration_ms, started_at, project_id
        FROM traces WHERE trace_id = $1
        ",
    )
    .bind(&trace_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("trace".into()))?;

    let trace_project_id: Option<Uuid> = trace.get("project_id");
    require_observe_read(&state, &auth, trace_project_id).await?;

    let spans = sqlx::query(
        r"
        SELECT span_id, parent_span_id, name, service, kind, status,
               duration_ms, started_at, finished_at, attributes, events
        FROM spans WHERE trace_id = $1
        ORDER BY started_at ASC
        ",
    )
    .bind(&trace_id)
    .fetch_all(&state.pool)
    .await?;

    let span_responses = spans
        .into_iter()
        .map(|s| SpanResponse {
            span_id: s.get("span_id"),
            parent_span_id: s.get("parent_span_id"),
            name: s.get("name"),
            service: s.get("service"),
            kind: s.get("kind"),
            status: s.get("status"),
            duration_ms: s.get("duration_ms"),
            started_at: s.get("started_at"),
            finished_at: s.get("finished_at"),
            attributes: s.get("attributes"),
            events: s.get("events"),
        })
        .collect();

    Ok(Json(TraceDetailResponse {
        trace_id: trace.get("trace_id"),
        root_span: trace.get("root_span"),
        service: trace.get("service"),
        status: trace.get("status"),
        duration_ms: trace.get("duration_ms"),
        started_at: trace.get("started_at"),
        spans: span_responses,
    }))
}

// ---------------------------------------------------------------------------
// Metric query
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state), err)]
async fn query_metrics(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(params): Query<MetricQueryParams>,
) -> Result<Json<Vec<MetricSeries>>, ApiError> {
    require_observe_read(&state, &auth, params.project_id).await?;

    let name = params
        .name
        .as_deref()
        .ok_or_else(|| ApiError::BadRequest("name is required".into()))?;
    validation::check_length("name", name, 1, 255)?;

    let labels_filter: Option<serde_json::Value> = params
        .labels
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|_| ApiError::BadRequest("invalid labels JSON".into()))?;

    let limit = params.limit.unwrap_or(1000).min(10_000);

    let from = resolve_range(params.from, params.range.as_deref());

    let rows = sqlx::query(
        r"
        SELECT ser.id as series_id, ser.labels, ms.timestamp, ms.value
        FROM metric_samples ms
        JOIN metric_series ser ON ser.id = ms.series_id
        WHERE ser.name = $1
          AND ($2::jsonb IS NULL OR ser.labels @> $2)
          AND ($3::uuid IS NULL OR ser.project_id = $3)
          AND ($4::timestamptz IS NULL OR ms.timestamp >= $4)
          AND ($5::timestamptz IS NULL OR ms.timestamp <= $5)
        ORDER BY ser.id, ms.timestamp ASC
        LIMIT $6
        ",
    )
    .bind(name)
    .bind(&labels_filter)
    .bind(params.project_id)
    .bind(from)
    .bind(params.to)
    .bind(limit)
    .fetch_all(&state.pool)
    .await?;

    // Group by series_id
    let mut series_map: std::collections::HashMap<Uuid, (serde_json::Value, Vec<MetricDataPoint>)> =
        std::collections::HashMap::new();
    for r in &rows {
        let series_id: Uuid = r.get("series_id");
        let entry = series_map.entry(series_id).or_insert_with(|| {
            let labels: serde_json::Value = r.get("labels");
            (labels, Vec::new())
        });
        entry.1.push(MetricDataPoint {
            timestamp: r.get("timestamp"),
            value: r.get("value"),
        });
    }

    let items: Vec<MetricSeries> = series_map
        .into_values()
        .map(|(labels_json, points)| {
            let labels = match labels_json {
                serde_json::Value::Object(m) => m
                    .into_iter()
                    .map(|(k, v)| (k, v.as_str().unwrap_or_default().to_string()))
                    .collect(),
                _ => std::collections::HashMap::new(),
            };
            MetricSeries {
                name: name.to_string(),
                labels,
                points,
            }
        })
        .collect();

    Ok(Json(items))
}

#[tracing::instrument(skip(state), err)]
async fn list_metric_names(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(params): Query<MetricQueryParams>,
) -> Result<Json<Vec<MetricNameResponse>>, ApiError> {
    require_observe_read(&state, &auth, params.project_id).await?;

    let limit = params.limit.unwrap_or(100).min(1000);

    let rows = sqlx::query(
        r"
        SELECT name, labels, metric_type, unit
        FROM metric_series
        WHERE ($1::uuid IS NULL OR project_id = $1)
        ORDER BY name ASC
        LIMIT $2
        ",
    )
    .bind(params.project_id)
    .bind(limit)
    .fetch_all(&state.pool)
    .await?;

    let items = rows
        .into_iter()
        .map(|r| MetricNameResponse {
            name: r.get("name"),
            labels: r.get("labels"),
            metric_type: r.get("metric_type"),
            unit: r.get("unit"),
        })
        .collect();

    Ok(Json(items))
}

// ---------------------------------------------------------------------------
// Session timeline
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state), err)]
async fn session_timeline(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(session_id): Path<Uuid>,
) -> Result<Json<Vec<TimelineEntry>>, ApiError> {
    // Look up project for this session
    let session = sqlx::query("SELECT project_id FROM agent_sessions WHERE id = $1")
        .bind(session_id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| ApiError::NotFound("session".into()))?;

    let session_project_id: Uuid = session.get("project_id");
    require_observe_read(&state, &auth, Some(session_project_id)).await?;

    let logs = sqlx::query(
        r"
        SELECT timestamp, service, level, message, trace_id, span_id
        FROM log_entries WHERE session_id = $1
        ORDER BY timestamp ASC LIMIT 1000
        ",
    )
    .bind(session_id)
    .fetch_all(&state.pool)
    .await?;

    let spans = sqlx::query(
        r"
        SELECT s.started_at as timestamp, s.service, s.name, s.trace_id, s.span_id
        FROM spans s
        JOIN traces t ON t.trace_id = s.trace_id
        WHERE t.session_id = $1
        ORDER BY s.started_at ASC LIMIT 1000
        ",
    )
    .bind(session_id)
    .fetch_all(&state.pool)
    .await?;

    let mut entries: Vec<TimelineEntry> = Vec::new();

    for log in logs {
        entries.push(TimelineEntry {
            timestamp: log.get("timestamp"),
            kind: "log".into(),
            service: log.get("service"),
            message: log.get("message"),
            trace_id: log.get("trace_id"),
            span_id: log.get("span_id"),
            level: Some(log.get("level")),
        });
    }

    for span in spans {
        entries.push(TimelineEntry {
            timestamp: span.get("timestamp"),
            kind: "span".into(),
            service: span.get("service"),
            message: span.get("name"),
            trace_id: Some(span.get("trace_id")),
            span_id: Some(span.get("span_id")),
            level: None,
        });
    }

    entries.sort_by_key(|e| e.timestamp);

    Ok(Json(entries))
}

// ---------------------------------------------------------------------------
// Live tail SSE
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state), err)]
async fn live_tail_sse(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(params): Query<LiveTailParams>,
) -> Result<impl IntoResponse, ApiError> {
    let project_id = params
        .project_id
        .ok_or_else(|| ApiError::BadRequest("project_id required for live tail".into()))?;

    require_observe_read(&state, &auth, Some(project_id)).await?;

    let channel = format!("logs:{project_id}");

    // Dedicated subscriber connection for this SSE stream.
    let subscriber = state.valkey.next().clone_new();
    subscriber
        .subscribe(&channel)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    let mut msg_rx = subscriber.message_rx();

    let (tx, rx) = tokio::sync::mpsc::channel::<String>(256);

    tokio::spawn(async move {
        while let Ok(msg) = msg_rx.recv().await {
            let text: String = match msg.value.convert() {
                Ok(s) => s,
                Err(_) => continue,
            };
            if should_forward(&text, &params) && tx.send(text).await.is_err() {
                break;
            }
        }
        let _ = subscriber.unsubscribe(&channel).await;
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx)
        .map(|text| Ok::<_, std::convert::Infallible>(Event::default().event("log").data(text)));

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// Check if a live tail message matches optional level/service/source filters.
fn should_forward(text: &str, params: &LiveTailParams) -> bool {
    let Ok(msg) = serde_json::from_str::<serde_json::Value>(text) else {
        return true;
    };

    if let Some(ref level) = params.level
        && msg.get("level").and_then(|v| v.as_str()) != Some(level)
    {
        return false;
    }

    if let Some(ref service) = params.service
        && msg.get("service").and_then(|v| v.as_str()) != Some(service)
    {
        return false;
    }

    if let Some(ref source) = params.source
        && msg.get("source").and_then(|v| v.as_str()) != Some(source)
    {
        return false;
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(level: Option<&str>, service: Option<&str>) -> LiveTailParams {
        LiveTailParams {
            project_id: None,
            level: level.map(String::from),
            service: service.map(String::from),
            source: None,
        }
    }

    fn params_with_source(
        level: Option<&str>,
        service: Option<&str>,
        source: Option<&str>,
    ) -> LiveTailParams {
        LiveTailParams {
            project_id: None,
            level: level.map(String::from),
            service: service.map(String::from),
            source: source.map(String::from),
        }
    }

    #[test]
    fn should_forward_no_filters() {
        let p = params(None, None);
        assert!(should_forward(r#"{"level":"error","service":"api"}"#, &p));
    }

    #[test]
    fn should_forward_level_match() {
        let p = params(Some("error"), None);
        assert!(should_forward(r#"{"level":"error","service":"api"}"#, &p));
    }

    #[test]
    fn should_forward_level_mismatch() {
        let p = params(Some("error"), None);
        assert!(!should_forward(r#"{"level":"info","service":"api"}"#, &p));
    }

    #[test]
    fn should_forward_service_match() {
        let p = params(None, Some("api"));
        assert!(should_forward(r#"{"level":"info","service":"api"}"#, &p));
    }

    #[test]
    fn should_forward_service_mismatch() {
        let p = params(None, Some("worker"));
        assert!(!should_forward(r#"{"level":"info","service":"api"}"#, &p));
    }

    #[test]
    fn should_forward_invalid_json() {
        let p = params(Some("error"), None);
        assert!(should_forward("not json", &p));
    }

    #[test]
    fn should_forward_combined_filters() {
        let p = params(Some("error"), Some("api"));
        assert!(should_forward(r#"{"level":"error","service":"api"}"#, &p));
        assert!(!should_forward(r#"{"level":"info","service":"api"}"#, &p));
        assert!(!should_forward(
            r#"{"level":"error","service":"worker"}"#,
            &p
        ));
    }

    #[test]
    fn should_forward_source_match() {
        let p = params_with_source(None, None, Some("session"));
        assert!(should_forward(
            r#"{"level":"info","service":"api","source":"session"}"#,
            &p
        ));
    }

    #[test]
    fn should_forward_source_mismatch() {
        let p = params_with_source(None, None, Some("session"));
        assert!(!should_forward(
            r#"{"level":"info","service":"api","source":"external"}"#,
            &p
        ));
    }

    #[test]
    fn should_forward_source_combined() {
        let p = params_with_source(Some("error"), None, Some("system"));
        assert!(should_forward(
            r#"{"level":"error","service":"api","source":"system"}"#,
            &p
        ));
        assert!(!should_forward(
            r#"{"level":"error","service":"api","source":"external"}"#,
            &p
        ));
    }
}
