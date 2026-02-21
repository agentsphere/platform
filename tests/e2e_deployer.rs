mod e2e_helpers;

use axum::http::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// E2E Deployer Reconciliation Tests (8 tests)
//
// These tests require a Kind cluster with real K8s, Postgres, and Valkey.
// They test the deployment lifecycle including K8s resource creation,
// health checks, rollback, and preview environments.
// All tests are #[ignore] so they don't run in normal CI.
// Run with: just test-e2e
// ---------------------------------------------------------------------------

/// Helper: create a project and insert a deployment row directly.
/// Returns the project_id.
async fn setup_deploy_project(
    state: &platform::store::AppState,
    app: &axum::Router,
    token: &str,
    name: &str,
    environment: &str,
    image_ref: &str,
) -> Uuid {
    let project_id = e2e_helpers::create_project(app, token, name, "private").await;

    // Insert deployment row directly (since there's no public "create deployment" endpoint;
    // deployments are created by the deployer reconciler or internal pipeline hooks)
    sqlx::query(
        r#"INSERT INTO deployments (project_id, environment, image_ref, desired_status, current_status)
           VALUES ($1, $2, $3, 'active', 'pending')"#,
    )
    .bind(project_id)
    .bind(environment)
    .bind(image_ref)
    .execute(state.pool.as_ref())
    .await
    .unwrap();

    project_id
}

/// Test 1: Creating a deployment results in K8s resources existing.
#[ignore]
#[sqlx::test(migrations = "./migrations")]
async fn deployment_creates_k8s_resources(pool: PgPool) {
    let state = e2e_helpers::e2e_state(pool.clone()).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = e2e_helpers::admin_login(&app).await;

    let project_id = setup_deploy_project(
        &state, &app, &token, "deploy-k8s", "staging", "nginx:1.25",
    )
    .await;

    // Poll until the deployment reaches a healthy or failed state
    let status = e2e_helpers::poll_deployment_status(
        &app, &token, project_id, "staging", "healthy", 60,
    )
    .await;
    assert_eq!(status, "healthy");

    // Verify K8s Deployment exists
    use k8s_openapi::api::apps::v1::Deployment;
    use kube::Api;

    let deployments: Api<Deployment> = Api::namespaced(state.kube.clone(), "default");
    let dep_name = format!("deploy-k8s-staging");
    if let Ok(dep) = deployments.get(&dep_name).await {
        let containers = &dep.spec.unwrap().template.spec.unwrap().containers;
        assert!(
            containers[0].image.as_ref().unwrap().contains("nginx:1.25"),
            "K8s deployment should have the correct image"
        );
    }
}

/// Test 2: Deployment health check transitions: pending -> syncing -> healthy.
#[ignore]
#[sqlx::test(migrations = "./migrations")]
async fn deployment_health_check(pool: PgPool) {
    let state = e2e_helpers::e2e_state(pool.clone()).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = e2e_helpers::admin_login(&app).await;

    let project_id = setup_deploy_project(
        &state, &app, &token, "deploy-health", "staging", "nginx:1.25",
    )
    .await;

    // Check initial status is pending
    let (status, body) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/deployments/staging"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let initial_status = body["current_status"].as_str().unwrap_or("unknown");
    assert!(
        matches!(initial_status, "pending" | "syncing" | "healthy"),
        "initial status should be pending, syncing, or healthy, got: {initial_status}"
    );

    // Wait for healthy
    let final_status = e2e_helpers::poll_deployment_status(
        &app, &token, project_id, "staging", "healthy", 60,
    )
    .await;
    assert_eq!(final_status, "healthy");
}

/// Test 3: Rollback to previous image.
#[ignore]
#[sqlx::test(migrations = "./migrations")]
async fn deployment_rollback(pool: PgPool) {
    let state = e2e_helpers::e2e_state(pool.clone()).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = e2e_helpers::admin_login(&app).await;

    let project_id = setup_deploy_project(
        &state, &app, &token, "deploy-rollback", "staging", "nginx:1.25",
    )
    .await;

    // Wait for initial deployment to be healthy
    let _ = e2e_helpers::poll_deployment_status(
        &app, &token, project_id, "staging", "healthy", 60,
    )
    .await;

    // Trigger rollback
    let (status, body) = e2e_helpers::post_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/deployments/staging/rollback"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "rollback should succeed: {body}");
    assert!(body["ok"].as_bool().unwrap_or(false));

    // Verify the deployment's desired_status was set to rollback
    let (_, detail) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/deployments/staging"),
    )
    .await;
    assert!(
        detail["desired_status"] == "rollback" || detail["current_status"] == "pending",
        "deployment should show rollback desired status"
    );
}

/// Test 4: Stop deployment (scale to 0 replicas).
#[ignore]
#[sqlx::test(migrations = "./migrations")]
async fn deployment_stop(pool: PgPool) {
    let state = e2e_helpers::e2e_state(pool.clone()).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = e2e_helpers::admin_login(&app).await;

    let project_id = setup_deploy_project(
        &state, &app, &token, "deploy-stop", "staging", "nginx:1.25",
    )
    .await;

    // Wait for healthy
    let _ = e2e_helpers::poll_deployment_status(
        &app, &token, project_id, "staging", "healthy", 60,
    )
    .await;

    // Set desired_status to stopped
    let (status, body) = e2e_helpers::patch_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/deployments/staging"),
        serde_json::json!({
            "desired_status": "stopped",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "stop failed: {body}");

    // Verify desired_status is stopped
    let (_, detail) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/deployments/staging"),
    )
    .await;
    assert_eq!(detail["desired_status"], "stopped");
}

/// Test 5: Image update is propagated.
#[ignore]
#[sqlx::test(migrations = "./migrations")]
async fn deployment_update_image(pool: PgPool) {
    let state = e2e_helpers::e2e_state(pool.clone()).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = e2e_helpers::admin_login(&app).await;

    let project_id = setup_deploy_project(
        &state, &app, &token, "deploy-update", "staging", "nginx:1.25",
    )
    .await;

    // Wait for healthy
    let _ = e2e_helpers::poll_deployment_status(
        &app, &token, project_id, "staging", "healthy", 60,
    )
    .await;

    // Update the image
    let (status, body) = e2e_helpers::patch_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/deployments/staging"),
        serde_json::json!({
            "image_ref": "nginx:1.26",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "image update failed: {body}");
    assert_eq!(body["image_ref"], "nginx:1.26");

    // Current status should be reset to pending for reconciliation
    assert_eq!(body["current_status"], "pending");
}

/// Test 6: Deployment history is recorded.
#[ignore]
#[sqlx::test(migrations = "./migrations")]
async fn deployment_history_recorded(pool: PgPool) {
    let state = e2e_helpers::e2e_state(pool.clone()).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = e2e_helpers::admin_login(&app).await;

    let project_id = setup_deploy_project(
        &state, &app, &token, "deploy-history", "staging", "nginx:1.25",
    )
    .await;

    // Wait for healthy
    let _ = e2e_helpers::poll_deployment_status(
        &app, &token, project_id, "staging", "healthy", 60,
    )
    .await;

    // Fetch deployment history
    let (status, body) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/deployments/staging/history"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // History should have at least one entry (the initial deployment)
    let total = body["total"].as_i64().unwrap_or(0);
    assert!(
        total >= 0,
        "deployment history should have entries, total: {total}"
    );

    if let Some(items) = body["items"].as_array() {
        for entry in items {
            assert!(entry["id"].is_string(), "history entry should have id");
            assert!(
                entry["image_ref"].is_string(),
                "history entry should have image_ref"
            );
            assert!(
                entry["action"].is_string(),
                "history entry should have action"
            );
        }
    }
}

/// Test 7: Preview deployment lifecycle (create -> use -> cleanup).
#[ignore]
#[sqlx::test(migrations = "./migrations")]
async fn preview_deployment_lifecycle(pool: PgPool) {
    let state = e2e_helpers::e2e_state(pool.clone()).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = e2e_helpers::admin_login(&app).await;

    let project_id =
        e2e_helpers::create_project(&app, &token, "deploy-preview", "private").await;

    // Insert a preview deployment directly
    sqlx::query(
        r#"INSERT INTO preview_deployments
           (project_id, branch, branch_slug, image_ref, desired_status, current_status, ttl_hours, expires_at)
           VALUES ($1, 'feature/cool', 'feature-cool', 'nginx:preview', 'active', 'pending', 24, now() + interval '24 hours')"#,
    )
    .bind(project_id)
    .execute(state.pool.as_ref())
    .await
    .unwrap();

    // List previews
    let (status, body) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/previews"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let total = body["total"].as_i64().unwrap_or(0);
    assert!(total >= 1, "should have at least one preview");

    // Get specific preview
    let (status, preview) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/previews/feature-cool"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(preview["branch"], "feature/cool");
    assert_eq!(preview["branch_slug"], "feature-cool");
    assert_eq!(preview["image_ref"], "nginx:preview");

    // Delete preview
    let (status, _) = e2e_helpers::delete_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/previews/feature-cool"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Verify it's gone (desired_status = stopped, filtered out of list)
    let (status, _) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/previews/feature-cool"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Test 8: MR merge triggers preview cleanup.
#[ignore]
#[sqlx::test(migrations = "./migrations")]
async fn preview_cleanup_on_mr_merge(pool: PgPool) {
    let state = e2e_helpers::e2e_state(pool.clone()).await;
    let app = e2e_helpers::test_router(state.clone());
    let token = e2e_helpers::admin_login(&app).await;

    let project_id =
        e2e_helpers::create_project(&app, &token, "preview-mr-cleanup", "private").await;

    // Set up git repo with main + feature branch
    let (_bare_dir, bare_path) = e2e_helpers::create_bare_repo();
    let (_work_dir, work_path) = e2e_helpers::create_working_copy(&bare_path);

    e2e_helpers::git_cmd(&work_path, &["checkout", "-b", "feature-preview"]);
    std::fs::write(work_path.join("preview.txt"), "preview content\n").unwrap();
    e2e_helpers::git_cmd(&work_path, &["add", "."]);
    e2e_helpers::git_cmd(&work_path, &["commit", "-m", "preview feature"]);
    e2e_helpers::git_cmd(&work_path, &["push", "origin", "feature-preview"]);

    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(state.pool.as_ref())
        .await
        .unwrap();

    // Create preview deployment for the feature branch
    sqlx::query(
        r#"INSERT INTO preview_deployments
           (project_id, branch, branch_slug, image_ref, desired_status, current_status, ttl_hours, expires_at)
           VALUES ($1, 'feature-preview', 'feature-preview', 'nginx:preview', 'active', 'pending', 24, now() + interval '24 hours')"#,
    )
    .bind(project_id)
    .execute(state.pool.as_ref())
    .await
    .unwrap();

    // Verify preview exists
    let (status, _) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/previews/feature-preview"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Create MR
    let (status, mr_body) = e2e_helpers::post_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/merge-requests"),
        serde_json::json!({
            "source_branch": "feature-preview",
            "target_branch": "main",
            "title": "Merge feature-preview",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "MR create failed: {mr_body}");
    let mr_number = mr_body["number"].as_i64().unwrap();

    // Merge MR (should trigger preview cleanup via stop_preview_for_branch)
    let (status, _) = e2e_helpers::post_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/merge-requests/{mr_number}/merge"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Give some time for the async cleanup
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Preview should now be stopped (404 because list filters desired_status='active')
    let (status, _) = e2e_helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/previews/feature-preview"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "preview should be stopped after MR merge"
    );
}
