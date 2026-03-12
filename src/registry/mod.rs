pub mod auth;
pub mod blobs;
pub mod digest;
pub mod error;
pub mod gc;
pub mod manifests;
pub mod pull_secret;
pub mod seed;
pub mod tags;
pub mod types;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, head, patch, post};
use uuid::Uuid;

use self::auth::{OptionalRegistryUser, RegistryUser};
use self::error::RegistryError;
use crate::rbac::{Permission, resolver};
use crate::store::AppState;
use crate::workspace;

/// Resolved repository access — returned by `resolve_repo_with_access`.
pub struct RepoAccess {
    pub repository_id: Uuid,
    pub project_id: Uuid,
}

/// Resolved project info from a repository lookup.
struct RepoProject {
    repository_id: Uuid,
    project_id: Uuid,
    owner_id: Uuid,
    workspace_id: Uuid,
    visibility: String,
}

/// Look up a repository by name, joining to its parent project.
///
/// The URL path segment is a **repository** name (which may differ from
/// the project name, e.g. `platform-runner-bare` repo under `platform-runner` project).
async fn lookup_repo_and_project(
    pool: &sqlx::PgPool,
    name: &str,
) -> Result<Option<RepoProject>, sqlx::Error> {
    sqlx::query_as!(
        RepoProject,
        r#"SELECT r.id AS "repository_id!", p.id AS "project_id!",
                  p.owner_id AS "owner_id!", p.workspace_id AS "workspace_id!",
                  p.visibility AS "visibility!"
           FROM registry_repositories r
           JOIN projects p ON p.id = r.project_id AND p.is_active = true
           WHERE r.name = $1"#,
        name,
    )
    .fetch_optional(pool)
    .await
}

/// Resolve a repository name to a project, checking ownership and permissions.
///
/// Looks up by repository name first (supports multi-repo projects like
/// `platform-runner-bare` under project `platform-runner`). Falls back to
/// project-name lookup with lazy repo creation for push operations.
///
/// Returns 404 (not 403) if user lacks access to avoid leaking existence.
pub async fn resolve_repo_with_access(
    state: &AppState,
    user: &RegistryUser,
    name: &str,
    need_push: bool,
) -> Result<RepoAccess, RegistryError> {
    // 1. Try to find existing repository → project
    let resolved = lookup_repo_and_project(&state.pool, name).await?;

    let (repository_id, project_id, owner_id, workspace_id) = if let Some(rp) = resolved {
        (
            Some(rp.repository_id),
            rp.project_id,
            rp.owner_id,
            rp.workspace_id,
        )
    } else if need_push {
        // No repo found — for push, fall back to project-name lookup + lazy-create.
        // For namespaced names like "project/dev", use the first segment as project name.
        let project_lookup_name = if let Some(slash) = name.find('/') {
            &name[..slash]
        } else {
            name
        };
        let project = sqlx::query!(
            r#"SELECT id, owner_id, workspace_id
               FROM projects
               WHERE name = $1 AND is_active = true"#,
            project_lookup_name,
        )
        .fetch_optional(&state.pool)
        .await?
        .ok_or(RegistryError::NameUnknown)?;
        (None, project.id, project.owner_id, project.workspace_id)
    } else {
        // Pull with no existing repo — nothing to pull
        return Err(RegistryError::NameUnknown);
    };

    // Enforce hard project boundary from API token
    if let Some(boundary_pid) = user.boundary_project_id
        && boundary_pid != project_id
    {
        return Err(RegistryError::NameUnknown);
    }

    // Enforce hard workspace boundary from API token
    if let Some(boundary_wid) = user.boundary_workspace_id
        && workspace_id != boundary_wid
    {
        return Err(RegistryError::NameUnknown);
    }

    // 2. Owner always has full access
    let is_owner = owner_id == user.user_id;

    if !is_owner {
        // 3. Check workspace membership
        let is_workspace_member =
            workspace::service::is_member(&state.pool, workspace_id, user.user_id)
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

    // 4. Get or create the repository
    let repository_id = if let Some(id) = repository_id {
        id
    } else {
        // Lazy-create on first push (only reachable when need_push && no repo found)
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

/// Resolve a repository for anonymous or authenticated access.
///
/// When `user` is `None`, only public projects are accessible (pull only).
/// When `user` is `Some`, delegates to the full `resolve_repo_with_access`.
pub async fn resolve_repo_with_optional_access(
    state: &AppState,
    user: Option<&RegistryUser>,
    name: &str,
    need_push: bool,
) -> Result<RepoAccess, RegistryError> {
    if let Some(user) = user {
        return resolve_repo_with_access(state, user, name, need_push).await;
    }

    // Anonymous access: push is never allowed
    if need_push {
        return Err(RegistryError::Unauthorized);
    }

    // Look up repository → project (must exist and be public)
    let rp = lookup_repo_and_project(&state.pool, name)
        .await?
        .ok_or(RegistryError::NameUnknown)?;

    if rp.visibility != "public" {
        // Return 401 (not 404) so containerd/Docker retries with credentials
        // from imagePullSecrets. Returning 404 would make it give up immediately.
        return Err(RegistryError::Unauthorized);
    }

    Ok(RepoAccess {
        repository_id: rp.repository_id,
        project_id: rp.project_id,
    })
}

/// OCI Distribution Spec version check endpoint.
async fn version_check(
    State(_state): State<AppState>,
    _user: OptionalRegistryUser,
) -> Result<Response, RegistryError> {
    // Per OCI spec: return 200 {} to indicate the registry supports v2
    let mut headers = HeaderMap::new();
    headers.insert(
        "docker-distribution-api-version",
        HeaderValue::from_static("registry/2.0"),
    );
    Ok((StatusCode::OK, headers, "{}").into_response())
}

/// Check if an image reference (e.g. `"myapp-dev:session-abc"`) matches a glob pattern.
///
/// The pattern supports `*` as a wildcard matching any sequence of characters.
/// Returns `true` if pattern is `None` (no restriction).
pub fn matches_tag_pattern(image_ref: &str, pattern: &str) -> bool {
    glob_match(pattern, image_ref)
}

/// Simple glob match: `*` matches any sequence of characters. No other wildcards.
fn glob_match(pattern: &str, input: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == input;
    }

    let mut pos = 0;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if let Some(found) = input[pos..].find(part) {
            if i == 0 && found != 0 {
                return false; // first segment must be a prefix
            }
            pos += found + part.len();
        } else {
            return false;
        }
    }

    // If pattern doesn't end with *, input must be fully consumed
    if !pattern.ends_with('*') {
        return pos == input.len();
    }

    true
}

/// Copy a manifest from one tag to another within the same repository.
///
/// Metadata-only — blobs are shared via content-addressable storage.
/// Returns error if `dest_tag` already exists (immutable alias tags).
pub async fn copy_tag(
    pool: &sqlx::PgPool,
    repo_name: &str,
    source_tag: &str,
    dest_tag: &str,
) -> Result<(), error::RegistryError> {
    // Look up repository
    let repo = sqlx::query_scalar!(
        "SELECT id FROM registry_repositories WHERE name = $1",
        repo_name,
    )
    .fetch_optional(pool)
    .await?
    .ok_or(error::RegistryError::NameUnknown)?;

    // Get digest for source tag
    let source_digest = sqlx::query_scalar!(
        "SELECT manifest_digest FROM registry_tags WHERE repository_id = $1 AND name = $2",
        repo,
        source_tag,
    )
    .fetch_optional(pool)
    .await?
    .ok_or(error::RegistryError::ManifestUnknown)?;

    // Check dest_tag doesn't already exist
    let existing = sqlx::query_scalar!(
        "SELECT manifest_digest FROM registry_tags WHERE repository_id = $1 AND name = $2",
        repo,
        dest_tag,
    )
    .fetch_optional(pool)
    .await?;

    if existing.is_some() {
        return Err(error::RegistryError::TagExists(dest_tag.to_string()));
    }

    // Create tag pointing to the same digest
    sqlx::query!(
        "INSERT INTO registry_tags (repository_id, name, manifest_digest) VALUES ($1, $2, $3)",
        repo,
        dest_tag,
        source_digest,
    )
    .execute(pool)
    .await?;

    Ok(())
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
        // --- Two-segment namespaced routes (e.g. project/app, project/dev) ---
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_tag_pattern_exact() {
        assert!(matches_tag_pattern(
            "myapp-dev:session-abc",
            "myapp-dev:session-*"
        ));
    }

    #[test]
    fn matches_tag_pattern_rejects_other_tag() {
        assert!(!matches_tag_pattern("myapp:latest", "myapp-dev:session-*"));
    }

    #[test]
    fn matches_tag_pattern_rejects_other_repo() {
        assert!(!matches_tag_pattern(
            "other-dev:session-abc",
            "myapp-dev:session-*"
        ));
    }

    #[test]
    fn matches_tag_pattern_wildcard_suffix() {
        assert!(matches_tag_pattern(
            "myapp-dev:session-abc12345-build1",
            "myapp-dev:session-abc12345-*"
        ));
    }

    #[test]
    fn matches_tag_pattern_no_wildcard_exact() {
        assert!(matches_tag_pattern("myapp:v1", "myapp:v1"));
        assert!(!matches_tag_pattern("myapp:v2", "myapp:v1"));
    }
}
