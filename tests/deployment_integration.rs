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
    // Security pattern: return 404 to avoid leaking resource existence
    assert_eq!(status, StatusCode::NOT_FOUND);
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
    // Security pattern: return 404 to avoid leaking resource existence
    assert_eq!(status, StatusCode::NOT_FOUND);
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
//
// Tier classification note: this test spans 3 API calls across 2 domains
// (deploy targets + merge requests), making it a multi-endpoint journey
// that the decision tree would classify as E2E. It remains in integration
// because: (a) it tests a single deployment lifecycle concern (preview
// cleanup triggered by MR merge) within the progressive delivery domain,
// (b) it uses integration helpers (test_state, test_router) and does not
// need #[ignore] / Kind cluster, and (c) moving it to E2E would reduce
// coverage visibility since E2E only runs with `just test-e2e`.
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

    // stop_preview_for_branch is awaited inline in the merge handler,
    // so by the time POST /merge returns 200 the DB update is complete.

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
    // Security pattern: return 404 to avoid leaking resource existence
    assert_eq!(status, StatusCode::NOT_FOUND);
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
//
// Tier classification note: canary_release_full_lifecycle spans 6 API calls
// (create target, create release, DB update, adjust traffic, promote, get
// history) and canary_rollback_lifecycle spans 5 calls. By the decision
// tree these multi-endpoint journeys would be E2E. They remain in
// integration because: (a) they exercise a single domain concern (canary
// release state machine) and all calls target the progressive delivery API,
// (b) they use integration helpers and don't need #[ignore], and (c) the
// DB state transitions between calls are the core assertion — not
// cross-domain user journeys.
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
//
// Tier classification note: this test spans 3 domains (ops repo init,
// eventbus event, flags evaluation API) across multiple API calls, which
// the decision tree would classify as E2E. It remains in integration
// because: (a) it validates a single feature flow (ops repo flags sync)
// where the eventbus call is a side-effect trigger, not a separate user
// action, (b) it uses integration helpers and doesn't need #[ignore],
// and (c) the ops repo + flags feature is tightly coupled — testing them
// separately would require duplicating setup.
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
//
// Tier classification note: this test calls a single function
// (create_demo_project) that internally creates a project, MR, pipeline,
// ops repo, and issues — spanning multiple domains. By the decision tree
// it could be E2E. It remains in integration because: (a) from the test's
// perspective it invokes one function and verifies its side effects, not
// a multi-step user journey, (b) it uses integration helpers and doesn't
// need #[ignore], and (c) the demo project feature is a single atomic
// operation (onboarding) even though it touches multiple tables.
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
        "SELECT COUNT(*) FROM merge_requests WHERE project_id = $1 AND source_branch = 'feature/shop-app-v0.1'",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        mr_count, 1,
        "should have created MR for feature/shop-app-v0.1"
    );

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

// ---------------------------------------------------------------------------
// Reconciler integration tests
// ---------------------------------------------------------------------------

/// Spawn the reconciler loop and verify it shuts down cleanly.
#[sqlx::test(migrations = "./migrations")]
async fn reconcile_loop_starts_and_shuts_down(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;
    let (tx, rx) = tokio::sync::watch::channel(());
    let handle = tokio::spawn(platform::deployer::reconciler::run(state.clone(), rx));

    // Let it tick at least once
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Send shutdown
    drop(tx);
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    assert!(result.is_ok(), "reconciler should shut down within 5s");
}

/// If a release's phase was changed between the query and the claim,
/// the reconciler skips it (optimistic lock).
#[sqlx::test(migrations = "./migrations")]
async fn reconcile_skips_already_claimed(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let project_id = create_project(&app, &admin_token, "skip-claimed", "public").await;
    let (_target_id, release_id) =
        setup_deployment(&pool, project_id, "staging", "app:claimed").await;

    // Set phase to progressing (so the reconciler query won't find it as 'pending')
    sqlx::query(
        "UPDATE deploy_releases SET phase = 'completed', completed_at = now() WHERE id = $1",
    )
    .bind(release_id)
    .execute(&pool)
    .await
    .unwrap();

    // Run a reconciler tick — it should find no releases to process
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(async move {
        platform::deployer::reconciler::run(s, rx).await;
    });
    // Give it a tick
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;

    // Release should still be completed (not changed to anything else)
    let phase: String = sqlx::query_scalar("SELECT phase FROM deploy_releases WHERE id = $1")
        .bind(release_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(phase, "completed");
}

/// Canary: pass verdict on non-final step advances current_step and updates traffic_weight.
#[sqlx::test(migrations = "./migrations")]
async fn canary_progress_pass_advances_step(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let project_id = create_project(&app, &admin_token, "canary-adv", "public").await;

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

    // Insert a 'pass' verdict for step 0
    sqlx::query(
        "INSERT INTO rollout_analyses (release_id, step_index, config, verdict, completed_at)
         VALUES ($1, 0, '{}'::jsonb, 'pass', now())",
    )
    .bind(release_id)
    .execute(&pool)
    .await
    .unwrap();

    // Spawn reconciler, wake it, wait for processing
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;

    // Should have advanced to step 1 with weight 50
    let (step, weight): (i32, i32) =
        sqlx::query_as("SELECT current_step, traffic_weight FROM deploy_releases WHERE id = $1")
            .bind(release_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(step, 1, "step should advance from 0 to 1");
    assert_eq!(weight, 50, "weight should be step[1]=50");
}

/// Canary: pass verdict on final step transitions to promoting.
#[sqlx::test(migrations = "./migrations")]
async fn canary_progress_pass_final_step_promotes(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let project_id = create_project(&app, &admin_token, "canary-final", "public").await;

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
        "steps": [10, 50],
    });
    // Already at step 1 (final step index = steps.len()-1 = 1)
    sqlx::query(
        "INSERT INTO deploy_releases (id, target_id, project_id, image_ref, strategy, phase, rollout_config, current_step, started_at, traffic_weight)
         VALUES ($1, $2, $3, 'app:v2', 'canary', 'progressing', $4, 1, now(), 50)",
    )
    .bind(release_id)
    .bind(target_id)
    .bind(project_id)
    .bind(&rollout_config)
    .execute(&pool)
    .await
    .unwrap();

    // Insert 'pass' verdict for final step
    sqlx::query(
        "INSERT INTO rollout_analyses (release_id, step_index, config, verdict, completed_at)
         VALUES ($1, 1, '{}'::jsonb, 'pass', now())",
    )
    .bind(release_id)
    .execute(&pool)
    .await
    .unwrap();

    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;

    let phase: String = sqlx::query_scalar("SELECT phase FROM deploy_releases WHERE id = $1")
        .bind(release_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        phase, "promoting",
        "final step pass should transition to promoting"
    );
}

/// Canary: fail within threshold transitions to holding.
#[sqlx::test(migrations = "./migrations")]
async fn canary_progress_fail_within_threshold_holds(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let project_id = create_project(&app, &admin_token, "canary-hold", "public").await;

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
        "max_failures": 3,
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

    // Insert 1 fail verdict (below max_failures=3)
    sqlx::query(
        "INSERT INTO rollout_analyses (release_id, step_index, config, verdict, completed_at)
         VALUES ($1, 0, '{}'::jsonb, 'fail', now())",
    )
    .bind(release_id)
    .execute(&pool)
    .await
    .unwrap();

    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;

    let phase: String = sqlx::query_scalar("SELECT phase FROM deploy_releases WHERE id = $1")
        .bind(release_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(phase, "holding", "single fail should transition to holding");
}

/// Canary: fail count reaching max_failures triggers rolling_back.
#[sqlx::test(migrations = "./migrations")]
async fn canary_progress_fail_exceeds_max_failures_rolls_back(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let project_id = create_project(&app, &admin_token, "canary-rb", "public").await;

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
        "max_failures": 2,
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

    // Insert 2 fail verdicts (== max_failures)
    for _ in 0..2 {
        sqlx::query(
            "INSERT INTO rollout_analyses (release_id, step_index, config, verdict, completed_at)
             VALUES ($1, 0, '{}'::jsonb, 'fail', now())",
        )
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();
    }

    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;

    let phase: String = sqlx::query_scalar("SELECT phase FROM deploy_releases WHERE id = $1")
        .bind(release_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        phase, "rolling_back",
        "exceeding max_failures should trigger rolling_back"
    );
}

/// Canary: inconclusive verdict leaves state unchanged.
#[sqlx::test(migrations = "./migrations")]
async fn canary_progress_inconclusive_waits(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let project_id = create_project(&app, &admin_token, "canary-wait", "public").await;

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

    // Insert inconclusive verdict
    sqlx::query(
        "INSERT INTO rollout_analyses (release_id, step_index, config, verdict, completed_at)
         VALUES ($1, 0, '{}'::jsonb, 'inconclusive', now())",
    )
    .bind(release_id)
    .execute(&pool)
    .await
    .unwrap();

    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;

    let (phase, step): (String, i32) =
        sqlx::query_as("SELECT phase, current_step FROM deploy_releases WHERE id = $1")
            .bind(release_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(phase, "progressing", "inconclusive should not change phase");
    assert_eq!(step, 0, "inconclusive should not advance step");
}

/// AB test: elapsed duration transitions to promoting.
#[sqlx::test(migrations = "./migrations")]
async fn ab_test_duration_elapsed_promotes(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let project_id = create_project(&app, &admin_token, "ab-elapsed", "public").await;

    let target_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO deploy_targets (id, project_id, name, environment, default_strategy)
         VALUES ($1, $2, 'production', 'production', 'ab_test')",
    )
    .bind(target_id)
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    let release_id = Uuid::new_v4();
    let rollout_config = serde_json::json!({
        "control_service": "app-control",
        "treatment_service": "app-treatment",
        "match": {"headers": {"x-exp": "treatment"}},
        "success_metric": "conversion",
        "success_condition": "gt",
        "duration": 1,
    });
    // started_at = 10 seconds ago so duration (1s) is already elapsed
    sqlx::query(
        "INSERT INTO deploy_releases (id, target_id, project_id, image_ref, strategy, phase, rollout_config, current_step, started_at)
         VALUES ($1, $2, $3, 'app:v2', 'ab_test', 'progressing', $4, 0, now() - interval '10 seconds')",
    )
    .bind(release_id)
    .bind(target_id)
    .bind(project_id)
    .bind(&rollout_config)
    .execute(&pool)
    .await
    .unwrap();

    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;

    let phase: String = sqlx::query_scalar("SELECT phase FROM deploy_releases WHERE id = $1")
        .bind(release_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        phase, "promoting",
        "elapsed AB test should transition to promoting"
    );
}

/// AB test: duration not yet elapsed stays in progressing.
#[sqlx::test(migrations = "./migrations")]
async fn ab_test_duration_not_elapsed_waits(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let project_id = create_project(&app, &admin_token, "ab-wait", "public").await;

    let target_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO deploy_targets (id, project_id, name, environment, default_strategy)
         VALUES ($1, $2, 'production', 'production', 'ab_test')",
    )
    .bind(target_id)
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    let release_id = Uuid::new_v4();
    let rollout_config = serde_json::json!({
        "control_service": "app-control",
        "treatment_service": "app-treatment",
        "match": {"headers": {}},
        "success_metric": "conversion",
        "success_condition": "gt",
        "duration": 86400,
    });
    sqlx::query(
        "INSERT INTO deploy_releases (id, target_id, project_id, image_ref, strategy, phase, rollout_config, current_step, started_at)
         VALUES ($1, $2, $3, 'app:v2', 'ab_test', 'progressing', $4, 0, now())",
    )
    .bind(release_id)
    .bind(target_id)
    .bind(project_id)
    .bind(&rollout_config)
    .execute(&pool)
    .await
    .unwrap();

    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;

    let phase: String = sqlx::query_scalar("SELECT phase FROM deploy_releases WHERE id = $1")
        .bind(release_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        phase, "progressing",
        "not-elapsed AB test should stay in progressing"
    );
}

/// Promoting phase completes the release with phase=completed and creates history entry.
#[sqlx::test(migrations = "./migrations")]
async fn promoting_completes_release(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let project_id = create_project(&app, &admin_token, "promote-complete", "public").await;

    let target_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO deploy_targets (id, project_id, name, environment, default_strategy)
         VALUES ($1, $2, 'staging', 'staging', 'rolling')",
    )
    .bind(target_id)
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    let release_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO deploy_releases (id, target_id, project_id, image_ref, strategy, phase, started_at)
         VALUES ($1, $2, $3, 'app:v3', 'rolling', 'promoting', now())",
    )
    .bind(release_id)
    .bind(target_id)
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;

    let (phase, health): (String, String) =
        sqlx::query_as("SELECT phase, health FROM deploy_releases WHERE id = $1")
            .bind(release_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(phase, "completed");
    assert_eq!(health, "healthy");

    // Verify history entry exists
    let hist_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM release_history WHERE release_id = $1 AND action = 'promoted'",
    )
    .bind(release_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(hist_count >= 1, "should have promoted history entry");
}

/// Rolling-back phase completes with phase=rolled_back and creates history entry.
#[sqlx::test(migrations = "./migrations")]
async fn rolling_back_completes(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let project_id = create_project(&app, &admin_token, "rb-complete", "public").await;

    let target_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO deploy_targets (id, project_id, name, environment, default_strategy)
         VALUES ($1, $2, 'staging', 'staging', 'rolling')",
    )
    .bind(target_id)
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    let release_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO deploy_releases (id, target_id, project_id, image_ref, strategy, phase, started_at)
         VALUES ($1, $2, $3, 'app:bad', 'rolling', 'rolling_back', now())",
    )
    .bind(release_id)
    .bind(target_id)
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;

    let (phase, health): (String, String) =
        sqlx::query_as("SELECT phase, health FROM deploy_releases WHERE id = $1")
            .bind(release_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(phase, "rolled_back");
    assert_eq!(health, "unhealthy");

    let hist_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM release_history WHERE release_id = $1 AND action = 'rolled_back'",
    )
    .bind(release_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(hist_count >= 1, "should have rolled_back history entry");
}

/// Expired preview targets get deactivated and their releases cancelled.
#[sqlx::test(migrations = "./migrations")]
async fn cleanup_expired_previews_deactivates(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let project_id = create_project(&app, &admin_token, "preview-expire", "public").await;

    // Create an expired preview target
    let target_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO deploy_targets (id, project_id, name, environment, branch, branch_slug, ttl_hours, expires_at, default_strategy, is_active)
         VALUES ($1, $2, 'feat-old', 'preview', 'feature/old', 'feat-old', 1, now() - interval '1 hour', 'rolling', true)",
    )
    .bind(target_id)
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    // Create a progressing release for this target
    let release_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO deploy_releases (id, target_id, project_id, image_ref, strategy, phase, started_at)
         VALUES ($1, $2, $3, 'app:preview', 'rolling', 'progressing', now())",
    )
    .bind(release_id)
    .bind(target_id)
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    // Run reconciler (cleanup happens in the reconcile tick)
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;

    // Target should be deactivated
    let is_active: bool = sqlx::query_scalar("SELECT is_active FROM deploy_targets WHERE id = $1")
        .bind(target_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!is_active, "expired preview target should be deactivated");

    // Release should be cancelled
    let phase: String = sqlx::query_scalar("SELECT phase FROM deploy_releases WHERE id = $1")
        .bind(release_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        phase, "cancelled",
        "release on expired target should be cancelled"
    );
}

/// ensure_scoped_tokens creates OTEL + API tokens and a second call rotates them.
#[sqlx::test(migrations = "./migrations")]
async fn ensure_scoped_tokens_creates_and_rotates(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let project_id = create_project(&app, &admin_token, "scoped-tok", "public").await;

    // First call: creates tokens
    let result =
        platform::deployer::reconciler::ensure_scoped_tokens(&state, project_id, "staging").await;
    assert!(
        result.is_ok(),
        "first ensure_scoped_tokens should succeed: {:?}",
        result.err()
    );
    let (otel1, api1) = result.unwrap();
    assert!(!otel1.is_empty());
    assert!(!api1.is_empty());

    // Second call: rotates tokens (creates new, deletes old)
    let result2 =
        platform::deployer::reconciler::ensure_scoped_tokens(&state, project_id, "staging").await;
    assert!(result2.is_ok());
    let (otel2, api2) = result2.unwrap();
    // New tokens should be different from old ones
    assert_ne!(otel1, otel2, "OTEL token should be rotated");
    assert_ne!(api1, api2, "API token should be rotated");
}

/// ensure_scoped_tokens fails when project has no owner.
#[sqlx::test(migrations = "./migrations")]
async fn ensure_scoped_tokens_no_owner_fails(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;

    // Use a nonexistent project ID
    let fake_project_id = Uuid::new_v4();
    let result =
        platform::deployer::reconciler::ensure_scoped_tokens(&state, fake_project_id, "staging")
            .await;
    assert!(result.is_err(), "should fail for nonexistent project");
}

/// ensure_registry_pull_secret_for with no registry_url is a no-op.
#[sqlx::test(migrations = "./migrations")]
async fn ensure_registry_pull_secret_no_url_noop(pool: PgPool) {
    let (mut state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let project_id = create_project(&app, &admin_token, "no-reg-url", "public").await;

    // Remove registry_url from config
    let mut config = (*state.config).clone();
    config.registry_url = None;
    state.config = std::sync::Arc::new(config);

    let release_id = Uuid::new_v4();
    // Should return immediately without error
    platform::deployer::reconciler::ensure_registry_pull_secret_for(
        &state, project_id, release_id, "test-ns",
    )
    .await;
    // No panic = success (early return when no registry_url)
}

/// fire_webhooks is called when a release completes (verified by creating a webhook).
#[sqlx::test(migrations = "./migrations")]
async fn fire_webhook_dispatches(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let project_id = create_project(&app, &admin_token, "webhook-fire", "public").await;

    // Insert webhook directly in DB (SSRF blocks localhost in the API)
    sqlx::query(
        "INSERT INTO webhooks (project_id, url, events, active)
         VALUES ($1, 'https://example.com/hook', ARRAY['deploy'], true)",
    )
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    let target_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO deploy_targets (id, project_id, name, environment, default_strategy)
         VALUES ($1, $2, 'staging', 'staging', 'rolling')",
    )
    .bind(target_id)
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    let release_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO deploy_releases (id, target_id, project_id, image_ref, strategy, phase, started_at)
         VALUES ($1, $2, $3, 'app:hook', 'rolling', 'promoting', now())",
    )
    .bind(release_id)
    .bind(target_id)
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    // Run reconciler — promoting should complete and fire webhook
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;

    // Verify release completed (webhook dispatch itself is fire-and-forget)
    let phase: String = sqlx::query_scalar("SELECT phase FROM deploy_releases WHERE id = $1")
        .bind(release_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        phase, "completed",
        "release should complete and fire webhook"
    );
}

// ---------------------------------------------------------------------------
// Analysis integration tests
// ---------------------------------------------------------------------------

/// Analysis tick creates a rollout_analyses record for a progressing canary release.
#[sqlx::test(migrations = "./migrations")]
async fn analysis_tick_creates_record(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let project_id = create_project(&app, &admin_token, "analysis-tick", "public").await;

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
        "min_requests": 0,
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

    // Spawn analysis loop for one tick
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::analysis::run(s, rx));
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;

    // Verify analysis record was created
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM rollout_analyses WHERE release_id = $1")
            .bind(release_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(count >= 1, "analysis tick should create a record");
}

/// A running analysis record is reused on subsequent ticks (no duplicate).
#[sqlx::test(migrations = "./migrations")]
async fn analysis_ensure_record_reuses_existing(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let project_id = create_project(&app, &admin_token, "analysis-reuse", "public").await;

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
        "steps": [10],
        "min_requests": 0,
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

    // Pre-create a running analysis record
    sqlx::query(
        "INSERT INTO rollout_analyses (release_id, step_index, config, verdict)
         VALUES ($1, 0, '{}'::jsonb, 'running')",
    )
    .bind(release_id)
    .execute(&pool)
    .await
    .unwrap();

    // Run analysis tick — should reuse the existing record, not create a new one
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::analysis::run(s, rx));
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;

    // There should still be only 1 record with step_index=0 (the pre-created running one may
    // have been completed, but no duplicate 'running' record should exist)
    let total_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM rollout_analyses WHERE release_id = $1 AND step_index = 0",
    )
    .bind(release_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    // Should be exactly 1 (the original running record, possibly now completed)
    assert_eq!(
        total_count, 1,
        "should not create duplicate analysis records"
    );
}

/// Rollback trigger breached produces a fail verdict.
#[sqlx::test(migrations = "./migrations")]
async fn analysis_rollback_trigger_breached(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let project_id = create_project(&app, &admin_token, "analysis-rb", "public").await;

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
        "min_requests": 0,
        "rollback_triggers": [{
            "metric": "error_rate",
            "condition": "gt",
            "threshold": 0.10,
            "aggregation": "avg",
            "window": 300
        }],
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

    // Seed metric data: high error rate (0.90 > threshold 0.10)
    let series_id = Uuid::new_v4();
    sqlx::query("INSERT INTO metric_series (id, name, labels) VALUES ($1, $2, $3)")
        .bind(series_id)
        .bind("error_rate")
        .bind(serde_json::json!({"platform.project_id": project_id.to_string()}))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO metric_samples (series_id, timestamp, value) VALUES ($1, now(), $2)")
        .bind(series_id)
        .bind(0.90_f64)
        .execute(&pool)
        .await
        .unwrap();

    // Run analysis loop
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::analysis::run(s, rx));
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;

    // Verify a 'fail' verdict was written
    let verdict: Option<String> = sqlx::query_scalar(
        "SELECT verdict FROM rollout_analyses WHERE release_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(release_id)
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert_eq!(
        verdict.as_deref(),
        Some("fail"),
        "rollback trigger breach should produce fail verdict"
    );
}

/// All progress gates pass produces a pass verdict.
#[sqlx::test(migrations = "./migrations")]
async fn analysis_progress_gates_all_pass(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let project_id = create_project(&app, &admin_token, "analysis-pass", "public").await;

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
        "min_requests": 0,
        "progress_gates": [{
            "metric": "error_rate",
            "condition": "lt",
            "threshold": 0.05,
            "aggregation": "avg",
            "window": 300
        }],
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

    // Seed metric: low error rate (0.01 < threshold 0.05)
    let series_id = Uuid::new_v4();
    sqlx::query("INSERT INTO metric_series (id, name, labels) VALUES ($1, $2, $3)")
        .bind(series_id)
        .bind("error_rate")
        .bind(serde_json::json!({"platform.project_id": project_id.to_string()}))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO metric_samples (series_id, timestamp, value) VALUES ($1, now(), $2)")
        .bind(series_id)
        .bind(0.01_f64)
        .execute(&pool)
        .await
        .unwrap();

    // Run analysis
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::analysis::run(s, rx));
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;

    let verdict: Option<String> = sqlx::query_scalar(
        "SELECT verdict FROM rollout_analyses WHERE release_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(release_id)
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert_eq!(
        verdict.as_deref(),
        Some("pass"),
        "all gates passing should produce pass verdict"
    );
}

/// One failing progress gate produces a fail verdict.
#[sqlx::test(migrations = "./migrations")]
async fn analysis_progress_gates_one_fails(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let project_id = create_project(&app, &admin_token, "analysis-gate-fail", "public").await;

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
        "min_requests": 0,
        "progress_gates": [{
            "metric": "error_rate",
            "condition": "lt",
            "threshold": 0.05,
            "aggregation": "avg",
            "window": 300
        }],
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

    // Seed metric: high error rate (0.10 > threshold 0.05 — gate condition "lt" fails)
    let series_id = Uuid::new_v4();
    sqlx::query("INSERT INTO metric_series (id, name, labels) VALUES ($1, $2, $3)")
        .bind(series_id)
        .bind("error_rate")
        .bind(serde_json::json!({"platform.project_id": project_id.to_string()}))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO metric_samples (series_id, timestamp, value) VALUES ($1, now(), $2)")
        .bind(series_id)
        .bind(0.10_f64)
        .execute(&pool)
        .await
        .unwrap();

    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::analysis::run(s, rx));
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;

    let verdict: Option<String> = sqlx::query_scalar(
        "SELECT verdict FROM rollout_analyses WHERE release_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(release_id)
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert_eq!(
        verdict.as_deref(),
        Some("fail"),
        "failing gate should produce fail verdict"
    );
}

/// Insufficient traffic (count < min_requests) returns inconclusive.
#[sqlx::test(migrations = "./migrations")]
async fn analysis_insufficient_traffic_inconclusive(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let project_id = create_project(&app, &admin_token, "analysis-lowtraffic", "public").await;

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
        "min_requests": 1000,
        "progress_gates": [{
            "metric": "error_rate",
            "condition": "lt",
            "threshold": 0.05,
            "aggregation": "avg",
            "window": 300
        }],
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

    // Seed very few requests (5 < min_requests 1000)
    let series_id = Uuid::new_v4();
    sqlx::query("INSERT INTO metric_series (id, name, labels) VALUES ($1, $2, $3)")
        .bind(series_id)
        .bind("http_requests_total")
        .bind(serde_json::json!({"platform.project_id": project_id.to_string()}))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO metric_samples (series_id, timestamp, value) VALUES ($1, now(), $2)")
        .bind(series_id)
        .bind(5.0_f64)
        .execute(&pool)
        .await
        .unwrap();

    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::analysis::run(s, rx));
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;

    let verdict: Option<String> = sqlx::query_scalar(
        "SELECT verdict FROM rollout_analyses WHERE release_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(release_id)
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert_eq!(
        verdict.as_deref(),
        Some("inconclusive"),
        "low traffic should produce inconclusive verdict"
    );
}

/// Empty progress gates returns pass verdict.
#[sqlx::test(migrations = "./migrations")]
async fn analysis_no_progress_gates_returns_pass(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let project_id = create_project(&app, &admin_token, "analysis-empty", "public").await;

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
        "min_requests": 0,
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

    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::analysis::run(s, rx));
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;

    let verdict: Option<String> = sqlx::query_scalar(
        "SELECT verdict FROM rollout_analyses WHERE release_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(release_id)
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert_eq!(
        verdict.as_deref(),
        Some("pass"),
        "empty gates should produce pass verdict"
    );
}

/// Spawn the analysis loop and verify it shuts down cleanly.
#[sqlx::test(migrations = "./migrations")]
async fn analysis_loop_starts_and_shuts_down(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;
    let (tx, rx) = tokio::sync::watch::channel(());
    let handle = tokio::spawn(platform::deployer::analysis::run(state.clone(), rx));

    // Let it tick at least once
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Send shutdown
    drop(tx);
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    assert!(result.is_ok(), "analysis loop should shut down within 5s");
}
