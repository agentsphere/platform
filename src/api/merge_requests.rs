use std::path::PathBuf;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::audit::{AuditEntry, write_audit};
use crate::auth::middleware::AuthUser;
use crate::error::ApiError;
use crate::rbac::Permission;
use crate::store::AppState;
use crate::validation;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateMrRequest {
    pub source_branch: String,
    pub target_branch: String,
    pub title: String,
    pub body: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateMrRequest {
    pub title: Option<String>,
    pub body: Option<String>,
    pub status: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ListMrParams {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub status: Option<String>,
    pub author_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct CreateReviewRequest {
    pub verdict: String,
    pub body: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateCommentRequest {
    pub body: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateCommentRequest {
    pub body: String,
}

#[derive(Debug, Serialize)]
pub struct MrResponse {
    pub id: Uuid,
    pub project_id: Uuid,
    pub number: i32,
    pub author_id: Uuid,
    pub source_branch: String,
    pub target_branch: String,
    pub title: String,
    pub body: Option<String>,
    pub status: String,
    pub merged_by: Option<Uuid>,
    pub merged_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct ReviewResponse {
    pub id: Uuid,
    pub mr_id: Uuid,
    pub reviewer_id: Uuid,
    pub verdict: String,
    pub body: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct CommentResponse {
    pub id: Uuid,
    pub author_id: Uuid,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

use super::helpers::{ListResponse, require_project_read, require_project_write};

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/projects/{id}/merge-requests",
            get(list_mrs).post(create_mr),
        )
        .route(
            "/api/projects/{id}/merge-requests/{number}",
            get(get_mr).patch(update_mr),
        )
        .route(
            "/api/projects/{id}/merge-requests/{number}/merge",
            post(merge_mr),
        )
        .route(
            "/api/projects/{id}/merge-requests/{number}/reviews",
            get(list_reviews).post(create_review),
        )
        .route(
            "/api/projects/{id}/merge-requests/{number}/comments",
            get(list_comments).post(create_comment),
        )
        .route(
            "/api/projects/{id}/merge-requests/{number}/comments/{comment_id}",
            axum::routing::patch(update_comment),
        )
}

// ---------------------------------------------------------------------------
// MR handlers
// ---------------------------------------------------------------------------

async fn get_project_repo_path(pool: &sqlx::PgPool, project_id: Uuid) -> Result<String, ApiError> {
    sqlx::query_scalar!(
        "SELECT repo_path FROM projects WHERE id = $1 AND is_active = true",
        project_id,
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("project".into()))?
    .ok_or_else(|| ApiError::BadRequest("project has no repo".into()))
}

#[tracing::instrument(skip(state, body), fields(%id), err)]
async fn create_mr(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<CreateMrRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // Validate input
    validation::check_length("title", &body.title, 1, 500)?;
    if let Some(ref b) = body.body {
        validation::check_length("body", b, 0, 100_000)?;
    }
    validation::check_branch_name(&body.source_branch)?;
    validation::check_branch_name(&body.target_branch)?;

    require_project_write(&state, &auth, id).await?;

    if body.source_branch == body.target_branch {
        return Err(ApiError::BadRequest(
            "source and target branches must differ".into(),
        ));
    }

    let repo_path = get_project_repo_path(&state.pool, id).await?;
    if !branch_exists_in_repo(&PathBuf::from(&repo_path), &body.source_branch).await {
        return Err(ApiError::BadRequest(format!(
            "source branch '{}' does not exist",
            body.source_branch
        )));
    }

    // Atomic increment of MR number
    let number = sqlx::query_scalar!(
        r#"
        UPDATE projects SET next_mr_number = next_mr_number + 1
        WHERE id = $1 AND is_active = true
        RETURNING next_mr_number
        "#,
        id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("project".into()))?;

    let mr = sqlx::query!(
        r#"
        INSERT INTO merge_requests (project_id, number, author_id, source_branch, target_branch, title, body)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING id, project_id, number, author_id, source_branch, target_branch, title, body,
                  status, merged_by, merged_at, created_at, updated_at
        "#,
        id,
        number,
        auth.user_id,
        body.source_branch,
        body.target_branch,
        body.title,
        body.body,
    )
    .fetch_one(&state.pool)
    .await?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "mr.create",
            resource: "merge_request",
            resource_id: Some(mr.id),
            project_id: Some(id),
            detail: Some(serde_json::json!({
                "number": number,
                "source": body.source_branch,
                "target": body.target_branch,
            })),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    crate::api::webhooks::fire_webhooks(
        &state.pool,
        id,
        "mr",
        &serde_json::json!({
            "action": "created",
            "merge_request": {"id": mr.id, "number": number, "title": body.title},
        }),
    )
    .await;

    Ok((
        StatusCode::CREATED,
        Json(MrResponse {
            id: mr.id,
            project_id: mr.project_id,
            number: mr.number,
            author_id: mr.author_id,
            source_branch: mr.source_branch,
            target_branch: mr.target_branch,
            title: mr.title,
            body: mr.body,
            status: mr.status,
            merged_by: mr.merged_by,
            merged_at: mr.merged_at,
            created_at: mr.created_at,
            updated_at: mr.updated_at,
        }),
    ))
}

async fn list_mrs(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Query(params): Query<ListMrParams>,
) -> Result<Json<ListResponse<MrResponse>>, ApiError> {
    require_project_read(&state, &auth, id).await?;

    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);

    let total = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) as "count!: i64"
        FROM merge_requests
        WHERE project_id = $1
          AND ($2::text IS NULL OR status = $2)
          AND ($3::uuid IS NULL OR author_id = $3)
        "#,
        id,
        params.status,
        params.author_id,
    )
    .fetch_one(&state.pool)
    .await?;

    let rows = sqlx::query!(
        r#"
        SELECT id, project_id, number, author_id, source_branch, target_branch, title, body,
               status, merged_by, merged_at, created_at, updated_at
        FROM merge_requests
        WHERE project_id = $1
          AND ($2::text IS NULL OR status = $2)
          AND ($3::uuid IS NULL OR author_id = $3)
        ORDER BY number DESC
        LIMIT $4 OFFSET $5
        "#,
        id,
        params.status,
        params.author_id,
        limit,
        offset,
    )
    .fetch_all(&state.pool)
    .await?;

    let items = rows
        .into_iter()
        .map(|m| MrResponse {
            id: m.id,
            project_id: m.project_id,
            number: m.number,
            author_id: m.author_id,
            source_branch: m.source_branch,
            target_branch: m.target_branch,
            title: m.title,
            body: m.body,
            status: m.status,
            merged_by: m.merged_by,
            merged_at: m.merged_at,
            created_at: m.created_at,
            updated_at: m.updated_at,
        })
        .collect();

    Ok(Json(ListResponse { items, total }))
}

async fn get_mr(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, number)): Path<(Uuid, i32)>,
) -> Result<Json<MrResponse>, ApiError> {
    require_project_read(&state, &auth, id).await?;

    let mr = sqlx::query!(
        r#"
        SELECT id, project_id, number, author_id, source_branch, target_branch, title, body,
               status, merged_by, merged_at, created_at, updated_at
        FROM merge_requests WHERE project_id = $1 AND number = $2
        "#,
        id,
        number,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("merge request".into()))?;

    Ok(Json(MrResponse {
        id: mr.id,
        project_id: mr.project_id,
        number: mr.number,
        author_id: mr.author_id,
        source_branch: mr.source_branch,
        target_branch: mr.target_branch,
        title: mr.title,
        body: mr.body,
        status: mr.status,
        merged_by: mr.merged_by,
        merged_at: mr.merged_at,
        created_at: mr.created_at,
        updated_at: mr.updated_at,
    }))
}

#[tracing::instrument(skip(state, body), fields(%id, %number), err)]
async fn update_mr(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, number)): Path<(Uuid, i32)>,
    Json(body): Json<UpdateMrRequest>,
) -> Result<Json<MrResponse>, ApiError> {
    // Validate input
    if let Some(ref t) = body.title {
        validation::check_length("title", t, 1, 500)?;
    }
    if let Some(ref b) = body.body {
        validation::check_length("body", b, 0, 100_000)?;
    }

    let mr_author = sqlx::query_scalar!(
        "SELECT author_id FROM merge_requests WHERE project_id = $1 AND number = $2",
        id,
        number,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("merge request".into()))?;

    if mr_author != auth.user_id {
        let allowed = crate::rbac::resolver::has_permission(
            &state.pool,
            &state.valkey,
            auth.user_id,
            Some(id),
            Permission::ProjectWrite,
        )
        .await
        .map_err(ApiError::Internal)?;

        if !allowed {
            return Err(ApiError::Forbidden);
        }
    }

    // Only allow closing via update, not merging
    if let Some(ref status) = body.status
        && !["open", "closed"].contains(&status.as_str())
    {
        return Err(ApiError::BadRequest(
            "status must be open or closed (use merge endpoint to merge)".into(),
        ));
    }

    let mr = sqlx::query!(
        r#"
        UPDATE merge_requests SET
            title = COALESCE($3, title),
            body = COALESCE($4, body),
            status = COALESCE($5, status)
        WHERE project_id = $1 AND number = $2
        RETURNING id, project_id, number, author_id, source_branch, target_branch, title, body,
                  status, merged_by, merged_at, created_at, updated_at
        "#,
        id,
        number,
        body.title,
        body.body,
        body.status,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("merge request".into()))?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "mr.update",
            resource: "merge_request",
            resource_id: Some(mr.id),
            project_id: Some(id),
            detail: None,
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(Json(MrResponse {
        id: mr.id,
        project_id: mr.project_id,
        number: mr.number,
        author_id: mr.author_id,
        source_branch: mr.source_branch,
        target_branch: mr.target_branch,
        title: mr.title,
        body: mr.body,
        status: mr.status,
        merged_by: mr.merged_by,
        merged_at: mr.merged_at,
        created_at: mr.created_at,
        updated_at: mr.updated_at,
    }))
}

#[tracing::instrument(skip(state), fields(%id, %number), err)]
async fn merge_mr(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, number)): Path<(Uuid, i32)>,
) -> Result<Json<MrResponse>, ApiError> {
    let allowed = crate::rbac::resolver::has_permission(
        &state.pool,
        &state.valkey,
        auth.user_id,
        Some(id),
        Permission::ProjectWrite,
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::Forbidden);
    }

    let mr = sqlx::query!(
        r#"
        SELECT id, source_branch, target_branch, status
        FROM merge_requests WHERE project_id = $1 AND number = $2
        "#,
        id,
        number,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("merge request".into()))?;

    if mr.status != "open" {
        return Err(ApiError::BadRequest(format!(
            "cannot merge MR with status '{}'",
            mr.status
        )));
    }

    let repo_path = get_project_repo_path(&state.pool, id).await?;
    git_merge_no_ff(
        &PathBuf::from(&repo_path),
        &mr.source_branch,
        &mr.target_branch,
    )
    .await
    .map_err(|e| ApiError::BadRequest(format!("merge failed: {e}")))?;

    let now = Utc::now();
    let merged = sqlx::query!(
        r#"
        UPDATE merge_requests
        SET status = 'merged', merged_by = $3, merged_at = $4
        WHERE project_id = $1 AND number = $2
        RETURNING id, project_id, number, author_id, source_branch, target_branch, title, body,
                  status, merged_by, merged_at, created_at, updated_at
        "#,
        id,
        number,
        auth.user_id,
        now,
    )
    .fetch_one(&state.pool)
    .await?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "mr.merge",
            resource: "merge_request",
            resource_id: Some(merged.id),
            project_id: Some(id),
            detail: Some(serde_json::json!({
                "number": number,
                "source": mr.source_branch,
                "target": mr.target_branch,
            })),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    crate::api::webhooks::fire_webhooks(
        &state.pool,
        id,
        "mr",
        &serde_json::json!({
            "action": "merged",
            "merge_request": {"id": merged.id, "number": number},
        }),
    )
    .await;

    Ok(Json(MrResponse {
        id: merged.id,
        project_id: merged.project_id,
        number: merged.number,
        author_id: merged.author_id,
        source_branch: merged.source_branch,
        target_branch: merged.target_branch,
        title: merged.title,
        body: merged.body,
        status: merged.status,
        merged_by: merged.merged_by,
        merged_at: merged.merged_at,
        created_at: merged.created_at,
        updated_at: merged.updated_at,
    }))
}

// ---------------------------------------------------------------------------
// Review handlers
// ---------------------------------------------------------------------------

async fn list_reviews(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, number)): Path<(Uuid, i32)>,
) -> Result<Json<Vec<ReviewResponse>>, ApiError> {
    require_project_read(&state, &auth, id).await?;

    let mr_id = sqlx::query_scalar!(
        "SELECT id FROM merge_requests WHERE project_id = $1 AND number = $2",
        id,
        number,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("merge request".into()))?;

    let rows = sqlx::query!(
        r#"
        SELECT id, mr_id, reviewer_id, verdict, body, created_at
        FROM mr_reviews WHERE mr_id = $1
        ORDER BY created_at ASC
        "#,
        mr_id,
    )
    .fetch_all(&state.pool)
    .await?;

    let items = rows
        .into_iter()
        .map(|r| ReviewResponse {
            id: r.id,
            mr_id: r.mr_id,
            reviewer_id: r.reviewer_id,
            verdict: r.verdict,
            body: r.body,
            created_at: r.created_at,
        })
        .collect();

    Ok(Json(items))
}

#[tracing::instrument(skip(state, body), fields(%id, %number), err)]
async fn create_review(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, number)): Path<(Uuid, i32)>,
    Json(body): Json<CreateReviewRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_project_read(&state, &auth, id).await?;

    if let Some(ref b) = body.body {
        validation::check_length("body", b, 0, 100_000)?;
    }

    if !["approve", "request_changes", "comment"].contains(&body.verdict.as_str()) {
        return Err(ApiError::BadRequest(
            "verdict must be approve, request_changes, or comment".into(),
        ));
    }

    let mr_id = sqlx::query_scalar!(
        "SELECT id FROM merge_requests WHERE project_id = $1 AND number = $2",
        id,
        number,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("merge request".into()))?;

    let review = sqlx::query!(
        r#"
        INSERT INTO mr_reviews (project_id, mr_id, reviewer_id, verdict, body)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id, mr_id, reviewer_id, verdict, body, created_at
        "#,
        id,
        mr_id,
        auth.user_id,
        body.verdict,
        body.body,
    )
    .fetch_one(&state.pool)
    .await?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "review.create",
            resource: "mr_review",
            resource_id: Some(review.id),
            project_id: Some(id),
            detail: Some(serde_json::json!({"mr_number": number, "verdict": body.verdict})),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok((
        StatusCode::CREATED,
        Json(ReviewResponse {
            id: review.id,
            mr_id: review.mr_id,
            reviewer_id: review.reviewer_id,
            verdict: review.verdict,
            body: review.body,
            created_at: review.created_at,
        }),
    ))
}

// ---------------------------------------------------------------------------
// MR Comment handlers
// ---------------------------------------------------------------------------

async fn list_comments(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, number)): Path<(Uuid, i32)>,
) -> Result<Json<Vec<CommentResponse>>, ApiError> {
    require_project_read(&state, &auth, id).await?;

    let mr_id = sqlx::query_scalar!(
        "SELECT id FROM merge_requests WHERE project_id = $1 AND number = $2",
        id,
        number,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("merge request".into()))?;

    let rows = sqlx::query!(
        r#"
        SELECT id, author_id, body, created_at, updated_at
        FROM comments WHERE mr_id = $1
        ORDER BY created_at ASC
        "#,
        mr_id,
    )
    .fetch_all(&state.pool)
    .await?;

    let items = rows
        .into_iter()
        .map(|c| CommentResponse {
            id: c.id,
            author_id: c.author_id,
            body: c.body,
            created_at: c.created_at,
            updated_at: c.updated_at,
        })
        .collect();

    Ok(Json(items))
}

#[tracing::instrument(skip(state, body), fields(%id, %number), err)]
async fn create_comment(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, number)): Path<(Uuid, i32)>,
    Json(body): Json<CreateCommentRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_project_read(&state, &auth, id).await?;
    validation::check_length("body", &body.body, 1, 100_000)?;

    let mr_id = sqlx::query_scalar!(
        "SELECT id FROM merge_requests WHERE project_id = $1 AND number = $2",
        id,
        number,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("merge request".into()))?;

    let comment = sqlx::query!(
        r#"
        INSERT INTO comments (project_id, mr_id, author_id, body)
        VALUES ($1, $2, $3, $4)
        RETURNING id, author_id, body, created_at, updated_at
        "#,
        id,
        mr_id,
        auth.user_id,
        body.body,
    )
    .fetch_one(&state.pool)
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(CommentResponse {
            id: comment.id,
            author_id: comment.author_id,
            body: comment.body,
            created_at: comment.created_at,
            updated_at: comment.updated_at,
        }),
    ))
}

#[tracing::instrument(skip(state, body), fields(%id, %number, %comment_id), err)]
async fn update_comment(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, number, comment_id)): Path<(Uuid, i32, Uuid)>,
    Json(body): Json<UpdateCommentRequest>,
) -> Result<Json<CommentResponse>, ApiError> {
    validation::check_length("body", &body.body, 1, 100_000)?;

    // Verify MR exists
    let _mr_id = sqlx::query_scalar!(
        "SELECT id FROM merge_requests WHERE project_id = $1 AND number = $2",
        id,
        number,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("merge request".into()))?;

    let comment_author =
        sqlx::query_scalar!("SELECT author_id FROM comments WHERE id = $1", comment_id,)
            .fetch_optional(&state.pool)
            .await?
            .ok_or_else(|| ApiError::NotFound("comment".into()))?;

    if comment_author != auth.user_id {
        let is_admin = crate::rbac::resolver::has_permission(
            &state.pool,
            &state.valkey,
            auth.user_id,
            None,
            Permission::AdminUsers,
        )
        .await
        .map_err(ApiError::Internal)?;

        if !is_admin {
            return Err(ApiError::Forbidden);
        }
    }

    let comment = sqlx::query!(
        r#"
        UPDATE comments SET body = $2
        WHERE id = $1
        RETURNING id, author_id, body, created_at, updated_at
        "#,
        comment_id,
        body.body,
    )
    .fetch_one(&state.pool)
    .await?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "comment.update",
            resource: "comment",
            resource_id: Some(comment_id),
            project_id: Some(id),
            detail: None,
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(Json(CommentResponse {
        id: comment.id,
        author_id: comment.author_id,
        body: comment.body,
        created_at: comment.created_at,
        updated_at: comment.updated_at,
    }))
}

// ---------------------------------------------------------------------------
// Git helpers
// ---------------------------------------------------------------------------

async fn branch_exists_in_repo(repo_path: &std::path::Path, branch: &str) -> bool {
    let result = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("rev-parse")
        .arg("--verify")
        .arg(format!("refs/heads/{branch}"))
        .output()
        .await;

    matches!(result, Ok(output) if output.status.success())
}

/// Execute `git merge --no-ff` in a bare repository using a temporary worktree.
///
/// Bare repos can't merge directly, so we use `git worktree` for the operation.
async fn git_merge_no_ff(
    repo_path: &std::path::Path,
    source_branch: &str,
    target_branch: &str,
) -> anyhow::Result<()> {
    let worktree_dir = repo_path.join(format!("_merge_worktree_{}", uuid::Uuid::new_v4()));

    // Add temporary worktree on target branch
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("worktree")
        .arg("add")
        .arg(&worktree_dir)
        .arg(target_branch)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("failed to create worktree: {stderr}");
    }

    // Merge source into target with --no-ff
    let merge_output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&worktree_dir)
        .arg("merge")
        .arg("--no-ff")
        .arg(format!("origin/{source_branch}"))
        .arg("-m")
        .arg(format!(
            "Merge branch '{source_branch}' into {target_branch}"
        ))
        .output()
        .await?;

    // Clean up worktree regardless of merge result
    let _ = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("worktree")
        .arg("remove")
        .arg("--force")
        .arg(&worktree_dir)
        .output()
        .await;

    // Also remove the directory if it still exists
    let _ = tokio::fs::remove_dir_all(&worktree_dir).await;

    if !merge_output.status.success() {
        let stderr = String::from_utf8_lossy(&merge_output.stderr);
        anyhow::bail!("merge failed: {stderr}");
    }

    Ok(())
}
