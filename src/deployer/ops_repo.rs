use std::path::{Path, PathBuf};

use sqlx::PgPool;
use uuid::Uuid;

use super::error::DeployerError;
use crate::store::valkey;

/// Sync a single ops repo: clone if missing, pull if exists.
/// Returns the current HEAD SHA. Respects `sync_interval_s` via Valkey cache.
#[tracing::instrument(skip(pool, valkey_pool), fields(%ops_repo_id), err)]
pub async fn sync_repo(
    pool: &PgPool,
    valkey_pool: &fred::clients::Pool,
    repos_dir: &Path,
    ops_repo_id: Uuid,
) -> Result<String, DeployerError> {
    let repo = sqlx::query!(
        "SELECT name, repo_url, branch, path, sync_interval_s FROM ops_repos WHERE id = $1",
        ops_repo_id,
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| DeployerError::OpsRepoNotFound(ops_repo_id.to_string()))?;

    // Check sync freshness cache
    let cache_key = format!("ops_repo_sync:{ops_repo_id}");
    if let Some(sha) = valkey::get_cached::<String>(valkey_pool, &cache_key).await {
        return Ok(sha);
    }

    // SSRF validate the repo URL
    crate::validation::check_ssrf_url(&repo.repo_url, &["http", "https"])
        .map_err(|e| DeployerError::SyncFailed(e.to_string()))?;

    let local_path = repos_dir.join(&repo.name);

    if local_path.exists() {
        pull_repo(&local_path, &repo.branch).await?;
    } else {
        clone_repo(&repo.repo_url, &repo.branch, &local_path).await?;
    }

    let sha = get_head_sha(&local_path).await?;

    // Cache the SHA with TTL = sync_interval_s
    let _ = valkey::set_cached(
        valkey_pool,
        &cache_key,
        &sha,
        i64::from(repo.sync_interval_s),
    )
    .await;

    Ok(sha)
}

/// Force-sync an ops repo (ignores cache).
#[tracing::instrument(skip(pool, valkey_pool), fields(%ops_repo_id), err)]
pub async fn force_sync(
    pool: &PgPool,
    valkey_pool: &fred::clients::Pool,
    repos_dir: &Path,
    ops_repo_id: Uuid,
) -> Result<String, DeployerError> {
    // Invalidate cache first
    let cache_key = format!("ops_repo_sync:{ops_repo_id}");
    let _ = valkey::invalidate(valkey_pool, &cache_key).await;

    sync_repo(pool, valkey_pool, repos_dir, ops_repo_id).await
}

/// Resolve the full filesystem path to a manifest file within an ops repo.
/// Guards against path traversal by ensuring the result stays within the repo directory.
pub fn resolve_manifest_path(
    repos_dir: &Path,
    ops_repo_name: &str,
    ops_repo_subpath: &str,
    manifest_path: &str,
) -> Result<PathBuf, DeployerError> {
    // Reject obvious traversal attempts
    if manifest_path.contains("..") || ops_repo_subpath.contains("..") {
        return Err(DeployerError::InvalidManifest(
            "path traversal detected".into(),
        ));
    }

    let repo_root = repos_dir.join(ops_repo_name);
    let full_path = repo_root
        .join(ops_repo_subpath.trim_matches('/'))
        .join(manifest_path);

    // Verify the resolved path stays within the repo root
    if !full_path.starts_with(&repo_root) {
        return Err(DeployerError::InvalidManifest(
            "path traversal detected".into(),
        ));
    }

    Ok(full_path)
}

async fn clone_repo(url: &str, branch: &str, dest: &Path) -> Result<(), DeployerError> {
    let output = tokio::process::Command::new("git")
        .args(["clone", "--depth", "1", "--branch", branch, url])
        .arg(dest)
        .output()
        .await
        .map_err(|e| DeployerError::SyncFailed(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DeployerError::SyncFailed(format!(
            "git clone failed: {stderr}"
        )));
    }

    Ok(())
}

async fn pull_repo(repo_dir: &Path, branch: &str) -> Result<(), DeployerError> {
    let fetch = tokio::process::Command::new("git")
        .args(["-C"])
        .arg(repo_dir)
        .args(["fetch", "origin", branch])
        .output()
        .await
        .map_err(|e| DeployerError::SyncFailed(e.to_string()))?;

    if !fetch.status.success() {
        let stderr = String::from_utf8_lossy(&fetch.stderr);
        return Err(DeployerError::SyncFailed(format!(
            "git fetch failed: {stderr}"
        )));
    }

    let reset = tokio::process::Command::new("git")
        .args(["-C"])
        .arg(repo_dir)
        .args(["reset", "--hard", &format!("origin/{branch}")])
        .output()
        .await
        .map_err(|e| DeployerError::SyncFailed(e.to_string()))?;

    if !reset.status.success() {
        let stderr = String::from_utf8_lossy(&reset.stderr);
        return Err(DeployerError::SyncFailed(format!(
            "git reset failed: {stderr}"
        )));
    }

    Ok(())
}

async fn get_head_sha(repo_dir: &Path) -> Result<String, DeployerError> {
    let output = tokio::process::Command::new("git")
        .args(["-C"])
        .arg(repo_dir)
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
}
