use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use uuid::Uuid;

use super::auth::{OptionalRegistryUser, RegistryUser};
use super::digest::{Digest, sha256_digest};
use super::error::RegistryError;
use super::types::OciManifest;
use super::{RepoAccess, resolve_repo_with_access, resolve_repo_with_optional_access};
use crate::store::AppState;

// ---------------------------------------------------------------------------
// HEAD /v2/{name}/manifests/{reference}
// ---------------------------------------------------------------------------

pub async fn head_manifest(
    State(state): State<AppState>,
    OptionalRegistryUser(user): OptionalRegistryUser,
    Path((name, reference)): Path<(String, String)>,
) -> Result<Response, RegistryError> {
    let RepoAccess {
        repository_id,
        project_id: _,
    } = resolve_repo_with_optional_access(&state, user.as_ref(), &name, false).await?;

    let manifest = resolve_manifest(&state.pool, repository_id, &reference).await?;

    let mut headers = HeaderMap::new();
    headers.insert("docker-content-digest", header_val(&manifest.digest));
    headers.insert(
        "content-length",
        header_val(&manifest.size_bytes.to_string()),
    );
    headers.insert("content-type", header_val(&manifest.media_type));

    Ok((StatusCode::OK, headers).into_response())
}

// ---------------------------------------------------------------------------
// GET /v2/{name}/manifests/{reference}
// ---------------------------------------------------------------------------

pub async fn get_manifest(
    State(state): State<AppState>,
    OptionalRegistryUser(user): OptionalRegistryUser,
    Path((name, reference)): Path<(String, String)>,
) -> Result<Response, RegistryError> {
    let RepoAccess {
        repository_id,
        project_id: _,
    } = resolve_repo_with_optional_access(&state, user.as_ref(), &name, false).await?;

    let manifest = resolve_manifest(&state.pool, repository_id, &reference).await?;

    let mut headers = HeaderMap::new();
    headers.insert("docker-content-digest", header_val(&manifest.digest));
    headers.insert(
        "content-length",
        header_val(&manifest.size_bytes.to_string()),
    );
    headers.insert("content-type", header_val(&manifest.media_type));

    Ok((StatusCode::OK, headers, manifest.content).into_response())
}

// ---------------------------------------------------------------------------
// PUT /v2/{name}/manifests/{reference}
// ---------------------------------------------------------------------------

pub async fn put_manifest(
    State(state): State<AppState>,
    user: RegistryUser,
    Path((name, reference)): Path<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, RegistryError> {
    let RepoAccess {
        repository_id,
        project_id: _,
    } = resolve_repo_with_access(&state, &user, &name, true).await?;

    // Enforce tag pattern restriction from scoped tokens
    if let Some(ref pattern) = user.registry_tag_pattern {
        let full_ref = format!("{name}:{reference}");
        if !super::matches_tag_pattern(&full_ref, pattern) {
            return Err(RegistryError::Denied);
        }
    }

    let content = body.to_vec();

    // Compute digest of the raw manifest bytes
    let digest = sha256_digest(&content);
    let digest_str = digest.as_str();
    let size_bytes = i64::try_from(content.len())
        .map_err(|e| RegistryError::Internal(anyhow::anyhow!("content length overflow: {e}")))?;

    // Determine media type from Content-Type header or default
    let media_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/vnd.oci.image.manifest.v1+json")
        .to_string();

    // Parse the manifest to validate and extract referenced blobs
    let manifest: OciManifest = serde_json::from_slice(&content)
        .map_err(|e| RegistryError::ManifestInvalid(format!("invalid JSON: {e}")))?;

    // Verify all referenced blobs exist and are linked to this repository
    verify_blob_references(&state, repository_id, &manifest).await?;

    // Upsert manifest
    sqlx::query!(
        r#"INSERT INTO registry_manifests (repository_id, digest, media_type, content, size_bytes)
           VALUES ($1, $2, $3, $4, $5)
           ON CONFLICT (repository_id, digest)
           DO UPDATE SET media_type = $3, content = $4, size_bytes = $5"#,
        repository_id,
        digest_str,
        media_type,
        content,
        size_bytes,
    )
    .execute(&state.pool)
    .await?;

    // If the reference is not a digest, treat it as a tag
    if Digest::parse(&reference).is_err() {
        sqlx::query!(
            r#"INSERT INTO registry_tags (repository_id, name, manifest_digest)
               VALUES ($1, $2, $3)
               ON CONFLICT (repository_id, name)
               DO UPDATE SET manifest_digest = $3, updated_at = now()"#,
            repository_id,
            reference,
            digest_str,
        )
        .execute(&state.pool)
        .await?;
    }

    let mut resp_headers = HeaderMap::new();
    resp_headers.insert(
        "location",
        header_val(&format!("/v2/{name}/manifests/{digest_str}")),
    );
    resp_headers.insert("docker-content-digest", header_val(&digest_str));

    Ok((StatusCode::CREATED, resp_headers).into_response())
}

// ---------------------------------------------------------------------------
// DELETE /v2/{name}/manifests/{reference}
// ---------------------------------------------------------------------------

pub async fn delete_manifest(
    State(state): State<AppState>,
    user: RegistryUser,
    Path((name, reference)): Path<(String, String)>,
) -> Result<Response, RegistryError> {
    let RepoAccess {
        repository_id,
        project_id: _,
    } = resolve_repo_with_access(&state, &user, &name, true).await?;

    // Resolve reference to digest
    let digest_str = if Digest::parse(&reference).is_ok() {
        reference.clone()
    } else {
        // It's a tag — resolve to digest
        sqlx::query_scalar!(
            "SELECT manifest_digest FROM registry_tags WHERE repository_id = $1 AND name = $2",
            repository_id,
            reference,
        )
        .fetch_optional(&state.pool)
        .await?
        .ok_or(RegistryError::ManifestUnknown)?
    };

    // Delete tags pointing to this manifest
    sqlx::query!(
        "DELETE FROM registry_tags WHERE repository_id = $1 AND manifest_digest = $2",
        repository_id,
        digest_str,
    )
    .execute(&state.pool)
    .await?;

    // Delete the manifest
    let result = sqlx::query!(
        "DELETE FROM registry_manifests WHERE repository_id = $1 AND digest = $2",
        repository_id,
        digest_str,
    )
    .execute(&state.pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(RegistryError::ManifestUnknown);
    }

    Ok(StatusCode::ACCEPTED.into_response())
}

// ---------------------------------------------------------------------------
// Namespaced wrappers (two-segment: {ns}/{repo})
// ---------------------------------------------------------------------------

pub async fn head_manifest_ns(
    state: State<AppState>,
    user: OptionalRegistryUser,
    Path((ns, repo, reference)): Path<(String, String, String)>,
) -> Result<Response, RegistryError> {
    head_manifest(state, user, Path((format!("{ns}/{repo}"), reference))).await
}

pub async fn get_manifest_ns(
    state: State<AppState>,
    user: OptionalRegistryUser,
    Path((ns, repo, reference)): Path<(String, String, String)>,
) -> Result<Response, RegistryError> {
    get_manifest(state, user, Path((format!("{ns}/{repo}"), reference))).await
}

pub async fn put_manifest_ns(
    state: State<AppState>,
    user: RegistryUser,
    Path((ns, repo, reference)): Path<(String, String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, RegistryError> {
    put_manifest(
        state,
        user,
        Path((format!("{ns}/{repo}"), reference)),
        headers,
        body,
    )
    .await
}

pub async fn delete_manifest_ns(
    state: State<AppState>,
    user: RegistryUser,
    Path((ns, repo, reference)): Path<(String, String, String)>,
) -> Result<Response, RegistryError> {
    delete_manifest(state, user, Path((format!("{ns}/{repo}"), reference))).await
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct ManifestRow {
    digest: String,
    media_type: String,
    content: Vec<u8>,
    size_bytes: i64,
}

async fn resolve_manifest(
    pool: &sqlx::PgPool,
    repository_id: Uuid,
    reference: &str,
) -> Result<ManifestRow, RegistryError> {
    // Try as digest first
    if Digest::parse(reference).is_ok() {
        return sqlx::query_as!(
            ManifestRow,
            r#"SELECT digest, media_type, content, size_bytes
               FROM registry_manifests
               WHERE repository_id = $1 AND digest = $2"#,
            repository_id,
            reference,
        )
        .fetch_optional(pool)
        .await?
        .ok_or(RegistryError::ManifestUnknown);
    }

    // Try as tag
    let tag = sqlx::query_scalar!(
        "SELECT manifest_digest FROM registry_tags WHERE repository_id = $1 AND name = $2",
        repository_id,
        reference,
    )
    .fetch_optional(pool)
    .await?
    .ok_or(RegistryError::ManifestUnknown)?;

    sqlx::query_as!(
        ManifestRow,
        r#"SELECT digest, media_type, content, size_bytes
           FROM registry_manifests
           WHERE repository_id = $1 AND digest = $2"#,
        repository_id,
        tag,
    )
    .fetch_optional(pool)
    .await?
    .ok_or(RegistryError::ManifestUnknown)
}

/// Verify that all blobs referenced by a manifest exist and are linked to the repository.
async fn verify_blob_references(
    state: &AppState,
    repository_id: Uuid,
    manifest: &OciManifest,
) -> Result<(), RegistryError> {
    let mut digests = Vec::new();

    if let Some(ref config) = manifest.config {
        digests.push(&config.digest);
    }

    if let Some(ref layers) = manifest.layers {
        for layer in layers {
            digests.push(&layer.digest);
        }
    }

    // For image indexes, the referenced manifests are checked as manifests, not blobs.
    // We don't verify sub-manifests here since they may be pushed later or in any order.

    for digest_str in digests {
        // Verify the blob exists and is linked to this repository
        let exists = sqlx::query_scalar!(
            r#"SELECT EXISTS(
                SELECT 1 FROM registry_blob_links
                WHERE repository_id = $1 AND blob_digest = $2
            ) as "exists!: bool""#,
            repository_id,
            digest_str,
        )
        .fetch_one(&state.pool)
        .await?;

        if !exists {
            return Err(RegistryError::ManifestInvalid(format!(
                "referenced blob {digest_str} not found in repository"
            )));
        }
    }

    Ok(())
}

fn header_val(s: &str) -> HeaderValue {
    HeaderValue::from_str(s).unwrap_or_else(|_| HeaderValue::from_static(""))
}
