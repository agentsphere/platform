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
pub mod admin;
pub mod alerts;
pub mod branch_protection;
pub mod cli_auth;
pub mod commands;
pub mod dashboard;
pub mod downloads;
pub mod flags;
pub mod gpg_keys;
pub mod health;
pub mod issues;
pub mod llm_providers;
pub mod mesh;
pub mod notifications;
pub mod releases;
pub mod setup;
pub mod ssh_keys;
pub mod user_keys;
pub mod users;
pub mod webhooks;

pub mod deployments;
pub mod merge_requests;
#[allow(dead_code)]
pub mod onboarding;
pub mod passkeys;
pub mod pipelines;
pub mod preview;
pub mod projects;
pub mod secrets;
pub mod sessions;
pub mod workspaces;

use axum::Router;

use crate::state::PlatformState;

#[allow(dead_code)]
pub fn router() -> Router<PlatformState> {
    Router::new()
        .merge(admin::router())
        .merge(alerts::router())
        .merge(branch_protection::router())
        .merge(cli_auth::router())
        .merge(commands::router())
        .merge(dashboard::router())
        .merge(downloads::router())
        .merge(flags::router())
        .merge(gpg_keys::router())
        .merge(health::router())
        .merge(issues::router())
        .merge(llm_providers::router())
        .merge(mesh::router())
        .merge(notifications::router())
        .merge(passkeys::router())
        .merge(releases::router())
        .merge(setup::router())
        .merge(ssh_keys::router())
        .merge(user_keys::router())
        .merge(users::router())
        .merge(preview::router())
        .merge(sessions::router())
        .merge(deployments::router())
        .merge(merge_requests::router())
        .merge(onboarding::router())
        .merge(pipelines::router())
        .merge(projects::router())
        .merge(secrets::router())
        .merge(webhooks::router())
        .merge(workspaces::router())
}
