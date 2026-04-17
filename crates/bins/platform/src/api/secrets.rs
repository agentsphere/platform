// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

#[allow(dead_code, unused_imports)]
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use std::time::Instant;

use crate::secrets_request::{MAX_PENDING_PER_SESSION, SecretRequest, SecretRequestStatus};
use crate::state::PlatformState;
use platform_agent::provider::{ProgressEvent, ProgressKind};
use platform_agent::pubsub_bridge;
use platform_auth::resolver;
use platform_secrets::engine;
use platform_types::ApiError;
use platform_types::AuthUser;
use platform_types::Permission;
use platform_types::validation;
use platform_types::{AuditEntry, send_audit};

use super::helpers::{ListResponse, require_admin};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateSecretRequest {
    pub name: String,
    pub value: String,
    pub scope: String,
    pub environment: Option<String>,
}

const VALID_SCOPES: &[&str] = &["all", "pipeline", "agent", "test", "staging", "prod"];

#[derive(Debug, Deserialize)]
pub struct CreateSecretRequestBody {
    pub name: String,
    pub description: Option<String>,
    pub environments: Vec<String>,
    pub session_id: Uuid,
}

#[derive(Debug, Deserialize)]
struct CompleteSecretRequestBody {
    pub value: String,
}

#[derive(Debug, Deserialize)]
struct ListSecretRequestsParams {
    session_id: Option<Uuid>,
    status: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Debug, Serialize)]
struct SecretRequestResponse {
    id: Uuid,
    project_id: Uuid,
    session_id: Uuid,
    name: String,
    description: String,
    environments: Vec<String>,
    status: SecretRequestStatus,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn get_master_key(state: &PlatformState) -> Result<[u8; 32], ApiError> {
    let hex_str = state
        .config
        .secrets
        .master_key
        .as_deref()
        .ok_or_else(|| ApiError::ServiceUnavailable("secrets engine not configured".into()))?;
    engine::parse_master_key(hex_str).map_err(|e| {
        tracing::error!(error = %e, "invalid master key configuration");
        ApiError::ServiceUnavailable("secrets engine misconfigured".into())
    })
}

fn validate_scope(scope: &str) -> Result<(), ApiError> {
    if !VALID_SCOPES.contains(&scope) {
        return Err(ApiError::BadRequest(format!(
            "scope must be one of {VALID_SCOPES:?}"
        )));
    }
    Ok(())
}

const VALID_ENVIRONMENTS: &[&str] = &["preview", "staging", "production"];

fn validate_environment(env: &str) -> Result<(), ApiError> {
    if !VALID_ENVIRONMENTS.contains(&env) {
        return Err(ApiError::BadRequest(format!(
            "environment must be one of {VALID_ENVIRONMENTS:?}"
        )));
    }
    Ok(())
}

async fn require_secret_read(
    state: &PlatformState,
    auth: &AuthUser,
    project_id: Uuid,
) -> Result<(), ApiError> {
    auth.check_project_scope(project_id)?;
    let allowed = resolver::has_permission_scoped(
        &state.pool,
        &state.valkey,
        auth.user_id,
        Some(project_id),
        Permission::SecretRead,
        auth.token_scopes.as_deref(),
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::NotFound("secret".into()));
    }
    Ok(())
}

async fn require_secret_write(
    state: &PlatformState,
    auth: &AuthUser,
    project_id: Uuid,
) -> Result<(), ApiError> {
    auth.check_project_scope(project_id)?;
    let allowed = resolver::has_permission_scoped(
        &state.pool,
        &state.valkey,
        auth.user_id,
        Some(project_id),
        Permission::SecretWrite,
        auth.token_scopes.as_deref(),
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::NotFound("secret".into()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<PlatformState> {
    Router::new()
        .route(
            "/api/projects/{id}/secrets",
            get(list_project_secrets).post(create_project_secret),
        )
        .route(
            "/api/projects/{id}/secrets/{name}",
            get(read_project_secret).delete(delete_project_secret),
        )
        .route(
            "/api/projects/{id}/secret-requests",
            get(list_secret_requests).post(create_secret_request),
        )
        .route(
            "/api/projects/{id}/secret-requests/{request_id}",
            get(get_secret_request).post(complete_secret_request),
        )
        .route(
            "/api/workspaces/{id}/secrets",
            get(list_workspace_secrets).post(create_workspace_secret),
        )
        .route(
            "/api/workspaces/{id}/secrets/{name}",
            axum::routing::delete(delete_workspace_secret),
        )
        .route(
            "/api/admin/secrets",
            get(list_global_secrets).post(create_global_secret),
        )
        .route(
            "/api/admin/secrets/{name}",
            axum::routing::delete(delete_global_secret),
        )
}

// ---------------------------------------------------------------------------
// Project-scoped handlers
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state, body), fields(%id), err)]
async fn create_project_secret(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<CreateSecretRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_secret_write(&state, &auth, id).await?;

    // Verify project is active (soft-delete check)
    let active: bool = sqlx::query_scalar("SELECT is_active FROM projects WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?
        .unwrap_or(false);
    if !active {
        return Err(ApiError::NotFound("project".into()));
    }

    validation::check_name(&body.name)?;
    validation::check_length("value", &body.value, 1, 65_536)?;
    validate_scope(&body.scope)?;

    let master_key = get_master_key(&state)?;

    if let Some(ref env) = body.environment {
        validate_environment(env)?;
    }

    let meta = engine::create_secret(
        &state.pool,
        &master_key,
        engine::CreateSecretParams {
            project_id: Some(id),
            workspace_id: None,
            environment: body.environment.as_deref(),
            name: &body.name,
            value: body.value.as_bytes(),
            scope: &body.scope,
            created_by: auth.user_id,
        },
    )
    .await
    .map_err(ApiError::Internal)?;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "secret.create".into(),
            resource: "secret".into(),
            resource_id: Some(meta.id),
            project_id: Some(id),
            detail: Some(serde_json::json!({
                "name": body.name,
                "scope": body.scope,
                "environment": body.environment,
            })),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok((StatusCode::CREATED, Json(meta)))
}

#[derive(Debug, Deserialize)]
struct ListSecretsParams {
    environment: Option<String>,
}

async fn list_project_secrets(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Query(params): Query<ListSecretsParams>,
) -> Result<Json<ListResponse<engine::SecretMetadata>>, ApiError> {
    require_secret_read(&state, &auth, id).await?;

    let secrets = engine::list_secrets(&state.pool, Some(id), params.environment.as_deref())
        .await
        .map_err(ApiError::Internal)?;

    #[allow(clippy::cast_possible_wrap)]
    let total = secrets.len() as i64;
    Ok(Json(ListResponse {
        items: secrets,
        total,
    }))
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // environment reserved for future env-aware resolution
struct ReadSecretParams {
    scope: Option<String>,
    environment: Option<String>,
}

/// Read (decrypt) a single project secret by name.
/// Requires `secret:read`. Agents use this to retrieve secrets at runtime.
#[tracing::instrument(skip(state), fields(%id, %name), err)]
async fn read_project_secret(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((id, name)): Path<(Uuid, String)>,
    Query(params): Query<ReadSecretParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_secret_read(&state, &auth, id).await?;

    let master_key = get_master_key(&state)?;
    let requested_scope = params.scope.as_deref().unwrap_or("agent");

    let value = engine::resolve_secret(&state.pool, &master_key, id, &name, requested_scope)
        .await
        .map_err(|e| {
            tracing::debug!(error = %e, %id, %name, "secret resolution failed");
            ApiError::NotFound("secret".into())
        })?;

    // S43: Audit every secret read -- most sensitive data in the platform
    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "secret.read".into(),
            resource: "secret".into(),
            resource_id: None,
            project_id: Some(id),
            detail: Some(serde_json::json!({ "name": &name, "scope": requested_scope })),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(Json(serde_json::json!({
        "name": name,
        "value": value,
    })))
}

#[tracing::instrument(skip(state), fields(%id, %name), err)]
async fn delete_project_secret(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((id, name)): Path<(Uuid, String)>,
) -> Result<StatusCode, ApiError> {
    require_secret_write(&state, &auth, id).await?;

    let deleted = engine::delete_secret(&state.pool, Some(id), &name)
        .await
        .map_err(ApiError::Internal)?;

    if !deleted {
        return Err(ApiError::NotFound("secret".into()));
    }

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "secret.delete".into(),
            resource: "secret".into(),
            resource_id: None,
            project_id: Some(id),
            detail: Some(serde_json::json!({"name": name})),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Secret request handlers (agent -> UI -> secret flow)
// ---------------------------------------------------------------------------

const SECRET_REQUEST_VALID_ENVS: &[&str] = &["preview", "staging", "production"];

#[tracing::instrument(skip(state, body), fields(%id), err)]
async fn create_secret_request(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<CreateSecretRequestBody>,
) -> Result<impl IntoResponse, ApiError> {
    // Agents only have secret:read -- requesting a secret is not the same as
    // writing one.  The actual value is written by complete_secret_request
    // which correctly requires secret:write (called by the human user).
    require_secret_read(&state, &auth, id).await?;

    validation::check_name(&body.name)?;
    let desc = body.description.as_deref().unwrap_or("");
    validation::check_length("description", desc, 0, 500)?;

    if body.environments.len() > 5 {
        return Err(ApiError::BadRequest(
            "at most 5 environments allowed".into(),
        ));
    }
    for env in &body.environments {
        if !SECRET_REQUEST_VALID_ENVS.contains(&env.as_str()) {
            return Err(ApiError::BadRequest(format!(
                "environment must be one of {SECRET_REQUEST_VALID_ENVS:?}"
            )));
        }
    }

    // Enforce max 10 pending per session
    let pending_count = {
        let map = state
            .secret_requests
            .read()
            .map_err(|_| ApiError::Internal(anyhow::anyhow!("secret_requests lock poisoned")))?;
        map.values()
            .filter(|r| {
                r.session_id == body.session_id
                    && r.effective_status() == SecretRequestStatus::Pending
            })
            .count()
    };
    if pending_count >= MAX_PENDING_PER_SESSION {
        return Err(ApiError::TooManyRequests);
    }

    let req = SecretRequest {
        id: Uuid::new_v4(),
        project_id: id,
        session_id: body.session_id,
        name: body.name,
        description: desc.to_owned(),
        environments: body.environments,
        status: SecretRequestStatus::Pending,
        created_at: Instant::now(),
    };

    let response = SecretRequestResponse {
        id: req.id,
        project_id: req.project_id,
        session_id: req.session_id,
        name: req.name.clone(),
        description: req.description.clone(),
        environments: req.environments.clone(),
        status: req.status,
    };

    let req_name = req.name.clone();
    let req_description = req.description.clone();
    let req_environments = req.environments.clone();
    let req_session_id = req.session_id;
    let req_id = req.id;
    {
        let mut map = state
            .secret_requests
            .write()
            .map_err(|_| ApiError::Internal(anyhow::anyhow!("secret_requests lock poisoned")))?;
        map.insert(req.id, req);
    }

    // Publish SSE event so the UI shows the secret-request modal
    let event = ProgressEvent {
        kind: ProgressKind::SecretRequest,
        message: format!("Secret requested: {req_name}"),
        metadata: Some(serde_json::json!({
            "request_id": req_id,
            "name": req_name,
            "prompt": req_description,
            "environments": req_environments,
        })),
    };
    if let Err(e) = pubsub_bridge::publish_event(&state.valkey, req_session_id, &event).await {
        tracing::warn!(error = %e, %req_session_id, "failed to publish SecretRequest SSE event");
    }

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "secret_request.create".into(),
            resource: "secret_request".into(),
            resource_id: Some(response.id),
            project_id: Some(id),
            detail: Some(serde_json::json!({
                "name": req_name,
                "session_id": req_session_id,
            })),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok((StatusCode::CREATED, Json(response)))
}

#[tracing::instrument(skip(state), fields(%id), err)]
async fn list_secret_requests(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Query(params): Query<ListSecretRequestsParams>,
) -> Result<Json<ListResponse<SecretRequestResponse>>, ApiError> {
    require_secret_read(&state, &auth, id).await?;

    let map = state
        .secret_requests
        .read()
        .map_err(|_| ApiError::Internal(anyhow::anyhow!("secret_requests lock poisoned")))?;

    let limit = params.limit.unwrap_or(50).clamp(1, 100);
    let offset = params.offset.unwrap_or(0).max(0);

    let mut items: Vec<SecretRequestResponse> = map
        .values()
        .filter(|r| r.project_id == id)
        .filter(|r| {
            if let Some(ref sid) = params.session_id {
                r.session_id == *sid
            } else {
                true
            }
        })
        .filter(|r| {
            if let Some(ref status_filter) = params.status {
                let effective = r.effective_status();
                let status_str = match effective {
                    SecretRequestStatus::Pending => "pending",
                    SecretRequestStatus::Completed => "completed",
                    SecretRequestStatus::TimedOut => "timed_out",
                };
                status_str == status_filter.as_str()
            } else {
                true
            }
        })
        .map(|r| SecretRequestResponse {
            id: r.id,
            project_id: r.project_id,
            session_id: r.session_id,
            name: r.name.clone(),
            description: r.description.clone(),
            environments: r.environments.clone(),
            status: r.effective_status(),
        })
        .collect();

    // Sort by name for stable ordering
    items.sort_by(|a, b| a.name.cmp(&b.name));

    #[allow(clippy::cast_possible_wrap)]
    let total = items.len() as i64;
    let offset_usize = usize::try_from(offset).unwrap_or(0);
    let limit_usize = usize::try_from(limit).unwrap_or(50);
    let paged: Vec<SecretRequestResponse> = items
        .into_iter()
        .skip(offset_usize)
        .take(limit_usize)
        .collect();

    Ok(Json(ListResponse {
        items: paged,
        total,
    }))
}

#[tracing::instrument(skip(state), fields(%id, %request_id), err)]
async fn get_secret_request(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<SecretRequestResponse>, ApiError> {
    require_secret_read(&state, &auth, id).await?;

    let map = state
        .secret_requests
        .read()
        .map_err(|_| ApiError::Internal(anyhow::anyhow!("secret_requests lock poisoned")))?;

    let req = map
        .get(&request_id)
        .filter(|r| r.project_id == id)
        .ok_or_else(|| ApiError::NotFound("secret request".into()))?;

    Ok(Json(SecretRequestResponse {
        id: req.id,
        project_id: req.project_id,
        session_id: req.session_id,
        name: req.name.clone(),
        description: req.description.clone(),
        environments: req.environments.clone(),
        status: req.effective_status(),
    }))
}

#[tracing::instrument(skip(state, body), fields(%id, %request_id), err)]
async fn complete_secret_request(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<CompleteSecretRequestBody>,
) -> Result<Json<SecretRequestResponse>, ApiError> {
    require_secret_write(&state, &auth, id).await?;

    validation::check_length("value", &body.value, 1, 65_536)?;

    // Extract request metadata from in-memory map, validate state
    let (req_name, req_description, req_environments, req_session_id) = {
        let mut map = state
            .secret_requests
            .write()
            .map_err(|_| ApiError::Internal(anyhow::anyhow!("secret_requests lock poisoned")))?;

        let req = map
            .get_mut(&request_id)
            .filter(|r| r.project_id == id)
            .ok_or_else(|| ApiError::NotFound("secret request".into()))?;

        if req.is_timed_out() {
            req.status = SecretRequestStatus::TimedOut;
            return Err(ApiError::BadRequest("secret request has timed out".into()));
        }

        if req.status != SecretRequestStatus::Pending {
            return Err(ApiError::BadRequest(format!(
                "secret request is already {:?}",
                req.status
            )));
        }

        req.status = SecretRequestStatus::Completed;
        (
            req.name.clone(),
            req.description.clone(),
            req.environments.clone(),
            req.session_id,
        )
    };

    // Store the secret in the database for each requested environment
    let master_key = get_master_key(&state)?;
    if req_environments.is_empty() {
        // No specific environment -- store with NULL environment, scope=agent
        engine::create_secret(
            &state.pool,
            &master_key,
            engine::CreateSecretParams {
                project_id: Some(id),
                workspace_id: None,
                environment: None,
                name: &req_name,
                value: body.value.as_bytes(),
                scope: "agent",
                created_by: auth.user_id,
            },
        )
        .await
        .map_err(ApiError::Internal)?;
    } else {
        for env in &req_environments {
            engine::create_secret(
                &state.pool,
                &master_key,
                engine::CreateSecretParams {
                    project_id: Some(id),
                    workspace_id: None,
                    environment: Some(env),
                    name: &req_name,
                    value: body.value.as_bytes(),
                    scope: "agent",
                    created_by: auth.user_id,
                },
            )
            .await
            .map_err(ApiError::Internal)?;
        }
    }

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "secret_request.complete".into(),
            resource: "secret_request".into(),
            resource_id: Some(request_id),
            project_id: Some(id),
            detail: Some(serde_json::json!({ "name": req_name })),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(Json(SecretRequestResponse {
        id: request_id,
        project_id: id,
        session_id: req_session_id,
        name: req_name.clone(),
        description: req_description,
        environments: req_environments,
        status: SecretRequestStatus::Completed,
    }))
}

// ---------------------------------------------------------------------------
// Global (admin) handlers
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state, body), err)]
async fn create_global_secret(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Json(body): Json<CreateSecretRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &auth).await?;

    validation::check_name(&body.name)?;
    validation::check_length("value", &body.value, 1, 65_536)?;
    validate_scope(&body.scope)?;

    let master_key = get_master_key(&state)?;

    let meta = engine::create_global_secret(
        &state.pool,
        &master_key,
        &body.name,
        body.value.as_bytes(),
        &body.scope,
        auth.user_id,
    )
    .await
    .map_err(ApiError::Internal)?;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "secret.create".into(),
            resource: "secret".into(),
            resource_id: Some(meta.id),
            project_id: None,
            detail: Some(
                serde_json::json!({"name": body.name, "scope": body.scope, "global": true}),
            ),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok((StatusCode::CREATED, Json(meta)))
}

async fn list_global_secrets(
    State(state): State<PlatformState>,
    auth: AuthUser,
) -> Result<Json<ListResponse<engine::SecretMetadata>>, ApiError> {
    require_admin(&state, &auth).await?;

    let secrets = engine::list_secrets(&state.pool, None, None)
        .await
        .map_err(ApiError::Internal)?;

    #[allow(clippy::cast_possible_wrap)]
    let total = secrets.len() as i64;
    Ok(Json(ListResponse {
        items: secrets,
        total,
    }))
}

#[tracing::instrument(skip(state), fields(%name), err)]
async fn delete_global_secret(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_admin(&state, &auth).await?;

    let deleted = engine::delete_secret(&state.pool, None, &name)
        .await
        .map_err(ApiError::Internal)?;

    if !deleted {
        return Err(ApiError::NotFound("secret".into()));
    }

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "secret.delete".into(),
            resource: "secret".into(),
            resource_id: None,
            project_id: None,
            detail: Some(serde_json::json!({"name": name, "global": true})),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Workspace-scoped secret handlers
// ---------------------------------------------------------------------------

async fn require_workspace_admin(
    state: &PlatformState,
    auth: &AuthUser,
    workspace_id: Uuid,
) -> Result<(), ApiError> {
    let is_admin = sqlx::query_scalar!(
        r#"SELECT EXISTS(
            SELECT 1 FROM workspace_members
            WHERE workspace_id = $1 AND user_id = $2 AND role IN ('owner', 'admin')
        ) as "exists!""#,
        workspace_id,
        auth.user_id,
    )
    .fetch_one(&state.pool)
    .await?;
    if !is_admin {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

async fn require_workspace_member_for_secrets(
    state: &PlatformState,
    auth: &AuthUser,
    workspace_id: Uuid,
) -> Result<(), ApiError> {
    let is_member = sqlx::query_scalar!(
        r#"SELECT EXISTS(
            SELECT 1 FROM workspace_members
            WHERE workspace_id = $1 AND user_id = $2
        ) as "exists!""#,
        workspace_id,
        auth.user_id,
    )
    .fetch_one(&state.pool)
    .await?;
    if !is_member {
        return Err(ApiError::NotFound("workspace".into()));
    }
    Ok(())
}

#[tracing::instrument(skip(state, body), fields(%id), err)]
async fn create_workspace_secret(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<CreateSecretRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_workspace_admin(&state, &auth, id).await?;

    validation::check_name(&body.name)?;
    validation::check_length("value", &body.value, 1, 65_536)?;
    validate_scope(&body.scope)?;

    let master_key = get_master_key(&state)?;

    let meta = engine::create_secret(
        &state.pool,
        &master_key,
        engine::CreateSecretParams {
            project_id: None,
            workspace_id: Some(id),
            environment: None,
            name: &body.name,
            value: body.value.as_bytes(),
            scope: &body.scope,
            created_by: auth.user_id,
        },
    )
    .await
    .map_err(ApiError::Internal)?;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "secret.create".into(),
            resource: "secret".into(),
            resource_id: Some(meta.id),
            project_id: None,
            detail: Some(serde_json::json!({
                "name": body.name,
                "scope": body.scope,
                "workspace_id": id,
            })),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok((StatusCode::CREATED, Json(meta)))
}

async fn list_workspace_secrets(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<ListResponse<engine::SecretMetadata>>, ApiError> {
    require_workspace_member_for_secrets(&state, &auth, id).await?;

    let secrets = engine::list_workspace_secrets(&state.pool, id)
        .await
        .map_err(ApiError::Internal)?;

    #[allow(clippy::cast_possible_wrap)]
    let total = secrets.len() as i64;
    Ok(Json(ListResponse {
        items: secrets,
        total,
    }))
}

#[tracing::instrument(skip(state), fields(%id, %name), err)]
async fn delete_workspace_secret(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((id, name)): Path<(Uuid, String)>,
) -> Result<StatusCode, ApiError> {
    require_workspace_admin(&state, &auth, id).await?;

    // Delete workspace-scoped secret specifically
    let result = sqlx::query!(
        "DELETE FROM secrets WHERE workspace_id = $1 AND project_id IS NULL AND name = $2",
        id,
        name,
    )
    .execute(&state.pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::NotFound("secret".into()));
    }

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "secret.delete".into(),
            resource: "secret".into(),
            resource_id: None,
            project_id: None,
            detail: Some(serde_json::json!({"name": name, "workspace_id": id})),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(StatusCode::NO_CONTENT)
}
