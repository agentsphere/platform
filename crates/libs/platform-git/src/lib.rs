// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Git operations library for the platform.
//!
//! Provides trait-based abstractions over git CLI operations, plus pure-function
//! parsers for SSH keys, GPG keys, commit signatures, pkt-line protocol, and
//! SSH command parsing.
//!
//! ## Traits
//!
//! Core traits (defined in `platform-types`, re-exported here):
//! - [`GitCoreRead`] — core read operations (rev-parse, read-file, list-dir, etc.)
//! - [`GitWriter`] — write files via worktrees with per-repo locking
//! - [`GitMerger`] — merge strategies via worktrees
//!
//! Extended traits (defined here):
//! - [`GitRepo`] — browser/UI read operations (extends `GitCoreRead`)
//! - [`GitRepoManager`] — create/init repos, tags
//!
//! App-specific traits (no default impl, require DB access):
//! - [`PostReceiveHandler`], [`BranchProtectionProvider`], [`GitAuthenticator`],
//!   [`GitAccessControl`], [`ProjectResolver`]
//!
//! ## Concrete implementations
//!
//! - [`CliGitRepo`] — shells out to `git` CLI (implements `GitCoreRead` + `GitRepo`)
//! - [`CliGitRepoManager`] — shells out to `git` plumbing commands
//! - [`CliGitMerger`] — merge via worktrees + `git` CLI
//! - [`CliGitWorktreeWriter`] — write files via worktrees + per-repo locking

pub mod auto_merge;
pub mod browser;
pub mod browser_types;
pub mod db_services;
pub mod error;
pub mod gpg_keys;
pub mod hooks;
pub mod lfs;
pub mod lock;
pub mod ops;
pub mod pkt_line;
pub mod plumbing;
pub mod protection;
pub mod server_config;
pub mod server_services;
pub mod signature;
pub mod smart_http;
pub mod ssh_command;
pub mod ssh_keys;
pub mod ssh_server;
pub mod templates;
pub mod traits;
pub mod types;
pub mod validation;
pub mod worktree;

// Re-export core traits from platform-types at crate root.
pub use platform_types::{GitCoreRead, GitError, GitMerger, GitWriter};

// Re-export key types at crate root for convenience.
pub use auto_merge::AutoMergeHandler;
pub use browser_types::{BlobContent, BranchInfo, CommitInfo, TreeEntry};
pub use db_services::{
    PgBranchProtectionProvider, PgGitAccessControl, PgGitAuthenticator, PgPostReceiveHandler,
    PgProjectResolver,
};
// Note: PostReceiveSideEffects trait removed — PgPostReceiveHandler now publishes
// PlatformEvent variants directly via Valkey, handled by the eventbus.
pub use error::{GpgKeyError, SshError, SshKeyError};
pub use hooks::{PostReceiveParams, RefUpdate};
pub use lock::repo_lock;
pub use ops::CliGitRepo;
pub use pkt_line::{find_flush_pkt, pkt_line_header};
pub use plumbing::CliGitRepoManager;
pub use protection::BranchProtection;
pub use server_config::GitServerConfig;
pub use server_services::{
    BrowserUser, GitPushAudit, GitRepoPathResolver, GitServerServices, GitServerState,
    GitSignatureVerifier, GpgKeyInfo, LfsObjectStore,
};
pub use signature::{SignatureInfo, SignatureStatus};
pub use ssh_command::ParsedCommand;
pub use ssh_keys::ParsedSshKey;
pub use templates::TemplateFile;
pub use traits::*;
pub use types::*;
pub use worktree::{CliGitMerger, CliGitWorktreeWriter};
