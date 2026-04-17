// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

#[allow(dead_code, unused_imports)]
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use platform_auth::resolver;
use platform_types::validation;
use platform_types::{ApiError, AuthUser, Permission};
use platform_types::{AuditEntry, send_audit};

use crate::state::PlatformState;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateIssueRequest {
    pub title: String,
    pub body: Option<String>,
    pub labels: Option<Vec<String>>,
    pub assignee_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateIssueRequest {
    pub title: Option<String>,
    pub body: Option<String>,
    pub status: Option<String>,
    pub labels: Option<Vec<String>>,
    pub assignee_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct ListIssuesParams {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub status: Option<String>,
    pub assignee_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct ListCommentsParams {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
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
pub struct IssueResponse {
    pub id: Uuid,
    pub project_id: Uuid,
    pub number: i32,
    pub author_id: Uuid,
    pub title: String,
    pub body: Option<String>,
    pub status: String,
    pub labels: Vec<String>,
    pub assignee_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct CommentResponse {
    pub id: Uuid,
    pub author_id: Uuid,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

use super::helpers::{require_project_read, require_project_write};
use platform_types::ListResponse;

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<PlatformState> {
    Router::new()
        .route(
            "/api/projects/{id}/issues",
            get(list_issues).post(create_issue),
        )
        .route(
            "/api/projects/{id}/issues/{number}",
            get(get_issue).patch(update_issue).delete(delete_issue),
        )
        .route(
            "/api/projects/{id}/issues/{number}/comments",
            get(list_comments).post(create_comment),
        )
        .route(
            "/api/projects/{id}/issues/{number}/comments/{comment_id}",
            get(get_comment)
                .patch(update_comment)
                .delete(delete_comment),
        )
}

// ---------------------------------------------------------------------------
// Issue handlers
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state, body), fields(%id), err)]
async fn create_issue(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<CreateIssueRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // Validate input
    validation::check_length("title", &body.title, 1, 500)?;
    if let Some(ref b) = body.body {
        validation::check_length("body", b, 0, 100_000)?;
    }
    if let Some(ref labels) = body.labels {
        validation::check_labels(labels)?;
    }

    // Creating issues requires project:write
    let allowed = resolver::has_permission_scoped(
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

    // Atomic increment of issue number
    let number = sqlx::query_scalar!(
        r#"
        UPDATE projects SET next_issue_number = next_issue_number + 1
        WHERE id = $1 AND is_active = true
        RETURNING next_issue_number
        "#,
        id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("project".into()))?;

    let labels = body.labels.unwrap_or_default();

    let issue = sqlx::query!(
        r#"
        INSERT INTO issues (project_id, number, author_id, title, body, labels, assignee_id)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING id, project_id, number, author_id, title, body, status, labels, assignee_id, created_at, updated_at
        "#,
        id,
        number,
        auth.user_id,
        body.title,
        body.body,
        &labels,
        body.assignee_id,
    )
    .fetch_one(&state.pool)
    .await?;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "issue.create".into(),
            resource: "issue".into(),
            resource_id: Some(issue.id),
            project_id: Some(id),
            detail: Some(serde_json::json!({"number": number, "title": body.title})),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    // Fire webhooks
    crate::api::webhooks::fire_webhooks(
        &state.pool,
        id,
        "issue",
        &serde_json::json!({
            "action": "created",
            "issue": {"id": issue.id, "number": number, "title": body.title},
        }),
        &state.webhook_semaphore,
    )
    .await;

    Ok((
        StatusCode::CREATED,
        Json(IssueResponse {
            id: issue.id,
            project_id: issue.project_id,
            number: issue.number,
            author_id: issue.author_id,
            title: issue.title,
            body: issue.body,
            status: issue.status,
            labels: issue.labels,
            assignee_id: issue.assignee_id,
            created_at: issue.created_at,
            updated_at: issue.updated_at,
        }),
    ))
}

async fn list_issues(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Query(params): Query<ListIssuesParams>,
) -> Result<Json<ListResponse<IssueResponse>>, ApiError> {
    require_project_read(&state, &auth, id).await?;

    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);

    let total = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) as "count!: i64"
        FROM issues
        WHERE project_id = $1
          AND ($2::text IS NULL OR status = $2)
          AND ($3::uuid IS NULL OR assignee_id = $3)
        "#,
        id,
        params.status,
        params.assignee_id,
    )
    .fetch_one(&state.pool)
    .await?;

    let rows = sqlx::query!(
        r#"
        SELECT id, project_id, number, author_id, title, body, status, labels, assignee_id, created_at, updated_at
        FROM issues
        WHERE project_id = $1
          AND ($2::text IS NULL OR status = $2)
          AND ($3::uuid IS NULL OR assignee_id = $3)
        ORDER BY number DESC
        LIMIT $4 OFFSET $5
        "#,
        id,
        params.status,
        params.assignee_id,
        limit,
        offset,
    )
    .fetch_all(&state.pool)
    .await?;

    let items = rows
        .into_iter()
        .map(|i| IssueResponse {
            id: i.id,
            project_id: i.project_id,
            number: i.number,
            author_id: i.author_id,
            title: i.title,
            body: i.body,
            status: i.status,
            labels: i.labels,
            assignee_id: i.assignee_id,
            created_at: i.created_at,
            updated_at: i.updated_at,
        })
        .collect();

    Ok(Json(ListResponse { items, total }))
}

async fn get_issue(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((id, number)): Path<(Uuid, i32)>,
) -> Result<Json<IssueResponse>, ApiError> {
    require_project_read(&state, &auth, id).await?;

    let issue = sqlx::query!(
        r#"
        SELECT id, project_id, number, author_id, title, body, status, labels, assignee_id, created_at, updated_at
        FROM issues WHERE project_id = $1 AND number = $2
        "#,
        id,
        number,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("issue".into()))?;

    Ok(Json(IssueResponse {
        id: issue.id,
        project_id: issue.project_id,
        number: issue.number,
        author_id: issue.author_id,
        title: issue.title,
        body: issue.body,
        status: issue.status,
        labels: issue.labels,
        assignee_id: issue.assignee_id,
        created_at: issue.created_at,
        updated_at: issue.updated_at,
    }))
}

#[tracing::instrument(skip(state, body), fields(%id, %number), err)]
async fn update_issue(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((id, number)): Path<(Uuid, i32)>,
    Json(body): Json<UpdateIssueRequest>,
) -> Result<Json<IssueResponse>, ApiError> {
    // Validate input
    if let Some(ref t) = body.title {
        validation::check_length("title", t, 1, 500)?;
    }
    if let Some(ref b) = body.body {
        validation::check_length("body", b, 0, 100_000)?;
    }
    if let Some(ref labels) = body.labels {
        validation::check_labels(labels)?;
    }

    // A49: Even the author needs current project-write permission (may have been revoked)
    require_project_write(&state, &auth, id).await?;

    // Verify issue exists and check authorship (non-authors also need admin to edit)
    let issue_author = sqlx::query_scalar!(
        "SELECT author_id FROM issues WHERE project_id = $1 AND number = $2",
        id,
        number,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("issue".into()))?;

    if issue_author != auth.user_id {
        let is_admin = resolver::has_permission_scoped(
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

    if let Some(ref status) = body.status
        && !["open", "closed"].contains(&status.as_str())
    {
        return Err(ApiError::BadRequest("status must be open or closed".into()));
    }

    let issue = sqlx::query!(
        r#"
        UPDATE issues SET
            title = COALESCE($3, title),
            body = COALESCE($4, body),
            status = COALESCE($5, status),
            labels = COALESCE($6, labels),
            assignee_id = COALESCE($7, assignee_id)
        WHERE project_id = $1 AND number = $2
        RETURNING id, project_id, number, author_id, title, body, status, labels, assignee_id, created_at, updated_at
        "#,
        id,
        number,
        body.title,
        body.body,
        body.status,
        body.labels.as_deref(),
        body.assignee_id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("issue".into()))?;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "issue.update".into(),
            resource: "issue".into(),
            resource_id: Some(issue.id),
            project_id: Some(id),
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(Json(IssueResponse {
        id: issue.id,
        project_id: issue.project_id,
        number: issue.number,
        author_id: issue.author_id,
        title: issue.title,
        body: issue.body,
        status: issue.status,
        labels: issue.labels,
        assignee_id: issue.assignee_id,
        created_at: issue.created_at,
        updated_at: issue.updated_at,
    }))
}

#[tracing::instrument(skip(state), fields(%id, %number), err)]
async fn delete_issue(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((id, number)): Path<(Uuid, i32)>,
) -> Result<StatusCode, ApiError> {
    require_project_write(&state, &auth, id).await?;

    let row = sqlx::query(
        "UPDATE issues SET status = 'closed', updated_at = now() \
         WHERE project_id = $1 AND number = $2 AND status != 'closed' \
         RETURNING id",
    )
    .bind(id)
    .bind(number)
    .fetch_optional(&state.pool)
    .await?;

    if let Some(row) = row {
        let issue_id: Uuid = row.get("id");

        send_audit(
            &state.audit_tx,
            AuditEntry {
                actor_id: auth.user_id,
                actor_name: auth.user_name.clone(),
                action: "issue.delete".into(),
                resource: "issue".into(),
                resource_id: Some(issue_id),
                project_id: Some(id),
                detail: Some(serde_json::json!({"number": number})),
                ip_addr: auth.ip_addr.clone(),
            },
        );

        crate::api::webhooks::fire_webhooks(
            &state.pool,
            id,
            "issue",
            &serde_json::json!({
                "action": "closed",
                "issue": {"id": issue_id, "number": number},
            }),
            &state.webhook_semaphore,
        )
        .await;
    }

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Comment handlers
// ---------------------------------------------------------------------------

async fn list_comments(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((id, number)): Path<(Uuid, i32)>,
    Query(params): Query<ListCommentsParams>,
) -> Result<Json<ListResponse<CommentResponse>>, ApiError> {
    require_project_read(&state, &auth, id).await?;

    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);

    let issue_id = sqlx::query_scalar!(
        "SELECT id FROM issues WHERE project_id = $1 AND number = $2",
        id,
        number,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("issue".into()))?;

    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM comments WHERE issue_id = $1")
        .bind(issue_id)
        .fetch_one(&state.pool)
        .await
        .unwrap_or(Some(0))
        .unwrap_or(0);

    let rows = sqlx::query(
        "SELECT id, author_id, body, created_at, updated_at \
         FROM comments WHERE issue_id = $1 \
         ORDER BY created_at ASC \
         LIMIT $2 OFFSET $3",
    )
    .bind(issue_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.pool)
    .await?;

    let items = rows
        .into_iter()
        .map(|c| CommentResponse {
            id: c.get("id"),
            author_id: c.get("author_id"),
            body: c.get("body"),
            created_at: c.get("created_at"),
            updated_at: c.get("updated_at"),
        })
        .collect();

    Ok(Json(ListResponse { items, total }))
}

#[tracing::instrument(skip(state, body), fields(%id, %number), err)]
async fn create_comment(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((id, number)): Path<(Uuid, i32)>,
    Json(body): Json<CreateCommentRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_project_read(&state, &auth, id).await?;
    validation::check_length("body", &body.body, 1, 100_000)?;

    let issue_id = sqlx::query_scalar!(
        "SELECT id FROM issues WHERE project_id = $1 AND number = $2",
        id,
        number,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("issue".into()))?;

    let comment = sqlx::query!(
        r#"
        INSERT INTO comments (project_id, issue_id, author_id, body)
        VALUES ($1, $2, $3, $4)
        RETURNING id, author_id, body, created_at, updated_at
        "#,
        id,
        issue_id,
        auth.user_id,
        body.body,
    )
    .fetch_one(&state.pool)
    .await?;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "comment.create".into(),
            resource: "comment".into(),
            resource_id: Some(comment.id),
            project_id: Some(id),
            detail: Some(serde_json::json!({"issue_number": number})),
            ip_addr: auth.ip_addr.clone(),
        },
    );

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
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((id, number, comment_id)): Path<(Uuid, i32, Uuid)>,
    Json(body): Json<UpdateCommentRequest>,
) -> Result<Json<CommentResponse>, ApiError> {
    validation::check_length("body", &body.body, 1, 100_000)?;

    // A49: Even the author needs current project-write permission (may have been revoked)
    require_project_write(&state, &auth, id).await?;

    // Verify issue exists
    let _issue_id = sqlx::query_scalar!(
        "SELECT id FROM issues WHERE project_id = $1 AND number = $2",
        id,
        number,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("issue".into()))?;

    // Check comment author or admin
    let comment_author =
        sqlx::query_scalar!("SELECT author_id FROM comments WHERE id = $1", comment_id,)
            .fetch_optional(&state.pool)
            .await?
            .ok_or_else(|| ApiError::NotFound("comment".into()))?;

    if comment_author != auth.user_id {
        let is_admin = resolver::has_permission_scoped(
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

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "comment.update".into(),
            resource: "comment".into(),
            resource_id: Some(comment_id),
            project_id: Some(id),
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(Json(CommentResponse {
        id: comment.id,
        author_id: comment.author_id,
        body: comment.body,
        created_at: comment.created_at,
        updated_at: comment.updated_at,
    }))
}

async fn get_comment(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((id, number, comment_id)): Path<(Uuid, i32, Uuid)>,
) -> Result<Json<CommentResponse>, ApiError> {
    require_project_read(&state, &auth, id).await?;

    // Verify issue exists
    let _issue_id: Uuid =
        sqlx::query("SELECT id FROM issues WHERE project_id = $1 AND number = $2")
            .bind(id)
            .bind(number)
            .fetch_optional(&state.pool)
            .await?
            .ok_or_else(|| ApiError::NotFound("issue".into()))?
            .get("id");

    let row = sqlx::query(
        "SELECT id, author_id, body, created_at, updated_at \
         FROM comments WHERE id = $1 AND project_id = $2",
    )
    .bind(comment_id)
    .bind(id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("comment".into()))?;

    Ok(Json(CommentResponse {
        id: row.get("id"),
        author_id: row.get("author_id"),
        body: row.get("body"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }))
}

#[tracing::instrument(skip(state), fields(%id, %number, %comment_id), err)]
async fn delete_comment(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path((id, number, comment_id)): Path<(Uuid, i32, Uuid)>,
) -> Result<StatusCode, ApiError> {
    require_project_write(&state, &auth, id).await?;

    // Verify issue exists
    let _issue_id: Uuid =
        sqlx::query("SELECT id FROM issues WHERE project_id = $1 AND number = $2")
            .bind(id)
            .bind(number)
            .fetch_optional(&state.pool)
            .await?
            .ok_or_else(|| ApiError::NotFound("issue".into()))?
            .get("id");

    // Check author or admin
    let comment_row =
        sqlx::query("SELECT author_id FROM comments WHERE id = $1 AND project_id = $2")
            .bind(comment_id)
            .bind(id)
            .fetch_optional(&state.pool)
            .await?
            .ok_or_else(|| ApiError::NotFound("comment".into()))?;

    let comment_author: Uuid = comment_row.get("author_id");

    if comment_author != auth.user_id {
        let is_admin = resolver::has_permission_scoped(
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

    sqlx::query("DELETE FROM comments WHERE id = $1 AND project_id = $2")
        .bind(comment_id)
        .bind(id)
        .execute(&state.pool)
        .await?;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "comment.delete".into(),
            resource: "comment".into(),
            resource_id: Some(comment_id),
            project_id: Some(id),
            detail: Some(serde_json::json!({"issue_number": number})),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(StatusCode::NO_CONTENT)
}
