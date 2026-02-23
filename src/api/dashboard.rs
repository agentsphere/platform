use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::middleware::AuthUser;
use crate::error::ApiError;
use crate::store::AppState;

use super::helpers::ListResponse;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct DashboardStats {
    pub projects: i64,
    pub active_sessions: i64,
    pub running_builds: i64,
    pub failed_builds: i64,
    pub healthy_deployments: i64,
    pub degraded_deployments: i64,
}

#[derive(Debug, Serialize)]
pub struct AuditLogEntry {
    pub id: Uuid,
    pub actor_id: Uuid,
    pub actor_name: String,
    pub action: String,
    pub resource: String,
    pub resource_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
    pub detail: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct AuditLogParams {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/dashboard/stats", get(dashboard_stats))
        .route("/api/audit-log", get(list_audit_log))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn dashboard_stats(
    State(state): State<AppState>,
    _auth: AuthUser,
) -> Result<Json<DashboardStats>, ApiError> {
    let projects = sqlx::query_scalar("SELECT COUNT(*) FROM projects WHERE is_active = true")
        .fetch_one(&state.pool)
        .await
        .unwrap_or(Some(0))
        .unwrap_or(0);

    let active_sessions = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_sessions WHERE status IN ('pending', 'running')",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap_or(Some(0))
    .unwrap_or(0);

    let running_builds =
        sqlx::query_scalar("SELECT COUNT(*) FROM pipeline_runs WHERE status = 'running'")
            .fetch_one(&state.pool)
            .await
            .unwrap_or(Some(0))
            .unwrap_or(0);

    let failed_builds = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pipeline_runs WHERE status = 'failure' AND created_at > now() - interval '24 hours'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap_or(Some(0))
    .unwrap_or(0);

    let healthy_deployments =
        sqlx::query_scalar("SELECT COUNT(*) FROM deployments WHERE current_status = 'healthy'")
            .fetch_one(&state.pool)
            .await
            .unwrap_or(Some(0))
            .unwrap_or(0);

    let degraded_deployments =
        sqlx::query_scalar("SELECT COUNT(*) FROM deployments WHERE current_status = 'degraded'")
            .fetch_one(&state.pool)
            .await
            .unwrap_or(Some(0))
            .unwrap_or(0);

    Ok(Json(DashboardStats {
        projects,
        active_sessions,
        running_builds,
        failed_builds,
        healthy_deployments,
        degraded_deployments,
    }))
}

async fn list_audit_log(
    State(state): State<AppState>,
    _auth: AuthUser,
    Query(params): Query<AuditLogParams>,
) -> Result<Json<ListResponse<AuditLogEntry>>, ApiError> {
    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);

    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log")
        .fetch_one(&state.pool)
        .await
        .unwrap_or(Some(0))
        .unwrap_or(0);

    let rows = sqlx::query_as::<_, (Uuid, Uuid, String, String, String, Option<Uuid>, Option<Uuid>, Option<serde_json::Value>, DateTime<Utc>)>(
        "SELECT id, actor_id, actor_name, action, resource, resource_id, project_id, detail, created_at FROM audit_log ORDER BY created_at DESC LIMIT $1 OFFSET $2",
    )
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.pool)
    .await?;

    let items = rows
        .into_iter()
        .map(
            |(
                id,
                actor_id,
                actor_name,
                action,
                resource,
                resource_id,
                project_id,
                detail,
                created_at,
            )| {
                AuditLogEntry {
                    id,
                    actor_id,
                    actor_name,
                    action,
                    resource,
                    resource_id,
                    project_id,
                    detail,
                    created_at,
                }
            },
        )
        .collect();

    Ok(Json(ListResponse { items, total }))
}
