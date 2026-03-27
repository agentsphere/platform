use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use fred::interfaces::KeysInterface;
use serde::Deserialize;
use uuid::Uuid;

use super::auth::{OptionalRegistryUser, RegistryUser};
use super::digest::{Digest, sha256_digest};
use super::error::RegistryError;
use super::types::UploadSession;
use super::{RepoAccess, resolve_repo_with_access, resolve_repo_with_optional_access};
use crate::store::AppState;

// ---------------------------------------------------------------------------
// HEAD /v2/{name}/blobs/{digest}
// ---------------------------------------------------------------------------

pub async fn head_blob(
    State(state): State<AppState>,
    OptionalRegistryUser(user): OptionalRegistryUser,
    Path((name, digest_str)): Path<(String, String)>,
) -> Result<Response, RegistryError> {
    let RepoAccess {
        repository_id,
        project_id: _,
    } = resolve_repo_with_optional_access(&state, user.as_ref(), &name, false).await?;

    let digest = Digest::parse(&digest_str)?;

    let blob = sqlx::query!(
        r#"SELECT b.size_bytes FROM registry_blobs b
           JOIN registry_blob_links bl ON bl.blob_digest = b.digest
           WHERE b.digest = $1 AND bl.repository_id = $2"#,
        digest.as_str(),
        repository_id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or(RegistryError::BlobUnknown)?;

    let mut headers = HeaderMap::new();
    headers.insert("docker-content-digest", header_val(&digest.as_str()));
    headers.insert("content-length", header_val(&blob.size_bytes.to_string()));
    headers.insert(
        "content-type",
        HeaderValue::from_static("application/octet-stream"),
    );

    Ok((StatusCode::OK, headers).into_response())
}

// ---------------------------------------------------------------------------
// GET /v2/{name}/blobs/{digest}
// ---------------------------------------------------------------------------

pub async fn get_blob(
    State(state): State<AppState>,
    OptionalRegistryUser(user): OptionalRegistryUser,
    Path((name, digest_str)): Path<(String, String)>,
) -> Result<Response, RegistryError> {
    let RepoAccess {
        repository_id,
        project_id: _,
    } = resolve_repo_with_optional_access(&state, user.as_ref(), &name, false).await?;

    let digest = Digest::parse(&digest_str)?;

    let blob = sqlx::query!(
        r#"SELECT b.size_bytes, b.minio_path FROM registry_blobs b
           JOIN registry_blob_links bl ON bl.blob_digest = b.digest
           WHERE b.digest = $1 AND bl.repository_id = $2"#,
        digest.as_str(),
        repository_id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or(RegistryError::BlobUnknown)?;

    let mut headers = HeaderMap::new();
    headers.insert("docker-content-digest", header_val(&digest.as_str()));

    if state.config.registry_proxy_blobs {
        // Stream blob through the platform — needed when MinIO is not directly
        // reachable from clients (e.g. kaniko pods in Kind clusters).
        let data = state.minio.read(&blob.minio_path).await?;
        headers.insert("content-length", HeaderValue::from(blob.size_bytes));
        headers.insert(
            "content-type",
            HeaderValue::from_static("application/octet-stream"),
        );
        Ok((StatusCode::OK, headers, data.to_vec()).into_response())
    } else {
        // A21: presigned URL redirect (default — avoids loading blobs into memory)
        let presigned = state
            .minio
            .presign_read(&blob.minio_path, Duration::from_secs(300))
            .await?;
        headers.insert("location", header_val(&presigned.uri().to_string()));
        Ok((StatusCode::TEMPORARY_REDIRECT, headers).into_response())
    }
}

// ---------------------------------------------------------------------------
// POST /v2/{name}/blobs/uploads/
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct UploadQuery {
    pub digest: Option<String>,
}

pub async fn start_upload(
    State(state): State<AppState>,
    user: RegistryUser,
    Path(name): Path<String>,
    Query(query): Query<UploadQuery>,
    body: axum::body::Bytes,
) -> Result<Response, RegistryError> {
    let RepoAccess {
        repository_id,
        project_id,
    } = resolve_repo_with_access(&state, &user, &name, true).await?;

    // Monolithic upload: POST with ?digest=sha256:...
    if let Some(ref digest_str) = query.digest {
        return complete_monolithic(&state, &name, repository_id, digest_str, body.to_vec()).await;
    }

    // Create upload session in Valkey
    let upload_id = Uuid::new_v4();
    let session = UploadSession {
        repository_id: repository_id.to_string(),
        project_id: project_id.map(|id| id.to_string()).unwrap_or_default(),
        user_id: user.user_id.to_string(),
        offset: 0,
        part_count: 0,
    };

    let session_json =
        serde_json::to_string(&session).map_err(|e| RegistryError::Internal(e.into()))?;

    let key = upload_key(&upload_id);
    let _: () = state
        .valkey
        .set(&key, session_json.as_str(), None, None, false)
        .await
        .map_err(|e| RegistryError::Internal(e.into()))?;
    // TTL: 1 hour
    let _: () = state
        .valkey
        .expire(&key, 3600, None)
        .await
        .map_err(|e| RegistryError::Internal(e.into()))?;

    let location = format!("/v2/{name}/blobs/uploads/{upload_id}");
    let mut headers = HeaderMap::new();
    headers.insert("location", header_val(&location));
    headers.insert("docker-upload-uuid", header_val(&upload_id.to_string()));
    headers.insert("range", HeaderValue::from_static("0-0"));

    Ok((StatusCode::ACCEPTED, headers).into_response())
}

// ---------------------------------------------------------------------------
// PATCH /v2/{name}/blobs/uploads/{uuid}
// ---------------------------------------------------------------------------

pub async fn upload_chunk(
    State(state): State<AppState>,
    user: RegistryUser,
    Path((name, upload_id)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Result<Response, RegistryError> {
    let _access = resolve_repo_with_access(&state, &user, &name, true).await?;

    let upload_uuid: Uuid = upload_id
        .parse()
        .map_err(|_| RegistryError::BlobUploadUnknown)?;

    let key = upload_key(&upload_uuid);
    let session_json: Option<String> = state
        .valkey
        .get(&key)
        .await
        .map_err(|e| RegistryError::Internal(e.into()))?;
    let session_json = session_json.ok_or(RegistryError::BlobUploadUnknown)?;
    let mut session: UploadSession =
        serde_json::from_str(&session_json).map_err(|e| RegistryError::Internal(e.into()))?;

    // Verify the session belongs to this user
    if session.user_id != user.user_id.to_string() {
        return Err(RegistryError::BlobUploadUnknown);
    }

    let chunk_data = body.to_vec();
    let chunk_size = i64::try_from(chunk_data.len())
        .map_err(|e| RegistryError::Internal(anyhow::anyhow!("chunk size overflow: {e}")))?;

    // Write chunk to MinIO
    let part_path = format!("registry/uploads/{upload_uuid}/part-{}", session.part_count);
    state.minio.write(&part_path, chunk_data).await?;

    session.offset += chunk_size;
    session.part_count += 1;

    let updated_json =
        serde_json::to_string(&session).map_err(|e| RegistryError::Internal(e.into()))?;
    let _: () = state
        .valkey
        .set(&key, updated_json.as_str(), None, None, false)
        .await
        .map_err(|e| RegistryError::Internal(e.into()))?;
    let _: () = state
        .valkey
        .expire(&key, 3600, None)
        .await
        .map_err(|e| RegistryError::Internal(e.into()))?;

    let location = format!("/v2/{name}/blobs/uploads/{upload_uuid}");
    let range = format!("0-{}", session.offset.saturating_sub(1).max(0));

    let mut headers = HeaderMap::new();
    headers.insert("location", header_val(&location));
    headers.insert("docker-upload-uuid", header_val(&upload_uuid.to_string()));
    headers.insert("range", header_val(&range));

    Ok((StatusCode::ACCEPTED, headers).into_response())
}

// ---------------------------------------------------------------------------
// PUT /v2/{name}/blobs/uploads/{uuid}?digest=sha256:...
// ---------------------------------------------------------------------------

pub async fn complete_upload(
    State(state): State<AppState>,
    user: RegistryUser,
    Path((name, upload_id)): Path<(String, String)>,
    Query(query): Query<UploadQuery>,
    body: axum::body::Bytes,
) -> Result<Response, RegistryError> {
    let _access = resolve_repo_with_access(&state, &user, &name, true).await?;

    let digest_str = query
        .digest
        .as_deref()
        .ok_or_else(|| RegistryError::DigestInvalid("missing digest query param".into()))?;
    let expected_digest = Digest::parse(digest_str)?;

    let upload_uuid: Uuid = upload_id
        .parse()
        .map_err(|_| RegistryError::BlobUploadUnknown)?;

    let key = upload_key(&upload_uuid);
    let session_json: Option<String> = state
        .valkey
        .get(&key)
        .await
        .map_err(|e| RegistryError::Internal(e.into()))?;
    let session_json = session_json.ok_or(RegistryError::BlobUploadUnknown)?;
    let session: UploadSession =
        serde_json::from_str(&session_json).map_err(|e| RegistryError::Internal(e.into()))?;

    if session.user_id != user.user_id.to_string() {
        return Err(RegistryError::BlobUploadUnknown);
    }

    // A14: Enforce maximum blob size limit
    let max_blob_size: u64 = 5_368_709_120; // 5 GB
    let total_size = u64::try_from(session.offset).unwrap_or(0) + body.len() as u64;
    if total_size > max_blob_size {
        return Err(RegistryError::BlobUploadInvalid(format!(
            "blob size {total_size} exceeds maximum {max_blob_size}"
        )));
    }

    let repo_id: Uuid = session
        .repository_id
        .parse()
        .map_err(|e: uuid::Error| RegistryError::Internal(e.into()))?;

    // Collect all data: existing parts + final body
    let mut full_data = Vec::new();
    for i in 0..session.part_count {
        let part_path = format!("registry/uploads/{upload_uuid}/part-{i}");
        let part = state.minio.read(&part_path).await?.to_vec();
        full_data.extend_from_slice(&part);
    }

    // Append final chunk if non-empty
    if !body.is_empty() {
        full_data.extend_from_slice(&body);
    }

    // Verify digest
    let actual_digest = sha256_digest(&full_data);
    if actual_digest != expected_digest {
        return Err(RegistryError::DigestInvalid(format!(
            "expected {expected_digest}, got {actual_digest}"
        )));
    }

    let size_bytes = i64::try_from(full_data.len())
        .map_err(|e| RegistryError::Internal(anyhow::anyhow!("data size overflow: {e}")))?;

    // Write to final path in MinIO
    let minio_path = expected_digest.minio_path();
    state.minio.write(&minio_path, full_data).await?;

    // Insert blob (ON CONFLICT: blob already exists, that's fine — content-addressable)
    sqlx::query!(
        r#"INSERT INTO registry_blobs (digest, size_bytes, minio_path)
           VALUES ($1, $2, $3)
           ON CONFLICT (digest) DO NOTHING"#,
        expected_digest.as_str(),
        size_bytes,
        minio_path,
    )
    .execute(&state.pool)
    .await?;

    // Link blob to repository
    sqlx::query!(
        r#"INSERT INTO registry_blob_links (repository_id, blob_digest)
           VALUES ($1, $2)
           ON CONFLICT DO NOTHING"#,
        repo_id,
        expected_digest.as_str(),
    )
    .execute(&state.pool)
    .await?;

    // Cleanup: delete session and temp parts
    cleanup_upload(&state, &upload_uuid, &session).await;

    let mut headers = HeaderMap::new();
    headers.insert(
        "location",
        header_val(&format!("/v2/{name}/blobs/{expected_digest}")),
    );
    headers.insert(
        "docker-content-digest",
        header_val(&expected_digest.as_str()),
    );

    Ok((StatusCode::CREATED, headers).into_response())
}

// ---------------------------------------------------------------------------
// Namespaced wrappers (two-segment: {ns}/{repo})
// ---------------------------------------------------------------------------

pub async fn head_blob_ns(
    state: State<AppState>,
    user: OptionalRegistryUser,
    Path((ns, repo, digest)): Path<(String, String, String)>,
) -> Result<Response, RegistryError> {
    head_blob(state, user, Path((format!("{ns}/{repo}"), digest))).await
}

pub async fn get_blob_ns(
    state: State<AppState>,
    user: OptionalRegistryUser,
    Path((ns, repo, digest)): Path<(String, String, String)>,
) -> Result<Response, RegistryError> {
    get_blob(state, user, Path((format!("{ns}/{repo}"), digest))).await
}

pub async fn start_upload_ns(
    state: State<AppState>,
    user: RegistryUser,
    Path((ns, repo)): Path<(String, String)>,
    query: Query<UploadQuery>,
    body: axum::body::Bytes,
) -> Result<Response, RegistryError> {
    start_upload(state, user, Path(format!("{ns}/{repo}")), query, body).await
}

pub async fn upload_chunk_ns(
    state: State<AppState>,
    user: RegistryUser,
    Path((ns, repo, uuid)): Path<(String, String, String)>,
    body: axum::body::Bytes,
) -> Result<Response, RegistryError> {
    upload_chunk(state, user, Path((format!("{ns}/{repo}"), uuid)), body).await
}

pub async fn complete_upload_ns(
    state: State<AppState>,
    user: RegistryUser,
    Path((ns, repo, uuid)): Path<(String, String, String)>,
    query: Query<UploadQuery>,
    body: axum::body::Bytes,
) -> Result<Response, RegistryError> {
    complete_upload(
        state,
        user,
        Path((format!("{ns}/{repo}"), uuid)),
        query,
        body,
    )
    .await
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn upload_key(id: &Uuid) -> String {
    format!("registry:upload:{id}")
}

fn header_val(s: &str) -> HeaderValue {
    HeaderValue::from_str(s).unwrap_or_else(|_| HeaderValue::from_static(""))
}

async fn complete_monolithic(
    state: &AppState,
    name: &str,
    repository_id: Uuid,
    digest_str: &str,
    data: Vec<u8>,
) -> Result<Response, RegistryError> {
    let expected_digest = Digest::parse(digest_str)?;
    let actual_digest = sha256_digest(&data);

    if actual_digest != expected_digest {
        return Err(RegistryError::DigestInvalid(format!(
            "expected {expected_digest}, got {actual_digest}"
        )));
    }

    let size_bytes = i64::try_from(data.len())
        .map_err(|e| RegistryError::Internal(anyhow::anyhow!("data size overflow: {e}")))?;
    let minio_path = expected_digest.minio_path();
    state.minio.write(&minio_path, data).await?;

    sqlx::query!(
        r#"INSERT INTO registry_blobs (digest, size_bytes, minio_path)
           VALUES ($1, $2, $3)
           ON CONFLICT (digest) DO NOTHING"#,
        expected_digest.as_str(),
        size_bytes,
        minio_path,
    )
    .execute(&state.pool)
    .await?;

    sqlx::query!(
        r#"INSERT INTO registry_blob_links (repository_id, blob_digest)
           VALUES ($1, $2)
           ON CONFLICT DO NOTHING"#,
        repository_id,
        expected_digest.as_str(),
    )
    .execute(&state.pool)
    .await?;

    let mut headers = HeaderMap::new();
    headers.insert(
        "location",
        header_val(&format!("/v2/{name}/blobs/{expected_digest}")),
    );
    headers.insert(
        "docker-content-digest",
        header_val(&expected_digest.as_str()),
    );

    Ok((StatusCode::CREATED, headers).into_response())
}

async fn cleanup_upload(state: &AppState, upload_id: &Uuid, session: &UploadSession) {
    // Delete temp parts from MinIO
    for i in 0..session.part_count {
        let path = format!("registry/uploads/{upload_id}/part-{i}");
        if let Err(e) = state.minio.delete(&path).await {
            tracing::warn!(error = %e, %path, "failed to clean up upload part");
        }
    }

    // Delete session from Valkey
    let key = upload_key(upload_id);
    let _: Result<(), _> = fred::interfaces::KeysInterface::del::<(), _>(&state.valkey, &key).await;
}
