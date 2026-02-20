pub mod admin;
pub mod deployments;
pub mod helpers;
pub mod issues;
pub mod merge_requests;
pub mod notifications;
pub mod passkeys;
pub mod pipelines;
pub mod projects;
pub mod secrets;
pub mod sessions;
pub mod users;
pub mod webhooks;

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
        .merge(crate::git::browser_router())
}
