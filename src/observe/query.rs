use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use fred::prelude::*;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::auth::middleware::AuthUser;
use crate::error::ApiError;
use crate::rbac::{Permission, resolver};
use crate::store::AppState;
use crate::validation;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ListResponse<T: Serialize> {
    pub items: Vec<T>,
    pub total: i64,
}

// --- Log types ---

#[derive(Debug, Deserialize)]
pub struct LogSearchParams {
    pub project_id: Option<Uuid>,
    pub session_id: Option<Uuid>,
    pub trace_id: Option<String>,
    pub level: Option<String>,
    pub service: Option<String>,
    pub q: Option<String>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct LogEntryResponse {
    pub id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub trace_id: Option<String>,
    pub span_id: Option<String>,
    pub project_id: Option<Uuid>,
    pub session_id: Option<Uuid>,
    pub service: String,
    pub level: String,
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

#[derive(Debug, Serialize)]
pub struct TraceSummaryResponse {
    pub trace_id: String,
    pub root_span: String,
    pub service: String,
    pub status: String,
    pub duration_ms: Option<i32>,
    pub started_at: DateTime<Utc>,
    pub project_id: Option<Uuid>,
}

#[derive(Debug, Serialize)]
pub struct TraceDetailResponse {
    pub trace_id: String,
    pub root_span: String,
    pub service: String,
    pub status: String,
    pub duration_ms: Option<i32>,
    pub started_at: DateTime<Utc>,
    pub spans: Vec<SpanResponse>,
}

#[derive(Debug, Serialize)]
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
    pub attributes: Option<serde_json::Value>,
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
    #[serde(rename = "step")]
    _step: Option<i64>,
    #[serde(rename = "agg")]
    _agg: Option<String>,
    pub limit: Option<i64>,
    #[serde(rename = "offset")]
    _offset: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct MetricDataPoint {
    pub timestamp: DateTime<Utc>,
    pub value: f64,
}

#[derive(Debug, Serialize)]
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
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/observe/logs", get(search_logs))
        .route("/api/observe/logs/tail", get(live_tail_ws))
        .route("/api/observe/traces", get(list_traces))
        .route("/api/observe/traces/{trace_id}", get(get_trace))
        .route("/api/observe/metrics", get(query_metrics))
        .route("/api/observe/metrics/names", get(list_metric_names))
        .route(
            "/api/observe/sessions/{session_id}/timeline",
            get(session_timeline),
        )
}

// ---------------------------------------------------------------------------
// Permission helper
// ---------------------------------------------------------------------------

async fn require_observe_read(
    state: &AppState,
    auth: &AuthUser,
    project_id: Option<Uuid>,
) -> Result<(), ApiError> {
    let allowed = resolver::has_permission(
        &state.pool,
        &state.valkey,
        auth.user_id,
        None,
        Permission::ObserveRead,
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
    let project =
        sqlx::query("SELECT visibility, owner_id FROM projects WHERE id = $1 AND is_active = true")
            .bind(project_id)
            .fetch_optional(&state.pool)
            .await?
            .ok_or_else(|| ApiError::NotFound("project".into()))?;

    let visibility: String = project.get("visibility");
    let owner_id: Uuid = project.get("owner_id");

    if visibility == "public" || visibility == "internal" || owner_id == auth.user_id {
        return Ok(());
    }

    let allowed = resolver::has_permission(
        &state.pool,
        &state.valkey,
        auth.user_id,
        Some(project_id),
        Permission::ProjectRead,
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
    require_observe_read(&state, &auth, params.project_id).await?;

    if let Some(ref q) = params.q {
        validation::check_length("q", q, 1, 1000)?;
    }

    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);
    let search_pattern = params.q.as_deref().map(|s| format!("%{s}%"));

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
        ",
    )
    .bind(params.project_id)
    .bind(params.session_id)
    .bind(params.trace_id.as_deref())
    .bind(params.level.as_deref())
    .bind(params.service.as_deref())
    .bind(search_pattern.as_deref())
    .bind(params.from)
    .bind(params.to)
    .fetch_one(&state.pool)
    .await?;

    let rows = sqlx::query(
        r"
        SELECT id, timestamp, trace_id, span_id, project_id, session_id,
               service, level, message, attributes
        FROM log_entries
        WHERE ($1::uuid IS NULL OR project_id = $1)
          AND ($2::uuid IS NULL OR session_id = $2)
          AND ($3::text IS NULL OR trace_id = $3)
          AND ($4::text IS NULL OR level = $4)
          AND ($5::text IS NULL OR service = $5)
          AND ($6::text IS NULL OR message ILIKE $6)
          AND ($7::timestamptz IS NULL OR timestamp >= $7)
          AND ($8::timestamptz IS NULL OR timestamp <= $8)
        ORDER BY timestamp DESC
        LIMIT $9 OFFSET $10
        ",
    )
    .bind(params.project_id)
    .bind(params.session_id)
    .bind(params.trace_id.as_deref())
    .bind(params.level.as_deref())
    .bind(params.service.as_deref())
    .bind(search_pattern.as_deref())
    .bind(params.from)
    .bind(params.to)
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
) -> Result<Json<Vec<MetricDataPoint>>, ApiError> {
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

    let rows = sqlx::query(
        r"
        SELECT ms.timestamp, ms.value
        FROM metric_samples ms
        JOIN metric_series ser ON ser.id = ms.series_id
        WHERE ser.name = $1
          AND ($2::jsonb IS NULL OR ser.labels @> $2)
          AND ($3::uuid IS NULL OR ser.project_id = $3)
          AND ($4::timestamptz IS NULL OR ms.timestamp >= $4)
          AND ($5::timestamptz IS NULL OR ms.timestamp <= $5)
        ORDER BY ms.timestamp ASC
        LIMIT $6
        ",
    )
    .bind(name)
    .bind(&labels_filter)
    .bind(params.project_id)
    .bind(params.from)
    .bind(params.to)
    .bind(limit)
    .fetch_all(&state.pool)
    .await?;

    let items = rows
        .into_iter()
        .map(|r| MetricDataPoint {
            timestamp: r.get("timestamp"),
            value: r.get("value"),
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
// Live tail WebSocket
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state, ws), err)]
async fn live_tail_ws(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(params): Query<LiveTailParams>,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, ApiError> {
    let project_id = params
        .project_id
        .ok_or_else(|| ApiError::BadRequest("project_id required for live tail".into()))?;

    require_observe_read(&state, &auth, Some(project_id)).await?;

    Ok(ws.on_upgrade(move |socket| handle_live_tail(socket, state, project_id, params)))
}

async fn handle_live_tail(
    mut socket: WebSocket,
    state: AppState,
    project_id: Uuid,
    params: LiveTailParams,
) {
    let subscriber = state.valkey.next().clone();
    let channel = format!("logs:{project_id}");

    if subscriber.subscribe(channel.as_str()).await.is_err() {
        let _ = socket.send(Message::Close(None)).await;
        return;
    }

    let mut rx = subscriber.message_rx();

    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Ok(message) => {
                        if let Ok(text) = message.value.convert::<String>()
                            && should_forward(&text, &params)
                            && socket.send(Message::Text(text.into())).await.is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            ws_msg = socket.recv() => {
                match ws_msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }

    let _ = subscriber.unsubscribe(channel.as_str()).await;
}

/// Check if a live tail message matches optional level/service filters.
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

    true
}
