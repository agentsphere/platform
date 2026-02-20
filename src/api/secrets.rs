use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use uuid::Uuid;

use crate::audit::{AuditEntry, write_audit};
use crate::auth::middleware::AuthUser;
use crate::error::ApiError;
use crate::rbac::{Permission, resolver};
use crate::secrets::engine;
use crate::store::AppState;
use crate::validation;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateSecretRequest {
    pub name: String,
    pub value: String,
    pub scope: String,
}

const VALID_SCOPES: &[&str] = &["pipeline", "agent", "deploy", "all"];

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

async fn require_secret_read(
    state: &AppState,
    auth: &AuthUser,
    project_id: Uuid,
) -> Result<(), ApiError> {
    let allowed = resolver::has_permission(
        &state.pool,
        &state.valkey,
        auth.user_id,
        Some(project_id),
        Permission::SecretRead,
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
    let allowed = resolver::has_permission(
        &state.pool,
        &state.valkey,
        auth.user_id,
        Some(project_id),
        Permission::SecretWrite,
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

    let meta = engine::create_secret(
        &state.pool,
        &master_key,
        Some(id),
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
            project_id: Some(id),
            detail: Some(serde_json::json!({"name": body.name, "scope": body.scope})),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok((StatusCode::CREATED, Json(meta)))
}

async fn list_project_secrets(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<engine::SecretMetadata>>, ApiError> {
    require_secret_read(&state, &auth, id).await?;

    let secrets = engine::list_secrets(&state.pool, Some(id))
        .await
        .map_err(ApiError::Internal)?;

    Ok(Json(secrets))
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
) -> Result<Json<Vec<engine::SecretMetadata>>, ApiError> {
    require_admin(&state, &auth).await?;

    let secrets = engine::list_secrets(&state.pool, None)
        .await
        .map_err(ApiError::Internal)?;

    Ok(Json(secrets))
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
