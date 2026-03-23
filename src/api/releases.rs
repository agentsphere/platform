use axum::extract::Multipart;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use ts_rs::TS;

use crate::audit::{AuditEntry, write_audit};
use crate::auth::middleware::AuthUser;
use crate::error::ApiError;
use crate::store::AppState;
use crate::validation;

use super::helpers::{ListResponse, require_project_read, require_project_write};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateReleaseRequest {
    pub tag_name: String,
    pub name: String,
    pub body: Option<String>,
    #[serde(default)]
    pub is_draft: bool,
    #[serde(default)]
    pub is_prerelease: bool,
}

#[derive(Debug, Deserialize)]
pub struct UpdateReleaseRequest {
    pub name: Option<String>,
    pub body: Option<String>,
    pub is_draft: Option<bool>,
    pub is_prerelease: Option<bool>,
}

#[derive(Debug, Serialize, TS)]
#[ts(export, rename = "Release")]
pub struct ReleaseResponse {
    pub id: Uuid,
    pub project_id: Uuid,
    pub tag_name: String,
    pub name: String,
    pub body: Option<String>,
    pub is_draft: bool,
    pub is_prerelease: bool,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, TS)]
#[ts(export, rename = "ReleaseAsset")]
pub struct AssetResponse {
    pub id: Uuid,
    pub release_id: Uuid,
    pub name: String,
    pub content_type: Option<String>,
    pub size_bytes: Option<i64>,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/projects/{id}/releases",
            get(list_releases).post(create_release),
        )
        .route(
            "/api/projects/{id}/releases/{tag_name}",
            get(get_release)
                .patch(update_release)
                .delete(delete_release),
        )
        .route(
            "/api/projects/{id}/releases/{tag_name}/assets",
            axum::routing::post(upload_asset),
        )
        .route(
            "/api/projects/{id}/releases/{tag_name}/assets/{asset_id}/download",
            get(download_asset),
        )
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn list_releases(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<ListResponse<ReleaseResponse>>, ApiError> {
    require_project_read(&state, &auth, id).await?;

    let rows = sqlx::query(
        r"
        SELECT id, project_id, tag_name, name, body, is_draft, is_prerelease,
               created_by, created_at, updated_at
        FROM releases WHERE project_id = $1
        ORDER BY created_at DESC
        ",
    )
    .bind(id)
    .fetch_all(&state.pool)
    .await?;

    let total = i64::try_from(rows.len()).unwrap_or(i64::MAX);
    let items = rows
        .into_iter()
        .map(|r| ReleaseResponse {
            id: r.get("id"),
            project_id: r.get("project_id"),
            tag_name: r.get("tag_name"),
            name: r.get("name"),
            body: r.get("body"),
            is_draft: r.get("is_draft"),
            is_prerelease: r.get("is_prerelease"),
            created_by: r.get("created_by"),
            created_at: r.get("created_at"),
            updated_at: r.get("updated_at"),
        })
        .collect();

    Ok(Json(ListResponse { items, total }))
}

#[tracing::instrument(skip(state, body), fields(%id), err)]
async fn create_release(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<CreateReleaseRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_project_write(&state, &auth, id).await?;

    validation::check_length("tag_name", &body.tag_name, 1, 255)?;
    validation::check_length("name", &body.name, 1, 255)?;
    if let Some(ref b) = body.body {
        validation::check_length("body", b, 0, 100_000)?;
    }

    let row = sqlx::query(
        r"
        INSERT INTO releases (project_id, tag_name, name, body, is_draft, is_prerelease, created_by)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING id, project_id, tag_name, name, body, is_draft, is_prerelease,
                  created_by, created_at, updated_at
        ",
    )
    .bind(id)
    .bind(&body.tag_name)
    .bind(&body.name)
    .bind(&body.body)
    .bind(body.is_draft)
    .bind(body.is_prerelease)
    .bind(auth.user_id)
    .fetch_one(&state.pool)
    .await?;

    let release_id: Uuid = row.get("id");

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "release.create",
            resource: "release",
            resource_id: Some(release_id),
            project_id: Some(id),
            detail: Some(serde_json::json!({"tag_name": body.tag_name})),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok((
        StatusCode::CREATED,
        Json(ReleaseResponse {
            id: release_id,
            project_id: row.get("project_id"),
            tag_name: row.get("tag_name"),
            name: row.get("name"),
            body: row.get("body"),
            is_draft: row.get("is_draft"),
            is_prerelease: row.get("is_prerelease"),
            created_by: row.get("created_by"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        }),
    ))
}

async fn get_release(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, tag_name)): Path<(Uuid, String)>,
) -> Result<Json<ReleaseResponse>, ApiError> {
    require_project_read(&state, &auth, id).await?;

    let row = sqlx::query(
        r"
        SELECT id, project_id, tag_name, name, body, is_draft, is_prerelease,
               created_by, created_at, updated_at
        FROM releases WHERE project_id = $1 AND tag_name = $2
        ",
    )
    .bind(id)
    .bind(&tag_name)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("release".into()))?;

    Ok(Json(ReleaseResponse {
        id: row.get("id"),
        project_id: row.get("project_id"),
        tag_name: row.get("tag_name"),
        name: row.get("name"),
        body: row.get("body"),
        is_draft: row.get("is_draft"),
        is_prerelease: row.get("is_prerelease"),
        created_by: row.get("created_by"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }))
}

#[tracing::instrument(skip(state, body), fields(%id, %tag_name), err)]
async fn update_release(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, tag_name)): Path<(Uuid, String)>,
    Json(body): Json<UpdateReleaseRequest>,
) -> Result<Json<ReleaseResponse>, ApiError> {
    require_project_write(&state, &auth, id).await?;

    if let Some(ref n) = body.name {
        validation::check_length("name", n, 1, 255)?;
    }
    if let Some(ref b) = body.body {
        validation::check_length("body", b, 0, 100_000)?;
    }

    let row = sqlx::query(
        r"
        UPDATE releases SET
            name = COALESCE($3, name),
            body = COALESCE($4, body),
            is_draft = COALESCE($5, is_draft),
            is_prerelease = COALESCE($6, is_prerelease),
            updated_at = now()
        WHERE project_id = $1 AND tag_name = $2
        RETURNING id, project_id, tag_name, name, body, is_draft, is_prerelease,
                  created_by, created_at, updated_at
        ",
    )
    .bind(id)
    .bind(&tag_name)
    .bind(&body.name)
    .bind(&body.body)
    .bind(body.is_draft)
    .bind(body.is_prerelease)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("release".into()))?;

    let release_id: Uuid = row.get("id");

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "release.update",
            resource: "release",
            resource_id: Some(release_id),
            project_id: Some(id),
            detail: None,
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(Json(ReleaseResponse {
        id: release_id,
        project_id: row.get("project_id"),
        tag_name: row.get("tag_name"),
        name: row.get("name"),
        body: row.get("body"),
        is_draft: row.get("is_draft"),
        is_prerelease: row.get("is_prerelease"),
        created_by: row.get("created_by"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }))
}

#[tracing::instrument(skip(state), fields(%id, %tag_name), err)]
async fn delete_release(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, tag_name)): Path<(Uuid, String)>,
) -> Result<StatusCode, ApiError> {
    require_project_write(&state, &auth, id).await?;

    let release: Uuid = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM releases WHERE project_id = $1 AND tag_name = $2",
    )
    .bind(id)
    .bind(&tag_name)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("release".into()))?;

    // Delete assets from MinIO
    let assets = sqlx::query("SELECT minio_path FROM release_assets WHERE release_id = $1")
        .bind(release)
        .fetch_all(&state.pool)
        .await?;

    for asset in &assets {
        let minio_path: String = asset.get("minio_path");
        let _ = state.minio.delete(&minio_path).await;
    }

    sqlx::query("DELETE FROM releases WHERE id = $1")
        .bind(release)
        .execute(&state.pool)
        .await?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "release.delete",
            resource: "release",
            resource_id: Some(release),
            project_id: Some(id),
            detail: Some(serde_json::json!({"tag_name": tag_name})),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

#[tracing::instrument(skip(state, multipart), fields(%id, %tag_name), err)]
async fn upload_asset(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, tag_name)): Path<(Uuid, String)>,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, ApiError> {
    require_project_write(&state, &auth, id).await?;

    let release_id: Uuid = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM releases WHERE project_id = $1 AND tag_name = $2",
    )
    .bind(id)
    .bind(&tag_name)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("release".into()))?;

    let field = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::BadRequest(format!("multipart error: {e}")))?
        .ok_or_else(|| ApiError::BadRequest("no file field in multipart".into()))?;

    let file_name = field.file_name().unwrap_or("asset").to_string();
    let content_type = field.content_type().map(str::to_string);

    validation::check_length("name", &file_name, 1, 255)?;

    let data = field
        .bytes()
        .await
        .map_err(|e| ApiError::BadRequest(format!("failed to read file: {e}")))?;

    let size_bytes = i64::try_from(data.len()).unwrap_or(i64::MAX);
    let minio_path = format!("releases/{release_id}/{file_name}");

    state
        .minio
        .write(&minio_path, data)
        .await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("storage write: {e}")))?;

    let row = sqlx::query(
        r"
        INSERT INTO release_assets (release_id, name, minio_path, content_type, size_bytes)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id, release_id, name, content_type, size_bytes, created_at
        ",
    )
    .bind(release_id)
    .bind(&file_name)
    .bind(&minio_path)
    .bind(content_type.as_deref())
    .bind(size_bytes)
    .fetch_one(&state.pool)
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(AssetResponse {
            id: row.get("id"),
            release_id: row.get("release_id"),
            name: row.get("name"),
            content_type: row.get("content_type"),
            size_bytes: row.get("size_bytes"),
            created_at: row.get("created_at"),
        }),
    ))
}

async fn download_asset(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, tag_name, asset_id)): Path<(Uuid, String, Uuid)>,
) -> Result<impl IntoResponse, ApiError> {
    require_project_read(&state, &auth, id).await?;

    let release_id: Uuid = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM releases WHERE project_id = $1 AND tag_name = $2",
    )
    .bind(id)
    .bind(&tag_name)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("release".into()))?;

    let asset = sqlx::query(
        r"
        SELECT name, minio_path, content_type
        FROM release_assets WHERE id = $1 AND release_id = $2
        ",
    )
    .bind(asset_id)
    .bind(release_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("release asset".into()))?;

    let minio_path: String = asset.get("minio_path");
    let data = state
        .minio
        .read(&minio_path)
        .await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("storage read: {e}")))?;

    let content_type: Option<String> = asset.get("content_type");
    let content_type = content_type.unwrap_or_else(|| "application/octet-stream".into());
    let name: String = asset.get("name");

    Ok((
        [
            ("content-type", content_type),
            (
                "content-disposition",
                format!("attachment; filename=\"{name}\""),
            ),
        ],
        data.to_vec(),
    ))
}
