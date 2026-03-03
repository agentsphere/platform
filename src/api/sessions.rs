use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use ts_rs::TS;

use crate::agent::AgentRoleName;
use crate::agent::service;
use crate::audit::{AuditEntry, write_audit};
use crate::auth::middleware::AuthUser;
use crate::error::ApiError;
use crate::rbac::{Permission, resolver};
use crate::store::AppState;
use crate::validation;

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    pub prompt: String,
    pub provider: Option<String>,
    pub branch: Option<String>,
    pub config: Option<serde_json::Value>,
    /// Agent role: "dev" (default), "ops", "test", "review", "manager".
    pub role: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ListSessionsParams {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub status: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SendMessageRequest {
    pub content: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // config + allowed_child_roles used when child pod spawning is implemented
pub struct SpawnChildRequest {
    pub prompt: String,
    pub config: Option<serde_json::Value>,
    pub allowed_child_roles: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct CreateAppRequest {
    pub description: String,
    pub provider: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateSessionRequest {
    pub project_id: Option<Uuid>,
}

#[derive(Debug, Serialize, TS)]
#[ts(export, rename = "AgentSession")]
pub struct SessionResponse {
    pub id: Uuid,
    pub project_id: Option<Uuid>,
    pub user_id: Uuid,
    pub agent_user_id: Option<Uuid>,
    pub prompt: String,
    pub status: String,
    pub branch: Option<String>,
    pub pod_name: Option<String>,
    pub provider: String,
    #[ts(type = "number | null")]
    pub cost_tokens: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub browser_enabled: bool,
    pub execution_mode: String,
}

#[derive(Debug, Serialize, TS)]
#[ts(export, rename = "SessionDetail")]
pub struct SessionDetailResponse {
    #[serde(flatten)]
    pub session: SessionResponse,
    pub messages: Vec<MessageResponse>,
}

#[derive(Debug, Serialize, TS)]
#[ts(export, rename = "SessionMessage")]
pub struct MessageResponse {
    pub id: Uuid,
    pub role: String,
    pub content: String,
    pub metadata: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

use super::helpers::ListResponse;

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/projects/{id}/sessions",
            get(list_sessions).post(create_session),
        )
        .route("/api/projects/{id}/sessions/{session_id}", get(get_session))
        .route(
            "/api/projects/{id}/sessions/{session_id}/message",
            axum::routing::post(send_message),
        )
        .route(
            "/api/projects/{id}/sessions/{session_id}/stop",
            axum::routing::post(stop_session),
        )
        .route(
            "/api/projects/{id}/sessions/{session_id}/spawn",
            axum::routing::post(spawn_child),
        )
        .route(
            "/api/projects/{id}/sessions/{session_id}/children",
            get(list_children),
        )
        .route(
            "/api/projects/{id}/sessions/{session_id}/events",
            get(sse_session_events),
        )
        // Global (project-less) endpoints
        .route("/api/create-app", axum::routing::post(create_app))
        .route(
            "/api/sessions/{session_id}",
            axum::routing::patch(update_session),
        )
        .route(
            "/api/sessions/{session_id}/message",
            axum::routing::post(send_message_global),
        )
        .route(
            "/api/sessions/{session_id}/events",
            get(sse_session_events_global),
        )
}

// ---------------------------------------------------------------------------
// Permission helpers
// ---------------------------------------------------------------------------

use super::helpers::require_project_read;

async fn require_agent_run(
    state: &AppState,
    auth: &AuthUser,
    project_id: Uuid,
) -> Result<(), ApiError> {
    auth.check_project_scope(project_id)?;
    let allowed = resolver::has_permission_scoped(
        &state.pool,
        &state.valkey,
        auth.user_id,
        Some(project_id),
        Permission::AgentRun,
        auth.token_scopes.as_deref(),
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

/// Session owner or project:write can mutate sessions.
async fn require_session_write(
    state: &AppState,
    auth: &AuthUser,
    project_id: Uuid,
    session_user_id: Uuid,
) -> Result<(), ApiError> {
    auth.check_project_scope(project_id)?;
    if auth.user_id == session_user_id {
        return Ok(());
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

/// Validate provider config fields (image, `setup_commands`, browser).
fn validate_provider_config(config: &serde_json::Value) -> Result<(), ApiError> {
    let Ok(parsed) =
        serde_json::from_value::<crate::agent::provider::ProviderConfig>(config.clone())
    else {
        return Ok(());
    };
    if let Some(ref image) = parsed.image {
        validation::check_container_image(image)?;
    }
    if let Some(ref commands) = parsed.setup_commands {
        validation::check_setup_commands(commands)?;
    }
    if let Some(ref role) = parsed.role
        && crate::agent::provider::resolve_role(role).is_none()
    {
        return Err(ApiError::BadRequest(format!(
            "invalid agent role: '{role}'. Valid roles: dev, ops, manager, test, review, admin, ui"
        )));
    }
    if let Some(ref browser) = parsed.browser {
        validation::check_browser_config(browser)?;
        let role = parsed.role.as_deref().unwrap_or("dev");
        if !matches!(role, "ui" | "test") {
            return Err(ApiError::BadRequest(
                "browser access is only available for 'ui' and 'test' roles".into(),
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state, body), fields(%id), err)]
async fn create_session(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<CreateSessionRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_agent_run(&state, &auth, id).await?;

    // Rate limit: 10 session creations per 5 minutes per user
    crate::auth::rate_limit::check_rate(
        &state.valkey,
        "agent_session",
        &auth.user_id.to_string(),
        10,
        300,
    )
    .await?;

    // Input validation
    validation::check_length("prompt", &body.prompt, 1, 100_000)?;
    let provider = body.provider.as_deref().unwrap_or("claude-code");
    validation::check_length("provider", provider, 1, 50)?;
    if let Some(ref branch) = body.branch {
        validation::check_branch_name(branch)?;
    }

    // Parse agent role (default: "dev")
    let role_str = body.role.as_deref().unwrap_or("dev");
    let agent_role: AgentRoleName = role_str.parse().map_err(|_| {
        ApiError::BadRequest(format!(
            "invalid agent role: '{role_str}'. Valid roles: dev, ops, test, review, manager"
        ))
    })?;

    // Validate provider config if provided
    if let Some(ref config) = body.config {
        validate_provider_config(config)?;
    }

    // Verify project exists
    sqlx::query!(
        "SELECT id FROM projects WHERE id = $1 AND is_active = true",
        id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("project".into()))?;

    // Create session (identity + pod)
    let session = service::create_session(
        &state,
        auth.user_id,
        id,
        &body.prompt,
        provider,
        body.branch.as_deref(),
        body.config,
        agent_role,
    )
    .await
    .map_err(ApiError::from)?;

    // Audit log (never log prompt content)
    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "agent_session.create",
            resource: "agent_session",
            resource_id: Some(session.id),
            project_id: Some(id),
            detail: Some(serde_json::json!({
                "provider": provider,
                "branch": session.branch,
                "role": role_str,
            })),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    // Fire webhook
    crate::api::webhooks::fire_webhooks(
        &state.pool,
        id,
        "agent",
        &serde_json::json!({"action": "created", "session_id": session.id}),
    )
    .await;

    Ok((
        StatusCode::CREATED,
        Json(session_to_response(&session, false)),
    ))
}

async fn list_sessions(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Query(params): Query<ListSessionsParams>,
) -> Result<Json<ListResponse<SessionResponse>>, ApiError> {
    require_project_read(&state, &auth, id).await?;

    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);

    let total = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) as "count!: i64" FROM agent_sessions
        WHERE project_id = $1 AND ($2::text IS NULL OR status = $2)
        "#,
        id,
        params.status,
    )
    .fetch_one(&state.pool)
    .await?;

    let rows = sqlx::query!(
        r#"
        SELECT id, project_id, user_id, agent_user_id, prompt, status, branch, pod_name,
               provider, cost_tokens, created_at, finished_at, execution_mode
        FROM agent_sessions
        WHERE project_id = $1 AND ($2::text IS NULL OR status = $2)
        ORDER BY created_at DESC
        LIMIT $3 OFFSET $4
        "#,
        id,
        params.status,
        limit,
        offset,
    )
    .fetch_all(&state.pool)
    .await?;

    let items = rows
        .into_iter()
        .map(|r| SessionResponse {
            id: r.id,
            project_id: r.project_id,
            user_id: r.user_id,
            agent_user_id: r.agent_user_id,
            prompt: r.prompt.chars().take(200).collect(), // Truncate in list view
            status: r.status,
            branch: r.branch,
            pod_name: r.pod_name,
            provider: r.provider,
            cost_tokens: r.cost_tokens,
            created_at: r.created_at,
            finished_at: r.finished_at,
            browser_enabled: false, // List view doesn't load provider_config
            execution_mode: r.execution_mode,
        })
        .collect();

    Ok(Json(ListResponse { items, total }))
}

async fn get_session(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, session_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<SessionDetailResponse>, ApiError> {
    require_project_read(&state, &auth, id).await?;

    let session = service::fetch_session(&state.pool, session_id)
        .await
        .map_err(ApiError::from)?;

    if session.project_id != Some(id) {
        return Err(ApiError::NotFound("session".into()));
    }

    let messages = sqlx::query!(
        r#"
        SELECT id, role, content, metadata, created_at
        FROM agent_messages
        WHERE session_id = $1
        ORDER BY created_at ASC
        LIMIT 100
        "#,
        session_id,
    )
    .fetch_all(&state.pool)
    .await?;

    let messages = messages
        .into_iter()
        .map(|m| MessageResponse {
            id: m.id,
            role: m.role,
            content: m.content,
            metadata: m.metadata,
            created_at: m.created_at,
        })
        .collect();

    Ok(Json(SessionDetailResponse {
        session: session_to_response(&session, false),
        messages,
    }))
}

#[tracing::instrument(skip(state, body), fields(%id, %session_id), err)]
async fn send_message(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, session_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<SendMessageRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    validation::check_length("content", &body.content, 1, 100_000)?;

    let session = service::fetch_session(&state.pool, session_id)
        .await
        .map_err(ApiError::from)?;

    if session.project_id != Some(id) {
        return Err(ApiError::NotFound("session".into()));
    }

    require_session_write(&state, &auth, id, session.user_id).await?;

    service::send_message(&state, session_id, &body.content)
        .await
        .map_err(ApiError::from)?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "agent_session.message",
            resource: "agent_session",
            resource_id: Some(session_id),
            project_id: Some(id),
            detail: None, // Never log message content
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(Json(serde_json::json!({"ok": true})))
}

#[tracing::instrument(skip(state), fields(%id, %session_id), err)]
async fn stop_session(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, session_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let session = service::fetch_session(&state.pool, session_id)
        .await
        .map_err(ApiError::from)?;

    if session.project_id != Some(id) {
        return Err(ApiError::NotFound("session".into()));
    }

    require_session_write(&state, &auth, id, session.user_id).await?;

    service::stop_session(&state, session_id)
        .await
        .map_err(ApiError::from)?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "agent_session.stop",
            resource: "agent_session",
            resource_id: Some(session_id),
            project_id: Some(id),
            detail: None,
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    crate::api::webhooks::fire_webhooks(
        &state.pool,
        id,
        "agent",
        &serde_json::json!({"action": "stopped", "session_id": session_id}),
    )
    .await;

    Ok(Json(serde_json::json!({"ok": true})))
}

// ---------------------------------------------------------------------------
// Create App (project-less session)
// ---------------------------------------------------------------------------

/// Create a project-less agent session for the "Create App" flow.
#[tracing::instrument(skip(state, body), err)]
async fn create_app(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<CreateAppRequest>,
) -> Result<(StatusCode, Json<SessionResponse>), ApiError> {
    // Project-scoped tokens cannot create project-less sessions
    if auth.scope_project_id.is_some() {
        return Err(ApiError::Forbidden);
    }

    // Rate limit: 5 create-app sessions per 10 minutes per user
    crate::auth::rate_limit::check_rate(
        &state.valkey,
        "create_app",
        &auth.user_id.to_string(),
        5,
        600,
    )
    .await?;

    validation::check_length("description", &body.description, 1, 100_000)?;
    let provider = body.provider.as_deref().unwrap_or("claude-code");
    validation::check_length("provider", provider, 1, 50)?;

    // Check that the user has project:write and agent:run (global scope)
    let has_write = resolver::has_permission_scoped(
        &state.pool,
        &state.valkey,
        auth.user_id,
        None,
        Permission::ProjectWrite,
        auth.token_scopes.as_deref(),
    )
    .await
    .map_err(ApiError::Internal)?;
    let has_run = resolver::has_permission_scoped(
        &state.pool,
        &state.valkey,
        auth.user_id,
        None,
        Permission::AgentRun,
        auth.token_scopes.as_deref(),
    )
    .await
    .map_err(ApiError::Internal)?;
    if !has_write || !has_run {
        return Err(ApiError::Forbidden);
    }

    // Create project-less session via service
    let session = service::create_global_session(&state, auth.user_id, &body.description, provider)
        .await
        .map_err(ApiError::from)?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "agent_session.create_app",
            resource: "agent_session",
            resource_id: Some(session.id),
            project_id: None,
            detail: Some(serde_json::json!({"provider": provider})),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok((
        StatusCode::CREATED,
        Json(session_to_response(&session, false)),
    ))
}

/// Link a project-less session to a newly created project.
#[tracing::instrument(skip(state, body), fields(%session_id), err)]
async fn update_session(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(session_id): Path<Uuid>,
    Json(body): Json<UpdateSessionRequest>,
) -> Result<Json<SessionResponse>, ApiError> {
    let session = service::fetch_session(&state.pool, session_id)
        .await
        .map_err(ApiError::from)?;

    // Only the session owner can update it
    if session.user_id != auth.user_id {
        return Err(ApiError::Forbidden);
    }

    if let Some(project_id) = body.project_id {
        // Verify project exists
        let exists: Option<(Uuid,)> =
            sqlx::query_as("SELECT id FROM projects WHERE id = $1 AND is_active = true")
                .bind(project_id)
                .fetch_optional(&state.pool)
                .await?;
        if exists.is_none() {
            return Err(ApiError::NotFound("project".into()));
        }

        // Verify user has project write access
        let allowed = crate::rbac::resolver::has_permission(
            &state.pool,
            &state.valkey,
            auth.user_id,
            Some(project_id),
            crate::rbac::Permission::ProjectWrite,
        )
        .await
        .map_err(ApiError::Internal)?;
        if !allowed {
            return Err(ApiError::Forbidden);
        }

        sqlx::query("UPDATE agent_sessions SET project_id = $2 WHERE id = $1")
            .bind(session_id)
            .bind(project_id)
            .execute(&state.pool)
            .await?;

        crate::audit::write_audit(
            &state.pool,
            &crate::audit::AuditEntry {
                actor_id: auth.user_id,
                actor_name: &auth.user_name,
                action: "agent_session.update",
                resource: "agent_session",
                resource_id: Some(session_id),
                project_id: Some(project_id),
                detail: None,
                ip_addr: auth.ip_addr.as_deref(),
            },
        )
        .await;
    }

    let updated = service::fetch_session(&state.pool, session_id)
        .await
        .map_err(ApiError::from)?;

    Ok(Json(session_to_response(&updated, false)))
}

// ---------------------------------------------------------------------------
// Spawn
// ---------------------------------------------------------------------------

const MAX_SPAWN_DEPTH: i32 = 5;

/// Spawn a child agent session from a parent session.
#[tracing::instrument(skip(state, body), fields(%id, %session_id), err)]
async fn spawn_child(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, session_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<SpawnChildRequest>,
) -> Result<(StatusCode, Json<SessionResponse>), ApiError> {
    validation::check_length("prompt", &body.prompt, 1, 100_000)?;

    // Enforce hard project scope from API token
    auth.check_project_scope(id)?;

    // Check agent:spawn permission
    let allowed = resolver::has_permission_scoped(
        &state.pool,
        &state.valkey,
        auth.user_id,
        Some(id),
        Permission::AgentSpawn,
        auth.token_scopes.as_deref(),
    )
    .await
    .map_err(ApiError::Internal)?;
    if !allowed {
        return Err(ApiError::Forbidden);
    }

    // Fetch parent session
    let parent = service::fetch_session(&state.pool, session_id)
        .await
        .map_err(ApiError::from)?;

    if parent.project_id != Some(id) {
        return Err(ApiError::NotFound("session".into()));
    }

    // Check spawn depth
    if parent.spawn_depth >= MAX_SPAWN_DEPTH {
        return Err(ApiError::BadRequest(format!(
            "spawn depth limit reached (max {MAX_SPAWN_DEPTH})"
        )));
    }

    // Create child session record
    let child_id = Uuid::new_v4();
    let child_depth = parent.spawn_depth + 1;
    let allowed_roles = body.allowed_child_roles.as_deref();

    sqlx::query!(
        r#"
        INSERT INTO agent_sessions (id, project_id, user_id, prompt, provider, parent_session_id, spawn_depth, allowed_child_roles)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        "#,
        child_id,
        id,
        parent.user_id, // Child inherits the original human user
        body.prompt,
        parent.provider,
        session_id,
        child_depth,
        allowed_roles as Option<&[String]>,
    )
    .execute(&state.pool)
    .await?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "agent_session.spawn",
            resource: "agent_session",
            resource_id: Some(child_id),
            project_id: Some(id),
            detail: Some(serde_json::json!({
                "parent_session_id": session_id,
                "spawn_depth": child_depth,
            })),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    // Fetch the created child to return
    let child = service::fetch_session(&state.pool, child_id)
        .await
        .map_err(ApiError::from)?;

    Ok((
        StatusCode::CREATED,
        Json(session_to_response(&child, false)),
    ))
}

/// List child sessions spawned from a parent session.
async fn list_children(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, session_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Vec<SessionResponse>>, ApiError> {
    require_project_read(&state, &auth, id).await?;

    let children = sqlx::query!(
        r#"
        SELECT id, project_id, user_id, agent_user_id, prompt, status, branch, pod_name,
               provider, cost_tokens, created_at, finished_at, spawn_depth, execution_mode
        FROM agent_sessions
        WHERE parent_session_id = $1 AND project_id = $2
        ORDER BY created_at
        "#,
        session_id,
        id,
    )
    .fetch_all(&state.pool)
    .await?;

    let items = children
        .into_iter()
        .map(|r| SessionResponse {
            id: r.id,
            project_id: r.project_id,
            user_id: r.user_id,
            agent_user_id: r.agent_user_id,
            prompt: truncate_prompt(&r.prompt, 200),
            status: r.status,
            branch: r.branch,
            pod_name: r.pod_name,
            provider: r.provider,
            cost_tokens: r.cost_tokens,
            created_at: r.created_at,
            finished_at: r.finished_at,
            browser_enabled: false,
            execution_mode: r.execution_mode,
        })
        .collect();

    Ok(Json(items))
}

fn truncate_prompt(prompt: &str, max_chars: usize) -> String {
    if prompt.chars().count() <= max_chars {
        prompt.to_owned()
    } else {
        let truncated: String = prompt.chars().take(max_chars).collect();
        format!("{truncated}...")
    }
}

fn session_to_response(
    session: &crate::agent::provider::AgentSession,
    truncate: bool,
) -> SessionResponse {
    // Detect browser config from provider_config JSON
    let browser_enabled = session
        .provider_config
        .as_ref()
        .and_then(|v| v.get("browser"))
        .is_some();

    SessionResponse {
        id: session.id,
        project_id: session.project_id,
        user_id: session.user_id,
        agent_user_id: session.agent_user_id,
        prompt: if truncate {
            truncate_prompt(&session.prompt, 200)
        } else {
            session.prompt.clone()
        },
        status: session.status.clone(),
        branch: session.branch.clone(),
        pod_name: session.pod_name.clone(),
        provider: session.provider.clone(),
        cost_tokens: session.cost_tokens,
        created_at: session.created_at,
        finished_at: session.finished_at,
        browser_enabled,
        execution_mode: session.execution_mode.clone(),
    }
}

// ---------------------------------------------------------------------------
// SSE (Server-Sent Events)
// ---------------------------------------------------------------------------

/// SSE handler: streams agent output in real-time via Valkey pub/sub.
/// Auth is validated via the `AuthUser` extractor (cookies sent by `EventSource`).
async fn sse_session_events(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, session_id)): Path<(Uuid, Uuid)>,
) -> Result<impl IntoResponse, ApiError> {
    require_project_read(&state, &auth, id).await?;

    let session = service::fetch_session(&state.pool, session_id)
        .await
        .map_err(ApiError::from)?;

    if session.project_id != Some(id) {
        return Err(ApiError::NotFound("session".into()));
    }

    let rx = crate::agent::pubsub_bridge::subscribe_session_events(&state.valkey, session_id)
        .await
        .map_err(ApiError::Internal)?;

    let stream = ReceiverStream::new(rx).map(|event| {
        let json = serde_json::to_string(&event).unwrap_or_default();
        Ok::<_, std::convert::Infallible>(Event::default().event("progress").data(json))
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

// ---------------------------------------------------------------------------
// Global (project-less) message + SSE handlers
// ---------------------------------------------------------------------------

/// Send a message to a global (project-less) session.
async fn send_message_global(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(session_id): Path<Uuid>,
    Json(body): Json<SendMessageRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let session = service::fetch_session(&state.pool, session_id)
        .await
        .map_err(ApiError::from)?;

    // Only the session owner can send messages
    if session.user_id != auth.user_id {
        return Err(ApiError::Forbidden);
    }

    validation::check_length("content", &body.content, 1, 100_000)?;

    service::send_message(&state, session_id, &body.content)
        .await
        .map_err(ApiError::from)?;

    Ok(Json(serde_json::json!({"ok": true})))
}

/// SSE handler for global (project-less) sessions.
async fn sse_session_events_global(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(session_id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    let session = service::fetch_session(&state.pool, session_id)
        .await
        .map_err(ApiError::from)?;

    // Only the session owner can connect
    if session.user_id != auth.user_id {
        return Err(ApiError::Forbidden);
    }

    let rx = crate::agent::pubsub_bridge::subscribe_session_events(&state.valkey, session_id)
        .await
        .map_err(ApiError::Internal)?;

    let stream = ReceiverStream::new(rx).map(|event| {
        let json = serde_json::to_string(&event).unwrap_or_default();
        Ok::<_, std::convert::Infallible>(Event::default().event("progress").data(json))
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_provider_config_empty_ok() {
        let config = serde_json::json!({});
        assert!(validate_provider_config(&config).is_ok());
    }

    #[test]
    fn validate_provider_config_valid_image() {
        let config = serde_json::json!({ "image": "alpine:3.19" });
        assert!(validate_provider_config(&config).is_ok());
    }

    #[test]
    fn validate_provider_config_invalid_image() {
        let config = serde_json::json!({ "image": "image;rm -rf /" });
        assert!(validate_provider_config(&config).is_err());
    }

    #[test]
    fn validate_provider_config_too_many_setup_commands() {
        let cmds: Vec<String> = (0..21).map(|i| format!("cmd {i}")).collect();
        let config = serde_json::json!({ "setup_commands": cmds });
        assert!(validate_provider_config(&config).is_err());
    }

    #[test]
    fn validate_provider_config_browser_requires_ui_role() {
        let config = serde_json::json!({
            "browser": { "allowed_origins": ["http://localhost:3000"] },
            "role": "dev"
        });
        let result = validate_provider_config(&config);
        assert!(result.is_err());
    }

    #[test]
    fn validate_provider_config_browser_ui_ok() {
        let config = serde_json::json!({
            "browser": { "allowed_origins": ["http://localhost:3000"] },
            "role": "ui"
        });
        assert!(validate_provider_config(&config).is_ok());
    }

    #[test]
    fn validate_provider_config_browser_test_ok() {
        let config = serde_json::json!({
            "browser": { "allowed_origins": ["http://localhost:3000"] },
            "role": "test"
        });
        assert!(validate_provider_config(&config).is_ok());
    }

    #[test]
    fn truncate_prompt_short() {
        assert_eq!(truncate_prompt("hello", 10), "hello");
    }

    #[test]
    fn truncate_prompt_long() {
        assert_eq!(truncate_prompt("hello world", 5), "hello...");
    }

    #[test]
    fn truncate_prompt_exact_length() {
        assert_eq!(truncate_prompt("exact", 5), "exact");
    }

    #[test]
    fn truncate_prompt_empty() {
        assert_eq!(truncate_prompt("", 10), "");
    }

    #[test]
    fn truncate_prompt_one_char_over() {
        assert_eq!(truncate_prompt("abcdef", 5), "abcde...");
    }

    #[test]
    fn truncate_prompt_zero_max() {
        assert_eq!(truncate_prompt("hello", 0), "...");
    }

    #[test]
    fn validate_provider_config_unparseable_json_ok() {
        // Config that doesn't match ProviderConfig struct → parse fails → returns Ok(())
        let config = serde_json::json!({ "unknown_field_only": 42 });
        // This should not error — unparseable configs are silently accepted
        assert!(validate_provider_config(&config).is_ok());
    }

    #[test]
    fn validate_provider_config_invalid_role() {
        let config = serde_json::json!({ "role": "nonexistent-role" });
        assert!(validate_provider_config(&config).is_err());
    }

    #[test]
    fn validate_provider_config_valid_roles() {
        for role in &["dev", "ops", "test", "review", "manager", "ui"] {
            let config = serde_json::json!({ "role": role });
            assert!(
                validate_provider_config(&config).is_ok(),
                "role '{role}' should be valid"
            );
        }
    }

    #[test]
    fn validate_provider_config_browser_with_manager_role_rejected() {
        let config = serde_json::json!({
            "browser": { "allowed_origins": ["http://localhost:3000"] },
            "role": "manager"
        });
        assert!(validate_provider_config(&config).is_err());
    }

    #[test]
    fn validate_provider_config_browser_with_ops_role_rejected() {
        let config = serde_json::json!({
            "browser": { "allowed_origins": ["http://localhost:3000"] },
            "role": "ops"
        });
        assert!(validate_provider_config(&config).is_err());
    }

    #[test]
    fn validate_provider_config_browser_no_role_defaults_to_dev_rejected() {
        // When no role is specified, defaults to "dev", which is NOT in [ui, test]
        let config = serde_json::json!({
            "browser": { "allowed_origins": ["http://localhost:3000"] }
        });
        assert!(validate_provider_config(&config).is_err());
    }

    #[test]
    fn validate_provider_config_setup_commands_ok() {
        let config = serde_json::json!({
            "setup_commands": ["apt-get update", "pip install -r requirements.txt"]
        });
        assert!(validate_provider_config(&config).is_ok());
    }

    #[test]
    fn session_to_response_no_truncation() {
        let session = crate::agent::provider::AgentSession {
            id: uuid::Uuid::new_v4(),
            project_id: None,
            user_id: uuid::Uuid::new_v4(),
            agent_user_id: None,
            prompt: "Hello world".to_owned(),
            status: "running".to_owned(),
            branch: Some("agent/test".to_owned()),
            pod_name: Some("agent-pod".to_owned()),
            provider: "claude-code".to_owned(),
            provider_config: None,
            cost_tokens: Some(1000),
            created_at: chrono::Utc::now(),
            finished_at: None,
            parent_session_id: None,
            spawn_depth: 0,
            allowed_child_roles: None,
            execution_mode: "pod".to_owned(),
            uses_pubsub: false,
        };

        let response = session_to_response(&session, false);
        assert_eq!(response.prompt, "Hello world");
        assert_eq!(response.status, "running");
        assert!(!response.browser_enabled);
    }

    #[test]
    fn session_to_response_with_truncation() {
        let long_prompt = "a".repeat(300);
        let session = crate::agent::provider::AgentSession {
            id: uuid::Uuid::new_v4(),
            project_id: None,
            user_id: uuid::Uuid::new_v4(),
            agent_user_id: None,
            prompt: long_prompt,
            status: "completed".to_owned(),
            branch: None,
            pod_name: None,
            provider: "claude-code".to_owned(),
            provider_config: None,
            cost_tokens: None,
            created_at: chrono::Utc::now(),
            finished_at: Some(chrono::Utc::now()),
            parent_session_id: None,
            spawn_depth: 0,
            allowed_child_roles: None,
            execution_mode: "pod".to_owned(),
            uses_pubsub: false,
        };

        let response = session_to_response(&session, true);
        assert!(response.prompt.len() <= 203); // 200 + "..."
        assert!(response.prompt.ends_with("..."));
    }

    #[test]
    fn session_to_response_browser_enabled_from_config() {
        let session = crate::agent::provider::AgentSession {
            id: uuid::Uuid::new_v4(),
            project_id: None,
            user_id: uuid::Uuid::new_v4(),
            agent_user_id: None,
            prompt: "test".to_owned(),
            status: "running".to_owned(),
            branch: None,
            pod_name: None,
            provider: "claude-code".to_owned(),
            provider_config: Some(serde_json::json!({
                "browser": {"allowed_origins": ["http://localhost:3000"]}
            })),
            cost_tokens: None,
            created_at: chrono::Utc::now(),
            finished_at: None,
            parent_session_id: None,
            spawn_depth: 0,
            allowed_child_roles: None,
            execution_mode: "pod".to_owned(),
            uses_pubsub: false,
        };

        let response = session_to_response(&session, false);
        assert!(response.browser_enabled);
    }

    #[test]
    fn session_to_response_browser_disabled_without_config() {
        let session = crate::agent::provider::AgentSession {
            id: uuid::Uuid::new_v4(),
            project_id: None,
            user_id: uuid::Uuid::new_v4(),
            agent_user_id: None,
            prompt: "test".to_owned(),
            status: "running".to_owned(),
            branch: None,
            pod_name: None,
            provider: "claude-code".to_owned(),
            provider_config: Some(serde_json::json!({"role": "dev"})),
            cost_tokens: None,
            created_at: chrono::Utc::now(),
            finished_at: None,
            parent_session_id: None,
            spawn_depth: 0,
            allowed_child_roles: None,
            execution_mode: "pod".to_owned(),
            uses_pubsub: false,
        };

        let response = session_to_response(&session, false);
        assert!(!response.browser_enabled);
    }
}
