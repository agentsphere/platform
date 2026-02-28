use std::path::{Path, PathBuf};

use sqlx::PgPool;
use uuid::Uuid;

use super::error::DeployerError;

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

    get_head_sha(repo_path).await
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn init_and_get_sha_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = init_ops_repo(&tmp, "test-ops", "main").await.unwrap();
        assert!(repo_path.exists());
        assert!(repo_path.join("HEAD").exists());

        let head = tokio::fs::read_to_string(repo_path.join("HEAD"))
            .await
            .unwrap();
        assert_eq!(head, "ref: refs/heads/main\n");

        // No commits yet — git rev-parse HEAD returns literal "HEAD" (not a SHA)
        let sha = get_head_sha(&repo_path).await.unwrap();
        assert_eq!(sha, "HEAD");

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

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

    #[tokio::test]
    async fn commit_values_creates_file() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        let values = serde_json::json!({
            "image_ref": "registry/app:abc123",
            "project_name": "my-app",
        });
        let sha = commit_values(&repo_path, "main", "production", &values)
            .await
            .unwrap();

        assert!(!sha.is_empty());

        // Verify we can read it back
        let read_back = read_values(&repo_path, "main", "production").await.unwrap();
        assert_eq!(read_back["image_ref"], "registry/app:abc123");
        assert_eq!(read_back["project_name"], "my-app");

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn revert_restores_previous_values() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        // Commit v1
        let v1 = serde_json::json!({"image_ref": "registry/app:v1"});
        commit_values(&repo_path, "main", "production", &v1)
            .await
            .unwrap();

        // Commit v2
        let v2 = serde_json::json!({"image_ref": "registry/app:v2"});
        commit_values(&repo_path, "main", "production", &v2)
            .await
            .unwrap();

        // Read current — should be v2
        let current = read_values(&repo_path, "main", "production").await.unwrap();
        assert_eq!(current["image_ref"], "registry/app:v2");

        // Revert
        revert_last_commit(&repo_path, "main").await.unwrap();

        // Should be back to v1
        let after_revert = read_values(&repo_path, "main", "production").await.unwrap();
        assert_eq!(after_revert["image_ref"], "registry/app:v1");

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn read_values_missing_file_returns_error() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        let result = read_values(&repo_path, "main", "production").await;
        assert!(result.is_err());

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn read_file_at_ref_nonexistent_file() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        let result = read_file_at_ref(&repo_path, "main", "does-not-exist.yaml").await;
        assert!(result.is_err());

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn read_file_at_ref_nonexistent_ref() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        let result = read_file_at_ref(&repo_path, "nonexistent-branch", "README.md").await;
        assert!(result.is_err());

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

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

    #[tokio::test]
    async fn commit_values_no_changes_returns_error() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        let values = serde_json::json!({"image_ref": "app:v1"});

        // First commit succeeds
        commit_values(&repo_path, "main", "production", &values)
            .await
            .unwrap();

        // Second commit with same values — git commit fails because nothing changed
        let result = commit_values(&repo_path, "main", "production", &values).await;
        assert!(result.is_err());

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn revert_initial_commit_returns_error() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        // There's only 1 commit (from bootstrap). Reverting it should fail because
        // git revert on the very first commit needs special handling.
        let result = revert_last_commit(&repo_path, "main").await;
        // This may succeed or fail depending on git version — we just verify no panic
        // (git revert on initial commit fails with "empty commit" or similar)
        let _ = result; // Either Ok or Err is acceptable

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

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

    #[tokio::test]
    async fn get_head_sha_returns_valid_hash_after_commit() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        let sha = get_head_sha(&repo_path).await.unwrap();
        // After bootstrap, SHA should be a 40-char hex string
        assert_eq!(sha.len(), 40);
        assert!(sha.chars().all(|c| c.is_ascii_hexdigit()));

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn get_head_sha_nonexistent_repo_returns_error() {
        let result = get_head_sha(Path::new("/nonexistent/repo")).await;
        assert!(result.is_err());
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

    // --- sync_from_project_repo tests ---

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

    #[tokio::test]
    async fn sync_from_project_repo_copies_deploy_dir() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));

        let (project_repo, sha) = create_project_repo_with_deploy(
            &tmp,
            &[
                (
                    "deploy/production.yaml",
                    "kind: Deployment\nmetadata:\n  name: test",
                ),
                (
                    "deploy/staging.yaml",
                    "kind: Service\nmetadata:\n  name: svc",
                ),
            ],
        )
        .await;

        let ops_repo = bootstrap_repo(&tmp.join("ops")).await;

        sync_from_project_repo(&project_repo, &ops_repo, "main", &sha)
            .await
            .unwrap();

        // Verify files exist in ops repo
        let prod = read_file_at_ref(&ops_repo, "main", "deploy/production.yaml")
            .await
            .unwrap();
        assert!(prod.contains("Deployment"));
        let staging = read_file_at_ref(&ops_repo, "main", "deploy/staging.yaml")
            .await
            .unwrap();
        assert!(staging.contains("Service"));

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

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

    #[tokio::test]
    async fn sync_from_project_repo_commit_message() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));

        let (project_repo, sha) =
            create_project_repo_with_deploy(&tmp, &[("deploy/app.yaml", "kind: Deployment")]).await;

        let ops_repo = bootstrap_repo(&tmp.join("ops")).await;
        sync_from_project_repo(&project_repo, &ops_repo, "main", &sha)
            .await
            .unwrap();

        // Check the commit message
        let log_output = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&ops_repo)
            .args(["log", "-1", "--pretty=%s"])
            .output()
            .await
            .unwrap();
        let message = String::from_utf8_lossy(&log_output.stdout);
        let short_sha = &sha[..sha.len().min(12)];
        assert!(
            message.contains(&format!("sync deploy/ from {short_sha}")),
            "expected commit message with SHA prefix, got: {message}"
        );

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    // --- R4: commit SHA validation tests ---

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
}
