use std::path::PathBuf;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use ts_rs::TS;

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
pub struct MergeMrRequest {
    pub merge_method: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AutoMergeRequest {
    pub merge_method: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateCommentRequest {
    pub body: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateCommentRequest {
    pub body: String,
}

#[derive(Debug, Serialize, TS)]
#[ts(export, rename = "MergeRequest")]
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

#[derive(Debug, Serialize, TS)]
#[ts(export, rename = "Review")]
pub struct ReviewResponse {
    pub id: Uuid,
    pub mr_id: Uuid,
    pub reviewer_id: Uuid,
    pub verdict: String,
    pub body: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, TS)]
#[ts(export, rename = "MrComment")]
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
            "/api/projects/{id}/merge-requests/{number}/auto-merge",
            axum::routing::put(enable_auto_merge).delete(disable_auto_merge),
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

    // Resolve HEAD SHA of source branch (needed for merge gate checks + post-merge deploy)
    let source_head_sha =
        get_branch_head_sha(&PathBuf::from(&repo_path), &body.source_branch).await;

    let mr = sqlx::query!(
        r#"
        INSERT INTO merge_requests (project_id, number, author_id, source_branch, target_branch, title, body, head_sha)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
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
        source_head_sha,
    )
    .fetch_one(&state.pool)
    .await?;

    run_mr_create_side_effects(&state, &auth, id, &mr.id, number, &body, &repo_path).await;

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
        let allowed = crate::rbac::resolver::has_permission_scoped(
            &state.pool,
            &state.valkey,
            auth.user_id,
            Some(id),
            Permission::ProjectWrite,
            auth.token_scopes.as_deref(),
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

#[tracing::instrument(skip(state, body), fields(%id, %number), err)]
async fn merge_mr(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, number)): Path<(Uuid, i32)>,
    body: Option<Json<MergeMrRequest>>,
) -> Result<Json<MrResponse>, ApiError> {
    let merge_method = body
        .as_ref()
        .and_then(|b| b.merge_method.as_deref())
        .unwrap_or("merge");

    do_merge(&state, &auth, id, number, merge_method).await
}

/// Check branch protection rules before allowing a merge.
#[allow(clippy::too_many_arguments)]
async fn enforce_merge_gates(
    state: &AppState,
    auth: &AuthUser,
    project_id: Uuid,
    mr_id: &Uuid,
    source_branch: &str,
    target_branch: &str,
    merge_method: &str,
) -> Result<(), ApiError> {
    let rule = crate::git::protection::get_protection(&state.pool, project_id, target_branch)
        .await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("protection check: {e}")))?;

    let Some(ref rule) = rule else {
        return Ok(());
    };

    // Check admin bypass
    let is_admin = if rule.allow_admin_bypass {
        crate::rbac::resolver::has_permission(
            &state.pool,
            &state.valkey,
            auth.user_id,
            Some(project_id),
            Permission::AdminUsers,
        )
        .await
        .unwrap_or(false)
    } else {
        false
    };

    if is_admin {
        return Ok(());
    }

    // Validate merge method
    if !rule.merge_methods.iter().any(|m| m == merge_method) {
        return Err(ApiError::BadRequest(format!(
            "merge method '{merge_method}' not allowed; permitted: {}",
            rule.merge_methods.join(", ")
        )));
    }

    // Check required approvals (non-stale)
    if rule.required_approvals > 0 {
        let approval_count = sqlx::query_scalar!(
            r#"SELECT COUNT(*) as "count!: i64" FROM mr_reviews
            WHERE mr_id = $1 AND verdict = 'approve' AND is_stale = false"#,
            mr_id,
        )
        .fetch_one(&state.pool)
        .await?;

        if approval_count < i64::from(rule.required_approvals) {
            return Err(ApiError::BadRequest(format!(
                "requires {} approval(s), has {}",
                rule.required_approvals, approval_count
            )));
        }
    }

    // Check CI pipeline status
    if !rule.required_checks.is_empty() {
        let latest_pipeline = sqlx::query!(
            r#"
            SELECT status FROM pipelines
            WHERE project_id = $1 AND git_ref = $2 AND trigger IN ('push', 'mr')
            ORDER BY created_at DESC LIMIT 1
            "#,
            project_id,
            format!("refs/heads/{source_branch}"),
        )
        .fetch_optional(&state.pool)
        .await?;

        match latest_pipeline {
            Some(ref p) if p.status == "success" => {}
            Some(ref p) => {
                return Err(ApiError::BadRequest(format!(
                    "CI pipeline status is '{}', must be 'success'",
                    p.status
                )));
            }
            None => {
                return Err(ApiError::BadRequest(
                    "no CI pipeline found for source branch".into(),
                ));
            }
        }
    }

    // Check require_up_to_date
    if rule.require_up_to_date {
        let repo_path = get_project_repo_path(&state.pool, project_id).await?;
        let is_up_to_date =
            check_branch_up_to_date(&PathBuf::from(&repo_path), source_branch, target_branch).await;
        if !is_up_to_date {
            return Err(ApiError::BadRequest(
                "source branch is not up to date with target branch".into(),
            ));
        }
    }

    Ok(())
}

/// Core merge logic shared by manual merge and auto-merge.
async fn do_merge(
    state: &AppState,
    auth: &AuthUser,
    project_id: Uuid,
    number: i32,
    merge_method: &str,
) -> Result<Json<MrResponse>, ApiError> {
    let allowed = crate::rbac::resolver::has_permission_scoped(
        &state.pool,
        &state.valkey,
        auth.user_id,
        Some(project_id),
        Permission::ProjectWrite,
        auth.token_scopes.as_deref(),
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::Forbidden);
    }

    let mr = sqlx::query!(
        r#"
        SELECT id, source_branch, target_branch, status, head_sha
        FROM merge_requests WHERE project_id = $1 AND number = $2
        "#,
        project_id,
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

    // Enforce merge gates from branch protection
    enforce_merge_gates(
        state,
        auth,
        project_id,
        &mr.id,
        &mr.source_branch,
        &mr.target_branch,
        merge_method,
    )
    .await?;

    let repo_path = get_project_repo_path(&state.pool, project_id).await?;
    let repo_path_buf = PathBuf::from(&repo_path);

    execute_git_merge(
        &repo_path_buf,
        &mr.source_branch,
        &mr.target_branch,
        &mr.id.to_string(),
        merge_method,
    )
    .await?;

    let now = Utc::now();
    let merged = sqlx::query!(
        r#"
        UPDATE merge_requests
        SET status = 'merged', merged_by = $3, merged_at = $4
        WHERE project_id = $1 AND number = $2
        RETURNING id, project_id, number, author_id, source_branch, target_branch, title, body,
                  status, merged_by, merged_at, created_at, updated_at
        "#,
        project_id,
        number,
        auth.user_id,
        now,
    )
    .fetch_one(&state.pool)
    .await?;

    run_post_merge_side_effects(
        state,
        auth,
        project_id,
        &merged.id,
        number,
        &mr.source_branch,
        &mr.target_branch,
        merge_method,
        mr.head_sha.as_deref(),
        &repo_path_buf,
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
) -> Result<Json<ListResponse<ReviewResponse>>, ApiError> {
    require_project_read(&state, &auth, id).await?;

    let mr_id = sqlx::query_scalar!(
        "SELECT id FROM merge_requests WHERE project_id = $1 AND number = $2",
        id,
        number,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("merge request".into()))?;

    let total = sqlx::query_scalar!(
        r#"SELECT COUNT(*) as "count!: i64" FROM mr_reviews WHERE mr_id = $1"#,
        mr_id,
    )
    .fetch_one(&state.pool)
    .await?;

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

    Ok(Json(ListResponse { items, total }))
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

    // Try auto-merge after approval
    if body.verdict == "approve" {
        let auto_merge_state = state.clone();
        tokio::spawn(async move {
            try_auto_merge(&auto_merge_state, id).await;
        });
    }

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
) -> Result<Json<ListResponse<CommentResponse>>, ApiError> {
    require_project_read(&state, &auth, id).await?;

    let mr_id = sqlx::query_scalar!(
        "SELECT id FROM merge_requests WHERE project_id = $1 AND number = $2",
        id,
        number,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("merge request".into()))?;

    let total = sqlx::query_scalar!(
        r#"SELECT COUNT(*) as "count!: i64" FROM comments WHERE mr_id = $1"#,
        mr_id,
    )
    .fetch_one(&state.pool)
    .await?;

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

    Ok(Json(ListResponse { items, total }))
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
        let is_admin = crate::rbac::resolver::has_permission_scoped(
            &state.pool,
            &state.valkey,
            auth.user_id,
            None,
            Permission::AdminUsers,
            auth.token_scopes.as_deref(),
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

/// Dispatch to the appropriate git merge strategy.
async fn execute_git_merge(
    repo_path: &std::path::Path,
    source_branch: &str,
    target_branch: &str,
    mr_id: &str,
    merge_method: &str,
) -> Result<(), ApiError> {
    match merge_method {
        "squash" => git_squash_merge(repo_path, source_branch, target_branch, mr_id)
            .await
            .map_err(|e| ApiError::BadRequest(format!("squash merge failed: {e}")))?,
        "rebase" => git_rebase_merge(repo_path, source_branch, target_branch)
            .await
            .map_err(|e| ApiError::BadRequest(format!("rebase merge failed: {e}")))?,
        _ => git_merge_no_ff(repo_path, source_branch, target_branch)
            .await
            .map_err(|e| ApiError::BadRequest(format!("merge failed: {e}")))?,
    }
    Ok(())
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
        .env("GIT_AUTHOR_NAME", "Platform")
        .env("GIT_AUTHOR_EMAIL", "platform@localhost")
        .env("GIT_COMMITTER_NAME", "Platform")
        .env("GIT_COMMITTER_EMAIL", "platform@localhost")
        .arg("merge")
        .arg("--no-ff")
        .arg(source_branch)
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

/// Squash merge: squash all source commits into a single commit on target.
async fn git_squash_merge(
    repo_path: &std::path::Path,
    source_branch: &str,
    target_branch: &str,
    mr_id: &str,
) -> anyhow::Result<()> {
    let worktree_dir = repo_path.join(format!("_squash_worktree_{}", uuid::Uuid::new_v4()));

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

    let squash_output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&worktree_dir)
        .env("GIT_AUTHOR_NAME", "Platform")
        .env("GIT_AUTHOR_EMAIL", "platform@localhost")
        .env("GIT_COMMITTER_NAME", "Platform")
        .env("GIT_COMMITTER_EMAIL", "platform@localhost")
        .arg("merge")
        .arg("--squash")
        .arg(source_branch)
        .output()
        .await?;

    let commit_result = if squash_output.status.success() {
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(&worktree_dir)
            .env("GIT_AUTHOR_NAME", "Platform")
            .env("GIT_AUTHOR_EMAIL", "platform@localhost")
            .env("GIT_COMMITTER_NAME", "Platform")
            .env("GIT_COMMITTER_EMAIL", "platform@localhost")
            .arg("commit")
            .arg("-m")
            .arg(format!(
                "Squash merge branch '{source_branch}' into {target_branch} (MR {mr_id})"
            ))
            .output()
            .await?
    } else {
        squash_output
    };

    cleanup_worktree(repo_path, &worktree_dir).await;

    if !commit_result.status.success() {
        let stderr = String::from_utf8_lossy(&commit_result.stderr);
        anyhow::bail!("squash merge failed: {stderr}");
    }

    Ok(())
}

/// Rebase merge: rebase source commits onto target.
async fn git_rebase_merge(
    repo_path: &std::path::Path,
    source_branch: &str,
    target_branch: &str,
) -> anyhow::Result<()> {
    let worktree_dir = repo_path.join(format!("_rebase_worktree_{}", uuid::Uuid::new_v4()));

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

    // Get the source branch commits and cherry-pick/rebase onto target
    let rebase_output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&worktree_dir)
        .env("GIT_AUTHOR_NAME", "Platform")
        .env("GIT_AUTHOR_EMAIL", "platform@localhost")
        .env("GIT_COMMITTER_NAME", "Platform")
        .env("GIT_COMMITTER_EMAIL", "platform@localhost")
        .arg("merge")
        .arg("--ff-only")
        .arg(source_branch)
        .output()
        .await?;

    cleanup_worktree(repo_path, &worktree_dir).await;

    if !rebase_output.status.success() {
        let stderr = String::from_utf8_lossy(&rebase_output.stderr);
        anyhow::bail!("rebase merge failed (source not rebased on target): {stderr}");
    }

    Ok(())
}

async fn cleanup_worktree(repo_path: &std::path::Path, worktree_dir: &std::path::Path) {
    let _ = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("worktree")
        .arg("remove")
        .arg("--force")
        .arg(worktree_dir)
        .output()
        .await;
    let _ = tokio::fs::remove_dir_all(worktree_dir).await;
}

/// Get the HEAD SHA of a branch.
async fn get_branch_head_sha(repo_path: &std::path::Path, branch: &str) -> Option<String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("rev-parse")
        .arg(format!("refs/heads/{branch}"))
        .output()
        .await
        .ok()?;

    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        None
    }
}

/// Check if target branch HEAD is an ancestor of source branch HEAD (i.e., source is up to date).
async fn check_branch_up_to_date(
    repo_path: &std::path::Path,
    source_branch: &str,
    target_branch: &str,
) -> bool {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("merge-base")
        .arg("--is-ancestor")
        .arg(format!("refs/heads/{target_branch}"))
        .arg(format!("refs/heads/{source_branch}"))
        .output()
        .await;

    matches!(output, Ok(o) if o.status.success())
}

// ---------------------------------------------------------------------------
// Auto-merge
// ---------------------------------------------------------------------------

async fn enable_auto_merge(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, number)): Path<(Uuid, i32)>,
    body: Option<Json<AutoMergeRequest>>,
) -> Result<StatusCode, ApiError> {
    require_project_write(&state, &auth, id).await?;

    let merge_method = body
        .as_ref()
        .and_then(|b| b.merge_method.as_deref())
        .unwrap_or("merge")
        .to_string();

    let result = sqlx::query!(
        r#"
        UPDATE merge_requests
        SET auto_merge = true, auto_merge_by = $3, auto_merge_method = $4, updated_at = now()
        WHERE project_id = $1 AND number = $2 AND status = 'open'
        "#,
        id,
        number,
        auth.user_id,
        merge_method,
    )
    .execute(&state.pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::NotFound("merge request".into()));
    }

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "mr.auto_merge.enable",
            resource: "merge_request",
            resource_id: None,
            project_id: Some(id),
            detail: Some(serde_json::json!({"number": number, "method": merge_method})),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(StatusCode::OK)
}

async fn disable_auto_merge(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, number)): Path<(Uuid, i32)>,
) -> Result<StatusCode, ApiError> {
    require_project_write(&state, &auth, id).await?;

    let result = sqlx::query!(
        r#"
        UPDATE merge_requests
        SET auto_merge = false, auto_merge_by = NULL, auto_merge_method = NULL, updated_at = now()
        WHERE project_id = $1 AND number = $2
        "#,
        id,
        number,
    )
    .execute(&state.pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::NotFound("merge request".into()));
    }

    Ok(StatusCode::OK)
}

/// Try to auto-merge all eligible MRs for a project after a pipeline succeeds or a review is submitted.
///
/// Called from the pipeline executor and review handler.
pub async fn try_auto_merge(state: &AppState, project_id: Uuid) {
    let mrs = sqlx::query!(
        r#"
        SELECT number, auto_merge_by, auto_merge_method
        FROM merge_requests
        WHERE project_id = $1 AND status = 'open' AND auto_merge = true
        "#,
        project_id,
    )
    .fetch_all(&state.pool)
    .await;

    let Ok(mrs) = mrs else {
        return;
    };

    for mr in mrs {
        let Some(user_id) = mr.auto_merge_by else {
            continue;
        };
        let method = mr.auto_merge_method.as_deref().unwrap_or("merge");

        // Build a synthetic AuthUser for the auto-merge
        let user_name = sqlx::query_scalar!("SELECT name FROM users WHERE id = $1", user_id)
            .fetch_optional(&state.pool)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();

        let auth = crate::auth::middleware::AuthUser {
            user_id,
            user_name,
            user_type: crate::auth::user_type::UserType::Human,
            ip_addr: None,
            token_scopes: None,
            boundary_project_id: None,
            boundary_workspace_id: None,
        };

        match do_merge(state, &auth, project_id, mr.number, method).await {
            Ok(_) => {
                tracing::info!(project_id = %project_id, mr_number = mr.number, "auto-merge succeeded");
            }
            Err(e) => {
                tracing::debug!(
                    project_id = %project_id,
                    mr_number = mr.number,
                    error = %e,
                    "auto-merge not ready"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Background task helpers
// ---------------------------------------------------------------------------

async fn run_mr_create_side_effects(
    state: &AppState,
    auth: &AuthUser,
    project_id: Uuid,
    mr_id: &Uuid,
    number: i32,
    body: &CreateMrRequest,
    repo_path: &str,
) {
    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "mr.create",
            resource: "merge_request",
            resource_id: Some(*mr_id),
            project_id: Some(project_id),
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
        project_id,
        "mr",
        &serde_json::json!({
            "action": "created",
            "merge_request": {"id": mr_id, "number": number, "title": body.title},
        }),
    )
    .await;

    spawn_mr_pipeline_trigger(
        state,
        project_id,
        auth.user_id,
        repo_path,
        &body.source_branch,
        "opened",
    );
}

#[allow(clippy::too_many_arguments)]
async fn run_post_merge_side_effects(
    state: &AppState,
    auth: &AuthUser,
    project_id: Uuid,
    merged_id: &Uuid,
    number: i32,
    source_branch: &str,
    target_branch: &str,
    merge_method: &str,
    head_sha: Option<&str>,
    repo_path: &std::path::Path,
) {
    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "mr.merge",
            resource: "merge_request",
            resource_id: Some(*merged_id),
            project_id: Some(project_id),
            detail: Some(serde_json::json!({
                "number": number,
                "source": source_branch,
                "target": target_branch,
                "method": merge_method,
            })),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    crate::api::webhooks::fire_webhooks(
        &state.pool,
        project_id,
        "mr",
        &serde_json::json!({
            "action": "merged",
            "merge_request": {"id": merged_id, "number": number},
        }),
    )
    .await;

    crate::deployer::preview::stop_preview_for_branch(&state.pool, project_id, source_branch).await;

    // Post-merge deploy (background, best-effort)
    let deploy_state = state.clone();
    let source_head_sha = head_sha.unwrap_or_default().to_string();
    let target_branch = target_branch.to_string();
    let deploy_repo_path = repo_path.to_path_buf();
    tokio::spawn(async move {
        post_merge_deploy(
            &deploy_state,
            project_id,
            &target_branch,
            &deploy_repo_path,
            &source_head_sha,
        )
        .await;
    });
}

fn spawn_mr_pipeline_trigger(
    state: &AppState,
    project_id: Uuid,
    user_id: Uuid,
    repo_path: &str,
    source_branch: &str,
    action: &str,
) {
    let pool = state.pool.clone();
    let trigger_state = state.clone();
    let repo = PathBuf::from(repo_path);
    let branch = source_branch.to_string();
    let action = action.to_string();
    tokio::spawn(async move {
        let sha = get_branch_head_sha(&repo, &branch).await;
        let mr_params = crate::pipeline::trigger::MrTriggerParams {
            project_id,
            user_id,
            repo_path: repo,
            source_branch: branch,
            commit_sha: sha,
            action,
        };
        match crate::pipeline::trigger::on_mr(&pool, &mr_params).await {
            Ok(Some(pipeline_id)) => {
                crate::pipeline::trigger::notify_executor(&trigger_state, pipeline_id).await;
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!(error = %e, "MR pipeline trigger failed");
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Post-merge deploy
// ---------------------------------------------------------------------------

/// Automatic post-merge deploy: create/update production deployment, sync `deploy/`
/// to ops repo, and wake the reconciler.
///
/// The `source_head_sha` is the MR's head commit — the exact commit that was built
/// and tested by the CI pipeline. The image was tagged with this full SHA by Kaniko.
async fn post_merge_deploy(
    state: &AppState,
    project_id: Uuid,
    target_branch: &str,
    repo_path: &std::path::Path,
    source_head_sha: &str,
) {
    if source_head_sha.is_empty() {
        return;
    }

    let short_sha = &source_head_sha[..source_head_sha.len().min(7)];
    let image_tag = format!("sha-{short_sha}");

    let project_name = sqlx::query_scalar!("SELECT name FROM projects WHERE id = $1", project_id)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();

    // Build image_ref using node_registry_url (DaemonSet proxy — what containerd pulls from).
    // Must match the format used by detect_and_write_deployment in the pipeline executor.
    let registry = state
        .config
        .registry_node_url
        .as_deref()
        .or(state.config.registry_url.as_deref())
        .unwrap_or("localhost:5000");
    let image_ref = format!("{registry}/{project_name}/app:{source_head_sha}");

    // Read VERSION file from target branch (post-merge)
    let version = crate::pipeline::trigger::read_version_at_ref(repo_path, target_branch).await;

    // Optional: add version alias tag in registry
    if let Some(ref ver) = version
        && state.config.registry_url.is_some()
    {
        match crate::registry::copy_tag(&state.pool, &project_name, &image_tag, ver).await {
            Ok(()) => tracing::info!(version = %ver, "added version tag alias"),
            Err(e) => tracing::warn!(
                error = %e,
                version = %ver,
                "version tag already exists or copy failed"
            ),
        }
    }

    // Look up ops repo for this project
    let ops_repo = sqlx::query!(
        "SELECT id, repo_path, branch FROM ops_repos WHERE project_id = $1",
        project_id,
    )
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();

    let ops_repo_id = ops_repo.as_ref().map(|o| o.id);

    // Create or update the production deployment row so the reconciler picks it up
    if let Err(e) = sqlx::query!(
        r#"INSERT INTO deployments (project_id, environment, image_ref, ops_repo_id,
                                    desired_status, current_status)
           VALUES ($1, 'production', $2, $3, 'active', 'pending')
           ON CONFLICT (project_id, environment)
           DO UPDATE SET image_ref = $2,
                         ops_repo_id = COALESCE($3, deployments.ops_repo_id),
                         desired_status = 'active',
                         current_status = 'pending'"#,
        project_id,
        image_ref,
        ops_repo_id,
    )
    .execute(&state.pool)
    .await
    {
        tracing::warn!(error = %e, "post-merge deployment upsert failed");
    } else {
        tracing::info!(%project_id, %image_ref, "production deployment created/updated (post-merge)");
    }

    // Sync deploy/ to ops repo + commit values
    if let Some(ops) = ops_repo {
        let merge_sha = get_branch_head_sha(repo_path, target_branch).await;
        if let Some(sha) = merge_sha {
            let ops_path = std::path::PathBuf::from(&ops.repo_path);
            if let Err(e) = crate::deployer::ops_repo::sync_from_project_repo(
                repo_path,
                &ops_path,
                &ops.branch,
                &sha,
            )
            .await
            {
                tracing::warn!(error = %e, "post-merge deploy/ sync failed");
            }

            let values = serde_json::json!({
                "image_ref": image_ref,
                "version": version.as_deref().unwrap_or(short_sha),
            });
            if let Err(e) = crate::deployer::ops_repo::commit_values(
                &ops_path,
                &ops.branch,
                "production",
                &values,
            )
            .await
            {
                tracing::warn!(error = %e, "post-merge values commit failed");
            }
        }
    }

    // Wake reconciler
    state.deploy_notify.notify_one();
}
