pub mod admin;
pub mod cli_auth;
pub mod commands;
pub mod dashboard;
pub mod deployments;
pub mod gpg_keys;
pub mod helpers;
pub mod issues;
pub mod merge_requests;
pub mod notifications;
pub mod passkeys;
pub mod pipelines;
pub mod projects;
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
        .merge(sessions::router())
        .merge(secrets::router())
        .merge(notifications::router())
        .merge(passkeys::router())
        .merge(user_keys::router())
        .merge(ssh_keys::router())
        .merge(gpg_keys::router())
        .merge(workspaces::router())
        .merge(dashboard::router())
        .merge(setup::router())
        .merge(cli_auth::router())
        .merge(commands::router())
        .merge(crate::git::browser_router())
}
