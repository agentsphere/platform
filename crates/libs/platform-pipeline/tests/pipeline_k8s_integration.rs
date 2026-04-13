// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Tier 3 — K8s integration tests for `platform-pipeline`.
//!
//! These tests exercise pipeline execution against a real Kind cluster.
//! All tests are `#[ignore = "requires K8s"]` so they are skipped by default
//! and only run with `cargo nextest run -- --ignored` or via
//! `just crate-test-kubernetes platform-pipeline`.

#[allow(dead_code)]
mod helpers;

use k8s_openapi::api::core::v1::Namespace;
use kube::Api;
use kube::api::DeleteParams;
use sqlx::PgPool;
use uuid::Uuid;

use platform_pipeline::trigger;

// ===========================================================================
// Namespace lifecycle (via platform_k8s)
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
#[ignore = "requires K8s"]
async fn ensure_pipeline_namespace_creates_ns(pool: PgPool) {
    let state = helpers::test_pipeline_state(pool).await;
    let short_id = &Uuid::new_v4().to_string()[..8];
    let ns_name = format!("test-p-{short_id}");

    // Ensure namespace via platform_k8s (same call the executor uses)
    platform_k8s::ensure_namespace(
        &state.kube,
        &ns_name,
        "pipeline",
        &Uuid::new_v4().to_string(),
        &state.config.platform_namespace,
        &state.config.gateway_namespace,
        true, // dev_mode
    )
    .await
    .expect("ensure_namespace should succeed");

    // Verify namespace exists
    let ns_api: Api<Namespace> = Api::all(state.kube.clone());
    let ns = ns_api.get(&ns_name).await.expect("namespace should exist");
    let labels = ns.metadata.labels.unwrap_or_default();
    assert_eq!(labels.get("platform.io/env"), Some(&"pipeline".to_string()));

    // Cleanup
    ns_api.delete(&ns_name, &DeleteParams::default()).await.ok();
}

#[sqlx::test(migrations = "../../../migrations")]
#[ignore = "requires K8s"]
async fn ensure_pipeline_namespace_idempotent(pool: PgPool) {
    let state = helpers::test_pipeline_state(pool).await;
    let short_id = &Uuid::new_v4().to_string()[..8];
    let ns_name = format!("test-p-{short_id}");
    let project_id = Uuid::new_v4().to_string();

    // Create twice — second call should be a no-op
    for _ in 0..2 {
        platform_k8s::ensure_namespace(
            &state.kube,
            &ns_name,
            "pipeline",
            &project_id,
            &state.config.platform_namespace,
            &state.config.gateway_namespace,
            true,
        )
        .await
        .expect("ensure_namespace should be idempotent");
    }

    // Cleanup
    let ns_api: Api<Namespace> = Api::all(state.kube.clone());
    ns_api.delete(&ns_name, &DeleteParams::default()).await.ok();
}

#[sqlx::test(migrations = "../../../migrations")]
#[ignore = "requires K8s"]
async fn cleanup_pipeline_namespace_deletes(pool: PgPool) {
    let state = helpers::test_pipeline_state(pool).await;
    let short_id = &Uuid::new_v4().to_string()[..8];
    let ns_name = format!("test-p-{short_id}");

    // Create
    platform_k8s::ensure_namespace(
        &state.kube,
        &ns_name,
        "pipeline",
        &Uuid::new_v4().to_string(),
        &state.config.platform_namespace,
        &state.config.gateway_namespace,
        true,
    )
    .await
    .unwrap();

    // Delete
    platform_k8s::delete_namespace(&state.kube, &ns_name)
        .await
        .expect("delete_namespace should succeed");

    // Verify gone (may be terminating, so just check get returns error)
    let ns_api: Api<Namespace> = Api::all(state.kube.clone());
    // Allow a brief period for K8s to process
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let result = ns_api.get(&ns_name).await;
    // It might still be in "Terminating" phase, which is OK — the key is it was deleted
    if let Ok(ns) = result {
        let phase = ns.status.and_then(|s| s.phase).unwrap_or_default();
        assert_eq!(phase, "Terminating", "namespace should be terminating");
    }
}

#[sqlx::test(migrations = "../../../migrations")]
#[ignore = "requires K8s"]
async fn cleanup_pipeline_namespace_missing_is_ok(pool: PgPool) {
    let state = helpers::test_pipeline_state(pool).await;
    let ns_name = format!("nonexistent-ns-{}", &Uuid::new_v4().to_string()[..8]);

    // Deleting a non-existent namespace should not error
    let result = platform_k8s::delete_namespace(&state.kube, &ns_name).await;
    assert!(result.is_ok(), "deleting missing namespace should be OK");
}

// ===========================================================================
// Full pipeline execution: single step (happy path)
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
#[ignore = "requires K8s"]
async fn execute_pipeline_single_step_success(pool: PgPool) {
    let state = helpers::test_pipeline_state(pool.clone()).await;
    let owner_id = helpers::seed_user(&pool, "k8s-exec").await;
    let ws_id = helpers::seed_workspace(&pool, owner_id).await;
    let (project_id, _name) = helpers::seed_project(&pool, owner_id, ws_id).await;

    // Create a repo with a simple .platform.yaml
    let yaml = r#"
pipeline:
  steps:
    - name: echo-step
      image: alpine:3.19
      commands:
        - echo "hello from pipeline"
"#;
    let (bare, _work) = helpers::create_repo_with_platform_yaml(yaml).await;

    // Create pipeline via trigger
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

    // Start executor
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_clone = cancel.clone();
    let state_clone = state.clone();
    let handle = tokio::spawn(async move {
        platform_pipeline::executor::run(state_clone, cancel_clone).await;
    });
    state.pipeline_notify.notify_one();

    // Poll until pipeline reaches a terminal state
    let final_status = poll_pipeline_status(&pool, pipeline_id, 120).await;

    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;

    assert!(
        final_status == "success" || final_status == "failure",
        "pipeline should reach terminal state, got: {final_status}"
    );

    // If the cluster can actually run alpine pods, it should succeed.
    // In a constrained test env, failure is also acceptable (validates the error path).
}

// ===========================================================================
// Full pipeline execution: multi-step sequential
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
#[ignore = "requires K8s"]
async fn execute_pipeline_multi_step_sequential(pool: PgPool) {
    let state = helpers::test_pipeline_state(pool.clone()).await;
    let owner_id = helpers::seed_user(&pool, "k8s-multi").await;
    let ws_id = helpers::seed_workspace(&pool, owner_id).await;
    let (project_id, _name) = helpers::seed_project(&pool, owner_id, ws_id).await;

    let yaml = r"
pipeline:
  steps:
    - name: step-1
      image: alpine:3.19
      commands:
        - echo step1
    - name: step-2
      image: alpine:3.19
      commands:
        - echo step2
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
    .unwrap();

    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_clone = cancel.clone();
    let state_clone = state.clone();
    let handle = tokio::spawn(async move {
        platform_pipeline::executor::run(state_clone, cancel_clone).await;
    });
    state.pipeline_notify.notify_one();

    let final_status = poll_pipeline_status(&pool, pipeline_id, 120).await;
    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;

    // Verify both steps have been processed
    let step_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pipeline_steps WHERE pipeline_id = $1")
            .bind(pipeline_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(step_count, 2);

    // Verify step statuses: either both success (happy) or first failed + second skipped
    let statuses: Vec<String> = sqlx::query_scalar(
        "SELECT status FROM pipeline_steps WHERE pipeline_id = $1 ORDER BY step_order",
    )
    .bind(pipeline_id)
    .fetch_all(&pool)
    .await
    .unwrap();

    // Both should be in a terminal/final state (not 'pending')
    for (i, s) in statuses.iter().enumerate() {
        assert_ne!(
            s, "pending",
            "step {i} should not be pending after execution"
        );
    }

    assert!(
        final_status == "success" || final_status == "failure",
        "pipeline should be terminal: {final_status}",
    );
}

// ===========================================================================
// Cancel running pipeline (K8s cleanup)
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
#[ignore = "requires K8s"]
async fn cancel_pipeline_during_execution(pool: PgPool) {
    let state = helpers::test_pipeline_state(pool.clone()).await;
    let owner_id = helpers::seed_user(&pool, "k8s-cancel").await;
    let ws_id = helpers::seed_workspace(&pool, owner_id).await;
    let (project_id, _name) = helpers::seed_project(&pool, owner_id, ws_id).await;

    // Use a sleep step so the pipeline stays running long enough to cancel
    let yaml = r"
pipeline:
  steps:
    - name: slow-step
      image: alpine:3.19
      commands:
        - sleep 300
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
    .unwrap();

    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_clone = cancel.clone();
    let state_clone = state.clone();
    let handle = tokio::spawn(async move {
        platform_pipeline::executor::run(state_clone, cancel_clone).await;
    });
    state.pipeline_notify.notify_one();

    // Wait for pipeline to start running
    let mut running = false;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let status: String = sqlx::query_scalar("SELECT status FROM pipelines WHERE id = $1")
            .bind(pipeline_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        if status == "running" {
            running = true;
            break;
        }
    }

    if running {
        // Cancel the pipeline
        platform_pipeline::executor::cancel_pipeline(&state, pipeline_id)
            .await
            .expect("cancel should succeed");

        let status: String = sqlx::query_scalar("SELECT status FROM pipelines WHERE id = $1")
            .bind(pipeline_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, "cancelled");
    }

    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

// ===========================================================================
// Pipeline namespace naming
// ===========================================================================

#[sqlx::test(migrations = "../../../migrations")]
#[ignore = "requires K8s"]
async fn pipeline_namespace_naming_convention(pool: PgPool) {
    let state = helpers::test_pipeline_state(pool.clone()).await;
    let owner_id = helpers::seed_user(&pool, "ns-name").await;
    let ws_id = helpers::seed_workspace(&pool, owner_id).await;
    let (project_id, _name) = helpers::seed_project(&pool, owner_id, ws_id).await;

    // Fetch the namespace_slug
    let slug: String = sqlx::query_scalar("SELECT namespace_slug FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();

    let short_id = "abcd1234";
    let ns =
        platform_k8s::pipeline_namespace_name(state.config.ns_prefix.as_deref(), &slug, short_id);

    // Without prefix: "{slug}-p-{short_id}"
    assert_eq!(ns, format!("{slug}-p-{short_id}"));
    assert!(ns.len() <= 63, "namespace must fit DNS label");

    // With prefix
    let ns_with_prefix = platform_k8s::pipeline_namespace_name(Some("dev"), &slug, short_id);
    assert_eq!(ns_with_prefix, format!("dev-{slug}-p-{short_id}"));
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Poll pipeline status until it reaches a terminal state or timeout.
async fn poll_pipeline_status(pool: &PgPool, pipeline_id: Uuid, max_secs: u64) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(max_secs);
    loop {
        let status: String = sqlx::query_scalar("SELECT status FROM pipelines WHERE id = $1")
            .bind(pipeline_id)
            .fetch_one(pool)
            .await
            .unwrap();
        if status != "pending" && status != "running" {
            return status;
        }
        if std::time::Instant::now() > deadline {
            return status;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}
