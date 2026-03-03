pub mod auth;
pub mod blobs;
pub mod digest;
pub mod error;
pub mod gc;
pub mod manifests;
pub mod pull_secret;
pub mod tags;
pub mod types;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, head, patch, post};
use uuid::Uuid;

use self::auth::RegistryUser;
use self::error::RegistryError;
use crate::rbac::{Permission, resolver};
use crate::store::AppState;
use crate::workspace;

/// Resolved repository access — returned by `resolve_repo_with_access`.
pub struct RepoAccess {
    pub repository_id: Uuid,
    pub project_id: Uuid,
}

/// Resolve a repository name to a project, checking ownership and permissions.
///
/// Ownership chain: image → `registry_repository` → project → workspace → owner
///
/// Returns 404 (not 403) if user lacks access to avoid leaking existence.
pub async fn resolve_repo_with_access(
    state: &AppState,
    user: &RegistryUser,
    name: &str,
    need_push: bool,
) -> Result<RepoAccess, RegistryError> {
    // 1. Find the project by name (must be active)
    let project = sqlx::query!(
        r#"SELECT id, owner_id, workspace_id, visibility
           FROM projects
           WHERE name = $1 AND is_active = true"#,
        name,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or(RegistryError::NameUnknown)?;

    let project_id = project.id;

    // Enforce hard project scope from API token
    if let Some(scope_pid) = user.scope_project_id
        && scope_pid != project_id
    {
        return Err(RegistryError::NameUnknown);
    }

    // Enforce hard workspace scope from API token
    if let Some(scope_wid) = user.scope_workspace_id
        && project.workspace_id != scope_wid
    {
        return Err(RegistryError::NameUnknown);
    }

    // 2. Owner always has full access
    let is_owner = project.owner_id == user.user_id;

    if !is_owner {
        // 3. Check workspace membership
        let is_workspace_member =
            workspace::service::is_member(&state.pool, project.workspace_id, user.user_id)
                .await
                .map_err(|e| RegistryError::Internal(anyhow::anyhow!("{e}")))?;

        // Workspace members get implicit pull access.
        // Push always requires explicit RBAC permission.
        let needs_rbac_check = need_push || !is_workspace_member;

        if needs_rbac_check {
            let perm = if need_push {
                Permission::RegistryPush
            } else {
                Permission::RegistryPull
            };
            let allowed = resolver::has_permission(
                &state.pool,
                &state.valkey,
                user.user_id,
                Some(project_id),
                perm,
            )
            .await
            .map_err(RegistryError::Internal)?;

            if !allowed {
                return Err(RegistryError::NameUnknown); // 404, not 403
            }
        }
    }

    // 5. Lazily create registry_repository if it doesn't exist
    let repo = sqlx::query_scalar!(
        r#"SELECT id FROM registry_repositories WHERE name = $1"#,
        name,
    )
    .fetch_optional(&state.pool)
    .await?;

    let repository_id = if let Some(id) = repo {
        id
    } else {
        if !need_push {
            // For pull operations, if no repo exists, there's nothing to pull
            return Err(RegistryError::NameUnknown);
        }

        // Create the repository lazily on first push
        let id = Uuid::new_v4();
        sqlx::query!(
            r#"INSERT INTO registry_repositories (id, project_id, name)
               VALUES ($1, $2, $3)
               ON CONFLICT (name) DO UPDATE SET updated_at = now()
               RETURNING id"#,
            id,
            project_id,
            name,
        )
        .fetch_one(&state.pool)
        .await?
        .id
    };

    Ok(RepoAccess {
        repository_id,
        project_id,
    })
}

/// OCI Distribution Spec version check endpoint.
async fn version_check(
    State(_state): State<AppState>,
    _user: RegistryUser,
) -> Result<Response, RegistryError> {
    // Per OCI spec: return 200 {} to indicate the registry supports v2
    let mut headers = HeaderMap::new();
    headers.insert(
        "docker-distribution-api-version",
        HeaderValue::from_static("registry/2.0"),
    );
    Ok((StatusCode::OK, headers, "{}").into_response())
}

pub fn router() -> Router<AppState> {
    Router::new()
        // Version check
        .route("/v2/", get(version_check))
        // Blob operations
        .route(
            "/v2/{name}/blobs/{digest}",
            head(blobs::head_blob).get(blobs::get_blob),
        )
        .route("/v2/{name}/blobs/uploads/", post(blobs::start_upload))
        .route(
            "/v2/{name}/blobs/uploads/{uuid}",
            patch(blobs::upload_chunk).put(blobs::complete_upload),
        )
        // Manifest operations
        .route(
            "/v2/{name}/manifests/{reference}",
            head(manifests::head_manifest)
                .get(manifests::get_manifest)
                .put(manifests::put_manifest)
                .delete(manifests::delete_manifest),
        )
        // Tag listing
        .route("/v2/{name}/tags/list", get(tags::list_tags))
}
