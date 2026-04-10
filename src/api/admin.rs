// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use ts_rs::TS;

use crate::api::helpers::{ListResponse, require_admin};
use crate::api::users::{CreateTokenResponse, ListParams, TokenResponse, UserResponse};
use crate::audit::{AuditEntry, send_audit};
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

#[derive(Debug, Serialize, TS)]
#[ts(export, rename = "Role")]
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

#[derive(Debug, Deserialize)]
pub struct UpdateRoleRequest {
    pub name: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Serialize, TS)]
#[ts(export, rename = "Permission")]
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

#[derive(Debug, Serialize, TS)]
#[ts(export)]
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
            "/api/admin/roles/{id}",
            get(get_role).patch(update_role).delete(delete_role),
        )
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
            get(get_delegation).delete(revoke_delegation_handler),
        )
        // Service accounts
        .route(
            "/api/admin/service-accounts",
            post(create_service_account).get(list_service_accounts),
        )
        .route(
            "/api/admin/service-accounts/{id}",
            get(get_service_account).delete(deactivate_service_account),
        )
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
        .map_err(|_| ApiError::BadRequest("invalid user_type".into()))
}

// ---------------------------------------------------------------------------
// Role handlers
// ---------------------------------------------------------------------------

async fn list_roles(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<ListResponse<RoleResponse>>, ApiError> {
    require_admin(&state, &auth).await?;

    let roles = sqlx::query_as!(
        RoleResponse,
        "SELECT id, name, description, is_system, created_at FROM roles ORDER BY name LIMIT 200"
    )
    .fetch_all(&state.pool)
    .await?;

    let total = i64::try_from(roles.len()).unwrap_or(0);
    Ok(Json(ListResponse {
        items: roles,
        total,
    }))
}

#[tracing::instrument(skip(state, body), fields(role_name = %body.name), err)]
async fn create_role(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<CreateRoleRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &auth).await?;

    validation::check_name(&body.name)?;
    if let Some(ref desc) = body.description {
        validation::check_length("description", desc, 0, 10_000)?;
    }

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

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "role.create".into(),
            resource: "role".into(),
            resource_id: Some(role.id),
            project_id: None,
            detail: Some(serde_json::json!({"name": body.name})),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok((StatusCode::CREATED, Json(role)))
}

async fn get_role(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<RoleResponse>, ApiError> {
    require_admin(&state, &auth).await?;

    let row =
        sqlx::query("SELECT id, name, description, is_system, created_at FROM roles WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.pool)
            .await?
            .ok_or_else(|| ApiError::NotFound("role".into()))?;

    Ok(Json(RoleResponse {
        id: row.get("id"),
        name: row.get("name"),
        description: row.get("description"),
        is_system: row.get("is_system"),
        created_at: row.get("created_at"),
    }))
}

#[tracing::instrument(skip(state, body), fields(%id), err)]
async fn update_role(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateRoleRequest>,
) -> Result<Json<RoleResponse>, ApiError> {
    require_admin(&state, &auth).await?;

    if let Some(ref name) = body.name {
        validation::check_name(name)?;
    }
    if let Some(ref desc) = body.description {
        validation::check_length("description", desc, 0, 10_000)?;
    }

    // Check if role exists and is not a system role
    let existing = sqlx::query("SELECT is_system FROM roles WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| ApiError::NotFound("role".into()))?;

    let is_system: bool = existing.get("is_system");
    if is_system {
        return Err(ApiError::Conflict("cannot modify system role".into()));
    }

    let row = sqlx::query(
        "UPDATE roles SET \
             name = COALESCE($2, name), \
             description = COALESCE($3, description) \
         WHERE id = $1 \
         RETURNING id, name, description, is_system, created_at",
    )
    .bind(id)
    .bind(&body.name)
    .bind(&body.description)
    .fetch_one(&state.pool)
    .await?;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "role.update".into(),
            resource: "role".into(),
            resource_id: Some(id),
            project_id: None,
            detail: Some(serde_json::json!({"name": body.name, "description": body.description})),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(Json(RoleResponse {
        id: row.get("id"),
        name: row.get("name"),
        description: row.get("description"),
        is_system: row.get("is_system"),
        created_at: row.get("created_at"),
    }))
}

#[tracing::instrument(skip(state), fields(%id), err)]
async fn delete_role(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    require_admin(&state, &auth).await?;

    let existing = sqlx::query("SELECT is_system FROM roles WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| ApiError::NotFound("role".into()))?;

    let is_system: bool = existing.get("is_system");
    if is_system {
        return Err(ApiError::Conflict("cannot delete system role".into()));
    }

    // Check if any users are assigned this role
    let assigned_count: i64 =
        sqlx::query("SELECT COUNT(*) as count FROM user_roles WHERE role_id = $1")
            .bind(id)
            .fetch_one(&state.pool)
            .await?
            .get("count");

    if assigned_count > 0 {
        return Err(ApiError::Conflict(
            "cannot delete role that is assigned to users".into(),
        ));
    }

    sqlx::query("DELETE FROM roles WHERE id = $1")
        .bind(id)
        .execute(&state.pool)
        .await?;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "role.delete".into(),
            resource: "role".into(),
            resource_id: Some(id),
            project_id: None,
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(StatusCode::NO_CONTENT)
}

async fn list_role_permissions(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<ListResponse<PermissionResponse>>, ApiError> {
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

    let total = i64::try_from(perms.len()).unwrap_or(0);
    Ok(Json(ListResponse {
        items: perms,
        total,
    }))
}

#[tracing::instrument(skip(state, body), fields(%id), err)]
async fn set_role_permissions(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<SetPermissionsRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_admin(&state, &auth).await?;

    if body.permissions.len() > 100 {
        return Err(ApiError::BadRequest("too many permissions".into()));
    }
    for perm_name in &body.permissions {
        validation::check_length("permission", perm_name, 1, 255)?;
    }

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

    // A26: invalidate permission cache for all users with this role
    let affected_users: Vec<Uuid> =
        sqlx::query_scalar("SELECT user_id FROM user_roles WHERE role_id = $1")
            .bind(id)
            .fetch_all(&state.pool)
            .await
            .unwrap_or_default();
    for uid in affected_users {
        let _ = resolver::invalidate_permissions(&state.valkey, uid, None).await;
    }

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "role.update_permissions".into(),
            resource: "role".into(),
            resource_id: Some(id),
            project_id: None,
            detail: Some(serde_json::json!({"permissions": body.permissions})),
            ip_addr: auth.ip_addr.clone(),
        },
    );

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

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "role.assign".into(),
            resource: "user_role".into(),
            resource_id: Some(id),
            project_id: body.project_id,
            detail: Some(serde_json::json!({"user_id": user_id, "role_id": body.role_id})),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok((StatusCode::CREATED, Json(serde_json::json!({"ok": true}))))
}

#[tracing::instrument(skip(state), fields(%user_id, %role_id), err)]
async fn remove_role(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((user_id, role_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
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

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "role.remove".into(),
            resource: "user_role".into(),
            resource_id: None,
            project_id: result.project_id,
            detail: Some(serde_json::json!({"user_id": user_id, "role_id": role_id})),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(StatusCode::NO_CONTENT)
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

    if let Some(ref reason) = body.reason {
        validation::check_length("reason", reason, 0, 10_000)?;
    }

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

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "delegation.create".into(),
            resource: "delegation".into(),
            resource_id: Some(d.id),
            project_id: body.project_id,
            detail: Some(serde_json::json!({
                "delegate_id": body.delegate_id,
                "permission": body.permission,
            })),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok((StatusCode::CREATED, Json(d)))
}

async fn get_delegation(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_delegate(&state, &auth).await?;

    let row = sqlx::query(
        "SELECT d.id, d.delegator_id, d.delegate_id, d.permission_id, \
                p.name as permission_name, d.project_id, \
                d.expires_at, d.reason, d.revoked_at, d.created_at \
         FROM delegations d \
         JOIN permissions p ON p.id = d.permission_id \
         WHERE d.id = $1",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("delegation".into()))?;

    let delegation_id: Uuid = row.get("id");
    let delegator_id: Uuid = row.get("delegator_id");
    let delegate_id: Uuid = row.get("delegate_id");
    let permission_id: Uuid = row.get("permission_id");
    let permission_name: String = row.get("permission_name");
    let project_id: Option<Uuid> = row.get("project_id");
    let expires_at: Option<DateTime<Utc>> = row.get("expires_at");
    let reason: Option<String> = row.get("reason");
    let revoked_at: Option<DateTime<Utc>> = row.get("revoked_at");
    let created_at: DateTime<Utc> = row.get("created_at");

    Ok(Json(serde_json::json!({
        "id": delegation_id,
        "delegator_id": delegator_id,
        "delegate_id": delegate_id,
        "permission_id": permission_id,
        "permission": permission_name,
        "project_id": project_id,
        "expires_at": expires_at,
        "reason": reason,
        "revoked_at": revoked_at,
        "created_at": created_at,
    })))
}

#[tracing::instrument(skip(state), fields(%id), err)]
async fn revoke_delegation_handler(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    require_delegate(&state, &auth).await?;

    // S47: Only the delegator or a full admin can revoke a delegation
    let delegator_id: Option<Uuid> =
        sqlx::query_scalar("SELECT delegator_id FROM delegations WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.pool)
            .await
            .map_err(ApiError::from)?;
    let delegator_id = delegator_id.ok_or_else(|| ApiError::NotFound("delegation".into()))?;
    if delegator_id != auth.user_id {
        let is_admin = crate::rbac::resolver::has_permission_scoped(
            &state.pool,
            &state.valkey,
            auth.user_id,
            None,
            crate::rbac::Permission::AdminUsers,
            auth.token_scopes.as_deref(),
        )
        .await
        .map_err(ApiError::Internal)?;
        if !is_admin {
            return Err(ApiError::Forbidden);
        }
    }

    delegation::revoke_delegation(&state.pool, &state.valkey, id).await?;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "delegation.revoke".into(),
            resource: "delegation".into(),
            resource_id: Some(id),
            project_id: None,
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(StatusCode::NO_CONTENT)
}

async fn list_delegations(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(params): Query<DelegationQuery>,
) -> Result<Json<ListResponse<delegation::Delegation>>, ApiError> {
    require_delegate(&state, &auth).await?;

    let user_id = params.user_id.unwrap_or(auth.user_id);
    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);
    let delegations = delegation::list_delegations(&state.pool, user_id, limit, offset).await?;

    let total = i64::try_from(delegations.len()).unwrap_or(0);
    Ok(Json(ListResponse {
        items: delegations,
        total,
    }))
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

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "service_account.create".into(),
            resource: "user".into(),
            resource_id: Some(user.id),
            project_id: body.project_id,
            detail: Some(serde_json::json!({"name": body.name})),
            ip_addr: auth.ip_addr.clone(),
        },
    );

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

async fn get_service_account(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<UserResponse>, ApiError> {
    require_admin(&state, &auth).await?;

    let row = sqlx::query(
        "SELECT id, name, display_name, email, user_type, is_active, created_at, updated_at \
         FROM users WHERE id = $1 AND user_type = 'service_account'",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("service account".into()))?;

    Ok(Json(UserResponse {
        id: row.get("id"),
        name: row.get("name"),
        display_name: row.get("display_name"),
        email: row.get("email"),
        user_type: parse_user_type(row.get("user_type"))?,
        is_active: row.get("is_active"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }))
}

#[tracing::instrument(skip(state), fields(%id), err)]
async fn deactivate_service_account(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
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

    // A29: delete sessions before revoking tokens
    sqlx::query("DELETE FROM auth_sessions WHERE user_id = $1")
        .bind(id)
        .execute(&state.pool)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;

    // Revoke all tokens
    sqlx::query("DELETE FROM api_tokens WHERE user_id = $1")
        .bind(id)
        .execute(&state.pool)
        .await?;

    let _ = resolver::invalidate_permissions(&state.valkey, id, None).await;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "service_account.deactivate".into(),
            resource: "user".into(),
            resource_id: Some(id),
            project_id: None,
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(StatusCode::NO_CONTENT)
}
