use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::middleware::AuthUser;
use crate::auth::{password, token};
use crate::error::ApiError;
use crate::rbac::Permission;
use crate::store::AppState;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateUserRequest {
    pub name: String,
    pub email: String,
    pub password: String,
    pub display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateUserRequest {
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub password: Option<String>,
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

#[derive(Debug, Serialize)]
pub struct UserResponse {
    pub id: Uuid,
    pub name: String,
    pub display_name: Option<String>,
    pub email: String,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct ListResponse<T: Serialize> {
    pub items: Vec<T>,
    pub total: i64,
}

#[derive(Debug, Serialize)]
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

#[derive(Debug, Serialize)]
pub struct TokenResponse {
    pub id: Uuid,
    pub name: String,
    pub scopes: Vec<String>,
    pub project_id: Option<Uuid>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
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
        .route("/api/users", post(create_user))
        .route("/api/users/list", get(list_users))
        // User self-service + admin
        .route(
            "/api/users/{id}",
            get(get_user).patch(update_user).delete(deactivate_user),
        )
        // API token management (authenticated)
        .route("/api/tokens", post(create_api_token).get(list_api_tokens))
        .route("/api/tokens/{id}", delete(revoke_api_token))
}

// ---------------------------------------------------------------------------
// Audit helper
// ---------------------------------------------------------------------------

#[allow(dead_code)] // ip_addr stored for future ipnetwork support
struct AuditEntry<'a> {
    actor_id: Uuid,
    actor_name: &'a str,
    action: &'a str,
    resource: &'a str,
    resource_id: Option<Uuid>,
    project_id: Option<Uuid>,
    detail: Option<serde_json::Value>,
    ip_addr: Option<&'a str>,
}

async fn write_audit(pool: &PgPool, entry: &AuditEntry<'_>) {
    // Note: ip_addr is INET in postgres; we skip binding it to avoid needing the
    // ipnetwork crate. The column stays NULL. A future pass can add ipnetwork to Cargo.toml.
    let _ = sqlx::query!(
        r#"
        INSERT INTO audit_log (actor_id, actor_name, action, resource, resource_id, project_id, detail)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
        entry.actor_id,
        entry.actor_name,
        entry.action,
        entry.resource,
        entry.resource_id,
        entry.project_id,
        entry.detail,
    )
    .execute(pool)
    .await;
}

// ---------------------------------------------------------------------------
// Auth handlers
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state, body), fields(username = %body.name), err)]
async fn login(
    State(state): State<AppState>,
    Json(body): Json<LoginRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // Look up user by name
    let user = sqlx::query!(
        r#"
        SELECT id, name, display_name, email, password_hash, is_active,
               created_at, updated_at
        FROM users
        WHERE name = $1
        "#,
        body.name,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or(ApiError::Unauthorized)?;

    if !user.is_active {
        return Err(ApiError::Unauthorized);
    }

    // Verify password
    if !password::verify_password(&body.password, &user.password_hash)
        .map_err(ApiError::Internal)?
    {
        return Err(ApiError::Unauthorized);
    }

    // Create session
    let (raw_token, token_hash) = token::generate_session_token();
    let expires_at = Utc::now() + Duration::hours(24);

    sqlx::query!(
        r#"
        INSERT INTO auth_sessions (user_id, token_hash, expires_at)
        VALUES ($1, $2, $3)
        "#,
        user.id,
        token_hash,
        expires_at,
    )
    .execute(&state.pool)
    .await?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: user.id,
            actor_name: &user.name,
            action: "auth.login",
            resource: "session",
            resource_id: None,
            project_id: None,
            detail: None,
            ip_addr: None,
        },
    )
    .await;

    let response = LoginResponse {
        token: raw_token,
        expires_at,
        user: UserResponse {
            id: user.id,
            name: user.name,
            display_name: user.display_name,
            email: user.email,
            is_active: user.is_active,
            created_at: user.created_at,
            updated_at: user.updated_at,
        },
    };

    // Set session cookie + return JSON
    let cookie = format!(
        "session={}; Path=/; HttpOnly; SameSite=Strict; Max-Age=86400",
        response.token
    );
    Ok((
        StatusCode::OK,
        [(axum::http::header::SET_COOKIE, cookie)],
        Json(response),
    ))
}

#[tracing::instrument(skip(state), fields(user_id = %auth.user_id), err)]
async fn logout(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<impl IntoResponse, ApiError> {
    // Delete all sessions for this user (logout everywhere)
    // A more targeted approach would delete only the current session,
    // but we'd need to pass the token hash through. This is simpler.
    sqlx::query!("DELETE FROM auth_sessions WHERE user_id = $1", auth.user_id,)
        .execute(&state.pool)
        .await?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "auth.logout",
            resource: "session",
            resource_id: None,
            project_id: None,
            detail: None,
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    // Clear cookie
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
        SELECT id, name, display_name, email, is_active, created_at, updated_at
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
    // Check admin permission
    let is_admin = crate::rbac::resolver::has_permission(
        &state.pool,
        &state.valkey,
        auth.user_id,
        None,
        Permission::AdminUsers,
    )
    .await
    .map_err(ApiError::Internal)?;

    if !is_admin {
        return Err(ApiError::Forbidden);
    }

    let hash = password::hash_password(&body.password).map_err(ApiError::Internal)?;

    let user = sqlx::query!(
        r#"
        INSERT INTO users (name, display_name, email, password_hash)
        VALUES ($1, $2, $3, $4)
        RETURNING id, name, display_name, email, is_active, created_at, updated_at
        "#,
        body.name,
        body.display_name,
        body.email,
        hash,
    )
    .fetch_one(&state.pool)
    .await?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "user.create",
            resource: "user",
            resource_id: Some(user.id),
            project_id: None,
            detail: Some(serde_json::json!({"name": body.name})),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok((
        StatusCode::CREATED,
        Json(UserResponse {
            id: user.id,
            name: user.name,
            display_name: user.display_name,
            email: user.email,
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
    let is_admin = crate::rbac::resolver::has_permission(
        &state.pool,
        &state.valkey,
        auth.user_id,
        None,
        Permission::AdminUsers,
    )
    .await
    .map_err(ApiError::Internal)?;

    if !is_admin {
        return Err(ApiError::Forbidden);
    }

    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);

    let total = sqlx::query_scalar!("SELECT COUNT(*) as \"count!: i64\" FROM users")
        .fetch_one(&state.pool)
        .await?;

    let users = sqlx::query!(
        r#"
        SELECT id, name, display_name, email, is_active, created_at, updated_at
        FROM users ORDER BY created_at DESC LIMIT $1 OFFSET $2
        "#,
        limit,
        offset,
    )
    .fetch_all(&state.pool)
    .await?;

    let items = users
        .into_iter()
        .map(|u| UserResponse {
            id: u.id,
            name: u.name,
            display_name: u.display_name,
            email: u.email,
            is_active: u.is_active,
            created_at: u.created_at,
            updated_at: u.updated_at,
        })
        .collect();

    Ok(Json(ListResponse { items, total }))
}

async fn get_user(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<UserResponse>, ApiError> {
    // Self or admin
    if auth.user_id != id {
        let is_admin = crate::rbac::resolver::has_permission(
            &state.pool,
            &state.valkey,
            auth.user_id,
            None,
            Permission::AdminUsers,
        )
        .await
        .map_err(ApiError::Internal)?;

        if !is_admin {
            return Err(ApiError::Forbidden);
        }
    }

    let user = sqlx::query!(
        r#"
        SELECT id, name, display_name, email, is_active, created_at, updated_at
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
    // Self or admin
    if auth.user_id != id {
        let is_admin = crate::rbac::resolver::has_permission(
            &state.pool,
            &state.valkey,
            auth.user_id,
            None,
            Permission::AdminUsers,
        )
        .await
        .map_err(ApiError::Internal)?;

        if !is_admin {
            return Err(ApiError::Forbidden);
        }
    }

    // Build update — only set fields that are provided
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
        RETURNING id, name, display_name, email, is_active, created_at, updated_at
        "#,
        id,
        body.display_name,
        body.email,
        password_hash,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("user".into()))?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "user.update",
            resource: "user",
            resource_id: Some(id),
            project_id: None,
            detail: None,
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(Json(UserResponse {
        id: user.id,
        name: user.name,
        display_name: user.display_name,
        email: user.email,
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
) -> Result<Json<serde_json::Value>, ApiError> {
    let is_admin = crate::rbac::resolver::has_permission(
        &state.pool,
        &state.valkey,
        auth.user_id,
        None,
        Permission::AdminUsers,
    )
    .await
    .map_err(ApiError::Internal)?;

    if !is_admin {
        return Err(ApiError::Forbidden);
    }

    sqlx::query!("UPDATE users SET is_active = false WHERE id = $1", id,)
        .execute(&state.pool)
        .await?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "user.deactivate",
            resource: "user",
            resource_id: Some(id),
            project_id: None,
            detail: None,
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(Json(serde_json::json!({"ok": true})))
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
    let (raw_token, token_hash) = token::generate_api_token();

    let scopes = body.scopes.unwrap_or_default();
    let expires_at = body
        .expires_in_days
        .map(|days| Utc::now() + Duration::days(days));

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

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "token.create",
            resource: "api_token",
            resource_id: Some(row.id),
            project_id: body.project_id,
            detail: Some(serde_json::json!({"name": body.name})),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

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

async fn list_api_tokens(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<Vec<TokenResponse>>, ApiError> {
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

    let items = tokens
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

    Ok(Json(items))
}

#[tracing::instrument(skip(state), fields(%id), err)]
async fn revoke_api_token(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Only allow revoking own tokens
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

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "token.revoke",
            resource: "api_token",
            resource_id: Some(id),
            project_id: None,
            detail: None,
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(Json(serde_json::json!({"ok": true})))
}
