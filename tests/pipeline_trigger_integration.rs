//! Integration tests for `pipeline::trigger` — on_push, on_api, on_mr.
//!
//! These tests exercise the public trigger functions with real Postgres and
//! real (temp) git repos. Private helpers (`read_file_at_ref`, `get_ref_sha`,
//! `create_pipeline_with_steps`) are tested indirectly through the public API.

mod helpers;

use std::path::PathBuf;
use std::process::Command;

use sqlx::PgPool;
use tempfile::TempDir;
use uuid::Uuid;

use platform::pipeline::trigger::{self, MrTriggerParams, PushTriggerParams};

// ---------------------------------------------------------------------------
// Test git repo helpers
// ---------------------------------------------------------------------------

/// Create a bare git repo + working copy with `.platform.yaml`, push to `main`.
/// Returns the bare repo dir handle (drop = cleanup) and the bare repo path.
fn create_test_repo_with_pipeline_yaml(yaml_content: &str) -> (TempDir, TempDir, PathBuf) {
    let bare_dir = TempDir::new().unwrap();
    let bare_path = bare_dir.path().to_path_buf();

    // Init bare repo
    let out = Command::new("git")
        .args(["init", "--bare"])
        .arg(&bare_path)
        .output()
        .unwrap();
    assert!(out.status.success(), "git init --bare failed: {:?}", out);

    // Create a working copy
    let work_dir = TempDir::new().unwrap();
    let work_path = work_dir.path();

    let out = Command::new("git")
        .args(["clone"])
        .arg(&bare_path)
        .arg(work_path)
        .output()
        .unwrap();
    assert!(out.status.success(), "git clone failed: {:?}", out);

    // Write .platform.yaml
    std::fs::write(work_path.join(".platform.yaml"), yaml_content).unwrap();

    // Add, commit, push
    let out = Command::new("git")
        .args(["-C"])
        .arg(work_path)
        .args(["add", "."])
        .output()
        .unwrap();
    assert!(out.status.success(), "git add failed: {:?}", out);

    let out = Command::new("git")
        .args(["-C"])
        .arg(work_path)
        .args([
            "-c",
            "user.email=test@test.com",
            "-c",
            "user.name=Test",
            "commit",
            "-m",
            "init",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "git commit failed: {:?}", out);

    let out = Command::new("git")
        .args(["-C"])
        .arg(work_path)
        .args(["push", "origin", "main"])
        .output()
        .unwrap();
    assert!(out.status.success(), "git push failed: {:?}", out);

    (bare_dir, work_dir, bare_path)
}

/// Create a project in the DB with a specific repo_path. Returns (project_id, owner_id).
async fn create_project_with_repo(pool: &PgPool, repo_path: &str) -> (Uuid, Uuid) {
    // Get the admin user id (created by bootstrap)
    let row: (Uuid,) = sqlx::query_as("SELECT id FROM users WHERE name = 'admin'")
        .fetch_one(pool)
        .await
        .unwrap();
    let owner_id = row.0;

    // Get admin's workspace
    let ws_row: (Uuid,) = sqlx::query_as(
        "SELECT id FROM workspaces WHERE owner_id = $1 AND is_active = true ORDER BY created_at LIMIT 1",
    )
    .bind(owner_id)
    .fetch_one(pool)
    .await
    .unwrap();
    let workspace_id = ws_row.0;

    let project_name = format!("test-project-{}", Uuid::new_v4());
    let namespace_slug = platform::deployer::namespace::slugify_namespace(&project_name);
    let row: (Uuid,) = sqlx::query_as(
        "INSERT INTO projects (owner_id, name, repo_path, visibility, workspace_id, namespace_slug) VALUES ($1, $2, $3, 'private', $4, $5) RETURNING id",
    )
    .bind(owner_id)
    .bind(&project_name)
    .bind(repo_path)
    .bind(workspace_id)
    .bind(&namespace_slug)
    .fetch_one(pool)
    .await
    .unwrap();

    (row.0, owner_id)
}

const SIMPLE_YAML: &str = "\
pipeline:
  steps:
    - name: test
      image: alpine:3.19
      commands:
        - echo hello
";

const BRANCH_FILTERED_YAML: &str = "\
pipeline:
  steps:
    - name: test
      image: alpine:3.19
      commands:
        - echo hello
  on:
    push:
      branches: [main]
    mr:
      actions: [opened]
";

const MULTI_STEP_YAML: &str = "\
pipeline:
  steps:
    - name: lint
      image: rust:1.85
      commands:
        - cargo clippy
    - name: test
      image: rust:1.85
      commands:
        - cargo nextest run
    - name: build
      image: rust:1.85
      commands:
        - cargo build --release
";

// ---------------------------------------------------------------------------
// on_push — happy path
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn on_push_creates_pipeline(pool: PgPool) {
    let _state = helpers::test_state(pool.clone()).await;
    let (bare_dir, _work_dir, bare_path) = create_test_repo_with_pipeline_yaml(SIMPLE_YAML);
    let (project_id, user_id) = create_project_with_repo(&pool, bare_path.to_str().unwrap()).await;

    let params = PushTriggerParams {
        project_id,
        user_id,
        repo_path: bare_path.clone(),
        branch: "main".into(),
        commit_sha: None,
    };

    let result = trigger::on_push(&pool, &params).await.unwrap();
    assert!(result.is_some(), "on_push should create a pipeline");

    let pipeline_id = result.unwrap();

    // Verify the pipeline row exists in DB
    let row: (String, String, String) =
        sqlx::query_as("SELECT trigger, git_ref, status FROM pipelines WHERE id = $1")
            .bind(pipeline_id)
            .fetch_one(&pool)
            .await
            .unwrap();

    assert_eq!(row.0, "push");
    assert_eq!(row.1, "refs/heads/main");
    assert_eq!(row.2, "pending");

    // Verify steps were created
    let step_count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM pipeline_steps WHERE pipeline_id = $1")
            .bind(pipeline_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(step_count.0, 1);

    drop(bare_dir);
    drop(_work_dir);
}

// ---------------------------------------------------------------------------
// on_push — no .platform.yaml returns None
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn on_push_no_yaml_returns_none(pool: PgPool) {
    let _state = helpers::test_state(pool.clone()).await;

    // Create a bare repo with no .platform.yaml
    let bare_dir = TempDir::new().unwrap();
    let bare_path = bare_dir.path().to_path_buf();
    Command::new("git")
        .args(["init", "--bare"])
        .arg(&bare_path)
        .output()
        .unwrap();

    let work_dir = TempDir::new().unwrap();
    let work_path = work_dir.path();
    Command::new("git")
        .args(["clone"])
        .arg(&bare_path)
        .arg(work_path)
        .output()
        .unwrap();

    std::fs::write(work_path.join("README.md"), "hello").unwrap();
    Command::new("git")
        .args(["-C"])
        .arg(work_path)
        .args(["add", "."])
        .output()
        .unwrap();
    Command::new("git")
        .args(["-C"])
        .arg(work_path)
        .args([
            "-c",
            "user.email=test@test.com",
            "-c",
            "user.name=Test",
            "commit",
            "-m",
            "init",
        ])
        .output()
        .unwrap();
    Command::new("git")
        .args(["-C"])
        .arg(work_path)
        .args(["push", "origin", "main"])
        .output()
        .unwrap();

    let (project_id, user_id) = create_project_with_repo(&pool, bare_path.to_str().unwrap()).await;

    let params = PushTriggerParams {
        project_id,
        user_id,
        repo_path: bare_path,
        branch: "main".into(),
        commit_sha: None,
    };

    let result = trigger::on_push(&pool, &params).await.unwrap();
    assert!(
        result.is_none(),
        "on_push without .platform.yaml should return None"
    );
}

// ---------------------------------------------------------------------------
// on_push — branch filter mismatch returns None
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn on_push_branch_mismatch_returns_none(pool: PgPool) {
    let _state = helpers::test_state(pool.clone()).await;
    let (bare_dir, _work_dir, bare_path) =
        create_test_repo_with_pipeline_yaml(BRANCH_FILTERED_YAML);
    let (project_id, user_id) = create_project_with_repo(&pool, bare_path.to_str().unwrap()).await;

    // Push to "develop" but the YAML only triggers on "main"
    // We need a "develop" branch in the repo with the YAML for read_file_at_ref to work
    let work_path = _work_dir.path();
    Command::new("git")
        .args(["-C"])
        .arg(work_path)
        .args(["checkout", "-b", "develop"])
        .output()
        .unwrap();
    Command::new("git")
        .args(["-C"])
        .arg(work_path)
        .args(["push", "origin", "develop"])
        .output()
        .unwrap();

    let params = PushTriggerParams {
        project_id,
        user_id,
        repo_path: bare_path.clone(),
        branch: "develop".into(),
        commit_sha: None,
    };

    let result = trigger::on_push(&pool, &params).await.unwrap();
    assert!(
        result.is_none(),
        "on_push for non-matching branch should return None"
    );

    drop(bare_dir);
    drop(_work_dir);
}

// ---------------------------------------------------------------------------
// on_push — non-existent repo path returns None (no .platform.yaml)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn on_push_nonexistent_repo_returns_none(pool: PgPool) {
    let _state = helpers::test_state(pool.clone()).await;
    let (project_id, user_id) =
        create_project_with_repo(&pool, "/tmp/nonexistent-repo-12345").await;

    let params = PushTriggerParams {
        project_id,
        user_id,
        repo_path: PathBuf::from("/tmp/nonexistent-repo-12345"),
        branch: "main".into(),
        commit_sha: None,
    };

    // read_file_at_ref will fail → returns None
    let result = trigger::on_push(&pool, &params).await.unwrap();
    assert!(result.is_none());
}

// ---------------------------------------------------------------------------
// on_push — multi-step pipeline creates all steps
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn on_push_multi_step_creates_all_steps(pool: PgPool) {
    let _state = helpers::test_state(pool.clone()).await;
    let (bare_dir, _work_dir, bare_path) = create_test_repo_with_pipeline_yaml(MULTI_STEP_YAML);
    let (project_id, user_id) = create_project_with_repo(&pool, bare_path.to_str().unwrap()).await;

    let params = PushTriggerParams {
        project_id,
        user_id,
        repo_path: bare_path.clone(),
        branch: "main".into(),
        commit_sha: Some("abc123".into()),
    };

    let result = trigger::on_push(&pool, &params).await.unwrap();
    let pipeline_id = result.unwrap();

    // Should have 3 steps
    let step_count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM pipeline_steps WHERE pipeline_id = $1")
            .bind(pipeline_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(step_count.0, 3);

    // Verify step ordering
    let steps: Vec<(String, i32)> = sqlx::query_as(
        "SELECT name, step_order FROM pipeline_steps WHERE pipeline_id = $1 ORDER BY step_order",
    )
    .bind(pipeline_id)
    .fetch_all(&pool)
    .await
    .unwrap();

    assert_eq!(steps[0].0, "lint");
    assert_eq!(steps[0].1, 0);
    assert_eq!(steps[1].0, "test");
    assert_eq!(steps[1].1, 1);
    assert_eq!(steps[2].0, "build");
    assert_eq!(steps[2].1, 2);

    // Verify commit_sha was stored
    let row: (Option<String>,) = sqlx::query_as("SELECT commit_sha FROM pipelines WHERE id = $1")
        .bind(pipeline_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.0.as_deref(), Some("abc123"));

    drop(bare_dir);
    drop(_work_dir);
}

// ---------------------------------------------------------------------------
// on_api — happy path
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn on_api_creates_pipeline(pool: PgPool) {
    let _state = helpers::test_state(pool.clone()).await;
    let (bare_dir, _work_dir, bare_path) = create_test_repo_with_pipeline_yaml(SIMPLE_YAML);
    let (project_id, user_id) = create_project_with_repo(&pool, bare_path.to_str().unwrap()).await;

    let pipeline_id = trigger::on_api(&pool, &bare_path, project_id, "refs/heads/main", user_id)
        .await
        .unwrap();

    // Verify the pipeline row
    let row: (String, String, String) =
        sqlx::query_as("SELECT trigger, git_ref, status FROM pipelines WHERE id = $1")
            .bind(pipeline_id)
            .fetch_one(&pool)
            .await
            .unwrap();

    assert_eq!(row.0, "api");
    assert_eq!(row.1, "refs/heads/main");
    assert_eq!(row.2, "pending");

    // on_api also resolves the commit SHA from the ref
    let sha_row: (Option<String>,) =
        sqlx::query_as("SELECT commit_sha FROM pipelines WHERE id = $1")
            .bind(pipeline_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    // The commit SHA should be resolved from the git repo
    assert!(
        sha_row.0.is_some(),
        "commit_sha should be resolved from git ref"
    );
    assert_eq!(sha_row.0.unwrap().len(), 40, "SHA should be 40 hex chars");

    drop(bare_dir);
    drop(_work_dir);
}

// ---------------------------------------------------------------------------
// on_api — bare branch name (without refs/heads/) also works
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn on_api_bare_branch_name(pool: PgPool) {
    let _state = helpers::test_state(pool.clone()).await;
    let (bare_dir, _work_dir, bare_path) = create_test_repo_with_pipeline_yaml(SIMPLE_YAML);
    let (project_id, user_id) = create_project_with_repo(&pool, bare_path.to_str().unwrap()).await;

    // Pass "main" instead of "refs/heads/main" — on_api strips the prefix for read_file_at_ref
    let pipeline_id = trigger::on_api(&pool, &bare_path, project_id, "main", user_id)
        .await
        .unwrap();

    let row: (String,) = sqlx::query_as("SELECT git_ref FROM pipelines WHERE id = $1")
        .bind(pipeline_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.0, "main");

    drop(bare_dir);
    drop(_work_dir);
}

// ---------------------------------------------------------------------------
// on_api — no .platform.yaml returns error
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn on_api_no_yaml_returns_error(pool: PgPool) {
    let _state = helpers::test_state(pool.clone()).await;

    let result = trigger::on_api(
        &pool,
        std::path::Path::new("/tmp/nonexistent-repo-67890"),
        Uuid::new_v4(), // project doesn't need to exist for this error path
        "refs/heads/main",
        Uuid::new_v4(),
    )
    .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains(".platform.yaml"),
        "error should mention missing .platform.yaml, got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// on_mr — happy path
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn on_mr_creates_pipeline(pool: PgPool) {
    let _state = helpers::test_state(pool.clone()).await;
    let (bare_dir, _work_dir, bare_path) =
        create_test_repo_with_pipeline_yaml(BRANCH_FILTERED_YAML);
    let (project_id, user_id) = create_project_with_repo(&pool, bare_path.to_str().unwrap()).await;

    let params = MrTriggerParams {
        project_id,
        user_id,
        repo_path: bare_path.clone(),
        source_branch: "main".into(),
        commit_sha: Some("deadbeef".repeat(5)),
        action: "opened".into(),
    };

    let result = trigger::on_mr(&pool, &params).await.unwrap();
    assert!(
        result.is_some(),
        "on_mr should create a pipeline for matching action"
    );

    let pipeline_id = result.unwrap();

    let row: (String,) = sqlx::query_as("SELECT trigger FROM pipelines WHERE id = $1")
        .bind(pipeline_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.0, "mr");

    drop(bare_dir);
    drop(_work_dir);
}

// ---------------------------------------------------------------------------
// on_mr — action mismatch returns None
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn on_mr_action_mismatch_returns_none(pool: PgPool) {
    let _state = helpers::test_state(pool.clone()).await;
    let (bare_dir, _work_dir, bare_path) =
        create_test_repo_with_pipeline_yaml(BRANCH_FILTERED_YAML);
    let (project_id, user_id) = create_project_with_repo(&pool, bare_path.to_str().unwrap()).await;

    // YAML only allows "opened", not "closed"
    let params = MrTriggerParams {
        project_id,
        user_id,
        repo_path: bare_path.clone(),
        source_branch: "main".into(),
        commit_sha: None,
        action: "closed".into(),
    };

    let result = trigger::on_mr(&pool, &params).await.unwrap();
    assert!(
        result.is_none(),
        "on_mr for non-matching action should return None"
    );

    drop(bare_dir);
    drop(_work_dir);
}

// ---------------------------------------------------------------------------
// on_push — invalid YAML in .platform.yaml returns error
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn on_push_invalid_yaml_returns_error(pool: PgPool) {
    let _state = helpers::test_state(pool.clone()).await;

    let invalid_yaml = "this is not valid yaml: [[[";
    let (bare_dir, _work_dir, bare_path) = create_test_repo_with_pipeline_yaml(invalid_yaml);
    let (project_id, user_id) = create_project_with_repo(&pool, bare_path.to_str().unwrap()).await;

    let params = PushTriggerParams {
        project_id,
        user_id,
        repo_path: bare_path.clone(),
        branch: "main".into(),
        commit_sha: None,
    };

    let result = trigger::on_push(&pool, &params).await;
    assert!(result.is_err(), "invalid YAML should produce an error");

    drop(bare_dir);
    drop(_work_dir);
}

// ---------------------------------------------------------------------------
// notify_executor — smoke test
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn notify_executor_does_not_panic(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let pipeline_id = Uuid::new_v4();

    // Should not panic even though no executor is listening
    trigger::notify_executor(&state, pipeline_id).await;
}
