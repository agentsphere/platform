use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
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

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct TriggerRequest {
    pub git_ref: String,
}

#[derive(Debug, Deserialize)]
pub struct ListPipelinesParams {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub status: Option<String>,
    pub git_ref: Option<String>,
    pub trigger: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PipelineResponse {
    pub id: Uuid,
    pub project_id: Uuid,
    pub trigger: String,
    pub git_ref: String,
    pub commit_sha: Option<String>,
    pub status: String,
    pub triggered_by: Option<Uuid>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct PipelineDetailResponse {
    #[serde(flatten)]
    pub pipeline: PipelineResponse,
    pub steps: Vec<StepResponse>,
}

#[derive(Debug, Serialize)]
pub struct StepResponse {
    pub id: Uuid,
    pub step_order: i32,
    pub name: String,
    pub image: String,
    pub status: String,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<i32>,
    pub log_ref: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct ArtifactResponse {
    pub id: Uuid,
    pub name: String,
    pub content_type: Option<String>,
    pub size_bytes: Option<i64>,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
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
            "/api/projects/{id}/pipelines",
            get(list_pipelines).post(trigger_pipeline),
        )
        .route(
            "/api/projects/{id}/pipelines/{pipeline_id}",
            get(get_pipeline),
        )
        .route(
            "/api/projects/{id}/pipelines/{pipeline_id}/cancel",
            axum::routing::post(cancel_pipeline),
        )
        .route(
            "/api/projects/{id}/pipelines/{pipeline_id}/steps/{step_id}/logs",
            get(get_step_logs),
        )
        .route(
            "/api/projects/{id}/pipelines/{pipeline_id}/artifacts",
            get(list_artifacts),
        )
        .route(
            "/api/projects/{id}/pipelines/{pipeline_id}/artifacts/{artifact_id}/download",
            get(download_artifact),
        )
}

// ---------------------------------------------------------------------------
// Permission helper
// ---------------------------------------------------------------------------

async fn require_project_read(
    state: &AppState,
    auth: &AuthUser,
    project_id: Uuid,
) -> Result<(), ApiError> {
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
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

async fn require_project_write(
    state: &AppState,
    auth: &AuthUser,
    project_id: Uuid,
) -> Result<(), ApiError> {
    let allowed = resolver::has_permission(
        &state.pool,
        &state.valkey,
        auth.user_id,
        Some(project_id),
        Permission::ProjectWrite,
    )
    .await
    .map_err(ApiError::Internal)?;

    if !allowed {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state, body), fields(%id), err)]
async fn trigger_pipeline(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<TriggerRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_project_write(&state, &auth, id).await?;

    let project = sqlx::query!(
        "SELECT repo_path FROM projects WHERE id = $1 AND is_active = true",
        id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("project".into()))?;

    let repo_path = std::path::PathBuf::from(
        project
            .repo_path
            .ok_or_else(|| ApiError::BadRequest("project has no repo path".into()))?,
    );
    let pipeline_id =
        crate::pipeline::trigger::on_api(&state.pool, &repo_path, id, &body.git_ref, auth.user_id)
            .await
            .map_err(ApiError::from)?;

    // Notify executor
    crate::pipeline::trigger::notify_executor(&state.valkey, pipeline_id).await;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "pipeline.create",
            resource: "pipeline",
            resource_id: Some(pipeline_id),
            project_id: Some(id),
            detail: Some(serde_json::json!({"git_ref": body.git_ref, "trigger": "api"})),
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    // Fetch the created pipeline to return
    let pipeline = fetch_pipeline(&state, pipeline_id).await?;
    Ok((StatusCode::CREATED, Json(pipeline)))
}

async fn list_pipelines(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Query(params): Query<ListPipelinesParams>,
) -> Result<Json<ListResponse<PipelineResponse>>, ApiError> {
    require_project_read(&state, &auth, id).await?;

    let limit = params.limit.unwrap_or(50).min(100);
    let offset = params.offset.unwrap_or(0);

    let total = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) as "count!: i64" FROM pipelines
        WHERE project_id = $1
          AND ($2::text IS NULL OR status = $2)
          AND ($3::text IS NULL OR git_ref = $3)
          AND ($4::text IS NULL OR trigger = $4)
        "#,
        id,
        params.status,
        params.git_ref,
        params.trigger,
    )
    .fetch_one(&state.pool)
    .await?;

    let rows = sqlx::query!(
        r#"
        SELECT id, project_id, trigger, git_ref, commit_sha, status,
               triggered_by, started_at, finished_at, created_at
        FROM pipelines
        WHERE project_id = $1
          AND ($2::text IS NULL OR status = $2)
          AND ($3::text IS NULL OR git_ref = $3)
          AND ($4::text IS NULL OR trigger = $4)
        ORDER BY created_at DESC
        LIMIT $5 OFFSET $6
        "#,
        id,
        params.status,
        params.git_ref,
        params.trigger,
        limit,
        offset,
    )
    .fetch_all(&state.pool)
    .await?;

    let items = rows
        .into_iter()
        .map(|r| PipelineResponse {
            id: r.id,
            project_id: r.project_id,
            trigger: r.trigger,
            git_ref: r.git_ref,
            commit_sha: r.commit_sha,
            status: r.status,
            triggered_by: r.triggered_by,
            started_at: r.started_at,
            finished_at: r.finished_at,
            created_at: r.created_at,
        })
        .collect();

    Ok(Json(ListResponse { items, total }))
}

async fn get_pipeline(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, pipeline_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<PipelineDetailResponse>, ApiError> {
    require_project_read(&state, &auth, id).await?;

    let pipeline = fetch_pipeline(&state, pipeline_id).await?;

    // Verify the pipeline belongs to this project
    if pipeline.project_id != id {
        return Err(ApiError::NotFound("pipeline".into()));
    }

    let steps = sqlx::query!(
        r#"
        SELECT id, step_order, name, image, status, exit_code, duration_ms, log_ref, created_at
        FROM pipeline_steps
        WHERE pipeline_id = $1
        ORDER BY step_order ASC
        "#,
        pipeline_id,
    )
    .fetch_all(&state.pool)
    .await?;

    let steps = steps
        .into_iter()
        .map(|s| StepResponse {
            id: s.id,
            step_order: s.step_order,
            name: s.name,
            image: s.image,
            status: s.status,
            exit_code: s.exit_code,
            duration_ms: s.duration_ms,
            log_ref: s.log_ref,
            created_at: s.created_at,
        })
        .collect();

    Ok(Json(PipelineDetailResponse { pipeline, steps }))
}

#[tracing::instrument(skip(state), fields(%id, %pipeline_id), err)]
async fn cancel_pipeline(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, pipeline_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_project_write(&state, &auth, id).await?;

    // Verify the pipeline belongs to this project
    let exists = sqlx::query_scalar!(
        "SELECT COUNT(*) > 0 as \"exists!: bool\" FROM pipelines WHERE id = $1 AND project_id = $2",
        pipeline_id,
        id,
    )
    .fetch_one(&state.pool)
    .await?;

    if !exists {
        return Err(ApiError::NotFound("pipeline".into()));
    }

    crate::pipeline::executor::cancel_pipeline(&state, pipeline_id)
        .await
        .map_err(ApiError::from)?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: auth.user_id,
            actor_name: &auth.user_name,
            action: "pipeline.cancel",
            resource: "pipeline",
            resource_id: Some(pipeline_id),
            project_id: Some(id),
            detail: None,
            ip_addr: auth.ip_addr.as_deref(),
        },
    )
    .await;

    Ok(Json(serde_json::json!({"ok": true})))
}

async fn get_step_logs(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, pipeline_id, step_id)): Path<(Uuid, Uuid, Uuid)>,
) -> Result<Response, ApiError> {
    require_project_read(&state, &auth, id).await?;

    let step = sqlx::query!(
        r#"
        SELECT status, log_ref, name
        FROM pipeline_steps
        WHERE id = $1 AND pipeline_id = $2
        "#,
        step_id,
        pipeline_id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("step".into()))?;

    // Verify pipeline belongs to project
    let belongs = sqlx::query_scalar!(
        "SELECT COUNT(*) > 0 as \"belongs!: bool\" FROM pipelines WHERE id = $1 AND project_id = $2",
        pipeline_id,
        id,
    )
    .fetch_one(&state.pool)
    .await?;

    if !belongs {
        return Err(ApiError::NotFound("pipeline".into()));
    }

    if step.status == "running" {
        // Stream live logs from K8s
        return stream_live_logs(&state, pipeline_id, &step.name).await;
    }

    // Read stored logs from MinIO
    if let Some(ref log_ref) = step.log_ref {
        let data = state.minio.read(log_ref).await?;
        let body = Body::from(data.to_vec());
        Ok(Response::builder()
            .header("content-type", "text/plain; charset=utf-8")
            .body(body)
            .unwrap())
    } else {
        Ok(Response::builder()
            .header("content-type", "text/plain; charset=utf-8")
            .body(Body::from("No logs available"))
            .unwrap())
    }
}

async fn stream_live_logs(
    state: &AppState,
    pipeline_id: Uuid,
    step_name: &str,
) -> Result<Response, ApiError> {
    let namespace = &state.config.pipeline_namespace;
    let pods: kube::Api<k8s_openapi::api::core::v1::Pod> =
        kube::Api::namespaced(state.kube.clone(), namespace);

    let pod_name = format!("pl-{}-{}", &pipeline_id.to_string()[..8], slug(step_name));

    let log_params = kube::api::LogParams {
        container: Some("step".into()),
        follow: true,
        ..Default::default()
    };

    match pods.logs(&pod_name, &log_params).await {
        Ok(logs) => Ok(Response::builder()
            .header("content-type", "text/plain; charset=utf-8")
            .body(Body::from(logs))
            .unwrap()),
        Err(_) => Ok(Response::builder()
            .header("content-type", "text/plain; charset=utf-8")
            .body(Body::from("Logs not yet available"))
            .unwrap()),
    }
}

async fn list_artifacts(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, pipeline_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Vec<ArtifactResponse>>, ApiError> {
    require_project_read(&state, &auth, id).await?;

    // Verify pipeline belongs to project
    let belongs = sqlx::query_scalar!(
        "SELECT COUNT(*) > 0 as \"belongs!: bool\" FROM pipelines WHERE id = $1 AND project_id = $2",
        pipeline_id,
        id,
    )
    .fetch_one(&state.pool)
    .await?;

    if !belongs {
        return Err(ApiError::NotFound("pipeline".into()));
    }

    let rows = sqlx::query!(
        r#"
        SELECT id, name, content_type, size_bytes, expires_at, created_at
        FROM artifacts
        WHERE pipeline_id = $1
        ORDER BY created_at ASC
        "#,
        pipeline_id,
    )
    .fetch_all(&state.pool)
    .await?;

    let items = rows
        .into_iter()
        .map(|a| ArtifactResponse {
            id: a.id,
            name: a.name,
            content_type: a.content_type,
            size_bytes: a.size_bytes,
            expires_at: a.expires_at,
            created_at: a.created_at,
        })
        .collect();

    Ok(Json(items))
}

async fn download_artifact(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, pipeline_id, artifact_id)): Path<(Uuid, Uuid, Uuid)>,
) -> Result<Response, ApiError> {
    require_project_read(&state, &auth, id).await?;

    let artifact = sqlx::query!(
        r#"
        SELECT a.minio_path, a.content_type, a.name
        FROM artifacts a
        JOIN pipelines p ON p.id = a.pipeline_id
        WHERE a.id = $1 AND a.pipeline_id = $2 AND p.project_id = $3
        "#,
        artifact_id,
        pipeline_id,
        id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("artifact".into()))?;

    let data = state.minio.read(&artifact.minio_path).await?;
    let content_type = artifact
        .content_type
        .as_deref()
        .unwrap_or("application/octet-stream");

    Ok(Response::builder()
        .header("content-type", content_type)
        .header(
            "content-disposition",
            format!("attachment; filename=\"{}\"", artifact.name),
        )
        .body(Body::from(data.to_vec()))
        .unwrap())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn fetch_pipeline(state: &AppState, pipeline_id: Uuid) -> Result<PipelineResponse, ApiError> {
    let row = sqlx::query!(
        r#"
        SELECT id, project_id, trigger, git_ref, commit_sha, status,
               triggered_by, started_at, finished_at, created_at
        FROM pipelines WHERE id = $1
        "#,
        pipeline_id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound("pipeline".into()))?;

    Ok(PipelineResponse {
        id: row.id,
        project_id: row.project_id,
        trigger: row.trigger,
        git_ref: row.git_ref,
        commit_sha: row.commit_sha,
        status: row.status,
        triggered_by: row.triggered_by,
        started_at: row.started_at,
        finished_at: row.finished_at,
        created_at: row.created_at,
    })
}

fn slug(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_owned()
}
