use std::path::{Path, PathBuf};

use anyhow::Context;

/// Initialize a new bare git repository on disk.
/// Returns the full path to the created repo directory.
///
/// Called by the projects API (Phase 04) when creating a project.
/// Does NOT update the `projects` table — that is the caller's responsibility.
#[allow(dead_code)] // consumed by Phase 04 project creation
#[tracing::instrument(skip(repos_path), fields(%owner, %name, %default_branch), err)]
pub async fn init_bare_repo(
    repos_path: &Path,
    owner: &str,
    name: &str,
    default_branch: &str,
) -> anyhow::Result<PathBuf> {
    let repo_dir = repos_path.join(owner).join(format!("{name}.git"));

    // Create parent directories
    tokio::fs::create_dir_all(&repo_dir)
        .await
        .context("failed to create repo directory")?;

    // git init --bare
    let output = tokio::process::Command::new("git")
        .arg("init")
        .arg("--bare")
        .arg(&repo_dir)
        .output()
        .await
        .context("failed to run git init")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git init failed: {stderr}");
    }

    // Set default branch by writing HEAD
    let head_ref = format!("ref: refs/heads/{default_branch}\n");
    tokio::fs::write(repo_dir.join("HEAD"), head_ref)
        .await
        .context("failed to set HEAD")?;

    tracing::info!(path = %repo_dir.display(), "bare repository initialized");
    Ok(repo_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn init_bare_repo_creates_directory() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", uuid::Uuid::new_v4()));
        let path = init_bare_repo(&tmp, "alice", "my-project", "main")
            .await
            .unwrap();

        assert!(path.exists());
        assert!(path.join("HEAD").exists());

        let head = tokio::fs::read_to_string(path.join("HEAD")).await.unwrap();
        assert_eq!(head, "ref: refs/heads/main\n");

        // Cleanup
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn init_bare_repo_custom_branch() {
        let tmp = std::env::temp_dir().join(format!("platform-test-{}", uuid::Uuid::new_v4()));
        let path = init_bare_repo(&tmp, "bob", "repo", "develop")
            .await
            .unwrap();

        let head = tokio::fs::read_to_string(path.join("HEAD")).await.unwrap();
        assert_eq!(head, "ref: refs/heads/develop\n");

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }
}
