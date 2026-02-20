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
            .filter_map(|s| if let Ok(p) = Permission::from_str(s) {
                Some(p)
            } else {
                tracing::warn!(permission = %s, "unparseable permission string in cache, ignoring");
                None
            })
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

/// Check whether a user has a specific permission, intersected with optional
/// API token scopes. If `token_scopes` is `None` (session auth), checks full
/// role-based permissions. Otherwise, the permission must be both granted by
/// roles AND included in the token's scopes.
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
/// Returns `true` if:
/// - `token_scopes` is `None` (session auth, no restriction)
/// - scopes is empty (backward-compatible unrestricted token)
/// - scopes contains `"*"` (unrestricted token)
/// - scopes contains the permission's string representation
fn scope_allows(token_scopes: Option<&[String]>, perm: Permission) -> bool {
    let Some(scopes) = token_scopes else {
        return true; // session auth
    };
    if scopes.is_empty() || scopes.iter().any(|s| s == "*") {
        return true; // unrestricted token
    }
    scopes.iter().any(|s| s == perm.as_str())
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
    fn cache_key_different_users_differ() {
        let user_a = Uuid::nil();
        let user_b = Uuid::max();
        let project = Some(Uuid::nil());
        assert_ne!(
            cache_key(user_a, project),
            cache_key(user_b, project),
            "different users must produce different cache keys"
        );
    }

    #[test]
    fn cache_key_different_projects_differ() {
        let user = Uuid::nil();
        let project_a = Some(Uuid::nil());
        let project_b = Some(Uuid::max());
        assert_ne!(
            cache_key(user, project_a),
            cache_key(user, project_b),
            "different projects must produce different cache keys"
        );
    }

    // -- scope_allows --

    #[test]
    fn scope_allows_none_is_unrestricted() {
        assert!(scope_allows(None, Permission::ProjectRead));
        assert!(scope_allows(None, Permission::AdminUsers));
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
        assert!(scope_allows(Some(&scopes), Permission::AdminUsers));
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
        assert!(!scope_allows(Some(&scopes), Permission::AdminUsers));
    }

    #[test]
    fn scope_ignores_unknown_scopes() {
        let scopes = vec!["project:read".to_string(), "nonexistent:perm".to_string()];
        assert!(scope_allows(Some(&scopes), Permission::ProjectRead));
        assert!(!scope_allows(Some(&scopes), Permission::ProjectWrite));
    }
}
