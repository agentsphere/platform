// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Combined services trait and state for git server operations.
//!
//! Follows the `PipelineServices` pattern: one combined supertrait, one generic
//! state struct. The main binary provides an implementation backed by `AppState`.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use uuid::Uuid;

use crate::error::GitError;
use crate::server_config::GitServerConfig;
use crate::traits::{
    BranchProtectionProvider, GitAccessControl, GitAuthenticator, PostReceiveHandler,
    ProjectResolver,
};
use crate::types::ResolvedProject;

// ---------------------------------------------------------------------------
// GpgKeyInfo — data returned from GPG key lookup
// ---------------------------------------------------------------------------

/// Information about a stored GPG key, returned by
/// [`GitServerServices::lookup_gpg_key`].
#[derive(Debug, Clone)]
pub struct GpgKeyInfo {
    pub public_key_bytes: Vec<u8>,
    pub fingerprint: String,
    pub emails: Vec<String>,
}

// ---------------------------------------------------------------------------
// GitRepoPathResolver — path lookup + scoped read checks
// ---------------------------------------------------------------------------

/// Resolves repo paths and checks scoped read permissions for browser/UI access.
pub trait GitRepoPathResolver: Send + Sync + 'static {
    /// Look up a project repo path by project ID (for browser API).
    fn get_project_repo_path(
        &self,
        project_id: Uuid,
    ) -> impl Future<Output = Result<(PathBuf, String), GitError>> + Send;

    /// Look up an ops repo path by project ID (for browser API).
    fn get_ops_repo_path(
        &self,
        project_id: Uuid,
    ) -> impl Future<Output = Result<(PathBuf, String), GitError>> + Send;

    /// Check project-read permission with token scopes (for browser API).
    fn check_project_read_scoped(
        &self,
        user_id: Uuid,
        project_id: Uuid,
        token_scopes: Option<&[String]>,
    ) -> impl Future<Output = Result<(), GitError>> + Send;

    /// Check workspace boundary for a project (API token workspace scope).
    fn check_workspace_boundary(
        &self,
        project_id: Uuid,
        workspace_id: Uuid,
    ) -> impl Future<Output = Result<bool, GitError>> + Send;
}

// ---------------------------------------------------------------------------
// LfsObjectStore — presigned URLs + size limits for LFS
// ---------------------------------------------------------------------------

/// Object storage operations for Git LFS (presigned URLs, size limits).
pub trait LfsObjectStore: Send + Sync + 'static {
    /// Generate a presigned read URL for a LFS object.
    fn presign_lfs_read(
        &self,
        path: &str,
        duration: Duration,
    ) -> impl Future<Output = Result<String, anyhow::Error>> + Send;

    /// Generate a presigned write URL for a LFS object.
    fn presign_lfs_write(
        &self,
        path: &str,
        duration: Duration,
    ) -> impl Future<Output = Result<String, anyhow::Error>> + Send;

    /// Maximum allowed LFS object size in bytes.
    fn max_lfs_object_bytes(&self) -> u64;
}

// ---------------------------------------------------------------------------
// GitSignatureVerifier — GPG key lookup + signature cache
// ---------------------------------------------------------------------------

/// GPG key lookup and signature verification caching.
pub trait GitSignatureVerifier: Send + Sync + 'static {
    /// Look up a GPG key by key ID for signature verification.
    fn lookup_gpg_key(
        &self,
        key_id: &str,
    ) -> impl Future<Output = Result<Option<GpgKeyInfo>, anyhow::Error>> + Send;

    /// Get a cached signature verification result.
    fn sig_cache_get(
        &self,
        key: &str,
    ) -> impl Future<Output = Result<Option<String>, anyhow::Error>> + Send;

    /// Cache a signature verification result.
    fn sig_cache_set(
        &self,
        key: &str,
        value: &str,
        ttl_secs: u64,
    ) -> impl Future<Output = Result<(), anyhow::Error>> + Send;
}

// ---------------------------------------------------------------------------
// GitPushAudit — rate limiting, SSH key tracking, admin checks, audit logging
// ---------------------------------------------------------------------------

/// Rate limiting, SSH key tracking, admin/owner checks, and audit logging
/// for git push operations.
pub trait GitPushAudit: Send + Sync + 'static {
    /// Rate-limit git authentication attempts.
    fn check_git_rate(&self, username: &str) -> impl Future<Output = Result<(), GitError>> + Send;

    /// Update SSH key last-used timestamp (fire-and-forget).
    fn update_ssh_key_last_used(&self, fingerprint: &str);

    /// Check whether a user is admin or project owner (for branch protection bypass).
    fn check_admin_or_owner(
        &self,
        user_id: Uuid,
        project: &ResolvedProject,
        allow_admin_bypass: bool,
    ) -> impl Future<Output = Result<bool, anyhow::Error>> + Send;

    /// Audit log a git push event.
    fn audit_git_push(
        &self,
        user_id: Uuid,
        user_name: &str,
        project_id: Uuid,
        ip_addr: Option<&str>,
    );
}

// ---------------------------------------------------------------------------
// GitServerServices — marker supertrait + blanket impl
// ---------------------------------------------------------------------------

/// Combined services trait for git server operations.
///
/// This is a marker trait — all methods live in focused sub-traits.
/// The blanket impl auto-derives `GitServerServices` for any type that
/// implements all 9 sub-traits.
pub trait GitServerServices:
    GitAuthenticator
    + GitAccessControl
    + ProjectResolver
    + BranchProtectionProvider
    + PostReceiveHandler
    + GitRepoPathResolver
    + LfsObjectStore
    + GitSignatureVerifier
    + GitPushAudit
    + Send
    + Sync
    + 'static
{
}

impl<T> GitServerServices for T where
    T: GitAuthenticator
        + GitAccessControl
        + ProjectResolver
        + BranchProtectionProvider
        + PostReceiveHandler
        + GitRepoPathResolver
        + LfsObjectStore
        + GitSignatureVerifier
        + GitPushAudit
        + Send
        + Sync
        + 'static
{
}

// ---------------------------------------------------------------------------
// GitServerState — generic state struct
// ---------------------------------------------------------------------------

/// Shared state for git server handlers, generic over the services implementation.
pub struct GitServerState<Svc: GitServerServices> {
    pub svc: Arc<Svc>,
    pub config: Arc<GitServerConfig>,
}

// Manual Clone impl so we don't require Svc: Clone (Arc handles sharing).
impl<Svc: GitServerServices> Clone for GitServerState<Svc> {
    fn clone(&self) -> Self {
        Self {
            svc: Arc::clone(&self.svc),
            config: Arc::clone(&self.config),
        }
    }
}

// ---------------------------------------------------------------------------
// BrowserUser — lightweight auth extractor for browser API
// ---------------------------------------------------------------------------

/// Lightweight authenticated user for browser API handlers.
///
/// Reads from request extensions (set by the caller's auth middleware).
/// Avoids coupling to `platform-auth::AuthUser` directly.
#[derive(Debug, Clone)]
pub struct BrowserUser {
    pub user_id: Uuid,
    pub token_scopes: Option<Vec<String>>,
    pub boundary_project_id: Option<Uuid>,
    pub boundary_workspace_id: Option<Uuid>,
}

impl<Svc: GitServerServices> FromRequestParts<GitServerState<Svc>> for BrowserUser {
    type Rejection = GitError;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &GitServerState<Svc>,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<BrowserUser>()
            .cloned()
            .ok_or(GitError::Unauthorized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpg_key_info_debug() {
        let info = GpgKeyInfo {
            public_key_bytes: vec![1, 2, 3],
            fingerprint: "ABCD1234".into(),
            emails: vec!["alice@example.com".into()],
        };
        let debug = format!("{info:?}");
        assert!(debug.contains("GpgKeyInfo"));
        assert!(debug.contains("ABCD1234"));
    }

    #[test]
    fn browser_user_debug() {
        let user = BrowserUser {
            user_id: Uuid::nil(),
            token_scopes: Some(vec!["project:read".into()]),
            boundary_project_id: None,
            boundary_workspace_id: None,
        };
        let debug = format!("{user:?}");
        assert!(debug.contains("BrowserUser"));
    }

    #[test]
    fn blanket_impl_links_sub_traits() {
        // Compile-time verification that the blanket impl connects
        // all 9 sub-traits to the GitServerServices marker.
        fn _takes<T: GitServerServices>(_: &T) {}
    }

    #[test]
    fn browser_user_clone() {
        let user = BrowserUser {
            user_id: Uuid::nil(),
            token_scopes: None,
            boundary_project_id: Some(Uuid::nil()),
            boundary_workspace_id: None,
        };
        let cloned = user.clone();
        assert_eq!(cloned.user_id, user.user_id);
        assert_eq!(cloned.boundary_project_id, user.boundary_project_id);
    }
}
