use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::audit::{AuditEntry, write_audit};
use crate::auth::middleware::AuthUser;
use crate::error::ApiError;
use crate::rbac::{Permission, resolver};
use crate::store::AppState;
use crate::validation;

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

#[derive(Debug, Serialize)]
pub struct ListResponse<T: Serialize> {
    pub items: Vec<T>,
    pub total: i64,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/projects/{id}/issues",
            get(list_issues).post(create_issue),
        )
        .route(
            "/api/projects/{id}/issues/{number}",
            get(get_issue).patch(update_issue),
        )
        .route(
            "/api/projects/{id}/issues/{number}/comments",
            get(list_comments).post(create_comment),
        )
        .route(
            "/api/projects/{id}/issues/{number}/comments/{comment_id}",
            axum::routing::patch(update_comment),
        )
}

async fn require_project_read(
    state: &AppState,
    auth: &AuthUser,
    project_id: Uuid,
) -> Result<(), ApiError> {
    let project = sqlx::query!(
        "SELECT visibility, owner_id FROM projects WHERE id = $1 AND is_active = true",
        project_id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("project".into()))?;

    if project.visibility == "public"
        || project.visibility == "internal"
        || project.owner_id == auth.user_id
    {
        return Ok(());
    }

    let allowed = resolver::has_permission(
        &state.pool,
        &state.valkey,
        auth.user_id,
        Some(project_id),
        Permission::ProjectRead,
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::NotFound("project".into()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Issue handlers
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state, body), fields(%id), err)]
async fn create_issue(
    State(state): State<AppState>,
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

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "issue.create",
            resource: "issue",
            resource_id: Some(issue.id),
            project_id: Some(id),
            detail: Some(serde_json::json!({"number": number, "title": body.title})),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    // Fire webhooks
    crate::api::webhooks::fire_webhooks(
        &state.pool,
        id,
        "issue",
        &serde_json::json!({
            "action": "created",
            "issue": {"id": issue.id, "number": number, "title": body.title},
        }),
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
    State(state): State<AppState>,
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
    State(state): State<AppState>,
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
    State(state): State<AppState>,
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

    // Author or project:write
    let issue_author = sqlx::query_scalar!(
        "SELECT author_id FROM issues WHERE project_id = $1 AND number = $2",
        id,
        number,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("issue".into()))?;

    if issue_author != auth.user_id {
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

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "issue.update",
            resource: "issue",
            resource_id: Some(issue.id),
            project_id: Some(id),
            detail: None,
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

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

// ---------------------------------------------------------------------------
// Comment handlers
// ---------------------------------------------------------------------------

async fn list_comments(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, number)): Path<(Uuid, i32)>,
) -> Result<Json<Vec<CommentResponse>>, ApiError> {
    require_project_read(&state, &auth, id).await?;

    let issue_id = sqlx::query_scalar!(
        "SELECT id FROM issues WHERE project_id = $1 AND number = $2",
        id,
        number,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("issue".into()))?;

    let rows = sqlx::query!(
        r#"
        SELECT id, author_id, body, created_at, updated_at
        FROM comments WHERE issue_id = $1
        ORDER BY created_at ASC
        "#,
        issue_id,
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

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "comment.create",
            resource: "comment",
            resource_id: Some(comment.id),
            project_id: Some(id),
            detail: Some(serde_json::json!({"issue_number": number})),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

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
