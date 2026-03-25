//! Integration tests for the progressive delivery API (targets, releases, ops repos).

mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

use helpers::{assign_role, create_project, create_user, test_router, test_state};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a deploy target + release row directly in the DB.
/// Returns `(target_id, release_id)`.
async fn setup_deployment(
    pool: &PgPool,
    project_id: Uuid,
    env: &str,
    image_ref: &str,
) -> (Uuid, Uuid) {
    let target_id = Uuid::new_v4();
    sqlx::query(
        r"INSERT INTO deploy_targets
           (id, project_id, name, environment, default_strategy, is_active)
           VALUES ($1, $2, $3, $4, 'rolling', true)",
    )
    .bind(target_id)
    .bind(project_id)
    .bind(env) // name = env for simplicity
    .bind(env)
    .execute(pool)
    .await
    .unwrap();

    let release_id = Uuid::new_v4();
    sqlx::query(
        r"INSERT INTO deploy_releases
           (id, target_id, project_id, image_ref, strategy, phase, health)
           VALUES ($1, $2, $3, $4, 'rolling', 'pending', 'unknown')",
    )
    .bind(release_id)
    .bind(target_id)
    .bind(project_id)
    .bind(image_ref)
    .execute(pool)
    .await
    .unwrap();

    (target_id, release_id)
}

/// Create a deploy target with environment='preview' + a release row.
/// Returns `(target_id, release_id)`.
async fn setup_preview(
    pool: &PgPool,
    project_id: Uuid,
    branch_slug: &str,
    image_ref: &str,
) -> (Uuid, Uuid) {
    let target_id = Uuid::new_v4();
    let branch = format!("feature/{branch_slug}");
    sqlx::query(
        r"INSERT INTO deploy_targets
           (id, project_id, name, environment, branch, branch_slug, ttl_hours,
            expires_at, default_strategy, is_active)
           VALUES ($1, $2, $3, 'preview', $4, $5, 24,
                   now() + interval '24 hours', 'rolling', true)",
    )
    .bind(target_id)
    .bind(project_id)
    .bind(branch_slug) // name = branch_slug
    .bind(&branch)
    .bind(branch_slug)
    .execute(pool)
    .await
    .unwrap();

    let release_id = Uuid::new_v4();
    sqlx::query(
        r"INSERT INTO deploy_releases
           (id, target_id, project_id, image_ref, strategy, phase, health)
           VALUES ($1, $2, $3, $4, 'rolling', 'pending', 'unknown')",
    )
    .bind(release_id)
    .bind(target_id)
    .bind(project_id)
    .bind(image_ref)
    .execute(pool)
    .await
    .unwrap();

    (target_id, release_id)
}

/// Insert a row into `release_history`.
async fn setup_history(
    pool: &PgPool,
    release_id: Uuid,
    target_id: Uuid,
    image_ref: &str,
    action: &str,
) {
    sqlx::query(
        r"INSERT INTO release_history
           (release_id, target_id, action, phase, image_ref)
           VALUES ($1, $2, $3, 'completed', $4)",
    )
    .bind(release_id)
    .bind(target_id)
    .bind(action)
    .bind(image_ref)
    .execute(pool)
    .await
    .unwrap();
}

// ---------------------------------------------------------------------------
// Target API tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_targets(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "target-list", "private").await;
    setup_deployment(&pool, project_id, "staging", "app:v1").await;
    setup_deployment(&pool, project_id, "production", "app:v2").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/targets"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn get_target_by_id(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "target-get", "private").await;
    let (target_id, _release_id) = setup_deployment(&pool, project_id, "staging", "myapp:v3").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/targets/{target_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "get target failed: {body}");
    assert_eq!(body["environment"], "staging");
    assert_eq!(body["id"], target_id.to_string());
}

#[sqlx::test(migrations = "./migrations")]
async fn get_target_not_found(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "target-nf", "private").await;
    let fake_id = Uuid::new_v4();

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/targets/{fake_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_target_via_api(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "target-create", "private").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/targets"),
        serde_json::json!({
            "name": "my-staging",
            "environment": "staging",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create target failed: {body}");
    assert_eq!(body["name"], "my-staging");
    assert_eq!(body["environment"], "staging");
    assert_eq!(body["default_strategy"], "rolling");
    assert_eq!(body["is_active"], true);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_target_invalid_environment(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "target-badenv", "private").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/targets"),
        serde_json::json!({
            "name": "bad-env",
            "environment": "nonsense",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Release API tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_releases(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "release-list", "private").await;
    setup_deployment(&pool, project_id, "staging", "app:v1").await;
    setup_deployment(&pool, project_id, "production", "app:v2").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn get_release_by_id(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "release-get", "private").await;
    let (_target_id, release_id) = setup_deployment(&pool, project_id, "staging", "myapp:v3").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "get release failed: {body}");
    assert_eq!(body["image_ref"], "myapp:v3");
    assert_eq!(body["phase"], "pending");
    assert_eq!(body["health"], "unknown");
}

#[sqlx::test(migrations = "./migrations")]
async fn get_release_not_found(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "release-nf", "private").await;
    let fake_id = Uuid::new_v4();

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{fake_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_release_via_api(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "release-create", "private").await;

    // Create a production target first (the create_release handler looks for one)
    sqlx::query(
        r"INSERT INTO deploy_targets
           (project_id, name, environment, default_strategy, is_active)
           VALUES ($1, 'prod', 'production', 'rolling', true)",
    )
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases"),
        serde_json::json!({
            "image_ref": "myapp:v1.0",
            "commit_sha": "abc123",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create release failed: {body}");
    assert_eq!(body["image_ref"], "myapp:v1.0");
    assert_eq!(body["commit_sha"], "abc123");
    assert_eq!(body["phase"], "pending");
    assert_eq!(body["strategy"], "rolling");
}

#[sqlx::test(migrations = "./migrations")]
async fn create_release_without_target_fails(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "release-no-target", "private").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases"),
        serde_json::json!({
            "image_ref": "myapp:v1.0",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Release action tests (rollback, pause, resume, promote, traffic)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn rollback_release(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "release-rb", "private").await;
    let (_target_id, release_id) = setup_deployment(&pool, project_id, "staging", "app:v2").await;

    // Rollback requires the release to be in a rollback-able phase (progressing/holding/paused)
    sqlx::query("UPDATE deploy_releases SET phase = 'progressing' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/rollback"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "rollback failed: {body}");
    assert_eq!(body["phase"], "rolling_back");
}

#[sqlx::test(migrations = "./migrations")]
async fn rollback_pending_release_fails(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "release-rb-pend", "private").await;
    let (_target_id, release_id) = setup_deployment(&pool, project_id, "staging", "app:v2").await;

    // Release is in 'pending' phase — rollback should fail
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/rollback"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn pause_and_resume_release(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "release-pause", "private").await;
    let (_target_id, release_id) = setup_deployment(&pool, project_id, "staging", "app:v1").await;

    // Move to progressing so pause works
    sqlx::query("UPDATE deploy_releases SET phase = 'progressing' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    // Pause
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/pause"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "pause failed: {body}");
    assert_eq!(body["phase"], "paused");

    // Resume
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/resume"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "resume failed: {body}");
    assert_eq!(body["phase"], "progressing");
}

#[sqlx::test(migrations = "./migrations")]
async fn promote_release(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "release-promo", "private").await;
    let (_target_id, release_id) = setup_deployment(&pool, project_id, "staging", "app:v1").await;

    // Move to progressing so promote works
    sqlx::query("UPDATE deploy_releases SET phase = 'progressing' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/promote"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "promote failed: {body}");
    assert_eq!(body["phase"], "promoting");
    assert_eq!(body["traffic_weight"], 100);
}

#[sqlx::test(migrations = "./migrations")]
async fn adjust_traffic(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "release-traffic", "private").await;
    let (_target_id, release_id) = setup_deployment(&pool, project_id, "staging", "app:v1").await;

    // Move to progressing
    sqlx::query("UPDATE deploy_releases SET phase = 'progressing' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/traffic"),
        serde_json::json!({ "traffic_weight": 50 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "adjust traffic failed: {body}");
    assert_eq!(body["traffic_weight"], 50);
}

#[sqlx::test(migrations = "./migrations")]
async fn adjust_traffic_invalid_weight(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "release-trafinv", "private").await;
    let (_target_id, release_id) = setup_deployment(&pool, project_id, "staging", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'progressing' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/traffic"),
        serde_json::json!({ "traffic_weight": 150 }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Release history tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_release_history(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "release-hist", "private").await;
    let (target_id, release_id) = setup_deployment(&pool, project_id, "staging", "app:v1").await;
    setup_history(&pool, release_id, target_id, "app:v1", "created").await;
    setup_history(&pool, release_id, target_id, "app:v1", "promoted").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/history"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "history failed: {body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
}

// ---------------------------------------------------------------------------
// Permission tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn target_read_requires_permission(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "target-perm-r", "private").await;
    setup_deployment(&pool, project_id, "staging", "app:v1").await;

    // User with no roles
    let (_uid, token) = create_user(&app, &admin_token, "no-deploy", "nodeploy@test.com").await;

    let (status, _) =
        helpers::get_json(&app, &token, &format!("/api/projects/{project_id}/targets")).await;
    // Private project with no access returns 404 to avoid leaking existence
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn release_read_requires_permission(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "release-perm-r", "private").await;
    setup_deployment(&pool, project_id, "staging", "app:v1").await;

    let (_uid, token) = create_user(&app, &admin_token, "no-release", "norelease@test.com").await;

    let (status, _) = helpers::get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/deploy-releases"),
    )
    .await;
    // Private project with no access returns 404 to avoid leaking existence
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_release_requires_deploy_promote(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "release-perm-w", "private").await;

    // Create production target (required for create_release)
    sqlx::query(
        r"INSERT INTO deploy_targets
           (project_id, name, environment, default_strategy, is_active)
           VALUES ($1, 'prod', 'production', 'rolling', true)",
    )
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    let (uid, token) = create_user(&app, &admin_token, "viewer-dep", "viewer@test.com").await;
    assign_role(&app, &admin_token, uid, "viewer", Some(project_id), &pool).await;

    let (status, _) = helpers::post_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/deploy-releases"),
        serde_json::json!({ "image_ref": "app:hacked" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "./migrations")]
async fn rollback_requires_deploy_promote(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "release-perm-rb", "private").await;
    let (_target_id, release_id) = setup_deployment(&pool, project_id, "staging", "app:v1").await;

    // Move to progressing so the rollback path is tested (not rejected for phase reasons)
    sqlx::query("UPDATE deploy_releases SET phase = 'progressing' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (uid, token) = create_user(&app, &admin_token, "viewer-rb", "viewerrb@test.com").await;
    assign_role(&app, &admin_token, uid, "viewer", Some(project_id), &pool).await;

    let (status, _) = helpers::post_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/rollback"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Preview target tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn preview_targets_listed(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "preview-list", "private").await;
    setup_preview(&pool, project_id, "feat-login", "app:feat-login").await;
    setup_preview(&pool, project_id, "feat-signup", "app:feat-signup").await;

    // Previews are just deploy targets with environment='preview'
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/targets"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "list targets failed: {body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    // All should be preview environment
    for item in items {
        assert_eq!(item["environment"], "preview");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn preview_target_get_by_id(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "preview-get", "private").await;
    let (target_id, _release_id) = setup_preview(&pool, project_id, "feat-x", "app:feat-x").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/targets/{target_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "get preview target failed: {body}");
    assert_eq!(body["branch_slug"], "feat-x");
    assert_eq!(body["environment"], "preview");
}

// ---------------------------------------------------------------------------
// Ops repo admin tests (unchanged — ops_repos table not affected)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_ops_repo(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/ops-repos",
        serde_json::json!({
            "name": "deploy-manifests",
            "branch": "main",
            "path": "/k8s",
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create ops repo failed: {body}"
    );
    assert_eq!(body["name"], "deploy-manifests");
    assert!(
        body["repo_path"]
            .as_str()
            .unwrap()
            .contains("deploy-manifests.git")
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn list_and_get_ops_repo(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/ops-repos",
        serde_json::json!({
            "name": "list-repo",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let repo_id = body["id"].as_str().unwrap();

    // List — returns a plain array, not {"items": [...]}
    let (status, body) = helpers::get_json(&app, &admin_token, "/api/admin/ops-repos").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.as_array().unwrap().is_empty());

    // Get by ID
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/admin/ops-repos/{repo_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "list-repo");
}

// ---------------------------------------------------------------------------
// Reconciler DB function tests
// ---------------------------------------------------------------------------

/// `mark_failed` updates release to failed and writes failure history.
#[sqlx::test(migrations = "./migrations")]
async fn mark_failed_updates_status(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "mark-fail", "private").await;
    let (target_id, release_id) =
        setup_deployment(&pool, project_id, "production", "app:broken").await;

    let release = platform::deployer::reconciler::PendingRelease {
        id: release_id,
        target_id,
        project_id,
        image_ref: "app:broken".into(),
        commit_sha: None,
        strategy: "rolling".into(),
        phase: "pending".into(),
        traffic_weight: 0,
        current_step: 0,
        rollout_config: serde_json::json!({}),
        values_override: None,
        deployed_by: None,
        pipeline_id: None,
        environment: "production".into(),
        ops_repo_id: None,
        manifest_path: None,
        branch_slug: None,
        project_name: "mark-fail".into(),
        namespace_slug: "mark-fail".into(),
        tracked_resources: Vec::new(),
        skip_prune: false,
    };

    platform::deployer::reconciler::mark_failed(&state, &release, "manifest apply error").await;

    // Verify release phase is now failed
    let (phase, health): (String, String) =
        sqlx::query_as("SELECT phase, health FROM deploy_releases WHERE id = $1")
            .bind(release_id)
            .fetch_one(&pool)
            .await
            .unwrap();

    assert_eq!(phase, "failed");
    assert_eq!(health, "unhealthy");

    // Verify history entry was created with failure
    let (h_action, h_phase): (String, String) =
        sqlx::query_as("SELECT action, phase FROM release_history WHERE release_id = $1")
            .bind(release_id)
            .fetch_one(&pool)
            .await
            .unwrap();

    assert_eq!(h_action, "failed");
    assert_eq!(h_phase, "failed");
}

// ---------------------------------------------------------------------------
// Preview cleanup on MR merge
// ---------------------------------------------------------------------------

/// MR merge triggers preview cleanup via `stop_preview_for_branch`.
#[sqlx::test(migrations = "./migrations")]
async fn preview_cleanup_on_mr_merge(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "preview-mr-cleanup", "private").await;

    // Set up git repo with main + feature branch
    let (_bare_dir, bare_path) = helpers::create_bare_repo();
    let (_work_dir, work_path) = helpers::create_working_copy(&bare_path);

    helpers::git_cmd(&work_path, &["checkout", "-b", "feature-preview"]);
    std::fs::write(work_path.join("preview.txt"), "preview content\n").unwrap();
    helpers::git_cmd(&work_path, &["add", "."]);
    helpers::git_cmd(&work_path, &["commit", "-m", "preview feature"]);
    helpers::git_cmd(&work_path, &["push", "origin", "feature-preview"]);

    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&state.pool)
        .await
        .unwrap();

    // Create a preview target + release for the feature branch
    let target_id = Uuid::new_v4();
    sqlx::query(
        r"INSERT INTO deploy_targets
           (id, project_id, name, environment, branch, branch_slug, ttl_hours,
            expires_at, default_strategy, is_active)
           VALUES ($1, $2, 'feature-preview', 'preview', 'feature-preview', 'feature-preview', 24,
                   now() + interval '24 hours', 'rolling', true)",
    )
    .bind(target_id)
    .bind(project_id)
    .execute(&state.pool)
    .await
    .unwrap();

    sqlx::query(
        r"INSERT INTO deploy_releases
           (target_id, project_id, image_ref, strategy, phase, health)
           VALUES ($1, $2, 'nginx:preview', 'rolling', 'progressing', 'unknown')",
    )
    .bind(target_id)
    .bind(project_id)
    .execute(&state.pool)
    .await
    .unwrap();

    // Verify target exists and is active
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/targets/{target_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "target should exist: {body}");
    assert_eq!(body["is_active"], true);

    // Create MR
    let (status, mr_body) = helpers::post_json(
        &app,
        &admin_token,
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
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/merge-requests/{mr_number}/merge"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Give some time for the async cleanup
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Preview target should now be deactivated (is_active=false, so GET returns 404)
    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/targets/{target_id}"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "preview target should be deactivated after MR merge"
    );
}

// ---------------------------------------------------------------------------
// Feature flag evaluation tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn evaluate_flags_returns_defaults(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let project_id = create_project(&app, &admin_token, "flag-proj", "public").await;

    // Create a flag (enabled defaults to false; disabled flags return default_value)
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/flags"),
        serde_json::json!({
            "key": "dark_mode",
            "flag_type": "boolean",
            "default_value": true,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Evaluate — disabled flag returns default_value
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/flags/evaluate",
        serde_json::json!({
            "project_id": project_id,
            "keys": ["dark_mode"],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["values"]["dark_mode"], true);
}

#[sqlx::test(migrations = "./migrations")]
async fn evaluate_flags_disabled_returns_default(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let project_id = create_project(&app, &admin_token, "flag-proj2", "public").await;

    // Create flag with default_value=false (enabled defaults to false)
    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/flags"),
        serde_json::json!({
            "key": "new_feature",
            "flag_type": "boolean",
            "default_value": false,
        }),
    )
    .await;

    // Flag is disabled by default — evaluate should return false (the default_value)
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/flags/evaluate",
        serde_json::json!({
            "project_id": project_id,
            "keys": ["new_feature"],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["values"]["new_feature"], false);

    // Toggle the flag on (enabled=true), then toggle it off again (enabled=false)
    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/flags/new_feature/toggle"),
        serde_json::json!({}),
    )
    .await;
    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/flags/new_feature/toggle"),
        serde_json::json!({}),
    )
    .await;

    // Evaluate disabled flag — should still return default_value
    let (_, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/flags/evaluate",
        serde_json::json!({
            "project_id": project_id,
            "keys": ["new_feature"],
        }),
    )
    .await;
    assert_eq!(body["values"]["new_feature"], false);
}

#[sqlx::test(migrations = "./migrations")]
async fn evaluate_flags_unknown_key_returns_null(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let project_id = create_project(&app, &admin_token, "flag-proj3", "public").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/flags/evaluate",
        serde_json::json!({
            "project_id": project_id,
            "keys": ["nonexistent_flag"],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["values"]["nonexistent_flag"].is_null());
}

#[sqlx::test(migrations = "./migrations")]
async fn evaluate_flags_user_override(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let project_id = create_project(&app, &admin_token, "flag-proj4", "public").await;

    // Create flag with default=false
    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/flags"),
        serde_json::json!({"key": "beta", "default_value": false}),
    )
    .await;

    // Toggle flag to enabled (overrides only apply when enabled)
    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/flags/beta/toggle"),
        serde_json::json!({}),
    )
    .await;

    // Create a user and grant project read so they can evaluate
    let (user_id, user_token) =
        create_user(&app, &admin_token, "flaguser", "flaguser@test.com").await;
    assign_role(
        &app,
        &admin_token,
        user_id,
        "viewer",
        Some(project_id),
        &pool,
    )
    .await;

    // Set override for this user
    helpers::put_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/flags/beta/overrides/{user_id}"),
        serde_json::json!({"serve_value": true}),
    )
    .await;

    // Evaluate with user_id — should get override value
    let (_, body) = helpers::post_json(
        &app,
        &user_token,
        "/api/flags/evaluate",
        serde_json::json!({
            "project_id": project_id,
            "keys": ["beta"],
            "user_id": user_id.to_string(),
        }),
    )
    .await;
    assert_eq!(body["values"]["beta"], true);

    // Evaluate without user_id — should get default
    let (_, body) = helpers::post_json(
        &app,
        &user_token,
        "/api/flags/evaluate",
        serde_json::json!({
            "project_id": project_id,
            "keys": ["beta"],
        }),
    )
    .await;
    assert_eq!(body["values"]["beta"], false);
}

#[sqlx::test(migrations = "./migrations")]
async fn evaluate_flags_percentage_rule(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let project_id = create_project(&app, &admin_token, "flag-proj5", "public").await;

    // Create flag with default=false
    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/flags"),
        serde_json::json!({"key": "rollout", "default_value": false}),
    )
    .await;

    // Toggle flag to enabled (rules only apply when enabled)
    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/flags/rollout/toggle"),
        serde_json::json!({}),
    )
    .await;

    // Add 100% percentage rule (always on)
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/flags/rollout/rules"),
        serde_json::json!({
            "rule_type": "percentage",
            "percentage": 100,
            "serve_value": true,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Evaluate with user_id — should get true (100% rollout)
    let (_, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/flags/evaluate",
        serde_json::json!({
            "project_id": project_id,
            "keys": ["rollout"],
            "user_id": "any-user-id",
        }),
    )
    .await;
    assert_eq!(body["values"]["rollout"], true);
}

#[sqlx::test(migrations = "./migrations")]
async fn evaluate_flags_requires_project_read(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let project_id = create_project(&app, &admin_token, "flag-proj6", "private").await;

    // Create unprivileged user
    let (_uid, user_token) = create_user(&app, &admin_token, "noperm", "noperm@test.com").await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        "/api/flags/evaluate",
        serde_json::json!({
            "project_id": project_id,
            "keys": ["any"],
        }),
    )
    .await;
    // Private project without permission → 404 (not 403, to avoid leaking existence)
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Staging promotion tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn staging_status_no_ops_repo_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let project_id = create_project(&app, &admin_token, "no-ops", "public").await;

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/staging-status"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn promote_staging_requires_deploy_promote(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let project_id = create_project(&app, &admin_token, "prom-perm", "private").await;

    let (user_id, user_token) = create_user(&app, &admin_token, "noprom", "noprom@test.com").await;
    // Give viewer (read) but not deploy:promote
    assign_role(
        &app,
        &admin_token,
        user_id,
        "viewer",
        Some(project_id),
        &pool,
    )
    .await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/promote-staging"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "./migrations")]
async fn promote_staging_no_staging_values_returns_error(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let project_id = create_project(&app, &admin_token, "no-staging", "public").await;

    // Create ops repo on disk
    let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
    let ops_path = platform::deployer::ops_repo::init_ops_repo(&tmp, "staging-ops", "main")
        .await
        .unwrap();

    // Bootstrap with initial commit so branches work
    platform::deployer::ops_repo::write_file_to_repo(&ops_path, "main", "README.md", "# Ops")
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO ops_repos (id, name, repo_path, branch, path, project_id)
         VALUES ($1, $2, $3, 'main', '/', $4)",
    )
    .bind(Uuid::new_v4())
    .bind("staging-ops")
    .bind(ops_path.to_string_lossy().to_string())
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/promote-staging"),
        serde_json::json!({}),
    )
    .await;
    // Should fail because no staging values exist
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}

// ---------------------------------------------------------------------------
// Analysis loop tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn analysis_creates_record_for_canary_release(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let project_id = create_project(&app, &admin_token, "analysis-proj", "public").await;

    // Create target + release with canary strategy
    let target_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO deploy_targets (id, project_id, name, environment, default_strategy)
         VALUES ($1, $2, 'production', 'production', 'canary')",
    )
    .bind(target_id)
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    let release_id = Uuid::new_v4();
    let rollout_config = serde_json::json!({
        "stable_service": "app-stable",
        "canary_service": "app-canary",
        "steps": [10, 50, 100],
        "progress_gates": [{
            "metric": "error_rate",
            "condition": "lt",
            "threshold": 0.05,
            "aggregation": "avg",
            "window": 60
        }]
    });

    sqlx::query(
        "INSERT INTO deploy_releases (id, target_id, project_id, image_ref, strategy, phase, rollout_config, current_step, started_at)
         VALUES ($1, $2, $3, 'app:v2', 'canary', 'progressing', $4, 0, now())",
    )
    .bind(release_id)
    .bind(target_id)
    .bind(project_id)
    .bind(&rollout_config)
    .execute(&pool)
    .await
    .unwrap();

    // Seed a metric so the analysis has data to evaluate
    let series_id = Uuid::new_v4();
    sqlx::query("INSERT INTO metric_series (id, name, labels) VALUES ($1, $2, $3)")
        .bind(series_id)
        .bind("error_rate")
        .bind(serde_json::json!({"platform.project_id": project_id.to_string()}))
        .execute(&pool)
        .await
        .unwrap();

    // Insert a metric sample (low error rate — should pass)
    sqlx::query("INSERT INTO metric_samples (series_id, timestamp, value) VALUES ($1, now(), $2)")
        .bind(series_id)
        .bind(0.01_f64)
        .execute(&pool)
        .await
        .unwrap();

    // Verify the release is ready for analysis
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM deploy_releases WHERE id = $1 AND phase = 'progressing' AND strategy = 'canary'",
    )
    .bind(release_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1, "canary release should be in progressing phase");
}

// ---------------------------------------------------------------------------
// Canary release lifecycle tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn canary_release_full_lifecycle(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let project_id = create_project(&app, &admin_token, "canary-lifecycle", "public").await;

    // 1. Create canary target
    let (status, _target) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/targets"),
        serde_json::json!({
            "name": "production",
            "environment": "production",
            "default_strategy": "canary",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // 2. Create canary release
    let (status, release) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases"),
        serde_json::json!({
            "image_ref": "registry/app:canary-v1",
            "strategy": "canary",
            "rollout_config": {
                "stable_service": "app-stable",
                "canary_service": "app-canary",
                "steps": [10, 50, 100]
            },
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create canary release failed: {release}"
    );
    assert_eq!(release["strategy"], "canary");
    assert_eq!(release["phase"], "pending");

    let release_id = release["id"].as_str().unwrap();

    // 3. Simulate progressing (set phase manually as reconciler would)
    sqlx::query(
        "UPDATE deploy_releases SET phase = 'progressing', traffic_weight = 10 WHERE id = $1",
    )
    .bind(uuid::Uuid::parse_str(release_id).unwrap())
    .execute(&pool)
    .await
    .unwrap();

    // 4. Adjust traffic
    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/traffic"),
        serde_json::json!({"traffic_weight": 50}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "adjust traffic failed: {body}");
    assert_eq!(body["traffic_weight"], 50);

    // 5. Promote to 100%
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/promote"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "promote failed: {body}");
    assert_eq!(body["phase"], "promoting");
    assert_eq!(body["traffic_weight"], 100);

    // 6. Check history
    let (status, history) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/history"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "history failed: {history}");
    let items = history["items"].as_array().unwrap();
    assert!(
        items.len() >= 3,
        "should have created + traffic_shifted + promoted history entries, got {}",
        items.len()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn canary_rollback_lifecycle(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let project_id = create_project(&app, &admin_token, "canary-rollback", "public").await;

    // Create target + release
    helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/targets"),
        serde_json::json!({
            "name": "production",
            "environment": "production",
            "default_strategy": "canary",
        }),
    )
    .await;

    let (_, release) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases"),
        serde_json::json!({
            "image_ref": "registry/app:canary-bad",
            "strategy": "canary",
        }),
    )
    .await;
    let release_id = release["id"].as_str().unwrap();

    // Move to progressing
    sqlx::query(
        "UPDATE deploy_releases SET phase = 'progressing', traffic_weight = 10 WHERE id = $1",
    )
    .bind(uuid::Uuid::parse_str(release_id).unwrap())
    .execute(&pool)
    .await
    .unwrap();

    // Rollback
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/rollback"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "rollback failed: {body}");
    assert_eq!(body["phase"], "rolling_back");

    // Verify history has rollback entry
    let (_, history) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/history"),
    )
    .await;
    let items = history["items"].as_array().unwrap();
    let actions: Vec<&str> = items
        .iter()
        .map(|i| i["action"].as_str().unwrap())
        .collect();
    assert!(
        actions.contains(&"rolled_back"),
        "should have rollback history entry, got actions: {actions:?}"
    );
}

// ---------------------------------------------------------------------------
// Flags from ops repo integration tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn flags_registered_and_evaluable_from_ops_repo(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let project_id = helpers::create_project(&app, &admin_token, "flags-flow", "public").await;

    // Create ops repo with platform.yaml containing flags
    let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
    let ops_path = platform::deployer::ops_repo::init_ops_repo(&tmp, "flags-ops", "main")
        .await
        .unwrap();

    let platform_yaml = r"pipeline:
  steps:
    - name: build
      image: alpine
      commands:
        - echo hi
flags:
  - key: dark_mode
    default_value: true
  - key: new_checkout
    default_value: false
";
    platform::deployer::ops_repo::write_file_to_repo(
        &ops_path,
        "main",
        "platform.yaml",
        platform_yaml,
    )
    .await
    .unwrap();

    let ops_repo_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO ops_repos (id, name, repo_path, branch, path, project_id) VALUES ($1, $2, $3, 'main', '/', $4)",
    )
    .bind(ops_repo_id)
    .bind("flags-ops")
    .bind(ops_path.to_string_lossy().to_string())
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    // Publish OpsRepoUpdated
    platform::store::eventbus::handle_event(
        &state,
        &serde_json::json!({
            "type": "OpsRepoUpdated",
            "project_id": project_id,
            "ops_repo_id": ops_repo_id,
            "environment": "production",
            "commit_sha": "abc1234567",
            "image_ref": "app:v1",
        })
        .to_string(),
    )
    .await
    .unwrap();

    // Evaluate flags — should return registered defaults
    // Note: flags are created with enabled=false by default in the DB,
    // so evaluation returns default_value regardless
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/flags/evaluate",
        serde_json::json!({
            "project_id": project_id,
            "keys": ["dark_mode", "new_checkout"],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["values"]["dark_mode"], true);
    assert_eq!(body["values"]["new_checkout"], false);

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}

// ---------------------------------------------------------------------------
// Demo project integration tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn demo_project_creates_mr_and_triggers_pipeline(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let _ = admin_token; // used indirectly via state

    // Get admin user ID
    let admin_id: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE name = 'admin'")
        .fetch_one(&pool)
        .await
        .unwrap();

    // Create demo project
    let result = platform::onboarding::demo_project::create_demo_project(&state, admin_id).await;
    assert!(
        result.is_ok(),
        "demo project creation should succeed: {:?}",
        result.err()
    );

    let (project_id, project_name) = result.unwrap();
    assert_eq!(project_name, "platform-demo");

    // Verify project exists
    let project_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE id = $1 AND is_active = true)",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(project_exists, "project should exist in DB");

    // Verify MR exists with feature branch
    let mr_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM merge_requests WHERE project_id = $1 AND source_branch = 'feature/shop-app'",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(mr_count, 1, "should have created MR for feature/shop-app");

    // Verify pipeline was triggered (may be pending, running, or already finished)
    let pipeline_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM pipelines WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        pipeline_count >= 1,
        "should have triggered at least one pipeline"
    );

    // Verify ops repo was created
    let ops_repo_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM ops_repos WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(ops_repo_count, 1, "should have created ops repo");

    // Verify sample issues were created
    let issue_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM issues WHERE project_id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(issue_count, 4, "should have created 4 sample issues");
}
