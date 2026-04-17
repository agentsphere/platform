// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

use std::collections::HashSet;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use platform_types::valkey;
use platform_types::{ApiError, Permission, PermissionChecker, PermissionResolver};

static CACHE_TTL: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// Set the permission cache TTL (seconds). Call once at startup.
pub fn set_cache_ttl(ttl: u64) {
    CACHE_TTL.set(ttl).ok();
}

#[allow(clippy::cast_possible_wrap)]
fn cache_ttl() -> i64 {
    *CACHE_TTL.get().unwrap_or(&300) as i64
}

fn cache_key(user_id: Uuid, project_id: Option<Uuid>) -> String {
    match project_id {
        Some(pid) => format!("perms:{user_id}:{pid}"),
        None => format!("perms:{user_id}:global"),
    }
}

/// Resolve all effective permissions for a user, optionally scoped to a project.
#[tracing::instrument(skip(pool, valkey), fields(%user_id), err)]
pub async fn effective_permissions(
    pool: &PgPool,
    valkey: &fred::clients::Pool,
    user_id: Uuid,
    project_id: Option<Uuid>,
) -> anyhow::Result<HashSet<Permission>> {
    let key = cache_key(user_id, project_id);

    if let Some(cached) = valkey::get_cached::<Vec<String>>(valkey, &key).await {
        let perms = cached
            .iter()
            .filter_map(|s| if let Ok(p) = Permission::from_str(s) {
                Some(p)
            } else {
                tracing::warn!(permission = %s, "unparseable permission string in cache, ignoring");
                None
            })
            .collect();
        return Ok(perms);
    }

    let perm_names: Vec<String> = sqlx::query_scalar!(
        r#"SELECT DISTINCT p.name as "name!"
        FROM permissions p
        WHERE p.id IN (
            SELECT rp.permission_id
            FROM user_roles ur
            JOIN role_permissions rp ON rp.role_id = ur.role_id
            WHERE ur.user_id = $1
              AND ur.project_id IS NULL

            UNION

            SELECT rp.permission_id
            FROM user_roles ur
            JOIN role_permissions rp ON rp.role_id = ur.role_id
            WHERE ur.user_id = $1
              AND ur.project_id = $2

            UNION

            SELECT d.permission_id
            FROM delegations d
            WHERE d.delegate_id = $1
              AND d.project_id IS NULL
              AND d.revoked_at IS NULL
              AND (d.expires_at IS NULL OR d.expires_at > now())

            UNION

            SELECT d.permission_id
            FROM delegations d
            WHERE d.delegate_id = $1
              AND d.project_id = $2
              AND d.revoked_at IS NULL
              AND (d.expires_at IS NULL OR d.expires_at > now())
        )"#,
        user_id,
        project_id,
    )
    .fetch_all(pool)
    .await?;

    let mut perms: HashSet<Permission> = perm_names
        .iter()
        .filter_map(|s| Permission::from_str(s).ok())
        .collect();

    if let Some(pid) = project_id {
        add_workspace_permissions(pool, &mut perms, user_id, pid).await?;
    }

    let cache_strings: Vec<String> = perms.iter().map(|p| p.as_str().to_owned()).collect();
    let _ = valkey::set_cached(valkey, &key, &cache_strings, cache_ttl()).await;

    Ok(perms)
}

/// Check whether a user has a specific permission.
#[tracing::instrument(skip(pool, valkey), fields(%user_id, %perm), err)]
pub async fn has_permission(
    pool: &PgPool,
    valkey: &fred::clients::Pool,
    user_id: Uuid,
    project_id: Option<Uuid>,
    perm: Permission,
) -> anyhow::Result<bool> {
    let perms = effective_permissions(pool, valkey, user_id, project_id).await?;
    Ok(perms.contains(&perm))
}

/// Check whether a user has a permission, intersected with optional API token scopes.
#[tracing::instrument(skip(pool, valkey), fields(%user_id, %perm), err)]
pub async fn has_permission_scoped(
    pool: &PgPool,
    valkey: &fred::clients::Pool,
    user_id: Uuid,
    project_id: Option<Uuid>,
    perm: Permission,
    token_scopes: Option<&[String]>,
) -> anyhow::Result<bool> {
    if !scope_allows(token_scopes, perm) {
        return Ok(false);
    }
    has_permission(pool, valkey, user_id, project_id, perm).await
}

/// Check whether a set of token scopes allows a given permission.
pub fn scope_allows(token_scopes: Option<&[String]>, perm: Permission) -> bool {
    let Some(scopes) = token_scopes else {
        return true;
    };
    if scopes.is_empty() || scopes.iter().any(|s| s == "*") {
        return true;
    }
    scopes.iter().any(|s| s == perm.as_str())
}

/// Grant implicit project permissions based on workspace membership.
async fn add_workspace_permissions(
    pool: &PgPool,
    perms: &mut HashSet<Permission>,
    user_id: Uuid,
    project_id: Uuid,
) -> anyhow::Result<()> {
    let role: Option<String> = sqlx::query_scalar!(
        r#"SELECT wm.role as "role!"
        FROM workspace_members wm
        JOIN projects p ON p.workspace_id = wm.workspace_id
        JOIN workspaces w ON w.id = wm.workspace_id
        WHERE p.id = $1 AND p.is_active = true AND w.is_active = true AND wm.user_id = $2"#,
        project_id,
        user_id,
    )
    .fetch_optional(pool)
    .await?;

    if let Some(role) = role {
        perms.insert(Permission::ProjectRead);
        if role == "owner" || role == "admin" {
            perms.insert(Permission::ProjectWrite);
        }
    }

    Ok(())
}

/// Get permissions for a specific role by ID.
#[tracing::instrument(skip(pool), fields(%role_id), err)]
pub async fn role_permissions(pool: &PgPool, role_id: Uuid) -> anyhow::Result<HashSet<Permission>> {
    let names: Vec<String> = sqlx::query_scalar!(
        r#"SELECT p.name as "name!"
        FROM permissions p
        JOIN role_permissions rp ON rp.permission_id = p.id
        WHERE rp.role_id = $1"#,
        role_id,
    )
    .fetch_all(pool)
    .await?;

    Ok(names
        .iter()
        .filter_map(|s| Permission::from_str(s).ok())
        .collect())
}

/// Invalidate cached permissions for a user.
#[tracing::instrument(skip(valkey), fields(%user_id), err)]
pub async fn invalidate_permissions(
    valkey: &fred::clients::Pool,
    user_id: Uuid,
    project_id: Option<Uuid>,
) -> anyhow::Result<()> {
    if let Some(pid) = project_id {
        valkey::invalidate(valkey, &cache_key(user_id, None)).await?;
        valkey::invalidate(valkey, &cache_key(user_id, Some(pid))).await?;
    } else {
        valkey::invalidate_pattern(valkey, &format!("perms:{user_id}:*")).await?;
    }
    Ok(())
}

/// Concrete [`PermissionChecker`] backed by Postgres + Valkey.
pub struct PgPermissionChecker<'a> {
    pub pool: &'a PgPool,
    pub valkey: &'a fred::clients::Pool,
}

impl PermissionChecker for PgPermissionChecker<'_> {
    async fn has_permission(
        &self,
        user_id: Uuid,
        project_id: Option<Uuid>,
        perm: Permission,
    ) -> anyhow::Result<bool> {
        has_permission(self.pool, self.valkey, user_id, project_id, perm).await
    }

    async fn has_permission_scoped(
        &self,
        user_id: Uuid,
        project_id: Option<Uuid>,
        perm: Permission,
        token_scopes: Option<&[String]>,
    ) -> anyhow::Result<bool> {
        has_permission_scoped(
            self.pool,
            self.valkey,
            user_id,
            project_id,
            perm,
            token_scopes,
        )
        .await
    }
}

impl PermissionResolver for PgPermissionChecker<'_> {
    async fn role_permissions(
        &self,
        role_id: Uuid,
    ) -> anyhow::Result<std::collections::HashSet<Permission>> {
        role_permissions(self.pool, role_id).await
    }

    async fn effective_permissions(
        &self,
        user_id: Uuid,
        project_id: Option<Uuid>,
    ) -> anyhow::Result<std::collections::HashSet<Permission>> {
        effective_permissions(self.pool, self.valkey, user_id, project_id).await
    }

    async fn invalidate_permissions(
        &self,
        user_id: Uuid,
        project_id: Option<Uuid>,
    ) -> anyhow::Result<()> {
        invalidate_permissions(self.valkey, user_id, project_id).await
    }
}

// ---------------------------------------------------------------------------
// Delegation types and functions
// ---------------------------------------------------------------------------

/// Parameters for creating a permission delegation.
#[derive(Debug)]
pub struct CreateDelegationParams {
    pub delegator_id: Uuid,
    pub delegate_id: Uuid,
    pub permission: Permission,
    pub project_id: Option<Uuid>,
    pub expires_at: Option<DateTime<Utc>>,
    pub reason: Option<String>,
}

/// A delegation record from the database.
#[derive(Debug, serde::Serialize)]
pub struct Delegation {
    pub id: Uuid,
    pub delegator_id: Uuid,
    pub delegate_id: Uuid,
    pub permission_id: Uuid,
    pub permission_name: String,
    pub project_id: Option<Uuid>,
    pub expires_at: Option<DateTime<Utc>>,
    pub reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// Create a permission delegation from one user to another.
///
/// Security: prevents self-delegation and re-delegation of delegated-only permissions (A6).
#[tracing::instrument(skip(pool, valkey), fields(delegator = %req.delegator_id, delegate = %req.delegate_id), err)]
pub async fn create_delegation(
    pool: &PgPool,
    valkey: &fred::clients::Pool,
    req: &CreateDelegationParams,
) -> Result<Delegation, ApiError> {
    if req.delegator_id == req.delegate_id {
        return Err(ApiError::BadRequest(
            "cannot delegate permissions to yourself".into(),
        ));
    }

    // Validate delegator holds this permission
    let delegator_has = has_permission(
        pool,
        valkey,
        req.delegator_id,
        req.project_id,
        req.permission,
    )
    .await
    .map_err(ApiError::Internal)?;
    if !delegator_has {
        return Err(ApiError::Forbidden);
    }

    // A6: Prevent re-delegation — delegator must hold via direct role assignment
    let has_via_role: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM user_roles ur \
         JOIN role_permissions rp ON rp.role_id = ur.role_id \
         JOIN permissions p ON p.id = rp.permission_id \
         WHERE ur.user_id = $1 AND p.name = $2 \
         AND (ur.project_id = $3 OR ur.project_id IS NULL OR $3::uuid IS NULL))",
    )
    .bind(req.delegator_id)
    .bind(req.permission.as_str())
    .bind(req.project_id)
    .fetch_one(pool)
    .await?;

    if !has_via_role {
        return Err(ApiError::BadRequest(
            "cannot delegate a permission obtained only via delegation".into(),
        ));
    }

    // Look up permission ID
    let permission_id: Uuid = sqlx::query_scalar("SELECT id FROM permissions WHERE name = $1")
        .bind(req.permission.as_str())
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| ApiError::BadRequest("unknown permission".into()))?;

    let row = sqlx::query_as!(
        Delegation,
        r#"
        INSERT INTO delegations (delegator_id, delegate_id, permission_id, project_id, expires_at, reason)
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING id, delegator_id, delegate_id, permission_id,
                  (SELECT name FROM permissions WHERE id = permission_id) as "permission_name!",
                  project_id, expires_at, reason, created_at, revoked_at
        "#,
        req.delegator_id,
        req.delegate_id,
        permission_id,
        req.project_id,
        req.expires_at,
        req.reason,
    )
    .fetch_one(pool)
    .await?;

    // Invalidate delegate's cached permissions
    let _ = invalidate_permissions(valkey, req.delegate_id, req.project_id).await;

    Ok(row)
}

/// Revoke a delegation by setting its `revoked_at` timestamp.
#[tracing::instrument(skip(pool, valkey), fields(%delegation_id), err)]
pub async fn revoke_delegation(
    pool: &PgPool,
    valkey: &fred::clients::Pool,
    delegation_id: Uuid,
) -> Result<(), ApiError> {
    let row = sqlx::query!(
        r#"
        UPDATE delegations SET revoked_at = now()
        WHERE id = $1 AND revoked_at IS NULL
        RETURNING delegate_id, project_id
        "#,
        delegation_id,
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("delegation".into()))?;

    let _ = invalidate_permissions(valkey, row.delegate_id, row.project_id).await;

    Ok(())
}

/// List delegations granted by or received by a user.
#[tracing::instrument(skip(pool), fields(%user_id), err)]
pub async fn list_delegations(
    pool: &PgPool,
    user_id: Uuid,
    limit: i64,
    offset: i64,
) -> Result<Vec<Delegation>, ApiError> {
    let rows = sqlx::query_as!(
        Delegation,
        r#"
        SELECT
            d.id,
            d.delegator_id,
            d.delegate_id,
            d.permission_id,
            p.name as "permission_name!",
            d.project_id,
            d.expires_at,
            d.reason,
            d.created_at,
            d.revoked_at
        FROM delegations d
        JOIN permissions p ON p.id = d.permission_id
        WHERE d.delegator_id = $1 OR d.delegate_id = $1
        ORDER BY d.created_at DESC
        LIMIT $2 OFFSET $3
        "#,
        user_id,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_with_project() {
        let user = Uuid::nil();
        let project = Uuid::max();
        let key = cache_key(user, Some(project));
        assert_eq!(key, format!("perms:{user}:{project}"));
    }

    #[test]
    fn cache_key_without_project() {
        let user = Uuid::nil();
        let key = cache_key(user, None);
        assert_eq!(key, format!("perms:{user}:global"));
    }

    #[test]
    fn scope_allows_none_is_unrestricted() {
        assert!(scope_allows(None, Permission::ProjectRead));
    }

    #[test]
    fn scope_allows_empty_is_unrestricted() {
        let scopes: Vec<String> = vec![];
        assert!(scope_allows(Some(&scopes), Permission::ProjectRead));
    }

    #[test]
    fn scope_allows_wildcard_is_unrestricted() {
        let scopes = vec!["*".to_string()];
        assert!(scope_allows(Some(&scopes), Permission::ProjectRead));
    }

    #[test]
    fn scope_allows_matching_permission() {
        let scopes = vec!["project:read".to_string(), "project:write".to_string()];
        assert!(scope_allows(Some(&scopes), Permission::ProjectRead));
        assert!(scope_allows(Some(&scopes), Permission::ProjectWrite));
    }

    #[test]
    fn scope_denies_non_matching_permission() {
        let scopes = vec!["project:read".to_string()];
        assert!(!scope_allows(Some(&scopes), Permission::ProjectWrite));
    }

    #[test]
    fn cache_ttl_defaults_to_300() {
        let ttl = cache_ttl();
        assert!(ttl > 0, "cache_ttl should be positive, got {ttl}");
    }
}
