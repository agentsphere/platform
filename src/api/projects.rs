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

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateProjectRequest {
    pub name: String,
    pub visibility: Option<String>,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub default_branch: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateProjectRequest {
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub visibility: Option<String>,
    pub default_branch: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ListProjectsParams {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub owner_id: Option<Uuid>,
    pub visibility: Option<String>,
    pub search: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ProjectResponse {
    pub id: Uuid,
    pub owner_id: Uuid,
    pub name: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub visibility: String,
    pub default_branch: String,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct ListResponse<T: Serialize> {
    pub items: Vec<T>,
    pub total: i64,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/projects", get(list_projects).post(create_project))
        .route(
            "/api/projects/{id}",
            get(get_project)
                .patch(update_project)
                .delete(delete_project),
        )
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state, body), fields(project_name = %body.name), err)]
async fn create_project(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<CreateProjectRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // Require project:write globally or admin
    let allowed = resolver::has_permission(
        &state.pool,
        &state.valkey,
        auth.user_id,
        None,
        Permission::ProjectWrite,
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::Forbidden);
    }

    let visibility = body.visibility.as_deref().unwrap_or("private");
    if !["private", "internal", "public"].contains(&visibility) {
        return Err(ApiError::BadRequest(
            "visibility must be private, internal, or public".into(),
        ));
    }

    let default_branch = body.default_branch.as_deref().unwrap_or("main");

    // Look up owner name for repo path
    let owner_name = sqlx::query_scalar!("SELECT name FROM users WHERE id = $1", auth.user_id)
        .fetch_one(&state.pool)
        .await?;

    // Initialize bare git repo
    let repo_path = crate::git::repo::init_bare_repo(
        &state.config.git_repos_path,
        &owner_name,
        &body.name,
        default_branch,
    )
    .await
    .map_err(ApiError::Internal)?;

    let repo_path_str = repo_path.to_string_lossy().to_string();

    let project = sqlx::query!(
        r#"
        INSERT INTO projects (owner_id, name, display_name, description, visibility, default_branch, repo_path)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING id, owner_id, name, display_name, description, visibility, default_branch, is_active, created_at, updated_at
        "#,
        auth.user_id,
        body.name,
        body.display_name,
        body.description,
        visibility,
        default_branch,
        repo_path_str,
    )
    .fetch_one(&state.pool)
    .await?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "project.create",
            resource: "project",
            resource_id: Some(project.id),
            project_id: Some(project.id),
            detail: Some(serde_json::json!({"name": body.name, "visibility": visibility})),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok((
        StatusCode::CREATED,
        Json(ProjectResponse {
            id: project.id,
            owner_id: project.owner_id,
            name: project.name,
            display_name: project.display_name,
            description: project.description,
            visibility: project.visibility,
            default_branch: project.default_branch,
            is_active: project.is_active,
            created_at: project.created_at,
            updated_at: project.updated_at,
        }),
    ))
}

async fn list_projects(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(params): Query<ListProjectsParams>,
) -> Result<Json<ListResponse<ProjectResponse>>, ApiError> {
    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);
    let search_pattern = params.search.as_deref().map(|s| format!("%{s}%"));

    // Count matching projects visible to the user
    let total = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) as "count!: i64"
        FROM projects
        WHERE is_active = true
          AND ($1::uuid IS NULL OR owner_id = $1)
          AND ($2::text IS NULL OR visibility = $2)
          AND ($3::text IS NULL OR name ILIKE $3)
          AND (
              visibility = 'public'
              OR visibility = 'internal'
              OR owner_id = $4
          )
        "#,
        params.owner_id,
        params.visibility,
        search_pattern,
        auth.user_id,
    )
    .fetch_one(&state.pool)
    .await?;

    let rows = sqlx::query!(
        r#"
        SELECT id, owner_id, name, display_name, description, visibility, default_branch, is_active, created_at, updated_at
        FROM projects
        WHERE is_active = true
          AND ($1::uuid IS NULL OR owner_id = $1)
          AND ($2::text IS NULL OR visibility = $2)
          AND ($3::text IS NULL OR name ILIKE $3)
          AND (
              visibility = 'public'
              OR visibility = 'internal'
              OR owner_id = $4
          )
        ORDER BY created_at DESC
        LIMIT $5 OFFSET $6
        "#,
        params.owner_id,
        params.visibility,
        search_pattern,
        auth.user_id,
        limit,
        offset,
    )
    .fetch_all(&state.pool)
    .await?;

    let items = rows
        .into_iter()
        .map(|p| ProjectResponse {
            id: p.id,
            owner_id: p.owner_id,
            name: p.name,
            display_name: p.display_name,
            description: p.description,
            visibility: p.visibility,
            default_branch: p.default_branch,
            is_active: p.is_active,
            created_at: p.created_at,
            updated_at: p.updated_at,
        })
        .collect();

    Ok(Json(ListResponse { items, total }))
}

async fn get_project(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<ProjectResponse>, ApiError> {
    let project = sqlx::query!(
        r#"
        SELECT id, owner_id, name, display_name, description, visibility, default_branch, is_active, created_at, updated_at
        FROM projects WHERE id = $1 AND is_active = true
        "#,
        id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("project".into()))?;

    // Visibility check: private projects only visible to owner or those with project:read
    if project.visibility == "private" && project.owner_id != auth.user_id {
        let allowed = resolver::has_permission(
            &state.pool,
            &state.valkey,
            auth.user_id,
            Some(id),
            Permission::ProjectRead,
        )
        .await
        .map_err(ApiError::Internal)?;

        if !allowed {
            return Err(ApiError::NotFound("project".into()));
        }
    }

    Ok(Json(ProjectResponse {
        id: project.id,
        owner_id: project.owner_id,
        name: project.name,
        display_name: project.display_name,
        description: project.description,
        visibility: project.visibility,
        default_branch: project.default_branch,
        is_active: project.is_active,
        created_at: project.created_at,
        updated_at: project.updated_at,
    }))
}

#[tracing::instrument(skip(state, body), fields(%id), err)]
async fn update_project(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateProjectRequest>,
) -> Result<Json<ProjectResponse>, ApiError> {
    // Owner or project:write
    let project_owner = sqlx::query_scalar!(
        "SELECT owner_id FROM projects WHERE id = $1 AND is_active = true",
        id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("project".into()))?;

    if project_owner != auth.user_id {
        let allowed = resolver::has_permission(
            &state.pool,
            &state.valkey,
            auth.user_id,
            Some(id),
            Permission::ProjectWrite,
        )
        .await
        .map_err(ApiError::Internal)?;

        if !allowed {
            return Err(ApiError::Forbidden);
        }
    }

    if let Some(ref vis) = body.visibility
        && !["private", "internal", "public"].contains(&vis.as_str())
    {
        return Err(ApiError::BadRequest(
            "visibility must be private, internal, or public".into(),
        ));
    }

    let project = sqlx::query!(
        r#"
        UPDATE projects SET
            display_name = COALESCE($2, display_name),
            description = COALESCE($3, description),
            visibility = COALESCE($4, visibility),
            default_branch = COALESCE($5, default_branch)
        WHERE id = $1 AND is_active = true
        RETURNING id, owner_id, name, display_name, description, visibility, default_branch, is_active, created_at, updated_at
        "#,
        id,
        body.display_name,
        body.description,
        body.visibility,
        body.default_branch,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("project".into()))?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "project.update",
            resource: "project",
            resource_id: Some(id),
            project_id: Some(id),
            detail: None,
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(Json(ProjectResponse {
        id: project.id,
        owner_id: project.owner_id,
        name: project.name,
        display_name: project.display_name,
        description: project.description,
        visibility: project.visibility,
        default_branch: project.default_branch,
        is_active: project.is_active,
        created_at: project.created_at,
        updated_at: project.updated_at,
    }))
}

#[tracing::instrument(skip(state), fields(%id), err)]
async fn delete_project(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Owner or admin
    let project_owner = sqlx::query_scalar!(
        "SELECT owner_id FROM projects WHERE id = $1 AND is_active = true",
        id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("project".into()))?;

    if project_owner != auth.user_id {
        let is_admin = resolver::has_permission(
            &state.pool,
            &state.valkey,
            auth.user_id,
            None,
            Permission::AdminUsers,
        )
        .await
        .map_err(ApiError::Internal)?;

        if !is_admin {
            return Err(ApiError::Forbidden);
        }
    }

    // Soft-delete
    sqlx::query!("UPDATE projects SET is_active = false WHERE id = $1", id)
        .execute(&state.pool)
        .await?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "project.delete",
            resource: "project",
            resource_id: Some(id),
            project_id: Some(id),
            detail: None,
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(Json(serde_json::json!({"ok": true})))
}
