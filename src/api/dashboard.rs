use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use ts_rs::TS;

use crate::auth::middleware::AuthUser;
use crate::error::ApiError;
use crate::store::AppState;

use super::helpers::ListResponse;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct DashboardStats {
    #[ts(type = "number")]
    pub projects: i64,
    #[ts(type = "number")]
    pub active_sessions: i64,
    #[ts(type = "number")]
    pub running_builds: i64,
    #[ts(type = "number")]
    pub failed_builds: i64,
    #[ts(type = "number")]
    pub healthy_deployments: i64,
    #[ts(type = "number")]
    pub degraded_deployments: i64,
}

#[derive(Debug, Serialize, TS)]
#[ts(export)]
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

#[derive(Debug, Serialize, TS)]
#[ts(export)]
#[allow(clippy::struct_excessive_bools)]
pub struct OnboardingStatus {
    pub has_projects: bool,
    pub has_provider_key: bool,
    pub has_cli_credentials: bool,
    pub needs_onboarding: bool,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/dashboard/stats", get(dashboard_stats))
        .route("/api/audit-log", get(list_audit_log))
        .route("/api/onboarding/status", get(onboarding_status))
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
        sqlx::query_scalar("SELECT COUNT(*) FROM deploy_releases WHERE health = 'healthy' AND phase NOT IN ('completed','rolled_back','cancelled','failed')")
            .fetch_one(&state.pool)
            .await
            .unwrap_or(Some(0))
            .unwrap_or(0);

    let degraded_deployments =
        sqlx::query_scalar("SELECT COUNT(*) FROM deploy_releases WHERE health = 'degraded' AND phase NOT IN ('completed','rolled_back','cancelled','failed')")
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

async fn onboarding_status(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<OnboardingStatus>, ApiError> {
    let project_count: Option<i64> = sqlx::query_scalar(
        "SELECT COUNT(*) FROM projects WHERE owner_id = $1 AND is_active = true",
    )
    .bind(auth.user_id)
    .fetch_one(&state.pool)
    .await
    .unwrap_or(Some(0));

    let key_count: Option<i64> = sqlx::query_scalar(
        "SELECT COUNT(*) FROM user_provider_keys WHERE user_id = $1 AND provider = 'anthropic'",
    )
    .bind(auth.user_id)
    .fetch_one(&state.pool)
    .await
    .unwrap_or(Some(0));

    let cli_creds_count: Option<i64> =
        sqlx::query_scalar("SELECT COUNT(*) FROM cli_credentials WHERE user_id = $1")
            .bind(auth.user_id)
            .fetch_one(&state.pool)
            .await
            .unwrap_or(Some(0));

    let has_cli_credentials = cli_creds_count.unwrap_or(0) > 0;
    let has_projects = project_count.unwrap_or(0) > 0;
    let has_provider_key = key_count.unwrap_or(0) > 0 || has_cli_credentials;

    Ok(Json(OnboardingStatus {
        has_projects,
        has_provider_key,
        has_cli_credentials,
        needs_onboarding: !has_projects,
    }))
}
