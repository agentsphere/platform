// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

use uuid::Uuid;

use platform_auth::resolver;
use platform_types::{ApiError, AuthUser, Permission};

pub use platform_types::ListResponse;

use crate::state::PlatformState;

/// Check project-level read access considering scope, visibility, ownership, and RBAC.
/// Returns 404 (not 403) for private resources to avoid leaking existence.
pub async fn require_project_read(
    state: &PlatformState,
    auth: &AuthUser,
    project_id: Uuid,
) -> Result<(), ApiError> {
    // Hard scope check FIRST — before any DB query
    auth.check_project_scope(project_id)?;

    let project = sqlx::query!(
        "SELECT visibility, owner_id, workspace_id FROM projects WHERE id = $1 AND is_active = true",
        project_id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("project".into()))?;

    // If workspace-scoped, verify project belongs to that workspace
    if let Some(scope_wid) = auth.boundary_workspace_id
        && project.workspace_id != scope_wid
    {
        return Err(ApiError::NotFound("project".into()));
    }

    if project.visibility == "public"
        || project.visibility == "internal"
        || project.owner_id == auth.user_id
    {
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

/// Check the caller has admin:users permission (scope-aware), return Forbidden otherwise.
pub async fn require_admin(state: &PlatformState, auth: &AuthUser) -> Result<(), ApiError> {
    let allowed = resolver::has_permission_scoped(
        &state.pool,
        &state.valkey,
        auth.user_id,
        None,
        Permission::AdminUsers,
        auth.token_scopes.as_deref(),
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

/// Check project-level write access via scope + RBAC.
pub async fn require_project_write(
    state: &PlatformState,
    auth: &AuthUser,
    project_id: Uuid,
) -> Result<(), ApiError> {
    // Hard scope check FIRST
    auth.check_project_scope(project_id)?;

    // If workspace-scoped, verify project belongs to that workspace
    if let Some(scope_wid) = auth.boundary_workspace_id {
        let in_workspace = sqlx::query_scalar!(
            r#"SELECT EXISTS(SELECT 1 FROM projects WHERE id = $1 AND workspace_id = $2 AND is_active = true) as "exists!: bool""#,
            project_id, scope_wid,
        )
        .fetch_one(&state.pool)
        .await?;
        if !in_workspace {
            return Err(ApiError::NotFound("project".into()));
        }
    }

    let allowed = resolver::has_permission_scoped(
        &state.pool,
        &state.valkey,
        auth.user_id,
        Some(project_id),
        Permission::ProjectWrite,
        auth.token_scopes.as_deref(),
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}
