// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Tier 2 — DB integration tests for `platform-pipeline`.
//!
//! These tests exercise trigger functions, pipeline cancellation, and the
//! executor loop against real Postgres + Valkey + `MinIO` (in-memory).
//! No K8s interaction — the executor can claim pipelines but will fail
//! at K8s steps, which we use to test the failure/cleanup path.

mod helpers;

use sqlx::PgPool;
use uuid::Uuid;

use fred::interfaces::{ClientLike, EventInterface, PubsubInterface};
use platform_pipeline::trigger;

// ---------------------------------------------------------------------------
// Shared git helper (placed before tests to satisfy `items_after_statements`)
// ---------------------------------------------------------------------------

async fn run_git(dir: &std::path::Path, args: &[&str]) {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .expect("git command failed to execute");
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("fatal"),
            "git {args:?} failed in {}: {stderr}",
            dir.display(),
        );
    }
}

// ===========================================================================
// Trigger: `on_push`
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn on_push_creates_pipeline_with_steps(pool: PgPool) {
    let owner_id = helpers::seed_user(&pool, "push-owner").await;
    let ws_id = helpers::seed_workspace(&pool, owner_id).await;
    let (project_id, _name) = helpers::seed_project(&pool, owner_id, ws_id).await;

    let (bare, _work) =
        helpers::create_repo_with_platform_yaml(helpers::MINIMAL_PLATFORM_YAML).await;

    let params = trigger::PushTriggerParams {
        project_id,
        user_id: owner_id,
        repo_path: bare.clone(),
        branch: "main".into(),
        commit_sha: None,
    };
    let result = trigger::on_push(&pool, &params, "kaniko:latest")
        .await
        .expect("on_push should succeed");

    let pipeline_id = result.expect("should have created a pipeline");

    // Verify pipeline row
    let status: String = sqlx::query_scalar("SELECT status FROM pipelines WHERE id = $1")
        .bind(pipeline_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "pending");

    // Verify at least one step was created
    let step_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pipeline_steps WHERE pipeline_id = $1")
            .bind(pipeline_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        step_count >= 1,
        "expected at least 1 step, got {step_count}"
    );

    // Verify step name matches YAML
    let step_name: String = sqlx::query_scalar(
        "SELECT name FROM pipeline_steps WHERE pipeline_id = $1 ORDER BY step_order LIMIT 1",
    )
    .bind(pipeline_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(step_name, "test-step");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn on_push_no_yaml_returns_none(pool: PgPool) {
    let owner_id = helpers::seed_user(&pool, "no-yaml-owner").await;
    let ws_id = helpers::seed_workspace(&pool, owner_id).await;
    let (project_id, _name) = helpers::seed_project(&pool, owner_id, ws_id).await;

    // Create a repo WITHOUT `.platform.yaml` (just a README)
    let base = std::env::temp_dir().join(format!("pl-test-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&base).unwrap();
    let bare_path = base.join("bare.git");
    let work_path = base.join("work");
    run_git(&bare_path, &["init", "--bare", bare_path.to_str().unwrap()]).await;
    run_git(
        &work_path,
        &[
            "clone",
            bare_path.to_str().unwrap(),
            work_path.to_str().unwrap(),
        ],
    )
    .await;
    run_git(&work_path, &["config", "user.email", "test@test.local"]).await;
    run_git(&work_path, &["config", "user.name", "test"]).await;
    std::fs::write(work_path.join("README.md"), "# hello").unwrap();
    run_git(&work_path, &["add", "."]).await;
    run_git(&work_path, &["commit", "-m", "initial"]).await;
    run_git(&work_path, &["push", "origin", "main"]).await;

    let params = trigger::PushTriggerParams {
        project_id,
        user_id: owner_id,
        repo_path: bare_path,
        branch: "main".into(),
        commit_sha: None,
    };
    let result = trigger::on_push(&pool, &params, "kaniko:latest")
        .await
        .expect("on_push should succeed");
    assert!(result.is_none(), "no .platform.yaml should return None");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn on_push_branch_filter_skips_non_matching(pool: PgPool) {
    let owner_id = helpers::seed_user(&pool, "branch-filter").await;
    let ws_id = helpers::seed_workspace(&pool, owner_id).await;
    let (project_id, _name) = helpers::seed_project(&pool, owner_id, ws_id).await;

    let yaml = r"
pipeline:
  trigger:
    push:
      branches:
        - release/*
  steps:
    - name: build
      image: alpine:3.19
      commands:
        - echo build
";
    let (bare, _work) = helpers::create_repo_with_platform_yaml(yaml).await;

    let params = trigger::PushTriggerParams {
        project_id,
        user_id: owner_id,
        repo_path: bare,
        branch: "main".into(),
        commit_sha: None,
    };
    let result = trigger::on_push(&pool, &params, "kaniko:latest")
        .await
        .expect("on_push should succeed");
    assert!(
        result.is_none(),
        "main branch should not match release/* filter"
    );
}

// ===========================================================================
// Trigger: `on_mr`
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn on_mr_creates_pipeline(pool: PgPool) {
    let owner_id = helpers::seed_user(&pool, "mr-owner").await;
    let ws_id = helpers::seed_workspace(&pool, owner_id).await;
    let (project_id, _name) = helpers::seed_project(&pool, owner_id, ws_id).await;

    let (bare, work) =
        helpers::create_repo_with_platform_yaml(helpers::MINIMAL_PLATFORM_YAML).await;

    // Create a feature branch
    run_git(&work, &["checkout", "-b", "feature/test"]).await;
    std::fs::write(work.join("file.txt"), "change").unwrap();
    run_git(&work, &["add", "."]).await;
    run_git(&work, &["commit", "-m", "feature commit"]).await;
    run_git(&work, &["push", "origin", "feature/test"]).await;

    let params = trigger::MrTriggerParams {
        project_id,
        user_id: owner_id,
        repo_path: bare,
        source_branch: "feature/test".into(),
        commit_sha: None,
        action: "opened".into(),
    };
    let result = trigger::on_mr(&pool, &params, "kaniko:latest")
        .await
        .expect("on_mr should succeed");

    let pipeline_id = result.expect("should have created a pipeline");
    let trigger_type: String = sqlx::query_scalar("SELECT trigger FROM pipelines WHERE id = $1")
        .bind(pipeline_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(trigger_type, "mr");
}

// ===========================================================================
// Trigger: `on_tag`
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn on_tag_creates_pipeline(pool: PgPool) {
    let owner_id = helpers::seed_user(&pool, "tag-owner").await;
    let ws_id = helpers::seed_workspace(&pool, owner_id).await;
    let (project_id, _name) = helpers::seed_project(&pool, owner_id, ws_id).await;

    let yaml = r"
pipeline:
  trigger:
    tag:
      pattern: v*
  steps:
    - name: release
      image: alpine:3.19
      commands:
        - echo release
";
    let (bare, work) = helpers::create_repo_with_platform_yaml(yaml).await;

    // Create a tag
    run_git(&work, &["tag", "v1.0.0"]).await;
    run_git(&work, &["push", "origin", "v1.0.0"]).await;

    let params = trigger::TagTriggerParams {
        project_id,
        user_id: owner_id,
        repo_path: bare,
        tag_name: "v1.0.0".into(),
        commit_sha: None,
    };
    let result = trigger::on_tag(&pool, &params, "kaniko:latest")
        .await
        .expect("on_tag should succeed");

    let pipeline_id = result.expect("should have created a pipeline");
    let trigger_type: String = sqlx::query_scalar("SELECT trigger FROM pipelines WHERE id = $1")
        .bind(pipeline_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(trigger_type, "tag");
}

// ===========================================================================
// Trigger: `on_api`
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn on_api_creates_pipeline(pool: PgPool) {
    let owner_id = helpers::seed_user(&pool, "api-owner").await;
    let ws_id = helpers::seed_workspace(&pool, owner_id).await;
    let (project_id, _name) = helpers::seed_project(&pool, owner_id, ws_id).await;

    let (bare, _work) =
        helpers::create_repo_with_platform_yaml(helpers::MINIMAL_PLATFORM_YAML).await;

    let pipeline_id = trigger::on_api(
        &pool,
        &bare,
        project_id,
        "refs/heads/main",
        owner_id,
        "kaniko:latest",
    )
    .await
    .expect("on_api should succeed");

    let trigger_type: String = sqlx::query_scalar("SELECT trigger FROM pipelines WHERE id = $1")
        .bind(pipeline_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(trigger_type, "api");

    let step_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pipeline_steps WHERE pipeline_id = $1")
            .bind(pipeline_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(step_count >= 1);
}

#[sqlx::test(migrations = "../../../migrations")]
async fn on_api_no_yaml_errors(pool: PgPool) {
    let owner_id = helpers::seed_user(&pool, "api-noyaml").await;
    let ws_id = helpers::seed_workspace(&pool, owner_id).await;
    let (project_id, _name) = helpers::seed_project(&pool, owner_id, ws_id).await;

    // Create an empty repo with no `.platform.yaml`
    let base = std::env::temp_dir().join(format!("pl-test-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&base).unwrap();
    let bare_path = base.join("bare.git");
    let work_path = base.join("work");
    run_git(&bare_path, &["init", "--bare", bare_path.to_str().unwrap()]).await;
    run_git(
        &work_path,
        &[
            "clone",
            bare_path.to_str().unwrap(),
            work_path.to_str().unwrap(),
        ],
    )
    .await;
    run_git(&work_path, &["config", "user.email", "test@test.local"]).await;
    run_git(&work_path, &["config", "user.name", "test"]).await;
    std::fs::write(work_path.join("README.md"), "# hello").unwrap();
    run_git(&work_path, &["add", "."]).await;
    run_git(&work_path, &["commit", "-m", "initial"]).await;
    run_git(&work_path, &["push", "origin", "main"]).await;

    let result = trigger::on_api(
        &pool,
        &bare_path,
        project_id,
        "refs/heads/main",
        owner_id,
        "kaniko:latest",
    )
    .await;
    assert!(
        result.is_err(),
        "on_api without .platform.yaml should error"
    );
}

// ===========================================================================
// Trigger: `notify_executor`
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn notify_executor_publishes_event(pool: PgPool) {
    let state = helpers::test_pipeline_state(pool).await;
    let pipeline_id = Uuid::new_v4();

    // Set up a subscriber to verify the Valkey publish
    let sub = state.valkey.next().clone_new();
    sub.init().await.expect("subscriber init");
    sub.subscribe("pipeline:run").await.expect("subscribe");

    let mut rx = sub.message_rx();

    // Small delay for subscription propagation
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Fire the notification
    trigger::notify_executor(&state.pipeline_notify, &state.valkey, pipeline_id).await;

    // Verify the message arrives
    let msg = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("should receive message within 2s")
        .expect("channel should not be closed");

    let payload: String = msg.value.convert().expect("convert payload");
    assert_eq!(payload, pipeline_id.to_string());

    sub.unsubscribe("pipeline:run").await.ok();
    sub.quit().await.ok();
}

// ===========================================================================
// Trigger: multi-step YAML creates correct step order
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn multi_step_yaml_preserves_order(pool: PgPool) {
    let owner_id = helpers::seed_user(&pool, "multi-step").await;
    let ws_id = helpers::seed_workspace(&pool, owner_id).await;
    let (project_id, _name) = helpers::seed_project(&pool, owner_id, ws_id).await;

    let yaml = r"
pipeline:
  steps:
    - name: lint
      image: alpine:3.19
      commands:
        - echo lint
    - name: build
      image: alpine:3.19
      commands:
        - echo build
    - name: test
      image: alpine:3.19
      commands:
        - echo test
";
    let (bare, _work) = helpers::create_repo_with_platform_yaml(yaml).await;

    let pipeline_id = trigger::on_api(
        &pool,
        &bare,
        project_id,
        "refs/heads/main",
        owner_id,
        "kaniko:latest",
    )
    .await
    .expect("on_api should succeed");

    let names: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM pipeline_steps WHERE pipeline_id = $1 ORDER BY step_order ASC",
    )
    .bind(pipeline_id)
    .fetch_all(&pool)
    .await
    .unwrap();

    assert_eq!(names, vec!["lint", "build", "test"]);
}

// ===========================================================================
// Executor: `cancel_pipeline`
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn cancel_pending_pipeline(pool: PgPool) {
    let state = helpers::test_pipeline_state(pool.clone()).await;
    let owner_id = helpers::seed_user(&pool, "cancel-owner").await;
    let ws_id = helpers::seed_workspace(&pool, owner_id).await;
    let (project_id, _name) = helpers::seed_project(&pool, owner_id, ws_id).await;
    let pipeline_id = helpers::seed_pipeline(&pool, project_id, owner_id, "pending").await;
    helpers::seed_step(&pool, pipeline_id, project_id, 0, "step-0", "pending").await;

    platform_pipeline::executor::cancel_pipeline(&state, pipeline_id)
        .await
        .expect("cancel should succeed");

    let status: String = sqlx::query_scalar("SELECT status FROM pipelines WHERE id = $1")
        .bind(pipeline_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "cancelled");

    // Verify steps were also marked skipped
    let step_status: String =
        sqlx::query_scalar("SELECT status FROM pipeline_steps WHERE pipeline_id = $1 LIMIT 1")
            .bind(pipeline_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(step_status, "skipped");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn cancel_running_pipeline(pool: PgPool) {
    let state = helpers::test_pipeline_state(pool.clone()).await;
    let owner_id = helpers::seed_user(&pool, "cancel-run").await;
    let ws_id = helpers::seed_workspace(&pool, owner_id).await;
    let (project_id, _name) = helpers::seed_project(&pool, owner_id, ws_id).await;
    let pipeline_id = helpers::seed_pipeline(&pool, project_id, owner_id, "running").await;
    helpers::seed_step(&pool, pipeline_id, project_id, 0, "step-0", "running").await;
    helpers::seed_step(&pool, pipeline_id, project_id, 1, "step-1", "pending").await;

    platform_pipeline::executor::cancel_pipeline(&state, pipeline_id)
        .await
        .expect("cancel should succeed");

    let status: String = sqlx::query_scalar("SELECT status FROM pipelines WHERE id = $1")
        .bind(pipeline_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "cancelled");

    // Pending step should be skipped
    let step1_status: String = sqlx::query_scalar(
        "SELECT status FROM pipeline_steps WHERE pipeline_id = $1 AND step_order = 1",
    )
    .bind(pipeline_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(step1_status, "skipped");
}

#[sqlx::test(migrations = "../../../migrations")]
async fn cancel_already_succeeded_is_noop(pool: PgPool) {
    let state = helpers::test_pipeline_state(pool.clone()).await;
    let owner_id = helpers::seed_user(&pool, "cancel-done").await;
    let ws_id = helpers::seed_workspace(&pool, owner_id).await;
    let (project_id, _name) = helpers::seed_project(&pool, owner_id, ws_id).await;
    let pipeline_id = helpers::seed_pipeline(&pool, project_id, owner_id, "success").await;

    // Terminal state: cancel should be a no-op (not an error)
    let result = platform_pipeline::executor::cancel_pipeline(&state, pipeline_id).await;
    assert!(
        result.is_ok(),
        "cancelling a terminal pipeline should not error"
    );

    // Status unchanged
    let status: String = sqlx::query_scalar("SELECT status FROM pipelines WHERE id = $1")
        .bind(pipeline_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "success");
}

// ===========================================================================
// Executor: run loop claims pending pipelines
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn executor_run_claims_pending_pipeline(pool: PgPool) {
    let state = helpers::test_pipeline_state(pool.clone()).await;
    let owner_id = helpers::seed_user(&pool, "exec-owner").await;
    let ws_id = helpers::seed_workspace(&pool, owner_id).await;
    let (project_id, _name) = helpers::seed_project(&pool, owner_id, ws_id).await;
    let pipeline_id = helpers::seed_pipeline(&pool, project_id, owner_id, "pending").await;
    helpers::seed_step(&pool, pipeline_id, project_id, 0, "step-0", "pending").await;

    // Start executor, give it time to claim the pipeline, then cancel
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_clone = cancel.clone();
    let state_clone = state.clone();
    let handle = tokio::spawn(async move {
        platform_pipeline::executor::run(state_clone, cancel_clone).await;
    });

    // Wake it up
    state.pipeline_notify.notify_one();

    // Wait for the pipeline to be claimed (status changes from 'pending')
    let mut attempts = 0;
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let status: String = sqlx::query_scalar("SELECT status FROM pipelines WHERE id = $1")
            .bind(pipeline_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        if status != "pending" {
            // Pipeline was claimed — it should be 'running' or 'failure'
            // (it will fail because there's no K8s namespace setup, which is fine)
            assert!(
                status == "running" || status == "failure",
                "expected running or failure, got {status}"
            );
            break;
        }
        attempts += 1;
        assert!(attempts <= 25, "pipeline was not claimed after 5 seconds");
    }

    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
}

// ===========================================================================
// Trigger: imagebuild step type
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn imagebuild_step_generates_kaniko_command(pool: PgPool) {
    let owner_id = helpers::seed_user(&pool, "kaniko-owner").await;
    let ws_id = helpers::seed_workspace(&pool, owner_id).await;
    let (project_id, _name) = helpers::seed_project(&pool, owner_id, ws_id).await;

    let yaml = r"
pipeline:
  steps:
    - name: build-image
      type: imagebuild
      dockerfile: Dockerfile
      image_name: myapp
";
    let (bare, _work) = helpers::create_repo_with_platform_yaml(yaml).await;

    let pipeline_id = trigger::on_api(
        &pool,
        &bare,
        project_id,
        "refs/heads/main",
        owner_id,
        "kaniko:latest",
    )
    .await
    .expect("on_api should succeed");

    let step_type: String =
        sqlx::query_scalar("SELECT step_type FROM pipeline_steps WHERE pipeline_id = $1 LIMIT 1")
            .bind(pipeline_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(step_type, "imagebuild");

    // Kaniko image should be the one we passed
    let image: String =
        sqlx::query_scalar("SELECT image FROM pipeline_steps WHERE pipeline_id = $1 LIMIT 1")
            .bind(pipeline_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(image, "kaniko:latest");

    // Commands should contain `/kaniko/executor`
    let commands: Vec<String> =
        sqlx::query_scalar("SELECT unnest(commands) FROM pipeline_steps WHERE pipeline_id = $1")
            .bind(pipeline_id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(
        commands.iter().any(|c| c.contains("/kaniko/executor")),
        "expected kaniko command, got: {commands:?}"
    );
}

// ===========================================================================
// Trigger: `read_file_at_ref` / `read_version_at_ref`
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn read_file_at_ref_returns_content(_pool: PgPool) {
    let (bare, _work) =
        helpers::create_repo_with_platform_yaml(helpers::MINIMAL_PLATFORM_YAML).await;

    let content = trigger::read_file_at_ref(&bare, "main", ".platform.yaml").await;
    assert!(content.is_some());
    assert!(content.unwrap().contains("test-step"));
}

#[sqlx::test(migrations = "../../../migrations")]
async fn read_file_at_ref_missing_file(_pool: PgPool) {
    let (bare, _work) =
        helpers::create_repo_with_platform_yaml(helpers::MINIMAL_PLATFORM_YAML).await;

    let content = trigger::read_file_at_ref(&bare, "main", "nonexistent.txt").await;
    assert!(content.is_none());
}

#[sqlx::test(migrations = "../../../migrations")]
async fn read_version_at_ref_with_version_file(_pool: PgPool) {
    let base = std::env::temp_dir().join(format!("pl-test-ver-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&base).unwrap();
    let bare_path = base.join("bare.git");
    let work_path = base.join("work");
    run_git(&bare_path, &["init", "--bare", bare_path.to_str().unwrap()]).await;
    run_git(
        &work_path,
        &[
            "clone",
            bare_path.to_str().unwrap(),
            work_path.to_str().unwrap(),
        ],
    )
    .await;
    run_git(&work_path, &["config", "user.email", "test@test.local"]).await;
    run_git(&work_path, &["config", "user.name", "test"]).await;
    std::fs::write(work_path.join("VERSION"), "app=1.2.3\n").unwrap();
    std::fs::write(
        work_path.join(".platform.yaml"),
        helpers::MINIMAL_PLATFORM_YAML,
    )
    .unwrap();
    run_git(&work_path, &["add", "."]).await;
    run_git(&work_path, &["commit", "-m", "initial"]).await;
    run_git(&work_path, &["push", "origin", "main"]).await;

    let version = trigger::read_version_at_ref(&bare_path, "main").await;
    let vi = version.expect("VERSION file should be parsed");
    assert_eq!(vi.images.get("app"), Some(&"1.2.3".to_string()));
}

// ===========================================================================
// Trigger: step environment injection
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
async fn step_environment_stored_as_json(pool: PgPool) {
    let owner_id = helpers::seed_user(&pool, "env-owner").await;
    let ws_id = helpers::seed_workspace(&pool, owner_id).await;
    let (project_id, _name) = helpers::seed_project(&pool, owner_id, ws_id).await;

    let yaml = r"
pipeline:
  steps:
    - name: with-env
      image: alpine:3.19
      commands:
        - echo $MY_VAR
      environment:
        MY_VAR: hello
        OTHER: world
";
    let (bare, _work) = helpers::create_repo_with_platform_yaml(yaml).await;

    let pipeline_id = trigger::on_api(
        &pool,
        &bare,
        project_id,
        "refs/heads/main",
        owner_id,
        "kaniko:latest",
    )
    .await
    .expect("on_api should succeed");

    let env_json: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT environment FROM pipeline_steps WHERE pipeline_id = $1 LIMIT 1")
            .bind(pipeline_id)
            .fetch_one(&pool)
            .await
            .unwrap();

    let env = env_json.expect("environment should be stored");
    assert_eq!(env["MY_VAR"], "hello");
    assert_eq!(env["OTHER"], "world");
}
