pub mod admin;
pub mod issues;
pub mod merge_requests;
pub mod notifications;
pub mod pipelines;
pub mod projects;
pub mod secrets;
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
        .merge(secrets::router())
        .merge(notifications::router())
        .merge(crate::git::browser_router())
}
