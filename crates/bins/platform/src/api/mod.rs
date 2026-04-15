// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! HTTP API handlers and route definitions for platform-next.
//!
//! Handlers below are copied from `src/api/` with imports rewritten to use
//! workspace crate APIs.  Modules commented out with `// TODO` have unresolved
//! dependencies on internal `src/` modules that haven't been fully extracted
//! into workspace crates yet.  The files are on disk and will be uncommented
//! as the remaining crate APIs are wired up.

pub mod helpers;

// -- Modules that compile against workspace crate APIs --
pub mod branch_protection;
pub mod cli_auth;
pub mod dashboard;
pub mod downloads;
pub mod health;
pub mod mesh;
pub mod notifications;
pub mod releases;

// -- Modules with remaining internal deps (files exist, not yet wired) --
// TODO: uncomment as crate APIs are wired up
// pub mod admin;           // needs platform_auth::resolver delegation functions
// pub mod commands;        // needs crate::workspace, crate::agent::commands
// pub mod deployments;     // needs crate::deployer, crate::store::eventbus
// pub mod flags;           // needs validation::check_ssrf_url, check_email
// pub mod gpg_keys;        // needs platform_git::gpg_keys
// pub mod issues;          // needs validation::check_labels
// pub mod llm_providers;   // needs platform_secrets::llm_providers, platform_agent::llm_validate
// pub mod merge_requests;  // needs crate::git, crate::pipeline, crate::deployer
// pub mod onboarding;      // needs crate::workspace, crate::onboarding
// pub mod passkeys;        // needs crate::auth::passkey
// pub mod pipelines;       // needs crate::pipeline::trigger, crate::pipeline::executor
// pub mod preview;         // needs crate::deployer
// pub mod projects;        // needs crate::workspace, crate::git, crate::deployer
// pub mod secrets;         // needs crate::secrets::request, crate::workspace
// pub mod sessions;        // needs platform_agent::service, platform_agent::pubsub_bridge
// pub mod setup;           // needs crate::store::bootstrap, crate::workspace
// pub mod ssh_keys;        // needs platform_git::ssh_keys
// pub mod user_keys;       // needs platform_secrets::user_keys
pub mod users;
// pub mod webhooks;        // needs validation::check_ssrf_url
// pub mod workspaces;      // needs crate::workspace

use axum::Router;

use crate::state::PlatformState;

#[allow(dead_code)]
pub fn router() -> Router<PlatformState> {
    Router::new()
        .merge(branch_protection::router())
        .merge(cli_auth::router())
        .merge(dashboard::router())
        .merge(downloads::router())
        .merge(health::router())
        .merge(mesh::router())
        .merge(notifications::router())
        .merge(releases::router())
        .merge(users::router())
}
