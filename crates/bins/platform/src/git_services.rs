// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Production [`GitServerServices`] implementation for the platform binary.
//!
//! Delegates to `Pg*` implementations from `platform-git` for the 5 original
//! supertraits, and implements the 4 decomposed sub-traits (`GitRepoPathResolver`,
//! `LfsObjectStore`, `GitSignatureVerifier`, `GitPushAudit`) using shared
//! infrastructure (pool, valkey, minio, audit). The marker `GitServerServices`
//! trait is satisfied via blanket impl.
//!
//! Wired into the HTTP and SSH servers via `crate::git::router()` and
//! `platform_git::ssh_server::run()`.

use std::path::PathBuf;
use std::time::Duration;

use fred::interfaces::KeysInterface;
use sqlx::PgPool;
use uuid::Uuid;

use platform_git::db_services::{
    PgBranchProtectionProvider, PgGitAccessControl, PgGitAuthenticator, PgPostReceiveHandler,
    PgProjectResolver,
};
use platform_git::error::GitError;
use platform_git::protection::BranchProtection;
use platform_git::server_services::{
    GitPushAudit, GitRepoPathResolver, GitSignatureVerifier, GpgKeyInfo, LfsObjectStore,
};
use platform_git::traits::{
    BranchProtectionProvider, GitAccessControl, GitAuthenticator, PostReceiveHandler,
    ProjectResolver,
};
use platform_git::types::{GitUser, MrSyncEvent, PushEvent, ResolvedProject, TagEvent};
use platform_types::audit::AuditEntry;
use platform_types::traits::AuditLogger;
use platform_types::{AuditLog, Permission, PermissionChecker};

// ---------------------------------------------------------------------------
// AppPermissionChecker
// ---------------------------------------------------------------------------

/// RBAC permission checker backed by Postgres + Valkey cache.
#[derive(Clone)]
pub struct AppPermissionChecker {
    pool: PgPool,
    valkey: fred::clients::Pool,
}

impl AppPermissionChecker {
    pub fn new(pool: PgPool, valkey: fred::clients::Pool) -> Self {
        Self { pool, valkey }
    }
}

impl PermissionChecker for AppPermissionChecker {
    async fn has_permission(
        &self,
        user_id: Uuid,
        project_id: Option<Uuid>,
        perm: Permission,
    ) -> anyhow::Result<bool> {
        platform_auth::resolver::has_permission(&self.pool, &self.valkey, user_id, project_id, perm)
            .await
    }

    async fn has_permission_scoped(
        &self,
        user_id: Uuid,
        project_id: Option<Uuid>,
        perm: Permission,
        token_scopes: Option<&[String]>,
    ) -> anyhow::Result<bool> {
        platform_auth::resolver::has_permission_scoped(
            &self.pool,
            &self.valkey,
            user_id,
            project_id,
            perm,
            token_scopes,
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// AppGitServerServices
// ---------------------------------------------------------------------------

/// Production [`GitServerServices`] that delegates to `Pg*` implementations
/// and shared infrastructure.
pub struct AppGitServerServices {
    pool: PgPool,
    valkey: fred::clients::Pool,
    minio: opendal::Operator,
    perm_checker: AppPermissionChecker,
    repos_path: PathBuf,
    ops_repos_path: PathBuf,
    audit_tx: AuditLog,
    max_lfs_bytes: u64,
}

impl AppGitServerServices {
    pub fn new(
        pool: PgPool,
        valkey: fred::clients::Pool,
        minio: opendal::Operator,
        repos_path: PathBuf,
        ops_repos_path: PathBuf,
        audit_tx: AuditLog,
        max_lfs_bytes: u64,
    ) -> Self {
        let perm_checker = AppPermissionChecker::new(pool.clone(), valkey.clone());
        Self {
            pool,
            valkey,
            minio,
            perm_checker,
            repos_path,
            ops_repos_path,
            audit_tx,
            max_lfs_bytes,
        }
    }
}

// ---------------------------------------------------------------------------
// 5 supertrait delegations
// ---------------------------------------------------------------------------

impl GitAuthenticator for AppGitServerServices {
    async fn authenticate_basic(
        &self,
        username: &str,
        password: &str,
    ) -> Result<GitUser, GitError> {
        PgGitAuthenticator::new(&self.pool)
            .authenticate_basic(username, password)
            .await
    }

    async fn authenticate_ssh_key(&self, fingerprint: &str) -> Result<GitUser, GitError> {
        PgGitAuthenticator::new(&self.pool)
            .authenticate_ssh_key(fingerprint)
            .await
    }
}

impl GitAccessControl for AppGitServerServices {
    async fn check_read(&self, user: &GitUser, project: &ResolvedProject) -> Result<(), GitError> {
        PgGitAccessControl::new(self.perm_checker.clone())
            .check_read(user, project)
            .await
    }

    async fn check_write(&self, user: &GitUser, project: &ResolvedProject) -> Result<(), GitError> {
        PgGitAccessControl::new(self.perm_checker.clone())
            .check_write(user, project)
            .await
    }
}

impl ProjectResolver for AppGitServerServices {
    async fn resolve(&self, owner: &str, repo: &str) -> Result<ResolvedProject, GitError> {
        PgProjectResolver::new(&self.pool, &self.repos_path)
            .resolve(owner, repo)
            .await
    }
}

impl BranchProtectionProvider for AppGitServerServices {
    async fn get_protection(
        &self,
        project_id: Uuid,
        branch: &str,
    ) -> Result<Option<BranchProtection>, anyhow::Error> {
        PgBranchProtectionProvider::new(&self.pool)
            .get_protection(project_id, branch)
            .await
    }
}

impl PostReceiveHandler for AppGitServerServices {
    async fn on_push(&self, params: &PushEvent) -> Result<(), anyhow::Error> {
        PgPostReceiveHandler::new(self.pool.clone(), self.valkey.clone())
            .on_push(params)
            .await
    }

    async fn on_tag(&self, params: &TagEvent) -> Result<(), anyhow::Error> {
        PgPostReceiveHandler::new(self.pool.clone(), self.valkey.clone())
            .on_tag(params)
            .await
    }

    async fn on_mr_sync(&self, params: &MrSyncEvent) -> Result<(), anyhow::Error> {
        PgPostReceiveHandler::new(self.pool.clone(), self.valkey.clone())
            .on_mr_sync(params)
            .await
    }
}

// ---------------------------------------------------------------------------
// GitRepoPathResolver — path lookup + scoped read checks
// ---------------------------------------------------------------------------

impl GitRepoPathResolver for AppGitServerServices {
    async fn get_project_repo_path(&self, project_id: Uuid) -> Result<(PathBuf, String), GitError> {
        let row = sqlx::query!(
            r#"SELECT p.name as "name!", u.name as "owner!",
                      p.default_branch as "default_branch!"
               FROM projects p
               JOIN users u ON p.owner_id = u.id
               WHERE p.id = $1 AND p.is_active = true"#,
            project_id,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| GitError::Other(anyhow::anyhow!(e)))?
        .ok_or_else(|| GitError::NotFound(format!("project {project_id}")))?;

        let path = self
            .repos_path
            .join(&row.owner)
            .join(format!("{}.git", row.name));
        Ok((path, row.default_branch))
    }

    async fn get_ops_repo_path(&self, project_id: Uuid) -> Result<(PathBuf, String), GitError> {
        let row = sqlx::query!(
            r#"SELECT id as "id!", branch as "branch!"
               FROM ops_repos
               WHERE project_id = $1"#,
            project_id,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| GitError::Other(anyhow::anyhow!(e)))?
        .ok_or_else(|| GitError::NotFound(format!("ops repo for project {project_id}")))?;

        let path = self.ops_repos_path.join(row.id.to_string());
        Ok((path, row.branch))
    }

    async fn check_project_read_scoped(
        &self,
        user_id: Uuid,
        project_id: Uuid,
        token_scopes: Option<&[String]>,
    ) -> Result<(), GitError> {
        let allowed = self
            .perm_checker
            .has_permission_scoped(
                user_id,
                Some(project_id),
                Permission::ProjectRead,
                token_scopes,
            )
            .await
            .map_err(|e| GitError::Other(anyhow::anyhow!(e)))?;

        if !allowed {
            return Err(GitError::NotFound(String::new()));
        }
        Ok(())
    }

    async fn check_workspace_boundary(
        &self,
        project_id: Uuid,
        workspace_id: Uuid,
    ) -> Result<bool, GitError> {
        let belongs = sqlx::query_scalar!(
            r#"SELECT EXISTS(
                SELECT 1 FROM projects
                WHERE id = $1 AND workspace_id = $2 AND is_active = true
            ) as "exists!""#,
            project_id,
            workspace_id,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| GitError::Other(anyhow::anyhow!(e)))?;

        Ok(belongs)
    }
}

// ---------------------------------------------------------------------------
// LfsObjectStore — presigned URLs + size limits
// ---------------------------------------------------------------------------

impl LfsObjectStore for AppGitServerServices {
    async fn presign_lfs_read(
        &self,
        path: &str,
        duration: Duration,
    ) -> Result<String, anyhow::Error> {
        let signed = self.minio.presign_read(path, duration).await?;
        Ok(signed.uri().to_string())
    }

    async fn presign_lfs_write(
        &self,
        path: &str,
        duration: Duration,
    ) -> Result<String, anyhow::Error> {
        let signed = self.minio.presign_write(path, duration).await?;
        Ok(signed.uri().to_string())
    }

    fn max_lfs_object_bytes(&self) -> u64 {
        self.max_lfs_bytes
    }
}

// ---------------------------------------------------------------------------
// GitSignatureVerifier — GPG key lookup + signature cache
// ---------------------------------------------------------------------------

impl GitSignatureVerifier for AppGitServerServices {
    async fn lookup_gpg_key(&self, key_id: &str) -> Result<Option<GpgKeyInfo>, anyhow::Error> {
        let row = sqlx::query!(
            r#"SELECT public_key_bytes as "public_key_bytes!", fingerprint as "fingerprint!",
                      emails as "emails!"
               FROM user_gpg_keys
               WHERE key_id = $1"#,
            key_id,
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| GpgKeyInfo {
            public_key_bytes: r.public_key_bytes,
            fingerprint: r.fingerprint,
            emails: r.emails,
        }))
    }

    async fn sig_cache_get(&self, key: &str) -> Result<Option<String>, anyhow::Error> {
        let cache_key = format!("gpg:sig:{key}");
        let val: Option<String> = self.valkey.next().get(&cache_key).await.unwrap_or(None);
        Ok(val)
    }

    async fn sig_cache_set(
        &self,
        key: &str,
        value: &str,
        ttl_secs: u64,
    ) -> Result<(), anyhow::Error> {
        let cache_key = format!("gpg:sig:{key}");
        let client = self.valkey.next();
        let _: () = client.set(&cache_key, value, None, None, false).await?;
        let _: () = client
            .expire(&cache_key, ttl_secs.try_into().unwrap_or(3600), None)
            .await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// GitPushAudit — rate limiting, SSH key tracking, admin checks, audit logging
// ---------------------------------------------------------------------------

impl GitPushAudit for AppGitServerServices {
    async fn check_git_rate(&self, username: &str) -> Result<(), GitError> {
        platform_auth::rate_limit::check_rate(&self.valkey, "git", username, 100, 300)
            .await
            .map_err(|e| GitError::Other(anyhow::anyhow!("{e}")))
    }

    fn update_ssh_key_last_used(&self, fingerprint: &str) {
        let pool = self.pool.clone();
        let fp = fingerprint.to_string();
        tokio::spawn(async move {
            let _ = sqlx::query!(
                "UPDATE user_ssh_keys SET last_used_at = now() WHERE fingerprint = $1",
                fp,
            )
            .execute(&pool)
            .await;
        });
    }

    async fn check_admin_or_owner(
        &self,
        user_id: Uuid,
        project: &ResolvedProject,
        allow_admin_bypass: bool,
    ) -> Result<bool, anyhow::Error> {
        if user_id == project.owner_id {
            return Ok(true);
        }
        if allow_admin_bypass {
            let is_admin = platform_auth::resolver::has_permission(
                &self.pool,
                &self.valkey,
                user_id,
                None,
                Permission::AdminUsers,
            )
            .await?;
            if is_admin {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn audit_git_push(
        &self,
        user_id: Uuid,
        user_name: &str,
        project_id: Uuid,
        ip_addr: Option<&str>,
    ) {
        self.audit_tx.send_audit(AuditEntry {
            actor_id: user_id,
            actor_name: user_name.to_string(),
            action: "git.push".to_string(),
            resource: "project".to_string(),
            resource_id: Some(project_id),
            project_id: Some(project_id),
            detail: None,
            ip_addr: ip_addr.map(String::from),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perm_checker_is_constructible() {
        // Verify the types compose correctly (compile-time check).
        fn make(pool: PgPool, valkey: fred::clients::Pool) -> AppPermissionChecker {
            AppPermissionChecker::new(pool, valkey)
        }
        let _ = make as fn(PgPool, fred::clients::Pool) -> AppPermissionChecker;
    }

    #[test]
    fn app_git_server_services_satisfies_marker() {
        // Compile-time verification that AppGitServerServices satisfies
        // GitServerServices via the blanket impl.
        fn assert_impl<T: platform_git::GitServerServices>() {}
        assert_impl::<AppGitServerServices>();
    }

    #[test]
    fn git_services_is_constructible() {
        fn make(
            pool: PgPool,
            valkey: fred::clients::Pool,
            minio: opendal::Operator,
        ) -> AppGitServerServices {
            AppGitServerServices::new(
                pool,
                valkey,
                minio,
                PathBuf::from("/repos"),
                PathBuf::from("/ops-repos"),
                AuditLog::new(sqlx::PgPool::connect_lazy("postgres://unused").unwrap()),
                5_368_709_120,
            )
        }
        let _ = make as fn(PgPool, fred::clients::Pool, opendal::Operator) -> AppGitServerServices;
    }
}
