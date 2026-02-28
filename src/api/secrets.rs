use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use std::time::Instant;

use crate::audit::{AuditEntry, write_audit};
use crate::auth::middleware::AuthUser;
use crate::error::ApiError;
use crate::rbac::{Permission, resolver};
use crate::secrets::engine;
use crate::secrets::request::{MAX_PENDING_PER_SESSION, SecretRequest, SecretRequestStatus};
use crate::store::AppState;
use crate::validation;

use super::helpers::ListResponse;

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

const VALID_SCOPES: &[&str] = &["pipeline", "agent", "deploy", "all"];

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

#[derive(Debug, Serialize)]
struct SecretRequestResponse {
    id: Uuid,
    status: SecretRequestStatus,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn get_master_key(state: &AppState) -> Result<[u8; 32], ApiError> {
    let hex_str = state
        .config
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
        Permission::SecretRead,
        auth.token_scopes.as_deref(),
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

async fn require_secret_write(
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
        Permission::SecretWrite,
        auth.token_scopes.as_deref(),
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

async fn require_admin(state: &AppState, auth: &AuthUser) -> Result<(), ApiError> {
    let allowed = resolver::has_permission(
        &state.pool,
        &state.valkey,
        auth.user_id,
        None,
        Permission::AdminUsers,
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/projects/{id}/secrets",
            get(list_project_secrets).post(create_project_secret),
        )
        .route(
            "/api/projects/{id}/secrets/{name}",
            axum::routing::delete(delete_project_secret),
        )
        .route(
            "/api/projects/{id}/secret-requests",
            axum::routing::post(create_secret_request),
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
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<CreateSecretRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_secret_write(&state, &auth, id).await?;

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

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "secret.create",
            resource: "secret",
            resource_id: Some(meta.id),
            project_id: Some(id),
            detail: Some(serde_json::json!({
                "name": body.name,
                "scope": body.scope,
                "environment": body.environment,
            })),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok((StatusCode::CREATED, Json(meta)))
}

#[derive(Debug, Deserialize)]
struct ListSecretsParams {
    environment: Option<String>,
}

async fn list_project_secrets(
    State(state): State<AppState>,
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

#[tracing::instrument(skip(state), fields(%id, %name), err)]
async fn delete_project_secret(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, name)): Path<(Uuid, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_secret_write(&state, &auth, id).await?;

    let deleted = engine::delete_secret(&state.pool, Some(id), &name)
        .await
        .map_err(ApiError::Internal)?;

    if !deleted {
        return Err(ApiError::NotFound("secret".into()));
    }

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "secret.delete",
            resource: "secret",
            resource_id: None,
            project_id: Some(id),
            detail: Some(serde_json::json!({"name": name})),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(Json(serde_json::json!({"ok": true})))
}

// ---------------------------------------------------------------------------
// Secret request handlers (agent → UI → secret flow)
// ---------------------------------------------------------------------------

const SECRET_REQUEST_VALID_ENVS: &[&str] = &["preview", "staging", "production"];

#[tracing::instrument(skip(state, body), fields(%id), err)]
async fn create_secret_request(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<CreateSecretRequestBody>,
) -> Result<impl IntoResponse, ApiError> {
    require_secret_write(&state, &auth, id).await?;

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
        status: req.status,
    };

    let req_name = req.name.clone();
    let req_session_id = req.session_id;
    {
        let mut map = state
            .secret_requests
            .write()
            .map_err(|_| ApiError::Internal(anyhow::anyhow!("secret_requests lock poisoned")))?;
        map.insert(req.id, req);
    }

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "secret_request.create",
            resource: "secret_request",
            resource_id: Some(response.id),
            project_id: Some(id),
            detail: Some(serde_json::json!({
                "name": req_name,
                "session_id": req_session_id,
            })),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok((StatusCode::CREATED, Json(response)))
}

#[tracing::instrument(skip(state), fields(%id, %request_id), err)]
async fn get_secret_request(
    State(state): State<AppState>,
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
        status: req.effective_status(),
    }))
}

#[tracing::instrument(skip(state, body), fields(%id, %request_id), err)]
async fn complete_secret_request(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<CompleteSecretRequestBody>,
) -> Result<Json<SecretRequestResponse>, ApiError> {
    require_secret_write(&state, &auth, id).await?;

    validation::check_length("value", &body.value, 1, 65_536)?;

    // Extract request metadata from in-memory map, validate state
    let (req_name, req_environments) = {
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
        (req.name.clone(), req.environments.clone())
    };

    // Store the secret in the database for each requested environment
    let master_key = get_master_key(&state)?;
    if req_environments.is_empty() {
        // No specific environment — store with NULL environment, scope=agent
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

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "secret_request.complete",
            resource: "secret_request",
            resource_id: Some(request_id),
            project_id: Some(id),
            detail: Some(serde_json::json!({ "name": req_name })),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(Json(SecretRequestResponse {
        id: request_id,
        status: SecretRequestStatus::Completed,
    }))
}

// ---------------------------------------------------------------------------
// Global (admin) handlers
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state, body), err)]
async fn create_global_secret(
    State(state): State<AppState>,
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

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "secret.create",
            resource: "secret",
            resource_id: Some(meta.id),
            project_id: None,
            detail: Some(
                serde_json::json!({"name": body.name, "scope": body.scope, "global": true}),
            ),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok((StatusCode::CREATED, Json(meta)))
}

async fn list_global_secrets(
    State(state): State<AppState>,
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
    State(state): State<AppState>,
    auth: AuthUser,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_admin(&state, &auth).await?;

    let deleted = engine::delete_secret(&state.pool, None, &name)
        .await
        .map_err(ApiError::Internal)?;

    if !deleted {
        return Err(ApiError::NotFound("secret".into()));
    }

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "secret.delete",
            resource: "secret",
            resource_id: None,
            project_id: None,
            detail: Some(serde_json::json!({"name": name, "global": true})),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(Json(serde_json::json!({"ok": true})))
}

// ---------------------------------------------------------------------------
// Workspace-scoped secret handlers
// ---------------------------------------------------------------------------

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

async fn require_workspace_member_for_secrets(
    state: &AppState,
    auth: &AuthUser,
    workspace_id: Uuid,
) -> Result<(), ApiError> {
    if !crate::workspace::service::is_member(&state.pool, workspace_id, auth.user_id).await? {
        return Err(ApiError::NotFound("workspace".into()));
    }
    Ok(())
}

#[tracing::instrument(skip(state, body), fields(%id), err)]
async fn create_workspace_secret(
    State(state): State<AppState>,
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

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "secret.create",
            resource: "secret",
            resource_id: Some(meta.id),
            project_id: None,
            detail: Some(serde_json::json!({
                "name": body.name,
                "scope": body.scope,
                "workspace_id": id,
            })),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok((StatusCode::CREATED, Json(meta)))
}

async fn list_workspace_secrets(
    State(state): State<AppState>,
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
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, name)): Path<(Uuid, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
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

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "secret.delete",
            resource: "secret",
            resource_id: None,
            project_id: None,
            detail: Some(serde_json::json!({"name": name, "workspace_id": id})),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(Json(serde_json::json!({"ok": true})))
}
