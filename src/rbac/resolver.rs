use std::collections::HashSet;
use std::str::FromStr;

use sqlx::PgPool;
use uuid::Uuid;

use crate::rbac::types::Permission;
use crate::store::valkey;

const CACHE_TTL_SECS: i64 = 300; // 5 minutes

fn cache_key(user_id: Uuid, project_id: Option<Uuid>) -> String {
    match project_id {
        Some(pid) => format!("perms:{user_id}:{pid}"),
        None => format!("perms:{user_id}:global"),
    }
}

/// Resolve all effective permissions for a user, optionally scoped to a project.
/// Checks Valkey cache first, then queries DB on miss.
#[tracing::instrument(skip(pool, valkey), fields(%user_id), err)]
pub async fn effective_permissions(
    pool: &PgPool,
    valkey: &fred::clients::Pool,
    user_id: Uuid,
    project_id: Option<Uuid>,
) -> anyhow::Result<HashSet<Permission>> {
    let key = cache_key(user_id, project_id);

    // Check cache
    if let Some(cached) = valkey::get_cached::<Vec<String>>(valkey, &key).await {
        let perms = cached
            .iter()
            .filter_map(|s| Permission::from_str(s).ok())
            .collect();
        return Ok(perms);
    }

    // Cache miss — query DB
    // Union of:
    //   1. Global role permissions (user_roles where project_id IS NULL)
    //   2. Project-scoped role permissions (if project_id provided)
    //   3. Active delegations (not revoked, not expired)
    let perm_names: Vec<String> = sqlx::query_scalar!(
        r#"
        SELECT DISTINCT p.name as "name!"
        FROM permissions p
        WHERE p.id IN (
            -- Global roles
            SELECT rp.permission_id
            FROM user_roles ur
            JOIN role_permissions rp ON rp.role_id = ur.role_id
            WHERE ur.user_id = $1
              AND ur.project_id IS NULL

            UNION

            -- Project-scoped roles
            SELECT rp.permission_id
            FROM user_roles ur
            JOIN role_permissions rp ON rp.role_id = ur.role_id
            WHERE ur.user_id = $1
              AND ur.project_id = $2

            UNION

            -- Active delegations (global)
            SELECT d.permission_id
            FROM delegations d
            WHERE d.delegate_id = $1
              AND d.project_id IS NULL
              AND d.revoked_at IS NULL
              AND (d.expires_at IS NULL OR d.expires_at > now())

            UNION

            -- Active delegations (project-scoped)
            SELECT d.permission_id
            FROM delegations d
            WHERE d.delegate_id = $1
              AND d.project_id = $2
              AND d.revoked_at IS NULL
              AND (d.expires_at IS NULL OR d.expires_at > now())
        )
        "#,
        user_id,
        project_id,
    )
    .fetch_all(pool)
    .await?;

    let perms: HashSet<Permission> = perm_names
        .iter()
        .filter_map(|s| Permission::from_str(s).ok())
        .collect();

    // Cache result
    let cache_strings: Vec<String> = perms.iter().map(|p| p.as_str().to_owned()).collect();
    let _ = valkey::set_cached(valkey, &key, &cache_strings, CACHE_TTL_SECS).await;

    Ok(perms)
}

/// Check whether a user has a specific permission, optionally scoped to a project.
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

/// Invalidate all cached permissions for a user.
/// Called when roles or delegations change.
#[tracing::instrument(skip(valkey), fields(%user_id), err)]
pub async fn invalidate_permissions(
    valkey: &fred::clients::Pool,
    user_id: Uuid,
    project_id: Option<Uuid>,
) -> anyhow::Result<()> {
    // Invalidate both global and project-specific cache
    valkey::invalidate(valkey, &cache_key(user_id, None)).await?;
    if let Some(pid) = project_id {
        valkey::invalidate(valkey, &cache_key(user_id, Some(pid))).await?;
    }
    Ok(())
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
    fn cache_key_deterministic() {
        let user = Uuid::nil();
        let project = Some(Uuid::nil());
        assert_eq!(cache_key(user, project), cache_key(user, project));
    }
}
