use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::audit::{AuditEntry, write_audit};
use crate::auth::cli_creds;
use crate::auth::middleware::AuthUser;
use crate::error::ApiError;
use crate::secrets::engine;
use crate::store::AppState;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct StoreCredentialsRequest {
    pub auth_type: String,
    pub token: String,
    #[serde(default)]
    pub token_expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct CredentialStatusResponse {
    pub exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_expires_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
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

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `POST /api/auth/cli-credentials` — Store encrypted CLI credentials.
#[tracing::instrument(skip(state, body), fields(user_id = %auth.user_id), err)]
async fn store_credentials(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<StoreCredentialsRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // Rate limit
    crate::auth::rate_limit::check_rate(
        &state.valkey,
        "cli-creds",
        &auth.user_id.to_string(),
        10,
        300,
    )
    .await?;

    // Validate auth_type
    cli_creds::validate_auth_type(&body.auth_type).map_err(ApiError::BadRequest)?;

    // Validate token is non-empty, non-whitespace, and within size limit
    if body.token.trim().is_empty() {
        return Err(ApiError::BadRequest("token must not be empty".into()));
    }
    crate::validation::check_length("token", &body.token, 1, 10_000)?;

    let master_key = get_master_key(&state)?;

    let info = cli_creds::store_credentials(
        &state.pool,
        &master_key,
        auth.user_id,
        &body.auth_type,
        &body.token,
        body.token_expires_at,
    )
    .await
    .map_err(ApiError::Internal)?;

    // Audit — never log the credential value
    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "cli_creds.store",
            resource: "cli_credentials",
            resource_id: Some(info.id),
            project_id: None,
            detail: Some(serde_json::json!({ "auth_type": body.auth_type })),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok((
        StatusCode::OK,
        Json(CredentialStatusResponse {
            exists: true,
            auth_type: Some(info.auth_type),
            token_expires_at: info.token_expires_at,
            created_at: Some(info.created_at),
            updated_at: Some(info.updated_at),
        }),
    ))
}

/// `GET /api/auth/cli-credentials` — Check if credentials exist (no secrets returned).
#[tracing::instrument(skip(state), fields(user_id = %auth.user_id), err)]
async fn get_credentials(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<CredentialStatusResponse>, ApiError> {
    let info = cli_creds::get_credential_info(&state.pool, auth.user_id)
        .await
        .map_err(ApiError::Internal)?;

    match info {
        Some(info) => Ok(Json(CredentialStatusResponse {
            exists: true,
            auth_type: Some(info.auth_type),
            token_expires_at: info.token_expires_at,
            created_at: Some(info.created_at),
            updated_at: Some(info.updated_at),
        })),
        None => Ok(Json(CredentialStatusResponse {
            exists: false,
            auth_type: None,
            token_expires_at: None,
            created_at: None,
            updated_at: None,
        })),
    }
}

/// `DELETE /api/auth/cli-credentials` — Remove stored credentials.
#[tracing::instrument(skip(state), fields(user_id = %auth.user_id), err)]
async fn delete_credentials(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<impl IntoResponse, ApiError> {
    let deleted = cli_creds::delete_credentials(&state.pool, auth.user_id)
        .await
        .map_err(ApiError::Internal)?;

    if deleted {
        write_audit(
            &state.pool,
            &AuditEntry {
                actor_id: auth.user_id,
                actor_name: &auth.user_name,
                action: "cli_creds.delete",
                resource: "cli_credentials",
                resource_id: None,
                project_id: None,
                detail: None,
                ip_addr: auth.ip_addr.as_deref(),
            },
        )
        .await;
    }

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/api/auth/cli-credentials",
        post(store_credentials)
            .get(get_credentials)
            .delete(delete_credentials),
    )
}
