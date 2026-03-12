pub mod browser;
pub mod gpg_keys;
pub mod hooks;
pub mod lfs;
pub mod protection;
pub mod repo;
pub mod signature;
pub mod smart_http;
pub mod ssh_keys;
pub mod ssh_server;
pub mod templates;

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
