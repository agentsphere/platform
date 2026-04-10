// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use ts_rs::TS;

use crate::audit::{AuditEntry, send_audit};
use crate::auth::middleware::AuthUser;
use crate::auth::user_type::UserType;
use crate::auth::{password, token};
use crate::error::ApiError;
use crate::rbac::Permission;
use crate::store::AppState;
use crate::validation;

use super::helpers::require_admin;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateUserRequest {
    pub name: String,
    pub email: String,
    pub password: Option<String>,
    pub display_name: Option<String>,
    pub user_type: Option<UserType>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateUserRequest {
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub password: Option<String>,
    pub current_password: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub name: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct ListParams {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize, TS)]
#[ts(export, rename = "User")]
pub struct UserResponse {
    pub id: Uuid,
    pub name: String,
    pub display_name: Option<String>,
    pub email: String,
    pub user_type: UserType,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

use super::helpers::ListResponse;

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct LoginResponse {
    pub token: String,
    pub expires_at: DateTime<Utc>,
    pub user: UserResponse,
}

#[derive(Debug, Deserialize)]
pub struct CreateTokenRequest {
    pub name: String,
    pub scopes: Option<Vec<String>>,
    pub project_id: Option<Uuid>,
    pub expires_in_days: Option<i64>,
}

#[derive(Debug, Serialize, TS)]
#[ts(export, rename = "ApiToken")]
pub struct TokenResponse {
    pub id: Uuid,
    pub name: String,
    pub scopes: Vec<String>,
    pub project_id: Option<Uuid>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct CreateTokenResponse {
    pub token: String,
    #[serde(flatten)]
    pub info: TokenResponse,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        // Auth routes (no auth required for login)
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/me", get(me))
        // User management (admin checks done inline per handler)
        .route("/api/users", get(list_users).post(create_user))
        // User self-service + admin
        .route(
            "/api/users/{id}",
            get(get_user).patch(update_user).delete(deactivate_user),
        )
        // API token management (authenticated)
        .route("/api/tokens", post(create_api_token).get(list_api_tokens))
        .route(
            "/api/tokens/{id}",
            get(get_api_token).delete(revoke_api_token),
        )
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_user_type(s: &str) -> Result<UserType, ApiError> {
    s.parse::<UserType>()
        .map_err(|e: anyhow::Error| ApiError::Internal(e))
}

// ---------------------------------------------------------------------------
// Auth handlers
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state, body), fields(username = %body.name), err)]
async fn login(
    State(state): State<AppState>,
    Json(body): Json<LoginRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // Rate-limit login attempts (10 per 5 minutes per username)
    crate::auth::rate_limit::check_rate(&state.valkey, "login", &body.name, 10, 300).await?;

    // Look up user by name (include user_type for login gate)
    let user = sqlx::query!(
        r#"
        SELECT id, name, display_name, email, password_hash, is_active,
               user_type, created_at, updated_at
        FROM users
        WHERE name = $1
        "#,
        body.name,
    )
    .fetch_optional(&state.pool)
    .await?;

    // Timing-safe: always run argon2 verify even when user not found
    let (hash_to_verify, user) = match user {
        Some(u) => (u.password_hash.clone(), Some(u)),
        None => (password::dummy_hash().to_owned(), None),
    };

    let password_valid = password::verify_password(&body.password, &hash_to_verify);

    let user = match user {
        Some(u) if password_valid && u.is_active => u,
        _ => return Err(ApiError::Unauthorized),
    };

    // Reject non-human users (timing-safe: check after password verify)
    let user_type = parse_user_type(&user.user_type)?;
    if !user_type.can_login() {
        return Err(ApiError::Unauthorized);
    }

    // Check for disabled password (passkey-only accounts)
    if user.password_hash == "!disabled" {
        return Err(ApiError::BadRequest(
            "this account uses passkey authentication — use the passkey login flow".into(),
        ));
    }

    // Create session
    let session = create_login_session(&state, user.id, &user.name).await?;

    let response = LoginResponse {
        token: session.token,
        expires_at: session.expires_at,
        user: UserResponse {
            id: user.id,
            name: user.name,
            display_name: user.display_name,
            email: user.email,
            user_type,
            is_active: user.is_active,
            created_at: user.created_at,
            updated_at: user.updated_at,
        },
    };

    // Set session cookie + return JSON
    let secure_flag = if state.config.secure_cookies {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "session={}; Path=/; HttpOnly; SameSite=Strict; Max-Age=86400{secure_flag}",
        response.token
    );
    Ok((
        StatusCode::OK,
        [(axum::http::header::SET_COOKIE, cookie)],
        Json(response),
    ))
}

struct SessionInfo {
    token: String,
    expires_at: DateTime<Utc>,
}

async fn create_login_session(
    state: &AppState,
    user_id: Uuid,
    user_name: &str,
) -> Result<SessionInfo, ApiError> {
    let (raw_token, token_hash) = token::generate_session_token();
    let expires_at = Utc::now() + Duration::hours(24);

    sqlx::query!(
        r#"
        INSERT INTO auth_sessions (user_id, token_hash, expires_at)
        VALUES ($1, $2, $3)
        "#,
        user_id,
        token_hash,
        expires_at,
    )
    .execute(&state.pool)
    .await?;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: user_id,
            actor_name: user_name.to_string(),
            action: "auth.login".into(),
            resource: "session".into(),
            resource_id: None,
            project_id: None,
            detail: None,
            ip_addr: None,
        },
    );

    Ok(SessionInfo {
        token: raw_token,
        expires_at,
    })
}

#[tracing::instrument(skip(state), fields(user_id = %auth.user_id), err)]
async fn logout(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<impl IntoResponse, ApiError> {
    // If we have a session token hash (session-based auth), delete only this session.
    // If authenticated via API token, fall back to deleting all sessions for the user.
    if let Some(ref hash) = auth.session_token_hash {
        sqlx::query("DELETE FROM auth_sessions WHERE user_id = $1 AND token_hash = $2")
            .bind(auth.user_id)
            .bind(hash)
            .execute(&state.pool)
            .await?;
    } else {
        sqlx::query!("DELETE FROM auth_sessions WHERE user_id = $1", auth.user_id)
            .execute(&state.pool)
            .await?;
    }

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "auth.logout".into(),
            resource: "session".into(),
            resource_id: None,
            project_id: None,
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    let cookie = "session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0";
    Ok((
        StatusCode::OK,
        [(axum::http::header::SET_COOKIE, cookie.to_owned())],
        Json(serde_json::json!({"ok": true})),
    ))
}

async fn me(State(state): State<AppState>, auth: AuthUser) -> Result<Json<UserResponse>, ApiError> {
    let user = sqlx::query!(
        r#"
        SELECT id, name, display_name, email, user_type, is_active, created_at, updated_at
        FROM users WHERE id = $1
        "#,
        auth.user_id,
    )
    .fetch_one(&state.pool)
    .await?;

    Ok(Json(UserResponse {
        id: user.id,
        name: user.name,
        display_name: user.display_name,
        email: user.email,
        user_type: parse_user_type(&user.user_type)?,
        is_active: user.is_active,
        created_at: user.created_at,
        updated_at: user.updated_at,
    }))
}

// ---------------------------------------------------------------------------
// User CRUD handlers
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state, body), fields(new_user = %body.name), err)]
async fn create_user(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<CreateUserRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &auth).await?;

    let user_type = body.user_type.unwrap_or(UserType::Human);

    // Validate inputs
    validation::check_name(&body.name)?;
    validation::check_email(&body.email)?;
    if let Some(ref dn) = body.display_name {
        validation::check_length("display_name", dn, 1, 255)?;
    }

    // Password handling based on user type
    let hash = if user_type.requires_password() {
        let pw = body
            .password
            .as_deref()
            .ok_or_else(|| ApiError::BadRequest("password is required for human users".into()))?;
        validation::check_length("password", pw, 8, 1024)?;
        password::hash_password(pw).map_err(ApiError::Internal)?
    } else {
        if body.password.is_some() {
            return Err(ApiError::BadRequest(format!(
                "password must not be provided for {user_type} users"
            )));
        }
        "!disabled".to_owned()
    };

    let user = sqlx::query!(
        r#"
        INSERT INTO users (name, display_name, email, password_hash, user_type)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id, name, display_name, email, user_type, is_active, created_at, updated_at
        "#,
        body.name,
        body.display_name,
        body.email,
        hash,
        user_type.as_str(),
    )
    .fetch_one(&state.pool)
    .await?;

    // Create personal workspace for human users
    if user_type == UserType::Human {
        let display = user.display_name.as_deref().unwrap_or(&user.name);
        let _ = crate::workspace::service::get_or_create_default_workspace(
            &state.pool,
            user.id,
            &user.name,
            display,
        )
        .await;
    }

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "user.create".into(),
            resource: "user".into(),
            resource_id: Some(user.id),
            project_id: None,
            detail: Some(serde_json::json!({"name": body.name, "user_type": user_type.as_str()})),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok((
        StatusCode::CREATED,
        Json(UserResponse {
            id: user.id,
            name: user.name,
            display_name: user.display_name,
            email: user.email,
            user_type: parse_user_type(&user.user_type)?,
            is_active: user.is_active,
            created_at: user.created_at,
            updated_at: user.updated_at,
        }),
    ))
}

async fn list_users(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(params): Query<ListParams>,
) -> Result<Json<ListResponse<UserResponse>>, ApiError> {
    require_admin(&state, &auth).await?;

    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);

    let total = sqlx::query_scalar!("SELECT COUNT(*) as \"count!: i64\" FROM users")
        .fetch_one(&state.pool)
        .await?;

    let users = sqlx::query!(
        r#"
        SELECT id, name, display_name, email, user_type, is_active, created_at, updated_at
        FROM users ORDER BY created_at DESC LIMIT $1 OFFSET $2
        "#,
        limit,
        offset,
    )
    .fetch_all(&state.pool)
    .await?;

    let mut items = Vec::with_capacity(users.len());
    for u in users {
        items.push(UserResponse {
            id: u.id,
            name: u.name,
            display_name: u.display_name,
            email: u.email,
            user_type: parse_user_type(&u.user_type)?,
            is_active: u.is_active,
            created_at: u.created_at,
            updated_at: u.updated_at,
        });
    }

    Ok(Json(ListResponse { items, total }))
}

async fn get_user(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<UserResponse>, ApiError> {
    if auth.user_id != id {
        require_admin(&state, &auth).await?;
    }

    let user = sqlx::query!(
        r#"
        SELECT id, name, display_name, email, user_type, is_active, created_at, updated_at
        FROM users WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("user".into()))?;

    Ok(Json(UserResponse {
        id: user.id,
        name: user.name,
        display_name: user.display_name,
        email: user.email,
        user_type: parse_user_type(&user.user_type)?,
        is_active: user.is_active,
        created_at: user.created_at,
        updated_at: user.updated_at,
    }))
}

#[tracing::instrument(skip(state, body), fields(%id), err)]
async fn update_user(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateUserRequest>,
) -> Result<Json<UserResponse>, ApiError> {
    if auth.user_id != id {
        require_admin(&state, &auth).await?;
    }

    // Rate limit password changes: 5 per hour per user
    if body.password.is_some() {
        crate::auth::rate_limit::check_rate(
            &state.valkey,
            "password_change",
            &auth.user_id.to_string(),
            5,
            3600,
        )
        .await?;
    }

    // Validate inputs
    if let Some(ref dn) = body.display_name {
        validation::check_length("display_name", dn, 1, 255)?;
    }
    if let Some(ref email) = body.email {
        validation::check_email(email)?;
    }
    if let Some(ref pw) = body.password {
        validation::check_length("password", pw, 8, 1024)?;
        // Non-admin users changing their own password must verify current password
        if auth.user_id == id {
            let cp = body
                .current_password
                .as_deref()
                .ok_or_else(|| ApiError::BadRequest("current_password is required".into()))?;
            let current_hash: String =
                sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1")
                    .bind(id)
                    .fetch_one(&state.pool)
                    .await?;
            if !password::verify_password(cp, &current_hash) {
                return Err(ApiError::BadRequest("current password is incorrect".into()));
            }
        }
    }

    let password_hash = match &body.password {
        Some(pw) => Some(password::hash_password(pw).map_err(ApiError::Internal)?),
        None => None,
    };

    let user = sqlx::query!(
        r#"
        UPDATE users SET
            display_name = COALESCE($2, display_name),
            email = COALESCE($3, email),
            password_hash = COALESCE($4, password_hash)
        WHERE id = $1
        RETURNING id, name, display_name, email, user_type, is_active, created_at, updated_at
        "#,
        id,
        body.display_name,
        body.email,
        password_hash,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("user".into()))?;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "user.update".into(),
            resource: "user".into(),
            resource_id: Some(id),
            project_id: None,
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(Json(UserResponse {
        id: user.id,
        name: user.name,
        display_name: user.display_name,
        email: user.email,
        user_type: parse_user_type(&user.user_type)?,
        is_active: user.is_active,
        created_at: user.created_at,
        updated_at: user.updated_at,
    }))
}

#[tracing::instrument(skip(state), fields(%id), err)]
async fn deactivate_user(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    require_admin(&state, &auth).await?;

    let result = sqlx::query!("UPDATE users SET is_active = false WHERE id = $1", id,)
        .execute(&state.pool)
        .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::NotFound("user".into()));
    }

    // Revoke all sessions and tokens for the deactivated user
    sqlx::query!("DELETE FROM auth_sessions WHERE user_id = $1", id)
        .execute(&state.pool)
        .await?;
    sqlx::query("DELETE FROM api_tokens WHERE user_id = $1")
        .bind(id)
        .execute(&state.pool)
        .await?;

    // Invalidate permission cache (best-effort)
    let _ = crate::rbac::resolver::invalidate_permissions(&state.valkey, id, None).await;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "user.deactivate".into(),
            resource: "user".into(),
            resource_id: Some(id),
            project_id: None,
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// API Token handlers
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state, body), err)]
async fn create_api_token(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<CreateTokenRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // Rate limit: 20 token creations per hour per user
    crate::auth::rate_limit::check_rate(
        &state.valkey,
        "token_create",
        &auth.user_id.to_string(),
        20,
        3600,
    )
    .await?;

    validation::check_length("name", &body.name, 1, 255)?;

    let (raw_token, token_hash) = token::generate_api_token();

    let scopes = body.scopes.unwrap_or_default();

    // Validate that requested scopes are real permissions and subset of user's
    if !scopes.is_empty() && !scopes.contains(&"*".to_string()) {
        validate_token_scopes(&state, &auth, &scopes, body.project_id).await?;
    }

    const DEFAULT_TOKEN_EXPIRY_DAYS: i64 = 90;

    // S71: Max expiry configurable via PLATFORM_TOKEN_MAX_EXPIRY_DAYS (default 365)
    let max_days = i64::from(state.config.token_max_expiry_days);
    let days = body.expires_in_days.unwrap_or(DEFAULT_TOKEN_EXPIRY_DAYS);
    if !(1..=max_days).contains(&days) {
        return Err(ApiError::BadRequest(format!(
            "expires_in_days must be between 1 and {max_days}"
        )));
    }
    let expires_at = Some(Utc::now() + Duration::days(days));

    let row = sqlx::query!(
        r#"
        INSERT INTO api_tokens (user_id, name, token_hash, scopes, project_id, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING id, created_at
        "#,
        auth.user_id,
        body.name,
        token_hash,
        &scopes,
        body.project_id,
        expires_at,
    )
    .fetch_one(&state.pool)
    .await?;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "token.create".into(),
            resource: "api_token".into(),
            resource_id: Some(row.id),
            project_id: body.project_id,
            detail: Some(serde_json::json!({"name": body.name})),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok((
        StatusCode::CREATED,
        Json(CreateTokenResponse {
            token: raw_token,
            info: TokenResponse {
                id: row.id,
                name: body.name,
                scopes,
                project_id: body.project_id,
                last_used_at: None,
                expires_at,
                created_at: row.created_at,
            },
        }),
    ))
}

/// Validate that each scope string is a known permission and the user actually
/// holds that permission. Prevents scope escalation.
async fn validate_token_scopes(
    state: &AppState,
    auth: &AuthUser,
    scopes: &[String],
    project_id: Option<Uuid>,
) -> Result<(), ApiError> {
    let user_perms = crate::rbac::resolver::effective_permissions(
        &state.pool,
        &state.valkey,
        auth.user_id,
        project_id,
    )
    .await
    .map_err(ApiError::Internal)?;

    let user_perm_strings: std::collections::HashSet<&str> =
        user_perms.iter().map(|p| p.as_str()).collect();

    for scope in scopes {
        if scope == "*" {
            continue;
        }
        // Validate it's a known permission
        if scope.parse::<Permission>().is_err() {
            return Err(ApiError::BadRequest(format!("unknown scope '{scope}'")));
        }
        // Validate user has this permission
        if !user_perm_strings.contains(scope.as_str()) {
            return Err(ApiError::BadRequest(format!(
                "scope '{scope}' exceeds your permissions"
            )));
        }
    }
    Ok(())
}

async fn list_api_tokens(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<ListResponse<TokenResponse>>, ApiError> {
    let tokens = sqlx::query!(
        r#"
        SELECT id, name, scopes, project_id, last_used_at, expires_at, created_at
        FROM api_tokens WHERE user_id = $1
        ORDER BY created_at DESC
        "#,
        auth.user_id,
    )
    .fetch_all(&state.pool)
    .await?;

    let items: Vec<TokenResponse> = tokens
        .into_iter()
        .map(|t| TokenResponse {
            id: t.id,
            name: t.name,
            scopes: t.scopes,
            project_id: t.project_id,
            last_used_at: t.last_used_at,
            expires_at: t.expires_at,
            created_at: t.created_at,
        })
        .collect();

    let total = i64::try_from(items.len()).unwrap_or(0);
    Ok(Json(ListResponse { items, total }))
}

async fn get_api_token(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<TokenResponse>, ApiError> {
    let row = sqlx::query(
        "SELECT id, name, scopes, project_id, last_used_at, expires_at, created_at \
         FROM api_tokens WHERE id = $1 AND user_id = $2",
    )
    .bind(id)
    .bind(auth.user_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("token".into()))?;

    Ok(Json(TokenResponse {
        id: row.get("id"),
        name: row.get("name"),
        scopes: row.get("scopes"),
        project_id: row.get("project_id"),
        last_used_at: row.get("last_used_at"),
        expires_at: row.get("expires_at"),
        created_at: row.get("created_at"),
    }))
}

#[tracing::instrument(skip(state), fields(%id), err)]
async fn revoke_api_token(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let result = sqlx::query!(
        "DELETE FROM api_tokens WHERE id = $1 AND user_id = $2",
        id,
        auth.user_id,
    )
    .execute(&state.pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::NotFound("token".into()));
    }

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "token.revoke".into(),
            resource: "api_token".into(),
            resource_id: Some(id),
            project_id: None,
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(StatusCode::NO_CONTENT)
}
