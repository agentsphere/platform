pub mod browser;
pub mod hooks;
pub mod lfs;
pub mod repo;
pub mod smart_http;

use axum::Router;

use crate::store::AppState;

/// Smart HTTP + LFS routes. Mounted at root level in `main.rs`.
/// Matches `/:owner/:repo/*` patterns (not under `/api/`).
pub fn git_protocol_router() -> Router<AppState> {
    Router::new()
        .merge(smart_http::router())
        .merge(lfs::router())
}

/// Repository browser API routes. Mounted via `api::router()`.
/// Matches `/api/projects/:id/{tree,blob,branches,commits}`.
pub fn browser_router() -> Router<AppState> {
    browser::router()
}
