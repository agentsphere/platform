// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

#[allow(dead_code, unused_imports)]
use std::collections::HashMap;
use std::convert::Infallible;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
#[allow(unused_imports)]
use sqlx::Row;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use crate::state::PlatformState;
use platform_secrets::{engine, llm_providers};
use platform_types::{ApiError, AuditEntry, AuthUser, ListResponse, send_audit, validation};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateProviderRequest {
    pub provider_type: String,
    pub label: Option<String>,
    pub env_vars: HashMap<String, String>,
    pub model: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateProviderResponse {
    pub id: Uuid,
}

#[derive(Debug, Deserialize)]
pub struct UpdateProviderRequest {
    pub env_vars: HashMap<String, String>,
    pub model: Option<String>,
    pub label: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SetActiveProviderRequest {
    pub provider: String,
}

#[derive(Debug, Serialize)]
pub struct ActiveProviderResponse {
    pub provider: String,
    pub provider_type: Option<String>,
    pub label: Option<String>,
    pub has_oauth: bool,
    pub has_api_key: bool,
    pub custom_configs: Vec<llm_providers::ProviderConfigMeta>,
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

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<PlatformState> {
    Router::new()
        .route(
            "/api/users/me/llm-providers",
            axum::routing::post(create_provider).get(list_providers),
        )
        .route(
            "/api/users/me/llm-providers/{id}",
            get(get_provider)
                .put(update_provider)
                .delete(delete_provider),
        )
        .route(
            "/api/users/me/llm-providers/{id}/validate",
            get(validate_provider),
        )
        .route(
            "/api/users/me/active-provider",
            axum::routing::put(set_active_provider).get(get_active_provider),
        )
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// POST /api/users/me/llm-providers
async fn create_provider(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Json(body): Json<CreateProviderRequest>,
) -> Result<(StatusCode, Json<CreateProviderResponse>), ApiError> {
    let label = body.label.as_deref().unwrap_or("");
    validation::check_length("label", label, 0, 255)?;
    validation::check_length("provider_type", &body.provider_type, 1, 255)?;
    if let Some(ref model) = body.model {
        validation::check_length("model", model, 1, 255)?;
    }
    if body.env_vars.len() > 50 {
        return Err(ApiError::BadRequest("too many env vars (max 50)".into()));
    }
    for (k, v) in &body.env_vars {
        validation::check_length("env_var key", k, 1, 255)?;
        validation::check_length("env_var value", v, 0, 10_000)?;
    }

    let master_key = get_master_key(&state)?;

    let id = llm_providers::create_config(
        &state.pool,
        &master_key,
        auth.user_id,
        &body.provider_type,
        label,
        &body.env_vars,
        body.model.as_deref(),
    )
    .await
    .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "llm_provider.create".into(),
            resource: "llm_provider_config".into(),
            resource_id: Some(id),
            project_id: None,
            detail: Some(serde_json::json!({
                "provider_type": body.provider_type,
                "label": label,
            })),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok((StatusCode::CREATED, Json(CreateProviderResponse { id })))
}

/// GET /api/users/me/llm-providers/{id}
async fn get_provider(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<llm_providers::ProviderConfigMeta>, ApiError> {
    let row = sqlx::query(
        "SELECT id, provider_type, label, model, validation_status, \
                last_validated_at, created_at, updated_at \
         FROM llm_provider_configs WHERE id = $1 AND user_id = $2",
    )
    .bind(id)
    .bind(auth.user_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| ApiError::Internal(e.into()))?
    .ok_or_else(|| ApiError::NotFound("llm provider config".into()))?;

    Ok(Json(llm_providers::ProviderConfigMeta {
        id: row.get("id"),
        provider_type: row.get("provider_type"),
        label: row.get("label"),
        model: row.get("model"),
        validation_status: row.get("validation_status"),
        last_validated_at: row.get("last_validated_at"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }))
}

/// GET /api/users/me/llm-providers
async fn list_providers(
    State(state): State<PlatformState>,
    auth: AuthUser,
) -> Result<Json<ListResponse<llm_providers::ProviderConfigMeta>>, ApiError> {
    let items = llm_providers::list_configs(&state.pool, auth.user_id)
        .await
        .map_err(ApiError::Internal)?;
    let total = i64::try_from(items.len()).unwrap_or(0);
    Ok(Json(ListResponse { items, total }))
}

/// PUT /api/users/me/llm-providers/{id}
async fn update_provider(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateProviderRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let label = body.label.as_deref().unwrap_or("");
    validation::check_length("label", label, 0, 255)?;
    if let Some(ref model) = body.model {
        validation::check_length("model", model, 1, 255)?;
    }

    let master_key = get_master_key(&state)?;

    let updated = llm_providers::update_config(
        &state.pool,
        &master_key,
        id,
        auth.user_id,
        &body.env_vars,
        body.model.as_deref(),
        label,
    )
    .await
    .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    if !updated {
        return Err(ApiError::NotFound("llm provider config".into()));
    }

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "llm_provider.update".into(),
            resource: "llm_provider_config".into(),
            resource_id: Some(id),
            project_id: None,
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /api/users/me/llm-providers/{id}
async fn delete_provider(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    let deleted = llm_providers::delete_config(&state.pool, id, auth.user_id)
        .await
        .map_err(ApiError::Internal)?;

    if !deleted {
        return Err(ApiError::NotFound("llm provider config".into()));
    }

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "llm_provider.delete".into(),
            resource: "llm_provider_config".into(),
            resource_id: Some(id),
            project_id: None,
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(StatusCode::NO_CONTENT)
}

/// GET /api/users/me/llm-providers/{id}/validate — SSE validation stream.
async fn validate_provider(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    // Rate limit: 3 per 5 minutes
    platform_auth::rate_limit::check_rate(
        &state.valkey,
        "llm_validate",
        &auth.user_id.to_string(),
        3,
        300,
    )
    .await?;

    let master_key = get_master_key(&state)?;

    let config = llm_providers::get_config(&state.pool, &master_key, id, auth.user_id)
        .await
        .map_err(ApiError::Internal)?
        .ok_or_else(|| ApiError::NotFound("llm provider config".into()))?;

    let (api_key, extra_env) = platform_agent::llm_validate::build_provider_extra_env(
        &config.provider_type,
        &config.env_vars,
    );

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_clone = cancel.clone();
    let pool = state.pool.clone();
    let model = config.model.clone();
    let user_id = auth.user_id;

    tokio::spawn(async move {
        platform_agent::llm_validate::run_validation(
            &pool,
            id,
            user_id,
            api_key,
            extra_env,
            model,
            tx,
            cancel_clone,
        )
        .await;
    });

    let stream = ReceiverStream::new(rx).map(move |event| {
        let event_type = match &event {
            platform_agent::llm_validate::ValidationEvent::Test(_) => "test",
            platform_agent::llm_validate::ValidationEvent::Done { .. } => "done",
        };
        let data = serde_json::to_string(&event).unwrap_or_default();
        Ok::<_, Infallible>(Event::default().event(event_type).data(data))
    });

    // Cancel validation when client disconnects
    let cancel_on_drop = cancel;
    let guarded_stream = stream.map(move |item| {
        let _keep = &cancel_on_drop;
        item
    });

    Ok(Sse::new(guarded_stream).keep_alive(KeepAlive::default()))
}

/// PUT /api/users/me/active-provider
async fn set_active_provider(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Json(body): Json<SetActiveProviderRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let value = &body.provider;

    // Validate the provider value
    match value.as_str() {
        "auto" | "global" => {}
        "oauth" => {
            // Check that the user has OAuth credentials
            let has = sqlx::query_scalar!(
                r#"SELECT EXISTS(SELECT 1 FROM cli_credentials WHERE user_id = $1) as "exists!""#,
                auth.user_id,
            )
            .fetch_one(&state.pool)
            .await?;
            if !has {
                return Err(ApiError::BadRequest(
                    "no OAuth credentials configured".into(),
                ));
            }
        }
        "api_key" => {
            // Check that the user has an Anthropic API key
            let has = sqlx::query_scalar!(
                r#"SELECT EXISTS(SELECT 1 FROM user_provider_keys WHERE user_id = $1 AND provider = 'anthropic') as "exists!""#,
                auth.user_id,
            )
            .fetch_one(&state.pool)
            .await?;
            if !has {
                return Err(ApiError::BadRequest(
                    "no Anthropic API key configured".into(),
                ));
            }
        }
        v if v.starts_with("custom:") => {
            let config_id = v
                .strip_prefix("custom:")
                .and_then(|s| Uuid::parse_str(s).ok())
                .ok_or_else(|| ApiError::BadRequest("invalid custom provider ID".into()))?;

            // Verify config exists, belongs to user, and is validated
            let row = sqlx::query!(
                "SELECT validation_status FROM llm_provider_configs WHERE id = $1 AND user_id = $2",
                config_id,
                auth.user_id,
            )
            .fetch_optional(&state.pool)
            .await?;

            let Some(row) = row else {
                return Err(ApiError::NotFound("llm provider config".into()));
            };
            if row.validation_status != "valid" {
                return Err(ApiError::BadRequest(
                    "provider config must pass validation before it can be activated".into(),
                ));
            }
        }
        _ => {
            return Err(ApiError::BadRequest(format!(
                "invalid provider value: {value}"
            )));
        }
    }

    llm_providers::set_active_provider(&state.pool, auth.user_id, value)
        .await
        .map_err(ApiError::Internal)?;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "llm_provider.switch".into(),
            resource: "user".into(),
            resource_id: None,
            project_id: None,
            detail: Some(serde_json::json!({ "provider": value })),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(StatusCode::NO_CONTENT)
}

/// GET /api/users/me/active-provider
async fn get_active_provider(
    State(state): State<PlatformState>,
    auth: AuthUser,
) -> Result<Json<ActiveProviderResponse>, ApiError> {
    let active = llm_providers::get_active_provider(&state.pool, auth.user_id)
        .await
        .map_err(ApiError::Internal)?;

    let custom_configs = llm_providers::list_configs(&state.pool, auth.user_id)
        .await
        .map_err(ApiError::Internal)?;

    // Check if user has OAuth credentials
    let has_oauth = sqlx::query_scalar!(
        r#"SELECT EXISTS(SELECT 1 FROM cli_credentials WHERE user_id = $1) as "exists!""#,
        auth.user_id,
    )
    .fetch_one(&state.pool)
    .await
    .unwrap_or(false);

    // Check if user has an Anthropic API key
    let has_api_key = sqlx::query_scalar!(
        r#"SELECT EXISTS(SELECT 1 FROM user_provider_keys WHERE user_id = $1 AND provider = 'anthropic') as "exists!""#,
        auth.user_id,
    )
    .fetch_one(&state.pool)
    .await
    .unwrap_or(false);

    // Resolve provider_type and label for the active provider
    let (provider_type, label) = if active.starts_with("custom:") {
        let config_id = active
            .strip_prefix("custom:")
            .and_then(|s| Uuid::parse_str(s).ok());
        if let Some(cid) = config_id {
            custom_configs
                .iter()
                .find(|c| c.id == cid)
                .map_or((None, None), |c| {
                    (Some(c.provider_type.clone()), Some(c.label.clone()))
                })
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };

    Ok(Json(ActiveProviderResponse {
        provider: active,
        provider_type,
        label,
        has_oauth,
        has_api_key,
        custom_configs,
    }))
}
