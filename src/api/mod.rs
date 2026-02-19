pub mod admin;
pub mod issues;
pub mod merge_requests;
pub mod pipelines;
pub mod projects;
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
        .merge(sessions::router())
        .merge(crate::git::browser_router())
}
