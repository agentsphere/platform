// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Integration tests for `CliGitRepoManager::init_bare` — filesystem-only, no DB.

use platform_git::plumbing::CliGitRepoManager;
use platform_git::traits::GitRepoManager;

#[tokio::test]
async fn init_bare_repo_creates_directory() {
    let tmp = std::env::temp_dir().join(format!("platform-test-{}", uuid::Uuid::new_v4()));
    let mgr = CliGitRepoManager;
    let path = mgr
        .init_bare(&tmp, "alice", "my-project", "main")
        .await
        .unwrap();

    assert!(path.exists());
    assert!(path.join("HEAD").exists());

    let head = tokio::fs::read_to_string(path.join("HEAD")).await.unwrap();
    assert_eq!(head, "ref: refs/heads/main\n");

    // Verify initial commit exists on main
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&path)
        .args(["rev-parse", "refs/heads/main"])
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "main branch should have a commit");

    // Verify template files are in the tree
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&path)
        .args(["ls-tree", "--name-only", "main"])
        .output()
        .await
        .unwrap();
    let files = String::from_utf8(output.stdout).unwrap();
    assert!(files.contains(".platform.yaml"));
    assert!(files.contains("Dockerfile"));
    assert!(files.contains("CLAUDE.md"));
    assert!(files.contains("README.md"));
    assert!(files.contains("deploy"));

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}

#[tokio::test]
async fn init_bare_repo_custom_branch() {
    let tmp = std::env::temp_dir().join(format!("platform-test-{}", uuid::Uuid::new_v4()));
    let mgr = CliGitRepoManager;
    let path = mgr.init_bare(&tmp, "bob", "repo", "develop").await.unwrap();

    let head = tokio::fs::read_to_string(path.join("HEAD")).await.unwrap();
    assert_eq!(head, "ref: refs/heads/develop\n");

    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&path)
        .args(["rev-parse", "refs/heads/develop"])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "develop branch should have a commit"
    );

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}

#[tokio::test]
async fn init_bare_repo_readme_contains_project_name() {
    let tmp = std::env::temp_dir().join(format!("platform-test-{}", uuid::Uuid::new_v4()));
    let mgr = CliGitRepoManager;
    let path = mgr
        .init_bare(&tmp, "alice", "my-app", "main")
        .await
        .unwrap();

    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&path)
        .args(["show", "main:README.md"])
        .output()
        .await
        .unwrap();
    let content = String::from_utf8(output.stdout).unwrap();
    assert!(
        content.contains("my-app"),
        "README should contain project name"
    );

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}

#[tokio::test]
async fn init_bare_repo_has_nested_deploy_directory() {
    let tmp = std::env::temp_dir().join(format!("platform-test-{}", uuid::Uuid::new_v4()));
    let mgr = CliGitRepoManager;
    let path = mgr
        .init_bare(&tmp, "alice", "my-app", "main")
        .await
        .unwrap();

    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&path)
        .args(["show", "main:deploy/production.yaml"])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "deploy/production.yaml should exist"
    );
    let content = String::from_utf8(output.stdout).unwrap();
    assert!(
        content.contains("kind: Deployment"),
        "should contain K8s Deployment"
    );

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}

#[tokio::test]
async fn init_bare_repo_has_deeply_nested_claude_commands() {
    let tmp = std::env::temp_dir().join(format!("platform-test-{}", uuid::Uuid::new_v4()));
    let mgr = CliGitRepoManager;
    let path = mgr
        .init_bare(&tmp, "alice", "my-app", "main")
        .await
        .unwrap();

    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&path)
        .args(["show", "main:.claude/commands/dev.md"])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        ".claude/commands/dev.md should exist in initial commit"
    );
    let content = String::from_utf8(output.stdout).unwrap();
    assert!(
        content.contains("$ARGUMENTS"),
        "dev command should contain $ARGUMENTS placeholder"
    );

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}

#[tokio::test]
async fn init_bare_repo_commit_message() {
    let tmp = std::env::temp_dir().join(format!("platform-test-{}", uuid::Uuid::new_v4()));
    let mgr = CliGitRepoManager;
    let path = mgr
        .init_bare(&tmp, "alice", "my-app", "main")
        .await
        .unwrap();

    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&path)
        .args(["log", "--format=%s", "-1", "main"])
        .output()
        .await
        .unwrap();
    let message = String::from_utf8(output.stdout).unwrap();
    assert_eq!(message.trim(), "Initial commit: platform template");

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}
