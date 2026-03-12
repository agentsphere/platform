use std::path::Path;

use sqlx::PgPool;
use uuid::Uuid;

use super::definition::{self, PipelineDefinition};
use super::error::PipelineError;

/// Name of the auto-generated dev image build step.
pub const DEV_IMAGE_STEP_NAME: &str = "build-dev-image";

/// Kaniko image with shell support (debug variant includes busybox).
const DEV_IMAGE_KANIKO: &str = "gcr.io/kaniko-project/executor:debug";

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
        tracing::debug!("no .platform.yaml at ref, skipping pipeline trigger");
        return Ok(None);
    };

    let def = definition::parse(&yaml)?;

    if !definition::matches_push(def.trigger.as_ref(), &params.branch) {
        tracing::debug!("push trigger does not match branch, skipping");
        return Ok(None);
    }

    // Determine dev image dockerfile: explicit YAML config takes priority, auto-detect as fallback
    let dev_dockerfile = if let Some(dev) = &def.dev_image {
        Some(dev.dockerfile.as_str())
    } else if has_dockerfile_dev(&params.repo_path, &params.branch).await {
        Some("Dockerfile.dev")
    } else {
        None
    };

    let git_ref = format!("refs/heads/{}", params.branch);
    let version = read_version_at_ref(&params.repo_path, &params.branch).await;
    let pipeline_id = create_pipeline_with_steps(
        pool,
        params.project_id,
        &git_ref,
        params.commit_sha.as_deref(),
        params.user_id,
        "push",
        &def,
        dev_dockerfile,
        version.as_deref(),
    )
    .await?;

    tracing::info!(pipeline_id = %pipeline_id, ?dev_dockerfile, "pipeline triggered by push");
    Ok(Some(pipeline_id))
}

// ---------------------------------------------------------------------------
// MR trigger
// ---------------------------------------------------------------------------

/// Handle a merge request event: read `.platform.yaml`, check trigger match, create pipeline + steps.
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
    let version = read_version_at_ref(&params.repo_path, &params.source_branch).await;
    let pipeline_id = create_pipeline_with_steps(
        pool,
        params.project_id,
        &git_ref,
        params.commit_sha.as_deref(),
        params.user_id,
        "mr",
        &def,
        None,
        version.as_deref(),
    )
    .await?;

    tracing::info!(pipeline_id = %pipeline_id, "pipeline triggered by MR");
    Ok(Some(pipeline_id))
}

// ---------------------------------------------------------------------------
// Tag trigger
// ---------------------------------------------------------------------------

pub struct TagTriggerParams {
    pub project_id: Uuid,
    pub user_id: Uuid,
    pub repo_path: std::path::PathBuf,
    pub tag_name: String,
    pub commit_sha: Option<String>,
}

/// Handle a tag push event: read `.platform.yaml`, check trigger match, create pipeline + steps.
#[tracing::instrument(skip(pool, params), fields(project_id = %params.project_id, tag_name = %params.tag_name), err)]
pub async fn on_tag(
    pool: &PgPool,
    params: &TagTriggerParams,
) -> Result<Option<Uuid>, PipelineError> {
    // Read .platform.yaml from the tagged commit
    let git_ref = params.commit_sha.as_deref().unwrap_or("HEAD");
    let Some(yaml) = read_file_at_ref(&params.repo_path, git_ref, ".platform.yaml").await else {
        return Ok(None);
    };

    let def = definition::parse(&yaml)?;

    if !definition::matches_tag(def.trigger.as_ref(), &params.tag_name) {
        return Ok(None);
    }

    let tag_ref = format!("refs/tags/{}", params.tag_name);
    let version = read_version_at_ref(&params.repo_path, git_ref).await;

    let pipeline_id = create_pipeline_with_steps(
        pool,
        params.project_id,
        &tag_ref,
        params.commit_sha.as_deref(),
        params.user_id,
        "tag",
        &def,
        None,
        version.as_deref(),
    )
    .await?;

    tracing::info!(pipeline_id = %pipeline_id, "pipeline triggered by tag");
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
    let version = read_version_at_ref(repo_path, branch).await;

    create_pipeline_with_steps(
        pool,
        project_id,
        git_ref,
        commit_sha.as_deref(),
        user_id,
        "api",
        &def,
        None,
        version.as_deref(),
    )
    .await
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a pipeline row and its step rows in a single transaction.
///
/// When `dev_image_dockerfile` is `Some(path)`, an extra kaniko step is appended
/// that builds the specified Dockerfile and pushes to `$REGISTRY/$PROJECT-dev:$COMMIT_SHA`.
#[allow(clippy::too_many_arguments)]
async fn create_pipeline_with_steps(
    pool: &PgPool,
    project_id: Uuid,
    git_ref: &str,
    commit_sha: Option<&str>,
    triggered_by: Uuid,
    trigger_type: &str,
    def: &PipelineDefinition,
    dev_image_dockerfile: Option<&str>,
    version: Option<&str>,
) -> Result<Uuid, PipelineError> {
    let mut tx = pool.begin().await?;

    let pipeline_id = sqlx::query_scalar!(
        r#"
        INSERT INTO pipelines (project_id, trigger, git_ref, commit_sha, status, triggered_by, version)
        VALUES ($1, $2, $3, $4, 'pending', $5, $6)
        RETURNING id
        "#,
        project_id,
        trigger_type,
        git_ref,
        commit_sha,
        triggered_by,
        version,
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

    if let Some(dockerfile) = dev_image_dockerfile {
        insert_dev_image_step(
            &mut tx,
            pipeline_id,
            project_id,
            def.steps.len(),
            dockerfile,
        )
        .await?;
    }

    tx.commit().await?;
    Ok(pipeline_id)
}

/// Insert the auto-generated kaniko step that builds a dev image.
///
/// The `dockerfile` parameter specifies which Dockerfile to use (e.g. `Dockerfile.dev`).
async fn insert_dev_image_step(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    pipeline_id: Uuid,
    project_id: Uuid,
    existing_step_count: usize,
    dockerfile: &str,
) -> Result<(), PipelineError> {
    let step_order = i32::try_from(existing_step_count).unwrap_or(i32::MAX);
    let cmd = format!(
        "/kaniko/executor \
        --dockerfile={dockerfile} \
        --context=/workspace \
        --destination=${{REGISTRY}}/${{PLATFORM_PROJECT_NAME}}/dev:${{COMMIT_SHA:-latest}} \
        --build-arg=PLATFORM_RUNNER_IMAGE=${{REGISTRY}}/platform-runner:latest \
        --insecure \
        --insecure-pull \
        --cache=true"
    );
    let commands: Vec<&str> = vec![&cmd];

    sqlx::query!(
        r#"
        INSERT INTO pipeline_steps (pipeline_id, project_id, step_order, name, image, commands)
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
        pipeline_id,
        project_id,
        step_order,
        DEV_IMAGE_STEP_NAME,
        DEV_IMAGE_KANIKO,
        &commands as &[&str],
    )
    .execute(&mut **tx)
    .await?;

    tracing::info!(%pipeline_id, "added auto dev-image build step");
    Ok(())
}

/// Check if `Dockerfile.dev` exists at the given git ref.
async fn has_dockerfile_dev(repo_path: &Path, git_ref: &str) -> bool {
    read_file_at_ref(repo_path, git_ref, "Dockerfile.dev")
        .await
        .is_some()
}

/// Read a file's contents from a git repo at a given ref.
pub async fn read_file_at_ref(repo_path: &Path, git_ref: &str, file_path: &str) -> Option<String> {
    let output = match tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("show")
        .arg(format!("{git_ref}:{file_path}"))
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(error = %e, ?repo_path, git_ref, file_path, "git show command failed");
            return None;
        }
    };

    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::debug!(git_ref, file_path, ?repo_path, %stderr, "file not found at ref");
        None
    }
}

/// Read the VERSION file from a git repo at a given ref.
/// Returns the trimmed contents, or None if the file doesn't exist.
pub async fn read_version_at_ref(repo_path: &Path, git_ref: &str) -> Option<String> {
    let content = read_file_at_ref(repo_path, git_ref, "VERSION").await?;
    let trimmed = content.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
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

/// Strip `refs/heads/` prefix from a git ref to get the branch name.
#[cfg(test)]
fn ref_to_branch(git_ref: &str) -> &str {
    git_ref.strip_prefix("refs/heads/").unwrap_or(git_ref)
}

/// Determine if a push to the given branch should trigger a pipeline,
/// given a parsed pipeline definition.
#[cfg(test)]
fn should_trigger_push(def: &PipelineDefinition, branch: &str) -> bool {
    definition::matches_push(def.trigger.as_ref(), branch)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    // -- ref_to_branch --

    #[test]
    fn ref_to_branch_strips_refs_heads() {
        assert_eq!(ref_to_branch("refs/heads/main"), "main");
    }

    #[test]
    fn ref_to_branch_strips_refs_heads_nested() {
        assert_eq!(ref_to_branch("refs/heads/feature/login"), "feature/login");
    }

    #[test]
    fn ref_to_branch_bare_ref_unchanged() {
        assert_eq!(ref_to_branch("main"), "main");
    }

    #[test]
    fn ref_to_branch_tag_ref_unchanged() {
        assert_eq!(ref_to_branch("refs/tags/v1.0"), "refs/tags/v1.0");
    }

    #[test]
    fn ref_to_branch_empty_string() {
        assert_eq!(ref_to_branch(""), "");
    }

    #[test]
    fn ref_to_branch_partial_prefix() {
        // "refs/heads" without trailing "/" should not be stripped
        assert_eq!(ref_to_branch("refs/heads"), "refs/heads");
    }

    // -- should_trigger_push --

    #[test]
    fn should_trigger_no_trigger_config_matches_all() {
        let def = definition::parse("pipeline:\n  steps:\n    - name: test\n      image: alpine\n")
            .unwrap();
        assert!(should_trigger_push(&def, "any-branch"));
    }

    #[test]
    fn should_trigger_matching_branch() {
        let def = definition::parse(
            "pipeline:\n  steps:\n    - name: test\n      image: alpine\n  on:\n    push:\n      branches: [main, develop]\n",
        )
        .unwrap();
        assert!(should_trigger_push(&def, "main"));
        assert!(should_trigger_push(&def, "develop"));
        assert!(!should_trigger_push(&def, "feature/foo"));
    }

    #[test]
    fn should_trigger_wildcard_branch() {
        let def = definition::parse(
            "pipeline:\n  steps:\n    - name: test\n      image: alpine\n  on:\n    push:\n      branches: [\"feature/*\"]\n",
        )
        .unwrap();
        assert!(should_trigger_push(&def, "feature/login"));
        assert!(!should_trigger_push(&def, "main"));
    }
}
