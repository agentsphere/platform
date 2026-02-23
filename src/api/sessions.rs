use axum::extract::ws;
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

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
    /// Delegate deploy:read + deploy:promote to the agent.
    #[serde(default)]
    pub delegate_deploy: bool,
    /// Delegate observe:read to the agent.
    #[serde(default)]
    pub delegate_observe: bool,
    /// Delegate admin:users + admin:roles + admin:config to the agent.
    #[serde(default)]
    pub delegate_admin: bool,
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

#[derive(Debug, Serialize)]
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
    pub cost_tokens: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub browser_enabled: bool,
}

#[derive(Debug, Serialize)]
pub struct SessionDetailResponse {
    #[serde(flatten)]
    pub session: SessionResponse,
    pub messages: Vec<MessageResponse>,
}

#[derive(Debug, Serialize)]
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
            "/api/projects/{id}/sessions/{session_id}/ws",
            get(ws_handler),
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
        .route("/api/sessions/{session_id}/ws", get(ws_handler_global))
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
    let allowed = resolver::has_permission(
        &state.pool,
        &state.valkey,
        auth.user_id,
        Some(project_id),
        Permission::AgentRun,
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
    if auth.user_id == session_user_id {
        return Ok(());
    }
    let allowed = resolver::has_permission(
        &state.pool,
        &state.valkey,
        auth.user_id,
        Some(project_id),
        Permission::ProjectWrite,
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

/// Validate provider config fields (image, `setup_commands`, browser).
fn validate_provider_config(
    config: &serde_json::Value,
    delegate_admin: bool,
    user_id: Uuid,
) -> Result<(), ApiError> {
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
    if let Some(ref browser) = parsed.browser {
        validation::check_browser_config(browser)?;
        let role = parsed.role.as_deref().unwrap_or("dev");
        if !matches!(role, "ui" | "test") {
            return Err(ApiError::BadRequest(
                "browser access is only available for 'ui' and 'test' roles".into(),
            ));
        }
    }
    if parsed.role.as_deref() == Some("admin") && !delegate_admin {
        tracing::warn!(
            %user_id,
            "agent session requested admin role without delegate_admin flag — admin MCP tools will get 403"
        );
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

    // Validate provider config if provided
    if let Some(ref config) = body.config {
        validate_provider_config(config, body.delegate_admin, auth.user_id)?;
    }

    // Verify project exists
    sqlx::query!(
        "SELECT id FROM projects WHERE id = $1 AND is_active = true",
        id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("project".into()))?;

    // Build extra permission delegations based on request flags
    let mut extra_permissions = Vec::new();
    if body.delegate_deploy {
        extra_permissions.push(Permission::DeployRead);
        extra_permissions.push(Permission::DeployPromote);
    }
    if body.delegate_observe {
        extra_permissions.push(Permission::ObserveRead);
    }
    if body.delegate_admin {
        extra_permissions.push(Permission::AdminUsers);
        extra_permissions.push(Permission::AdminRoles);
        extra_permissions.push(Permission::AdminConfig);
    }

    // Create session (identity + pod)
    let session = service::create_session(
        &state,
        auth.user_id,
        id,
        &body.prompt,
        provider,
        body.branch.as_deref(),
        body.config,
        &extra_permissions,
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
               provider, cost_tokens, created_at, finished_at
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
    let has_write = resolver::has_permission(
        &state.pool,
        &state.valkey,
        auth.user_id,
        None,
        Permission::ProjectWrite,
    )
    .await
    .map_err(ApiError::Internal)?;
    let has_run = resolver::has_permission(
        &state.pool,
        &state.valkey,
        auth.user_id,
        None,
        Permission::AgentRun,
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
        // Verify project exists and user has access
        let exists: Option<(Uuid,)> =
            sqlx::query_as("SELECT id FROM projects WHERE id = $1 AND is_active = true")
                .bind(project_id)
                .fetch_optional(&state.pool)
                .await?;
        if exists.is_none() {
            return Err(ApiError::NotFound("project".into()));
        }

        sqlx::query("UPDATE agent_sessions SET project_id = $2 WHERE id = $1")
            .bind(session_id)
            .bind(project_id)
            .execute(&state.pool)
            .await?;
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

    // Check agent:spawn permission
    let allowed = resolver::has_permission(
        &state.pool,
        &state.valkey,
        auth.user_id,
        Some(id),
        Permission::AgentSpawn,
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
               provider, cost_tokens, created_at, finished_at, spawn_depth
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
        })
        .collect();

    Ok(Json(items))
}

fn truncate_prompt(prompt: &str, max_len: usize) -> String {
    if prompt.len() <= max_len {
        prompt.to_owned()
    } else {
        format!("{}...", &prompt[..max_len])
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
    }
}

// ---------------------------------------------------------------------------
// WebSocket
// ---------------------------------------------------------------------------

/// WebSocket handler: streams agent output in real-time.
/// Auth is validated during the HTTP upgrade via the `AuthUser` extractor.
async fn ws_handler(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, session_id)): Path<(Uuid, Uuid)>,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, ApiError> {
    require_project_read(&state, &auth, id).await?;

    let session = service::fetch_session(&state.pool, session_id)
        .await
        .map_err(ApiError::from)?;

    if session.project_id != Some(id) {
        return Err(ApiError::NotFound("session".into()));
    }

    let provider_name = session.provider.clone();

    Ok(ws.on_upgrade(move |socket| handle_ws(state, session_id, provider_name, socket)))
}

async fn handle_ws(
    state: AppState,
    session_id: Uuid,
    provider_name: String,
    mut socket: ws::WebSocket,
) {
    let Ok(provider) = service::get_provider(&provider_name) else {
        return;
    };

    // Get log lines from the pod
    let mut lines = match service::get_log_lines(&state, session_id).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, %session_id, "failed to get log stream for ws");
            let _ = socket
                .send(ws::Message::Text(
                    serde_json::json!({"error": "failed to connect to agent"})
                        .to_string()
                        .into(),
                ))
                .await;
            return;
        }
    };

    loop {
        tokio::select! {
            // Read from pod log stream and send to WebSocket client
            line = lines.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        if let Some(event) = provider.parse_progress(&line) {
                            // Store as assistant message
                            let _ = sqlx::query!(
                                r#"INSERT INTO agent_messages (session_id, role, content, metadata)
                                   VALUES ($1, 'assistant', $2, $3)"#,
                                session_id,
                                &event.message,
                                event.metadata,
                            )
                            .execute(&state.pool)
                            .await;

                            // Send to WebSocket
                            let json = serde_json::to_string(&event).unwrap_or_default();
                            if socket.send(ws::Message::Text(json.into())).await.is_err() {
                                break; // Client disconnected
                            }
                        }
                    }
                    Ok(None) => break, // Stream ended (pod exited)
                    Err(e) => {
                        tracing::warn!(error = %e, %session_id, "log stream error");
                        break;
                    }
                }
            }
            // Read from WebSocket client (user sending messages)
            msg = socket.recv() => {
                match msg {
                    Some(Ok(ws::Message::Text(text))) => {
                        if let Ok(cmd) = serde_json::from_str::<SendMessageRequest>(&text) {
                            let _ = service::send_message(&state, session_id, &cmd.content).await;
                        }
                    }
                    Some(Ok(ws::Message::Close(_))) | None => break,
                    _ => {} // Ignore pings, binary, etc.
                }
            }
        }
    }

    tracing::info!(%session_id, "websocket connection closed");
}

// ---------------------------------------------------------------------------
// Global (project-less) message + WebSocket handlers
// ---------------------------------------------------------------------------

/// Send a message to a global (project-less) in-process session.
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

/// WebSocket handler for global (project-less) in-process sessions.
async fn ws_handler_global(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(session_id): Path<Uuid>,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, ApiError> {
    let session = service::fetch_session(&state.pool, session_id)
        .await
        .map_err(ApiError::from)?;

    // Only the session owner can connect
    if session.user_id != auth.user_id {
        return Err(ApiError::Forbidden);
    }

    Ok(ws.on_upgrade(move |socket| handle_ws_global(state, session_id, socket)))
}

async fn handle_ws_global(state: AppState, session_id: Uuid, mut socket: ws::WebSocket) {
    // Subscribe to the in-process session's broadcast channel
    let Some(mut rx) = crate::agent::inprocess::subscribe(&state, session_id) else {
        // Session not found in in-process map — may have already completed.
        let _ = socket
            .send(ws::Message::Text(
                serde_json::json!({"kind":"error","message":"session not active"})
                    .to_string()
                    .into(),
            ))
            .await;
        return;
    };

    loop {
        tokio::select! {
            // Receive progress events from the in-process agent
            event = rx.recv() => {
                match event {
                    Ok(event) => {
                        let json = serde_json::to_string(&event).unwrap_or_default();
                        if socket.send(ws::Message::Text(json.into())).await.is_err() {
                            break; // Client disconnected
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(%session_id, lagged = n, "ws subscriber lagged");
                        // Continue receiving — we'll just miss some events
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        // Session ended — send completion and close
                        break;
                    }
                }
            }
            // Receive messages from the WebSocket client
            msg = socket.recv() => {
                match msg {
                    Some(Ok(ws::Message::Text(text))) => {
                        if let Ok(cmd) = serde_json::from_str::<SendMessageRequest>(&text) {
                            let _ = service::send_message(&state, session_id, &cmd.content).await;
                        }
                    }
                    Some(Ok(ws::Message::Close(_))) | None => break,
                    _ => {} // Ignore pings, binary, etc.
                }
            }
        }
    }

    tracing::info!(%session_id, "global websocket connection closed");
}
