// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! AI agent session orchestration: ephemeral K8s pods, identities, Valkey
//! pub/sub for events, and CLI subprocess management.
//!
//! This crate contains the pure logic and DB logic for agent sessions.
//! HTTP handlers stay in `src/api/sessions.rs` and call into crate functions.

pub mod claude_auth;
#[allow(dead_code)]
pub mod claude_cli;
pub mod claude_code;
pub mod cli_invoke;
pub mod commands;
pub mod config;
pub mod error;
pub mod identity;
pub mod llm_validate;
pub mod manager_prompt;
pub mod preview_watcher;
pub mod provider;
pub mod pubsub_bridge;
pub mod role;
pub mod service;
pub mod state;
pub mod valkey_acl;

// Re-export key types at crate root.
pub use claude_auth::CliAuthManager;
pub use claude_cli::session::CliSessionManager;
pub use config::AgentConfig;
pub use error::AgentError;
pub use provider::{
    AgentProvider, AgentSession, BuildPodParams, ProgressEvent, ProgressKind, ProviderConfig,
};
pub use role::{AgentRoleName, AgentRoleParseError};
pub use state::AgentState;
