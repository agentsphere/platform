// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! HTTP API handlers and route definitions.

pub mod admin;
pub mod branch_protection;
pub mod cli_auth;
pub mod commands;
pub mod dashboard;
pub mod deployments;
pub mod downloads;
pub mod flags;
pub mod gpg_keys;
pub mod health;
pub mod helpers;
pub mod issues;
pub mod llm_providers;
pub mod merge_requests;
pub mod mesh;
pub mod notifications;
pub mod onboarding;
pub mod passkeys;
pub mod pipelines;
pub mod preview;
pub mod projects;
pub mod releases;
pub mod secrets;
pub mod sessions;
pub mod setup;
pub mod ssh_keys;
pub mod user_keys;
pub mod users;
pub mod webhooks;
pub mod workspaces;

use axum::Router;

use crate::store::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .merge(users::router())
        .merge(admin::router())
        .merge(projects::router())
        .merge(issues::router())
        .merge(merge_requests::router())
        .merge(webhooks::router())
        .merge(pipelines::router())
        .merge(deployments::router())
        .merge(flags::router())
        .merge(sessions::router())
        .merge(secrets::router())
        .merge(notifications::router())
        .merge(passkeys::router())
        .merge(user_keys::router())
        .merge(ssh_keys::router())
        .merge(gpg_keys::router())
        .merge(workspaces::router())
        .merge(branch_protection::router())
        .merge(releases::router())
        .merge(dashboard::router())
        .merge(onboarding::router())
        .merge(setup::router())
        .merge(cli_auth::router())
        .merge(commands::router())
        .merge(downloads::router())
        .merge(health::router())
        .merge(llm_providers::router())
        .merge(mesh::router())
        .merge(crate::git::browser_router())
}
