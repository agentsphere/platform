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
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct CommandResponse {
    pub id: Uuid,
    pub project_id: Option<Uuid>,
    pub name: String,
    pub description: String,
    pub persistent_session: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Permission helpers
// ---------------------------------------------------------------------------

/// Global commands require `admin:config`. Project-scoped commands require
/// `project:write` on the target project.
async fn require_command_write(
    state: &AppState,
    auth: &AuthUser,
    project_id: Option<Uuid>,
) -> Result<(), ApiError> {
    if let Some(pid) = project_id {
        let allowed = resolver::has_permission(
            &state.pool,
            &state.valkey,
            auth.user_id,
            Some(pid),
            Permission::ProjectWrite,
        )
        .await
        .map_err(ApiError::Internal)?;
        if !allowed {
            // Return 404 to avoid leaking resource existence
            return Err(ApiError::NotFound("command".into()));
        }
    } else {
        let allowed = resolver::has_permission(
            &state.pool,
            &state.valkey,
            auth.user_id,
            None,
            Permission::AdminConfig,
        )
        .await
        .map_err(ApiError::Internal)?;
        if !allowed {
            return Err(ApiError::Forbidden);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `POST /api/commands` — Create a new platform command.
#[tracing::instrument(skip(state, body), fields(user_id = %auth.user_id), err)]
async fn create_command(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<CreateCommandRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_command_write(&state, &auth, body.project_id).await?;

    // Validate inputs
    validate_command_name(&body.name).map_err(ApiError::BadRequest)?;
    validate_template(&body.prompt_template).map_err(ApiError::BadRequest)?;
    if let Some(ref desc) = body.description {
        crate::validation::check_length("description", desc, 0, 10_000)?;
    }

    // Verify project exists if project-scoped
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
        INSERT INTO platform_commands (project_id, name, description, prompt_template, persistent_session)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id, created_at, updated_at
        "#,
        body.project_id,
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
            name: body.name,
            description: description.to_owned(),
            persistent_session: body.persistent_session,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }),
    ))
}

/// `GET /api/commands` — List commands (global + optionally project-scoped).
#[tracing::instrument(skip(state), fields(user_id = %auth.user_id), err)]
async fn list_commands(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(params): Query<ListCommandsParams>,
) -> Result<Json<ListResponse<CommandResponse>>, ApiError> {
    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);

    let (items, total) = if let Some(pid) = params.project_id {
        // Verify project exists and user has read access
        super::helpers::require_project_read(&state, &auth, pid).await?;

        // Return project-scoped + global commands
        let total = sqlx::query_scalar!(
            r#"
            SELECT COUNT(*) as "count!: i64" FROM platform_commands
            WHERE project_id = $1 OR project_id IS NULL
            "#,
            pid,
        )
        .fetch_one(&state.pool)
        .await?;

        let rows = sqlx::query!(
            r#"
            SELECT id, project_id, name, description, persistent_session, created_at, updated_at
            FROM platform_commands
            WHERE project_id = $1 OR project_id IS NULL
            ORDER BY project_id IS NULL ASC, name ASC
            LIMIT $2 OFFSET $3
            "#,
            pid,
            limit,
            offset,
        )
        .fetch_all(&state.pool)
        .await?;

        let items = rows
            .into_iter()
            .map(|r| CommandResponse {
                id: r.id,
                project_id: r.project_id,
                name: r.name,
                description: r.description,
                persistent_session: r.persistent_session,
                created_at: r.created_at,
                updated_at: r.updated_at,
            })
            .collect();
        (items, total)
    } else {
        // Global commands only
        let total = sqlx::query_scalar!(
            r#"SELECT COUNT(*) as "count!: i64" FROM platform_commands WHERE project_id IS NULL"#,
        )
        .fetch_one(&state.pool)
        .await?;

        let rows = sqlx::query!(
            r#"
            SELECT id, project_id, name, description, persistent_session, created_at, updated_at
            FROM platform_commands
            WHERE project_id IS NULL
            ORDER BY name ASC
            LIMIT $1 OFFSET $2
            "#,
            limit,
            offset,
        )
        .fetch_all(&state.pool)
        .await?;

        let items = rows
            .into_iter()
            .map(|r| CommandResponse {
                id: r.id,
                project_id: r.project_id,
                name: r.name,
                description: r.description,
                persistent_session: r.persistent_session,
                created_at: r.created_at,
                updated_at: r.updated_at,
            })
            .collect();
        (items, total)
    };

    Ok(Json(ListResponse { items, total }))
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
        SELECT id, project_id, name, description, persistent_session, created_at, updated_at
        FROM platform_commands
        WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("command".into()))?;

    // Project-scoped commands require project read access
    if let Some(pid) = row.project_id {
        super::helpers::require_project_read(&state, &auth, pid).await?;
    }

    Ok(Json(CommandResponse {
        id: row.id,
        project_id: row.project_id,
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
    // Fetch existing to check permissions
    let existing = sqlx::query!("SELECT project_id FROM platform_commands WHERE id = $1", id,)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| ApiError::NotFound("command".into()))?;

    require_command_write(&state, &auth, existing.project_id).await?;

    // Validate template and description if provided
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
        RETURNING id, project_id, name, description, persistent_session, created_at, updated_at
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
    // Fetch existing to check permissions
    let existing = sqlx::query!(
        "SELECT project_id, name FROM platform_commands WHERE id = $1",
        id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("command".into()))?;

    require_command_write(&state, &auth, existing.project_id).await?;

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
///
/// Used by clients to preview what a command will expand to.
#[tracing::instrument(skip(state, body), fields(user_id = %auth.user_id), err)]
async fn resolve_command_handler(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<ResolveCommandRequest>,
) -> Result<Json<ResolveCommandResponse>, ApiError> {
    crate::validation::check_length("input", &body.input, 1, 100_000)?;

    // Project-scoped resolution requires project read access
    if let Some(pid) = body.project_id {
        super::helpers::require_project_read(&state, &auth, pid).await?;
    }

    let resolved =
        crate::agent::commands::resolve_command(&state.pool, body.project_id, &body.input).await?;

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

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/commands", get(list_commands).post(create_command))
        // resolve must come before {id} to avoid "resolve" being parsed as a UUID
        .route(
            "/api/commands/resolve",
            axum::routing::post(resolve_command_handler),
        )
        .route(
            "/api/commands/{id}",
            get(get_command).put(update_command).delete(delete_command),
        )
}
