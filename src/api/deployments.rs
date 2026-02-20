use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::audit::{AuditEntry, write_audit};
use crate::auth::middleware::AuthUser;
use crate::error::ApiError;
use crate::rbac::{Permission, resolver};
use crate::store::AppState;
use crate::validation;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct DeploymentResponse {
    pub id: Uuid,
    pub project_id: Uuid,
    pub environment: String,
    pub ops_repo_id: Option<Uuid>,
    pub manifest_path: Option<String>,
    pub image_ref: String,
    pub values_override: Option<serde_json::Value>,
    pub desired_status: String,
    pub current_status: String,
    pub current_sha: Option<String>,
    pub deployed_by: Option<Uuid>,
    pub deployed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateDeploymentRequest {
    pub image_ref: Option<String>,
    pub desired_status: Option<String>,
    pub values_override: Option<serde_json::Value>,
    pub ops_repo_id: Option<Uuid>,
    pub manifest_path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct HistoryResponse {
    pub id: Uuid,
    pub deployment_id: Uuid,
    pub image_ref: String,
    pub ops_repo_sha: Option<String>,
    pub action: String,
    pub status: String,
    pub deployed_by: Option<Uuid>,
    pub message: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct CreateOpsRepoRequest {
    pub name: String,
    pub repo_url: String,
    pub branch: Option<String>,
    pub path: Option<String>,
    pub sync_interval_s: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateOpsRepoRequest {
    pub repo_url: Option<String>,
    pub branch: Option<String>,
    pub path: Option<String>,
    pub sync_interval_s: Option<i32>,
}

#[derive(Debug, Serialize)]
pub struct OpsRepoResponse {
    pub id: Uuid,
    pub name: String,
    pub repo_url: String,
    pub branch: String,
    pub path: String,
    pub sync_interval_s: i32,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct ListParams {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

use super::helpers::ListResponse;

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/projects/{id}/deployments", get(list_deployments))
        .route(
            "/api/projects/{id}/deployments/{env}",
            get(get_deployment).patch(update_deployment),
        )
        .route(
            "/api/projects/{id}/deployments/{env}/rollback",
            axum::routing::post(rollback_deployment),
        )
        .route(
            "/api/projects/{id}/deployments/{env}/history",
            get(list_history),
        )
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
        .route(
            "/api/admin/ops-repos/{repo_id}/sync",
            axum::routing::post(force_sync_ops_repo),
        )
}

// ---------------------------------------------------------------------------
// Permission helpers
// ---------------------------------------------------------------------------

async fn require_deploy_read(
    state: &AppState,
    auth: &AuthUser,
    project_id: Uuid,
) -> Result<(), ApiError> {
    let allowed = resolver::has_permission(
        &state.pool,
        &state.valkey,
        auth.user_id,
        Some(project_id),
        Permission::DeployRead,
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

async fn require_deploy_promote(
    state: &AppState,
    auth: &AuthUser,
    project_id: Uuid,
) -> Result<(), ApiError> {
    let allowed = resolver::has_permission(
        &state.pool,
        &state.valkey,
        auth.user_id,
        Some(project_id),
        Permission::DeployPromote,
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

async fn require_admin(state: &AppState, auth: &AuthUser) -> Result<(), ApiError> {
    let allowed = resolver::has_permission(
        &state.pool,
        &state.valkey,
        auth.user_id,
        None,
        Permission::AdminUsers,
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

fn validate_environment(env: &str) -> Result<(), ApiError> {
    if !matches!(env, "preview" | "staging" | "production") {
        return Err(ApiError::BadRequest(
            "environment must be one of: preview, staging, production".into(),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Deployment handlers
// ---------------------------------------------------------------------------

async fn list_deployments(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Query(params): Query<ListParams>,
) -> Result<Json<ListResponse<DeploymentResponse>>, ApiError> {
    require_deploy_read(&state, &auth, id).await?;

    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);

    let total = sqlx::query_scalar!(
        r#"SELECT COUNT(*) as "count!: i64" FROM deployments WHERE project_id = $1"#,
        id,
    )
    .fetch_one(&state.pool)
    .await?;

    let rows = sqlx::query!(
        r#"
        SELECT id, project_id, environment, ops_repo_id, manifest_path,
               image_ref, values_override, desired_status, current_status,
               current_sha, deployed_by, deployed_at, created_at, updated_at
        FROM deployments WHERE project_id = $1
        ORDER BY environment
        LIMIT $2 OFFSET $3
        "#,
        id,
        limit,
        offset,
    )
    .fetch_all(&state.pool)
    .await?;

    let items = rows
        .into_iter()
        .map(|r| DeploymentResponse {
            id: r.id,
            project_id: r.project_id,
            environment: r.environment,
            ops_repo_id: r.ops_repo_id,
            manifest_path: r.manifest_path,
            image_ref: r.image_ref,
            values_override: r.values_override,
            desired_status: r.desired_status,
            current_status: r.current_status,
            current_sha: r.current_sha,
            deployed_by: r.deployed_by,
            deployed_at: r.deployed_at,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
        .collect();

    Ok(Json(ListResponse { items, total }))
}

async fn get_deployment(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, env)): Path<(Uuid, String)>,
) -> Result<Json<DeploymentResponse>, ApiError> {
    validate_environment(&env)?;
    require_deploy_read(&state, &auth, id).await?;

    let r = sqlx::query!(
        r#"
        SELECT id, project_id, environment, ops_repo_id, manifest_path,
               image_ref, values_override, desired_status, current_status,
               current_sha, deployed_by, deployed_at, created_at, updated_at
        FROM deployments WHERE project_id = $1 AND environment = $2
        "#,
        id,
        env,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("deployment".into()))?;

    Ok(Json(DeploymentResponse {
        id: r.id,
        project_id: r.project_id,
        environment: r.environment,
        ops_repo_id: r.ops_repo_id,
        manifest_path: r.manifest_path,
        image_ref: r.image_ref,
        values_override: r.values_override,
        desired_status: r.desired_status,
        current_status: r.current_status,
        current_sha: r.current_sha,
        deployed_by: r.deployed_by,
        deployed_at: r.deployed_at,
        created_at: r.created_at,
        updated_at: r.updated_at,
    }))
}

#[tracing::instrument(skip(state, body), fields(%id, %env), err)]
async fn update_deployment(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, env)): Path<(Uuid, String)>,
    Json(body): Json<UpdateDeploymentRequest>,
) -> Result<Json<DeploymentResponse>, ApiError> {
    validate_environment(&env)?;
    require_deploy_promote(&state, &auth, id).await?;
    validate_update_body(&body)?;

    let r = sqlx::query!(
        r#"
        UPDATE deployments SET
            image_ref = COALESCE($3, image_ref),
            desired_status = COALESCE($4, desired_status),
            values_override = COALESCE($5, values_override),
            ops_repo_id = COALESCE($6, ops_repo_id),
            manifest_path = COALESCE($7, manifest_path),
            current_status = 'pending',
            deployed_by = $8
        WHERE project_id = $1 AND environment = $2
        RETURNING id, project_id, environment, ops_repo_id, manifest_path,
                  image_ref, values_override, desired_status, current_status,
                  current_sha, deployed_by, deployed_at, created_at, updated_at
        "#,
        id,
        env,
        body.image_ref,
        body.desired_status,
        body.values_override,
        body.ops_repo_id,
        body.manifest_path,
        auth.user_id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("deployment".into()))?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "deployment.update",
            resource: "deployment",
            resource_id: Some(r.id),
            project_id: Some(id),
            detail: Some(serde_json::json!({"environment": env})),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    crate::api::webhooks::fire_webhooks(
        &state.pool,
        id,
        "deploy",
        &serde_json::json!({"action": "updated", "environment": env}),
    )
    .await;

    Ok(Json(DeploymentResponse {
        id: r.id,
        project_id: r.project_id,
        environment: r.environment,
        ops_repo_id: r.ops_repo_id,
        manifest_path: r.manifest_path,
        image_ref: r.image_ref,
        values_override: r.values_override,
        desired_status: r.desired_status,
        current_status: r.current_status,
        current_sha: r.current_sha,
        deployed_by: r.deployed_by,
        deployed_at: r.deployed_at,
        created_at: r.created_at,
        updated_at: r.updated_at,
    }))
}

fn validate_update_body(body: &UpdateDeploymentRequest) -> Result<(), ApiError> {
    if let Some(ref image_ref) = body.image_ref {
        validation::check_length("image_ref", image_ref, 1, 500)?;
    }
    if let Some(ref desired_status) = body.desired_status
        && !matches!(desired_status.as_str(), "active" | "stopped")
    {
        return Err(ApiError::BadRequest(
            "desired_status must be 'active' or 'stopped' (use rollback endpoint for rollback)"
                .into(),
        ));
    }
    if let Some(ref manifest_path) = body.manifest_path {
        validation::check_length("manifest_path", manifest_path, 1, 500)?;
    }
    Ok(())
}

#[tracing::instrument(skip(state), fields(%id, %env), err)]
async fn rollback_deployment(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, env)): Path<(Uuid, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    validate_environment(&env)?;
    require_deploy_promote(&state, &auth, id).await?;

    let result = sqlx::query!(
        r#"
        UPDATE deployments
        SET desired_status = 'rollback', current_status = 'pending', deployed_by = $3
        WHERE project_id = $1 AND environment = $2
        RETURNING id
        "#,
        id,
        env,
        auth.user_id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("deployment".into()))?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "deployment.rollback",
            resource: "deployment",
            resource_id: Some(result.id),
            project_id: Some(id),
            detail: Some(serde_json::json!({"environment": env})),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(Json(serde_json::json!({"ok": true})))
}

async fn list_history(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, env)): Path<(Uuid, String)>,
    Query(params): Query<ListParams>,
) -> Result<Json<ListResponse<HistoryResponse>>, ApiError> {
    validate_environment(&env)?;
    require_deploy_read(&state, &auth, id).await?;

    let deployment_id = sqlx::query_scalar!(
        "SELECT id FROM deployments WHERE project_id = $1 AND environment = $2",
        id,
        env,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("deployment".into()))?;

    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);

    let total = sqlx::query_scalar!(
        r#"SELECT COUNT(*) as "count!: i64" FROM deployment_history WHERE deployment_id = $1"#,
        deployment_id,
    )
    .fetch_one(&state.pool)
    .await?;

    let rows = sqlx::query!(
        r#"
        SELECT id, deployment_id, image_ref, ops_repo_sha, action,
               status, deployed_by, message, created_at
        FROM deployment_history
        WHERE deployment_id = $1
        ORDER BY created_at DESC
        LIMIT $2 OFFSET $3
        "#,
        deployment_id,
        limit,
        offset,
    )
    .fetch_all(&state.pool)
    .await?;

    let items = rows
        .into_iter()
        .map(|r| HistoryResponse {
            id: r.id,
            deployment_id: r.deployment_id,
            image_ref: r.image_ref,
            ops_repo_sha: r.ops_repo_sha,
            action: r.action,
            status: r.status,
            deployed_by: r.deployed_by,
            message: r.message,
            created_at: r.created_at,
        })
        .collect();

    Ok(Json(ListResponse { items, total }))
}

// ---------------------------------------------------------------------------
// Ops repo admin handlers
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state, body), err)]
async fn create_ops_repo(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<CreateOpsRepoRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &auth).await?;
    validate_ops_repo_create(&body)?;

    let r = sqlx::query!(
        r#"
        INSERT INTO ops_repos (name, repo_url, branch, path, sync_interval_s)
        VALUES ($1, $2, COALESCE($3, 'main'), COALESCE($4, '/'), COALESCE($5, 60))
        RETURNING id, name, repo_url, branch, path, sync_interval_s, created_at
        "#,
        body.name,
        body.repo_url,
        body.branch,
        body.path,
        body.sync_interval_s,
    )
    .fetch_one(&state.pool)
    .await?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "ops_repo.create",
            resource: "ops_repo",
            resource_id: Some(r.id),
            project_id: None,
            detail: Some(serde_json::json!({"name": body.name})),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok((
        StatusCode::CREATED,
        Json(OpsRepoResponse {
            id: r.id,
            name: r.name,
            repo_url: r.repo_url,
            branch: r.branch,
            path: r.path,
            sync_interval_s: r.sync_interval_s,
            created_at: r.created_at,
        }),
    ))
}

fn validate_ops_repo_create(body: &CreateOpsRepoRequest) -> Result<(), ApiError> {
    validation::check_name(&body.name)?;
    validation::check_url(&body.repo_url)?;
    validation::check_ssrf_url(&body.repo_url, &["http", "https"])?;
    if let Some(ref branch) = body.branch {
        validation::check_branch_name(branch)?;
    }
    if let Some(ref path) = body.path {
        validation::check_length("path", path, 1, 500)?;
    }
    if let Some(interval) = body.sync_interval_s
        && !(10..=86400).contains(&interval)
    {
        return Err(ApiError::BadRequest(
            "sync_interval_s must be between 10 and 86400".into(),
        ));
    }
    Ok(())
}

async fn list_ops_repos(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<Vec<OpsRepoResponse>>, ApiError> {
    require_admin(&state, &auth).await?;

    let rows = sqlx::query!(
        r#"
        SELECT id, name, repo_url, branch, path, sync_interval_s, created_at
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
            repo_url: r.repo_url,
            branch: r.branch,
            path: r.path,
            sync_interval_s: r.sync_interval_s,
            created_at: r.created_at,
        })
        .collect();

    Ok(Json(items))
}

async fn get_ops_repo(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(repo_id): Path<Uuid>,
) -> Result<Json<OpsRepoResponse>, ApiError> {
    require_admin(&state, &auth).await?;

    let r = sqlx::query!(
        r#"
        SELECT id, name, repo_url, branch, path, sync_interval_s, created_at
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
        repo_url: r.repo_url,
        branch: r.branch,
        path: r.path,
        sync_interval_s: r.sync_interval_s,
        created_at: r.created_at,
    }))
}

#[tracing::instrument(skip(state, body), fields(%repo_id), err)]
async fn update_ops_repo(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(repo_id): Path<Uuid>,
    Json(body): Json<UpdateOpsRepoRequest>,
) -> Result<Json<OpsRepoResponse>, ApiError> {
    require_admin(&state, &auth).await?;
    validate_ops_repo_update(&body)?;

    let r = sqlx::query!(
        r#"
        UPDATE ops_repos SET
            repo_url = COALESCE($2, repo_url),
            branch = COALESCE($3, branch),
            path = COALESCE($4, path),
            sync_interval_s = COALESCE($5, sync_interval_s)
        WHERE id = $1
        RETURNING id, name, repo_url, branch, path, sync_interval_s, created_at
        "#,
        repo_id,
        body.repo_url,
        body.branch,
        body.path,
        body.sync_interval_s,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("ops_repo".into()))?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "ops_repo.update",
            resource: "ops_repo",
            resource_id: Some(repo_id),
            project_id: None,
            detail: None,
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(Json(OpsRepoResponse {
        id: r.id,
        name: r.name,
        repo_url: r.repo_url,
        branch: r.branch,
        path: r.path,
        sync_interval_s: r.sync_interval_s,
        created_at: r.created_at,
    }))
}

fn validate_ops_repo_update(body: &UpdateOpsRepoRequest) -> Result<(), ApiError> {
    if let Some(ref repo_url) = body.repo_url {
        validation::check_url(repo_url)?;
        validation::check_ssrf_url(repo_url, &["http", "https"])?;
    }
    if let Some(ref branch) = body.branch {
        validation::check_branch_name(branch)?;
    }
    if let Some(ref path) = body.path {
        validation::check_length("path", path, 1, 500)?;
    }
    if let Some(interval) = body.sync_interval_s
        && !(10..=86400).contains(&interval)
    {
        return Err(ApiError::BadRequest(
            "sync_interval_s must be between 10 and 86400".into(),
        ));
    }
    Ok(())
}

#[tracing::instrument(skip(state), fields(%repo_id), err)]
async fn delete_ops_repo(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(repo_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_admin(&state, &auth).await?;

    let ref_count = sqlx::query_scalar!(
        r#"SELECT COUNT(*) as "count!: i64" FROM deployments WHERE ops_repo_id = $1"#,
        repo_id,
    )
    .fetch_one(&state.pool)
    .await?;

    if ref_count > 0 {
        return Err(ApiError::Conflict(
            "ops repo is referenced by active deployments".into(),
        ));
    }

    let result = sqlx::query!("DELETE FROM ops_repos WHERE id = $1", repo_id)
        .execute(&state.pool)
        .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::NotFound("ops_repo".into()));
    }

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "ops_repo.delete",
            resource: "ops_repo",
            resource_id: Some(repo_id),
            project_id: None,
            detail: None,
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(Json(serde_json::json!({"ok": true})))
}

#[tracing::instrument(skip(state), fields(%repo_id), err)]
async fn force_sync_ops_repo(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(repo_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_admin(&state, &auth).await?;

    let sha: String = crate::deployer::ops_repo::force_sync(
        &state.pool,
        &state.valkey,
        &state.config.ops_repos_path,
        repo_id,
    )
    .await
    .map_err(ApiError::from)?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "ops_repo.sync",
            resource: "ops_repo",
            resource_id: Some(repo_id),
            project_id: None,
            detail: None,
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(Json(serde_json::json!({"ok": true, "sha": sha})))
}
