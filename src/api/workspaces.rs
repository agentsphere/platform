use chrono::{DateTime, Utc};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use ts_rs::TS;

use crate::audit::{AuditEntry, write_audit};
use crate::auth::middleware::AuthUser;
use crate::error::ApiError;
use crate::rbac::resolver;
use crate::store::AppState;
use crate::validation;
use crate::workspace::{self, service};

use super::helpers::ListResponse;

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, TS)]
#[ts(export, rename = "Workspace")]
pub struct WorkspaceResponse {
    pub id: Uuid,
    pub name: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub owner_id: Uuid,
    pub is_active: bool,
    pub created_at: String,
    pub updated_at: String,
}

impl From<workspace::Workspace> for WorkspaceResponse {
    fn from(w: workspace::Workspace) -> Self {
        Self {
            id: w.id,
            name: w.name,
            display_name: w.display_name,
            description: w.description,
            owner_id: w.owner_id,
            is_active: w.is_active,
            created_at: w.created_at.to_rfc3339(),
            updated_at: w.updated_at.to_rfc3339(),
        }
    }
}

#[derive(Debug, Serialize, TS)]
#[ts(export, rename = "WorkspaceMember")]
pub struct MemberResponse {
    pub id: Uuid,
    pub user_id: Uuid,
    pub user_name: String,
    pub role: String,
    pub created_at: String,
}

impl From<workspace::WorkspaceMember> for MemberResponse {
    fn from(m: workspace::WorkspaceMember) -> Self {
        Self {
            id: m.id,
            user_id: m.user_id,
            user_name: m.user_name,
            role: m.role,
            created_at: m.created_at.to_rfc3339(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ListParams {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/workspaces",
            get(list_workspaces).post(create_workspace),
        )
        .route(
            "/api/workspaces/{id}",
            get(get_workspace)
                .patch(update_workspace)
                .delete(delete_workspace),
        )
        .route(
            "/api/workspaces/{id}/members",
            get(list_members).post(add_member),
        )
        .route(
            "/api/workspaces/{id}/members/{user_id}",
            axum::routing::delete(remove_member),
        )
        .route(
            "/api/workspaces/{id}/projects",
            get(list_workspace_projects),
        )
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn require_workspace_member(
    state: &AppState,
    auth: &AuthUser,
    workspace_id: Uuid,
) -> Result<(), ApiError> {
    if !service::is_member(&state.pool, workspace_id, auth.user_id).await? {
        return Err(ApiError::NotFound("workspace".into()));
    }
    Ok(())
}

async fn require_workspace_admin(
    state: &AppState,
    auth: &AuthUser,
    workspace_id: Uuid,
) -> Result<(), ApiError> {
    if !service::is_admin(&state.pool, workspace_id, auth.user_id).await? {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Create a workspace.
async fn create_workspace(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<workspace::CreateWorkspaceRequest>,
) -> Result<(StatusCode, Json<WorkspaceResponse>), ApiError> {
    validation::check_name(&body.name)?;
    if let Some(ref dn) = body.display_name {
        validation::check_length("display_name", dn, 1, 255)?;
    }
    if let Some(ref desc) = body.description {
        validation::check_length("description", desc, 0, 10_000)?;
    }

    let ws = service::create_workspace(
        &state.pool,
        auth.user_id,
        &body.name,
        body.display_name.as_deref(),
        body.description.as_deref(),
    )
    .await?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "workspace.create",
            resource: "workspace",
            resource_id: Some(ws.id),
            project_id: None,
            detail: Some(serde_json::json!({ "name": ws.name })),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok((StatusCode::CREATED, Json(ws.into())))
}

/// List workspaces.
async fn list_workspaces(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(params): Query<ListParams>,
) -> Result<Json<ListResponse<WorkspaceResponse>>, ApiError> {
    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);

    let (workspaces, total) =
        service::list_user_workspaces(&state.pool, auth.user_id, limit, offset).await?;

    Ok(Json(ListResponse {
        items: workspaces.into_iter().map(Into::into).collect(),
        total,
    }))
}

/// Get workspace by ID.
async fn get_workspace(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<WorkspaceResponse>, ApiError> {
    require_workspace_member(&state, &auth, id).await?;

    let ws = service::get_workspace(&state.pool, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("workspace".into()))?;

    Ok(Json(ws.into()))
}

/// Update workspace settings.
async fn update_workspace(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<workspace::UpdateWorkspaceRequest>,
) -> Result<Json<WorkspaceResponse>, ApiError> {
    require_workspace_admin(&state, &auth, id).await?;

    if let Some(ref dn) = body.display_name {
        validation::check_length("display_name", dn, 1, 255)?;
    }
    if let Some(ref desc) = body.description {
        validation::check_length("description", desc, 0, 10_000)?;
    }

    let ws = service::update_workspace(
        &state.pool,
        id,
        body.display_name.as_deref(),
        body.description.as_deref(),
    )
    .await?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "workspace.update",
            resource: "workspace",
            resource_id: Some(id),
            project_id: None,
            detail: None,
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(Json(ws.into()))
}

/// Delete a workspace.
async fn delete_workspace(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    // Only workspace owner can delete
    if !service::is_owner(&state.pool, id, auth.user_id).await? {
        return Err(ApiError::Forbidden);
    }

    // Collect members BEFORE deletion so we can invalidate their permission caches
    let members = service::list_members(&state.pool, id).await?;

    let deleted = service::delete_workspace(&state.pool, id).await?;
    if !deleted {
        return Err(ApiError::NotFound("workspace".into()));
    }

    // Invalidate permission caches for all workspace members — workspace-derived
    // project permissions must be revoked immediately, not after cache TTL.
    for member in &members {
        let _ = resolver::invalidate_permissions(&state.valkey, member.user_id, None).await;
    }

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "workspace.delete",
            resource: "workspace",
            resource_id: Some(id),
            project_id: None,
            detail: None,
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

/// List workspace members.
async fn list_members(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<MemberResponse>>, ApiError> {
    require_workspace_member(&state, &auth, id).await?;

    let members = service::list_members(&state.pool, id).await?;
    Ok(Json(members.into_iter().map(Into::into).collect()))
}

/// Add a member to a workspace.
async fn add_member(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<workspace::AddMemberRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_workspace_admin(&state, &auth, id).await?;

    let role = body.role.as_deref().unwrap_or("member");
    if !matches!(role, "admin" | "member") {
        return Err(ApiError::BadRequest(
            "role must be 'admin' or 'member'".into(),
        ));
    }

    // Prevent demoting the workspace owner via the upsert
    let existing_role = sqlx::query_scalar!(
        "SELECT role FROM workspace_members WHERE workspace_id = $1 AND user_id = $2",
        id,
        body.user_id,
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::from)?;

    if existing_role.as_deref() == Some("owner") {
        return Err(ApiError::BadRequest(
            "cannot modify workspace owner role".into(),
        ));
    }

    service::add_member(&state.pool, id, body.user_id, role).await?;

    // Invalidate permission cache — workspace membership grants project access
    let _ = resolver::invalidate_permissions(&state.valkey, body.user_id, None).await;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "workspace.member_add",
            resource: "workspace_member",
            resource_id: Some(id),
            project_id: None,
            detail: Some(serde_json::json!({ "user_id": body.user_id, "role": role })),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(StatusCode::CREATED)
}

/// Delete a workspace member.
async fn remove_member(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, user_id)): Path<(Uuid, Uuid)>,
) -> Result<impl IntoResponse, ApiError> {
    require_workspace_admin(&state, &auth, id).await?;

    // Can't remove the workspace owner
    if service::is_owner(&state.pool, id, user_id).await? {
        return Err(ApiError::BadRequest("cannot remove workspace owner".into()));
    }

    let removed = service::remove_member(&state.pool, id, user_id).await?;
    if !removed {
        return Err(ApiError::NotFound("member".into()));
    }

    // Invalidate permission cache — workspace membership grants project access
    let _ = resolver::invalidate_permissions(&state.valkey, user_id, None).await;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "workspace.member_remove",
            resource: "workspace_member",
            resource_id: Some(id),
            project_id: None,
            detail: Some(serde_json::json!({ "user_id": user_id })),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Workspace Projects
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct WorkspaceProjectResponse {
    id: Uuid,
    name: String,
    display_name: Option<String>,
    description: Option<String>,
    visibility: String,
    default_branch: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

/// List projects in a workspace.
async fn list_workspace_projects(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Query(params): Query<ListParams>,
) -> Result<Json<ListResponse<WorkspaceProjectResponse>>, ApiError> {
    require_workspace_member(&state, &auth, id).await?;

    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);

    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM projects WHERE workspace_id = $1 AND is_active = true",
    )
    .bind(id)
    .fetch_one(&state.pool)
    .await
    .unwrap_or(Some(0))
    .unwrap_or(0);

    let rows = sqlx::query(
        r"SELECT id, name, display_name, description, visibility,
                 default_branch, created_at, updated_at
          FROM projects
          WHERE workspace_id = $1 AND is_active = true
          ORDER BY updated_at DESC
          LIMIT $2 OFFSET $3",
    )
    .bind(id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.pool)
    .await?;

    let items = rows
        .into_iter()
        .map(|p| {
            use sqlx::Row;
            WorkspaceProjectResponse {
                id: p.get("id"),
                name: p.get("name"),
                display_name: p.get("display_name"),
                description: p.get("description"),
                visibility: p.get("visibility"),
                default_branch: p.get("default_branch"),
                created_at: p.get("created_at"),
                updated_at: p.get("updated_at"),
            }
        })
        .collect();

    Ok(Json(ListResponse { items, total }))
}
