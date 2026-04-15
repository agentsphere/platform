// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! DB-backed implementations of git traits.
//!
//! These implementations require `PgPool` for database queries. They implement
//! the traits defined in `traits.rs` that have no default implementation.
//!
//! - [`PgProjectResolver`] — resolve owner/repo path to a project
//! - [`PgBranchProtectionProvider`] — look up branch protection rules
//! - [`PgGitAuthenticator`] — authenticate via basic auth or SSH key
//! - [`PgGitAccessControl`] — check read/write access via RBAC
//! - [`PgPostReceiveHandler`] — side effects after push (generic over event sinks)

use sqlx::PgPool;
use uuid::Uuid;

use platform_types::{GitError, Permission, PermissionChecker};

use crate::protection::BranchProtection;
use crate::traits::{
    BranchProtectionProvider, GitAccessControl, GitAuthenticator, PostReceiveHandler,
    ProjectResolver,
};
use crate::types::{GitUser, MrSyncEvent, PushEvent, ResolvedProject, TagEvent};

// ---------------------------------------------------------------------------
// 1. PgProjectResolver
// ---------------------------------------------------------------------------

/// Resolves owner/repo path segments to a project via Postgres.
pub struct PgProjectResolver<'a> {
    pool: &'a PgPool,
    repos_path: &'a std::path::Path,
}

impl<'a> PgProjectResolver<'a> {
    pub fn new(pool: &'a PgPool, repos_path: &'a std::path::Path) -> Self {
        Self { pool, repos_path }
    }
}

impl ProjectResolver for PgProjectResolver<'_> {
    async fn resolve(&self, owner: &str, repo: &str) -> Result<ResolvedProject, GitError> {
        let row = sqlx::query!(
            r#"SELECT p.id as "project_id!", p.owner_id as "owner_id!",
                      p.default_branch as "default_branch!", p.visibility as "visibility!"
               FROM projects p
               JOIN users u ON p.owner_id = u.id
               WHERE u.name = $1 AND p.name = $2 AND p.is_active = true"#,
            owner,
            repo,
        )
        .fetch_optional(self.pool)
        .await
        .map_err(|e| GitError::Other(anyhow::anyhow!(e)))?
        .ok_or_else(|| GitError::NotFound(format!("{owner}/{repo}")))?;

        Ok(ResolvedProject {
            project_id: row.project_id,
            owner_id: row.owner_id,
            repo_disk_path: self.repos_path.join(owner).join(format!("{repo}.git")),
            default_branch: row.default_branch,
            visibility: row.visibility,
        })
    }
}

// ---------------------------------------------------------------------------
// 2. PgBranchProtectionProvider
// ---------------------------------------------------------------------------

/// Looks up branch protection rules from Postgres.
pub struct PgBranchProtectionProvider<'a> {
    pool: &'a PgPool,
}

impl<'a> PgBranchProtectionProvider<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }
}

impl BranchProtectionProvider for PgBranchProtectionProvider<'_> {
    async fn get_protection(
        &self,
        project_id: Uuid,
        branch: &str,
    ) -> Result<Option<BranchProtection>, anyhow::Error> {
        let row = sqlx::query!(
            r#"SELECT id, pattern, require_pr, block_force_push, required_approvals,
                      dismiss_stale_reviews, required_checks, require_up_to_date,
                      allow_admin_bypass, merge_methods
               FROM branch_protection_rules
               WHERE project_id = $1 AND pattern = $2"#,
            project_id,
            branch,
        )
        .fetch_optional(self.pool)
        .await?;

        Ok(row.map(|r| BranchProtection {
            id: r.id,
            project_id,
            pattern: r.pattern,
            require_pr: r.require_pr,
            block_force_push: r.block_force_push,
            required_approvals: r.required_approvals,
            dismiss_stale_reviews: r.dismiss_stale_reviews,
            required_checks: r.required_checks,
            require_up_to_date: r.require_up_to_date,
            allow_admin_bypass: r.allow_admin_bypass,
            merge_methods: r.merge_methods,
        }))
    }
}

// ---------------------------------------------------------------------------
// 3. PgGitAuthenticator
// ---------------------------------------------------------------------------

/// Authenticates git users via basic auth (API tokens) or SSH keys from Postgres.
pub struct PgGitAuthenticator<'a> {
    pool: &'a PgPool,
}

impl<'a> PgGitAuthenticator<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }
}

impl GitAuthenticator for PgGitAuthenticator<'_> {
    async fn authenticate_basic(
        &self,
        username: &str,
        password: &str,
    ) -> Result<GitUser, GitError> {
        // Treat password as an API token
        let token_hash = platform_auth::token::hash_token(password);

        let row = sqlx::query!(
            r#"SELECT u.id as "user_id!", u.name as "user_name!",
                      t.project_id, t.scope_workspace_id, t.scopes
               FROM api_tokens t
               JOIN users u ON t.user_id = u.id
               WHERE t.token_hash = $1
                 AND u.name = $2
                 AND u.is_active = true
                 AND (t.expires_at IS NULL OR t.expires_at > now())"#,
            &token_hash,
            username,
        )
        .fetch_optional(self.pool)
        .await
        .map_err(|e| GitError::Other(anyhow::anyhow!(e)))?
        .ok_or(GitError::Unauthorized)?;

        Ok(GitUser {
            user_id: row.user_id,
            user_name: row.user_name,
            ip_addr: None,
            boundary_project_id: row.project_id,
            boundary_workspace_id: row.scope_workspace_id,
            token_scopes: Some(row.scopes),
        })
    }

    async fn authenticate_ssh_key(&self, fingerprint: &str) -> Result<GitUser, GitError> {
        let row = sqlx::query!(
            r#"SELECT u.id as "user_id!", u.name as "user_name!"
               FROM users u
               JOIN user_ssh_keys sk ON u.id = sk.user_id
               WHERE sk.fingerprint = $1 AND u.is_active = true"#,
            fingerprint,
        )
        .fetch_optional(self.pool)
        .await
        .map_err(|e| GitError::Other(anyhow::anyhow!(e)))?
        .ok_or(GitError::Unauthorized)?;

        Ok(GitUser {
            user_id: row.user_id,
            user_name: row.user_name,
            ip_addr: None,
            boundary_project_id: None,
            boundary_workspace_id: None,
            token_scopes: None,
        })
    }
}

// ---------------------------------------------------------------------------
// 4. PgGitAccessControl
// ---------------------------------------------------------------------------

/// Checks read/write access via the RBAC permission checker.
///
/// Generic over `P: PermissionChecker` to allow different permission
/// resolution strategies (DB-backed, cached, mock).
pub struct PgGitAccessControl<P: PermissionChecker> {
    perm_checker: P,
}

impl<P: PermissionChecker> PgGitAccessControl<P> {
    pub fn new(perm_checker: P) -> Self {
        Self { perm_checker }
    }
}

impl<P: PermissionChecker + 'static> GitAccessControl for PgGitAccessControl<P> {
    async fn check_read(&self, user: &GitUser, project: &ResolvedProject) -> Result<(), GitError> {
        // Public and internal repos are readable by any authenticated user
        if project.visibility == "public" || project.visibility == "internal" {
            return Ok(());
        }
        // Owner always has access
        if user.user_id == project.owner_id {
            return Ok(());
        }

        let allowed = self
            .perm_checker
            .has_permission_scoped(
                user.user_id,
                Some(project.project_id),
                Permission::ProjectRead,
                user.token_scopes.as_deref(),
            )
            .await
            .map_err(|e| GitError::Other(anyhow::anyhow!(e)))?;

        if !allowed {
            // Return NotFound to avoid leaking existence of private repos
            return Err(GitError::NotFound(String::new()));
        }
        Ok(())
    }

    async fn check_write(&self, user: &GitUser, project: &ResolvedProject) -> Result<(), GitError> {
        // Owner always has write access
        if user.user_id == project.owner_id {
            return Ok(());
        }

        let allowed = self
            .perm_checker
            .has_permission_scoped(
                user.user_id,
                Some(project.project_id),
                Permission::ProjectWrite,
                user.token_scopes.as_deref(),
            )
            .await
            .map_err(|e| GitError::Other(anyhow::anyhow!(e)))?;

        if !allowed {
            return Err(GitError::Forbidden);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 5. PgPostReceiveHandler
// ---------------------------------------------------------------------------

/// Post-receive handler backed by Postgres that publishes platform events.
///
/// DB-local side effects (MR updates) are performed directly. Cross-module
/// side effects (pipeline triggers, webhooks) are handled by publishing
/// `PlatformEvent` variants, which the eventbus dispatches.
pub struct PgPostReceiveHandler {
    pool: PgPool,
    valkey: fred::clients::Pool,
}

impl PgPostReceiveHandler {
    pub fn new(pool: PgPool, valkey: fred::clients::Pool) -> Self {
        Self { pool, valkey }
    }
}

impl PostReceiveHandler for PgPostReceiveHandler {
    async fn on_push(&self, params: &PushEvent) -> Result<(), anyhow::Error> {
        // Publish CodePushed event (eventbus handles pipeline trigger + webhooks)
        let event = platform_types::events::PlatformEvent::CodePushed {
            project_id: params.project_id,
            user_id: params.user_id,
            user_name: params.user_name.clone(),
            repo_path: params.repo_path.clone(),
            branch: params.branch.clone(),
            commit_sha: params.commit_sha.clone(),
        };
        if let Err(e) = platform_types::events::publish(&self.valkey, &event).await {
            tracing::warn!(error = %e, "failed to publish CodePushed event");
        }

        // Update any open MRs that match the pushed branch (local DB side effect)
        let _ = sqlx::query!(
            "UPDATE merge_requests SET updated_at = now()
             WHERE project_id = $1 AND source_branch = $2 AND status = 'open'",
            params.project_id,
            params.branch,
        )
        .execute(&self.pool)
        .await;

        Ok(())
    }

    async fn on_tag(&self, params: &TagEvent) -> Result<(), anyhow::Error> {
        // Publish TagPushed event (eventbus handles webhooks)
        let event = platform_types::events::PlatformEvent::TagPushed {
            project_id: params.project_id,
            user_id: params.user_id,
            user_name: params.user_name.clone(),
            repo_path: params.repo_path.clone(),
            tag_name: params.tag_name.clone(),
            commit_sha: params.commit_sha.clone(),
        };
        if let Err(e) = platform_types::events::publish(&self.valkey, &event).await {
            tracing::warn!(error = %e, "failed to publish TagPushed event");
        }

        Ok(())
    }

    async fn on_mr_sync(&self, params: &MrSyncEvent) -> Result<(), anyhow::Error> {
        // Update MR head SHA (local DB side effect)
        let _ = sqlx::query!(
            "UPDATE merge_requests SET head_sha = $1, updated_at = now()
             WHERE project_id = $2 AND source_branch = $3 AND status = 'open'",
            params.commit_sha,
            params.project_id,
            params.branch,
        )
        .execute(&self.pool)
        .await;

        // Publish MrBranchSynced event (eventbus handles webhooks)
        let event = platform_types::events::PlatformEvent::MrBranchSynced {
            project_id: params.project_id,
            user_id: params.user_id,
            repo_path: params.repo_path.clone(),
            branch: params.branch.clone(),
            commit_sha: params.commit_sha.clone(),
        };
        if let Err(e) = platform_types::events::publish(&self.valkey, &event).await {
            tracing::warn!(error = %e, "failed to publish MrBranchSynced event");
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_resolver_is_constructible() {
        fn make(pool: &PgPool) -> PgProjectResolver<'_> {
            PgProjectResolver::new(pool, std::path::Path::new("/repos"))
        }
        let _ = make as fn(&PgPool) -> PgProjectResolver<'_>;
    }

    #[test]
    fn branch_protection_provider_is_constructible() {
        fn make(pool: &PgPool) -> PgBranchProtectionProvider<'_> {
            PgBranchProtectionProvider::new(pool)
        }
        let _ = make as fn(&PgPool) -> PgBranchProtectionProvider<'_>;
    }

    #[test]
    fn git_authenticator_is_constructible() {
        fn make(pool: &PgPool) -> PgGitAuthenticator<'_> {
            PgGitAuthenticator::new(pool)
        }
        let _ = make as fn(&PgPool) -> PgGitAuthenticator<'_>;
    }

    #[test]
    fn git_access_control_compiles_with_mock() {
        struct MockChecker;
        impl PermissionChecker for MockChecker {
            async fn has_permission(
                &self,
                _user_id: Uuid,
                _project_id: Option<Uuid>,
                _perm: Permission,
            ) -> anyhow::Result<bool> {
                Ok(true)
            }
            async fn has_permission_scoped(
                &self,
                _user_id: Uuid,
                _project_id: Option<Uuid>,
                _perm: Permission,
                _token_scopes: Option<&[String]>,
            ) -> anyhow::Result<bool> {
                Ok(true)
            }
        }
        let _control = PgGitAccessControl::new(MockChecker);
    }

    #[test]
    fn post_receive_handler_is_constructible() {
        // Verify PgPostReceiveHandler compiles (needs PgPool + fred::Pool at runtime)
        fn make(pool: PgPool, valkey: fred::clients::Pool) -> PgPostReceiveHandler {
            PgPostReceiveHandler::new(pool, valkey)
        }
        let _ = make as fn(PgPool, fred::clients::Pool) -> PgPostReceiveHandler;
    }
}
