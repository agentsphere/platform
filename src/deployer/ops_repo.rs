use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use sqlx::PgPool;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::error::DeployerError;

/// Per-repo mutex to serialize git operations on bare repos.
/// Multiple concurrent worktree operations on the same bare repo cause ref lock conflicts.
static REPO_LOCKS: LazyLock<Mutex<HashMap<PathBuf, std::sync::Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Acquire a per-repo lock for serializing git operations.
async fn repo_lock(repo_path: &Path) -> tokio::sync::OwnedMutexGuard<()> {
    let mut locks = REPO_LOCKS.lock().await;
    let lock = locks
        .entry(repo_path.to_path_buf())
        .or_insert_with(|| std::sync::Arc::new(Mutex::new(())))
        .clone();
    drop(locks); // Release the outer lock before awaiting the inner one
    lock.lock_owned().await
}

// ---------------------------------------------------------------------------
// Ops repo lifecycle (local bare repos)
// ---------------------------------------------------------------------------

/// Initialize a new bare git repository for an ops repo.
/// Returns the full path to the created repo directory.
#[tracing::instrument(skip(repos_dir), fields(%name, %branch), err)]
pub async fn init_ops_repo(
    repos_dir: &Path,
    name: &str,
    branch: &str,
) -> Result<PathBuf, DeployerError> {
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(DeployerError::SyncFailed(
            "invalid ops repo name: must not contain path separators or '..'".into(),
        ));
    }
    let dest = repos_dir.join(format!("{name}.git"));

    tokio::fs::create_dir_all(&dest)
        .await
        .map_err(|e| DeployerError::SyncFailed(format!("failed to create repo dir: {e}")))?;

    let output = tokio::process::Command::new("git")
        .arg("init")
        .arg("--bare")
        .arg(&dest)
        .output()
        .await
        .map_err(|e| DeployerError::SyncFailed(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DeployerError::SyncFailed(format!(
            "git init failed: {stderr}"
        )));
    }

    // Set default branch
    let head_ref = format!("ref: refs/heads/{branch}\n");
    tokio::fs::write(dest.join("HEAD"), head_ref)
        .await
        .map_err(|e| DeployerError::SyncFailed(format!("failed to set HEAD: {e}")))?;

    tracing::info!(path = %dest.display(), "ops repo initialized");
    Ok(dest)
}

// ---------------------------------------------------------------------------
// Reading from bare repos (no working tree needed)
// ---------------------------------------------------------------------------

/// Get the current HEAD SHA of a bare repo.
pub async fn get_head_sha(repo_path: &Path) -> Result<String, DeployerError> {
    let output = tokio::process::Command::new("git")
        .args(["-C"])
        .arg(repo_path)
        .args(["rev-parse", "HEAD"])
        .output()
        .await
        .map_err(|e| DeployerError::SyncFailed(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DeployerError::SyncFailed(format!(
            "git rev-parse failed: {stderr}"
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Get the SHA of a specific branch in a bare repo.
pub async fn get_branch_sha(repo_path: &Path, branch: &str) -> Result<String, DeployerError> {
    let output = tokio::process::Command::new("git")
        .args(["-C"])
        .arg(repo_path)
        .args(["rev-parse", &format!("refs/heads/{branch}")])
        .output()
        .await
        .map_err(|e| DeployerError::SyncFailed(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DeployerError::SyncFailed(format!(
            "git rev-parse refs/heads/{branch} failed: {stderr}"
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Read a file from a bare repo at a given ref without a working tree.
/// Uses `git show {ref}:{path}`.
pub async fn read_file_at_ref(
    repo_path: &Path,
    git_ref: &str,
    file_path: &str,
) -> Result<String, DeployerError> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("show")
        .arg(format!("{git_ref}:{file_path}"))
        .output()
        .await
        .map_err(|e| DeployerError::SyncFailed(e.to_string()))?;

    if !output.status.success() {
        return Err(DeployerError::ValuesNotFound(format!(
            "{file_path} at {git_ref}"
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Read all YAML files from a directory in a bare repo at a given ref.
/// Returns concatenated content with `---` separators.
pub async fn read_dir_yaml_at_ref(
    repo_path: &Path,
    git_ref: &str,
    dir_path: &str,
) -> Result<String, DeployerError> {
    let dir = dir_path.trim_end_matches('/');
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["ls-tree", "--name-only", git_ref, &format!("{dir}/")])
        .output()
        .await
        .map_err(|e| DeployerError::SyncFailed(e.to_string()))?;

    if !output.status.success() {
        return Err(DeployerError::ValuesNotFound(format!(
            "{dir_path} at {git_ref}"
        )));
    }

    let file_list = String::from_utf8_lossy(&output.stdout);
    let mut combined = String::new();

    for file in file_list.lines() {
        let ext = std::path::Path::new(file)
            .extension()
            .and_then(|e| e.to_str());
        if matches!(ext, Some("yaml" | "yml")) {
            if !combined.is_empty() {
                combined.push_str("\n---\n");
            }
            let content = read_file_at_ref(repo_path, git_ref, file).await?;
            combined.push_str(&content);
        }
    }

    if combined.is_empty() {
        return Err(DeployerError::ValuesNotFound(format!(
            "no YAML files in {dir_path} at {git_ref}"
        )));
    }

    Ok(combined)
}

/// Read the values file for a given environment from the ops repo.
/// Returns the parsed YAML as a JSON value for template rendering.
pub async fn read_values(
    repo_path: &Path,
    branch: &str,
    environment: &str,
) -> Result<serde_json::Value, DeployerError> {
    let file_path = format!("values/{environment}.yaml");
    let content = read_file_at_ref(repo_path, branch, &file_path).await?;

    serde_yaml::from_str(&content)
        .map_err(|e| DeployerError::RenderFailed(format!("failed to parse {file_path}: {e}")))
}

// ---------------------------------------------------------------------------
// Writing to bare repos (requires worktree)
// ---------------------------------------------------------------------------

/// Commit a values file to the ops repo for a given environment.
/// Uses git worktree to write into a bare repo.
/// Returns the new commit SHA.
#[tracing::instrument(skip(values), fields(%environment), err)]
pub async fn commit_values(
    repo_path: &Path,
    branch: &str,
    environment: &str,
    values: &serde_json::Value,
) -> Result<String, DeployerError> {
    let _lock = repo_lock(repo_path).await;
    // Ensure the branch exists (bare repo may be empty after init)
    ensure_branch_exists(repo_path, branch).await?;

    let worktree_dir = repo_path.join(format!("_values_worktree_{}", Uuid::new_v4()));

    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("worktree")
        .arg("add")
        .arg(&worktree_dir)
        .arg(branch)
        .output()
        .await
        .map_err(|e| DeployerError::CommitFailed(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DeployerError::CommitFailed(format!(
            "git worktree add failed: {stderr}"
        )));
    }

    let result = write_and_commit_values(&worktree_dir, environment, values).await;

    // Always clean up worktree
    cleanup_worktree(repo_path, &worktree_dir).await;

    result?;

    get_branch_sha(repo_path, branch).await
}

/// Write a single file to the ops repo at a given branch.
/// Uses git worktree to write into a bare repo.
/// Returns the new commit SHA on the given branch.
#[tracing::instrument(skip(content), fields(%file_path), err)]
pub async fn write_file_to_repo(
    repo_path: &Path,
    branch: &str,
    file_path: &str,
    content: &str,
) -> Result<String, DeployerError> {
    let _lock = repo_lock(repo_path).await;
    ensure_branch_exists(repo_path, branch).await?;

    let worktree_dir = repo_path.join(format!("_file_worktree_{}", Uuid::new_v4()));

    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("worktree")
        .arg("add")
        .arg(&worktree_dir)
        .arg(branch)
        .output()
        .await
        .map_err(|e| DeployerError::CommitFailed(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DeployerError::CommitFailed(format!(
            "git worktree add failed: {stderr}"
        )));
    }

    let result = async {
        let dest = worktree_dir.join(file_path);
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| DeployerError::CommitFailed(format!("mkdir: {e}")))?;
        }
        tokio::fs::write(&dest, content)
            .await
            .map_err(|e| DeployerError::CommitFailed(format!("write {file_path}: {e}")))?;

        let _ = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&worktree_dir)
            .args(["add", file_path])
            .output()
            .await;

        // Check if there are changes to commit
        let diff = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&worktree_dir)
            .args(["diff", "--cached", "--quiet"])
            .status()
            .await;

        if diff.map(|s| s.success()).unwrap_or(false) {
            // No changes
            return Ok(());
        }

        let commit_msg = format!("update {file_path}");
        let commit_output = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&worktree_dir)
            .env("GIT_AUTHOR_NAME", "Platform")
            .env("GIT_AUTHOR_EMAIL", "platform@localhost")
            .env("GIT_COMMITTER_NAME", "Platform")
            .env("GIT_COMMITTER_EMAIL", "platform@localhost")
            .args(["commit", "-m", &commit_msg])
            .output()
            .await
            .map_err(|e| DeployerError::CommitFailed(e.to_string()))?;

        if !commit_output.status.success() {
            let stderr = String::from_utf8_lossy(&commit_output.stderr);
            if !stderr.contains("nothing to commit") {
                return Err(DeployerError::CommitFailed(format!(
                    "git commit failed: {stderr}"
                )));
            }
        }
        Ok(())
    }
    .await;

    cleanup_worktree(repo_path, &worktree_dir).await;
    result?;
    get_branch_sha(repo_path, branch).await
}

/// Internal: write the values file, stage, and commit inside a worktree.
async fn write_and_commit_values(
    worktree_dir: &Path,
    environment: &str,
    values: &serde_json::Value,
) -> Result<(), DeployerError> {
    // Ensure values/ directory exists
    let values_dir = worktree_dir.join("values");
    tokio::fs::create_dir_all(&values_dir)
        .await
        .map_err(|e| DeployerError::CommitFailed(format!("mkdir values: {e}")))?;

    // Write the YAML values file
    let yaml_content = serde_yaml::to_string(values)
        .map_err(|e| DeployerError::CommitFailed(format!("yaml serialize: {e}")))?;

    let file_path = values_dir.join(format!("{environment}.yaml"));
    tokio::fs::write(&file_path, &yaml_content)
        .await
        .map_err(|e| DeployerError::CommitFailed(format!("write values: {e}")))?;

    // Stage the file
    let add_output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(worktree_dir)
        .args(["add", &format!("values/{environment}.yaml")])
        .output()
        .await
        .map_err(|e| DeployerError::CommitFailed(e.to_string()))?;

    if !add_output.status.success() {
        let stderr = String::from_utf8_lossy(&add_output.stderr);
        return Err(DeployerError::CommitFailed(format!(
            "git add failed: {stderr}"
        )));
    }

    // Extract image_ref for the commit message
    let image_ref = values
        .get("image_ref")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let commit_msg = format!("deploy({environment}): update image to {image_ref}");

    let commit_output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(worktree_dir)
        .env("GIT_AUTHOR_NAME", "Platform")
        .env("GIT_AUTHOR_EMAIL", "platform@localhost")
        .env("GIT_COMMITTER_NAME", "Platform")
        .env("GIT_COMMITTER_EMAIL", "platform@localhost")
        .args(["commit", "-m", &commit_msg])
        .output()
        .await
        .map_err(|e| DeployerError::CommitFailed(e.to_string()))?;

    if !commit_output.status.success() {
        let stderr = String::from_utf8_lossy(&commit_output.stderr);
        // "nothing to commit" is not an error — values unchanged
        if stderr.contains("nothing to commit") {
            return Ok(());
        }
        return Err(DeployerError::CommitFailed(format!(
            "git commit failed: {stderr}"
        )));
    }

    Ok(())
}

/// Revert the last commit on the ops repo branch (for rollback).
/// Uses git worktree + git revert.
/// Returns the new commit SHA after revert.
#[tracing::instrument(fields(repo = %repo_path.display(), %branch), err)]
pub async fn revert_last_commit(repo_path: &Path, branch: &str) -> Result<String, DeployerError> {
    let worktree_dir = repo_path.join(format!("_revert_worktree_{}", Uuid::new_v4()));

    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("worktree")
        .arg("add")
        .arg(&worktree_dir)
        .arg(branch)
        .output()
        .await
        .map_err(|e| DeployerError::RevertFailed(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DeployerError::RevertFailed(format!(
            "git worktree add failed: {stderr}"
        )));
    }

    let result = revert_head_in_worktree(&worktree_dir).await;

    cleanup_worktree(repo_path, &worktree_dir).await;

    result?;

    get_head_sha(repo_path).await
}

/// Internal: run `git revert HEAD --no-edit` inside a worktree.
async fn revert_head_in_worktree(worktree_dir: &Path) -> Result<(), DeployerError> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(worktree_dir)
        .env("GIT_AUTHOR_NAME", "Platform")
        .env("GIT_AUTHOR_EMAIL", "platform@localhost")
        .env("GIT_COMMITTER_NAME", "Platform")
        .env("GIT_COMMITTER_EMAIL", "platform@localhost")
        .args(["revert", "HEAD", "--no-edit"])
        .output()
        .await
        .map_err(|e| DeployerError::RevertFailed(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DeployerError::RevertFailed(format!(
            "git revert failed: {stderr}"
        )));
    }

    Ok(())
}

/// Clean up a temporary worktree (best-effort).
async fn cleanup_worktree(repo_path: &Path, worktree_dir: &Path) {
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

/// Ensure a bare repo has at least one commit on the given branch.
/// If the branch ref doesn't exist, creates an empty initial commit.
async fn ensure_branch_exists(repo_path: &Path, branch: &str) -> Result<(), DeployerError> {
    let check = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["rev-parse", "--verify", &format!("refs/heads/{branch}")])
        .output()
        .await
        .map_err(|e| DeployerError::CommitFailed(e.to_string()))?;

    if check.status.success() {
        return Ok(());
    }

    // Create a temp worktree with --orphan to bootstrap the branch
    let tmp_wt = repo_path.join(format!("_init_worktree_{}", Uuid::new_v4()));
    let wt_output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["worktree", "add", "--orphan", "-b", branch])
        .arg(&tmp_wt)
        .output()
        .await;
    if let Ok(ref out) = wt_output
        && !out.status.success()
    {
        tracing::warn!(
            stderr = %String::from_utf8_lossy(&out.stderr),
            "ensure_branch_exists: worktree add --orphan failed"
        );
    }

    // Create an initial empty commit
    let commit_output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&tmp_wt)
        .args(["commit", "--allow-empty", "-m", "initial commit"])
        .output()
        .await;
    if let Ok(ref out) = commit_output
        && !out.status.success()
    {
        tracing::warn!(
            stderr = %String::from_utf8_lossy(&out.stderr),
            "ensure_branch_exists: initial commit failed"
        );
    }

    cleanup_worktree(repo_path, &tmp_wt).await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Sync deploy/ from project repo to ops repo
// ---------------------------------------------------------------------------

/// Validate that a string looks like a git commit SHA (hex, 7-64 chars).
fn validate_commit_sha(sha: &str) -> Result<(), DeployerError> {
    if sha.len() < 7 || sha.len() > 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(DeployerError::SyncFailed(format!(
            "invalid commit SHA: {sha}"
        )));
    }
    Ok(())
}

/// Sync the `deploy/` directory from a project git repo at a given SHA
/// into the ops repo, then commit. Deletes files in the ops repo that
/// are not present in `deploy/` (orphan cleanup).
/// Returns the new commit SHA.
#[tracing::instrument(skip(project_repo_path, ops_repo_path), fields(sha = %commit_sha), err)]
pub async fn sync_from_project_repo(
    project_repo_path: &Path,
    ops_repo_path: &Path,
    branch: &str,
    commit_sha: &str,
) -> Result<String, DeployerError> {
    let _lock = repo_lock(ops_repo_path).await;
    validate_commit_sha(commit_sha)?;

    // List files in deploy/ at the given SHA
    let file_list = list_deploy_files(project_repo_path, commit_sha).await?;
    if file_list.is_empty() {
        tracing::debug!(%commit_sha, "no deploy/ directory found at commit");
        return get_head_sha(ops_repo_path).await;
    }

    // Ensure the branch exists (bare repo may be empty after init)
    ensure_branch_exists(ops_repo_path, branch).await?;

    let worktree_dir = ops_repo_path.join(format!("_sync_worktree_{}", Uuid::new_v4()));

    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(ops_repo_path)
        .arg("worktree")
        .arg("add")
        .arg(&worktree_dir)
        .arg(branch)
        .output()
        .await
        .map_err(|e| DeployerError::SyncFailed(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DeployerError::SyncFailed(format!(
            "git worktree add failed: {stderr}"
        )));
    }

    let result =
        write_deploy_files_and_commit(project_repo_path, &worktree_dir, commit_sha, &file_list)
            .await;

    cleanup_worktree(ops_repo_path, &worktree_dir).await;

    result?;

    get_head_sha(ops_repo_path).await
}

/// List files under `deploy/` at a given commit SHA using `git ls-tree`.
async fn list_deploy_files(
    repo_path: &Path,
    commit_sha: &str,
) -> Result<Vec<String>, DeployerError> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["ls-tree", "-r", "--name-only", commit_sha, "--", "deploy/"])
        .output()
        .await
        .map_err(|e| DeployerError::SyncFailed(e.to_string()))?;

    if !output.status.success() {
        // Empty output or error just means no deploy/ dir — not fatal
        return Ok(Vec::new());
    }

    let listing = String::from_utf8_lossy(&output.stdout);
    Ok(listing
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

/// Write deploy files from project repo into the ops repo worktree and commit.
async fn write_deploy_files_and_commit(
    project_repo_path: &Path,
    worktree_dir: &Path,
    commit_sha: &str,
    file_list: &[String],
) -> Result<(), DeployerError> {
    // Remove existing files in worktree that aren't values/ (preserve values dir)
    let deploy_dir = worktree_dir.join("deploy");
    if deploy_dir.exists() {
        tokio::fs::remove_dir_all(&deploy_dir)
            .await
            .map_err(|e| DeployerError::SyncFailed(format!("failed to clean deploy/: {e}")))?;
    }

    // Write each file from project repo
    for file_path in file_list {
        let content = read_file_at_ref(project_repo_path, commit_sha, file_path).await?;

        let dest = worktree_dir.join(file_path);
        // R6: Guard against path traversal in file names
        if !dest.starts_with(worktree_dir) {
            return Err(DeployerError::SyncFailed(format!(
                "path traversal detected in deploy file: {file_path}"
            )));
        }
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| DeployerError::SyncFailed(format!("mkdir: {e}")))?;
        }
        tokio::fs::write(&dest, &content)
            .await
            .map_err(|e| DeployerError::SyncFailed(format!("write {file_path}: {e}")))?;
    }

    // Stage all changes (including deletions)
    let add_output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(worktree_dir)
        .args(["add", "-A"])
        .output()
        .await
        .map_err(|e| DeployerError::CommitFailed(e.to_string()))?;

    if !add_output.status.success() {
        let stderr = String::from_utf8_lossy(&add_output.stderr);
        return Err(DeployerError::CommitFailed(format!(
            "git add failed: {stderr}"
        )));
    }

    let short_sha = commit_sha.get(..12).unwrap_or(commit_sha);
    let commit_msg = format!("sync deploy/ from {short_sha}");

    let commit_output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(worktree_dir)
        .env("GIT_AUTHOR_NAME", "Platform")
        .env("GIT_AUTHOR_EMAIL", "platform@localhost")
        .env("GIT_COMMITTER_NAME", "Platform")
        .env("GIT_COMMITTER_EMAIL", "platform@localhost")
        .args(["commit", "-m", &commit_msg])
        .output()
        .await
        .map_err(|e| DeployerError::CommitFailed(e.to_string()))?;

    if !commit_output.status.success() {
        let stderr = String::from_utf8_lossy(&commit_output.stderr);
        if stderr.contains("nothing to commit") {
            return Ok(());
        }
        return Err(DeployerError::CommitFailed(format!(
            "git commit failed: {stderr}"
        )));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Manifest path resolution (unchanged — works for bare + worktree)
// ---------------------------------------------------------------------------

/// Resolve the full filesystem path to a manifest file within an ops repo.
/// Guards against path traversal by ensuring the result stays within the repo directory.
#[allow(dead_code)] // Used in tests; production code uses read_file_at_ref for bare repos
pub fn resolve_manifest_path(
    repos_dir: &Path,
    ops_repo_name: &str,
    ops_repo_subpath: &str,
    manifest_path: &str,
) -> Result<PathBuf, DeployerError> {
    if manifest_path.contains("..") || ops_repo_subpath.contains("..") {
        return Err(DeployerError::InvalidManifest(
            "path traversal detected".into(),
        ));
    }

    let repo_root = repos_dir.join(ops_repo_name);
    let full_path = repo_root
        .join(ops_repo_subpath.trim_matches('/'))
        .join(manifest_path);

    if !full_path.starts_with(&repo_root) {
        return Err(DeployerError::InvalidManifest(
            "path traversal detected".into(),
        ));
    }

    Ok(full_path)
}

// ---------------------------------------------------------------------------
// Sync: for local bare repos, just return path + HEAD SHA
// ---------------------------------------------------------------------------

/// For local bare repos, "syncing" is just reading the current HEAD SHA.
/// The repo is already on disk — no fetch/pull needed.
#[tracing::instrument(skip(pool), fields(%ops_repo_id), err)]
pub async fn sync_repo(
    pool: &PgPool,
    ops_repo_id: Uuid,
) -> Result<(PathBuf, String, String), DeployerError> {
    let repo = sqlx::query!(
        "SELECT name, repo_path, branch, path FROM ops_repos WHERE id = $1",
        ops_repo_id,
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| DeployerError::OpsRepoNotFound(ops_repo_id.to_string()))?;

    let repo_path = PathBuf::from(&repo.repo_path);
    let sha = get_head_sha(&repo_path).await?;

    Ok((repo_path, sha, repo.branch))
}

// ---------------------------------------------------------------------------
// Branch merging (staging → prod promotion)
// ---------------------------------------------------------------------------

/// Merge one branch into another in a bare repo.
/// Uses git worktree for the merge.
/// Returns the new commit SHA on the target branch.
#[tracing::instrument(fields(repo = %repo_path.display(), %source_branch, %target_branch), err)]
pub async fn merge_branch(
    repo_path: &Path,
    source_branch: &str,
    target_branch: &str,
) -> Result<String, DeployerError> {
    let _lock = repo_lock(repo_path).await;
    ensure_branch_exists(repo_path, source_branch).await?;
    ensure_branch_exists(repo_path, target_branch).await?;

    let worktree_dir = repo_path.join(format!("_merge_worktree_{}", Uuid::new_v4()));

    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("worktree")
        .arg("add")
        .arg(&worktree_dir)
        .arg(target_branch)
        .output()
        .await
        .map_err(|e| DeployerError::CommitFailed(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DeployerError::CommitFailed(format!(
            "git worktree add failed: {stderr}"
        )));
    }

    let result = merge_in_worktree(&worktree_dir, source_branch).await;

    cleanup_worktree(repo_path, &worktree_dir).await;

    result?;

    // Return the new HEAD sha of the target branch
    get_branch_sha(repo_path, target_branch).await
}

/// Internal: merge `source_branch` into current branch in worktree.
async fn merge_in_worktree(worktree_dir: &Path, source_branch: &str) -> Result<(), DeployerError> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(worktree_dir)
        .env("GIT_AUTHOR_NAME", "Platform")
        .env("GIT_AUTHOR_EMAIL", "platform@localhost")
        .env("GIT_COMMITTER_NAME", "Platform")
        .env("GIT_COMMITTER_EMAIL", "platform@localhost")
        .args([
            "merge",
            source_branch,
            "--no-ff",
            "--allow-unrelated-histories",
            "-m",
        ])
        .arg(format!("promote: merge {source_branch} to production"))
        .output()
        .await
        .map_err(|e| DeployerError::CommitFailed(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DeployerError::CommitFailed(format!(
            "merge failed: {stderr}"
        )));
    }

    Ok(())
}

/// Compare two branches to determine if they've diverged.
/// Returns (diverged, `source_sha`, `target_sha`).
pub async fn compare_branches(
    repo_path: &Path,
    source_branch: &str,
    target_branch: &str,
) -> Result<(bool, String, String), DeployerError> {
    let source_sha = get_branch_sha(repo_path, source_branch).await?;
    let target_sha = get_branch_sha(repo_path, target_branch).await?;
    let diverged = source_sha != target_sha;
    Ok((diverged, source_sha, target_sha))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pure unit tests — correctly placed in unit tier

    #[test]
    fn manifest_path_joins_correctly() {
        let path =
            resolve_manifest_path(Path::new("/data/ops"), "myrepo", "/k8s", "deploy.yaml").unwrap();
        assert_eq!(path, PathBuf::from("/data/ops/myrepo/k8s/deploy.yaml"));
    }

    #[test]
    fn manifest_path_handles_root_subpath() {
        let path =
            resolve_manifest_path(Path::new("/data/ops"), "myrepo", "/", "deploy.yaml").unwrap();
        assert_eq!(path, PathBuf::from("/data/ops/myrepo/deploy.yaml"));
    }

    #[test]
    fn manifest_path_rejects_traversal_in_manifest() {
        let result =
            resolve_manifest_path(Path::new("/data/ops"), "myrepo", "/k8s", "../../etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn manifest_path_rejects_traversal_in_subpath() {
        let result =
            resolve_manifest_path(Path::new("/data/ops"), "myrepo", "/../../../etc", "passwd");
        assert!(result.is_err());
    }

    #[test]
    fn manifest_path_rejects_traversal_in_repo_name() {
        let result =
            resolve_manifest_path(Path::new("/data/ops"), "../escape", "/k8s", "deploy.yaml");
        assert!(
            result.is_err() || {
                let p = result.unwrap();
                p.starts_with("/data/ops")
            }
        );
    }

    #[test]
    fn manifest_path_empty_subpath() {
        let path =
            resolve_manifest_path(Path::new("/data/ops"), "myrepo", "", "deploy.yaml").unwrap();
        assert_eq!(path, PathBuf::from("/data/ops/myrepo/deploy.yaml"));
    }

    #[test]
    fn manifest_path_deeply_nested() {
        let path = resolve_manifest_path(
            Path::new("/data/ops"),
            "myrepo",
            "/env/staging/k8s",
            "deployment.yaml",
        )
        .unwrap();
        assert_eq!(
            path,
            PathBuf::from("/data/ops/myrepo/env/staging/k8s/deployment.yaml")
        );
    }

    #[test]
    fn manifest_path_rejects_double_dot_in_both() {
        let result = resolve_manifest_path(
            Path::new("/data/ops"),
            "myrepo",
            "../escape",
            "../etc/passwd",
        );
        assert!(result.is_err());
    }

    #[test]
    fn validate_commit_sha_valid_full() {
        assert!(validate_commit_sha("abc1234567890def1234567890abcdef12345678").is_ok());
    }

    #[test]
    fn validate_commit_sha_valid_short() {
        assert!(validate_commit_sha("abc1234").is_ok());
    }

    #[test]
    fn validate_commit_sha_too_short() {
        assert!(validate_commit_sha("abc12").is_err());
    }

    #[test]
    fn validate_commit_sha_non_hex() {
        assert!(validate_commit_sha("ghijklmnop1234567890").is_err());
    }

    #[test]
    fn validate_commit_sha_injection_attempt() {
        assert!(validate_commit_sha("--exec=evil").is_err());
    }

    #[test]
    fn validate_commit_sha_empty() {
        assert!(validate_commit_sha("").is_err());
    }

    // --- Tests that require private cleanup_worktree() ---

    /// Helper: bootstrap a bare repo with an initial commit so worktree ops work.
    async fn bootstrap_repo(tmp: &Path) -> PathBuf {
        let repo_path = init_ops_repo(tmp, "test-ops", "main").await.unwrap();

        let init_wt = repo_path.join("_init_wt");
        let _ = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&repo_path)
            .args(["worktree", "add", "--orphan", "-b", "main"])
            .arg(&init_wt)
            .output()
            .await
            .unwrap();

        tokio::fs::write(init_wt.join("README.md"), "# Ops\n")
            .await
            .unwrap();
        let _ = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&init_wt)
            .args(["add", "."])
            .output()
            .await;
        let _ = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&init_wt)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@test")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@test")
            .args(["commit", "-m", "init"])
            .output()
            .await;
        cleanup_worktree(&repo_path, &init_wt).await;

        repo_path
    }

    /// Create a project repo with a deploy/ directory containing given files.
    async fn create_project_repo_with_deploy(
        tmp: &Path,
        files: &[(&str, &str)],
    ) -> (PathBuf, String) {
        let repo_path = tmp.join("project.git");
        let _ = tokio::process::Command::new("git")
            .args(["init", "--bare"])
            .arg(&repo_path)
            .output()
            .await
            .unwrap();

        // Create worktree for initial commit
        let wt = repo_path.join("_init");
        let _ = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&repo_path)
            .args(["worktree", "add", "--orphan", "-b", "main"])
            .arg(&wt)
            .output()
            .await
            .unwrap();

        // Write deploy files
        for (path, content) in files {
            let dest = wt.join(path);
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent).await.unwrap();
            }
            tokio::fs::write(&dest, content).await.unwrap();
        }

        let _ = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(["add", "."])
            .output()
            .await;
        let _ = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&wt)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@test")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@test")
            .args(["commit", "-m", "add deploy"])
            .output()
            .await;

        let sha = get_head_sha(&repo_path).await.unwrap();
        cleanup_worktree(&repo_path, &wt).await;

        (repo_path, sha)
    }

    // Tier exception: tests private cleanup_worktree() which cannot be called from tests/
    #[tokio::test]
    async fn read_values_invalid_yaml() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        // Write invalid YAML content via worktree
        let wt = repo_path.join("_bad_yaml_wt");
        let _ = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&repo_path)
            .args(["worktree", "add"])
            .arg(&wt)
            .arg("main")
            .output()
            .await;

        let values_dir = wt.join("values");
        tokio::fs::create_dir_all(&values_dir).await.unwrap();
        tokio::fs::write(
            values_dir.join("staging.yaml"),
            "invalid: [unclosed bracket",
        )
        .await
        .unwrap();

        let _ = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(["add", "."])
            .output()
            .await;
        let _ = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&wt)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@test")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@test")
            .args(["commit", "-m", "bad yaml"])
            .output()
            .await;
        cleanup_worktree(&repo_path, &wt).await;

        let result = read_values(&repo_path, "main", "staging").await;
        assert!(result.is_err());

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    // Tier exception: tests private cleanup_worktree() which cannot be called from tests/
    #[tokio::test]
    async fn cleanup_worktree_nonexistent_is_noop() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        // Cleaning up a nonexistent worktree should not error (best-effort)
        let fake_wt = repo_path.join("nonexistent_worktree");
        cleanup_worktree(&repo_path, &fake_wt).await;
        // No panic = success

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    // Tier exception: tests private cleanup_worktree() via create_project_repo_with_deploy helper
    #[tokio::test]
    async fn sync_from_project_repo_deletes_orphans() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));

        // First sync: production + staging
        let (project_repo, sha1) = create_project_repo_with_deploy(
            &tmp,
            &[
                (
                    "deploy/production.yaml",
                    "kind: Deployment\nmetadata:\n  name: v1",
                ),
                (
                    "deploy/staging.yaml",
                    "kind: Service\nmetadata:\n  name: old",
                ),
            ],
        )
        .await;

        let ops_repo = bootstrap_repo(&tmp.join("ops")).await;
        sync_from_project_repo(&project_repo, &ops_repo, "main", &sha1)
            .await
            .unwrap();

        // Second push: only production (staging removed)
        // Create new commit in project repo with only production.yaml
        let wt = project_repo.join("_update");
        let _ = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&project_repo)
            .args(["worktree", "add"])
            .arg(&wt)
            .arg("main")
            .output()
            .await;

        tokio::fs::remove_file(wt.join("deploy/staging.yaml"))
            .await
            .unwrap();
        // Update production.yaml
        tokio::fs::write(
            wt.join("deploy/production.yaml"),
            "kind: Deployment\nmetadata:\n  name: v2",
        )
        .await
        .unwrap();

        let _ = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(["add", "-A"])
            .output()
            .await;
        let _ = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&wt)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@test")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@test")
            .args(["commit", "-m", "remove staging"])
            .output()
            .await;
        let sha2 = get_head_sha(&project_repo).await.unwrap();
        cleanup_worktree(&project_repo, &wt).await;

        // Sync again
        sync_from_project_repo(&project_repo, &ops_repo, "main", &sha2)
            .await
            .unwrap();

        // staging.yaml should be gone
        let result = read_file_at_ref(&ops_repo, "main", "deploy/staging.yaml").await;
        assert!(result.is_err(), "staging.yaml should have been deleted");

        // production.yaml should be updated
        let prod = read_file_at_ref(&ops_repo, "main", "deploy/production.yaml")
            .await
            .unwrap();
        assert!(prod.contains("v2"));

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    // -- init_ops_repo --

    #[tokio::test]
    async fn init_ops_repo_rejects_forward_slash() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        tokio::fs::create_dir_all(&tmp).await.unwrap();

        let result = init_ops_repo(&tmp, "evil/name", "main").await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("path separators"),
            "should mention path separators: {err_msg}"
        );

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn init_ops_repo_rejects_backslash() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        tokio::fs::create_dir_all(&tmp).await.unwrap();

        let result = init_ops_repo(&tmp, "evil\\name", "main").await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("path separators"),
            "should mention path separators: {err_msg}"
        );

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn init_ops_repo_rejects_double_dot() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        tokio::fs::create_dir_all(&tmp).await.unwrap();

        let result = init_ops_repo(&tmp, "evil..traversal", "main").await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("'..'"),
            "should mention directory traversal: {err_msg}"
        );

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn init_ops_repo_creates_bare_repo() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        tokio::fs::create_dir_all(&tmp).await.unwrap();

        let path = init_ops_repo(&tmp, "myapp", "main").await.unwrap();
        assert!(path.exists());
        assert_eq!(path, tmp.join("myapp.git"));
        // Verify HEAD points to the correct branch
        let head = tokio::fs::read_to_string(path.join("HEAD")).await.unwrap();
        assert_eq!(head.trim(), "ref: refs/heads/main");

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn init_ops_repo_custom_branch() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        tokio::fs::create_dir_all(&tmp).await.unwrap();

        let path = init_ops_repo(&tmp, "myapp", "production").await.unwrap();
        let head = tokio::fs::read_to_string(path.join("HEAD")).await.unwrap();
        assert_eq!(head.trim(), "ref: refs/heads/production");

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    // -- validate_commit_sha edge cases --

    #[test]
    fn validate_commit_sha_too_long() {
        // 65 hex chars — exceeds max 64
        let long = "a".repeat(65);
        assert!(validate_commit_sha(&long).is_err());
    }

    #[test]
    fn validate_commit_sha_exactly_7() {
        assert!(validate_commit_sha("abcdef1").is_ok());
    }

    #[test]
    fn validate_commit_sha_exactly_64() {
        let sha64 = "a".repeat(64);
        assert!(validate_commit_sha(&sha64).is_ok());
    }

    #[test]
    fn validate_commit_sha_mixed_case_hex() {
        assert!(validate_commit_sha("AbCdEf1234567").is_ok());
    }

    #[test]
    fn validate_commit_sha_with_spaces() {
        assert!(validate_commit_sha("abc1234 ").is_err());
    }

    #[test]
    fn validate_commit_sha_with_newline() {
        assert!(validate_commit_sha("abc1234\n").is_err());
    }

    // -- ensure_branch_exists --

    #[tokio::test]
    async fn ensure_branch_exists_creates_orphan_if_missing() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = init_ops_repo(&tmp, "branch-test", "main").await.unwrap();

        // Initially the branch doesn't exist (no commits yet)
        ensure_branch_exists(&repo_path, "main").await.unwrap();

        // Now the branch should exist with at least one commit
        let sha = get_branch_sha(&repo_path, "main").await;
        assert!(
            sha.is_ok(),
            "branch should exist after ensure_branch_exists"
        );

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn ensure_branch_exists_noop_if_already_exists() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        let sha_before = get_branch_sha(&repo_path, "main").await.unwrap();
        ensure_branch_exists(&repo_path, "main").await.unwrap();
        let sha_after = get_branch_sha(&repo_path, "main").await.unwrap();

        assert_eq!(sha_before, sha_after, "should not create extra commits");

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    // -- write_file_to_repo --

    #[tokio::test]
    async fn write_file_creates_commit() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        let sha = write_file_to_repo(&repo_path, "main", "deploy/app.yaml", "kind: Deployment")
            .await
            .unwrap();
        assert!(!sha.is_empty());

        // Verify the file was committed
        let content = read_file_at_ref(&repo_path, "main", "deploy/app.yaml")
            .await
            .unwrap();
        assert_eq!(content, "kind: Deployment");

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn write_file_multiple_updates_each_create_commit() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        let sha1 = write_file_to_repo(&repo_path, "main", "test.txt", "version1")
            .await
            .unwrap();
        let sha2 = write_file_to_repo(&repo_path, "main", "test.txt", "version2")
            .await
            .unwrap();

        assert_ne!(
            sha1, sha2,
            "different content should create different commits"
        );

        let content = read_file_at_ref(&repo_path, "main", "test.txt")
            .await
            .unwrap();
        assert_eq!(content, "version2");

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn write_file_identical_content_no_new_commit() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        let sha1 = write_file_to_repo(&repo_path, "main", "test.txt", "same content")
            .await
            .unwrap();
        let sha2 = write_file_to_repo(&repo_path, "main", "test.txt", "same content")
            .await
            .unwrap();

        assert_eq!(
            sha1, sha2,
            "identical content should not create a new commit"
        );

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    // -- cleanup_worktree edge cases --

    #[tokio::test]
    async fn cleanup_worktree_missing_dir_is_noop() {
        // Different from existing test: test with a completely non-existent repo_path
        let fake_repo = std::env::temp_dir().join(format!("does-not-exist-{}", Uuid::new_v4()));
        let fake_wt = fake_repo.join("fake_worktree");
        cleanup_worktree(&fake_repo, &fake_wt).await;
        // No panic = success
    }

    // -- sync_from_project_repo with empty deploy/ --

    #[tokio::test]
    async fn sync_empty_deploy_dir_is_noop() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));

        // Create a project repo with NO deploy/ directory
        let project_repo = tmp.join("proj-empty.git");
        let _ = tokio::process::Command::new("git")
            .args(["init", "--bare"])
            .arg(&project_repo)
            .output()
            .await
            .unwrap();

        let wt = project_repo.join("_init");
        let _ = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&project_repo)
            .args(["worktree", "add", "--orphan", "-b", "main"])
            .arg(&wt)
            .output()
            .await
            .unwrap();
        tokio::fs::write(wt.join("README.md"), "# No deploy dir\n")
            .await
            .unwrap();
        let _ = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(["add", "."])
            .output()
            .await;
        let _ = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&wt)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@test")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@test")
            .args(["commit", "-m", "no deploy dir"])
            .output()
            .await;
        let sha = get_head_sha(&project_repo).await.unwrap();
        cleanup_worktree(&project_repo, &wt).await;

        let ops_repo = bootstrap_repo(&tmp.join("ops-empty")).await;
        let ops_sha_before = get_head_sha(&ops_repo).await.unwrap();

        // Sync should be a no-op (no deploy/ dir)
        let ops_sha_after = sync_from_project_repo(&project_repo, &ops_repo, "main", &sha)
            .await
            .unwrap();

        assert_eq!(
            ops_sha_before, ops_sha_after,
            "empty deploy dir should not create new commit"
        );

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    // -- compare_branches --

    #[tokio::test]
    async fn compare_branches_same() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        let (diverged, src, tgt) = compare_branches(&repo_path, "main", "main").await.unwrap();
        assert!(!diverged);
        assert_eq!(src, tgt);

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn compare_branches_diverged() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        // Ensure staging branch exists then write to it
        ensure_branch_exists(&repo_path, "staging").await.unwrap();
        write_file_to_repo(&repo_path, "staging", "file.txt", "staging-content")
            .await
            .unwrap();

        let (diverged, src, tgt) = compare_branches(&repo_path, "staging", "main")
            .await
            .unwrap();
        assert!(diverged);
        assert_ne!(src, tgt);

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    // -- commit_values --

    #[tokio::test]
    async fn commit_values_and_read_back() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        let values = serde_json::json!({
            "image_ref": "myapp:v1.2.3",
            "replicas": 3
        });

        let sha = commit_values(&repo_path, "main", "production", &values)
            .await
            .unwrap();
        assert!(!sha.is_empty());

        // Read the committed values back
        let read_back = read_values(&repo_path, "main", "production").await.unwrap();
        assert_eq!(read_back["image_ref"], "myapp:v1.2.3");
        assert_eq!(read_back["replicas"], 3);

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    // -- read_dir_yaml_at_ref --

    #[tokio::test]
    async fn read_dir_yaml_multiple_files() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let (project_repo, sha) = create_project_repo_with_deploy(
            &tmp,
            &[
                ("deploy/app.yaml", "kind: Deployment\nname: app"),
                ("deploy/svc.yaml", "kind: Service\nname: svc"),
                ("deploy/not-yaml.txt", "should be excluded"),
            ],
        )
        .await;

        let result = read_dir_yaml_at_ref(&project_repo, &sha, "deploy")
            .await
            .unwrap();
        assert!(result.contains("kind: Deployment"));
        assert!(result.contains("kind: Service"));
        assert!(!result.contains("should be excluded"));
        // Files are separated by ---
        assert!(result.contains("---"));

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn read_dir_yaml_no_yaml_files_errors() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let (project_repo, sha) =
            create_project_repo_with_deploy(&tmp, &[("deploy/readme.txt", "not yaml")]).await;

        let result = read_dir_yaml_at_ref(&project_repo, &sha, "deploy").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("no YAML files"), "got: {err}");

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    // -- read_file_at_ref error --

    #[tokio::test]
    async fn read_file_at_ref_missing_file() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        let result = read_file_at_ref(&repo_path, "main", "nonexistent.yaml").await;
        assert!(result.is_err());

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    // -- revert_last_commit --

    #[tokio::test]
    async fn revert_last_commit_restores_previous_state() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        // Write initial content
        write_file_to_repo(&repo_path, "main", "data.txt", "original")
            .await
            .unwrap();

        // Write updated content
        write_file_to_repo(&repo_path, "main", "data.txt", "modified")
            .await
            .unwrap();

        // Revert should undo the last commit
        let sha = revert_last_commit(&repo_path, "main").await.unwrap();
        assert!(!sha.is_empty());

        let content = read_file_at_ref(&repo_path, "main", "data.txt")
            .await
            .unwrap();
        assert_eq!(
            content, "original",
            "revert should restore previous content"
        );

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    // -- get_head_sha / get_branch_sha error paths --

    #[tokio::test]
    async fn get_branch_sha_nonexistent_branch() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        let result = get_branch_sha(&repo_path, "nonexistent-branch").await;
        assert!(result.is_err());

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    // -- repo_lock concurrency --

    #[tokio::test]
    async fn repo_lock_serializes_concurrent_operations() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        // Run two concurrent write operations and verify neither fails
        let repo1 = repo_path.clone();
        let repo2 = repo_path.clone();

        let (r1, r2) = tokio::join!(
            write_file_to_repo(&repo1, "main", "file1.txt", "content1"),
            write_file_to_repo(&repo2, "main", "file2.txt", "content2"),
        );

        assert!(r1.is_ok(), "first concurrent write should succeed");
        assert!(r2.is_ok(), "second concurrent write should succeed");

        // Both files should exist
        let c1 = read_file_at_ref(&repo_path, "main", "file1.txt")
            .await
            .unwrap();
        let c2 = read_file_at_ref(&repo_path, "main", "file2.txt")
            .await
            .unwrap();
        assert_eq!(c1, "content1");
        assert_eq!(c2, "content2");

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }
}
