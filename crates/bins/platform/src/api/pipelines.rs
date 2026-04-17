// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

#[allow(dead_code, unused_imports)]
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::state::PlatformState;
use platform_types::ApiError;
use platform_types::AuthUser;
use platform_types::validation;
use platform_types::{AuditEntry, send_audit};

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
    pub gate: bool,
    pub depends_on: Vec<String>,
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

use super::helpers::{ListResponse, require_project_read, require_project_write};

// ---------------------------------------------------------------------------
// UI Preview types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct UiPreviewsQuery {
    pub branch: Option<String>,
    #[serde(rename = "type")]
    pub artifact_type: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UiPreviewsCompareQuery {
    pub base: String,
    pub head: String,
    #[serde(rename = "type")]
    pub artifact_type: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct UiPreviewArtifactResponse {
    pub id: Uuid,
    pub name: String,
    pub artifact_type: String,
    pub config: Option<serde_json::Value>,
    pub pipeline_id: Uuid,
    pub branch: String,
    pub created_at: DateTime<Utc>,
    pub files: Vec<UiPreviewFileResponse>,
}

#[derive(Debug, Serialize)]
pub struct UiPreviewFileResponse {
    pub id: Uuid,
    pub relative_path: String,
    pub content_type: Option<String>,
    pub size_bytes: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct UiPreviewsCompareResponse {
    pub base: Vec<UiPreviewArtifactResponse>,
    pub head: Vec<UiPreviewArtifactResponse>,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<PlatformState> {
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
        .route(
            "/api/projects/{id}/pipelines/{pipeline_id}/artifacts/{artifact_id}/view",
            get(view_artifact_inline),
        )
        .route("/api/projects/{id}/ui-previews", get(ui_previews_by_branch))
        .route(
            "/api/projects/{id}/ui-previews/compare",
            get(ui_previews_compare),
        )
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state, body), fields(%id), err)]
async fn trigger_pipeline(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<TriggerRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // Rate limit: 60 pipeline triggers per hour per user
    platform_auth::rate_limit::check_rate(
        &state.valkey,
        "pipeline_trigger",
        &auth.user_id.to_string(),
        60,
        3600,
    )
    .await?;

    require_project_write(&state, &auth, id).await?;
    validation::check_branch_name(&body.git_ref)?;

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
    let pipeline_id = platform_pipeline::trigger::on_api(
        &state.pool,
        &repo_path,
        id,
        &body.git_ref,
        auth.user_id,
        &state.config.pipeline.kaniko_image,
    )
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;

    // Notify executor
    platform_pipeline::trigger::notify_executor(&state.pipeline_notify, &state.valkey, pipeline_id)
        .await;

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "pipeline.create".into(),
            resource: "pipeline".into(),
            resource_id: Some(pipeline_id),
            project_id: Some(id),
            detail: Some(serde_json::json!({"git_ref": body.git_ref, "trigger": "api"})),
            ip_addr: auth.ip_addr.clone(),
        },
    );

    // Fetch the created pipeline to return
    let pipeline = fetch_pipeline(&state, pipeline_id).await?;
    Ok((StatusCode::CREATED, Json(pipeline)))
}

async fn list_pipelines(
    State(state): State<PlatformState>,
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
    State(state): State<PlatformState>,
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
        SELECT id, step_order, name, image, status, exit_code, duration_ms, log_ref,
               gate, depends_on, created_at
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
            gate: s.gate,
            depends_on: s.depends_on,
            created_at: s.created_at,
        })
        .collect();

    Ok(Json(PipelineDetailResponse { pipeline, steps }))
}

#[tracing::instrument(skip(state), fields(%id, %pipeline_id), err)]
async fn cancel_pipeline(
    State(state): State<PlatformState>,
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

    // Inline cancel logic (avoids needing PipelineServices trait impl)
    let current_status_str =
        sqlx::query_scalar::<_, String>("SELECT status FROM pipelines WHERE id = $1")
            .bind(pipeline_id)
            .fetch_one(&state.pool)
            .await?;

    let to = platform_pipeline::PipelineStatus::Cancelled;
    if let Some(current) = platform_pipeline::PipelineStatus::parse(&current_status_str) {
        if !current.can_transition_to(to) {
            return Err(ApiError::BadRequest(format!(
                "cannot cancel pipeline in status '{current_status_str}'"
            )));
        }
    } else {
        return Err(ApiError::BadRequest(format!(
            "unknown pipeline status '{current_status_str}'"
        )));
    }

    sqlx::query(
        "UPDATE pipelines SET status = $2, finished_at = now() WHERE id = $1 AND status = $3",
    )
    .bind(pipeline_id)
    .bind(to.as_str())
    .bind(&current_status_str)
    .execute(&state.pool)
    .await?;

    // Skip remaining pending steps
    sqlx::query(
        "UPDATE pipeline_steps SET status = 'skipped', finished_at = now() WHERE pipeline_id = $1 AND status = 'pending'",
    )
    .bind(pipeline_id)
    .execute(&state.pool)
    .await?;

    // Delete running pods (best-effort)
    let short_id = &pipeline_id.to_string()[..8];
    let ns_slug = sqlx::query_scalar::<_, String>(
        "SELECT p.namespace_slug FROM pipelines pl JOIN projects p ON p.id = pl.project_id WHERE pl.id = $1",
    )
    .bind(pipeline_id)
    .fetch_optional(&state.pool)
    .await?;

    let namespace = ns_slug.map_or_else(
        || state.config.pipeline.pipeline_namespace.clone(),
        |slug| {
            platform_k8s::pipeline_namespace_name(
                state.config.core.ns_prefix.as_deref(),
                &slug,
                short_id,
            )
        },
    );

    let pods: kube::Api<k8s_openapi::api::core::v1::Pod> =
        kube::Api::namespaced(state.kube.clone(), &namespace);
    let label = format!("platform.io/pipeline={pipeline_id}");
    let lp = kube::api::ListParams::default().labels(&label);
    if let Ok(pod_list) = pods.list(&lp).await {
        for pod in pod_list {
            if let Some(name) = pod.metadata.name {
                let _ = pods
                    .delete(&name, &kube::api::DeleteParams::default())
                    .await;
            }
        }
    }

    send_audit(
        &state.audit_tx,
        AuditEntry {
            actor_id: auth.user_id,
            actor_name: auth.user_name.clone(),
            action: "pipeline.cancel".into(),
            resource: "pipeline".into(),
            resource_id: Some(pipeline_id),
            project_id: Some(id),
            detail: None,
            ip_addr: auth.ip_addr.clone(),
        },
    );

    Ok(Json(serde_json::json!({"ok": true})))
}

async fn get_step_logs(
    State(state): State<PlatformState>,
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
        match state.minio.read(log_ref).await {
            Ok(data) => {
                let body = Body::from(data.to_vec());
                Ok(Response::builder()
                    .header("content-type", "text/plain; charset=utf-8")
                    .body(body)
                    .expect("infallible: valid status and header"))
            }
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => {
                Err(ApiError::NotFound("step logs".into()))
            }
            Err(e) => Err(e.into()),
        }
    } else {
        Ok(Response::builder()
            .header("content-type", "text/plain; charset=utf-8")
            .body(Body::from("No logs available"))
            .expect("infallible: valid status and header"))
    }
}

async fn stream_live_logs(
    state: &PlatformState,
    pipeline_id: Uuid,
    step_name: &str,
) -> Result<Response, ApiError> {
    let namespace = &state.config.pipeline.pipeline_namespace;
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
            .expect("infallible: valid status and header")),
        Err(kube::Error::Api(err_resp)) if err_resp.code == 404 => Ok(Response::builder()
            .header("content-type", "text/plain; charset=utf-8")
            .body(Body::from("Logs not yet available — pod not started"))
            .expect("infallible: valid status and header")),
        Err(e) => {
            tracing::warn!(error = %e, %pipeline_id, step = step_name, "failed to stream pod logs");
            Ok(Response::builder()
                .header("content-type", "text/plain; charset=utf-8")
                .body(Body::from("Logs temporarily unavailable"))
                .expect("infallible: valid status and header"))
        }
    }
}

async fn list_artifacts(
    State(state): State<PlatformState>,
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
    State(state): State<PlatformState>,
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
            format!(
                "attachment; filename=\"{}\"",
                sanitize_filename(&artifact.name)
            ),
        )
        .body(Body::from(data.to_vec()))
        .expect("infallible: valid status and header"))
}

// ---------------------------------------------------------------------------
// Inline view (Content-Disposition: inline)
// ---------------------------------------------------------------------------

async fn view_artifact_inline(
    State(state): State<PlatformState>,
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
        .header("content-disposition", "inline")
        .body(Body::from(data.to_vec()))
        .expect("infallible: valid status and header"))
}

// ---------------------------------------------------------------------------
// UI Previews — query by branch
// ---------------------------------------------------------------------------

async fn ui_previews_by_branch(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Query(params): Query<UiPreviewsQuery>,
) -> Result<Json<Vec<UiPreviewArtifactResponse>>, ApiError> {
    require_project_read(&state, &auth, id).await?;

    let branch = params.branch.as_deref().unwrap_or("main");
    let git_ref = format!("refs/heads/{branch}");

    let previews = fetch_ui_previews(&state, id, &git_ref, params.artifact_type.as_deref()).await?;
    Ok(Json(previews))
}

// ---------------------------------------------------------------------------
// UI Previews — branch comparison
// ---------------------------------------------------------------------------

async fn ui_previews_compare(
    State(state): State<PlatformState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Query(params): Query<UiPreviewsCompareQuery>,
) -> Result<Json<UiPreviewsCompareResponse>, ApiError> {
    require_project_read(&state, &auth, id).await?;

    let base_ref = format!("refs/heads/{}", params.base);
    let head_ref = format!("refs/heads/{}", params.head);

    let base = fetch_ui_previews(&state, id, &base_ref, params.artifact_type.as_deref()).await?;
    let head = fetch_ui_previews(&state, id, &head_ref, params.artifact_type.as_deref()).await?;

    Ok(Json(UiPreviewsCompareResponse { base, head }))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Fetch UI preview artifacts for the latest successful pipeline matching a `git_ref`.
///
/// Uses dynamic queries because the new columns (`is_directory`, `artifact_type`,
/// `parent_id`, `config`, `relative_path`) are added by migration
/// `20260331010001_artifact_collection` -- the `.sqlx/` offline cache will be
/// regenerated during integration.
async fn fetch_ui_previews(
    state: &PlatformState,
    project_id: Uuid,
    git_ref: &str,
    artifact_type: Option<&str>,
) -> Result<Vec<UiPreviewArtifactResponse>, ApiError> {
    // Find latest successful pipeline matching the git_ref
    let pipeline: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT id, git_ref FROM pipelines \
         WHERE project_id = $1 AND git_ref = $2 AND status = 'success' \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(project_id)
    .bind(git_ref)
    .fetch_optional(&state.pool)
    .await?;

    let Some((pipeline_id, _)) = pipeline else {
        return Ok(vec![]);
    };

    // Default types for UI previews
    let type_filter: Vec<String> = artifact_type.map_or_else(
        || vec!["ui-comp".into(), "ui-flow".into()],
        |t| vec![t.into()],
    );

    let parents = fetch_parent_artifacts(&state.pool, pipeline_id, &type_filter).await?;

    let branch = git_ref.strip_prefix("refs/heads/").unwrap_or(git_ref);

    let mut result = Vec::with_capacity(parents.len());
    for (pid, name, art_type, config, pl_id, created_at) in &parents {
        let files = fetch_child_files(&state.pool, *pid).await?;

        result.push(UiPreviewArtifactResponse {
            id: *pid,
            name: name.clone(),
            artifact_type: art_type.clone().unwrap_or_default(),
            config: config.clone(),
            pipeline_id: *pl_id,
            branch: branch.to_string(),
            created_at: *created_at,
            files,
        });
    }

    Ok(result)
}

/// Row type for parent artifact queries (avoids complex inline tuple).
type ParentArtifactRow = (
    Uuid,
    String,
    Option<String>,
    Option<serde_json::Value>,
    Uuid,
    DateTime<Utc>,
);

/// Row type for child artifact queries.
type ChildArtifactRow = (Uuid, Option<String>, Option<String>, Option<i64>);

async fn fetch_parent_artifacts(
    pool: &sqlx::PgPool,
    pipeline_id: Uuid,
    type_filter: &[String],
) -> Result<Vec<ParentArtifactRow>, ApiError> {
    let rows: Vec<ParentArtifactRow> = sqlx::query_as(
        "SELECT id, name, artifact_type, config, pipeline_id, created_at \
         FROM artifacts \
         WHERE pipeline_id = $1 AND is_directory = true AND artifact_type = ANY($2::text[]) \
         ORDER BY created_at ASC",
    )
    .bind(pipeline_id)
    .bind(type_filter)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

async fn fetch_child_files(
    pool: &sqlx::PgPool,
    parent_id: Uuid,
) -> Result<Vec<UiPreviewFileResponse>, ApiError> {
    let children: Vec<ChildArtifactRow> = sqlx::query_as(
        "SELECT id, relative_path, content_type, size_bytes \
         FROM artifacts WHERE parent_id = $1 ORDER BY relative_path ASC",
    )
    .bind(parent_id)
    .fetch_all(pool)
    .await?;

    Ok(children
        .into_iter()
        .map(|(id, rel_path, ct, sz)| UiPreviewFileResponse {
            id,
            relative_path: rel_path.unwrap_or_default(),
            content_type: ct,
            size_bytes: sz,
        })
        .collect())
}

async fn fetch_pipeline(
    state: &PlatformState,
    pipeline_id: Uuid,
) -> Result<PipelineResponse, ApiError> {
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

use platform_pipeline::slug;

/// Sanitize a filename for use in Content-Disposition headers.
/// Only allows alphanumeric characters, hyphens, underscores, and dots.
fn sanitize_filename(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
        .collect();
    if sanitized.is_empty() {
        "download".into()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_filename_preserves_normal() {
        assert_eq!(sanitize_filename("report.tar.gz"), "report.tar.gz");
    }

    #[test]
    fn sanitize_filename_strips_injection() {
        assert_eq!(
            sanitize_filename("file\".html\r\nX-Bad: injected"),
            "file.htmlX-Badinjected"
        );
    }

    #[test]
    fn sanitize_filename_fallback_on_empty() {
        assert_eq!(sanitize_filename(""), "download");
        assert_eq!(sanitize_filename("///"), "download");
    }
}
