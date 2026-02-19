use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::middleware::AuthUser;
use crate::error::ApiError;
use crate::rbac::{Permission, delegation, resolver};
use crate::store::AppState;

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
}

// ---------------------------------------------------------------------------
// Audit helper (shared with users.rs — could be extracted)
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

/// Check the caller has admin:users permission, return Forbidden otherwise.
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

    // Verify role exists and is not system
    let role = sqlx::query!("SELECT is_system FROM roles WHERE id = $1", id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| ApiError::NotFound("role".into()))?;

    if role.is_system {
        return Err(ApiError::BadRequest(
            "cannot modify system role permissions".into(),
        ));
    }

    // Replace all permissions for this role
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

    // Invalidate the target user's permission cache
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

    // Invalidate the target user's permission cache
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
    // Delegation requires admin:delegate permission
    let allowed = resolver::has_permission(
        &state.pool,
        &state.valkey,
        auth.user_id,
        None,
        Permission::AdminDelegate,
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::Forbidden);
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
    let allowed = resolver::has_permission(
        &state.pool,
        &state.valkey,
        auth.user_id,
        None,
        Permission::AdminDelegate,
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::Forbidden);
    }

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
    let allowed = resolver::has_permission(
        &state.pool,
        &state.valkey,
        auth.user_id,
        None,
        Permission::AdminDelegate,
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::Forbidden);
    }

    let user_id = params.user_id.unwrap_or(auth.user_id);
    let delegations = delegation::list_delegations(&state.pool, user_id).await?;

    Ok(Json(delegations))
}
