// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

use std::collections::BTreeMap;
use std::path::Path;

use sqlx::PgPool;
use uuid::Uuid;

use crate::definition::{self, PipelineDefinition};
use crate::error::PipelineError;

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

/// Parsed VERSION file contents.
#[derive(Debug, Clone)]
pub struct VersionInfo {
    /// Mapping of image name → semver version (e.g. "app" → "0.1.0").
    pub images: BTreeMap<String, String>,
    /// Raw file content for storage in the pipeline DB row.
    pub raw: String,
}

/// Parse a VERSION file into a map of image name → version.
///
/// Format: `key=major.minor.patch` lines. Lines starting with `#` are comments.
/// Blank lines are skipped. Returns error for invalid formats.
pub fn parse_version_file(content: &str) -> Result<BTreeMap<String, String>, PipelineError> {
    let mut map = BTreeMap::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            return Err(PipelineError::InvalidDefinition(format!(
                "Invalid VERSION file line: expected key=value, got: {trimmed}"
            )));
        };
        let key = key.trim();
        let value = value.trim();
        if !is_valid_semver(value) {
            return Err(PipelineError::InvalidDefinition(format!(
                "Invalid version format in VERSION file: expected major.minor.patch, got: {value}"
            )));
        }
        map.insert(key.to_string(), value.to_string());
    }
    Ok(map)
}

/// Check if a string is valid strict semver (major.minor.patch, all numeric).
fn is_valid_semver(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// Increment the patch component of a semver version string.
/// `"0.1.0"` → `"0.1.1"`.
pub fn increment_patch(version: &str) -> Result<String, PipelineError> {
    let parts: Vec<&str> = version.split('.').collect();
    if parts.len() != 3 {
        return Err(PipelineError::InvalidDefinition(format!(
            "Cannot increment non-semver version: {version}"
        )));
    }
    let patch: u64 = parts[2].parse().map_err(|_| {
        PipelineError::InvalidDefinition(format!("Invalid patch version: {}", parts[2]))
    })?;
    Ok(format!("{}.{}.{}", parts[0], parts[1], patch + 1))
}

// ---------------------------------------------------------------------------
// Push trigger
// ---------------------------------------------------------------------------

/// Handle a push event: read `.platform.yaml`, check trigger match, create pipeline + steps.
///
/// Returns the pipeline ID if a pipeline was created.
#[tracing::instrument(skip(pool, params, kaniko_image), fields(project_id = %params.project_id, branch = %params.branch), err)]
pub async fn on_push(
    pool: &PgPool,
    params: &PushTriggerParams,
    kaniko_image: &str,
) -> Result<Option<Uuid>, PipelineError> {
    // A18: Validate branch name to prevent ref injection
    platform_types::validation::check_branch_name(&params.branch)
        .map_err(|e| PipelineError::InvalidDefinition(e.to_string()))?;

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
        version.as_ref(),
        kaniko_image,
    )
    .await?;

    // Create annotated git tags for versioned pushes to main
    if (params.branch == "main" || params.branch == "master")
        && let Some(ref vi) = version
    {
        for ver in vi.images.values() {
            let tag_name = format!("v{ver}");
            create_annotated_tag(
                &params.repo_path,
                &tag_name,
                params.commit_sha.as_deref().unwrap_or("HEAD"),
                &format!("Release {ver}"),
            )
            .await;
        }
    }

    tracing::info!(pipeline_id = %pipeline_id, ?dev_dockerfile, "pipeline triggered by push");
    Ok(Some(pipeline_id))
}

// ---------------------------------------------------------------------------
// MR trigger
// ---------------------------------------------------------------------------

/// Handle a merge request event: read `.platform.yaml`, check trigger match, create pipeline + steps.
///
/// Safety-net: if the VERSION file on the source branch is identical to the target branch,
/// auto-increment the patch version and commit to the source branch so the developer
/// doesn't forget to bump.
#[tracing::instrument(skip(pool, params, kaniko_image), fields(project_id = %params.project_id, source_branch = %params.source_branch), err)]
pub async fn on_mr(
    pool: &PgPool,
    params: &MrTriggerParams,
    kaniko_image: &str,
) -> Result<Option<Uuid>, PipelineError> {
    // A18: Validate source branch name to prevent ref injection
    platform_types::validation::check_branch_name(&params.source_branch)
        .map_err(|e| PipelineError::InvalidDefinition(e.to_string()))?;

    let Some(yaml) =
        read_file_at_ref(&params.repo_path, &params.source_branch, ".platform.yaml").await
    else {
        return Ok(None);
    };

    let def = definition::parse(&yaml)?;

    if !definition::matches_mr(def.trigger.as_ref(), &params.action) {
        return Ok(None);
    }

    // Safety-net auto-bump: compare VERSION on source vs target (main)
    let mut version = read_version_at_ref(&params.repo_path, &params.source_branch).await;
    let mut commit_sha = params.commit_sha.clone();
    if let Some(ref source_vi) = version {
        let target_vi = read_version_at_ref(&params.repo_path, "main").await;
        if let Some(target_vi) = target_vi {
            // Check if all versions are identical — agent forgot to bump
            if source_vi.raw == target_vi.raw {
                tracing::info!(
                    source_branch = %params.source_branch,
                    "VERSION identical to main, auto-bumping patch"
                );
                match auto_bump_version(&params.repo_path, &params.source_branch, &source_vi.images)
                    .await
                {
                    Ok((bumped_vi, new_sha)) => {
                        // Post MR comment about the auto-bump (best-effort)
                        if let Some(first_ver) = bumped_vi.images.values().next() {
                            post_auto_bump_comment(
                                pool,
                                params.project_id,
                                &params.source_branch,
                                first_ver,
                            )
                            .await;
                        }
                        version = Some(bumped_vi);
                        commit_sha = Some(new_sha);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "auto-bump VERSION failed, continuing with original");
                    }
                }
            }
        }
    }

    let git_ref = format!("refs/heads/{}", params.source_branch);
    let pipeline_id = create_pipeline_with_steps(
        pool,
        params.project_id,
        &git_ref,
        commit_sha.as_deref(),
        params.user_id,
        "mr",
        &def,
        None,
        version.as_ref(),
        kaniko_image,
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
#[tracing::instrument(skip(pool, params, kaniko_image), fields(project_id = %params.project_id, tag_name = %params.tag_name), err)]
pub async fn on_tag(
    pool: &PgPool,
    params: &TagTriggerParams,
    kaniko_image: &str,
) -> Result<Option<Uuid>, PipelineError> {
    // A18: Validate tag name to prevent ref injection
    platform_types::validation::check_branch_name(&params.tag_name)
        .map_err(|e| PipelineError::InvalidDefinition(e.to_string()))?;

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
        version.as_ref(),
        kaniko_image,
    )
    .await?;

    tracing::info!(pipeline_id = %pipeline_id, "pipeline triggered by tag");
    Ok(Some(pipeline_id))
}

// ---------------------------------------------------------------------------
// API trigger (manual)
// ---------------------------------------------------------------------------

/// Manually trigger a pipeline for a given git ref.
#[tracing::instrument(skip(pool, kaniko_image), fields(%project_id, %git_ref), err)]
pub async fn on_api(
    pool: &PgPool,
    repo_path: &Path,
    project_id: Uuid,
    git_ref: &str,
    user_id: Uuid,
    kaniko_image: &str,
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
        version.as_ref(),
        kaniko_image,
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
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn create_pipeline_with_steps(
    pool: &PgPool,
    project_id: Uuid,
    git_ref: &str,
    commit_sha: Option<&str>,
    triggered_by: Uuid,
    trigger_type: &str,
    def: &PipelineDefinition,
    dev_image_dockerfile: Option<&str>,
    version: Option<&VersionInfo>,
    kaniko_image: &str,
) -> Result<Uuid, PipelineError> {
    let mut tx = pool.begin().await?;

    let pipeline_id: Uuid = sqlx::query_scalar(
        r"
        INSERT INTO pipelines (project_id, trigger, git_ref, commit_sha, status, triggered_by, version)
        VALUES ($1, $2, $3, $4, 'pending', $5, $6)
        RETURNING id
        ",
    )
    .bind(project_id)
    .bind(trigger_type)
    .bind(git_ref)
    .bind(commit_sha)
    .bind(triggered_by)
    .bind(version.map(|v| v.raw.as_str()))
    .fetch_one(&mut *tx)
    .await?;

    for (i, step) in def.steps.iter().enumerate() {
        let step_order = i32::try_from(i).unwrap_or(i32::MAX);

        let condition_events: Vec<&str> = step
            .only
            .as_ref()
            .map(|c| c.events.iter().map(String::as_str).collect())
            .unwrap_or_default();
        let condition_branches: Vec<&str> = step
            .only
            .as_ref()
            .map(|c| c.branches.iter().map(String::as_str).collect())
            .unwrap_or_default();
        let depends_on: Vec<&str> = step.depends_on.iter().map(String::as_str).collect();
        let environment_json = if step.environment.is_empty() {
            None
        } else {
            Some(serde_json::to_value(&step.environment).unwrap_or_default())
        };

        // Resolve step type and generate image/commands for platform-managed steps.
        let kind = step.kind();
        let (step_type_str, image, commands, deploy_test_json, step_config) = match kind {
            crate::definition::StepKind::ImageBuild => {
                let image_name = step.image_name.as_deref().unwrap_or("app");
                let dockerfile = step.dockerfile.as_deref().unwrap_or("Dockerfile");

                // Version tag: if VERSION maps this image, add a second --destination
                let version_dest = version
                    .and_then(|vi| vi.images.get(image_name))
                    .map(|ver| {
                        let short_sha = commit_sha.map_or("unknown", |s| &s[..s.len().min(8)]);
                        if trigger_type == "push" {
                            format!(
                                " --destination=${{REGISTRY}}/${{PLATFORM_PROJECT_NAME}}/{image_name}:{ver}"
                            )
                        } else {
                            format!(
                                " --destination=${{REGISTRY}}/${{PLATFORM_PROJECT_NAME}}/{image_name}:{ver}-rc-{short_sha}"
                            )
                        }
                    })
                    .unwrap_or_default();

                let kaniko_cmd = format!(
                    "/kaniko/executor \
                     --context=dir:///workspace \
                     --dockerfile={dockerfile} \
                     --destination=${{REGISTRY}}/${{PLATFORM_PROJECT_NAME}}/{image_name}:${{COMMIT_SHA}}{version_dest} \
                     --build-arg=PLATFORM_RUNNER_IMAGE=${{REGISTRY}}/platform-runner:v1 \
                     --insecure --insecure-registry=${{REGISTRY}} \
                     --insecure-pull \
                     --cache=true --cache-repo=${{REGISTRY}}/${{PLATFORM_PROJECT_NAME}}/cache"
                );
                let mut config = serde_json::json!({
                    "image_name": image_name,
                    "dockerfile": dockerfile,
                    "secrets": step.secrets,
                });
                if !step.artifacts.is_empty() {
                    config["artifacts"] = serde_json::to_value(&step.artifacts).unwrap_or_default();
                }
                (
                    "imagebuild",
                    kaniko_image.to_string(),
                    vec![kaniko_cmd],
                    None,
                    Some(config),
                )
            }
            crate::definition::StepKind::DeployTest => {
                let dt_json = step
                    .deploy_test
                    .as_ref()
                    .map(|dt| serde_json::to_value(dt).unwrap_or_default());
                (
                    "deploy_test",
                    step.image.clone(),
                    step.commands.clone(),
                    dt_json,
                    None,
                )
            }
            crate::definition::StepKind::GitopsSync => {
                let config = step
                    .gitops
                    .as_ref()
                    .map(|g| serde_json::to_value(g).unwrap_or_default());
                ("gitops_sync", String::new(), vec![], None, config)
            }
            crate::definition::StepKind::DeployWatch => {
                let config = step
                    .deploy_watch
                    .as_ref()
                    .map(|dw| serde_json::to_value(dw).unwrap_or_default());
                ("deploy_watch", String::new(), vec![], None, config)
            }
            crate::definition::StepKind::Command => {
                let config = if step.artifacts.is_empty() {
                    None
                } else {
                    let mut c = serde_json::Map::new();
                    c.insert(
                        "artifacts".into(),
                        serde_json::to_value(&step.artifacts).unwrap_or_default(),
                    );
                    Some(serde_json::Value::Object(c))
                };
                (
                    "command",
                    step.image.clone(),
                    step.commands.clone(),
                    None,
                    config,
                )
            }
        };
        let commands_refs: Vec<&str> = commands.iter().map(String::as_str).collect();

        sqlx::query(
            "INSERT INTO pipeline_steps (pipeline_id, project_id, step_order, name, image, commands,
                                        condition_events, condition_branches, deploy_test,
                                        depends_on, environment, gate, step_type, step_config)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
        )
        .bind(pipeline_id)
        .bind(project_id)
        .bind(step_order)
        .bind(&step.name)
        .bind(&image)
        .bind(&commands_refs as &[&str])
        .bind(&condition_events as &[&str])
        .bind(&condition_branches as &[&str])
        .bind(&deploy_test_json)
        .bind(&depends_on as &[&str])
        .bind(&environment_json)
        .bind(step.gate)
        .bind(step_type_str)
        .bind(&step_config)
        .execute(&mut *tx)
        .await?;
    }

    // NOTE: dev_image_dockerfile is no longer auto-injected as a magic step.
    // Projects should declare `type: imagebuild` steps for dev images in .platform.yaml.
    // The parameter is kept for backwards compat (callers still pass it) but ignored.
    let _ = dev_image_dockerfile;

    tx.commit().await?;
    Ok(pipeline_id)
}

// insert_dev_image_step removed — dev images are now explicit `type: imagebuild` steps.

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

/// Read all `.yaml`/`.yml` files from a directory in a git repo at a given ref.
///
/// Uses `git ls-tree` to list entries, then `git show` to read each file.
/// Returns concatenated content separated by `---\n`.
pub async fn read_dir_at_ref(repo_path: &Path, git_ref: &str, dir_path: &str) -> Option<String> {
    let dir_path = dir_path.trim_end_matches('/');
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("ls-tree")
        .arg("--name-only")
        .arg(format!("{git_ref}:{dir_path}"))
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let listing = String::from_utf8_lossy(&output.stdout);
    let yaml_files: Vec<&str> = listing
        .lines()
        .filter(|f| {
            std::path::Path::new(f).extension().is_some_and(|ext| {
                ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml")
            })
        })
        .collect();

    if yaml_files.is_empty() {
        return None;
    }

    let mut parts = Vec::new();
    for file in yaml_files {
        let file_path = format!("{dir_path}/{file}");
        if let Some(content) = read_file_at_ref(repo_path, git_ref, &file_path).await {
            parts.push(content);
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("---\n"))
    }
}

/// Read the VERSION file from a git repo at a given ref.
/// Returns parsed `VersionInfo`, or None if the file doesn't exist or is invalid.
pub async fn read_version_at_ref(repo_path: &Path, git_ref: &str) -> Option<VersionInfo> {
    let content = read_file_at_ref(repo_path, git_ref, "VERSION").await?;
    let trimmed = content.trim().to_string();
    if trimmed.is_empty() {
        return None;
    }
    match parse_version_file(&trimmed) {
        Ok(images) => Some(VersionInfo {
            images,
            raw: trimmed,
        }),
        Err(e) => {
            tracing::warn!(error = %e, "failed to parse VERSION file");
            None
        }
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
// Git tag + auto-bump helpers
// ---------------------------------------------------------------------------

/// Create an annotated git tag on a commit. Best-effort — logs warnings on failure.
async fn create_annotated_tag(repo_path: &Path, tag_name: &str, commit_sha: &str, message: &str) {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .env("GIT_COMMITTER_NAME", "Platform")
        .env("GIT_COMMITTER_EMAIL", "platform@localhost")
        .args(["tag", "-a", tag_name, "-m", message, commit_sha])
        .output()
        .await;

    match output {
        Ok(o) if o.status.success() => {
            tracing::info!(%tag_name, %commit_sha, "annotated git tag created");
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            // Tag already exists is not an error worth warning about
            if stderr.contains("already exists") {
                tracing::debug!(%tag_name, "git tag already exists, skipping");
            } else {
                tracing::warn!(%tag_name, %stderr, "failed to create annotated git tag");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, %tag_name, "git tag command failed");
        }
    }
}

/// Auto-bump VERSION file on a feature branch when it matches main.
///
/// Creates a direct commit on the branch (bypassing hooks) with the bumped version.
/// Returns the new `VersionInfo` and the new commit SHA.
async fn auto_bump_version(
    repo_path: &Path,
    branch: &str,
    current_images: &BTreeMap<String, String>,
) -> Result<(VersionInfo, String), PipelineError> {
    // Bump all image versions
    let mut bumped: BTreeMap<String, String> = BTreeMap::new();
    let mut raw_lines = Vec::new();
    for (name, ver) in current_images {
        let new_ver = increment_patch(ver)?;
        raw_lines.push(format!("{name}={new_ver}"));
        bumped.insert(name.clone(), new_ver);
    }
    let new_raw = raw_lines.join("\n");

    // Write the bumped VERSION via a temporary worktree
    let worktree_dir = repo_path
        .parent()
        .unwrap_or(repo_path)
        .join(format!("_autobump_{}", Uuid::new_v4()));

    // Prune stale worktrees
    let _ = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["worktree", "prune"])
        .output()
        .await;

    let wt = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["worktree", "add"])
        .arg(&worktree_dir)
        .arg(branch)
        .output()
        .await
        .map_err(|e| PipelineError::Other(e.into()))?;

    if !wt.status.success() {
        let stderr = String::from_utf8_lossy(&wt.stderr);
        return Err(PipelineError::Other(anyhow::anyhow!(
            "failed to create worktree for auto-bump: {stderr}"
        )));
    }

    // Write VERSION file
    tokio::fs::write(worktree_dir.join("VERSION"), format!("{new_raw}\n"))
        .await
        .map_err(|e| PipelineError::Other(e.into()))?;

    // Stage and commit
    let _ = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&worktree_dir)
        .args(["add", "VERSION"])
        .output()
        .await;

    let commit = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&worktree_dir)
        .env("GIT_AUTHOR_NAME", "Platform")
        .env("GIT_AUTHOR_EMAIL", "platform@localhost")
        .env("GIT_COMMITTER_NAME", "Platform")
        .env("GIT_COMMITTER_EMAIL", "platform@localhost")
        .args([
            "commit",
            "-m",
            &format!(
                "chore: auto-bump VERSION to {}",
                bumped.values().next().unwrap_or(&"unknown".to_string())
            ),
        ])
        .output()
        .await
        .map_err(|e| PipelineError::Other(e.into()))?;

    if !commit.status.success() {
        let stderr = String::from_utf8_lossy(&commit.stderr);
        // Clean up worktree before returning error
        let _ = tokio::process::Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(["worktree", "remove", "--force"])
            .arg(&worktree_dir)
            .output()
            .await;
        let _ = tokio::fs::remove_dir_all(&worktree_dir).await;
        return Err(PipelineError::Other(anyhow::anyhow!(
            "auto-bump commit failed: {stderr}"
        )));
    }

    // Get the new commit SHA
    let sha_output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&worktree_dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .await
        .map_err(|e| PipelineError::Other(e.into()))?;
    let new_sha = String::from_utf8_lossy(&sha_output.stdout)
        .trim()
        .to_string();

    // Clean up worktree
    let _ = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["worktree", "remove", "--force"])
        .arg(&worktree_dir)
        .output()
        .await;
    let _ = tokio::fs::remove_dir_all(&worktree_dir).await;

    let vi = VersionInfo {
        images: bumped,
        raw: new_raw,
    };

    tracing::info!(branch, new_sha = %new_sha, "auto-bumped VERSION on feature branch");
    Ok((vi, new_sha))
}

/// Post a comment on the MR explaining the auto-bump (best-effort).
async fn post_auto_bump_comment(
    pool: &PgPool,
    project_id: Uuid,
    source_branch: &str,
    new_version: &str,
) {
    let comment = format!(
        "Automatically bumped VERSION to {new_version} — your local branch is now behind \
         remote. Run `git pull` before pushing again."
    );

    // Find the open MR for this source branch
    let mr = sqlx::query(
        "SELECT id FROM merge_requests WHERE project_id = $1 AND source_branch = $2 AND status = 'open' LIMIT 1",
    )
    .bind(project_id)
    .bind(source_branch)
    .fetch_optional(pool)
    .await;

    if let Ok(Some(mr_row)) = mr {
        use sqlx::Row as _;
        let mr_id: Uuid = mr_row.get("id");
        let _ = sqlx::query(
            "INSERT INTO comments (project_id, mr_id, author_id, body) \
             VALUES ($1, $2, (SELECT owner_id FROM projects WHERE id = $1), $3)",
        )
        .bind(project_id)
        .bind(mr_id)
        .bind(&comment)
        .execute(pool)
        .await;
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
pub async fn notify_executor(
    pipeline_notify: &tokio::sync::Notify,
    valkey: &fred::clients::Pool,
    pipeline_id: Uuid,
) {
    pipeline_notify.notify_one();
    let msg = pipeline_id.to_string();
    if let Err(e) = platform_types::valkey::publish(valkey, "pipeline:run", &msg).await {
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

    // -- parse_version_file --

    #[test]
    fn parse_version_file_simple() {
        let result = parse_version_file("app=0.1.0").unwrap();
        assert_eq!(result.get("app").unwrap(), "0.1.0");
    }

    #[test]
    fn parse_version_file_multiple() {
        let result = parse_version_file("app=0.1.0\nworker=1.2.3").unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result["app"], "0.1.0");
        assert_eq!(result["worker"], "1.2.3");
    }

    #[test]
    fn parse_version_file_comments_and_blanks() {
        let content = "# This is a version file\n\napp=0.1.0\n# another comment\n";
        let result = parse_version_file(content).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result["app"], "0.1.0");
    }

    #[test]
    fn parse_version_file_rejects_non_semver() {
        let result = parse_version_file("app=latest");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("major.minor.patch"));
    }

    #[test]
    fn parse_version_file_rejects_partial_semver() {
        assert!(parse_version_file("app=1.0").is_err());
    }

    #[test]
    fn parse_version_file_rejects_invalid_line() {
        assert!(parse_version_file("no-equals-sign").is_err());
    }

    #[test]
    fn parse_version_file_handles_whitespace() {
        let result = parse_version_file("  app = 0.1.0  ").unwrap();
        assert_eq!(result["app"], "0.1.0");
    }

    // -- increment_patch --

    #[test]
    fn increment_patch_basic() {
        assert_eq!(increment_patch("0.1.0").unwrap(), "0.1.1");
    }

    #[test]
    fn increment_patch_high_numbers() {
        assert_eq!(increment_patch("1.2.99").unwrap(), "1.2.100");
    }

    #[test]
    fn increment_patch_rejects_non_semver() {
        assert!(increment_patch("1.0").is_err());
        assert!(increment_patch("latest").is_err());
    }

    #[test]
    fn increment_patch_rejects_non_numeric() {
        assert!(increment_patch("1.2.abc").is_err());
    }

    // -- is_valid_semver --

    #[test]
    fn is_valid_semver_valid() {
        assert!(is_valid_semver("1.2.3"));
        assert!(is_valid_semver("0.0.0"));
        assert!(is_valid_semver("10.20.30"));
    }

    #[test]
    fn is_valid_semver_missing_part() {
        assert!(!is_valid_semver("1.2"));
        assert!(!is_valid_semver("1"));
    }

    #[test]
    fn is_valid_semver_non_numeric() {
        assert!(!is_valid_semver("1.2.a"));
        assert!(!is_valid_semver("a.b.c"));
        assert!(!is_valid_semver("1.2.3-beta"));
    }

    #[test]
    fn is_valid_semver_empty_parts() {
        assert!(!is_valid_semver(".."));
        assert!(!is_valid_semver("1..3"));
        assert!(!is_valid_semver(""));
    }

    #[test]
    fn is_valid_semver_too_many_parts() {
        assert!(!is_valid_semver("1.2.3.4"));
    }

    // -- increment_patch additional --

    #[test]
    fn increment_patch_from_zero() {
        assert_eq!(increment_patch("0.0.0").unwrap(), "0.0.1");
    }

    // -- parse_version_file additional edge cases --

    #[test]
    fn parse_version_file_empty_input() {
        let result = parse_version_file("");
        // Empty input → empty map (no lines to parse)
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn parse_version_file_only_comments() {
        let result = parse_version_file("# comment\n# another").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn parse_version_file_preserves_order() {
        let result = parse_version_file("worker=1.0.0\napp=2.0.0").unwrap();
        let keys: Vec<&String> = result.keys().collect();
        // BTreeMap sorts alphabetically
        assert_eq!(keys, vec!["app", "worker"]);
    }

    // -- increment_patch edge cases --

    #[test]
    fn increment_patch_preserves_major_minor() {
        assert_eq!(increment_patch("5.10.0").unwrap(), "5.10.1");
        assert_eq!(increment_patch("0.0.0").unwrap(), "0.0.1");
    }

    #[test]
    fn increment_patch_large_patch_number() {
        assert_eq!(increment_patch("1.0.999").unwrap(), "1.0.1000");
    }

    #[test]
    fn increment_patch_empty_string() {
        assert!(increment_patch("").is_err());
    }

    #[test]
    fn increment_patch_single_number() {
        assert!(increment_patch("42").is_err());
    }

    #[test]
    fn increment_patch_four_parts() {
        assert!(increment_patch("1.2.3.4").is_err());
    }

    // -- is_valid_semver edge cases --

    #[test]
    fn is_valid_semver_leading_zeros() {
        // Leading zeros are technically valid digits
        assert!(is_valid_semver("01.02.03"));
    }

    #[test]
    fn is_valid_semver_extra_dots() {
        assert!(!is_valid_semver("1.2.3."));
    }

    // -- parse_version_file edge cases --

    #[test]
    fn parse_version_file_trailing_newline() {
        let result = parse_version_file("app=1.0.0\n").unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result["app"], "1.0.0");
    }

    #[test]
    fn parse_version_file_mixed_comments_and_blanks() {
        let content = "\n# header\n\napp=1.0.0\n\n# footer\nworker=2.0.0\n\n";
        let result = parse_version_file(content).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result["app"], "1.0.0");
        assert_eq!(result["worker"], "2.0.0");
    }

    #[test]
    fn parse_version_file_duplicate_keys() {
        // BTreeMap replaces the earlier entry
        let result = parse_version_file("app=1.0.0\napp=2.0.0").unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result["app"], "2.0.0");
    }

    #[test]
    fn parse_version_file_key_with_dashes() {
        let result = parse_version_file("my-service=1.0.0").unwrap();
        assert_eq!(result["my-service"], "1.0.0");
    }

    #[test]
    fn parse_version_file_rejects_empty_value() {
        // Empty value after = is not valid semver
        assert!(parse_version_file("app=").is_err());
    }

    #[test]
    fn parse_version_file_rejects_value_with_prefix() {
        assert!(parse_version_file("app=v1.0.0").is_err());
    }

    // -- VersionInfo struct --

    #[test]
    fn version_info_fields() {
        let mut images = BTreeMap::new();
        images.insert("app".to_string(), "1.0.0".to_string());
        let vi = VersionInfo {
            images: images.clone(),
            raw: "app=1.0.0".to_string(),
        };
        assert_eq!(vi.images, images);
        assert_eq!(vi.raw, "app=1.0.0");
    }

    #[test]
    fn version_info_clone() {
        let mut images = BTreeMap::new();
        images.insert("app".to_string(), "1.0.0".to_string());
        let vi = VersionInfo {
            images,
            raw: "app=1.0.0".to_string(),
        };
        let cloned = vi.clone();
        assert_eq!(cloned.raw, vi.raw);
        assert_eq!(cloned.images, vi.images);
    }

    // -- PushTriggerParams / MrTriggerParams / TagTriggerParams struct usage --

    #[test]
    fn push_trigger_params_fields() {
        let params = PushTriggerParams {
            project_id: Uuid::nil(),
            user_id: Uuid::nil(),
            repo_path: std::path::PathBuf::from("/tmp/test"),
            branch: "main".to_string(),
            commit_sha: Some("abc123".to_string()),
        };
        assert_eq!(params.branch, "main");
        assert_eq!(params.commit_sha, Some("abc123".to_string()));
    }

    #[test]
    fn mr_trigger_params_fields() {
        let params = MrTriggerParams {
            project_id: Uuid::nil(),
            user_id: Uuid::nil(),
            repo_path: std::path::PathBuf::from("/tmp/test"),
            source_branch: "feature/login".to_string(),
            commit_sha: None,
            action: "opened".to_string(),
        };
        assert_eq!(params.source_branch, "feature/login");
        assert_eq!(params.action, "opened");
        assert!(params.commit_sha.is_none());
    }

    #[test]
    fn tag_trigger_params_fields() {
        let params = TagTriggerParams {
            project_id: Uuid::nil(),
            user_id: Uuid::nil(),
            repo_path: std::path::PathBuf::from("/tmp/test"),
            tag_name: "v1.0.0".to_string(),
            commit_sha: Some("deadbeef".to_string()),
        };
        assert_eq!(params.tag_name, "v1.0.0");
        assert_eq!(params.commit_sha.as_deref(), Some("deadbeef"));
    }

    // -- ref_to_branch edge cases --

    #[test]
    fn ref_to_branch_double_prefix() {
        // "refs/heads/refs/heads/main" strips only the first prefix
        assert_eq!(
            ref_to_branch("refs/heads/refs/heads/main"),
            "refs/heads/main"
        );
    }

    #[test]
    fn ref_to_branch_only_slash() {
        assert_eq!(ref_to_branch("/"), "/");
    }

    // -- should_trigger_push edge cases --

    #[test]
    fn should_trigger_empty_branches_matches_all() {
        let def = definition::parse(
            "pipeline:\n  steps:\n    - name: test\n      image: alpine\n  on:\n    push:\n      branches: []\n",
        )
        .unwrap();
        assert!(should_trigger_push(&def, "anything"));
    }
}
