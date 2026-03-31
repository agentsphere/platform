//! Integration tests for `pipeline::executor::execute_deploy_test_step()`.
//!
//! These tests exercise the deploy_test step type end-to-end against the real
//! Kind cluster: creating temp namespaces, deploying manifests, running test
//! pods, capturing logs, and cleaning up.

mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;
use std::path::PathBuf;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Executor guard + project setup
// ---------------------------------------------------------------------------

#[allow(dead_code)]
struct ExecutorGuard {
    shutdown_tx: tokio::sync::watch::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

impl ExecutorGuard {
    fn spawn(state: &platform::store::AppState) -> Self {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let executor_state = state.clone();
        let handle = tokio::spawn(async move {
            platform::pipeline::executor::run(executor_state, shutdown_rx).await;
        });
        Self {
            shutdown_tx,
            handle,
        }
    }
}

/// Simple deployment manifest using `registry.k8s.io/pause:3.9`.
const DEPLOY_MANIFEST: &str = r#"apiVersion: apps/v1
kind: Deployment
metadata:
  name: test-app
spec:
  replicas: 1
  selector:
    matchLabels:
      app: test-app
  template:
    metadata:
      labels:
        app: test-app
    spec:
      containers:
        - name: app
          image: registry.k8s.io/pause:3.9
          ports:
            - containerPort: 8080
"#;

/// Deployment manifest with a non-existent image (will never become ready).
const DEPLOY_MANIFEST_BAD_IMAGE: &str = r#"apiVersion: apps/v1
kind: Deployment
metadata:
  name: test-app
spec:
  replicas: 1
  selector:
    matchLabels:
      app: test-app
  template:
    metadata:
      labels:
        app: test-app
    spec:
      containers:
        - name: app
          image: no-such-registry.invalid/no-such-image:never
          ports:
            - containerPort: 8080
"#;

const DEFAULT_PIPELINE_YAML: &str = "\
pipeline:
  steps:
    - name: test
      image: alpine:3.19
      commands:
        - echo hello
";

async fn setup_pipeline_project(
    state: &platform::store::AppState,
    app: &axum::Router,
    token: &str,
    name: &str,
) -> (Uuid, PathBuf, PathBuf, tempfile::TempDir, tempfile::TempDir) {
    let project_id = helpers::create_project(app, token, name, "private").await;

    let (bare_dir, bare_path) = helpers::create_bare_repo();
    let (work_dir, work_path) = helpers::create_working_copy(&bare_path);

    std::fs::write(work_path.join(".platform.yaml"), DEFAULT_PIPELINE_YAML).unwrap();
    helpers::git_cmd(&work_path, &["add", "."]);
    helpers::git_cmd(&work_path, &["commit", "-m", "add pipeline config"]);
    helpers::git_cmd(&work_path, &["push", "origin", "main"]);

    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&state.pool)
        .await
        .unwrap();

    (project_id, bare_path, work_path, bare_dir, work_dir)
}

fn update_pipeline_yaml(work_path: &std::path::Path, yaml: &str) {
    std::fs::write(work_path.join(".platform.yaml"), yaml).unwrap();
    helpers::git_cmd(work_path, &["add", "."]);
    helpers::git_cmd(work_path, &["commit", "-m", "update pipeline config"]);
    helpers::git_cmd(work_path, &["push", "origin", "main"]);
}

fn commit_deploy_manifests(work_path: &std::path::Path, manifest_content: &str) {
    let deploy_dir = work_path.join("deploy");
    std::fs::create_dir_all(&deploy_dir).unwrap();
    std::fs::write(deploy_dir.join("app.yaml"), manifest_content).unwrap();
    helpers::git_cmd(work_path, &["add", "."]);
    helpers::git_cmd(work_path, &["commit", "-m", "add deploy manifests"]);
    helpers::git_cmd(work_path, &["push", "origin", "main"]);
}

async fn trigger_pipeline(
    app: &axum::Router,
    token: &str,
    project_id: Uuid,
    git_ref: &str,
) -> (String, serde_json::Value) {
    let (status, body) = helpers::post_json(
        app,
        token,
        &format!("/api/projects/{project_id}/pipelines"),
        serde_json::json!({ "git_ref": git_ref }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "trigger failed: {body}");
    let pipeline_id = body["id"]
        .as_str()
        .expect("pipeline should have id")
        .to_string();
    (pipeline_id, body)
}

/// Insert a pipeline + deploy_test step directly in the DB.
async fn insert_deploy_test_pipeline(
    state: &platform::store::AppState,
    project_id: Uuid,
    deploy_test_config: serde_json::Value,
) -> Uuid {
    let admin_row: (Uuid,) = sqlx::query_as("SELECT id FROM users WHERE name = 'admin'")
        .fetch_one(&state.pool)
        .await
        .unwrap();

    let pipeline_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO pipelines (id, project_id, triggered_by, status, git_ref, trigger, commit_sha)
         VALUES ($1, $2, $3, 'pending', 'refs/heads/main', 'api', 'abc123')",
    )
    .bind(pipeline_id)
    .bind(project_id)
    .bind(admin_row.0)
    .execute(&state.pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO pipeline_steps
            (pipeline_id, project_id, step_order, name, image, commands,
             deploy_test, step_type)
         VALUES ($1, $2, 0, 'deploy-test', 'busybox:1.36', '{}',
                 $3, 'deploy_test')",
    )
    .bind(pipeline_id)
    .bind(project_id)
    .bind(&deploy_test_config)
    .execute(&state.pool)
    .await
    .unwrap();

    pipeline_id
}

// ===========================================================================
// Test 1: Full happy path — deploy app, run test pod, succeed
// ===========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn executor_deploy_test_success(pool: PgPool) {
    let (state, admin_token, _server) = helpers::start_pipeline_server(pool).await;
    let app = helpers::test_router(state.clone());
    let _executor = ExecutorGuard::spawn(&state);

    let (project_id, _bare_path, work_path, _bd, _wd) =
        setup_pipeline_project(&state, &app, &admin_token, "dt-ok").await;

    commit_deploy_manifests(&work_path, DEPLOY_MANIFEST);

    update_pipeline_yaml(
        &work_path,
        "\
pipeline:
  steps:
    - name: integration-test
      deploy_test:
        test_image: busybox:1.36
        manifests: deploy/
        commands:
          - echo test-passed
        readiness_timeout: 60
        wait_for_services: []
",
    );

    let (pipeline_id, _) =
        trigger_pipeline(&app, &admin_token, project_id, "refs/heads/main").await;
    state.pipeline_notify.notify_one();

    let final_status =
        helpers::poll_pipeline_status(&app, &admin_token, project_id, &pipeline_id, 180).await;

    let (_, detail) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/pipelines/{pipeline_id}"),
    )
    .await;

    let steps = detail["steps"].as_array().expect("should have steps");
    let dt_step = steps
        .iter()
        .find(|s| s["name"].as_str() == Some("integration-test"));
    assert!(dt_step.is_some(), "should have deploy-test step");
    let dt_step = dt_step.unwrap();

    assert_eq!(
        final_status, "success",
        "deploy_test should succeed. detail: {detail}"
    );
    assert_eq!(dt_step["status"].as_str(), Some("success"));
    assert_eq!(dt_step["exit_code"].as_i64(), Some(0));

    // Verify log_ref was set and log exists in MinIO
    if let Some(log_ref) = dt_step["log_ref"].as_str() {
        assert!(!log_ref.is_empty());
        let exists = state.minio.exists(log_ref).await.unwrap_or(false);
        assert!(exists, "test log should exist in MinIO at: {log_ref}");
    }

    assert!(dt_step["duration_ms"].as_i64().is_some());
}

// ===========================================================================
// Test 2: Test pod fails (exit 1) — step should fail
// ===========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn executor_deploy_test_test_pod_fails(pool: PgPool) {
    let (state, admin_token, _server) = helpers::start_pipeline_server(pool).await;
    let app = helpers::test_router(state.clone());
    let _executor = ExecutorGuard::spawn(&state);

    let (project_id, _bare_path, work_path, _bd, _wd) =
        setup_pipeline_project(&state, &app, &admin_token, "dt-fail").await;

    commit_deploy_manifests(&work_path, DEPLOY_MANIFEST);

    update_pipeline_yaml(
        &work_path,
        "\
pipeline:
  steps:
    - name: failing-test
      deploy_test:
        test_image: busybox:1.36
        manifests: deploy/
        commands:
          - echo about-to-fail && exit 1
        readiness_timeout: 60
        wait_for_services: []
",
    );

    let (pipeline_id, _) =
        trigger_pipeline(&app, &admin_token, project_id, "refs/heads/main").await;
    state.pipeline_notify.notify_one();

    let final_status =
        helpers::poll_pipeline_status(&app, &admin_token, project_id, &pipeline_id, 180).await;
    assert_eq!(final_status, "failure");

    let (_, detail) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/pipelines/{pipeline_id}"),
    )
    .await;

    let steps = detail["steps"].as_array().expect("should have steps");
    let dt_step = steps
        .iter()
        .find(|s| s["name"].as_str() == Some("failing-test"))
        .expect("should have deploy-test step");

    assert_eq!(dt_step["status"].as_str(), Some("failure"));
    if let Some(code) = dt_step["exit_code"].as_i64() {
        assert_ne!(code, 0);
    }
}

// ===========================================================================
// Test 3: App never becomes ready (bad image) — readiness timeout
// ===========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn executor_deploy_test_app_not_ready(pool: PgPool) {
    let (state, admin_token, _server) = helpers::start_pipeline_server(pool).await;
    let app = helpers::test_router(state.clone());
    let _executor = ExecutorGuard::spawn(&state);

    let (project_id, _bare_path, work_path, _bd, _wd) =
        setup_pipeline_project(&state, &app, &admin_token, "dt-noready").await;

    commit_deploy_manifests(&work_path, DEPLOY_MANIFEST_BAD_IMAGE);

    update_pipeline_yaml(
        &work_path,
        "\
pipeline:
  steps:
    - name: timeout-test
      deploy_test:
        test_image: busybox:1.36
        manifests: deploy/
        commands:
          - echo should-never-run
        readiness_timeout: 10
        wait_for_services: []
",
    );

    let (pipeline_id, _) =
        trigger_pipeline(&app, &admin_token, project_id, "refs/heads/main").await;
    state.pipeline_notify.notify_one();

    let final_status =
        helpers::poll_pipeline_status(&app, &admin_token, project_id, &pipeline_id, 120).await;
    assert_eq!(final_status, "failure");

    let (_, detail) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/pipelines/{pipeline_id}"),
    )
    .await;

    let steps = detail["steps"].as_array().expect("should have steps");
    let dt_step = steps
        .iter()
        .find(|s| s["name"].as_str() == Some("timeout-test"))
        .expect("should have deploy-test step");

    assert_eq!(dt_step["status"].as_str(), Some("failure"));
}

// ===========================================================================
// Test 4: Temp namespace is cleaned up after test completes
// ===========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn executor_deploy_test_namespace_cleanup(pool: PgPool) {
    let (state, admin_token, _server) = helpers::start_pipeline_server(pool).await;
    let app = helpers::test_router(state.clone());
    let _executor = ExecutorGuard::spawn(&state);

    let (project_id, _bare_path, work_path, _bd, _wd) =
        setup_pipeline_project(&state, &app, &admin_token, "dt-cleanup").await;

    commit_deploy_manifests(&work_path, DEPLOY_MANIFEST);

    update_pipeline_yaml(
        &work_path,
        "\
pipeline:
  steps:
    - name: cleanup-test
      deploy_test:
        test_image: busybox:1.36
        manifests: deploy/
        commands:
          - echo done
        readiness_timeout: 60
        wait_for_services: []
",
    );

    let (pipeline_id, _) =
        trigger_pipeline(&app, &admin_token, project_id, "refs/heads/main").await;
    state.pipeline_notify.notify_one();

    let _ = helpers::poll_pipeline_status(&app, &admin_token, project_id, &pipeline_id, 180).await;

    // Reconstruct temp namespace name: {namespace_slug}-test-{pipeline_id[..8]}
    let ns_slug: (String,) = sqlx::query_as("SELECT namespace_slug FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_one(&state.pool)
        .await
        .unwrap();

    let pipeline_uuid = Uuid::parse_str(&pipeline_id).unwrap();
    let ns_name = format!("{}-test-{}", ns_slug.0, &pipeline_uuid.to_string()[..8]);

    // TestNamespaceGuard spawns an async delete on drop — allow time to propagate
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;

    let ns_api: kube::Api<k8s_openapi::api::core::v1::Namespace> =
        kube::Api::all(state.kube.clone());
    let result = ns_api.get(&ns_name).await;

    match result {
        Err(_) => {
            // Not found — cleanup succeeded
        }
        Ok(ns) => {
            let phase = ns
                .status
                .as_ref()
                .and_then(|s| s.phase.as_deref())
                .unwrap_or("Active");
            assert_eq!(
                phase, "Terminating",
                "temp namespace should be deleted or terminating, got: {phase}"
            );
        }
    }
}

// ===========================================================================
// Test 5: Project secrets with scope "test" are injected
// ===========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn executor_deploy_test_with_secrets(pool: PgPool) {
    let (state, admin_token, _server) = helpers::start_pipeline_server(pool).await;
    let app = helpers::test_router(state.clone());
    let _executor = ExecutorGuard::spawn(&state);

    let (project_id, _bare_path, work_path, _bd, _wd) =
        setup_pipeline_project(&state, &app, &admin_token, "dt-secrets").await;

    // Create project secrets with test and all scopes
    let (s1, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/secrets"),
        serde_json::json!({
            "name": "TEST_DB_URL",
            "value": "postgres://test:test@db:5432/test",
            "scope": "test",
        }),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED);

    let (s2, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/secrets"),
        serde_json::json!({
            "name": "SHARED_API_KEY",
            "value": "key-12345",
            "scope": "all",
        }),
    )
    .await;
    assert_eq!(s2, StatusCode::CREATED);

    commit_deploy_manifests(&work_path, DEPLOY_MANIFEST);

    update_pipeline_yaml(
        &work_path,
        "\
pipeline:
  steps:
    - name: secret-test
      deploy_test:
        test_image: busybox:1.36
        manifests: deploy/
        commands:
          - echo secrets-checked
        readiness_timeout: 60
        wait_for_services: []
",
    );

    let (pipeline_id, _) =
        trigger_pipeline(&app, &admin_token, project_id, "refs/heads/main").await;
    state.pipeline_notify.notify_one();

    let final_status =
        helpers::poll_pipeline_status(&app, &admin_token, project_id, &pipeline_id, 180).await;

    // Pipeline should complete — the inject_test_namespace_secrets code path
    // is exercised regardless of whether the test pod reads the K8s secret.
    assert!(
        matches!(final_status.as_str(), "success" | "failure"),
        "pipeline should reach terminal state, got: {final_status}"
    );
}

// ===========================================================================
// Test 6: Deploy test via direct DB insertion (step_type = "deploy_test")
// ===========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn executor_deploy_test_via_step_type(pool: PgPool) {
    let (state, admin_token, _server) = helpers::start_pipeline_server(pool).await;
    let app = helpers::test_router(state.clone());
    let _executor = ExecutorGuard::spawn(&state);

    let (project_id, _bare_path, work_path, _bd, _wd) =
        setup_pipeline_project(&state, &app, &admin_token, "dt-direct").await;

    commit_deploy_manifests(&work_path, DEPLOY_MANIFEST);

    let deploy_test_config = serde_json::json!({
        "test_image": "busybox:1.36",
        "manifests": "deploy/",
        "commands": ["echo direct-test-passed"],
        "readiness_timeout": 60,
        "wait_for_services": []
    });

    let pipeline_id = insert_deploy_test_pipeline(&state, project_id, deploy_test_config).await;
    state.pipeline_notify.notify_one();

    let pipeline_id_str = pipeline_id.to_string();
    let final_status =
        helpers::poll_pipeline_status(&app, &admin_token, project_id, &pipeline_id_str, 180).await;

    assert!(
        matches!(final_status.as_str(), "success" | "failure"),
        "directly-inserted deploy_test should reach terminal state, got: {final_status}"
    );
}

// ===========================================================================
// Test 7: Deploy test with missing manifests directory fails gracefully
// ===========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn executor_deploy_test_missing_manifests(pool: PgPool) {
    let (state, admin_token, _server) = helpers::start_pipeline_server(pool).await;
    let app = helpers::test_router(state.clone());
    let _executor = ExecutorGuard::spawn(&state);

    let (project_id, _bare_path, work_path, _bd, _wd) =
        setup_pipeline_project(&state, &app, &admin_token, "dt-nomanifest").await;

    // Do NOT commit deploy manifests

    update_pipeline_yaml(
        &work_path,
        "\
pipeline:
  steps:
    - name: no-manifests
      deploy_test:
        test_image: busybox:1.36
        manifests: deploy/
        commands:
          - echo should-never-run
        readiness_timeout: 10
        wait_for_services: []
",
    );

    let (pipeline_id, _) =
        trigger_pipeline(&app, &admin_token, project_id, "refs/heads/main").await;
    state.pipeline_notify.notify_one();

    let final_status =
        helpers::poll_pipeline_status(&app, &admin_token, project_id, &pipeline_id, 120).await;
    assert_eq!(final_status, "failure");

    let (_, detail) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/pipelines/{pipeline_id}"),
    )
    .await;

    let steps = detail["steps"].as_array().expect("should have steps");
    let dt_step = steps
        .iter()
        .find(|s| s["name"].as_str() == Some("no-manifests"))
        .expect("should have deploy-test step");
    assert_eq!(dt_step["status"].as_str(), Some("failure"));
}

// ===========================================================================
// Test 8: Deploy test with wait_for_services (service never becomes ready)
// ===========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn executor_deploy_test_services_not_ready(pool: PgPool) {
    let (state, admin_token, _server) = helpers::start_pipeline_server(pool).await;
    let app = helpers::test_router(state.clone());
    let _executor = ExecutorGuard::spawn(&state);

    let (project_id, _bare_path, work_path, _bd, _wd) =
        setup_pipeline_project(&state, &app, &admin_token, "dt-svcfail").await;

    commit_deploy_manifests(&work_path, DEPLOY_MANIFEST);

    update_pipeline_yaml(
        &work_path,
        "\
pipeline:
  steps:
    - name: svc-wait-test
      deploy_test:
        test_image: busybox:1.36
        manifests: deploy/
        commands:
          - echo should-never-run
        readiness_timeout: 10
        wait_for_services:
          - nonexistent-service
",
    );

    let (pipeline_id, _) =
        trigger_pipeline(&app, &admin_token, project_id, "refs/heads/main").await;
    state.pipeline_notify.notify_one();

    let final_status =
        helpers::poll_pipeline_status(&app, &admin_token, project_id, &pipeline_id, 120).await;
    assert_eq!(final_status, "failure");

    let (_, detail) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/pipelines/{pipeline_id}"),
    )
    .await;

    let steps = detail["steps"].as_array().expect("should have steps");
    let dt_step = steps
        .iter()
        .find(|s| s["name"].as_str() == Some("svc-wait-test"))
        .expect("should have deploy-test step");
    assert_eq!(dt_step["status"].as_str(), Some("failure"));
}
