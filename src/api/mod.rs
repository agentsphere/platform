pub mod admin;
pub mod issues;
pub mod merge_requests;
pub mod projects;
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
        .merge(crate::git::browser_router())
}
