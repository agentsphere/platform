// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Git HTTP router — combines smart HTTP, LFS, and browser routes.
//!
//! Smart HTTP and LFS routes use git's own Basic Auth (handled inline).
//! Browser routes require the platform's `AuthUser` auth, converted to
//! `BrowserUser` via a middleware layer.

use axum::extract::{FromRequestParts, State};
use axum::{Router, middleware};

use platform_git::BrowserUser;
use platform_types::{ApiError, AuthUser};

use crate::state::PlatformState;

/// Build the combined git HTTP router, finalized with `.with_state()`.
///
/// Returns `Router<()>` segments ready to merge into the `PlatformState` router.
pub fn router(state: &PlatformState) -> Router {
    let git_state = state.git_state();

    // Smart HTTP + LFS: Basic Auth handled inline by git handlers
    let transport = Router::new()
        .merge(platform_git::smart_http::router())
        .merge(platform_git::lfs::router())
        .with_state(git_state.clone());

    // Browser API: needs auth middleware to inject BrowserUser from AuthUser
    let browser = platform_git::browser::router()
        .layer(middleware::from_fn_with_state(
            state.clone(),
            inject_browser_user,
        ))
        .with_state(git_state);

    Router::new().merge(transport).merge(browser)
}

/// Middleware that extracts `AuthUser` from the platform's auth system and
/// injects a `BrowserUser` into request extensions for the browser routes.
async fn inject_browser_user(
    State(state): State<PlatformState>,
    request: axum::extract::Request,
    next: middleware::Next,
) -> Result<axum::response::Response, ApiError> {
    // Extract AuthUser using the platform binary's FromRequestParts impl
    let (mut parts, body) = request.into_parts();
    let auth = AuthUser::from_request_parts(&mut parts, &state).await?;

    let browser_user = BrowserUser {
        user_id: auth.user_id,
        token_scopes: auth.token_scopes,
        boundary_project_id: auth.boundary_project_id,
        boundary_workspace_id: auth.boundary_workspace_id,
    };

    let mut request = axum::extract::Request::from_parts(parts, body);
    request.extensions_mut().insert(browser_user);
    Ok(next.run(request).await)
}
