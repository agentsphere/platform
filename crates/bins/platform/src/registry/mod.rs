// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! OCI container registry HTTP handlers — push, pull, manifest, and tag operations.
//!
//! Implements the OCI Distribution Spec v2 endpoints (`/v2/...`). Core logic
//! (access control, types, digests, error mapping) lives in the `platform-registry`
//! library crate; this module provides the axum HTTP handlers wired to
//! `PlatformState`.

pub mod auth;
pub mod blobs;
pub mod manifests;
pub mod tags;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, head, patch, post};

use platform_registry::{RegistryError, RegistryUser, RepoAccess};

use crate::state::PlatformState;

// ---------------------------------------------------------------------------
// Access helpers — construct permission + workspace checkers from PlatformState
// ---------------------------------------------------------------------------

pub(crate) async fn resolve_access(
    state: &PlatformState,
    user: &RegistryUser,
    name: &str,
    need_push: bool,
) -> Result<RepoAccess, RegistryError> {
    let perm =
        crate::git_services::AppPermissionChecker::new(state.pool.clone(), state.valkey.clone());
    let ws = platform_auth::PgWorkspaceMembershipChecker::new(&state.pool);
    platform_registry::access::resolve_repo_with_access(
        &state.pool,
        &perm,
        &ws,
        user,
        name,
        need_push,
    )
    .await
}

pub(crate) async fn resolve_optional_access(
    state: &PlatformState,
    user: Option<&RegistryUser>,
    name: &str,
    need_push: bool,
) -> Result<RepoAccess, RegistryError> {
    let perm =
        crate::git_services::AppPermissionChecker::new(state.pool.clone(), state.valkey.clone());
    let ws = platform_auth::PgWorkspaceMembershipChecker::new(&state.pool);
    platform_registry::access::resolve_repo_with_optional_access(
        &state.pool,
        &perm,
        &ws,
        user,
        name,
        need_push,
    )
    .await
}

/// Build the OCI registry router (all `/v2/` routes).
pub fn router() -> Router<PlatformState> {
    Router::new()
        // Version check
        .route("/v2/", get(version_check))
        // Blob operations — single-segment name
        .route(
            "/v2/{name}/blobs/{digest}",
            head(blobs::head_blob).get(blobs::get_blob),
        )
        .route("/v2/{name}/blobs/uploads/", post(blobs::start_upload))
        .route(
            "/v2/{name}/blobs/uploads/{uuid}",
            patch(blobs::upload_chunk).put(blobs::complete_upload),
        )
        // Manifest operations — single-segment name
        .route(
            "/v2/{name}/manifests/{reference}",
            head(manifests::head_manifest)
                .get(manifests::get_manifest)
                .put(manifests::put_manifest)
                .delete(manifests::delete_manifest),
        )
        // Tag listing — single-segment name
        .route("/v2/{name}/tags/list", get(tags::list_tags))
        // --- Two-segment namespaced routes (e.g. project/app) ---
        .route(
            "/v2/{ns}/{repo}/blobs/{digest}",
            head(blobs::head_blob_ns).get(blobs::get_blob_ns),
        )
        .route(
            "/v2/{ns}/{repo}/blobs/uploads/",
            post(blobs::start_upload_ns),
        )
        .route(
            "/v2/{ns}/{repo}/blobs/uploads/{uuid}",
            patch(blobs::upload_chunk_ns).put(blobs::complete_upload_ns),
        )
        .route(
            "/v2/{ns}/{repo}/manifests/{reference}",
            head(manifests::head_manifest_ns)
                .get(manifests::get_manifest_ns)
                .put(manifests::put_manifest_ns)
                .delete(manifests::delete_manifest_ns),
        )
        .route("/v2/{ns}/{repo}/tags/list", get(tags::list_tags_ns))
}

/// GET /v2/ — OCI version check (always returns 200).
async fn version_check(State(_state): State<PlatformState>) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        "docker-distribution-api-version",
        HeaderValue::from_static("registry/2.0"),
    );
    (StatusCode::OK, headers).into_response()
}

/// Create a `HeaderValue` from a string, falling back to "unknown" on invalid chars.
pub(crate) fn header_val(s: &str) -> HeaderValue {
    HeaderValue::from_str(s).unwrap_or_else(|_| HeaderValue::from_static("unknown"))
}
