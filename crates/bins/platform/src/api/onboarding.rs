// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

#[allow(dead_code, unused_imports)]
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use platform_auth::resolver;
use platform_types::validation;
use platform_types::{ApiError, AuditEntry, AuthUser, Permission, send_audit};

use crate::api::helpers::require_admin;
use crate::state::PlatformState;

// TODO: wire from platform-operator — onboarding::presets not yet extracted
// use crate::onboarding::presets::{self, OrgType, PasskeyPolicy};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct WizardStatusResponse {
    pub show_wizard: bool,
}

// TODO: wire from platform-operator — OrgType and PasskeyPolicy enums not yet extracted
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct OrgType;

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PasskeyPolicy;

#[derive(Debug, Deserialize)]
pub struct WizardRequest {
    pub org_type: serde_json::Value,
    /// Override passkey enforcement (defaults to org-type preset).
    pub passkey_policy: Option<serde_json::Value>,
    /// Anthropic API key (validated and saved if provided).
    pub provider_key: Option<String>,
    /// CLI credential token — for manual OAuth token paste (stored in `cli_credentials`).
    pub cli_token: Option<CliTokenInput>,
    /// Custom LLM provider (Bedrock, Vertex, Azure Foundry, or custom endpoint).
    pub custom_provider: Option<CustomProviderInput>,
}

#[derive(Debug, Deserialize)]
pub struct CustomProviderInput {
    pub provider_type: String,
    pub env_vars: std::collections::HashMap<String, String>,
    pub model: Option<String>,
    pub label: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CliTokenInput {
    pub auth_type: String,
    pub token: String,
}

#[derive(Debug, Serialize)]
pub struct WizardResponse {
    pub success: bool,
}

#[derive(Debug, Serialize)]
pub struct SettingsResponse {
    pub org_type: Option<serde_json::Value>,
    pub onboarding_completed: bool,
    pub security_policy: Option<serde_json::Value>,
    pub preset_config: Option<serde_json::Value>,
    pub demo_project_id: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateSettingsRequest {
    pub org_type: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct DemoProjectResponse {
    pub project_id: Uuid,
    pub project_name: String,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

// Claude auth flow types
#[derive(Debug, Serialize)]
pub struct ClaudeAuthStartResponse {
    pub session_id: Uuid,
    pub auth_url: String,
}

#[derive(Debug, Deserialize)]
pub struct ClaudeAuthCodeRequest {
    pub code: String,
}

#[derive(Debug, Serialize)]
pub struct ClaudeAuthCodeResponse {
    pub success: bool,
}

#[derive(Debug, Deserialize)]
pub struct VerifyOAuthTokenRequest {
    pub token: String,
}

#[derive(Debug, Serialize)]
pub struct VerifyOAuthTokenResponse {
    pub valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub fn router() -> Router<PlatformState> {
    Router::new()
        .route("/api/onboarding/wizard-status", get(wizard_status))
        .route("/api/onboarding/wizard", post(complete_wizard))
        .route(
            "/api/onboarding/settings",
            get(get_settings).patch(update_settings),
        )
        .route("/api/onboarding/demo-project", post(create_demo_project))
        .route("/api/onboarding/claude-auth/start", post(start_claude_auth))
        .route(
            "/api/onboarding/claude-auth/{id}",
            get(claude_auth_status).delete(cancel_claude_auth),
        )
        .route(
            "/api/onboarding/claude-auth/{id}/code",
            post(submit_auth_code),
        )
        .route(
            "/api/onboarding/claude-auth/verify-token",
            post(verify_oauth_token),
        )
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Check if the wizard should be shown for the current user.
async fn wizard_status(
    State(state): State<PlatformState>,
    auth: AuthUser,
) -> Result<Json<WizardStatusResponse>, ApiError> {
    // TODO: wire from platform-operator — presets::is_wizard_completed not yet extracted
    let completed = is_wizard_completed_fallback(&state).await.unwrap_or(false);

    // Only show wizard if: not completed AND user is admin
    let is_admin = resolver::has_permission_scoped(
        &state.pool,
        &state.valkey,
        auth.user_id,
        None,
        Permission::AdminUsers,
        auth.token_scopes.as_deref(),
    )
    .await
    .unwrap_or(false);

    Ok(Json(WizardStatusResponse {
        show_wizard: !completed && is_admin,
    }))
}

/// Save all wizard choices and apply the selected preset.
async fn complete_wizard(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Json(body): Json<WizardRequest>,
) -> Result<Json<WizardResponse>, ApiError> {
    require_admin(&state, &auth).await?;

    // TODO: wire from platform-operator — presets::apply_preset not yet extracted
    // Apply preset (writes org_type, preset_config, security_policy)
    // presets::apply_preset(&state.pool, body.org_type)
    //     .await
    //     .map_err(|e| ApiError::Internal(e.into()))?;

    // Save provider key if provided
    if let Some(ref key) = body.provider_key {
        save_provider_key(&state, auth.user_id, key).await?;
    }

    // Save CLI token if provided (manual OAuth token paste)
    if let Some(ref cli_token) = body.cli_token {
        save_cli_token(&state, auth.user_id, cli_token).await?;
    }

    // Save custom provider if provided (Bedrock, Vertex, Azure Foundry, custom endpoint)
    if let Some(ref custom) = body.custom_provider {
        save_custom_provider(&state, auth.user_id, custom).await?;
    }

    // TODO: wire from platform-operator — presets::mark_wizard_completed not yet extracted
    // presets::mark_wizard_completed(&state.pool)
    //     .await
    //     .map_err(|e| ApiError::Internal(e.into()))?;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "onboarding.wizard_completed".into(),
            resource: "platform_settings".into(),
            resource_id: None,
            project_id: None,
            detail: Some(serde_json::json!({
                "org_type": body.org_type,
            })),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(Json(WizardResponse { success: true }))
}

/// Read current onboarding settings.
async fn get_settings(
    State(state): State<PlatformState>,
    auth: AuthUser,
) -> Result<Json<SettingsResponse>, ApiError> {
    require_admin(&state, &auth).await?;

    // TODO: wire from platform-operator — presets::get_setting not yet extracted
    let org_type = get_setting_fallback(&state, "org_type").await;
    let completed = is_wizard_completed_fallback(&state).await.unwrap_or(false);
    let security = get_setting_fallback(&state, "security_policy").await;
    let preset = get_setting_fallback(&state, "preset_config").await;
    let demo_id = get_setting_fallback(&state, "demo_project_id").await;

    Ok(Json(SettingsResponse {
        org_type,
        onboarding_completed: completed,
        security_policy: security,
        preset_config: preset,
        demo_project_id: demo_id,
    }))
}

/// Update org type (additive changes only).
async fn update_settings(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Json(_body): Json<UpdateSettingsRequest>,
) -> Result<Json<SettingsResponse>, ApiError> {
    require_admin(&state, &auth).await?;

    // TODO: wire from platform-operator — presets::apply_preset not yet extracted
    // if let Some(org_type) = body.org_type {
    //     presets::apply_preset(&state.pool, org_type)
    //         .await
    //         .map_err(|e| ApiError::Internal(e.into()))?;
    // }

    // Re-read and return current settings
    get_settings(State(state), auth).await
}

/// Create demo project with full infrastructure + pipeline trigger.
async fn create_demo_project(
    State(state): State<PlatformState>,
    auth: AuthUser,
) -> Result<Json<DemoProjectResponse>, ApiError> {
    require_admin(&state, &auth).await?;

    let (project_id, project_name) =
        crate::demo::demo_project::create_demo_project(&state, auth.user_id)
            .await
            .map_err(ApiError::Internal)?;

    Ok(Json(DemoProjectResponse {
        project_id,
        project_name,
    }))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Fallback for checking wizard completion directly from DB.
async fn is_wizard_completed_fallback(state: &PlatformState) -> Result<bool, anyhow::Error> {
    let val: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT value FROM platform_settings WHERE key = 'onboarding_completed'",
    )
    .fetch_optional(&state.pool)
    .await?;

    Ok(val.is_some_and(|v| v.as_bool().unwrap_or(false)))
}

/// Fallback for reading a `platform_settings` key directly from DB.
async fn get_setting_fallback(state: &PlatformState, key: &str) -> Option<serde_json::Value> {
    sqlx::query_scalar("SELECT value FROM platform_settings WHERE key = $1")
        .bind(key)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten()
}

/// Save an Anthropic API key for the user (reuses existing provider key logic).
async fn save_provider_key(
    state: &PlatformState,
    user_id: Uuid,
    api_key: &str,
) -> Result<(), ApiError> {
    let hex_str = state
        .config
        .secrets
        .master_key
        .as_deref()
        .ok_or_else(|| ApiError::BadRequest("master key not configured".into()))?;

    let master_key =
        platform_secrets::engine::parse_master_key(hex_str).map_err(ApiError::Internal)?;

    let key_bytes = platform_secrets::engine::encrypt(api_key.as_bytes(), &master_key)
        .map_err(ApiError::Internal)?;

    let suffix = if api_key.len() >= 4 {
        &api_key[api_key.len() - 4..]
    } else {
        api_key
    };

    sqlx::query(
        r"INSERT INTO user_provider_keys (id, user_id, provider, encrypted_key, key_suffix, created_at, updated_at)
           VALUES (gen_random_uuid(), $1, 'anthropic', $2, $3, now(), now())
           ON CONFLICT (user_id, provider)
           DO UPDATE SET encrypted_key = $2, key_suffix = $3, updated_at = now()",
    )
    .bind(user_id)
    .bind(&key_bytes)
    .bind(suffix)
    .execute(&state.pool)
    .await?;

    Ok(())
}

/// Save a CLI credential token (`OAuth`/`setup_token`) for the user.
async fn save_cli_token(
    state: &PlatformState,
    user_id: uuid::Uuid,
    input: &CliTokenInput,
) -> Result<(), ApiError> {
    let hex_str = state
        .config
        .secrets
        .master_key
        .as_deref()
        .ok_or_else(|| ApiError::BadRequest("master key not configured".into()))?;

    let master_key =
        platform_secrets::engine::parse_master_key(hex_str).map_err(ApiError::Internal)?;

    platform_secrets::cli_creds::store_credentials(
        &state.pool,
        &master_key,
        user_id,
        &input.auth_type,
        &input.token,
        None,
    )
    .await
    .map_err(ApiError::Internal)?;

    Ok(())
}

/// Save a custom LLM provider config and set it as the user's active provider.
async fn save_custom_provider(
    state: &PlatformState,
    user_id: Uuid,
    input: &CustomProviderInput,
) -> Result<(), ApiError> {
    let hex_str = state
        .config
        .secrets
        .master_key
        .as_deref()
        .ok_or_else(|| ApiError::BadRequest("master key not configured".into()))?;

    let master_key =
        platform_secrets::engine::parse_master_key(hex_str).map_err(ApiError::Internal)?;

    let label = input.label.as_deref().unwrap_or("");

    let config_id = platform_secrets::llm_providers::create_config(
        &state.pool,
        &master_key,
        user_id,
        &input.provider_type,
        label,
        &input.env_vars,
        input.model.as_deref(),
    )
    .await
    .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    // Set as active provider (untested — user should validate via the Settings page)
    let active_value = format!("custom:{config_id}");
    platform_secrets::llm_providers::set_active_provider(&state.pool, user_id, &active_value)
        .await
        .map_err(ApiError::Internal)?;

    Ok(())
}

/// Create a team workspace named "team" if one doesn't already exist.
async fn create_team_workspace(state: &PlatformState, owner_id: Uuid) -> Result<(), ApiError> {
    let exists: bool = sqlx::query_scalar(
        r#"SELECT EXISTS(
            SELECT 1 FROM workspaces
            WHERE owner_id = $1 AND name = 'team'
        ) as "exists!""#,
    )
    .bind(owner_id)
    .fetch_one(&state.pool)
    .await
    .unwrap_or(false);

    if !exists {
        // TODO: wire from platform crate — workspace::service not yet extracted
        // if let Err(e) = crate::workspace::service::create_workspace(
        //     &state.pool,
        //     owner_id,
        //     "team",
        //     Some("Team"),
        //     Some("Shared team workspace"),
        // )
        // .await
        // {
        //     tracing::warn!(error = %e, "failed to create team workspace");
        // }
        let _ = owner_id;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Claude CLI auth flow handlers
// ---------------------------------------------------------------------------

/// Start a Claude CLI OAuth flow. Spawns `claude setup-token`, extracts the
/// OAuth URL, and returns it. The process stays alive waiting for the code.
async fn start_claude_auth(
    State(state): State<PlatformState>,
    auth: AuthUser,
) -> Result<Json<ClaudeAuthStartResponse>, ApiError> {
    require_admin(&state, &auth).await?;

    // Rate limit: 50/hour in dev mode, 5/hour in production
    let max_attempts = if state.config.core.dev_mode { 50 } else { 5 };
    platform_auth::rate_limit::check_rate(
        &state.valkey,
        "claude-auth",
        &auth.user_id.to_string(),
        max_attempts,
        3600,
    )
    .await?;

    // Resolve claude CLI path (prefer installed binary, fall back to "claude")
    let cli_path = which_claude().unwrap_or_else(|| "claude".to_string());

    // TODO: wire from platform crate — cli_auth_manager not yet on PlatformState
    // let (session_id, auth_url) = state
    //     .cli_auth_manager
    //     .start_auth(auth.user_id, &cli_path)
    //     .await
    //     .map_err(ApiError::Internal)?;
    let _ = cli_path;
    Err(ApiError::Internal(anyhow::anyhow!(
        "cli_auth_manager not yet wired in platform crate"
    )))
}

/// Check the status of a Claude CLI auth session.
/// Only the session owner or an admin can check status.
async fn claude_auth_status(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    // TODO: wire from platform crate — cli_auth_manager not yet on PlatformState
    // let owner_id = state
    //     .cli_auth_manager
    //     .get_owner(id)
    //     .await
    //     .ok_or_else(|| ApiError::NotFound("session".into()))?;
    let _ = (&state, &auth, id);
    Err::<Json<serde_json::Value>, _>(ApiError::Internal(anyhow::anyhow!(
        "cli_auth_manager not yet wired in platform crate"
    )))
}

/// Submit the authentication code from claude.ai to the CLI process.
/// The code is piped to the running `claude setup-token` which exchanges it
/// for an OAuth token. The token is then validated via `validate_oauth_token`
/// and stored — **the token never leaves the backend**.
async fn submit_auth_code(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<ClaudeAuthCodeRequest>,
) -> Result<Json<ClaudeAuthCodeResponse>, ApiError> {
    require_admin(&state, &auth).await?;

    // TODO: wire from platform crate — cli_auth_manager not yet on PlatformState
    let _ = (&state, id, &body);
    Err(ApiError::Internal(anyhow::anyhow!(
        "cli_auth_manager not yet wired in platform crate"
    )))
}

/// Cancel a Claude CLI auth session.
/// Only the session owner or an admin can cancel.
async fn cancel_claude_auth(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    // TODO: wire from platform crate — cli_auth_manager not yet on PlatformState
    let _ = (&state, &auth, id);
    Err::<StatusCode, _>(ApiError::Internal(anyhow::anyhow!(
        "cli_auth_manager not yet wired in platform crate"
    )))
}

/// Verify an existing OAuth token by spawning `claude --print` with it.
/// If valid, stores the token encrypted and returns `{valid: true}`.
async fn verify_oauth_token(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Json(body): Json<VerifyOAuthTokenRequest>,
) -> Result<Json<VerifyOAuthTokenResponse>, ApiError> {
    require_admin(&state, &auth).await?;

    validation::check_length("token", &body.token, 10, 500)?;

    let max_attempts = if state.config.core.dev_mode { 50 } else { 10 };
    platform_auth::rate_limit::check_rate(
        &state.valkey,
        "verify-oauth",
        &auth.user_id.to_string(),
        max_attempts,
        300,
    )
    .await?;

    // TODO: wire from platform-operator — onboarding::claude_auth not yet extracted
    // let cli_path = which_claude().unwrap_or_else(|| "claude".to_string());
    // let valid = crate::onboarding::claude_auth::validate_oauth_token(&cli_path, &body.token)
    //     .await
    //     .map_err(|e| {
    //         tracing::warn!(error = %e, "oauth token validation failed");
    //         ApiError::Internal(e)
    //     })?;
    let _ = &body;
    Err(ApiError::Internal(anyhow::anyhow!(
        "oauth token validation not yet wired in platform crate"
    )))
}

/// Find the Claude CLI binary path.
///
/// Checks `CLAUDE_CLI_PATH` env var first (set by test harness to point at the
/// mock CLI in `tests/fixtures/claude-mock/claude`), then falls back to `which claude`.
fn which_claude() -> Option<String> {
    if let Ok(p) = std::env::var("CLAUDE_CLI_PATH")
        && !p.is_empty()
    {
        return Some(p);
    }
    std::process::Command::new("which")
        .arg("claude")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}
