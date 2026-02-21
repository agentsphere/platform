use std::path::Path;

use sqlx::PgPool;
use uuid::Uuid;

use super::definition::{self, PipelineDefinition};
use super::error::PipelineError;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

pub struct PushTriggerParams {
    pub project_id: Uuid,
    pub user_id: Uuid,
    pub repo_path: std::path::PathBuf,
    pub branch: String,
    pub commit_sha: Option<String>,
}

#[allow(dead_code)] // used when MR integration is wired
pub struct MrTriggerParams {
    pub project_id: Uuid,
    pub user_id: Uuid,
    pub repo_path: std::path::PathBuf,
    pub source_branch: String,
    pub commit_sha: Option<String>,
    pub action: String,
}

// ---------------------------------------------------------------------------
// Push trigger
// ---------------------------------------------------------------------------

/// Handle a push event: read `.platform.yaml`, check trigger match, create pipeline + steps.
///
/// Returns the pipeline ID if a pipeline was created.
#[tracing::instrument(skip(pool, params), fields(project_id = %params.project_id, branch = %params.branch), err)]
pub async fn on_push(
    pool: &PgPool,
    params: &PushTriggerParams,
) -> Result<Option<Uuid>, PipelineError> {
    let Some(yaml) = read_file_at_ref(&params.repo_path, &params.branch, ".platform.yaml").await
    else {
        return Ok(None);
    };

    let def = definition::parse(&yaml)?;

    if !definition::matches_push(def.trigger.as_ref(), &params.branch) {
        return Ok(None);
    }

    let git_ref = format!("refs/heads/{}", params.branch);
    let pipeline_id = create_pipeline_with_steps(
        pool,
        params.project_id,
        &git_ref,
        params.commit_sha.as_deref(),
        params.user_id,
        "push",
        &def,
    )
    .await?;

    tracing::info!(pipeline_id = %pipeline_id, "pipeline triggered by push");
    Ok(Some(pipeline_id))
}

// ---------------------------------------------------------------------------
// MR trigger
// ---------------------------------------------------------------------------

/// Handle a merge request event: read `.platform.yaml`, check trigger match, create pipeline + steps.
#[allow(dead_code)] // wired when MR integration is complete
#[tracing::instrument(skip(pool, params), fields(project_id = %params.project_id, source_branch = %params.source_branch), err)]
pub async fn on_mr(pool: &PgPool, params: &MrTriggerParams) -> Result<Option<Uuid>, PipelineError> {
    let Some(yaml) =
        read_file_at_ref(&params.repo_path, &params.source_branch, ".platform.yaml").await
    else {
        return Ok(None);
    };

    let def = definition::parse(&yaml)?;

    if !definition::matches_mr(def.trigger.as_ref(), &params.action) {
        return Ok(None);
    }

    let git_ref = format!("refs/heads/{}", params.source_branch);
    let pipeline_id = create_pipeline_with_steps(
        pool,
        params.project_id,
        &git_ref,
        params.commit_sha.as_deref(),
        params.user_id,
        "mr",
        &def,
    )
    .await?;

    tracing::info!(pipeline_id = %pipeline_id, "pipeline triggered by MR");
    Ok(Some(pipeline_id))
}

// ---------------------------------------------------------------------------
// API trigger (manual)
// ---------------------------------------------------------------------------

/// Manually trigger a pipeline for a given git ref.
#[tracing::instrument(skip(pool), fields(%project_id, %git_ref), err)]
pub async fn on_api(
    pool: &PgPool,
    repo_path: &Path,
    project_id: Uuid,
    git_ref: &str,
    user_id: Uuid,
) -> Result<Uuid, PipelineError> {
    // Resolve branch name from ref
    let branch = git_ref.strip_prefix("refs/heads/").unwrap_or(git_ref);

    let yaml = read_file_at_ref(repo_path, branch, ".platform.yaml")
        .await
        .ok_or_else(|| {
            PipelineError::InvalidDefinition("no .platform.yaml found at the given ref".into())
        })?;

    let def = definition::parse(&yaml)?;

    let commit_sha = get_ref_sha(repo_path, git_ref).await;

    create_pipeline_with_steps(
        pool,
        project_id,
        git_ref,
        commit_sha.as_deref(),
        user_id,
        "api",
        &def,
    )
    .await
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a pipeline row and its step rows in a single transaction.
async fn create_pipeline_with_steps(
    pool: &PgPool,
    project_id: Uuid,
    git_ref: &str,
    commit_sha: Option<&str>,
    triggered_by: Uuid,
    trigger_type: &str,
    def: &PipelineDefinition,
) -> Result<Uuid, PipelineError> {
    let mut tx = pool.begin().await?;

    let pipeline_id = sqlx::query_scalar!(
        r#"
        INSERT INTO pipelines (project_id, trigger, git_ref, commit_sha, status, triggered_by)
        VALUES ($1, $2, $3, $4, 'pending', $5)
        RETURNING id
        "#,
        project_id,
        trigger_type,
        git_ref,
        commit_sha,
        triggered_by,
    )
    .fetch_one(&mut *tx)
    .await?;

    for (i, step) in def.steps.iter().enumerate() {
        let commands: Vec<&str> = step.commands.iter().map(String::as_str).collect();
        let step_order = i32::try_from(i).unwrap_or(i32::MAX);

        sqlx::query!(
            r#"
            INSERT INTO pipeline_steps (pipeline_id, project_id, step_order, name, image, commands)
            VALUES ($1, $2, $3, $4, $5, $6)
            "#,
            pipeline_id,
            project_id,
            step_order,
            step.name,
            step.image,
            &commands as &[&str],
        )
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(pipeline_id)
}

/// Read a file's contents from a git repo at a given ref.
async fn read_file_at_ref(repo_path: &Path, git_ref: &str, file_path: &str) -> Option<String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("show")
        .arg(format!("{git_ref}:{file_path}"))
        .output()
        .await
        .ok()?;

    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        None
    }
}

/// Get the SHA of a ref (branch, tag, or full ref path).
async fn get_ref_sha(repo_path: &Path, git_ref: &str) -> Option<String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("rev-parse")
        .arg(git_ref)
        .output()
        .await
        .ok()?;

    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Executor notification
// ---------------------------------------------------------------------------

/// Notify the executor that a pipeline is ready to run.
///
/// Uses an in-process `tokio::sync::Notify` for immediate wake-up,
/// plus a Valkey pub/sub message for observability / future multi-instance support.
pub async fn notify_executor(state: &crate::store::AppState, pipeline_id: Uuid) {
    state.pipeline_notify.notify_one();
    let msg = pipeline_id.to_string();
    if let Err(e) = crate::store::valkey::publish(&state.valkey, "pipeline:run", &msg).await {
        tracing::warn!(error = %e, %pipeline_id, "failed to notify executor via valkey");
    }
}
