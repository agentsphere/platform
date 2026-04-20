// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

use std::path::{Path, PathBuf};

use platform_git::{CliGitMerger, CliGitRepo, CliGitWorktreeWriter};
use platform_types::{GitCoreRead, GitMerger, GitWriter};
use sqlx::{PgPool, Row};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum OpsRepoError {
    #[error("ops repo not found: {0}")]
    NotFound(String),

    #[error("ops repo sync failed: {0}")]
    SyncFailed(String),

    #[error("ops repo commit failed: {0}")]
    CommitFailed(String),

    #[error("ops repo revert failed: {0}")]
    RevertFailed(String),

    #[error("values file not found: {0}")]
    ValuesNotFound(String),

    #[error("template render failed: {0}")]
    RenderFailed(String),

    #[error("invalid manifest: {0}")]
    InvalidManifest(String),

    #[error(transparent)]
    Db(#[from] sqlx::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<platform_types::GitError> for OpsRepoError {
    fn from(err: platform_types::GitError) -> Self {
        match err {
            platform_types::GitError::FileNotFound { git_ref, path } => {
                Self::NotFound(format!("{git_ref}:{path}"))
            }
            platform_types::GitError::RefNotFound(r) => Self::NotFound(r),
            platform_types::GitError::MergeConflict(msg) => Self::CommitFailed(msg),
            other => Self::Other(other.into()),
        }
    }
}

// Zero-cost module-level instances (no fields, no state).
const GIT: CliGitRepo = CliGitRepo;
const WRITER: CliGitWorktreeWriter = CliGitWorktreeWriter;
const MERGER: CliGitMerger = CliGitMerger;

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
) -> Result<PathBuf, OpsRepoError> {
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(OpsRepoError::SyncFailed(
            "invalid ops repo name: must not contain path separators or '..'".into(),
        ));
    }
    let dest = repos_dir.join(format!("{name}.git"));

    tokio::fs::create_dir_all(&dest)
        .await
        .map_err(|e| OpsRepoError::SyncFailed(format!("failed to create repo dir: {e}")))?;

    let output = tokio::process::Command::new("git")
        .arg("init")
        .arg("--bare")
        .arg(&dest)
        .output()
        .await
        .map_err(|e| OpsRepoError::SyncFailed(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(OpsRepoError::SyncFailed(format!(
            "git init failed: {stderr}"
        )));
    }

    // Set default branch
    let head_ref = format!("ref: refs/heads/{branch}\n");
    tokio::fs::write(dest.join("HEAD"), head_ref)
        .await
        .map_err(|e| OpsRepoError::SyncFailed(format!("failed to set HEAD: {e}")))?;

    tracing::info!(path = %dest.display(), "ops repo initialized");
    Ok(dest)
}

// ---------------------------------------------------------------------------
// Reading from bare repos (no working tree needed)
// ---------------------------------------------------------------------------

/// Get the current HEAD SHA of a bare repo.
pub async fn get_head_sha(repo_path: &Path) -> Result<String, OpsRepoError> {
    Ok(GIT.rev_parse(repo_path, "HEAD").await?)
}

/// Get the SHA of a specific branch in a bare repo.
pub async fn get_branch_sha(repo_path: &Path, branch: &str) -> Result<String, OpsRepoError> {
    Ok(GIT
        .rev_parse(repo_path, &format!("refs/heads/{branch}"))
        .await?)
}

/// Read a file from a bare repo at a given ref without a working tree.
/// Uses `git show {ref}:{path}`.
pub async fn read_file_at_ref(
    repo_path: &Path,
    git_ref: &str,
    file_path: &str,
) -> Result<String, OpsRepoError> {
    GIT.read_file(repo_path, git_ref, file_path)
        .await?
        .ok_or_else(|| OpsRepoError::ValuesNotFound(format!("{file_path} at {git_ref}")))
}

/// Read all YAML files from a directory in a bare repo at a given ref.
/// Returns concatenated content with `---` separators.
pub async fn read_dir_yaml_at_ref(
    repo_path: &Path,
    git_ref: &str,
    dir_path: &str,
) -> Result<String, OpsRepoError> {
    let dir = dir_path.trim_end_matches('/');
    let entries = GIT
        .list_dir(repo_path, git_ref, dir)
        .await
        .map_err(|_| OpsRepoError::ValuesNotFound(format!("{dir_path} at {git_ref}")))?;

    let mut combined = String::new();

    for file_name in &entries {
        let ext = std::path::Path::new(file_name)
            .extension()
            .and_then(|e| e.to_str());
        // Skip values/variables files — they're not K8s manifests
        let basename = std::path::Path::new(file_name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if matches!(ext, Some("yaml" | "yml")) && !basename.starts_with("variables_") {
            if !combined.is_empty() {
                combined.push_str("\n---\n");
            }
            let path = format!("{dir}/{file_name}");
            let content = read_file_at_ref(repo_path, git_ref, &path).await?;
            combined.push_str(&content);
        }
    }

    if combined.is_empty() {
        return Err(OpsRepoError::ValuesNotFound(format!(
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
) -> Result<serde_json::Value, OpsRepoError> {
    let file_path = format!("values/{environment}.yaml");
    let content = read_file_at_ref(repo_path, branch, &file_path).await?;

    serde_yaml::from_str(&content)
        .map_err(|e| OpsRepoError::RenderFailed(format!("failed to parse {file_path}: {e}")))
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
) -> Result<String, OpsRepoError> {
    let yaml_content = serde_yaml::to_string(values)
        .map_err(|e| OpsRepoError::CommitFailed(format!("yaml serialize: {e}")))?;

    let file_path = format!("values/{environment}.yaml");
    let image_ref = values
        .get("image_ref")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let msg = format!("deploy({environment}): update image to {image_ref}");

    Ok(WRITER
        .commit_files(
            repo_path,
            branch,
            &[(&file_path, yaml_content.as_bytes())],
            &msg,
        )
        .await?)
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
    msg: Option<&str>,
) -> Result<String, OpsRepoError> {
    let default_msg = format!("update {file_path}");
    let message = msg.unwrap_or(&default_msg);
    Ok(WRITER
        .commit_files(
            repo_path,
            branch,
            &[(file_path, content.as_bytes())],
            message,
        )
        .await?)
}

/// Revert the last commit on the ops repo branch (for rollback).
/// Uses git worktree + git revert.
/// Returns the new commit SHA after revert.
#[tracing::instrument(fields(repo = %repo_path.display(), %branch), err)]
pub async fn revert_last_commit(repo_path: &Path, branch: &str) -> Result<String, OpsRepoError> {
    Ok(WRITER.revert_head(repo_path, branch).await?)
}

// ---------------------------------------------------------------------------
// Sync deploy/ from project repo to ops repo
// ---------------------------------------------------------------------------

/// Validate that a string looks like a git commit SHA (hex, 7-64 chars).
fn validate_commit_sha(sha: &str) -> Result<(), OpsRepoError> {
    if sha.len() < 7 || sha.len() > 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(OpsRepoError::SyncFailed(format!(
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
) -> Result<String, OpsRepoError> {
    validate_commit_sha(commit_sha)?;

    // List files in deploy/ at the given SHA
    let file_list = GIT
        .list_tree_recursive(project_repo_path, commit_sha, "deploy")
        .await
        .map_err(|e| OpsRepoError::SyncFailed(e.to_string()))?;

    if file_list.is_empty() {
        tracing::debug!(%commit_sha, "no deploy/ directory found at commit");
        return get_head_sha(ops_repo_path).await.or_else(|_| {
            // Ops repo may have no commits yet — return a sentinel
            Ok(String::from("0000000000000000000000000000000000000000"))
        });
    }

    // Read each file from project repo
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    for file_path in &file_list {
        // Path traversal guard
        if file_path.contains("..") {
            return Err(OpsRepoError::SyncFailed(format!(
                "path traversal detected in deploy file: {file_path}"
            )));
        }
        let content = read_file_at_ref(project_repo_path, commit_sha, file_path).await?;
        files.push((file_path.clone(), content.into_bytes()));
    }

    let file_refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_slice()))
        .collect();
    let short_sha = commit_sha.get(..12).unwrap_or(commit_sha);
    let msg = format!("sync deploy/ from {short_sha}");

    Ok(WRITER
        .commit_all(ops_repo_path, branch, &file_refs, &["deploy"], &msg)
        .await?)
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
) -> Result<PathBuf, OpsRepoError> {
    if manifest_path.contains("..") || ops_repo_subpath.contains("..") {
        return Err(OpsRepoError::InvalidManifest(
            "path traversal detected".into(),
        ));
    }

    let repo_root = repos_dir.join(ops_repo_name);
    let full_path = repo_root
        .join(ops_repo_subpath.trim_matches('/'))
        .join(manifest_path);

    if !full_path.starts_with(&repo_root) {
        return Err(OpsRepoError::InvalidManifest(
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
) -> Result<(PathBuf, String, String), OpsRepoError> {
    let repo = sqlx::query("SELECT name, repo_path, branch, path FROM ops_repos WHERE id = $1")
        .bind(ops_repo_id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| OpsRepoError::NotFound(ops_repo_id.to_string()))?;

    let repo_path = PathBuf::from(repo.get::<String, _>("repo_path"));
    let branch: String = repo.get("branch");
    let sha = get_head_sha(&repo_path).await?;

    Ok((repo_path, sha, branch))
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
) -> Result<String, OpsRepoError> {
    let msg = format!("promote: merge {source_branch} to production");
    Ok(MERGER
        .merge_no_ff(repo_path, source_branch, target_branch, &msg)
        .await?)
}

/// Compare two branches to determine if they've diverged.
/// Returns (diverged, `source_sha`, `target_sha`).
pub async fn compare_branches(
    repo_path: &Path,
    source_branch: &str,
    target_branch: &str,
) -> Result<(bool, String, String), OpsRepoError> {
    let source_sha = get_branch_sha(repo_path, source_branch).await?;
    let target_sha = get_branch_sha(repo_path, target_branch).await?;
    let diverged = source_sha != target_sha;
    Ok((diverged, source_sha, target_sha))
}

// ---------------------------------------------------------------------------
// OpsRepoManager trait implementation
// ---------------------------------------------------------------------------

use platform_types::OpsRepoManager;

/// Service struct that implements `OpsRepoManager` from `platform-types`.
///
/// Holds a `PgPool` for the `sync_from_project` method which needs to
/// look up ops repo paths by project ID.
#[derive(Clone)]
pub struct OpsRepoService {
    pool: PgPool,
}

impl OpsRepoService {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl OpsRepoManager for OpsRepoService {
    async fn read_file(&self, repo_path: &Path, git_ref: &str, file: &str) -> Option<String> {
        read_file_at_ref(repo_path, git_ref, file).await.ok()
    }

    async fn sync_from_project(
        &self,
        project_id: Uuid,
        source: &Path,
        branch: &str,
    ) -> anyhow::Result<()> {
        // Look up the ops repo path directly (not via sync_repo, which calls
        // get_head_sha and fails on freshly-initialised empty bare repos).
        let ops_repo_path: String = sqlx::query_scalar(
            "SELECT o.repo_path FROM ops_repos o WHERE o.project_id = $1 LIMIT 1",
        )
        .bind(project_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no ops repo for project {project_id}"))?;

        let source_sha = get_head_sha(source).await?;
        sync_from_project_repo(source, Path::new(&ops_repo_path), branch, &source_sha).await?;
        Ok(())
    }

    async fn write_file(
        &self,
        repo_path: &Path,
        branch: &str,
        file: &str,
        content: &[u8],
        msg: &str,
    ) -> anyhow::Result<String> {
        let text = std::str::from_utf8(content)
            .map_err(|e| anyhow::anyhow!("content is not valid UTF-8: {e}"))?;
        let sha = write_file_to_repo(repo_path, branch, file, text, Some(msg)).await?;
        Ok(sha)
    }

    async fn read_dir_yaml(&self, repo_path: &Path, git_ref: &str, dir: &str) -> Option<String> {
        read_dir_yaml_at_ref(repo_path, git_ref, dir).await.ok()
    }

    async fn commit_values(
        &self,
        ops_path: &Path,
        branch: &str,
        values: &[(&str, &str)],
        msg: &str,
    ) -> anyhow::Result<String> {
        // Read existing values, merge key-value pairs, write back
        let existing = read_file_at_ref(ops_path, branch, "values.yaml")
            .await
            .ok()
            .and_then(|s| serde_yaml::from_str::<serde_json::Value>(&s).ok())
            .unwrap_or(serde_json::json!({}));

        let mut map = existing.as_object().cloned().unwrap_or_default();
        for (k, v) in values {
            map.insert(
                (*k).to_string(),
                serde_json::Value::String((*v).to_string()),
            );
        }

        let yaml = serde_yaml::to_string(&serde_json::Value::Object(map))
            .map_err(|e| anyhow::anyhow!("yaml serialize: {e}"))?;
        let sha = write_file_to_repo(ops_path, branch, "values.yaml", &yaml, Some(msg)).await?;
        Ok(sha)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- OpsRepoError display tests --

    #[test]
    fn error_not_found_display() {
        let err = OpsRepoError::NotFound("main:values.yaml".into());
        assert_eq!(err.to_string(), "ops repo not found: main:values.yaml");
    }

    #[test]
    fn error_sync_failed_display() {
        let err = OpsRepoError::SyncFailed("connection refused".into());
        assert_eq!(err.to_string(), "ops repo sync failed: connection refused");
    }

    #[test]
    fn error_commit_failed_display() {
        let err = OpsRepoError::CommitFailed("merge conflict".into());
        assert_eq!(err.to_string(), "ops repo commit failed: merge conflict");
    }

    #[test]
    fn error_revert_failed_display() {
        let err = OpsRepoError::RevertFailed("dirty worktree".into());
        assert_eq!(err.to_string(), "ops repo revert failed: dirty worktree");
    }

    #[test]
    fn error_values_not_found_display() {
        let err = OpsRepoError::ValuesNotFound("staging.yaml".into());
        assert_eq!(err.to_string(), "values file not found: staging.yaml");
    }

    #[test]
    fn error_render_failed_display() {
        let err = OpsRepoError::RenderFailed("missing variable".into());
        assert_eq!(err.to_string(), "template render failed: missing variable");
    }

    #[test]
    fn error_invalid_manifest_display() {
        let err = OpsRepoError::InvalidManifest("missing kind".into());
        assert_eq!(err.to_string(), "invalid manifest: missing kind");
    }

    // -- From conversion tests --

    #[test]
    fn from_sqlx_creates_db() {
        let err: OpsRepoError = sqlx::Error::RowNotFound.into();
        assert!(matches!(err, OpsRepoError::Db(_)));
    }

    #[test]
    fn from_anyhow_creates_other() {
        let err: OpsRepoError = anyhow::anyhow!("unexpected").into();
        assert!(matches!(err, OpsRepoError::Other(_)));
    }

    #[test]
    fn from_git_error_file_not_found_maps_to_not_found() {
        let git_err = platform_types::GitError::FileNotFound {
            git_ref: "main".into(),
            path: "values.yaml".into(),
        };
        let err: OpsRepoError = git_err.into();
        match err {
            OpsRepoError::NotFound(msg) => assert_eq!(msg, "main:values.yaml"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn from_git_error_ref_not_found_maps_to_not_found() {
        let git_err = platform_types::GitError::RefNotFound("missing-branch".into());
        let err: OpsRepoError = git_err.into();
        match err {
            OpsRepoError::NotFound(msg) => assert_eq!(msg, "missing-branch"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn from_git_error_merge_conflict_maps_to_commit_failed() {
        let git_err = platform_types::GitError::MergeConflict("conflict in values.yaml".into());
        let err: OpsRepoError = git_err.into();
        match err {
            OpsRepoError::CommitFailed(msg) => assert_eq!(msg, "conflict in values.yaml"),
            other => panic!("expected CommitFailed, got {other:?}"),
        }
    }

    #[test]
    fn from_git_error_other_maps_to_other() {
        let git_err = platform_types::GitError::CommandFailed {
            command: "push".into(),
            stderr: "fatal error".into(),
        };
        let err: OpsRepoError = git_err.into();
        assert!(matches!(err, OpsRepoError::Other(_)));
    }

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

    // --- Test helpers ---

    /// Helper: bootstrap a bare repo with an initial commit so worktree ops work.
    async fn bootstrap_repo(tmp: &Path) -> PathBuf {
        let repo_path = init_ops_repo(tmp, "test-ops", "main").await.unwrap();
        // Create the initial commit via platform-git's worktree writer
        WRITER
            .commit_files(&repo_path, "main", &[("README.md", b"# Ops\n")], "init")
            .await
            .unwrap();
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
        // Set default branch
        tokio::fs::write(repo_path.join("HEAD"), "ref: refs/heads/main\n")
            .await
            .unwrap();

        // Use platform-git to commit files
        let file_data: Vec<(&str, Vec<u8>)> = files
            .iter()
            .map(|(p, c)| (*p, c.as_bytes().to_vec()))
            .collect();
        let file_refs: Vec<(&str, &[u8])> =
            file_data.iter().map(|(p, c)| (*p, c.as_slice())).collect();

        WRITER
            .commit_files(&repo_path, "main", &file_refs, "add deploy")
            .await
            .unwrap();

        let sha = get_head_sha(&repo_path).await.unwrap();
        (repo_path, sha)
    }

    #[tokio::test]
    async fn read_values_invalid_yaml() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        // Write invalid YAML content via platform-git
        WRITER
            .commit_files(
                &repo_path,
                "main",
                &[("values/staging.yaml", b"invalid: [unclosed bracket")],
                "bad yaml",
            )
            .await
            .unwrap();

        let result = read_values(&repo_path, "main", "staging").await;
        assert!(result.is_err());

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

        // Second push: only production (staging removed) — use commit_all to replace
        WRITER
            .commit_all(
                &project_repo,
                "main",
                &[(
                    "deploy/production.yaml",
                    b"kind: Deployment\nmetadata:\n  name: v2" as &[u8],
                )],
                &["deploy"],
                "remove staging",
            )
            .await
            .unwrap();
        let sha2 = get_head_sha(&project_repo).await.unwrap();

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

    // ensure_branch_exists and cleanup_worktree are now tested in platform-git

    // -- write_file_to_repo --

    #[tokio::test]
    async fn write_file_creates_commit() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        let sha = write_file_to_repo(
            &repo_path,
            "main",
            "deploy/app.yaml",
            "kind: Deployment",
            None,
        )
        .await
        .unwrap();
        assert!(!sha.is_empty());

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

        let sha1 = write_file_to_repo(&repo_path, "main", "test.txt", "version1", None)
            .await
            .unwrap();
        let sha2 = write_file_to_repo(&repo_path, "main", "test.txt", "version2", None)
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

        let sha1 = write_file_to_repo(&repo_path, "main", "test.txt", "same content", None)
            .await
            .unwrap();
        let sha2 = write_file_to_repo(&repo_path, "main", "test.txt", "same content", None)
            .await
            .unwrap();

        assert_eq!(
            sha1, sha2,
            "identical content should not create a new commit"
        );

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    // cleanup_worktree edge cases are now tested in platform-git

    // -- sync_from_project_repo with empty deploy/ --

    #[tokio::test]
    async fn sync_empty_deploy_dir_is_noop() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));

        // Create project repo with no deploy/ directory
        let project_repo = tmp.join("proj-empty.git");
        let _ = tokio::process::Command::new("git")
            .args(["init", "--bare"])
            .arg(&project_repo)
            .output()
            .await
            .unwrap();
        tokio::fs::write(project_repo.join("HEAD"), "ref: refs/heads/main\n")
            .await
            .unwrap();
        WRITER
            .commit_files(
                &project_repo,
                "main",
                &[("README.md", b"# No deploy dir\n")],
                "no deploy dir",
            )
            .await
            .unwrap();
        let sha = get_head_sha(&project_repo).await.unwrap();

        let ops_repo = bootstrap_repo(&tmp.join("ops-empty")).await;
        let ops_sha_before = get_head_sha(&ops_repo).await.unwrap();

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

        write_file_to_repo(&repo_path, "staging", "file.txt", "staging-content", None)
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

        write_file_to_repo(&repo_path, "main", "data.txt", "original", None)
            .await
            .unwrap();

        write_file_to_repo(&repo_path, "main", "data.txt", "modified", None)
            .await
            .unwrap();

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

    // -- concurrent write serialization (via platform-git repo_lock) --

    #[tokio::test]
    async fn concurrent_writes_serialize_correctly() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        let repo1 = repo_path.clone();
        let repo2 = repo_path.clone();

        let (r1, r2) = tokio::join!(
            write_file_to_repo(&repo1, "main", "file1.txt", "content1", None),
            write_file_to_repo(&repo2, "main", "file2.txt", "content2", None),
        );

        assert!(r1.is_ok(), "first concurrent write should succeed");
        assert!(r2.is_ok(), "second concurrent write should succeed");

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

    // -- read_dir_yaml_at_ref: trailing slash handling --

    #[tokio::test]
    async fn read_dir_yaml_trailing_slash_stripped() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let (project_repo, sha) = create_project_repo_with_deploy(
            &tmp,
            &[("deploy/app.yaml", "kind: Deployment\nname: app")],
        )
        .await;

        let result = read_dir_yaml_at_ref(&project_repo, &sha, "deploy/").await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("kind: Deployment"));

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    // -- read_dir_yaml_at_ref: yml extension --

    #[tokio::test]
    async fn read_dir_yaml_includes_yml_extension() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let (project_repo, sha) = create_project_repo_with_deploy(
            &tmp,
            &[("deploy/service.yml", "kind: Service\nname: svc")],
        )
        .await;

        let result = read_dir_yaml_at_ref(&project_repo, &sha, "deploy").await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("kind: Service"));

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    // -- merge_branch --

    #[tokio::test]
    async fn merge_branch_integrates_changes() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        // Create staging branch with content
        write_file_to_repo(
            &repo_path,
            "staging",
            "deploy/app.yaml",
            "kind: Deployment\nv2",
            None,
        )
        .await
        .unwrap();

        // Create production branch
        write_file_to_repo(&repo_path, "production", "README.md", "# Production", None)
            .await
            .unwrap();

        let sha = merge_branch(&repo_path, "staging", "production")
            .await
            .unwrap();
        assert!(!sha.is_empty());

        let content = read_file_at_ref(&repo_path, &sha, "deploy/app.yaml")
            .await
            .unwrap();
        assert!(content.contains("v2"));

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    // -- validate_commit_sha: 6-char boundary --

    #[test]
    fn validate_commit_sha_exactly_6_fails() {
        assert!(validate_commit_sha("abcdef").is_err());
    }

    // -- init_ops_repo: creates correct directory structure --

    #[tokio::test]
    async fn init_ops_repo_appends_git_suffix() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        tokio::fs::create_dir_all(&tmp).await.unwrap();

        let path = init_ops_repo(&tmp, "test-repo", "main").await.unwrap();
        assert!(path.to_str().unwrap().ends_with("test-repo.git"));

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    // -- write_file_to_repo: nested directories --

    #[tokio::test]
    async fn write_file_to_nested_directory() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        let sha = write_file_to_repo(
            &repo_path,
            "main",
            "deep/nested/dir/config.yaml",
            "key: value",
            None,
        )
        .await
        .unwrap();
        assert!(!sha.is_empty());

        let content = read_file_at_ref(&repo_path, "main", "deep/nested/dir/config.yaml")
            .await
            .unwrap();
        assert_eq!(content, "key: value");

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    // -- commit_values: image_ref in commit message --

    #[tokio::test]
    async fn commit_values_without_image_ref() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        let values = serde_json::json!({
            "replicas": 2,
            "memory": "512Mi"
        });

        let sha = commit_values(&repo_path, "main", "staging", &values)
            .await
            .unwrap();
        assert!(!sha.is_empty());

        let read_back = read_values(&repo_path, "main", "staging").await.unwrap();
        assert_eq!(read_back["replicas"], 2);

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    // -- read_values: missing file --

    #[tokio::test]
    async fn read_values_missing_environment_errors() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        let result = read_values(&repo_path, "main", "nonexistent").await;
        assert!(result.is_err());

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    // -- resolve_manifest_path: additional edge cases --

    #[test]
    fn manifest_path_with_leading_slash_in_subpath() {
        let path =
            resolve_manifest_path(Path::new("/data/ops"), "myrepo", "/subdir/", "deploy.yaml")
                .unwrap();
        assert_eq!(path, PathBuf::from("/data/ops/myrepo/subdir/deploy.yaml"));
    }

    #[test]
    fn manifest_path_multiple_subdirectories() {
        let path = resolve_manifest_path(Path::new("/data/ops"), "myrepo", "/a/b/c", "deploy.yaml")
            .unwrap();
        assert_eq!(path, PathBuf::from("/data/ops/myrepo/a/b/c/deploy.yaml"));
    }

    // -- OpsRepoService trait impl --

    #[tokio::test]
    async fn ops_repo_service_read_file_delegates() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        write_file_to_repo(&repo_path, "main", "config.yaml", "key: value", None)
            .await
            .unwrap();

        // Use the trait method via OpsRepoService (pool unused for read_file)
        let svc = OpsRepoService {
            pool: sqlx::PgPool::connect_lazy("postgres://unused@localhost/unused").unwrap(),
        };
        let result = OpsRepoManager::read_file(&svc, &repo_path, "main", "config.yaml").await;
        assert_eq!(result, Some("key: value".to_string()));

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn ops_repo_service_read_file_returns_none_on_missing() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        let svc = OpsRepoService {
            pool: sqlx::PgPool::connect_lazy("postgres://unused@localhost/unused").unwrap(),
        };
        let result = OpsRepoManager::read_file(&svc, &repo_path, "main", "nonexistent.yaml").await;
        assert_eq!(result, None);

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn ops_repo_service_write_file_with_msg() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        let svc = OpsRepoService {
            pool: sqlx::PgPool::connect_lazy("postgres://unused@localhost/unused").unwrap(),
        };
        let sha = OpsRepoManager::write_file(
            &svc,
            &repo_path,
            "main",
            "deploy/app.yaml",
            b"kind: Service",
            "custom commit message",
        )
        .await
        .unwrap();
        assert!(!sha.is_empty());

        // Verify the file was written
        let content = read_file_at_ref(&repo_path, "main", "deploy/app.yaml")
            .await
            .unwrap();
        assert_eq!(content, "kind: Service");

        // Verify custom commit message was used
        let log = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&repo_path)
            .args(["log", "-1", "--format=%s", "main"])
            .output()
            .await
            .unwrap();
        let msg = String::from_utf8_lossy(&log.stdout).trim().to_string();
        assert_eq!(msg, "custom commit message");

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn ops_repo_service_read_dir_yaml_returns_none_on_missing() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        let svc = OpsRepoService {
            pool: sqlx::PgPool::connect_lazy("postgres://unused@localhost/unused").unwrap(),
        };
        let result = OpsRepoManager::read_dir_yaml(&svc, &repo_path, "main", "nonexistent/").await;
        assert!(result.is_none());

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn ops_repo_service_commit_values_merges_kv() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
        let repo_path = bootstrap_repo(&tmp).await;

        let svc = OpsRepoService {
            pool: sqlx::PgPool::connect_lazy("postgres://unused@localhost/unused").unwrap(),
        };

        // First commit: set initial values
        let sha1 = OpsRepoManager::commit_values(
            &svc,
            &repo_path,
            "main",
            &[("image", "app:v1"), ("replicas", "3")],
            "set initial values",
        )
        .await
        .unwrap();
        assert!(!sha1.is_empty());

        // Second commit: update one value, add another
        let sha2 = OpsRepoManager::commit_values(
            &svc,
            &repo_path,
            "main",
            &[("image", "app:v2"), ("region", "us-east")],
            "bump image",
        )
        .await
        .unwrap();
        assert_ne!(sha1, sha2);

        // Read back and verify merge
        let content = read_file_at_ref(&repo_path, "main", "values.yaml")
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_yaml::from_str(&content).unwrap();
        assert_eq!(parsed["image"], "app:v2");
        assert_eq!(parsed["replicas"], "3");
        assert_eq!(parsed["region"], "us-east");

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }
}
