use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::audit::{AuditEntry, write_audit};
use crate::auth::middleware::AuthUser;
use crate::error::ApiError;
use crate::secrets::{engine, user_keys};
use crate::store::AppState;
use crate::validation;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SetProviderKeyRequest {
    pub api_key: String,
}

#[derive(Debug, Deserialize)]
pub struct ValidateKeyRequest {
    pub api_key: String,
}

#[derive(Debug, Serialize)]
pub struct ValidateKeyResponse {
    pub valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/users/me/provider-keys", get(list_provider_keys))
        .route(
            "/api/users/me/provider-keys/validate",
            axum::routing::post(validate_provider_key),
        )
        .route(
            "/api/users/me/provider-keys/{provider}",
            axum::routing::put(set_provider_key).delete(delete_provider_key),
        )
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// PUT /api/users/me/provider-keys/{provider}
async fn set_provider_key(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(provider): Path<String>,
    Json(body): Json<SetProviderKeyRequest>,
) -> Result<impl IntoResponse, ApiError> {
    validation::check_name(&provider)?;
    validation::check_length("api_key", &body.api_key, 10, 500)?;

    let master_key = get_master_key(&state)?;

    user_keys::set_user_key(
        &state.pool,
        &master_key,
        auth.user_id,
        &provider,
        &body.api_key,
    )
    .await
    .map_err(ApiError::Internal)?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "provider_key.set",
            resource: "provider_key",
            resource_id: None,
            project_id: None,
            detail: Some(serde_json::json!({ "provider": provider })),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

/// GET /api/users/me/provider-keys
async fn list_provider_keys(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<Vec<user_keys::ProviderKeyMetadata>>, ApiError> {
    let keys = user_keys::list_user_keys(&state.pool, auth.user_id)
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(keys))
}

/// DELETE /api/users/me/provider-keys/{provider}
async fn delete_provider_key(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(provider): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    validation::check_name(&provider)?;

    let deleted = user_keys::delete_user_key(&state.pool, auth.user_id, &provider)
        .await
        .map_err(ApiError::Internal)?;

    if !deleted {
        return Err(ApiError::NotFound("provider key".into()));
    }

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "provider_key.delete",
            resource: "provider_key",
            resource_id: None,
            project_id: None,
            detail: Some(serde_json::json!({ "provider": provider })),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/users/me/provider-keys/validate
///
/// Makes a minimal Anthropic API call to verify the key works.
/// Returns 200 with `{ valid, error }` — never returns 4xx for bad keys.
async fn validate_provider_key(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<ValidateKeyRequest>,
) -> Result<Json<ValidateKeyResponse>, ApiError> {
    validation::check_length("api_key", &body.api_key, 10, 500)?;

    crate::auth::rate_limit::check_rate(
        &state.valkey,
        "validate_key",
        &auth.user_id.to_string(),
        10,
        300,
    )
    .await?;

    let result = validate_anthropic_key(&body.api_key).await;
    Ok(Json(result))
}

async fn validate_anthropic_key(api_key: &str) -> ValidateKeyResponse {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default();

    let body = serde_json::json!({
        "model": "claude-sonnet-4-5-20250929",
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "hi"}]
    });

    let resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => ValidateKeyResponse {
            valid: true,
            error: None,
        },
        Ok(r) => {
            let status = r.status().as_u16();
            let body_text = r.text().await.unwrap_or_default();
            let err_msg = serde_json::from_str::<serde_json::Value>(&body_text)
                .ok()
                .and_then(|v| {
                    v.get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|m| m.as_str().map(String::from))
                })
                .unwrap_or_else(|| format!("API returned status {status}"));
            ValidateKeyResponse {
                valid: false,
                error: Some(err_msg),
            }
        }
        Err(e) => ValidateKeyResponse {
            valid: false,
            error: Some(format!("Connection error: {e}")),
        },
    }
}
