// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Shared types, error handling, and utility functions for the platform.

pub mod audit;
pub mod auth_user;
pub mod config;
pub mod error;
pub mod events;
pub mod git_error;
pub mod git_traits;
pub mod health;
pub mod permission;
pub mod pool;
pub mod traits;
pub mod user_type;
pub mod validation;
pub mod valkey;

// Re-export key types at crate root for convenience.
pub use audit::{AuditEntry, AuditLog, send_audit};
pub use auth_user::{AuthUser, PermissionChecker, PermissionResolver, parse_user_type};
pub use error::ApiError;
pub use events::PlatformEvent;
pub use git_error::GitError;
pub use git_traits::{GitCoreRead, GitMerger, GitWriter};
pub use health::TaskRegistry;
pub use permission::Permission;
pub use traits::{
    AuditLogger, MergeRequestHandler, NotificationDispatcher, NotifyParams, OpsRepoManager,
    SecretsResolver, TaskHeartbeat, WebhookDispatcher, WorkspaceMembershipChecker,
};
#[cfg(feature = "kube")]
pub use traits::{ManifestApplier, RegistryCredentialProvider};
pub use user_type::UserType;
pub use validation::slugify_branch;

/// Generic paginated list response.
#[derive(Debug, serde::Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
pub struct ListResponse<T> {
    pub items: Vec<T>,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub total: i64,
}
