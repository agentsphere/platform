// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

#[allow(dead_code, unused_imports)]
use chrono::{DateTime, Utc};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use platform_auth::resolver;
use platform_types::validation;
use platform_types::{ApiError, AuthUser};
use platform_types::{AuditEntry, send_audit};

use platform_types::ListResponse;

use crate::state::PlatformState;

// ---------------------------------------------------------------------------
// Domain types (from archive/src/workspace/types.rs)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct Workspace {
    id: Uuid,
    name: String,
    display_name: Option<String>,
    description: Option<String>,
    owner_id: Uuid,
    is_active: bool,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
struct WorkspaceMember {
    id: Uuid,
    #[allow(dead_code)]
    workspace_id: Uuid,
    user_id: Uuid,
    user_name: String,
    role: String,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct CreateWorkspaceRequest {
    name: String,
    display_name: Option<String>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UpdateWorkspaceRequest {
    display_name: Option<String>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AddMemberRequest {
    user_id: Uuid,
    role: Option<String>,
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct WorkspaceResponse {
    pub id: Uuid,
    pub name: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub owner_id: Uuid,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<Workspace> for WorkspaceResponse {
    fn from(w: Workspace) -> Self {
        Self {
            id: w.id,
            name: w.name,
            display_name: w.display_name,
            description: w.description,
            owner_id: w.owner_id,
            is_active: w.is_active,
            created_at: w.created_at,
            updated_at: w.updated_at,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct MemberResponse {
    pub id: Uuid,
    pub user_id: Uuid,
    pub user_name: String,
    pub role: String,
    pub created_at: DateTime<Utc>,
}

impl From<WorkspaceMember> for MemberResponse {
    fn from(m: WorkspaceMember) -> Self {
        Self {
            id: m.id,
            user_id: m.user_id,
            user_name: m.user_name,
            role: m.role,
            created_at: m.created_at,
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

pub fn router() -> Router<PlatformState> {
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

mod service {
    use platform_types::ApiError;
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::{Workspace, WorkspaceMember};

    pub async fn is_member(
        pool: &PgPool,
        workspace_id: Uuid,
        user_id: Uuid,
    ) -> Result<bool, ApiError> {
        let exists = sqlx::query_scalar!(
            r#"SELECT EXISTS(
                SELECT 1 FROM workspace_members WHERE workspace_id = $1 AND user_id = $2
            ) as "exists!: bool""#,
            workspace_id,
            user_id,
        )
        .fetch_one(pool)
        .await?;
        Ok(exists)
    }

    pub async fn is_admin(
        pool: &PgPool,
        workspace_id: Uuid,
        user_id: Uuid,
    ) -> Result<bool, ApiError> {
        let exists = sqlx::query_scalar!(
            r#"SELECT EXISTS(
                SELECT 1 FROM workspace_members
                WHERE workspace_id = $1 AND user_id = $2 AND role IN ('owner', 'admin')
            ) as "exists!: bool""#,
            workspace_id,
            user_id,
        )
        .fetch_one(pool)
        .await?;
        Ok(exists)
    }

    pub async fn is_owner(
        pool: &PgPool,
        workspace_id: Uuid,
        user_id: Uuid,
    ) -> Result<bool, ApiError> {
        let exists = sqlx::query_scalar!(
            r#"SELECT EXISTS(
                SELECT 1 FROM workspaces WHERE id = $1 AND owner_id = $2 AND is_active = true
            ) as "exists!: bool""#,
            workspace_id,
            user_id,
        )
        .fetch_one(pool)
        .await?;
        Ok(exists)
    }

    pub async fn get_workspace(pool: &PgPool, id: Uuid) -> Result<Option<Workspace>, ApiError> {
        let row = sqlx::query_as!(
            Workspace,
            r#"SELECT id, name, display_name, description, owner_id, is_active,
                      created_at, updated_at
               FROM workspaces WHERE id = $1 AND is_active = true"#,
            id,
        )
        .fetch_optional(pool)
        .await?;
        Ok(row)
    }

    pub async fn list_user_workspaces(
        pool: &PgPool,
        user_id: Uuid,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<Workspace>, i64), ApiError> {
        let total = sqlx::query_scalar!(
            r#"SELECT COUNT(*) as "count!: i64"
               FROM workspaces w
               JOIN workspace_members wm ON wm.workspace_id = w.id
               WHERE wm.user_id = $1 AND w.is_active = true"#,
            user_id,
        )
        .fetch_one(pool)
        .await?;

        let rows = sqlx::query_as!(
            Workspace,
            r#"SELECT w.id, w.name, w.display_name, w.description, w.owner_id,
                      w.is_active, w.created_at, w.updated_at
               FROM workspaces w
               JOIN workspace_members wm ON wm.workspace_id = w.id
               WHERE wm.user_id = $1 AND w.is_active = true
               ORDER BY w.updated_at DESC
               LIMIT $2 OFFSET $3"#,
            user_id,
            limit,
            offset,
        )
        .fetch_all(pool)
        .await?;

        Ok((rows, total))
    }

    pub async fn create_workspace(
        pool: &PgPool,
        owner_id: Uuid,
        name: &str,
        display_name: Option<&str>,
        description: Option<&str>,
    ) -> Result<Workspace, ApiError> {
        let id = Uuid::new_v4();
        let mut tx = pool.begin().await?;

        sqlx::query!(
            r#"INSERT INTO workspaces (id, name, display_name, description, owner_id)
               VALUES ($1, $2, $3, $4, $5)"#,
            id,
            name,
            display_name,
            description,
            owner_id,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            if let sqlx::Error::Database(ref db_err) = e
                && db_err.constraint() == Some("workspaces_name_key")
            {
                return ApiError::Conflict("workspace name already taken".into());
            }
            ApiError::from(e)
        })?;

        sqlx::query!(
            "INSERT INTO workspace_members (workspace_id, user_id, role) VALUES ($1, $2, 'owner')",
            id,
            owner_id,
        )
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        get_workspace(pool, id)
            .await?
            .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("workspace vanished after creation")))
    }

    pub async fn update_workspace(
        pool: &PgPool,
        id: Uuid,
        display_name: Option<&str>,
        description: Option<&str>,
    ) -> Result<Workspace, ApiError> {
        let result = sqlx::query!(
            r#"UPDATE workspaces SET
                display_name = COALESCE($2, display_name),
                description = COALESCE($3, description),
                updated_at = now()
               WHERE id = $1 AND is_active = true"#,
            id,
            display_name,
            description,
        )
        .execute(pool)
        .await?;

        if result.rows_affected() == 0 {
            return Err(ApiError::NotFound("workspace".into()));
        }

        get_workspace(pool, id)
            .await?
            .ok_or_else(|| ApiError::NotFound("workspace".into()))
    }

    pub async fn delete_workspace(pool: &PgPool, id: Uuid) -> Result<bool, ApiError> {
        let result = sqlx::query!(
            "UPDATE workspaces SET is_active = false, updated_at = now() WHERE id = $1 AND is_active = true",
            id,
        )
        .execute(pool)
        .await?;

        if result.rows_affected() > 0 {
            sqlx::query(
                "UPDATE projects SET is_active = false, updated_at = now() WHERE workspace_id = $1 AND is_active = true",
            )
            .bind(id)
            .execute(pool)
            .await?;
        }

        Ok(result.rows_affected() > 0)
    }

    pub async fn list_members(
        pool: &PgPool,
        workspace_id: Uuid,
    ) -> Result<Vec<WorkspaceMember>, ApiError> {
        let rows = sqlx::query!(
            r#"SELECT wm.id, wm.workspace_id, wm.user_id, u.name as user_name,
                      wm.role, wm.created_at
               FROM workspace_members wm
               JOIN users u ON u.id = wm.user_id
               WHERE wm.workspace_id = $1
               ORDER BY wm.created_at"#,
            workspace_id,
        )
        .fetch_all(pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| WorkspaceMember {
                id: r.id,
                workspace_id: r.workspace_id,
                user_id: r.user_id,
                user_name: r.user_name,
                role: r.role,
                created_at: r.created_at,
            })
            .collect())
    }

    pub async fn add_member(
        pool: &PgPool,
        workspace_id: Uuid,
        user_id: Uuid,
        role: &str,
    ) -> Result<(), ApiError> {
        sqlx::query!(
            "INSERT INTO workspace_members (workspace_id, user_id, role) VALUES ($1, $2, $3)
             ON CONFLICT (workspace_id, user_id) DO UPDATE SET role = EXCLUDED.role",
            workspace_id,
            user_id,
            role,
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    pub async fn remove_member(
        pool: &PgPool,
        workspace_id: Uuid,
        user_id: Uuid,
    ) -> Result<bool, ApiError> {
        let result = sqlx::query!(
            "DELETE FROM workspace_members WHERE workspace_id = $1 AND user_id = $2",
            workspace_id,
            user_id,
        )
        .execute(pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

async fn require_workspace_member(
    state: &PlatformState,
    auth: &AuthUser,
    workspace_id: Uuid,
) -> Result<(), ApiError> {
    if !service::is_member(&state.pool, workspace_id, auth.user_id).await? {
        return Err(ApiError::NotFound("workspace".into()));
    }
    Ok(())
}

async fn require_workspace_admin(
    state: &PlatformState,
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
    State(state): State<PlatformState>,
    auth: AuthUser,
    Json(body): Json<CreateWorkspaceRequest>,
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

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "workspace.create".into(),
            resource: "workspace".into(),
            resource_id: Some(ws.id),
            project_id: None,
            detail: Some(serde_json::json!({ "name": ws.name })),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok((StatusCode::CREATED, Json(ws.into())))
}

/// List workspaces.
async fn list_workspaces(
    State(state): State<PlatformState>,
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
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<WorkspaceResponse>, ApiError> {
    auth.check_workspace_scope(id)?;
    require_workspace_member(&state, &auth, id).await?;

    let ws = service::get_workspace(&state.pool, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("workspace".into()))?;

    Ok(Json(ws.into()))
}

/// Update workspace settings.
async fn update_workspace(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateWorkspaceRequest>,
) -> Result<Json<WorkspaceResponse>, ApiError> {
    auth.check_workspace_scope(id)?;
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

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "workspace.update".into(),
            resource: "workspace".into(),
            resource_id: Some(id),
            project_id: None,
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(Json(ws.into()))
}

/// Delete a workspace.
async fn delete_workspace(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    auth.check_workspace_scope(id)?;
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

    // Invalidate permission caches for all workspace members -- workspace-derived
    // project permissions must be revoked immediately, not after cache TTL.
    for member in &members {
        let _ = resolver::invalidate_permissions(&state.valkey, member.user_id, None).await;
    }

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "workspace.delete".into(),
            resource: "workspace".into(),
            resource_id: Some(id),
            project_id: None,
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(StatusCode::NO_CONTENT)
}

/// List workspace members.
async fn list_members(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Query(params): Query<ListParams>,
) -> Result<Json<ListResponse<MemberResponse>>, ApiError> {
    auth.check_workspace_scope(id)?;
    require_workspace_member(&state, &auth, id).await?;

    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);

    let total: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM workspace_members WHERE workspace_id = $1")
            .bind(id)
            .fetch_one(&state.pool)
            .await
            .unwrap_or(Some(0))
            .unwrap_or(0);

    let rows = sqlx::query(
        r"SELECT wm.id, wm.user_id, u.name as user_name, wm.role, wm.created_at
          FROM workspace_members wm
          JOIN users u ON u.id = wm.user_id
          WHERE wm.workspace_id = $1
          ORDER BY wm.created_at
          LIMIT $2 OFFSET $3",
    )
    .bind(id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.pool)
    .await?;

    let items = rows
        .into_iter()
        .map(|r| {
            use sqlx::Row;
            MemberResponse {
                id: r.get("id"),
                user_id: r.get("user_id"),
                user_name: r.get("user_name"),
                role: r.get("role"),
                created_at: r.get("created_at"),
            }
        })
        .collect();

    Ok(Json(ListResponse { items, total }))
}

/// Add a member to a workspace.
async fn add_member(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<AddMemberRequest>,
) -> Result<impl IntoResponse, ApiError> {
    auth.check_workspace_scope(id)?;
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

    // Invalidate permission cache -- workspace membership grants project access
    let _ = resolver::invalidate_permissions(&state.valkey, body.user_id, None).await;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "workspace.member_add".into(),
            resource: "workspace_member".into(),
            resource_id: Some(id),
            project_id: None,
            detail: Some(serde_json::json!({ "user_id": body.user_id, "role": role })),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(StatusCode::CREATED)
}

/// Delete a workspace member.
async fn remove_member(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((id, user_id)): Path<(Uuid, Uuid)>,
) -> Result<impl IntoResponse, ApiError> {
    auth.check_workspace_scope(id)?;
    require_workspace_admin(&state, &auth, id).await?;

    // Can't remove the workspace owner
    if service::is_owner(&state.pool, id, user_id).await? {
        return Err(ApiError::BadRequest("cannot remove workspace owner".into()));
    }

    let removed = service::remove_member(&state.pool, id, user_id).await?;
    if !removed {
        return Err(ApiError::NotFound("member".into()));
    }

    // Invalidate permission cache -- workspace membership grants project access
    let _ = resolver::invalidate_permissions(&state.valkey, user_id, None).await;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "workspace.member_remove".into(),
            resource: "workspace_member".into(),
            resource_id: Some(id),
            project_id: None,
            detail: Some(serde_json::json!({ "user_id": user_id })),
            ip_addr: auth.ip_addr.clone(),
        },
    );

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
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Query(params): Query<ListParams>,
) -> Result<Json<ListResponse<WorkspaceProjectResponse>>, ApiError> {
    auth.check_workspace_scope(id)?;
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
