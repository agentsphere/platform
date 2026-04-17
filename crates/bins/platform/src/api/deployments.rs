// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

#[allow(dead_code, unused_imports)]
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::state::PlatformState;
use platform_auth::resolver;
use platform_types::ApiError;
use platform_types::AuthUser;
use platform_types::Permission;
use platform_types::validation;
use platform_types::{AuditEntry, send_audit};

use super::helpers::{require_admin, require_project_read};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct TargetResponse {
    pub id: Uuid,
    pub project_id: Uuid,
    pub name: String,
    pub environment: String,
    pub branch: Option<String>,
    pub branch_slug: Option<String>,
    pub ttl_hours: Option<i32>,
    pub expires_at: Option<DateTime<Utc>>,
    pub default_strategy: String,
    pub ops_repo_id: Option<Uuid>,
    pub manifest_path: Option<String>,
    pub hostname: Option<String>,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct ReleaseResponse {
    pub id: Uuid,
    pub target_id: Uuid,
    pub project_id: Uuid,
    pub image_ref: String,
    pub commit_sha: Option<String>,
    pub strategy: String,
    pub phase: String,
    pub traffic_weight: i32,
    pub health: String,
    pub current_step: i32,
    pub rollout_config: serde_json::Value,
    pub values_override: Option<serde_json::Value>,
    pub deployed_by: Option<Uuid>,
    pub pipeline_id: Option<Uuid>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct HistoryResponse {
    pub id: Uuid,
    pub release_id: Uuid,
    pub target_id: Uuid,
    pub action: String,
    pub phase: String,
    pub traffic_weight: Option<i32>,
    pub image_ref: String,
    pub detail: Option<serde_json::Value>,
    pub actor_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct CreateTargetRequest {
    pub name: String,
    pub environment: Option<String>,
    pub default_strategy: Option<String>,
    pub ops_repo_id: Option<Uuid>,
    pub manifest_path: Option<String>,
    pub hostname: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateReleaseRequest {
    pub image_ref: String,
    pub commit_sha: Option<String>,
    pub strategy: Option<String>,
    pub rollout_config: Option<serde_json::Value>,
    pub values_override: Option<serde_json::Value>,
    pub pipeline_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct AdjustTrafficRequest {
    pub traffic_weight: i32,
}

#[derive(Debug, Serialize)]
pub struct StagingStatusResponse {
    pub diverged: bool,
    pub staging_image: Option<String>,
    pub prod_image: Option<String>,
    pub staging_sha: String,
    pub prod_sha: String,
}

#[derive(Debug, Deserialize)]
pub struct ListParams {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

use super::helpers::ListResponse;

// Ops repo types (unchanged)
#[derive(Debug, Deserialize)]
pub struct CreateOpsRepoRequest {
    pub name: String,
    pub branch: Option<String>,
    pub path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateOpsRepoRequest {
    pub branch: Option<String>,
    pub path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct OpsRepoResponse {
    pub id: Uuid,
    pub name: String,
    pub repo_path: String,
    pub branch: String,
    pub path: String,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<PlatformState> {
    Router::new()
        // Deploy targets
        .route(
            "/api/projects/{id}/targets",
            get(list_targets).post(create_target),
        )
        .route("/api/projects/{id}/targets/{target_id}", get(get_target))
        // Releases
        .route(
            "/api/projects/{id}/deploy-releases",
            get(list_releases).post(create_release),
        )
        .route(
            "/api/projects/{id}/deploy-releases/{release_id}",
            get(get_release),
        )
        // Release actions (traffic management)
        .route(
            "/api/projects/{id}/deploy-releases/{release_id}/traffic",
            axum::routing::patch(adjust_traffic),
        )
        .route(
            "/api/projects/{id}/deploy-releases/{release_id}/promote",
            axum::routing::post(promote_release),
        )
        .route(
            "/api/projects/{id}/deploy-releases/{release_id}/rollback",
            axum::routing::post(rollback_release),
        )
        .route(
            "/api/projects/{id}/deploy-releases/{release_id}/pause",
            axum::routing::post(pause_release),
        )
        .route(
            "/api/projects/{id}/deploy-releases/{release_id}/resume",
            axum::routing::post(resume_release),
        )
        // Release history
        .route(
            "/api/projects/{id}/deploy-releases/{release_id}/history",
            get(release_history),
        )
        // Staging promotion
        .route(
            "/api/projects/{id}/promote-staging",
            axum::routing::post(promote_staging),
        )
        .route("/api/projects/{id}/staging-status", get(staging_status))
        // Deploy preview iframes (unchanged)
        .route(
            "/api/projects/{id}/deploy-preview/iframes",
            get(list_deploy_iframes),
        )
        // Ops repo admin routes (unchanged)
        .route(
            "/api/admin/ops-repos",
            get(list_ops_repos).post(create_ops_repo),
        )
        .route(
            "/api/admin/ops-repos/{repo_id}",
            get(get_ops_repo)
                .patch(update_ops_repo)
                .delete(delete_ops_repo),
        )
}

// ---------------------------------------------------------------------------
// Permission helpers
// ---------------------------------------------------------------------------

async fn require_deploy_read(
    state: &PlatformState,
    auth: &AuthUser,
    project_id: Uuid,
) -> Result<(), ApiError> {
    auth.check_project_scope(project_id)?;
    let allowed = resolver::has_permission_scoped(
        &state.pool,
        &state.valkey,
        auth.user_id,
        Some(project_id),
        Permission::DeployRead,
        auth.token_scopes.as_deref(),
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::NotFound("project".into()));
    }
    Ok(())
}

async fn require_deploy_promote(
    state: &PlatformState,
    auth: &AuthUser,
    project_id: Uuid,
) -> Result<(), ApiError> {
    auth.check_project_scope(project_id)?;
    let allowed = resolver::has_permission_scoped(
        &state.pool,
        &state.valkey,
        auth.user_id,
        Some(project_id),
        Permission::DeployPromote,
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
// Target handlers
// ---------------------------------------------------------------------------

async fn list_targets(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Query(params): Query<ListParams>,
) -> Result<Json<ListResponse<TargetResponse>>, ApiError> {
    require_deploy_read(&state, &auth, id).await?;

    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);

    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM deploy_targets WHERE project_id = $1 AND is_active = true",
    )
    .bind(id)
    .fetch_one(&state.pool)
    .await?;

    let rows = sqlx::query(
        "SELECT id, project_id, name, environment, branch, branch_slug, ttl_hours, expires_at,
                default_strategy, ops_repo_id, manifest_path, hostname, is_active, created_at, updated_at
         FROM deploy_targets WHERE project_id = $1 AND is_active = true
         ORDER BY environment, name LIMIT $2 OFFSET $3",
    )
    .bind(id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.pool)
    .await?;

    let items = rows.iter().map(row_to_target).collect();
    Ok(Json(ListResponse { items, total }))
}

async fn get_target(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((id, target_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<TargetResponse>, ApiError> {
    require_deploy_read(&state, &auth, id).await?;

    let row = sqlx::query(
        "SELECT id, project_id, name, environment, branch, branch_slug, ttl_hours, expires_at,
                default_strategy, ops_repo_id, manifest_path, hostname, is_active, created_at, updated_at
         FROM deploy_targets WHERE id = $1 AND project_id = $2 AND is_active = true",
    )
    .bind(target_id)
    .bind(id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("target".into()))?;

    Ok(Json(row_to_target(&row)))
}

async fn create_target(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<CreateTargetRequest>,
) -> Result<(StatusCode, Json<TargetResponse>), ApiError> {
    require_deploy_promote(&state, &auth, id).await?;
    validation::check_name(&body.name)?;

    let env = body.environment.as_deref().unwrap_or("production");
    let strategy = body.default_strategy.as_deref().unwrap_or("rolling");

    if !matches!(env, "preview" | "staging" | "production") {
        return Err(ApiError::BadRequest(
            "environment must be preview, staging, or production".into(),
        ));
    }
    if !matches!(strategy, "rolling" | "canary" | "ab_test") {
        return Err(ApiError::BadRequest(
            "strategy must be rolling, canary, or ab_test".into(),
        ));
    }

    if let Some(ref h) = body.hostname {
        validation::check_length("hostname", h, 1, 255)?;
    }

    let row = sqlx::query(
        "INSERT INTO deploy_targets (project_id, name, environment, default_strategy, ops_repo_id, manifest_path, hostname, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         RETURNING id, project_id, name, environment, branch, branch_slug, ttl_hours, expires_at,
                   default_strategy, ops_repo_id, manifest_path, hostname, is_active, created_at, updated_at",
    )
    .bind(id)
    .bind(&body.name)
    .bind(env)
    .bind(strategy)
    .bind(body.ops_repo_id)
    .bind(&body.manifest_path)
    .bind(&body.hostname)
    .bind(auth.user_id)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(ref db) if db.is_unique_violation() => {
            ApiError::Conflict("target already exists for this environment".into())
        }
        _ => ApiError::from(e),
    })?;

    let target_id: Uuid = row.get("id");
    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "deploy.target.create".into(),
            resource: "deploy_target".into(),
            resource_id: Some(target_id),
            project_id: Some(id),
            detail: Some(serde_json::json!({"name": body.name, "environment": env})),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok((StatusCode::CREATED, Json(row_to_target(&row))))
}

// ---------------------------------------------------------------------------
// Release handlers
// ---------------------------------------------------------------------------

async fn list_releases(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Query(params): Query<ListParams>,
) -> Result<Json<ListResponse<ReleaseResponse>>, ApiError> {
    require_deploy_read(&state, &auth, id).await?;

    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);

    let total: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM deploy_releases WHERE project_id = $1")
            .bind(id)
            .fetch_one(&state.pool)
            .await?;

    let rows = sqlx::query(
        "SELECT id, target_id, project_id, image_ref, commit_sha, strategy, phase,
                traffic_weight, health, current_step, rollout_config, values_override,
                deployed_by, pipeline_id, started_at, completed_at, created_at, updated_at
         FROM deploy_releases WHERE project_id = $1
         ORDER BY created_at DESC LIMIT $2 OFFSET $3",
    )
    .bind(id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.pool)
    .await?;

    let items = rows.iter().map(row_to_release).collect();
    Ok(Json(ListResponse { items, total }))
}

async fn get_release(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((id, release_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<ReleaseResponse>, ApiError> {
    require_deploy_read(&state, &auth, id).await?;

    let row = sqlx::query(
        "SELECT id, target_id, project_id, image_ref, commit_sha, strategy, phase,
                traffic_weight, health, current_step, rollout_config, values_override,
                deployed_by, pipeline_id, started_at, completed_at, created_at, updated_at
         FROM deploy_releases WHERE id = $1 AND project_id = $2",
    )
    .bind(release_id)
    .bind(id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("release".into()))?;

    Ok(Json(row_to_release(&row)))
}

async fn create_release(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<CreateReleaseRequest>,
) -> Result<(StatusCode, Json<ReleaseResponse>), ApiError> {
    // Rate limit: 30 releases per hour per user
    platform_auth::rate_limit::check_rate(
        &state.valkey,
        "release_create",
        &auth.user_id.to_string(),
        30,
        3600,
    )
    .await?;

    require_deploy_promote(&state, &auth, id).await?;
    validation::check_length("image_ref", &body.image_ref, 1, 2048)?;

    // Find or require a target
    let target = sqlx::query(
        "SELECT id, default_strategy FROM deploy_targets
         WHERE project_id = $1 AND environment = 'production' AND is_active = true
         LIMIT 1",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::BadRequest("no production target exists; create one first".into()))?;

    let target_id: Uuid = target.get("id");
    let default_strategy: String = target.get("default_strategy");
    let strategy = body.strategy.as_deref().unwrap_or(&default_strategy);

    let rollout_config = body
        .rollout_config
        .clone()
        .unwrap_or_else(|| serde_json::json!({}));

    let row = sqlx::query(
        "INSERT INTO deploy_releases (target_id, project_id, image_ref, commit_sha, strategy, rollout_config, values_override, deployed_by, pipeline_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         RETURNING id, target_id, project_id, image_ref, commit_sha, strategy, phase,
                   traffic_weight, health, current_step, rollout_config, values_override,
                   deployed_by, pipeline_id, started_at, completed_at, created_at, updated_at",
    )
    .bind(target_id)
    .bind(id)
    .bind(&body.image_ref)
    .bind(&body.commit_sha)
    .bind(strategy)
    .bind(&rollout_config)
    .bind(&body.values_override)
    .bind(auth.user_id)
    .bind(body.pipeline_id)
    .fetch_one(&state.pool)
    .await?;

    let release_id: Uuid = row.get("id");

    // Record history
    record_release_history(
        &state.pool,
        release_id,
        target_id,
        "created",
        "pending",
        None,
        &body.image_ref,
        Some(auth.user_id),
    )
    .await;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "deploy.release.create".into(),
            resource: "deploy_release".into(),
            resource_id: Some(release_id),
            project_id: Some(id),
            detail: Some(serde_json::json!({"image_ref": body.image_ref, "strategy": strategy})),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    // Wake reconciler
    state.deploy_notify.notify_one();

    // Publish event
    let _ = platform_types::events::publish(
        &state.valkey,
        &platform_types::events::PlatformEvent::ReleaseCreated {
            target_id,
            release_id,
            project_id: id,
            image_ref: body.image_ref.clone(),
            strategy: strategy.to_string(),
        },
    )
    .await;

    Ok((StatusCode::CREATED, Json(row_to_release(&row))))
}

// ---------------------------------------------------------------------------
// Traffic management handlers
// ---------------------------------------------------------------------------

async fn adjust_traffic(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((id, release_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<AdjustTrafficRequest>,
) -> Result<Json<ReleaseResponse>, ApiError> {
    require_deploy_promote(&state, &auth, id).await?;

    if !(0..=100).contains(&body.traffic_weight) {
        return Err(ApiError::BadRequest("traffic_weight must be 0-100".into()));
    }

    let row = sqlx::query(
        "UPDATE deploy_releases SET traffic_weight = $3
         WHERE id = $1 AND project_id = $2 AND phase NOT IN ('completed','rolled_back','cancelled','failed')
         RETURNING id, target_id, project_id, image_ref, commit_sha, strategy, phase,
                   traffic_weight, health, current_step, rollout_config, values_override,
                   deployed_by, pipeline_id, started_at, completed_at, created_at, updated_at",
    )
    .bind(release_id)
    .bind(id)
    .bind(body.traffic_weight)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("release".into()))?;

    let target_id: Uuid = row.get("target_id");
    record_release_history(
        &state.pool,
        release_id,
        target_id,
        "traffic_shifted",
        row.get::<String, _>("phase").as_str(),
        Some(body.traffic_weight),
        row.get::<String, _>("image_ref").as_str(),
        Some(auth.user_id),
    )
    .await;
    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "deploy.traffic.adjust".into(),
            resource: "deploy_release".into(),
            resource_id: Some(release_id),
            project_id: Some(id),
            detail: Some(serde_json::json!({"traffic_weight": body.traffic_weight})),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    state.deploy_notify.notify_one();
    Ok(Json(row_to_release(&row)))
}

async fn promote_release(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((id, release_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<ReleaseResponse>, ApiError> {
    require_deploy_promote(&state, &auth, id).await?;

    let row = sqlx::query(
        "UPDATE deploy_releases SET phase = 'promoting', traffic_weight = 100
         WHERE id = $1 AND project_id = $2 AND phase IN ('progressing','holding','paused')
         RETURNING id, target_id, project_id, image_ref, commit_sha, strategy, phase,
                   traffic_weight, health, current_step, rollout_config, values_override,
                   deployed_by, pipeline_id, started_at, completed_at, created_at, updated_at",
    )
    .bind(release_id)
    .bind(id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::BadRequest("release not found or not in a promotable phase".into()))?;

    let target_id: Uuid = row.get("target_id");
    record_release_history(
        &state.pool,
        release_id,
        target_id,
        "promoted",
        "promoting",
        Some(100),
        row.get::<String, _>("image_ref").as_str(),
        Some(auth.user_id),
    )
    .await;
    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "deploy.release.promote".into(),
            resource: "deploy_release".into(),
            resource_id: Some(release_id),
            project_id: Some(id),
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    state.deploy_notify.notify_one();
    Ok(Json(row_to_release(&row)))
}

async fn rollback_release(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((id, release_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<ReleaseResponse>, ApiError> {
    require_deploy_promote(&state, &auth, id).await?;

    let row = sqlx::query(
        "UPDATE deploy_releases SET phase = 'rolling_back'
         WHERE id = $1 AND project_id = $2 AND phase IN ('progressing','holding','paused')
         RETURNING id, target_id, project_id, image_ref, commit_sha, strategy, phase,
                   traffic_weight, health, current_step, rollout_config, values_override,
                   deployed_by, pipeline_id, started_at, completed_at, created_at, updated_at",
    )
    .bind(release_id)
    .bind(id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| {
        ApiError::BadRequest("release not found or not in a rollback-able phase".into())
    })?;

    let target_id: Uuid = row.get("target_id");
    record_release_history(
        &state.pool,
        release_id,
        target_id,
        "rolled_back",
        "rolling_back",
        None,
        row.get::<String, _>("image_ref").as_str(),
        Some(auth.user_id),
    )
    .await;
    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "deploy.release.rollback".into(),
            resource: "deploy_release".into(),
            resource_id: Some(release_id),
            project_id: Some(id),
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    state.deploy_notify.notify_one();
    Ok(Json(row_to_release(&row)))
}

async fn pause_release(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((id, release_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<ReleaseResponse>, ApiError> {
    require_deploy_promote(&state, &auth, id).await?;

    let row = sqlx::query(
        "UPDATE deploy_releases SET phase = 'paused'
         WHERE id = $1 AND project_id = $2 AND phase = 'progressing'
         RETURNING id, target_id, project_id, image_ref, commit_sha, strategy, phase,
                   traffic_weight, health, current_step, rollout_config, values_override,
                   deployed_by, pipeline_id, started_at, completed_at, created_at, updated_at",
    )
    .bind(release_id)
    .bind(id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::BadRequest("release not found or not progressing".into()))?;

    let target_id: Uuid = row.get("target_id");
    record_release_history(
        &state.pool,
        release_id,
        target_id,
        "paused",
        "paused",
        None,
        row.get::<String, _>("image_ref").as_str(),
        Some(auth.user_id),
    )
    .await;
    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "deploy.release.pause".into(),
            resource: "deploy_release".into(),
            resource_id: Some(release_id),
            project_id: Some(id),
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(Json(row_to_release(&row)))
}

async fn resume_release(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((id, release_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<ReleaseResponse>, ApiError> {
    require_deploy_promote(&state, &auth, id).await?;

    let row = sqlx::query(
        "UPDATE deploy_releases SET phase = 'progressing'
         WHERE id = $1 AND project_id = $2 AND phase = 'paused'
         RETURNING id, target_id, project_id, image_ref, commit_sha, strategy, phase,
                   traffic_weight, health, current_step, rollout_config, values_override,
                   deployed_by, pipeline_id, started_at, completed_at, created_at, updated_at",
    )
    .bind(release_id)
    .bind(id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::BadRequest("release not found or not paused".into()))?;

    let target_id: Uuid = row.get("target_id");
    record_release_history(
        &state.pool,
        release_id,
        target_id,
        "resumed",
        "progressing",
        None,
        row.get::<String, _>("image_ref").as_str(),
        Some(auth.user_id),
    )
    .await;
    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "deploy.release.resume".into(),
            resource: "deploy_release".into(),
            resource_id: Some(release_id),
            project_id: Some(id),
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    state.deploy_notify.notify_one();
    Ok(Json(row_to_release(&row)))
}

// ---------------------------------------------------------------------------
// Release history
// ---------------------------------------------------------------------------

async fn release_history(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((id, release_id)): Path<(Uuid, Uuid)>,
    Query(params): Query<ListParams>,
) -> Result<Json<ListResponse<HistoryResponse>>, ApiError> {
    require_deploy_read(&state, &auth, id).await?;

    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);

    let total: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM release_history WHERE release_id = $1")
            .bind(release_id)
            .fetch_one(&state.pool)
            .await?;

    let rows = sqlx::query(
        "SELECT id, release_id, target_id, action, phase, traffic_weight, image_ref, detail, actor_id, created_at
         FROM release_history WHERE release_id = $1
         ORDER BY created_at DESC LIMIT $2 OFFSET $3",
    )
    .bind(release_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.pool)
    .await?;

    let items = rows
        .iter()
        .map(|r| HistoryResponse {
            id: r.get("id"),
            release_id: r.get("release_id"),
            target_id: r.get("target_id"),
            action: r.get("action"),
            phase: r.get("phase"),
            traffic_weight: r.get("traffic_weight"),
            image_ref: r.get("image_ref"),
            detail: r.get("detail"),
            actor_id: r.get("actor_id"),
            created_at: r.get("created_at"),
        })
        .collect();

    Ok(Json(ListResponse { items, total }))
}

// ---------------------------------------------------------------------------
// Staging promotion handlers
// ---------------------------------------------------------------------------

async fn staging_status(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<StagingStatusResponse>, ApiError> {
    require_deploy_read(&state, &auth, id).await?;

    let ops_repo = fetch_ops_repo_for_project(&state, id).await?;
    let ops_path = std::path::PathBuf::from(&ops_repo.repo_path);

    // staging branch may not exist yet (no deployment to staging has occurred)
    let Ok((diverged, staging_sha, prod_sha)) =
        platform_ops_repo::compare_branches(&ops_path, "staging", &ops_repo.branch).await
    else {
        let prod_sha = platform_ops_repo::get_head_sha(&ops_path)
            .await
            .unwrap_or_default();
        return Ok(Json(StagingStatusResponse {
            diverged: false,
            staging_image: None,
            prod_image: None,
            staging_sha: String::new(),
            prod_sha,
        }));
    };

    let staging_image = platform_ops_repo::read_values(&ops_path, "staging", "staging")
        .await
        .ok()
        .and_then(|v| v["image_ref"].as_str().map(String::from));

    let prod_image = platform_ops_repo::read_values(&ops_path, &ops_repo.branch, "production")
        .await
        .ok()
        .and_then(|v| v["image_ref"].as_str().map(String::from));

    Ok(Json(StagingStatusResponse {
        diverged,
        staging_image,
        prod_image,
        staging_sha,
        prod_sha,
    }))
}

async fn promote_staging(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_deploy_promote(&state, &auth, id).await?;

    let ops_repo = fetch_ops_repo_for_project(&state, id).await?;
    let ops_path = std::path::PathBuf::from(&ops_repo.repo_path);

    let staging_values = platform_ops_repo::read_values(&ops_path, "staging", "staging")
        .await
        .map_err(|e| ApiError::BadRequest(format!("no staging deployment to promote: {e}")))?;

    let image_ref = staging_values["image_ref"]
        .as_str()
        .ok_or_else(|| ApiError::BadRequest("staging values missing image_ref".into()))?
        .to_string();

    let new_sha = platform_ops_repo::merge_branch(&ops_path, "staging", &ops_repo.branch)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;

    let _ = platform_types::events::publish(
        &state.valkey,
        &platform_types::events::PlatformEvent::OpsRepoUpdated {
            project_id: id,
            ops_repo_id: ops_repo.id,
            environment: "production".into(),
            commit_sha: new_sha.clone(),
            image_ref: image_ref.clone(),
        },
    )
    .await;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "deploy.promote_staging".into(),
            resource: "project".into(),
            resource_id: Some(id),
            project_id: Some(id),
            detail: Some(serde_json::json!({
                "image_ref": image_ref,
                "commit_sha": new_sha,
            })),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(Json(serde_json::json!({
        "status": "promoted",
        "image_ref": image_ref,
        "commit_sha": new_sha,
    })))
}

/// Fetch the ops repo associated with a project, returning 404 if none exists.
async fn fetch_ops_repo_for_project(
    state: &PlatformState,
    project_id: Uuid,
) -> Result<OpsRepoRow, ApiError> {
    let row = sqlx::query!(
        r#"SELECT id, repo_path, branch FROM ops_repos WHERE project_id = $1"#,
        project_id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("ops_repo".into()))?;

    Ok(OpsRepoRow {
        id: row.id,
        repo_path: row.repo_path,
        branch: row.branch,
    })
}

/// Lightweight struct for ops repo fields needed by staging endpoints.
struct OpsRepoRow {
    id: Uuid,
    repo_path: String,
    branch: String,
}

// ---------------------------------------------------------------------------
// Deploy preview iframes (unchanged — uses K8s API, not DB tables)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct DeployIframePanel {
    service_name: String,
    port: i32,
    port_name: String,
    preview_url: String,
}

#[derive(Debug, Deserialize)]
struct DeployIframeQuery {
    #[serde(default = "default_deploy_env")]
    env: String,
}

fn default_deploy_env() -> String {
    "production".into()
}

#[tracing::instrument(skip(state, auth), fields(%id), err)]
async fn list_deploy_iframes(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Query(query): Query<DeployIframeQuery>,
) -> Result<Json<Vec<DeployIframePanel>>, ApiError> {
    require_project_read(&state, &auth, id).await?;

    let project = sqlx::query!(
        r#"SELECT namespace_slug FROM projects WHERE id = $1 AND is_active = true"#,
        id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("project".into()))?;

    let slug = &project.namespace_slug;
    if slug.is_empty() {
        return Ok(Json(vec![]));
    }

    let namespace = platform_deployer::reconciler::target_namespace(
        &state.config.to_deployer_config(),
        slug,
        &query.env,
    );

    if !super::preview::validate_namespace_format(&namespace) {
        return Ok(Json(vec![]));
    }

    let svc_api: kube::Api<k8s_openapi::api::core::v1::Service> =
        kube::Api::namespaced(state.kube.clone(), &namespace);
    let lp = kube::api::ListParams::default().labels("platform.io/component=iframe-preview");

    let svcs = match svc_api.list(&lp).await {
        Ok(list) => list,
        Err(kube::Error::Api(resp)) if resp.code == 404 => {
            return Ok(Json(vec![]));
        }
        Err(e) => return Err(ApiError::Internal(e.into())),
    };

    // Filter out services with no ready endpoints (e.g. scaled-to-0 old versions)
    let ep_api: kube::Api<k8s_openapi::api::core::v1::Endpoints> =
        kube::Api::namespaced(state.kube.clone(), &namespace);

    let env_str = &query.env;
    let mut panels = Vec::new();
    for svc in &svcs.items {
        let name = svc.metadata.name.as_deref().unwrap_or_default();
        let has_endpoints = ep_api
            .get(name)
            .await
            .ok()
            .and_then(|ep| ep.subsets)
            .is_some_and(|subsets| {
                subsets
                    .iter()
                    .any(|s| s.addresses.as_ref().is_some_and(|a| !a.is_empty()))
            });
        if !has_endpoints {
            continue;
        }
        let ports = svc.spec.as_ref().and_then(|s| s.ports.as_ref());
        for p in ports.into_iter().flatten() {
            if p.name.as_deref() == Some("iframe") {
                panels.push(DeployIframePanel {
                    service_name: name.to_string(),
                    port: p.port,
                    port_name: "iframe".into(),
                    preview_url: format!("/deploy-preview/{id}/{name}/{env_str}/"),
                });
            }
        }
    }

    Ok(Json(panels))
}

// ---------------------------------------------------------------------------
// Ops repo admin handlers (unchanged — ops_repos table not affected)
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state, body), err)]
async fn create_ops_repo(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Json(body): Json<CreateOpsRepoRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &auth).await?;
    validate_ops_repo_create(&body)?;

    let branch = body.branch.as_deref().unwrap_or("main");
    let path = body.path.as_deref().unwrap_or("/");

    let repo_path =
        platform_ops_repo::init_ops_repo(&state.config.deployer.ops_repos_path, &body.name, branch)
            .await
            .map_err(|e| ApiError::Internal(e.into()))?;

    let repo_path_str = repo_path.to_string_lossy().to_string();

    let r = sqlx::query!(
        r#"
        INSERT INTO ops_repos (name, repo_path, branch, path)
        VALUES ($1, $2, $3, $4)
        RETURNING id, name, repo_path, branch, path, created_at
        "#,
        body.name,
        repo_path_str,
        branch,
        path,
    )
    .fetch_one(&state.pool)
    .await?;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "ops_repo.create".into(),
            resource: "ops_repo".into(),
            resource_id: Some(r.id),
            project_id: None,
            detail: Some(serde_json::json!({"name": body.name})),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok((
        StatusCode::CREATED,
        Json(OpsRepoResponse {
            id: r.id,
            name: r.name,
            repo_path: r.repo_path,
            branch: r.branch,
            path: r.path,
            created_at: r.created_at,
        }),
    ))
}

fn validate_ops_repo_create(body: &CreateOpsRepoRequest) -> Result<(), ApiError> {
    validation::check_name(&body.name)?;
    if let Some(ref branch) = body.branch {
        validation::check_branch_name(branch)?;
    }
    if let Some(ref path) = body.path {
        validation::check_length("path", path, 1, 500)?;
    }
    Ok(())
}

async fn list_ops_repos(
    State(state): State<PlatformState>,
    auth: AuthUser,
) -> Result<Json<Vec<OpsRepoResponse>>, ApiError> {
    require_admin(&state, &auth).await?;

    let rows = sqlx::query!(
        r#"
        SELECT id, name, repo_path, branch, path, created_at
        FROM ops_repos ORDER BY name
        "#,
    )
    .fetch_all(&state.pool)
    .await?;

    let items = rows
        .into_iter()
        .map(|r| OpsRepoResponse {
            id: r.id,
            name: r.name,
            repo_path: r.repo_path,
            branch: r.branch,
            path: r.path,
            created_at: r.created_at,
        })
        .collect();

    Ok(Json(items))
}

async fn get_ops_repo(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(repo_id): Path<Uuid>,
) -> Result<Json<OpsRepoResponse>, ApiError> {
    require_admin(&state, &auth).await?;

    let r = sqlx::query!(
        r#"
        SELECT id, name, repo_path, branch, path, created_at
        FROM ops_repos WHERE id = $1
        "#,
        repo_id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("ops_repo".into()))?;

    Ok(Json(OpsRepoResponse {
        id: r.id,
        name: r.name,
        repo_path: r.repo_path,
        branch: r.branch,
        path: r.path,
        created_at: r.created_at,
    }))
}

#[tracing::instrument(skip(state, body), fields(%repo_id), err)]
async fn update_ops_repo(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(repo_id): Path<Uuid>,
    Json(body): Json<UpdateOpsRepoRequest>,
) -> Result<Json<OpsRepoResponse>, ApiError> {
    require_admin(&state, &auth).await?;
    validate_ops_repo_update(&body)?;

    let r = sqlx::query!(
        r#"
        UPDATE ops_repos SET
            branch = COALESCE($2, branch),
            path = COALESCE($3, path)
        WHERE id = $1
        RETURNING id, name, repo_path, branch, path, created_at
        "#,
        repo_id,
        body.branch,
        body.path,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("ops_repo".into()))?;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "ops_repo.update".into(),
            resource: "ops_repo".into(),
            resource_id: Some(repo_id),
            project_id: None,
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(Json(OpsRepoResponse {
        id: r.id,
        name: r.name,
        repo_path: r.repo_path,
        branch: r.branch,
        path: r.path,
        created_at: r.created_at,
    }))
}

fn validate_ops_repo_update(body: &UpdateOpsRepoRequest) -> Result<(), ApiError> {
    if let Some(ref branch) = body.branch {
        validation::check_branch_name(branch)?;
    }
    if let Some(ref path) = body.path {
        validation::check_length("path", path, 1, 500)?;
    }
    Ok(())
}

#[tracing::instrument(skip(state), fields(%repo_id), err)]
async fn delete_ops_repo(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(repo_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    require_admin(&state, &auth).await?;

    // Check if any deploy targets reference this ops repo
    let ref_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM deploy_targets WHERE ops_repo_id = $1")
            .bind(repo_id)
            .fetch_one(&state.pool)
            .await?;

    if ref_count > 0 {
        return Err(ApiError::Conflict(
            "ops repo is referenced by active deploy targets".into(),
        ));
    }

    let result = sqlx::query!("DELETE FROM ops_repos WHERE id = $1", repo_id)
        .execute(&state.pool)
        .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::NotFound("ops_repo".into()));
    }

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "ops_repo.delete".into(),
            resource: "ops_repo".into(),
            resource_id: Some(repo_id),
            project_id: None,
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn row_to_target(row: &sqlx::postgres::PgRow) -> TargetResponse {
    TargetResponse {
        id: row.get("id"),
        project_id: row.get("project_id"),
        name: row.get("name"),
        environment: row.get("environment"),
        branch: row.get("branch"),
        branch_slug: row.get("branch_slug"),
        ttl_hours: row.get("ttl_hours"),
        expires_at: row.get("expires_at"),
        default_strategy: row.get("default_strategy"),
        ops_repo_id: row.get("ops_repo_id"),
        manifest_path: row.get("manifest_path"),
        hostname: row.get("hostname"),
        is_active: row.get("is_active"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn row_to_release(row: &sqlx::postgres::PgRow) -> ReleaseResponse {
    ReleaseResponse {
        id: row.get("id"),
        target_id: row.get("target_id"),
        project_id: row.get("project_id"),
        image_ref: row.get("image_ref"),
        commit_sha: row.get("commit_sha"),
        strategy: row.get("strategy"),
        phase: row.get("phase"),
        traffic_weight: row.get("traffic_weight"),
        health: row.get("health"),
        current_step: row.get("current_step"),
        rollout_config: row.get("rollout_config"),
        values_override: row.get("values_override"),
        deployed_by: row.get("deployed_by"),
        pipeline_id: row.get("pipeline_id"),
        started_at: row.get("started_at"),
        completed_at: row.get("completed_at"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

#[allow(clippy::too_many_arguments)]
async fn record_release_history(
    pool: &sqlx::PgPool,
    release_id: Uuid,
    target_id: Uuid,
    action: &str,
    phase: &str,
    traffic_weight: Option<i32>,
    image_ref: &str,
    actor_id: Option<Uuid>,
) {
    let _ = sqlx::query(
        "INSERT INTO release_history (release_id, target_id, action, phase, traffic_weight, image_ref, actor_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(release_id)
    .bind(target_id)
    .bind(action)
    .bind(phase)
    .bind(traffic_weight)
    .bind(image_ref)
    .bind(actor_id)
    .execute(pool)
    .await;
}
