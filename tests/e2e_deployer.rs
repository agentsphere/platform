mod e2e_helpers;

use axum::http::StatusCode;
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// E2E Deployer Reconciler Tests (7 tests)
//
// These tests spawn the deployer reconciler as a background task and verify
// that it reconciles deployments with real K8s.
//
// Deployment API CRUD tests are in deployment_integration.rs.
// Preview lifecycle tests are in deployment_integration.rs.
// ---------------------------------------------------------------------------

/// Helper: create a project and insert a deployment row directly.
/// Returns the `project_id`.
async fn setup_deploy_project(
    state: &platform::store::AppState,
    app: &axum::Router,
    token: &str,
    name: &str,
    environment: &str,
    image_ref: &str,
) -> Uuid {
    let project_id = e2e_helpers::create_project(app, token, name, "private").await;

    sqlx::query(
        r"INSERT INTO deployments (project_id, environment, image_ref, desired_status, current_status)
           VALUES ($1, $2, $3, 'active', 'pending')",
    )
    .bind(project_id)
    .bind(environment)
    .bind(image_ref)
    .execute(&state.pool)
    .await
    .unwrap();

    project_id
}

/// RAII guard that spawns the deployer reconciler and shuts it down on drop.
#[allow(dead_code)]
struct ReconcilerGuard {
    shutdown_tx: tokio::sync::watch::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

impl ReconcilerGuard {
    fn spawn(state: &platform::store::AppState) -> Self {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let reconciler_state = state.clone();
        let handle = tokio::spawn(async move {
            platform::deployer::reconciler::run(reconciler_state, shutdown_rx).await;
        });
        Self {
            shutdown_tx,
            handle,
        }
    }

    #[allow(dead_code)]
    async fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
        let _ = self.handle.await;
    }
}

/// Reconciler picks up a pending deployment with a basic manifest and
/// transitions it to healthy status (real K8s apply).
#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn reconciler_deploys_basic_manifest(pool: PgPool) {
    let (state, admin_token) = e2e_helpers::e2e_state(pool.clone()).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = admin_token.clone();

    let project_id = setup_deploy_project(
        &state,
        &app,
        &token,
        "recon-basic",
        "staging",
        "nginx:1.25-alpine",
    )
    .await;

    let _reconciler = ReconcilerGuard::spawn(&state);

    let final_status =
        e2e_helpers::poll_deployment_status(&app, &token, project_id, "staging", "healthy", 120)
            .await;
    assert_eq!(
        final_status, "healthy",
        "reconciler should drive deployment to healthy"
    );

    // Verify history entry was created
    let (status, body) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/deployments/staging/history"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let total = body["total"].as_i64().unwrap_or(0);
    assert!(
        total >= 1,
        "should have at least 1 history entry after reconciliation"
    );

    // Clean up K8s deployment
    let deploy_name = "recon-basic-staging";
    let target_ns =
        platform::deployer::reconciler::target_namespace(&state.config, "recon-basic", "staging");
    let deployments: kube::Api<k8s_openapi::api::apps::v1::Deployment> =
        kube::Api::namespaced(state.kube.clone(), &target_ns);
    let _ = deployments
        .delete(deploy_name, &kube::api::DeleteParams::default())
        .await;
}

/// Reconciler rollback restores the previous image.
#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn reconciler_rollback_restores_previous(pool: PgPool) {
    let (state, admin_token) = e2e_helpers::e2e_state(pool.clone()).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = admin_token.clone();

    let project_id = setup_deploy_project(
        &state,
        &app,
        &token,
        "recon-rb",
        "staging",
        "nginx:1.25-alpine",
    )
    .await;

    let _reconciler = ReconcilerGuard::spawn(&state);

    // Wait for initial deployment to become healthy
    e2e_helpers::poll_deployment_status(&app, &token, project_id, "staging", "healthy", 120).await;

    // Update to v2
    let (status, _) = e2e_helpers::patch_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/deployments/staging"),
        serde_json::json!({ "image_ref": "nginx:1.26-alpine" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Wait for v2 to become healthy
    e2e_helpers::poll_deployment_status(&app, &token, project_id, "staging", "healthy", 120).await;

    // Trigger rollback
    let (status, _) = e2e_helpers::post_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/deployments/staging/rollback"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Wait for rollback reconciliation
    e2e_helpers::poll_deployment_status(&app, &token, project_id, "staging", "healthy", 120).await;

    // Verify image was rolled back to v1
    let (_, body) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/deployments/staging"),
    )
    .await;
    assert_eq!(
        body["image_ref"], "nginx:1.25-alpine",
        "image should be rolled back to v1"
    );

    // Clean up
    let deploy_name = "recon-rb-staging";
    let target_ns =
        platform::deployer::reconciler::target_namespace(&state.config, "recon-rb", "staging");
    let deployments: kube::Api<k8s_openapi::api::apps::v1::Deployment> =
        kube::Api::namespaced(state.kube.clone(), &target_ns);
    let _ = deployments
        .delete(deploy_name, &kube::api::DeleteParams::default())
        .await;
}

/// Reconciler stop scales the K8s deployment to 0 replicas.
#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn reconciler_stop_scales_to_zero(pool: PgPool) {
    let (state, admin_token) = e2e_helpers::e2e_state(pool.clone()).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = admin_token.clone();

    let project_id = setup_deploy_project(
        &state,
        &app,
        &token,
        "recon-stop",
        "staging",
        "nginx:1.25-alpine",
    )
    .await;

    let _reconciler = ReconcilerGuard::spawn(&state);

    // Wait for healthy first
    e2e_helpers::poll_deployment_status(&app, &token, project_id, "staging", "healthy", 120).await;

    // Set desired_status to stopped
    let (status, _) = e2e_helpers::patch_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/deployments/staging"),
        serde_json::json!({ "desired_status": "stopped" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Wait for reconciler to process
    e2e_helpers::poll_deployment_status(&app, &token, project_id, "staging", "healthy", 60).await;

    // Verify K8s deployment has 0 replicas
    let deploy_name = "recon-stop-staging";
    let target_ns =
        platform::deployer::reconciler::target_namespace(&state.config, "recon-stop", "staging");
    let deployments: kube::Api<k8s_openapi::api::apps::v1::Deployment> =
        kube::Api::namespaced(state.kube.clone(), &target_ns);
    let k8s_deploy = deployments.get(deploy_name).await.unwrap();
    let replicas = k8s_deploy
        .spec
        .as_ref()
        .and_then(|s| s.replicas)
        .unwrap_or(1);
    assert_eq!(replicas, 0, "deployment should be scaled to 0 replicas");

    // Clean up
    let _ = deployments
        .delete(deploy_name, &kube::api::DeleteParams::default())
        .await;
}

/// Optimistic lock prevents double-processing of the same deployment.
#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn reconciler_optimistic_lock(pool: PgPool) {
    let (state, admin_token) = e2e_helpers::e2e_state(pool.clone()).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = admin_token.clone();

    let project_id = setup_deploy_project(
        &state,
        &app,
        &token,
        "recon-lock",
        "staging",
        "nginx:1.25-alpine",
    )
    .await;

    // Spawn TWO reconcilers to test optimistic locking
    let _reconciler1 = ReconcilerGuard::spawn(&state);
    let _reconciler2 = ReconcilerGuard::spawn(&state);

    // Wait for deployment to become healthy
    e2e_helpers::poll_deployment_status(&app, &token, project_id, "staging", "healthy", 120).await;

    // There should be exactly 1 success history entry (not 2)
    let (_, body) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/deployments/staging/history"),
    )
    .await;
    let items = body["items"].as_array().unwrap();
    let success_count = items
        .iter()
        .filter(|i| i["status"] == "success" && i["action"] == "deploy")
        .count();
    assert_eq!(
        success_count, 1,
        "optimistic lock should prevent double deploy, got {success_count} success entries"
    );

    // Clean up
    let deploy_name = "recon-lock-staging";
    let target_ns =
        platform::deployer::reconciler::target_namespace(&state.config, "recon-lock", "staging");
    let deployments: kube::Api<k8s_openapi::api::apps::v1::Deployment> =
        kube::Api::namespaced(state.kube.clone(), &target_ns);
    let _ = deployments
        .delete(deploy_name, &kube::api::DeleteParams::default())
        .await;
}

/// Preview cleanup deactivates expired preview deploy_targets.
#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn preview_expired_cleanup(pool: PgPool) {
    let (state, admin_token) = e2e_helpers::e2e_state(pool.clone()).await;
    let app = e2e_helpers::test_router(state.clone());

    let project_id =
        e2e_helpers::create_project(&app, &admin_token, "preview-expire", "private").await;

    // Insert an already-expired preview deploy target
    sqlx::query(
        r"INSERT INTO deploy_targets
           (project_id, name, environment, branch, branch_slug, ttl_hours, expires_at, is_active)
           VALUES ($1, 'preview-feature-old', 'preview', 'feature/old', 'feature-old', 1,
                   now() - interval '1 hour', true)",
    )
    .bind(project_id)
    .execute(&state.pool)
    .await
    .unwrap();

    // Spawn the reconciler (which handles preview TTL cleanup)
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let recon_state = state.clone();
    let handle = tokio::spawn(async move {
        platform::deployer::reconciler::run(recon_state, shutdown_rx).await;
    });

    // Wait for one cleanup cycle (reconciler runs every 10s)
    tokio::time::sleep(Duration::from_secs(15)).await;

    // Shut down reconciler
    let _ = shutdown_tx.send(());
    let _ = handle.await;

    // Expired preview should now be deactivated
    let is_active: Option<bool> = sqlx::query_scalar(
        "SELECT is_active FROM deploy_targets WHERE project_id = $1 AND branch_slug = 'feature-old'",
    )
    .bind(project_id)
    .fetch_optional(&state.pool)
    .await
    .unwrap();

    if let Some(active) = is_active {
        assert!(
            !active,
            "expired preview target should be deactivated (is_active=false)"
        );
    }
    // If row is None, it was cleaned up entirely — also acceptable
}

/// Multiple deployments across different environments are reconciled independently.
#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn reconciler_multi_env(pool: PgPool) {
    let (state, admin_token) = e2e_helpers::e2e_state(pool.clone()).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = admin_token.clone();

    let project_id = e2e_helpers::create_project(&app, &token, "recon-multi", "private").await;

    // Insert two deployments in different environments
    for (env, image) in [
        ("staging", "nginx:1.25-alpine"),
        ("production", "nginx:1.26-alpine"),
    ] {
        sqlx::query(
            r"INSERT INTO deployments (project_id, environment, image_ref, desired_status, current_status)
               VALUES ($1, $2, $3, 'active', 'pending')",
        )
        .bind(project_id)
        .bind(env)
        .bind(image)
        .execute(&state.pool)
        .await
        .unwrap();
    }

    let _reconciler = ReconcilerGuard::spawn(&state);

    // Both should reach healthy
    e2e_helpers::poll_deployment_status(&app, &token, project_id, "staging", "healthy", 120).await;
    e2e_helpers::poll_deployment_status(&app, &token, project_id, "production", "healthy", 120)
        .await;

    // Verify both have correct images
    let (_, staging) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/deployments/staging"),
    )
    .await;
    assert_eq!(staging["image_ref"], "nginx:1.25-alpine");

    let (_, prod) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/deployments/production"),
    )
    .await;
    assert_eq!(prod["image_ref"], "nginx:1.26-alpine");

    // Clean up
    for (name, env) in [
        ("recon-multi-staging", "staging"),
        ("recon-multi-production", "production"),
    ] {
        let target_ns =
            platform::deployer::reconciler::target_namespace(&state.config, "recon-multi", env);
        let deployments: kube::Api<k8s_openapi::api::apps::v1::Deployment> =
            kube::Api::namespaced(state.kube.clone(), &target_ns);
        let _ = deployments
            .delete(name, &kube::api::DeleteParams::default())
            .await;
    }
}

/// Deployment history records correct actions for deploy and rollback.
#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn reconciler_history_actions(pool: PgPool) {
    let (state, admin_token) = e2e_helpers::e2e_state(pool.clone()).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = admin_token.clone();

    let project_id = setup_deploy_project(
        &state,
        &app,
        &token,
        "recon-hist",
        "staging",
        "nginx:1.25-alpine",
    )
    .await;

    let _reconciler = ReconcilerGuard::spawn(&state);

    // Wait for initial deploy
    e2e_helpers::poll_deployment_status(&app, &token, project_id, "staging", "healthy", 120).await;

    // Check history has a "deploy" action
    let (_, body) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/deployments/staging/history"),
    )
    .await;
    let items = body["items"].as_array().unwrap();
    assert!(
        items
            .iter()
            .any(|i| i["action"] == "deploy" && i["status"] == "success"),
        "should have a successful deploy action in history"
    );
    assert!(
        items.iter().any(|i| i["image_ref"] == "nginx:1.25-alpine"),
        "history should reference the deployed image"
    );

    // Clean up
    let deploy_name = "recon-hist-staging";
    let target_ns =
        platform::deployer::reconciler::target_namespace(&state.config, "recon-hist", "staging");
    let deployments: kube::Api<k8s_openapi::api::apps::v1::Deployment> =
        kube::Api::namespaced(state.kube.clone(), &target_ns);
    let _ = deployments
        .delete(deploy_name, &kube::api::DeleteParams::default())
        .await;
}
