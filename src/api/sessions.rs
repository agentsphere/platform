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
use crate::audit::{AuditEntry, send_audit};
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
    /// Initial prompt. If omitted, session starts idle and waits for a follow-up message.
    pub prompt: Option<String>,
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
pub struct UpdateSessionRequest {
    pub project_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct CreateManagerSessionRequest {
    pub prompt: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SetManagerModeRequest {
    pub mode: String,
}

#[derive(Debug, Serialize)]
pub struct ManagerSessionResponse {
    pub id: Uuid,
    pub status: String,
    pub prompt: String,
    pub mode: String,
    pub created_at: DateTime<Utc>,
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
            "/api/projects/{id}/sessions/{session_id}/iframes",
            get(list_iframes),
        )
        .route(
            "/api/projects/{id}/sessions/{session_id}/progress",
            get(get_session_progress),
        )
        .route(
            "/api/projects/{id}/sessions/{session_id}/events",
            get(sse_session_events),
        )
        // Global (project-less) endpoints
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
        // Manager agent endpoints
        .route(
            "/api/manager/sessions",
            get(list_manager_sessions).post(create_manager_session_handler),
        )
        .route(
            "/api/manager/sessions/{id}/message",
            axum::routing::post(send_manager_message),
        )
        .route(
            "/api/manager/sessions/{id}/events",
            get(manager_session_events),
        )
        .route(
            "/api/manager/sessions/{id}",
            axum::routing::delete(stop_manager_session),
        )
        .route(
            "/api/manager/sessions/{id}/mode",
            axum::routing::post(set_manager_mode),
        )
        .route(
            "/api/manager/sessions/{id}/approve_action",
            axum::routing::post(approve_manager_action),
        )
        .route(
            "/api/manager/sessions/{id}/approve_tool",
            axum::routing::post(approve_manager_tool),
        )
        .route(
            "/api/manager/sessions/{id}/pending_action",
            axum::routing::post(register_pending_action),
        )
        .route(
            "/api/manager/sessions/{id}/approval/{hash}",
            axum::routing::get(check_action_approval),
        )
        .route(
            "/api/manager/sessions/{id}/reject_action",
            axum::routing::post(reject_manager_action),
        )
        .route(
            "/api/manager/sessions/{id}/rejection/{hash}",
            axum::routing::get(check_action_rejection),
        )
        .route(
            "/api/manager/sessions/{id}/approved_tools",
            axum::routing::get(list_approved_tools),
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
        return Err(ApiError::NotFound("project".into()));
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
        return Err(ApiError::NotFound("project".into()));
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

    // Input validation — prompt is optional; empty means the session starts idle
    // and waits for the first message via pub/sub.
    let prompt = body.prompt.as_deref().unwrap_or("");
    if !prompt.is_empty() {
        validation::check_length("prompt", prompt, 1, 100_000)?;
    }
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
        prompt,
        provider,
        body.branch.as_deref(),
        body.config,
        agent_role,
        None, // No parent session for API-created sessions
    )
    .await
    .map_err(ApiError::from)?;

    // Audit log (never log prompt content)
    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "agent_session.create".into(),
            resource: "agent_session".into(),
            resource_id: Some(session.id),
            project_id: Some(id),
            detail: Some(serde_json::json!({
                "provider": provider,
                "branch": session.branch,
                "role": role_str,
            })),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    // Fire webhook
    crate::api::webhooks::fire_webhooks(
        &state.pool,
        id,
        "agent",
        &serde_json::json!({"action": "created", "session_id": session.id}),
        &state.webhook_semaphore,
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

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "agent_session.message".into(),
            resource: "agent_session".into(),
            resource_id: Some(session_id),
            project_id: Some(id),
            detail: None, // Never log message content
            ip_addr: auth.ip_addr.clone(),
        },
    );

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

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "agent_session.stop".into(),
            resource: "agent_session".into(),
            resource_id: Some(session_id),
            project_id: Some(id),
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    crate::api::webhooks::fire_webhooks(
        &state.pool,
        id,
        "agent",
        &serde_json::json!({"action": "stopped", "session_id": session_id}),
        &state.webhook_semaphore,
    )
    .await;

    Ok(Json(serde_json::json!({"ok": true})))
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
        let allowed = crate::rbac::resolver::has_permission_scoped(
            &state.pool,
            &state.valkey,
            auth.user_id,
            Some(project_id),
            crate::rbac::Permission::ProjectWrite,
            auth.token_scopes.as_deref(),
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

        crate::audit::send_audit(
            &state.audit_tx,
            crate::audit::AuditEntry {
                actor_id: auth.user_id,
                actor_name: auth.user_name.clone(),
                action: "agent_session.update".into(),
                resource: "agent_session".into(),
                resource_id: Some(session_id),
                project_id: Some(project_id),
                detail: None,
                ip_addr: auth.ip_addr.clone(),
            },
        );
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
        return Err(ApiError::NotFound("project".into()));
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

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "agent_session.spawn".into(),
            resource: "agent_session".into(),
            resource_id: Some(child_id),
            project_id: Some(id),
            detail: Some(serde_json::json!({
                "parent_session_id": session_id,
                "spawn_depth": child_depth,
            })),
            ip_addr: auth.ip_addr.clone(),
        },
    );

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
) -> Result<Json<ListResponse<SessionResponse>>, ApiError> {
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

    let items: Vec<SessionResponse> = children
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

    let total = i64::try_from(items.len()).unwrap_or(0);
    Ok(Json(ListResponse { items, total }))
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
// Iframe panels
// ---------------------------------------------------------------------------

/// Response type for iframe panels discovered from K8s Services.
#[derive(Debug, Serialize)]
struct IframePanel {
    service_name: String,
    port: i32,
    port_name: String,
    preview_url: String,
}

/// List iframe panels for a session (queries K8s Services in the session namespace).
#[tracing::instrument(skip(state, auth), fields(%id, %session_id), err)]
async fn list_iframes(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, session_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<ListResponse<IframePanel>>, ApiError> {
    require_project_read(&state, &auth, id).await?;

    let session = service::fetch_session(&state.pool, session_id)
        .await
        .map_err(ApiError::from)?;

    if session.project_id != Some(id) {
        return Err(ApiError::NotFound("session".into()));
    }

    let ns = session
        .session_namespace
        .as_deref()
        .ok_or_else(|| ApiError::NotFound("session namespace".into()))?;

    // Defence-in-depth: namespace comes from DB but validate format before K8s API call
    if ns.is_empty()
        || !ns
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(ApiError::NotFound("session namespace".into()));
    }

    let api: kube::Api<k8s_openapi::api::core::v1::Service> =
        kube::Api::namespaced(state.kube.clone(), ns);
    let lp = kube::api::ListParams::default().labels("platform.io/component=iframe-preview");
    let svcs = api
        .list(&lp)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;

    let panels: Vec<IframePanel> = svcs
        .items
        .iter()
        .flat_map(|svc| {
            let spec = svc.spec.as_ref();
            let ports = spec.and_then(|s| s.ports.as_ref());
            let name = svc.metadata.name.clone().unwrap_or_default();
            ports
                .into_iter()
                .flatten()
                .filter(|p| p.name.as_deref() == Some("iframe"))
                .map(move |p| IframePanel {
                    service_name: name.clone(),
                    port: p.port,
                    port_name: "iframe".into(),
                    preview_url: format!("/preview/{session_id}/"),
                })
        })
        .collect();

    let total = i64::try_from(panels.len()).unwrap_or(0);
    Ok(Json(ListResponse {
        items: panels,
        total,
    }))
}

// ---------------------------------------------------------------------------
// SSE (Server-Sent Events)
// ---------------------------------------------------------------------------

/// Get the latest progress update for a session.
///
/// Returns the most recent `progress_update` message content, or 404 if none exists.
async fn get_session_progress(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, session_id)): Path<(Uuid, Uuid)>,
) -> Result<impl IntoResponse, ApiError> {
    require_project_read(&state, &auth, id).await?;

    let row: Option<(String,)> = sqlx::query_as(
        r"SELECT content FROM agent_messages
        WHERE session_id = $1
          AND role = 'progress_update'
        ORDER BY created_at DESC LIMIT 1",
    )
    .bind(session_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;

    match row {
        Some((content,)) => Ok(Json(serde_json::json!({ "message": content }))),
        None => Err(ApiError::NotFound("progress".into())),
    }
}

/// SSE handler: streams agent output in real-time via Valkey pub/sub.
/// Auth is validated via the `AuthUser` extractor (cookies sent by `EventSource`).
///
/// Replays any stored events from `agent_messages` first (so late-connecting
/// clients don't miss events published before SSE subscription), then streams
/// live pub/sub events.
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

    // Replay stored events from DB (persisted by the persistence subscriber)
    let stored = replay_stored_events(&state.pool, session_id).await;

    // Subscribe to live events
    let rx = crate::agent::pubsub_bridge::subscribe_session_events(&state.valkey, session_id)
        .await
        .map_err(ApiError::Internal)?;

    let replay_stream =
        tokio_stream::iter(stored.into_iter().map(Ok::<_, std::convert::Infallible>));
    let live_stream = ReceiverStream::new(rx).map(Ok::<_, std::convert::Infallible>);
    let stream = replay_stream.chain(live_stream).map(|event| {
        let json =
            serde_json::to_string(&event.expect("Infallible error type")).unwrap_or_default();
        Ok::<_, std::convert::Infallible>(Event::default().event("progress").data(json))
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// Load stored events from `agent_messages` and convert back to `ProgressEvent`.
async fn replay_stored_events(
    pool: &sqlx::PgPool,
    session_id: Uuid,
) -> Vec<crate::agent::provider::ProgressEvent> {
    use sqlx::Row;

    let rows = sqlx::query(
        "SELECT role, content, metadata FROM agent_messages WHERE session_id = $1 ORDER BY created_at",
    )
    .bind(session_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    rows.into_iter()
        .filter_map(|row| {
            let kind_str: String = row.get("role");
            let content: String = row.get("content");
            let metadata: Option<serde_json::Value> = row.get("metadata");
            // Parse kind from the stored role string (e.g. "Milestone", "WaitingForInput")
            let kind_json = format!("\"{kind_str}\"");
            let kind: crate::agent::provider::ProgressKind =
                serde_json::from_str(&kind_json).ok()?;
            Some(crate::agent::provider::ProgressEvent {
                kind,
                message: content,
                metadata,
            })
        })
        .collect()
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

/// Query parameters for the global SSE endpoint.
#[derive(Debug, Deserialize)]
struct SseGlobalParams {
    /// When true, also stream events from child sessions.
    include_children: Option<bool>,
}

/// SSE handler for global (project-less) sessions.
///
/// With `?include_children=true`, streams events from the parent session AND
/// all its current child sessions. Each event includes a `session_id` field
/// so the client can distinguish which session emitted it.
async fn sse_session_events_global(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(session_id): Path<Uuid>,
    Query(params): Query<SseGlobalParams>,
) -> Result<impl IntoResponse, ApiError> {
    let session = service::fetch_session(&state.pool, session_id)
        .await
        .map_err(ApiError::from)?;

    // Only the session owner can connect
    if session.user_id != auth.user_id {
        return Err(ApiError::Forbidden);
    }

    // Build list of session IDs to subscribe to
    let include_children = params.include_children.unwrap_or(false);
    let mut all_ids = vec![session_id];
    if include_children {
        let child_ids: Vec<Uuid> =
            sqlx::query_scalar("SELECT id FROM agent_sessions WHERE parent_session_id = $1")
                .bind(session_id)
                .fetch_all(&state.pool)
                .await
                .map_err(|e| ApiError::Internal(e.into()))?;
        all_ids.extend(child_ids);
    }

    let rx = crate::agent::pubsub_bridge::subscribe_session_tree_events(&state.valkey, &all_ids)
        .await
        .map_err(ApiError::Internal)?;

    let stream = ReceiverStream::new(rx).map(move |(sid, event)| {
        if include_children {
            // Include session_id so client knows which session emitted the event
            let mut json = serde_json::to_value(&event).unwrap_or_default();
            if let Some(obj) = json.as_object_mut() {
                obj.insert("session_id".into(), serde_json::json!(sid));
            }
            let data = serde_json::to_string(&json).unwrap_or_default();
            Ok::<_, std::convert::Infallible>(Event::default().event("progress").data(data))
        } else {
            let data = serde_json::to_string(&event).unwrap_or_default();
            Ok::<_, std::convert::Infallible>(Event::default().event("progress").data(data))
        }
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

// ---------------------------------------------------------------------------
// Manager Agent endpoints
// ---------------------------------------------------------------------------

/// Valid permission modes for manager sessions.
const VALID_MANAGER_MODES: &[&str] = &["plan", "guided", "auto_read", "auto_write", "full_auto"];

/// Create a new manager session.
#[tracing::instrument(skip(state, body), err)]
async fn create_manager_session_handler(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<CreateManagerSessionRequest>,
) -> Result<(StatusCode, Json<ManagerSessionResponse>), ApiError> {
    // Project-scoped tokens cannot create manager sessions
    if auth.boundary_project_id.is_some() {
        return Err(ApiError::Forbidden);
    }

    // Rate limit: 30 manager session creations per 5 minutes per user
    crate::auth::rate_limit::check_rate(
        &state.valkey,
        "manager_session",
        &auth.user_id.to_string(),
        30,
        300,
    )
    .await?;

    if let Some(ref prompt) = body.prompt {
        validation::check_length("prompt", prompt, 1, 100_000)?;
    }

    let session_id = service::create_manager_session(&state, auth.user_id, body.prompt)
        .await
        .map_err(ApiError::from)?;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "manager_session.create".into(),
            resource: "manager_session".into(),
            resource_id: Some(session_id),
            project_id: None,
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    // Read back session for response
    let session = service::fetch_session(&state.pool, session_id)
        .await
        .map_err(ApiError::from)?;

    Ok((
        StatusCode::CREATED,
        Json(ManagerSessionResponse {
            id: session.id,
            status: session.status,
            prompt: session.prompt,
            mode: "auto_read".into(),
            created_at: session.created_at,
        }),
    ))
}

/// List current user's manager sessions.
async fn list_manager_sessions(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(params): Query<ListSessionsParams>,
) -> Result<Json<ListResponse<ManagerSessionResponse>>, ApiError> {
    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);

    let total: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM agent_sessions WHERE user_id = $1 AND execution_mode = 'manager'",
    )
    .bind(auth.user_id)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;

    let rows: Vec<(Uuid, String, String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT id, status, prompt, created_at FROM agent_sessions
         WHERE user_id = $1 AND execution_mode = 'manager'
         ORDER BY created_at DESC LIMIT $2 OFFSET $3",
    )
    .bind(auth.user_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;

    // Read modes from Valkey (best-effort, default to auto_read)
    let mut items = Vec::with_capacity(rows.len());
    for (id, status, prompt, created_at) in rows {
        let mode = read_manager_mode(&state.valkey, id).await;
        items.push(ManagerSessionResponse {
            id,
            status,
            prompt: truncate_prompt(&prompt, 200),
            mode,
            created_at,
        });
    }

    Ok(Json(ListResponse {
        items,
        total: total.0,
    }))
}

/// Send a message to a running manager session.
#[tracing::instrument(skip(state, body), fields(%id), err)]
async fn send_manager_message(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<SendMessageRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    validation::check_length("content", &body.content, 1, 100_000)?;

    let session = service::fetch_session(&state.pool, id)
        .await
        .map_err(ApiError::from)?;

    if session.user_id != auth.user_id {
        return Err(ApiError::NotFound("session".into()));
    }
    if session.execution_mode != "manager" {
        return Err(ApiError::NotFound("session".into()));
    }

    service::send_manager_message(&state, id, &body.content)
        .await
        .map_err(ApiError::from)?;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "manager_session.message".into(),
            resource: "manager_session".into(),
            resource_id: Some(id),
            project_id: None,
            detail: None, // Never log message content
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(Json(serde_json::json!({"ok": true})))
}

/// SSE events for a manager session.
async fn manager_session_events(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    let session = service::fetch_session(&state.pool, id)
        .await
        .map_err(ApiError::from)?;

    if session.user_id != auth.user_id {
        return Err(ApiError::NotFound("session".into()));
    }
    if session.execution_mode != "manager" {
        return Err(ApiError::NotFound("session".into()));
    }

    // Replay stored events from DB
    let stored = replay_stored_events(&state.pool, id).await;

    // Subscribe to live events
    let rx = crate::agent::pubsub_bridge::subscribe_session_events(&state.valkey, id)
        .await
        .map_err(ApiError::Internal)?;

    let replay_stream =
        tokio_stream::iter(stored.into_iter().map(Ok::<_, std::convert::Infallible>));
    let live_stream = ReceiverStream::new(rx).map(Ok::<_, std::convert::Infallible>);
    let stream = replay_stream.chain(live_stream).map(|event| {
        let json =
            serde_json::to_string(&event.expect("Infallible error type")).unwrap_or_default();
        Ok::<_, std::convert::Infallible>(Event::default().event("progress").data(json))
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// Stop a manager session.
#[tracing::instrument(skip(state), fields(%id), err)]
async fn stop_manager_session(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let session = service::fetch_session(&state.pool, id)
        .await
        .map_err(ApiError::from)?;

    if session.user_id != auth.user_id {
        return Err(ApiError::NotFound("session".into()));
    }
    if session.execution_mode != "manager" {
        return Err(ApiError::NotFound("session".into()));
    }

    service::stop_session(&state, id)
        .await
        .map_err(ApiError::from)?;

    // Clean up MCP config temp file
    let mcp_path = format!("/tmp/manager-mcp-{id}.json");
    let _ = tokio::fs::remove_file(&mcp_path).await;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "manager_session.stop".into(),
            resource: "manager_session".into(),
            resource_id: Some(id),
            project_id: None,
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(Json(serde_json::json!({"ok": true})))
}

/// Set the permission mode for a manager session.
#[tracing::instrument(skip(state, body), fields(%id), err)]
async fn set_manager_mode(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<SetManagerModeRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !VALID_MANAGER_MODES.contains(&body.mode.as_str()) {
        return Err(ApiError::BadRequest(format!(
            "invalid mode: '{}'. Valid modes: {}",
            body.mode,
            VALID_MANAGER_MODES.join(", ")
        )));
    }

    let session = service::fetch_session(&state.pool, id)
        .await
        .map_err(ApiError::from)?;

    if session.user_id != auth.user_id {
        return Err(ApiError::NotFound("session".into()));
    }
    if session.execution_mode != "manager" {
        return Err(ApiError::NotFound("session".into()));
    }

    // Store mode in Valkey (4h TTL matching session token)
    {
        let mode_key = format!("manager:{id}:mode");
        state
            .valkey
            .next()
            .set::<(), _, _>(
                &mode_key,
                body.mode.as_str(),
                Some(fred::types::Expiration::EX(4 * 3600)),
                None,
                false,
            )
            .await
            .map_err(|e| ApiError::Internal(e.into()))?;
    }

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "manager_session.set_mode".into(),
            resource: "manager_session".into(),
            resource_id: Some(id),
            project_id: None,
            detail: Some(serde_json::json!({"mode": body.mode})),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(Json(serde_json::json!({"ok": true, "mode": body.mode})))
}

// ---------------------------------------------------------------------------
// Manager session: MCP gate approval endpoints
// ---------------------------------------------------------------------------

use fred::interfaces::{KeysInterface, SetsInterface};

/// Approve a specific pending action (by hash). Single-use, 60s TTL.
async fn approve_manager_action(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let hash = body["action_hash"]
        .as_str()
        .ok_or_else(|| ApiError::BadRequest("action_hash required".into()))?;

    let key = format!("manager:{id}:approved:{hash}");
    state
        .valkey
        .next()
        .set::<(), _, _>(
            &key,
            "1",
            Some(fred::types::Expiration::EX(60)),
            None,
            false,
        )
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;

    tracing::info!(%id, %hash, user_id = %auth.user_id, "manager action approved");
    Ok(Json(serde_json::json!({"ok": true})))
}

/// Session-approve a tool name (CREATE/UPDATE only). Lasts for session lifetime.
async fn approve_manager_tool(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tool = body["tool_name"]
        .as_str()
        .ok_or_else(|| ApiError::BadRequest("tool_name required".into()))?;

    let key = format!("manager:{id}:approved_tools");
    state
        .valkey
        .next()
        .sadd::<(), _, _>(&key, tool)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;

    // Set TTL on the set (4h)
    state
        .valkey
        .next()
        .expire::<(), _>(&key, 4 * 3600, None)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;

    tracing::info!(%id, %tool, user_id = %auth.user_id, "manager tool session-approved");
    Ok(Json(serde_json::json!({"ok": true})))
}

/// Register a pending action (called by MCP gate).
/// Publishes a `confirmation_needed` event to SSE so the UI shows approve/reject buttons.
async fn register_pending_action(
    State(state): State<AppState>,
    _auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let hash = body["action_hash"]
        .as_str()
        .ok_or_else(|| ApiError::BadRequest("action_hash required".into()))?;
    let summary = body["summary"].as_str().unwrap_or("action pending");
    let tool = body["tool"].as_str().unwrap_or("");

    let key = format!("manager:{id}:pending:{hash}");
    state
        .valkey
        .next()
        .set::<(), _, _>(
            &key,
            summary,
            Some(fred::types::Expiration::EX(300)),
            None,
            false,
        )
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;

    // Publish confirmation_needed event to SSE
    let event = crate::agent::provider::ProgressEvent {
        kind: crate::agent::provider::ProgressKind::SecretRequest, // Reuse SecretRequest kind for confirmation UI
        message: summary.to_string(),
        metadata: Some(serde_json::json!({
            "type": "confirmation_needed",
            "action_hash": hash,
            "tool": tool,
            "summary": summary,
        })),
    };
    let _ = crate::agent::pubsub_bridge::publish_event(&state.valkey, id, &event).await;

    Ok(Json(serde_json::json!({"ok": true})))
}

/// Reject a pending action.
async fn reject_manager_action(
    State(state): State<AppState>,
    _auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let hash = body["action_hash"]
        .as_str()
        .ok_or_else(|| ApiError::BadRequest("action_hash required".into()))?;

    let key = format!("manager:{id}:rejected:{hash}");
    state
        .valkey
        .next()
        .set::<(), _, _>(
            &key,
            "1",
            Some(fred::types::Expiration::EX(60)),
            None,
            false,
        )
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;

    Ok(Json(serde_json::json!({"ok": true})))
}

/// Check if an action hash has been rejected.
async fn check_action_rejection(
    State(state): State<AppState>,
    _auth: AuthUser,
    Path((id, hash)): Path<(Uuid, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let key = format!("manager:{id}:rejected:{hash}");
    let exists: bool = state
        .valkey
        .next()
        .exists::<i64, _>(&key)
        .await
        .map(|n| n > 0)
        .unwrap_or(false);
    if exists {
        let _: () = state.valkey.next().del(&key).await.unwrap_or(());
    }
    Ok(Json(serde_json::json!({"rejected": exists})))
}

/// Check if an action hash has been approved.
async fn check_action_approval(
    State(state): State<AppState>,
    _auth: AuthUser,
    Path((id, hash)): Path<(Uuid, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let key = format!("manager:{id}:approved:{hash}");
    let exists: bool = state
        .valkey
        .next()
        .exists::<i64, _>(&key)
        .await
        .map(|n| n > 0)
        .unwrap_or(false);

    // If approved, consume it (single-use)
    if exists {
        let _: () = state.valkey.next().del(&key).await.unwrap_or(());
    }

    Ok(Json(serde_json::json!({"approved": exists})))
}

/// List session-approved tool names.
async fn list_approved_tools(
    State(state): State<AppState>,
    _auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let key = format!("manager:{id}:approved_tools");
    let tools: Vec<String> = state.valkey.next().smembers(&key).await.unwrap_or_default();

    Ok(Json(serde_json::json!({"tools": tools})))
}

/// Read the current permission mode for a manager session from Valkey.
async fn read_manager_mode(valkey: &fred::clients::Pool, session_id: Uuid) -> String {
    let mode_key = format!("manager:{session_id}:mode");
    valkey
        .next()
        .get::<Option<String>, _>(&mode_key)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "auto_read".into())
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
            session_namespace: None,
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
            session_namespace: None,
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
            session_namespace: None,
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
            session_namespace: None,
        };

        let response = session_to_response(&session, false);
        assert!(!response.browser_enabled);
    }

    #[test]
    fn valid_manager_modes_contains_expected() {
        assert!(VALID_MANAGER_MODES.contains(&"plan"));
        assert!(VALID_MANAGER_MODES.contains(&"guided"));
        assert!(VALID_MANAGER_MODES.contains(&"auto_read"));
        assert!(VALID_MANAGER_MODES.contains(&"auto_write"));
        assert!(VALID_MANAGER_MODES.contains(&"full_auto"));
    }

    #[test]
    fn invalid_mode_not_in_valid_list() {
        assert!(!VALID_MANAGER_MODES.contains(&"invalid"));
        assert!(!VALID_MANAGER_MODES.contains(&""));
    }
}
