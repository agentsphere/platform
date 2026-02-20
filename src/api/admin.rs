use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::helpers::ListResponse;
use crate::api::users::{CreateTokenResponse, ListParams, TokenResponse, UserResponse};
use crate::audit::{AuditEntry, write_audit};
use crate::auth::middleware::AuthUser;
use crate::auth::token;
use crate::auth::user_type::UserType;
use crate::error::ApiError;
use crate::rbac::{Permission, delegation, resolver};
use crate::store::AppState;
use crate::validation;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct RoleResponse {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub is_system: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct CreateRoleRequest {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PermissionResponse {
    pub id: Uuid,
    pub name: String,
    pub resource: String,
    pub action: String,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SetPermissionsRequest {
    pub permissions: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct AssignRoleRequest {
    pub role_id: Uuid,
    pub project_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct CreateDelegationRequest {
    pub delegate_id: Uuid,
    pub permission: String,
    pub project_id: Option<Uuid>,
    pub expires_at: Option<DateTime<Utc>>,
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DelegationQuery {
    pub user_id: Option<Uuid>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct CreateServiceAccountRequest {
    pub name: String,
    pub email: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub scopes: Option<Vec<String>>,
    pub project_id: Option<Uuid>,
}

#[derive(Debug, Serialize)]
pub struct ServiceAccountResponse {
    pub user: UserResponse,
    pub token: Option<CreateTokenResponse>,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        // Roles
        .route("/api/admin/roles", get(list_roles).post(create_role))
        .route(
            "/api/admin/roles/{id}/permissions",
            get(list_role_permissions).put(set_role_permissions),
        )
        // User role assignments
        .route("/api/admin/users/{user_id}/roles", post(assign_role))
        .route(
            "/api/admin/users/{user_id}/roles/{role_id}",
            delete(remove_role),
        )
        // Delegations
        .route(
            "/api/admin/delegations",
            get(list_delegations).post(create_delegation_handler),
        )
        .route(
            "/api/admin/delegations/{id}",
            delete(revoke_delegation_handler),
        )
        // Service accounts
        .route(
            "/api/admin/service-accounts",
            post(create_service_account).get(list_service_accounts),
        )
        .route(
            "/api/admin/service-accounts/{id}",
            delete(deactivate_service_account),
        )
}

/// Check the caller has admin:users permission (scope-aware), return Forbidden otherwise.
async fn require_admin(state: &AppState, auth: &AuthUser) -> Result<(), ApiError> {
    let allowed = resolver::has_permission_scoped(
        &state.pool,
        &state.valkey,
        auth.user_id,
        None,
        Permission::AdminUsers,
        auth.token_scopes.as_deref(),
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

/// Check admin:delegate permission (scope-aware).
async fn require_delegate(state: &AppState, auth: &AuthUser) -> Result<(), ApiError> {
    let allowed = resolver::has_permission_scoped(
        &state.pool,
        &state.valkey,
        auth.user_id,
        None,
        Permission::AdminDelegate,
        auth.token_scopes.as_deref(),
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

fn parse_user_type(s: &str) -> Result<UserType, ApiError> {
    s.parse::<UserType>()
        .map_err(|e: anyhow::Error| ApiError::Internal(e))
}

// ---------------------------------------------------------------------------
// Role handlers
// ---------------------------------------------------------------------------

async fn list_roles(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<Vec<RoleResponse>>, ApiError> {
    require_admin(&state, &auth).await?;

    let roles = sqlx::query_as!(
        RoleResponse,
        "SELECT id, name, description, is_system, created_at FROM roles ORDER BY name"
    )
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(roles))
}

#[tracing::instrument(skip(state, body), fields(role_name = %body.name), err)]
async fn create_role(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<CreateRoleRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &auth).await?;

    let role = sqlx::query_as!(
        RoleResponse,
        r#"
        INSERT INTO roles (name, description, is_system)
        VALUES ($1, $2, false)
        RETURNING id, name, description, is_system, created_at
        "#,
        body.name,
        body.description,
    )
    .fetch_one(&state.pool)
    .await?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "role.create",
            resource: "role",
            resource_id: Some(role.id),
            project_id: None,
            detail: Some(serde_json::json!({"name": body.name})),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok((StatusCode::CREATED, Json(role)))
}

async fn list_role_permissions(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<PermissionResponse>>, ApiError> {
    require_admin(&state, &auth).await?;

    let perms = sqlx::query_as!(
        PermissionResponse,
        r#"
        SELECT p.id, p.name, p.resource, p.action, p.description
        FROM permissions p
        JOIN role_permissions rp ON rp.permission_id = p.id
        WHERE rp.role_id = $1
        ORDER BY p.name
        "#,
        id,
    )
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(perms))
}

#[tracing::instrument(skip(state, body), fields(%id), err)]
async fn set_role_permissions(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<SetPermissionsRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_admin(&state, &auth).await?;

    let role = sqlx::query!("SELECT is_system FROM roles WHERE id = $1", id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| ApiError::NotFound("role".into()))?;

    if role.is_system {
        return Err(ApiError::BadRequest(
            "cannot modify system role permissions".into(),
        ));
    }

    let mut tx = state.pool.begin().await?;

    sqlx::query!("DELETE FROM role_permissions WHERE role_id = $1", id)
        .execute(&mut *tx)
        .await?;

    for perm_name in &body.permissions {
        sqlx::query!(
            r#"
            INSERT INTO role_permissions (role_id, permission_id)
            SELECT $1, p.id FROM permissions p WHERE p.name = $2
            "#,
            id,
            perm_name,
        )
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "role.update_permissions",
            resource: "role",
            resource_id: Some(id),
            project_id: None,
            detail: Some(serde_json::json!({"permissions": body.permissions})),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(Json(serde_json::json!({"ok": true})))
}

// ---------------------------------------------------------------------------
// User role assignment handlers
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state, body), fields(%user_id), err)]
async fn assign_role(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(user_id): Path<Uuid>,
    Json(body): Json<AssignRoleRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &auth).await?;

    let id = Uuid::new_v4();

    sqlx::query!(
        r#"
        INSERT INTO user_roles (id, user_id, role_id, project_id, granted_by)
        VALUES ($1, $2, $3, $4, $5)
        "#,
        id,
        user_id,
        body.role_id,
        body.project_id,
        auth.user_id,
    )
    .execute(&state.pool)
    .await?;

    let _ = resolver::invalidate_permissions(&state.valkey, user_id, body.project_id).await;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "role.assign",
            resource: "user_role",
            resource_id: Some(id),
            project_id: body.project_id,
            detail: Some(serde_json::json!({"user_id": user_id, "role_id": body.role_id})),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok((StatusCode::CREATED, Json(serde_json::json!({"ok": true}))))
}

#[tracing::instrument(skip(state), fields(%user_id, %role_id), err)]
async fn remove_role(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((user_id, role_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_admin(&state, &auth).await?;

    let result = sqlx::query!(
        "DELETE FROM user_roles WHERE user_id = $1 AND role_id = $2 RETURNING project_id",
        user_id,
        role_id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("user_role".into()))?;

    let _ = resolver::invalidate_permissions(&state.valkey, user_id, result.project_id).await;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "role.remove",
            resource: "user_role",
            resource_id: None,
            project_id: result.project_id,
            detail: Some(serde_json::json!({"user_id": user_id, "role_id": role_id})),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(Json(serde_json::json!({"ok": true})))
}

// ---------------------------------------------------------------------------
// Delegation handlers
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state, body), err)]
async fn create_delegation_handler(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<CreateDelegationRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_delegate(&state, &auth).await?;

    let perm: Permission = body
        .permission
        .parse()
        .map_err(|e: anyhow::Error| ApiError::BadRequest(e.to_string()))?;

    let params = delegation::CreateDelegationParams {
        delegator_id: auth.user_id,
        delegate_id: body.delegate_id,
        permission: perm,
        project_id: body.project_id,
        expires_at: body.expires_at,
        reason: body.reason.clone(),
    };

    let d = delegation::create_delegation(&state.pool, &state.valkey, &params).await?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "delegation.create",
            resource: "delegation",
            resource_id: Some(d.id),
            project_id: body.project_id,
            detail: Some(serde_json::json!({
                "delegate_id": body.delegate_id,
                "permission": body.permission,
            })),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok((StatusCode::CREATED, Json(d)))
}

#[tracing::instrument(skip(state), fields(%id), err)]
async fn revoke_delegation_handler(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_delegate(&state, &auth).await?;

    delegation::revoke_delegation(&state.pool, &state.valkey, id).await?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "delegation.revoke",
            resource: "delegation",
            resource_id: Some(id),
            project_id: None,
            detail: None,
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(Json(serde_json::json!({"ok": true})))
}

async fn list_delegations(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(params): Query<DelegationQuery>,
) -> Result<Json<Vec<delegation::Delegation>>, ApiError> {
    require_delegate(&state, &auth).await?;

    let user_id = params.user_id.unwrap_or(auth.user_id);
    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);
    let delegations = delegation::list_delegations(&state.pool, user_id, limit, offset).await?;

    Ok(Json(delegations))
}

// ---------------------------------------------------------------------------
// Service account handlers
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state, body), fields(sa_name = %body.name), err)]
async fn create_service_account(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<CreateServiceAccountRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &auth).await?;

    validation::check_name(&body.name)?;
    validation::check_email(&body.email)?;
    if let Some(ref dn) = body.display_name {
        validation::check_length("display_name", dn, 1, 255)?;
    }
    if let Some(ref desc) = body.description {
        validation::check_length("description", desc, 0, 10_000)?;
    }

    let metadata = body
        .description
        .as_ref()
        .map(|d| serde_json::json!({"description": d}));

    let user = sqlx::query!(
        r#"
        INSERT INTO users (name, display_name, email, password_hash, user_type, metadata)
        VALUES ($1, $2, $3, '!disabled', 'service_account', $4)
        RETURNING id, name, display_name, email, user_type, is_active, created_at, updated_at
        "#,
        body.name,
        body.display_name,
        body.email,
        metadata,
    )
    .fetch_one(&state.pool)
    .await?;

    // Optionally create an API token for the service account
    let token_response = if body.scopes.is_some() || body.project_id.is_some() {
        let scopes = body.scopes.unwrap_or_default();
        let (raw_token, token_hash) = token::generate_api_token();
        let expires_at = Some(Utc::now() + Duration::days(365));

        let tok = sqlx::query!(
            r#"
            INSERT INTO api_tokens (user_id, name, token_hash, scopes, project_id, expires_at)
            VALUES ($1, $2, $3, $4, $5, $6)
            RETURNING id, created_at
            "#,
            user.id,
            format!("{}-default", body.name),
            token_hash,
            &scopes,
            body.project_id,
            expires_at,
        )
        .fetch_one(&state.pool)
        .await?;

        Some(CreateTokenResponse {
            token: raw_token,
            info: TokenResponse {
                id: tok.id,
                name: format!("{}-default", body.name),
                scopes,
                project_id: body.project_id,
                last_used_at: None,
                expires_at,
                created_at: tok.created_at,
            },
        })
    } else {
        None
    };

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "service_account.create",
            resource: "user",
            resource_id: Some(user.id),
            project_id: body.project_id,
            detail: Some(serde_json::json!({"name": body.name})),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok((
        StatusCode::CREATED,
        Json(ServiceAccountResponse {
            user: UserResponse {
                id: user.id,
                name: user.name,
                display_name: user.display_name,
                email: user.email,
                user_type: parse_user_type(&user.user_type)?,
                is_active: user.is_active,
                created_at: user.created_at,
                updated_at: user.updated_at,
            },
            token: token_response,
        }),
    ))
}

async fn list_service_accounts(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(params): Query<ListParams>,
) -> Result<Json<ListResponse<UserResponse>>, ApiError> {
    require_admin(&state, &auth).await?;

    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);

    let total = sqlx::query_scalar!(
        "SELECT COUNT(*) as \"count!: i64\" FROM users WHERE user_type = 'service_account'"
    )
    .fetch_one(&state.pool)
    .await?;

    let users = sqlx::query!(
        r#"
        SELECT id, name, display_name, email, user_type, is_active, created_at, updated_at
        FROM users
        WHERE user_type = 'service_account'
        ORDER BY created_at DESC LIMIT $1 OFFSET $2
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

#[tracing::instrument(skip(state), fields(%id), err)]
async fn deactivate_service_account(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_admin(&state, &auth).await?;

    // Verify target is actually a service account
    let user = sqlx::query!("SELECT user_type FROM users WHERE id = $1", id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| ApiError::NotFound("service account".into()))?;

    if user.user_type != "service_account" {
        return Err(ApiError::BadRequest("user is not a service account".into()));
    }

    sqlx::query!("UPDATE users SET is_active = false WHERE id = $1", id)
        .execute(&state.pool)
        .await?;

    // Revoke all tokens
    sqlx::query("DELETE FROM api_tokens WHERE user_id = $1")
        .bind(id)
        .execute(&state.pool)
        .await?;

    let _ = resolver::invalidate_permissions(&state.valkey, id, None).await;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "service_account.deactivate",
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
