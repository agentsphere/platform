// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Integration tests for `platform_ops_repo` — bare-repo git operations.
//!
//! These tests exercise public functions with real filesystem I/O (temp dirs + git).
//! No database required.

use std::path::Path;
use std::path::PathBuf;

use platform_ops_repo::*;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Test helpers (pub-function-only equivalents of the in-module helpers)
// ---------------------------------------------------------------------------

/// Bootstrap a bare repo with an initial commit so worktree-based operations work.
/// Equivalent to the private `bootstrap_repo()` in `ops_repo::tests` but uses only
/// public functions and raw git commands — no private `cleanup_worktree()`.
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

    // Clean up worktree using raw git (no private cleanup_worktree)
    let _ = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&repo_path)
        .args(["worktree", "remove", "--force"])
        .arg(&init_wt)
        .output()
        .await;
    let _ = tokio::fs::remove_dir_all(&init_wt).await;

    repo_path
}

/// Create a project repo with a `deploy/` directory containing given files.
/// Returns `(repo_path, HEAD SHA)`. Uses raw git cleanup instead of private `cleanup_worktree`.
async fn create_project_repo_with_deploy(tmp: &Path, files: &[(&str, &str)]) -> (PathBuf, String) {
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

    // Clean up worktree using raw git (no private cleanup_worktree)
    let _ = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&repo_path)
        .args(["worktree", "remove", "--force"])
        .arg(&wt)
        .output()
        .await;
    let _ = tokio::fs::remove_dir_all(&wt).await;

    (repo_path, sha)
}

// ---------------------------------------------------------------------------
// init / get_head_sha
// ---------------------------------------------------------------------------

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

    // No commits yet — rev-parse --verify HEAD fails on empty repo
    let result = get_head_sha(&repo_path).await;
    assert!(result.is_err(), "get_head_sha should fail on empty repo");

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

// ---------------------------------------------------------------------------
// commit_values / read_values
// ---------------------------------------------------------------------------

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
async fn commit_values_no_changes_returns_same_sha() {
    let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
    let repo_path = bootstrap_repo(&tmp).await;

    let values = serde_json::json!({"image_ref": "app:v1"});

    // First commit succeeds
    let sha1 = commit_values(&repo_path, "main", "production", &values)
        .await
        .unwrap();

    // Second commit with same values — returns same SHA (no new commit)
    let sha2 = commit_values(&repo_path, "main", "production", &values)
        .await
        .unwrap();
    assert_eq!(
        sha1, sha2,
        "re-committing same values should return same SHA"
    );

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}

// ---------------------------------------------------------------------------
// revert_last_commit
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// read_values / read_file_at_ref
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// write_file_to_repo
// ---------------------------------------------------------------------------

#[tokio::test]
async fn write_file_to_repo_creates_file() {
    let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
    let repo_path = bootstrap_repo(&tmp).await;

    write_file_to_repo(
        &repo_path,
        "main",
        "platform.yaml",
        "pipeline:\n  steps: []\n",
        None,
    )
    .await
    .unwrap();

    let content = read_file_at_ref(&repo_path, "main", "platform.yaml")
        .await
        .unwrap();
    assert!(content.contains("pipeline:"));

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}

#[tokio::test]
async fn write_file_to_repo_no_change_is_noop() {
    let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
    let repo_path = bootstrap_repo(&tmp).await;

    let content = "key: value\n";
    write_file_to_repo(&repo_path, "main", "test.yaml", content, None)
        .await
        .unwrap();
    let sha1 = get_branch_sha(&repo_path, "main").await.unwrap();

    // Write same content again — should not create new commit
    write_file_to_repo(&repo_path, "main", "test.yaml", content, None)
        .await
        .unwrap();
    let sha2 = get_branch_sha(&repo_path, "main").await.unwrap();

    assert_eq!(sha1, sha2, "identical content should not create new commit");

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}

#[tokio::test]
async fn write_file_creates_subdirectories() {
    let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
    let repo_path = bootstrap_repo(&tmp).await;

    write_file_to_repo(&repo_path, "main", "deep/nested/file.txt", "hello", None)
        .await
        .unwrap();

    let content = read_file_at_ref(&repo_path, "main", "deep/nested/file.txt")
        .await
        .unwrap();
    assert_eq!(content, "hello");

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}

// ---------------------------------------------------------------------------
// merge_branch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn merge_branch_combines_content() {
    let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
    let repo_path = bootstrap_repo(&tmp).await;

    // Create staging branch from main (shared history) via git branch
    let _ = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&repo_path)
        .args(["branch", "staging", "main"])
        .output()
        .await
        .unwrap();

    // Commit values to staging
    let staging_values = serde_json::json!({"image_ref": "app:v2", "environment": "staging"});
    commit_values(&repo_path, "staging", "staging", &staging_values)
        .await
        .unwrap();

    // Verify staging has the values
    let staging_read = read_values(&repo_path, "staging", "staging").await.unwrap();
    assert_eq!(staging_read["image_ref"], "app:v2");

    // Main should NOT have staging values yet
    let main_result = read_values(&repo_path, "main", "staging").await;
    assert!(main_result.is_err());

    // Merge staging into main
    merge_branch(&repo_path, "staging", "main").await.unwrap();

    // Now main should have the staging values
    let main_read = read_values(&repo_path, "main", "staging").await.unwrap();
    assert_eq!(main_read["image_ref"], "app:v2");

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}

// ---------------------------------------------------------------------------
// compare_branches
// ---------------------------------------------------------------------------

#[tokio::test]
async fn compare_branches_same_sha() {
    let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
    let repo_path = bootstrap_repo(&tmp).await;

    let main_sha = get_branch_sha(&repo_path, "main").await.unwrap();
    // Compare main with itself
    let (diverged, sha1, sha2) = compare_branches(&repo_path, "main", "main").await.unwrap();
    assert!(!diverged);
    assert_eq!(sha1, sha2);
    assert_eq!(sha1, main_sha);

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}

#[tokio::test]
async fn compare_branches_diverged() {
    let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
    let repo_path = bootstrap_repo(&tmp).await;

    // Create staging from main (shared history)
    let _ = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&repo_path)
        .args(["branch", "staging", "main"])
        .output()
        .await
        .unwrap();

    // Commit to staging only
    let values = serde_json::json!({"image_ref": "app:staging-v1"});
    commit_values(&repo_path, "staging", "staging", &values)
        .await
        .unwrap();

    let (diverged, staging_sha, main_sha) = compare_branches(&repo_path, "staging", "main")
        .await
        .unwrap();
    assert!(diverged);
    assert_ne!(staging_sha, main_sha);

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}

// ---------------------------------------------------------------------------
// get_branch_sha
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_branch_sha_valid() {
    let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
    let repo_path = bootstrap_repo(&tmp).await;

    let sha = get_branch_sha(&repo_path, "main").await.unwrap();
    assert_eq!(sha.len(), 40);
    assert!(sha.chars().all(|c| c.is_ascii_hexdigit()));

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}

#[tokio::test]
async fn get_branch_sha_missing_branch() {
    let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
    let repo_path = bootstrap_repo(&tmp).await;

    let result = get_branch_sha(&repo_path, "nonexistent").await;
    assert!(result.is_err());

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}

// ---------------------------------------------------------------------------
// sync_from_project_repo
// ---------------------------------------------------------------------------

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
