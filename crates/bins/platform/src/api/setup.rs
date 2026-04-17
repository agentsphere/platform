// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

#[allow(dead_code, unused_imports)]
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use platform_auth::password;
use platform_types::validation;
use platform_types::{ApiError, AuditEntry, send_audit};

use crate::state::PlatformState;

// TODO: wire from platform crate — bootstrap module not yet extracted
// use crate::store::bootstrap;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SetupRequest {
    pub token: String,
    pub name: String,
    pub email: String,
    pub password: String,
    pub display_name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SetupResponse {
    pub id: String,
    pub name: String,
    pub email: String,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct SetupStatusResponse {
    pub needs_setup: bool,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<PlatformState> {
    Router::new()
        .route("/api/setup", post(setup))
        .route("/api/setup/status", get(setup_status))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /api/setup/status — check if setup is needed (no auth required).
async fn setup_status(
    State(state): State<PlatformState>,
) -> Result<Json<SetupStatusResponse>, ApiError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM users WHERE user_type IS DISTINCT FROM 'service_account'",
    )
    .fetch_one(&state.pool)
    .await?;

    Ok(Json(SetupStatusResponse {
        needs_setup: count == 0,
    }))
}

/// POST /api/setup — create the first admin user using a setup token (no auth required).
async fn setup(
    State(state): State<PlatformState>,
    Json(body): Json<SetupRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // If human users already exist, return 404 (no information leak)
    let user_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM users WHERE user_type IS DISTINCT FROM 'service_account'",
    )
    .fetch_one(&state.pool)
    .await?;

    if user_count > 0 {
        return Err(ApiError::NotFound("not found".into()));
    }

    // Rate limit: 3 attempts per 5 minutes
    platform_auth::rate_limit::check_rate(&state.valkey, "setup", "global", 3, 300).await?;

    // Validate inputs
    validate_setup_request(&body)?;

    // Atomically consume the token (prevents race condition with concurrent requests)
    // TODO: wire from platform crate — bootstrap::hash_setup_token not yet extracted
    let token_hash = crate::bootstrap::hash_setup_token(&body.token);
    let consumed = sqlx::query(
        "UPDATE setup_tokens SET used_at = now()
         WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now()
         RETURNING id",
    )
    .bind(&token_hash)
    .fetch_optional(&state.pool)
    .await?;

    if consumed.is_none() {
        return Err(ApiError::Unauthorized);
    }

    // Create the admin user
    let hash = password::hash_password(&body.password).map_err(ApiError::Internal)?;

    let display = body.display_name.as_deref().unwrap_or(&body.name);

    let admin_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, name, display_name, email, password_hash)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(admin_id)
    .bind(&body.name)
    .bind(display)
    .bind(&body.email)
    .bind(&hash)
    .execute(&state.pool)
    .await?;

    // Assign admin role
    sqlx::query(
        "INSERT INTO user_roles (id, user_id, role_id)
         SELECT $1, $2, r.id FROM roles r WHERE r.name = 'admin'",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(admin_id)
    .execute(&state.pool)
    .await?;

    // Create personal workspace
    // TODO: wire from platform crate — workspace::service not yet extracted
    // crate::workspace::service::get_or_create_default_workspace(
    //     &state.pool,
    //     admin_id,
    //     &body.name,
    //     display,
    // )
    // .await?;

    // Audit
    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: admin_id,
            actor_name: body.name.clone(),
            action: "setup.complete".into(),
            resource: "user".into(),
            resource_id: Some(admin_id),
            project_id: None,
            detail: Some(serde_json::json!({"email": body.email})),
            ip_addr: None,
        },
    );

    tracing::info!(user_id = %admin_id, name = %body.name, "setup completed — admin user created");

    // Auto-create demo project + trigger pipeline in background
    // TODO: wire from platform-operator — onboarding::demo_project not yet extracted
    // let demo_state = state.clone();
    // tokio::spawn(async move {
    //     // Small delay so background tasks (executor, reconciler) are running
    //     tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    //     if let Err(e) =
    //         crate::onboarding::demo_project::create_and_trigger_demo(&demo_state, admin_id).await
    //     {
    //         tracing::warn!(error = %e, "auto demo project creation failed");
    //     }
    // });

    Ok((
        StatusCode::OK,
        Json(SetupResponse {
            id: admin_id.to_string(),
            name: body.name,
            email: body.email,
            message: "Admin user created successfully. You can now log in.".into(),
        }),
    ))
}

fn validate_setup_request(body: &SetupRequest) -> Result<(), ApiError> {
    validation::check_name(&body.name)?;
    validation::check_email(&body.email)?;
    validation::check_length("password", &body.password, 8, 1024)?;
    if let Some(ref dn) = body.display_name {
        validation::check_length("display_name", dn, 1, 255)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_body(name: &str, email: &str, password: &str) -> SetupRequest {
        SetupRequest {
            token: "dummy".into(),
            name: name.into(),
            email: email.into(),
            password: password.into(),
            display_name: None,
        }
    }

    #[test]
    fn setup_request_validation_empty_name() {
        assert!(validate_setup_request(&make_body("", "a@b.c", "password123")).is_err());
    }

    #[test]
    fn setup_request_validation_empty_email() {
        assert!(validate_setup_request(&make_body("admin", "", "password123")).is_err());
    }

    #[test]
    fn setup_request_validation_short_password() {
        assert!(validate_setup_request(&make_body("admin", "a@b.c", "short")).is_err());
    }

    #[test]
    fn setup_request_validation_email_format() {
        assert!(validate_setup_request(&make_body("admin", "notanemail", "password123")).is_err());
    }

    #[test]
    fn setup_request_validation_name_too_long() {
        let long_name = "a".repeat(256);
        assert!(validate_setup_request(&make_body(&long_name, "a@b.c", "password123")).is_err());
    }

    #[test]
    fn setup_request_validation_password_too_long() {
        let long_pw = "a".repeat(1025);
        assert!(validate_setup_request(&make_body("admin", "a@b.c", &long_pw)).is_err());
    }

    #[test]
    fn setup_request_validation_valid() {
        assert!(
            validate_setup_request(&make_body("admin", "admin@example.com", "password123")).is_ok()
        );
    }
}
