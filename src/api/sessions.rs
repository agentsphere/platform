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

#[derive(Debug, Serialize)]
pub struct SessionResponse {
    pub id: Uuid,
    pub project_id: Uuid,
    pub user_id: Uuid,
    pub prompt: String,
    pub status: String,
    pub branch: Option<String>,
    pub pod_name: Option<String>,
    pub provider: String,
    pub cost_tokens: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
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
            "/api/projects/{id}/sessions/{session_id}/ws",
            get(ws_handler),
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

    Ok((StatusCode::CREATED, Json(session_to_response(&session))))
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
        SELECT id, project_id, user_id, prompt, status, branch, pod_name,
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
            prompt: r.prompt.chars().take(200).collect(), // Truncate in list view
            status: r.status,
            branch: r.branch,
            pod_name: r.pod_name,
            provider: r.provider,
            cost_tokens: r.cost_tokens,
            created_at: r.created_at,
            finished_at: r.finished_at,
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

    if session.project_id != id {
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
        session: session_to_response(&session),
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

    if session.project_id != id {
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

    if session.project_id != id {
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

    if session.project_id != id {
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
// Helpers
// ---------------------------------------------------------------------------

fn session_to_response(session: &crate::agent::provider::AgentSession) -> SessionResponse {
    SessionResponse {
        id: session.id,
        project_id: session.project_id,
        user_id: session.user_id,
        prompt: session.prompt.clone(),
        status: session.status.clone(),
        branch: session.branch.clone(),
        pod_name: session.pod_name.clone(),
        provider: session.provider.clone(),
        cost_tokens: session.cost_tokens,
        created_at: session.created_at,
        finished_at: session.finished_at,
    }
}
