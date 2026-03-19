use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::agent::commands::{validate_command_name, validate_template};
use crate::audit::{AuditEntry, write_audit};
use crate::auth::middleware::AuthUser;
use crate::error::ApiError;
use crate::rbac::{Permission, resolver};
use crate::store::AppState;

use super::helpers::ListResponse;

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateCommandRequest {
    pub project_id: Option<Uuid>,
    pub workspace_id: Option<Uuid>,
    pub name: String,
    pub description: Option<String>,
    pub prompt_template: String,
    #[serde(default)]
    pub persistent_session: bool,
}

#[derive(Debug, Deserialize)]
pub struct UpdateCommandRequest {
    pub description: Option<String>,
    pub prompt_template: Option<String>,
    pub persistent_session: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct ListCommandsParams {
    pub project_id: Option<Uuid>,
    pub workspace_id: Option<Uuid>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct CommandResponse {
    pub id: Uuid,
    pub project_id: Option<Uuid>,
    pub workspace_id: Option<Uuid>,
    pub name: String,
    pub description: String,
    pub persistent_session: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct ResolvedCommandsParams {
    pub project_id: Uuid,
}

// ---------------------------------------------------------------------------
// Permission helpers
// ---------------------------------------------------------------------------

/// Global commands require `admin:config`. Workspace-scoped commands require
/// workspace admin. Project-scoped commands require `project:write`.
async fn require_command_write(
    state: &AppState,
    auth: &AuthUser,
    project_id: Option<Uuid>,
    workspace_id: Option<Uuid>,
) -> Result<(), ApiError> {
    if let Some(pid) = project_id {
        let allowed = resolver::has_permission_scoped(
            &state.pool,
            &state.valkey,
            auth.user_id,
            Some(pid),
            Permission::ProjectWrite,
            auth.token_scopes.as_deref(),
        )
        .await
        .map_err(ApiError::Internal)?;
        if !allowed {
            return Err(ApiError::NotFound("command".into()));
        }
    } else if let Some(wid) = workspace_id {
        require_workspace_admin(state, auth, wid).await?;
    } else {
        let allowed = resolver::has_permission_scoped(
            &state.pool,
            &state.valkey,
            auth.user_id,
            None,
            Permission::AdminConfig,
            auth.token_scopes.as_deref(),
        )
        .await
        .map_err(ApiError::Internal)?;
        if !allowed {
            return Err(ApiError::Forbidden);
        }
    }
    Ok(())
}

async fn require_workspace_admin(
    state: &AppState,
    auth: &AuthUser,
    workspace_id: Uuid,
) -> Result<(), ApiError> {
    if !crate::workspace::service::is_admin(&state.pool, workspace_id, auth.user_id).await? {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

async fn require_workspace_member(
    state: &AppState,
    auth: &AuthUser,
    workspace_id: Uuid,
) -> Result<(), ApiError> {
    if !crate::workspace::service::is_member(&state.pool, workspace_id, auth.user_id).await? {
        return Err(ApiError::NotFound("workspace".into()));
    }
    Ok(())
}

/// Look up a project's `workspace_id`.
async fn project_workspace_id(
    pool: &sqlx::PgPool,
    project_id: Uuid,
) -> Result<Option<Uuid>, ApiError> {
    let row = sqlx::query!(
        "SELECT workspace_id FROM projects WHERE id = $1 AND is_active = true",
        project_id,
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("project".into()))?;
    Ok(Some(row.workspace_id))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `POST /api/commands` — Create a new platform command (global or project-scoped).
#[tracing::instrument(skip(state, body), fields(user_id = %auth.user_id), err)]
async fn create_command(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<CreateCommandRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // Reject setting both workspace_id and project_id together via this endpoint.
    // Workspace commands should use POST /api/workspaces/{id}/commands.
    if body.workspace_id.is_some() && body.project_id.is_some() {
        return Err(ApiError::BadRequest(
            "cannot set both workspace_id and project_id; use workspace route for workspace commands"
                .into(),
        ));
    }
    if body.workspace_id.is_some() {
        return Err(ApiError::BadRequest(
            "use POST /api/workspaces/{id}/commands for workspace-scoped commands".into(),
        ));
    }

    require_command_write(&state, &auth, body.project_id, None).await?;

    validate_command_name(&body.name).map_err(ApiError::BadRequest)?;
    validate_template(&body.prompt_template).map_err(ApiError::BadRequest)?;
    if let Some(ref desc) = body.description {
        crate::validation::check_length("description", desc, 0, 10_000)?;
    }

    if let Some(pid) = body.project_id {
        sqlx::query!(
            "SELECT id FROM projects WHERE id = $1 AND is_active = true",
            pid,
        )
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| ApiError::NotFound("project".into()))?;
    }

    let description = body.description.as_deref().unwrap_or("");

    let row = sqlx::query!(
        r#"
        INSERT INTO platform_commands (project_id, workspace_id, name, description, prompt_template, persistent_session)
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING id, created_at, updated_at
        "#,
        body.project_id,
        Option::<Uuid>::None,
        body.name,
        description,
        body.prompt_template,
        body.persistent_session,
    )
    .fetch_one(&state.pool)
    .await?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "command.create",
            resource: "platform_command",
            resource_id: Some(row.id),
            project_id: body.project_id,
            detail: Some(serde_json::json!({ "name": body.name })),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok((
        StatusCode::CREATED,
        Json(CommandResponse {
            id: row.id,
            project_id: body.project_id,
            workspace_id: None,
            name: body.name,
            description: description.to_owned(),
            persistent_session: body.persistent_session,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }),
    ))
}

/// `GET /api/commands` — List commands (global, or project+workspace+global when `project_id` set).
#[tracing::instrument(skip(state), fields(user_id = %auth.user_id), err)]
async fn list_commands(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(params): Query<ListCommandsParams>,
) -> Result<Json<ListResponse<CommandResponse>>, ApiError> {
    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);

    let (items, total) = if let Some(pid) = params.project_id {
        super::helpers::require_project_read(&state, &auth, pid).await?;
        let wid = project_workspace_id(&state.pool, pid).await?;
        list_commands_for_project(&state.pool, pid, wid, limit, offset).await?
    } else if let Some(wid) = params.workspace_id {
        require_workspace_member(&state, &auth, wid).await?;
        list_commands_for_workspace(&state.pool, wid, limit, offset).await?
    } else {
        list_global_commands(&state.pool, limit, offset).await?
    };

    Ok(Json(ListResponse { items, total }))
}

async fn list_commands_for_project(
    pool: &sqlx::PgPool,
    pid: Uuid,
    wid: Option<Uuid>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<CommandResponse>, i64), ApiError> {
    let total = sqlx::query_scalar!(
        r#"SELECT COUNT(*) as "count!: i64" FROM platform_commands
        WHERE project_id = $1
           OR (workspace_id = $2 AND project_id IS NULL)
           OR (project_id IS NULL AND workspace_id IS NULL)"#,
        pid,
        wid,
    )
    .fetch_one(pool)
    .await?;

    let rows = sqlx::query!(
        r#"SELECT id, project_id, workspace_id, name, description,
               persistent_session, created_at, updated_at
        FROM platform_commands
        WHERE project_id = $1
           OR (workspace_id = $2 AND project_id IS NULL)
           OR (project_id IS NULL AND workspace_id IS NULL)
        ORDER BY
            CASE WHEN project_id IS NOT NULL THEN 0
                 WHEN workspace_id IS NOT NULL THEN 1
                 ELSE 2 END ASC,
            name ASC
        LIMIT $3 OFFSET $4"#,
        pid,
        wid,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;

    let items = rows
        .into_iter()
        .map(|r| CommandResponse {
            id: r.id,
            project_id: r.project_id,
            workspace_id: r.workspace_id,
            name: r.name,
            description: r.description,
            persistent_session: r.persistent_session,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
        .collect();
    Ok((items, total))
}

async fn list_commands_for_workspace(
    pool: &sqlx::PgPool,
    wid: Uuid,
    limit: i64,
    offset: i64,
) -> Result<(Vec<CommandResponse>, i64), ApiError> {
    let total = sqlx::query_scalar!(
        r#"SELECT COUNT(*) as "count!: i64" FROM platform_commands
        WHERE (workspace_id = $1 AND project_id IS NULL)
           OR (project_id IS NULL AND workspace_id IS NULL)"#,
        wid,
    )
    .fetch_one(pool)
    .await?;

    let rows = sqlx::query!(
        r#"SELECT id, project_id, workspace_id, name, description,
               persistent_session, created_at, updated_at
        FROM platform_commands
        WHERE (workspace_id = $1 AND project_id IS NULL)
           OR (project_id IS NULL AND workspace_id IS NULL)
        ORDER BY workspace_id IS NULL ASC, name ASC
        LIMIT $2 OFFSET $3"#,
        wid,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;

    let items = rows
        .into_iter()
        .map(|r| CommandResponse {
            id: r.id,
            project_id: r.project_id,
            workspace_id: r.workspace_id,
            name: r.name,
            description: r.description,
            persistent_session: r.persistent_session,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
        .collect();
    Ok((items, total))
}

async fn list_global_commands(
    pool: &sqlx::PgPool,
    limit: i64,
    offset: i64,
) -> Result<(Vec<CommandResponse>, i64), ApiError> {
    let total = sqlx::query_scalar!(
        r#"SELECT COUNT(*) as "count!: i64" FROM platform_commands
        WHERE project_id IS NULL AND workspace_id IS NULL"#,
    )
    .fetch_one(pool)
    .await?;

    let rows = sqlx::query!(
        r#"SELECT id, project_id, workspace_id, name, description,
               persistent_session, created_at, updated_at
        FROM platform_commands
        WHERE project_id IS NULL AND workspace_id IS NULL
        ORDER BY name ASC
        LIMIT $1 OFFSET $2"#,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;

    let items = rows
        .into_iter()
        .map(|r| CommandResponse {
            id: r.id,
            project_id: r.project_id,
            workspace_id: r.workspace_id,
            name: r.name,
            description: r.description,
            persistent_session: r.persistent_session,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
        .collect();
    Ok((items, total))
}

/// `GET /api/commands/{id}` — Get a single command.
#[tracing::instrument(skip(state), fields(user_id = %auth.user_id, %id), err)]
async fn get_command(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<CommandResponse>, ApiError> {
    let row = sqlx::query!(
        r#"
        SELECT id, project_id, workspace_id, name, description,
               persistent_session, created_at, updated_at
        FROM platform_commands
        WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("command".into()))?;

    if let Some(pid) = row.project_id {
        super::helpers::require_project_read(&state, &auth, pid).await?;
    } else if let Some(wid) = row.workspace_id {
        require_workspace_member(&state, &auth, wid).await?;
    }

    Ok(Json(CommandResponse {
        id: row.id,
        project_id: row.project_id,
        workspace_id: row.workspace_id,
        name: row.name,
        description: row.description,
        persistent_session: row.persistent_session,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }))
}

/// `PUT /api/commands/{id}` — Update a command.
#[tracing::instrument(skip(state, body), fields(user_id = %auth.user_id, %id), err)]
async fn update_command(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateCommandRequest>,
) -> Result<Json<CommandResponse>, ApiError> {
    let existing = sqlx::query!(
        "SELECT project_id, workspace_id FROM platform_commands WHERE id = $1",
        id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("command".into()))?;

    require_command_write(&state, &auth, existing.project_id, existing.workspace_id).await?;

    if let Some(ref template) = body.prompt_template {
        validate_template(template).map_err(ApiError::BadRequest)?;
    }
    if let Some(ref desc) = body.description {
        crate::validation::check_length("description", desc, 0, 10_000)?;
    }

    let row = sqlx::query!(
        r#"
        UPDATE platform_commands
        SET description = COALESCE($2, description),
            prompt_template = COALESCE($3, prompt_template),
            persistent_session = COALESCE($4, persistent_session),
            updated_at = now()
        WHERE id = $1
        RETURNING id, project_id, workspace_id, name, description,
                  persistent_session, created_at, updated_at
        "#,
        id,
        body.description,
        body.prompt_template,
        body.persistent_session,
    )
    .fetch_one(&state.pool)
    .await?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "command.update",
            resource: "platform_command",
            resource_id: Some(id),
            project_id: existing.project_id,
            detail: None,
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(Json(CommandResponse {
        id: row.id,
        project_id: row.project_id,
        workspace_id: row.workspace_id,
        name: row.name,
        description: row.description,
        persistent_session: row.persistent_session,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }))
}

/// `DELETE /api/commands/{id}` — Delete a command.
#[tracing::instrument(skip(state), fields(user_id = %auth.user_id, %id), err)]
async fn delete_command(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    let existing = sqlx::query!(
        "SELECT project_id, workspace_id, name FROM platform_commands WHERE id = $1",
        id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("command".into()))?;

    require_command_write(&state, &auth, existing.project_id, existing.workspace_id).await?;

    sqlx::query!("DELETE FROM platform_commands WHERE id = $1", id)
        .execute(&state.pool)
        .await?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "command.delete",
            resource: "platform_command",
            resource_id: Some(id),
            project_id: existing.project_id,
            detail: Some(serde_json::json!({ "name": existing.name })),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/commands/resolve` — Resolve a command input to its prompt.
#[tracing::instrument(skip(state, body), fields(user_id = %auth.user_id), err)]
async fn resolve_command_handler(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<ResolveCommandRequest>,
) -> Result<Json<ResolveCommandResponse>, ApiError> {
    crate::validation::check_length("input", &body.input, 1, 100_000)?;

    let workspace_id = if let Some(pid) = body.project_id {
        super::helpers::require_project_read(&state, &auth, pid).await?;
        project_workspace_id(&state.pool, pid).await?
    } else {
        None
    };

    let resolved = crate::agent::commands::resolve_command(
        &state.pool,
        body.project_id,
        workspace_id,
        &body.input,
    )
    .await?;

    Ok(Json(ResolveCommandResponse {
        name: resolved.name,
        prompt: resolved.prompt,
        persistent: resolved.persistent,
    }))
}

#[derive(Debug, Deserialize)]
pub struct ResolveCommandRequest {
    pub input: String,
    pub project_id: Option<Uuid>,
}

#[derive(Debug, Serialize)]
pub struct ResolveCommandResponse {
    pub name: String,
    pub prompt: String,
    pub persistent: bool,
}

/// `GET /api/commands/resolved?project_id=X` — Get the merged set of commands for a project.
///
/// Applies the override hierarchy: project > workspace > global. Returns one entry
/// per unique command name with scope annotation.
#[tracing::instrument(skip(state), fields(user_id = %auth.user_id), err)]
async fn list_resolved_commands(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(params): Query<ResolvedCommandsParams>,
) -> Result<Json<Vec<crate::agent::commands::ResolvedCommandFile>>, ApiError> {
    super::helpers::require_project_read(&state, &auth, params.project_id).await?;
    let workspace_id = project_workspace_id(&state.pool, params.project_id).await?;

    let resolved =
        crate::agent::commands::resolve_all_commands(&state.pool, params.project_id, workspace_id)
            .await?;

    Ok(Json(resolved))
}

// ---------------------------------------------------------------------------
// Workspace-scoped command handlers
// ---------------------------------------------------------------------------

/// `GET /api/workspaces/{id}/commands` — List workspace + global commands.
#[tracing::instrument(skip(state), fields(user_id = %auth.user_id, %id), err)]
async fn list_workspace_commands(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<ListResponse<CommandResponse>>, ApiError> {
    require_workspace_member(&state, &auth, id).await?;

    let rows = sqlx::query!(
        r#"
        SELECT id, project_id, workspace_id, name, description,
               persistent_session, created_at, updated_at
        FROM platform_commands
        WHERE (workspace_id = $1 AND project_id IS NULL)
           OR (project_id IS NULL AND workspace_id IS NULL)
        ORDER BY workspace_id IS NULL ASC, name ASC
        "#,
        id,
    )
    .fetch_all(&state.pool)
    .await?;

    #[allow(clippy::cast_possible_wrap)]
    let total = rows.len() as i64;
    let items = rows
        .into_iter()
        .map(|r| CommandResponse {
            id: r.id,
            project_id: r.project_id,
            workspace_id: r.workspace_id,
            name: r.name,
            description: r.description,
            persistent_session: r.persistent_session,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
        .collect();

    Ok(Json(ListResponse { items, total }))
}

/// `POST /api/workspaces/{id}/commands` — Create a workspace-scoped command.
#[tracing::instrument(skip(state, body), fields(user_id = %auth.user_id, %id), err)]
async fn create_workspace_command(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<CreateWorkspaceCommandRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_workspace_admin(&state, &auth, id).await?;

    validate_command_name(&body.name).map_err(ApiError::BadRequest)?;
    validate_template(&body.prompt_template).map_err(ApiError::BadRequest)?;
    if let Some(ref desc) = body.description {
        crate::validation::check_length("description", desc, 0, 10_000)?;
    }

    let description = body.description.as_deref().unwrap_or("");

    let row = sqlx::query!(
        r#"
        INSERT INTO platform_commands (workspace_id, project_id, name, description, prompt_template, persistent_session)
        VALUES ($1, NULL, $2, $3, $4, $5)
        RETURNING id, created_at, updated_at
        "#,
        id,
        body.name,
        description,
        body.prompt_template,
        body.persistent_session,
    )
    .fetch_one(&state.pool)
    .await?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "command.create",
            resource: "platform_command",
            resource_id: Some(row.id),
            project_id: None,
            detail: Some(serde_json::json!({ "name": body.name, "workspace_id": id })),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok((
        StatusCode::CREATED,
        Json(CommandResponse {
            id: row.id,
            project_id: None,
            workspace_id: Some(id),
            name: body.name,
            description: description.to_owned(),
            persistent_session: body.persistent_session,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct CreateWorkspaceCommandRequest {
    pub name: String,
    pub description: Option<String>,
    pub prompt_template: String,
    #[serde(default)]
    pub persistent_session: bool,
}

/// `DELETE /api/workspaces/{id}/commands/{command_id}` — Delete a workspace command.
#[tracing::instrument(skip(state), fields(user_id = %auth.user_id, %id, %command_id), err)]
async fn delete_workspace_command(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, command_id)): Path<(Uuid, Uuid)>,
) -> Result<impl IntoResponse, ApiError> {
    require_workspace_admin(&state, &auth, id).await?;

    let existing = sqlx::query!(
        "SELECT name FROM platform_commands WHERE id = $1 AND workspace_id = $2 AND project_id IS NULL",
        command_id,
        id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("command".into()))?;

    sqlx::query!("DELETE FROM platform_commands WHERE id = $1", command_id)
        .execute(&state.pool)
        .await?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "command.delete",
            resource: "platform_command",
            resource_id: Some(command_id),
            project_id: None,
            detail: Some(serde_json::json!({ "name": existing.name, "workspace_id": id })),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/commands", get(list_commands).post(create_command))
        // resolve/resolved must come before {id} to avoid being parsed as a UUID
        .route(
            "/api/commands/resolve",
            axum::routing::post(resolve_command_handler),
        )
        .route("/api/commands/resolved", get(list_resolved_commands))
        .route(
            "/api/commands/{id}",
            get(get_command).put(update_command).delete(delete_command),
        )
        .route(
            "/api/workspaces/{id}/commands",
            get(list_workspace_commands).post(create_workspace_command),
        )
        .route(
            "/api/workspaces/{id}/commands/{command_id}",
            axum::routing::delete(delete_workspace_command),
        )
}
