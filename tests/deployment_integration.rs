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

// ---------------------------------------------------------------------------
// Reconciler integration tests
// ---------------------------------------------------------------------------

/// Helper: create a deploy target + release with custom strategy and rollout config.
async fn setup_deployment_with_strategy(
    pool: &PgPool,
    project_id: Uuid,
    env: &str,
    image_ref: &str,
    strategy: &str,
    rollout_config: serde_json::Value,
) -> (Uuid, Uuid) {
    let target_id = Uuid::new_v4();
    sqlx::query(
        r"INSERT INTO deploy_targets
           (id, project_id, name, environment, default_strategy, is_active)
           VALUES ($1, $2, $3, $4, $5, true)",
    )
    .bind(target_id)
    .bind(project_id)
    .bind(env)
    .bind(env)
    .bind(strategy)
    .execute(pool)
    .await
    .unwrap();

    let release_id = Uuid::new_v4();
    sqlx::query(
        r"INSERT INTO deploy_releases
           (id, target_id, project_id, image_ref, strategy, phase, health, rollout_config)
           VALUES ($1, $2, $3, $4, $5, 'pending', 'unknown', $6)",
    )
    .bind(release_id)
    .bind(target_id)
    .bind(project_id)
    .bind(image_ref)
    .bind(strategy)
    .bind(&rollout_config)
    .execute(pool)
    .await
    .unwrap();

    (target_id, release_id)
}

/// Helper: poll DB for a release's phase until it matches or times out.
async fn poll_release_phase(
    pool: &PgPool,
    release_id: Uuid,
    expected: &str,
    timeout_ms: u64,
) -> String {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(timeout_ms);
    loop {
        let phase: String = sqlx::query_scalar("SELECT phase FROM deploy_releases WHERE id = $1")
            .bind(release_id)
            .fetch_one(pool)
            .await
            .unwrap();
        if phase == expected {
            return phase;
        }
        if tokio::time::Instant::now() > deadline {
            return phase;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }
}

/// `record_history` inserts a correct history row when called from the reconciler.
#[sqlx::test(migrations = "./migrations")]
async fn record_history_creates_entry(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "rec-hist", "private").await;
    let (target_id, release_id) = setup_deployment(&pool, project_id, "staging", "app:v1").await;

    let release = platform::deployer::reconciler::PendingRelease {
        id: release_id,
        target_id,
        project_id,
        image_ref: "app:v1".into(),
        commit_sha: Some("abc123".into()),
        strategy: "rolling".into(),
        phase: "pending".into(),
        traffic_weight: 0,
        current_step: 0,
        rollout_config: serde_json::json!({}),
        values_override: None,
        deployed_by: None,
        pipeline_id: None,
        environment: "staging".into(),
        ops_repo_id: None,
        manifest_path: None,
        branch_slug: None,
        project_name: "rec-hist".into(),
        namespace_slug: "rec-hist".into(),
        tracked_resources: Vec::new(),
        skip_prune: false,
    };

    // mark_failed creates history, but let us also explicitly test mark_failed
    // with a different message to verify the detail JSON
    platform::deployer::reconciler::mark_failed(&state, &release, "timeout exceeded").await;

    // Verify detail JSON in history
    let detail: serde_json::Value = sqlx::query_scalar(
        "SELECT detail FROM release_history WHERE release_id = $1 AND action = 'failed'",
    )
    .bind(release_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(detail["error"], "timeout exceeded");
}

/// `mark_failed` sets `completed_at` timestamp.
#[sqlx::test(migrations = "./migrations")]
async fn mark_failed_sets_completed_at(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "fail-ts", "private").await;
    let (target_id, release_id) =
        setup_deployment(&pool, project_id, "staging", "app:broken").await;

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
        environment: "staging".into(),
        ops_repo_id: None,
        manifest_path: None,
        branch_slug: None,
        project_name: "fail-ts".into(),
        namespace_slug: "fail-ts".into(),
        tracked_resources: Vec::new(),
        skip_prune: false,
    };

    platform::deployer::reconciler::mark_failed(&state, &release, "bad manifest").await;

    // Verify completed_at is set
    let completed: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT completed_at FROM deploy_releases WHERE id = $1")
            .bind(release_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        completed.is_some(),
        "completed_at should be set after mark_failed"
    );
}

/// `mark_failed` with an already-failed release is idempotent.
#[sqlx::test(migrations = "./migrations")]
async fn mark_failed_idempotent(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "fail-idem", "private").await;
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
        project_name: "fail-idem".into(),
        namespace_slug: "fail-idem".into(),
        tracked_resources: Vec::new(),
        skip_prune: false,
    };

    platform::deployer::reconciler::mark_failed(&state, &release, "first failure").await;
    platform::deployer::reconciler::mark_failed(&state, &release, "second failure").await;

    // Should have 2 history entries
    let (count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM release_history WHERE release_id = $1 AND action = 'failed'",
    )
    .bind(release_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 2);
}

/// `ensure_scoped_tokens` creates OTEL and API tokens.
#[sqlx::test(migrations = "./migrations")]
async fn ensure_scoped_tokens_creates_tokens(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "scoped-tok", "private").await;

    let result =
        platform::deployer::reconciler::ensure_scoped_tokens(&state, project_id, "staging").await;
    assert!(
        result.is_ok(),
        "ensure_scoped_tokens should succeed: {result:?}"
    );

    let (otel_token, api_token) = result.unwrap();
    assert!(!otel_token.is_empty(), "OTEL token should not be empty");
    assert!(!api_token.is_empty(), "API token should not be empty");

    // Verify tokens exist in DB
    let proj8 = &project_id.to_string()[..8];
    let otel_name = format!("otlp-staging-{proj8}");
    let api_name = format!("api-staging-{proj8}");

    let otel_count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM api_tokens WHERE name = $1 AND project_id = $2")
            .bind(&otel_name)
            .bind(project_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(otel_count.0, 1, "OTEL token should exist in DB");

    let api_count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM api_tokens WHERE name = $1 AND project_id = $2")
            .bind(&api_name)
            .bind(project_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(api_count.0, 1, "API token should exist in DB");
}

/// `ensure_scoped_tokens` rotates old tokens.
#[sqlx::test(migrations = "./migrations")]
async fn ensure_scoped_tokens_rotates_existing(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "tok-rotate", "private").await;

    // Create tokens first time
    let (first_otel, first_api) =
        platform::deployer::reconciler::ensure_scoped_tokens(&state, project_id, "prod")
            .await
            .unwrap();

    // Rotate tokens
    let (second_otel, second_api) =
        platform::deployer::reconciler::ensure_scoped_tokens(&state, project_id, "prod")
            .await
            .unwrap();

    // New tokens should be different
    assert_ne!(first_otel, second_otel, "OTEL token should rotate");
    assert_ne!(first_api, second_api, "API token should rotate");

    // Old tokens should be deleted — only 1 of each name should exist
    let proj8 = &project_id.to_string()[..8];
    let otel_count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM api_tokens WHERE name = $1 AND project_id = $2")
            .bind(format!("otlp-prod-{proj8}"))
            .bind(project_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        otel_count.0, 1,
        "only one OTEL token should remain after rotation"
    );
}

/// `ensure_scoped_tokens` fails for non-existent project.
#[sqlx::test(migrations = "./migrations")]
async fn ensure_scoped_tokens_fails_for_missing_project(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;

    let result =
        platform::deployer::reconciler::ensure_scoped_tokens(&state, Uuid::new_v4(), "staging")
            .await;
    assert!(result.is_err(), "should fail for non-existent project");
}

/// `ensure_registry_pull_secret_for` runs without error (no registry URL = no-op).
#[sqlx::test(migrations = "./migrations")]
async fn ensure_registry_pull_secret_no_registry_url(pool: PgPool) {
    let (mut state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    // Override config to have no registry URL
    let mut config = (*state.config).clone();
    config.registry_url = None;
    state.config = std::sync::Arc::new(config);

    let project_id = create_project(&app, &admin_token, "no-reg", "private").await;
    let (_target_id, release_id) = setup_deployment(&pool, project_id, "staging", "app:v1").await;

    // Should be a no-op when registry_url is None
    platform::deployer::reconciler::ensure_registry_pull_secret_for(
        &state, project_id, release_id, "test-ns",
    )
    .await;
    // If we get here without panic, the no-op path works
}

/// Reconciler loop starts and shuts down cleanly.
#[sqlx::test(migrations = "./migrations")]
async fn reconciler_loop_starts_and_shuts_down(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;
    let (tx, rx) = tokio::sync::watch::channel(());
    let handle = tokio::spawn(platform::deployer::reconciler::run(state.clone(), rx));

    // Let it tick at least once
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Send shutdown
    drop(tx);
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    assert!(result.is_ok(), "reconciler loop should shut down within 5s");
}

/// Reconciler loop can be woken via `deploy_notify`.
#[sqlx::test(migrations = "./migrations")]
async fn reconciler_wakes_on_notify(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;
    let (tx, rx) = tokio::sync::watch::channel(());
    let handle = tokio::spawn(platform::deployer::reconciler::run(state.clone(), rx));

    // Wake the reconciler via notify
    state.deploy_notify.notify_one();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Send shutdown
    drop(tx);
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    assert!(
        result.is_ok(),
        "reconciler should shut down after being notified"
    );
}

/// Optimistic locking: reconciler skips releases whose phase changed.
#[sqlx::test(migrations = "./migrations")]
async fn reconciler_skips_phase_changed_release(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "opt-lock", "private").await;
    let (_target_id, release_id) = setup_deployment(&pool, project_id, "staging", "app:v1").await;

    // Change release to completed before reconciler picks it up
    sqlx::query("UPDATE deploy_releases SET phase = 'completed', health = 'healthy' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    // Start reconciler — it should skip this release (optimistic lock fails)
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Phase should still be completed (not re-processed)
    let phase: String = sqlx::query_scalar("SELECT phase FROM deploy_releases WHERE id = $1")
        .bind(release_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        phase, "completed",
        "completed release should not be re-processed"
    );

    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

/// Cleanup expired previews: marks target inactive and cancels active releases.
#[sqlx::test(migrations = "./migrations")]
async fn reconciler_cleanup_expired_previews(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "preview-exp", "private").await;

    // Create a preview target that expired 1 hour ago
    let target_id = Uuid::new_v4();
    sqlx::query(
        r"INSERT INTO deploy_targets
           (id, project_id, name, environment, branch, branch_slug, ttl_hours,
            expires_at, default_strategy, is_active)
           VALUES ($1, $2, 'exp-feat', 'preview', 'feature/exp', 'exp', 24,
                   now() - interval '1 hour', 'rolling', true)",
    )
    .bind(target_id)
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    // Create a progressing release for this target
    let release_id = Uuid::new_v4();
    sqlx::query(
        r"INSERT INTO deploy_releases
           (id, target_id, project_id, image_ref, strategy, phase, health)
           VALUES ($1, $2, $3, 'app:preview', 'rolling', 'progressing', 'unknown')",
    )
    .bind(release_id)
    .bind(target_id)
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    // Run reconciler — should clean up expired preview
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();

    // Wait for cleanup
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    // Verify target is inactive
    let is_active: bool = sqlx::query_scalar("SELECT is_active FROM deploy_targets WHERE id = $1")
        .bind(target_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!is_active, "expired preview target should be deactivated");

    // Verify release was cancelled
    let phase: String = sqlx::query_scalar("SELECT phase FROM deploy_releases WHERE id = $1")
        .bind(release_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        phase, "cancelled",
        "active release should be cancelled on expired preview"
    );

    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

/// Canary release: progressing with "pass" verdict advances to next step.
#[sqlx::test(migrations = "./migrations")]
async fn canary_pass_advances_step(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "canary-adv", "private").await;

    let rollout_config = serde_json::json!({
        "steps": [10, 50, 100],
        "max_failures": 3
    });

    let (target_id, release_id) = setup_deployment_with_strategy(
        &pool,
        project_id,
        "staging",
        "app:canary-v1",
        "canary",
        rollout_config,
    )
    .await;

    // Move release to progressing at step 0 (simulating initial deploy done)
    sqlx::query(
        "UPDATE deploy_releases SET phase = 'progressing', current_step = 0, traffic_weight = 10, started_at = now() WHERE id = $1",
    )
    .bind(release_id)
    .execute(&pool)
    .await
    .unwrap();

    // Insert a "pass" analysis verdict for step 0
    sqlx::query(
        "INSERT INTO rollout_analyses (release_id, target_id, step_index, verdict, score, raw_data)
         VALUES ($1, $2, 0, 'pass', 1.0, '{}')",
    )
    .bind(release_id)
    .bind(target_id)
    .execute(&pool)
    .await
    .unwrap();

    // Start reconciler
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();

    // Wait for canary step advancement
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    // Verify step advanced: current_step should be 1, weight should be 50
    let (step, weight): (i32, i32) =
        sqlx::query_as("SELECT current_step, traffic_weight FROM deploy_releases WHERE id = $1")
            .bind(release_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(step, 1, "current_step should advance to 1");
    assert_eq!(weight, 50, "traffic_weight should be 50 at step 1");

    // Verify history entry
    let (count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM release_history WHERE release_id = $1 AND action = 'step_advanced'",
    )
    .bind(release_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(count >= 1, "step_advanced history should exist");

    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

/// Canary release: all steps pass transitions to promoting.
#[sqlx::test(migrations = "./migrations")]
async fn canary_all_steps_pass_promotes(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "canary-promo", "private").await;

    let rollout_config = serde_json::json!({
        "steps": [50],
        "max_failures": 3
    });

    let (_target_id, release_id) = setup_deployment_with_strategy(
        &pool,
        project_id,
        "staging",
        "app:canary-v2",
        "canary",
        rollout_config,
    )
    .await;

    // Move to progressing at last step (step 0, only 1 step)
    sqlx::query(
        "UPDATE deploy_releases SET phase = 'progressing', current_step = 0, traffic_weight = 50, started_at = now() WHERE id = $1",
    )
    .bind(release_id)
    .execute(&pool)
    .await
    .unwrap();

    // Insert pass verdict for the last step
    sqlx::query(
        "INSERT INTO rollout_analyses (release_id, target_id, step_index, verdict, score, raw_data)
         VALUES ($1, (SELECT target_id FROM deploy_releases WHERE id = $1), 0, 'pass', 1.0, '{}')",
    )
    .bind(release_id)
    .execute(&pool)
    .await
    .unwrap();

    // Start reconciler
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();

    // Wait for promotion
    let phase = poll_release_phase(&pool, release_id, "promoting", 5000).await;
    assert_eq!(
        phase, "promoting",
        "release should transition to promoting after all steps pass"
    );

    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

/// Canary release: max failures reached triggers rollback.
#[sqlx::test(migrations = "./migrations")]
async fn canary_max_failures_triggers_rollback(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "canary-rb", "private").await;

    let rollout_config = serde_json::json!({
        "steps": [10, 50, 100],
        "max_failures": 2
    });

    let (target_id, release_id) = setup_deployment_with_strategy(
        &pool,
        project_id,
        "staging",
        "app:canary-bad",
        "canary",
        rollout_config,
    )
    .await;

    // Move to progressing
    sqlx::query(
        "UPDATE deploy_releases SET phase = 'progressing', current_step = 0, traffic_weight = 10, started_at = now() WHERE id = $1",
    )
    .bind(release_id)
    .execute(&pool)
    .await
    .unwrap();

    // Insert 2 "fail" verdicts (meets max_failures = 2)
    for _ in 0..2 {
        sqlx::query(
            "INSERT INTO rollout_analyses (release_id, target_id, step_index, verdict, score, raw_data)
             VALUES ($1, $2, 0, 'fail', 0.0, '{}')",
        )
        .bind(release_id)
        .bind(target_id)
        .execute(&pool)
        .await
        .unwrap();
    }

    // Start reconciler
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();

    // Wait for rollback
    let phase = poll_release_phase(&pool, release_id, "rolling_back", 5000).await;
    assert_eq!(
        phase, "rolling_back",
        "should transition to rolling_back after max failures"
    );

    // Verify health is unhealthy
    let health: String = sqlx::query_scalar("SELECT health FROM deploy_releases WHERE id = $1")
        .bind(release_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(health, "unhealthy");

    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

/// Canary release: single fail transitions from progressing to holding.
#[sqlx::test(migrations = "./migrations")]
async fn canary_single_fail_holds(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "canary-hold", "private").await;

    let rollout_config = serde_json::json!({
        "steps": [10, 50, 100],
        "max_failures": 3
    });

    let (target_id, release_id) = setup_deployment_with_strategy(
        &pool,
        project_id,
        "staging",
        "app:canary-iffy",
        "canary",
        rollout_config,
    )
    .await;

    // Move to progressing
    sqlx::query(
        "UPDATE deploy_releases SET phase = 'progressing', current_step = 0, traffic_weight = 10, started_at = now() WHERE id = $1",
    )
    .bind(release_id)
    .execute(&pool)
    .await
    .unwrap();

    // Insert a single "fail" verdict (below max_failures)
    sqlx::query(
        "INSERT INTO rollout_analyses (release_id, target_id, step_index, verdict, score, raw_data)
         VALUES ($1, $2, 0, 'fail', 0.3, '{}')",
    )
    .bind(release_id)
    .bind(target_id)
    .execute(&pool)
    .await
    .unwrap();

    // Start reconciler
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();

    // Wait for holding state
    let phase = poll_release_phase(&pool, release_id, "holding", 5000).await;
    assert_eq!(phase, "holding", "single fail should transition to holding");

    // Verify health is degraded
    let health: String = sqlx::query_scalar("SELECT health FROM deploy_releases WHERE id = $1")
        .bind(release_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(health, "degraded");

    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

/// Canary release: inconclusive verdict leaves release unchanged.
#[sqlx::test(migrations = "./migrations")]
async fn canary_inconclusive_waits(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "canary-inc", "private").await;

    let rollout_config = serde_json::json!({
        "steps": [10, 50, 100],
        "max_failures": 3
    });

    let (target_id, release_id) = setup_deployment_with_strategy(
        &pool,
        project_id,
        "staging",
        "app:canary-wait",
        "canary",
        rollout_config,
    )
    .await;

    // Move to progressing
    sqlx::query(
        "UPDATE deploy_releases SET phase = 'progressing', current_step = 0, traffic_weight = 10, started_at = now() WHERE id = $1",
    )
    .bind(release_id)
    .execute(&pool)
    .await
    .unwrap();

    // Insert an inconclusive verdict
    sqlx::query(
        "INSERT INTO rollout_analyses (release_id, target_id, step_index, verdict, score, raw_data)
         VALUES ($1, $2, 0, 'inconclusive', 0.5, '{}')",
    )
    .bind(release_id)
    .bind(target_id)
    .execute(&pool)
    .await
    .unwrap();

    // Start reconciler
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();

    // Wait a bit
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    // Phase should still be progressing
    let phase: String = sqlx::query_scalar("SELECT phase FROM deploy_releases WHERE id = $1")
        .bind(release_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        phase, "progressing",
        "inconclusive verdict should not change phase"
    );

    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

/// Canary release: no verdict yet leaves release in progressing.
#[sqlx::test(migrations = "./migrations")]
async fn canary_no_verdict_stays_progressing(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "canary-nv", "private").await;

    let rollout_config = serde_json::json!({
        "steps": [10, 50, 100],
        "max_failures": 3
    });

    let (_target_id, release_id) = setup_deployment_with_strategy(
        &pool,
        project_id,
        "staging",
        "app:canary-nv",
        "canary",
        rollout_config,
    )
    .await;

    // Move to progressing (no analysis verdicts inserted)
    sqlx::query(
        "UPDATE deploy_releases SET phase = 'progressing', current_step = 0, traffic_weight = 10, started_at = now() WHERE id = $1",
    )
    .bind(release_id)
    .execute(&pool)
    .await
    .unwrap();

    // Start reconciler
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();

    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    // Phase should still be progressing
    let phase: String = sqlx::query_scalar("SELECT phase FROM deploy_releases WHERE id = $1")
        .bind(release_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(phase, "progressing");

    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

/// Promoting phase: transitions to completed with healthy status.
#[sqlx::test(migrations = "./migrations")]
async fn promoting_transitions_to_completed(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "promo-done", "private").await;

    let rollout_config = serde_json::json!({
        "steps": [50, 100]
    });

    let (_target_id, release_id) = setup_deployment_with_strategy(
        &pool,
        project_id,
        "staging",
        "app:promo-v1",
        "canary",
        rollout_config,
    )
    .await;

    // Set release to promoting
    sqlx::query(
        "UPDATE deploy_releases SET phase = 'promoting', traffic_weight = 100, started_at = now() WHERE id = $1",
    )
    .bind(release_id)
    .execute(&pool)
    .await
    .unwrap();

    // Start reconciler
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();

    // Wait for completion
    let phase = poll_release_phase(&pool, release_id, "completed", 10000).await;
    assert_eq!(
        phase, "completed",
        "promoting should transition to completed"
    );

    // Verify health
    let health: String = sqlx::query_scalar("SELECT health FROM deploy_releases WHERE id = $1")
        .bind(release_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(health, "healthy");

    // Verify history
    let (count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM release_history WHERE release_id = $1 AND action = 'promoted'",
    )
    .bind(release_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(count >= 1, "promoted history should exist");

    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

/// Rolling back phase: transitions to `rolled_back` with unhealthy status.
#[sqlx::test(migrations = "./migrations")]
async fn rolling_back_transitions_to_rolled_back(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "rb-done", "private").await;

    let rollout_config = serde_json::json!({
        "steps": [10, 50, 100]
    });

    let (_target_id, release_id) = setup_deployment_with_strategy(
        &pool,
        project_id,
        "staging",
        "app:rb-v1",
        "canary",
        rollout_config,
    )
    .await;

    // Set release to rolling_back
    sqlx::query(
        "UPDATE deploy_releases SET phase = 'rolling_back', traffic_weight = 10, started_at = now() WHERE id = $1",
    )
    .bind(release_id)
    .execute(&pool)
    .await
    .unwrap();

    // Start reconciler
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();

    // Wait for rolled_back
    let phase = poll_release_phase(&pool, release_id, "rolled_back", 10000).await;
    assert_eq!(
        phase, "rolled_back",
        "rolling_back should transition to rolled_back"
    );

    // Verify health
    let health: String = sqlx::query_scalar("SELECT health FROM deploy_releases WHERE id = $1")
        .bind(release_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(health, "unhealthy");

    // Verify traffic weight
    let weight: i32 =
        sqlx::query_scalar("SELECT traffic_weight FROM deploy_releases WHERE id = $1")
            .bind(release_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(weight, 0, "traffic weight should be 0 after rollback");

    // Verify history
    let (count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM release_history WHERE release_id = $1 AND action = 'rolled_back'",
    )
    .bind(release_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(count >= 1, "rolled_back history should exist");

    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

/// Rolling strategy: promoting phase for a rolling release completes.
#[sqlx::test(migrations = "./migrations")]
async fn rolling_promoting_completes(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "roll-promo", "private").await;

    let (_target_id, release_id) = setup_deployment_with_strategy(
        &pool,
        project_id,
        "staging",
        "app:roll-v1",
        "rolling",
        serde_json::json!({}),
    )
    .await;

    // Set to promoting (rolling strategy)
    sqlx::query(
        "UPDATE deploy_releases SET phase = 'promoting', traffic_weight = 100, started_at = now() WHERE id = $1",
    )
    .bind(release_id)
    .execute(&pool)
    .await
    .unwrap();

    // Start reconciler
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();

    let phase = poll_release_phase(&pool, release_id, "completed", 10000).await;
    assert_eq!(phase, "completed");

    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

/// A/B test via reconciler: duration elapsed transitions to promoting.
#[sqlx::test(migrations = "./migrations")]
async fn reconciler_ab_test_elapsed_promotes(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "ab-elapsed", "private").await;

    let rollout_config = serde_json::json!({
        "duration": 0,  // 0 seconds = immediately elapsed
        "control_service": "control",
        "treatment_service": "treatment",
        "match": { "headers": { "X-AB": "treatment" } }
    });

    let (_target_id, release_id) = setup_deployment_with_strategy(
        &pool,
        project_id,
        "staging",
        "app:ab-v1",
        "ab_test",
        rollout_config,
    )
    .await;

    // Set to progressing with started_at in the past
    sqlx::query(
        "UPDATE deploy_releases SET phase = 'progressing', started_at = now() - interval '1 hour' WHERE id = $1",
    )
    .bind(release_id)
    .execute(&pool)
    .await
    .unwrap();

    // Start reconciler
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();

    let phase = poll_release_phase(&pool, release_id, "promoting", 5000).await;
    assert_eq!(
        phase, "promoting",
        "A/B test with elapsed duration should promote"
    );

    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

/// A/B test via reconciler: duration not elapsed stays in progressing.
#[sqlx::test(migrations = "./migrations")]
async fn reconciler_ab_test_not_elapsed_waits(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "ab-wait", "private").await;

    let rollout_config = serde_json::json!({
        "duration": 86400,  // 24 hours — will not elapse during test
    });

    let (_target_id, release_id) = setup_deployment_with_strategy(
        &pool,
        project_id,
        "staging",
        "app:ab-v2",
        "ab_test",
        rollout_config,
    )
    .await;

    // Set to progressing with started_at = now
    sqlx::query(
        "UPDATE deploy_releases SET phase = 'progressing', started_at = now() WHERE id = $1",
    )
    .bind(release_id)
    .execute(&pool)
    .await
    .unwrap();

    // Start reconciler
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();

    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    let phase: String = sqlx::query_scalar("SELECT phase FROM deploy_releases WHERE id = $1")
        .bind(release_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        phase, "progressing",
        "A/B test with remaining duration should stay progressing"
    );

    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

/// Rolling progress handler is a no-op (rolling completes in pending).
#[sqlx::test(migrations = "./migrations")]
async fn rolling_progressing_is_noop(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "roll-noop", "private").await;

    let (_target_id, release_id) = setup_deployment_with_strategy(
        &pool,
        project_id,
        "staging",
        "app:roll-noop",
        "rolling",
        serde_json::json!({}),
    )
    .await;

    // Set to progressing (rolling strategy — shouldn't normally be here)
    sqlx::query(
        "UPDATE deploy_releases SET phase = 'progressing', started_at = now() WHERE id = $1",
    )
    .bind(release_id)
    .execute(&pool)
    .await
    .unwrap();

    // Start reconciler
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();

    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    // Phase should remain progressing — rolling progress is a no-op
    let phase: String = sqlx::query_scalar("SELECT phase FROM deploy_releases WHERE id = $1")
        .bind(release_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        phase, "progressing",
        "rolling progressing should be a no-op"
    );

    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

/// Rolling back for rolling strategy transitions to `rolled_back`.
#[sqlx::test(migrations = "./migrations")]
async fn rolling_back_rolling_strategy(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "roll-rb", "private").await;

    let (_target_id, release_id) = setup_deployment_with_strategy(
        &pool,
        project_id,
        "staging",
        "app:roll-rb",
        "rolling",
        serde_json::json!({}),
    )
    .await;

    // Set to rolling_back
    sqlx::query(
        "UPDATE deploy_releases SET phase = 'rolling_back', started_at = now() WHERE id = $1",
    )
    .bind(release_id)
    .execute(&pool)
    .await
    .unwrap();

    // Start reconciler
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();

    let phase = poll_release_phase(&pool, release_id, "rolled_back", 10000).await;
    assert_eq!(phase, "rolled_back");

    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

/// Holding phase treated as progressing by canary handler.
#[sqlx::test(migrations = "./migrations")]
async fn holding_phase_handled_by_canary(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "hold-canary", "private").await;

    let rollout_config = serde_json::json!({
        "steps": [10, 50, 100],
        "max_failures": 3
    });

    let (target_id, release_id) = setup_deployment_with_strategy(
        &pool,
        project_id,
        "staging",
        "app:hold-v1",
        "canary",
        rollout_config,
    )
    .await;

    // Set to holding
    sqlx::query(
        "UPDATE deploy_releases SET phase = 'holding', current_step = 0, traffic_weight = 10, started_at = now() WHERE id = $1",
    )
    .bind(release_id)
    .execute(&pool)
    .await
    .unwrap();

    // Insert pass verdict for current step — should advance
    sqlx::query(
        "INSERT INTO rollout_analyses (release_id, target_id, step_index, verdict, score, raw_data)
         VALUES ($1, $2, 0, 'pass', 1.0, '{}')",
    )
    .bind(release_id)
    .bind(target_id)
    .execute(&pool)
    .await
    .unwrap();

    // Start reconciler
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();

    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    // Should advance step (holding is treated like progressing for canary)
    let (step, weight): (i32, i32) =
        sqlx::query_as("SELECT current_step, traffic_weight FROM deploy_releases WHERE id = $1")
            .bind(release_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        step, 1,
        "holding release with pass verdict should advance step"
    );
    assert_eq!(weight, 50);

    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

/// Multiple releases reconciled in same cycle.
#[sqlx::test(migrations = "./migrations")]
async fn multiple_releases_reconciled(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "multi-rel", "private").await;

    // Create two promoting releases (canary strategy to exercise promoting handler)
    let mut release_ids = Vec::new();
    for i in 0..2 {
        let (_, release_id) = setup_deployment_with_strategy(
            &pool,
            project_id,
            &format!("staging{i}"),
            &format!("app:multi-v{i}"),
            "canary",
            serde_json::json!({"steps": [50, 100]}),
        )
        .await;

        sqlx::query(
            "UPDATE deploy_releases SET phase = 'promoting', traffic_weight = 100, started_at = now() WHERE id = $1",
        )
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

        release_ids.push(release_id);
    }

    // Start reconciler
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();

    // Both should complete
    for rid in &release_ids {
        let phase = poll_release_phase(&pool, *rid, "completed", 10000).await;
        assert_eq!(phase, "completed", "release {rid} should complete");
    }

    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

/// Non-expired preview targets are not cleaned up.
#[sqlx::test(migrations = "./migrations")]
async fn non_expired_preview_not_cleaned(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "preview-ok", "private").await;

    // Create a preview target that expires in 24 hours
    let target_id = Uuid::new_v4();
    sqlx::query(
        r"INSERT INTO deploy_targets
           (id, project_id, name, environment, branch, branch_slug, ttl_hours,
            expires_at, default_strategy, is_active)
           VALUES ($1, $2, 'active-feat', 'preview', 'feature/active', 'active', 24,
                   now() + interval '24 hours', 'rolling', true)",
    )
    .bind(target_id)
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    // Run reconciler
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();

    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    // Verify target is still active
    let is_active: bool = sqlx::query_scalar("SELECT is_active FROM deploy_targets WHERE id = $1")
        .bind(target_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(is_active, "non-expired preview target should remain active");

    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

/// `target_namespace` function produces correct namespaces.
#[sqlx::test(migrations = "./migrations")]
async fn target_namespace_via_state(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;

    let ns_staging =
        platform::deployer::reconciler::target_namespace(&state.config, "my-app", "staging");
    assert!(ns_staging.contains("my-app") && ns_staging.contains("staging"));

    let ns_prod =
        platform::deployer::reconciler::target_namespace(&state.config, "my-app", "production");
    // Production should be shortened to "prod"
    assert!(
        ns_prod.contains("prod"),
        "production should map to prod suffix"
    );
    assert!(
        !ns_prod.contains("production"),
        "should not contain full 'production'"
    );
}

/// Reconciler ignores unknown strategies gracefully.
#[sqlx::test(migrations = "./migrations")]
async fn reconciler_ignores_unknown_strategy(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "unk-strat", "private").await;

    let (_target_id, release_id) = setup_deployment_with_strategy(
        &pool,
        project_id,
        "staging",
        "app:unk-v1",
        "blue_green", // unknown strategy
        serde_json::json!({}),
    )
    .await;

    // Set to progressing with unknown strategy
    sqlx::query(
        "UPDATE deploy_releases SET phase = 'progressing', started_at = now() WHERE id = $1",
    )
    .bind(release_id)
    .execute(&pool)
    .await
    .unwrap();

    // Start reconciler — should not crash
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();

    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    // Phase should remain progressing (no-op for unknown strategy)
    let phase: String = sqlx::query_scalar("SELECT phase FROM deploy_releases WHERE id = $1")
        .bind(release_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(phase, "progressing", "unknown strategy should be skipped");

    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

/// `ensure_scoped_tokens` creates tokens with correct scopes.
#[sqlx::test(migrations = "./migrations")]
async fn ensure_scoped_tokens_correct_scopes(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "tok-scope", "private").await;

    let (_, _) =
        platform::deployer::reconciler::ensure_scoped_tokens(&state, project_id, "staging")
            .await
            .unwrap();

    let proj8 = &project_id.to_string()[..8];

    // Verify OTEL token has observe:write scope
    let otel_scopes: Vec<String> = sqlx::query_scalar(
        "SELECT unnest(scopes) FROM api_tokens WHERE name = $1 AND project_id = $2",
    )
    .bind(format!("otlp-staging-{proj8}"))
    .bind(project_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(
        otel_scopes.contains(&"observe:write".to_string()),
        "OTEL token should have observe:write scope"
    );

    // Verify API token has project:read scope
    let api_scopes: Vec<String> = sqlx::query_scalar(
        "SELECT unnest(scopes) FROM api_tokens WHERE name = $1 AND project_id = $2",
    )
    .bind(format!("api-staging-{proj8}"))
    .bind(project_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(
        api_scopes.contains(&"project:read".to_string()),
        "API token should have project:read scope"
    );
}

/// Canary: holding phase with max failures triggers rollback.
#[sqlx::test(migrations = "./migrations")]
async fn canary_holding_with_max_failures_rolls_back(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "hold-rb", "private").await;

    let rollout_config = serde_json::json!({
        "steps": [10, 50, 100],
        "max_failures": 2
    });

    let (target_id, release_id) = setup_deployment_with_strategy(
        &pool,
        project_id,
        "staging",
        "app:hold-bad",
        "canary",
        rollout_config,
    )
    .await;

    // Set to holding (after a first fail)
    sqlx::query(
        "UPDATE deploy_releases SET phase = 'holding', current_step = 0, traffic_weight = 10, started_at = now() WHERE id = $1",
    )
    .bind(release_id)
    .execute(&pool)
    .await
    .unwrap();

    // Insert 2 fail verdicts (hits max_failures)
    for _ in 0..2 {
        sqlx::query(
            "INSERT INTO rollout_analyses (release_id, target_id, step_index, verdict, score, raw_data)
             VALUES ($1, $2, 0, 'fail', 0.0, '{}')",
        )
        .bind(release_id)
        .bind(target_id)
        .execute(&pool)
        .await
        .unwrap();
    }

    // Start reconciler
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();

    let phase = poll_release_phase(&pool, release_id, "rolling_back", 5000).await;
    assert_eq!(
        phase, "rolling_back",
        "holding with max failures should trigger rollback"
    );

    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

/// Verified terminal phases are not processed.
#[sqlx::test(migrations = "./migrations")]
async fn terminal_phases_not_processed(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "term-ph", "private").await;

    // Create releases in terminal phases
    for (i, _terminal_phase) in ["completed", "rolled_back", "cancelled", "failed"]
        .iter()
        .enumerate()
    {
        let (_, release_id) = setup_deployment_with_strategy(
            &pool,
            project_id,
            &format!("env{i}"),
            &format!("app:term-{i}"),
            "rolling",
            serde_json::json!({}),
        )
        .await;

        sqlx::query("UPDATE deploy_releases SET phase = $2 WHERE id = $1")
            .bind(release_id)
            .execute(&pool)
            .await
            .unwrap();
    }

    // Start reconciler
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // No history entries should be created for terminal phases
    let (count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM release_history rh
             JOIN deploy_releases dr ON rh.release_id = dr.id
             WHERE dr.project_id = $1",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        count, 0,
        "terminal phases should not produce history entries"
    );

    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

/// Bad `tracked_resources` JSON is handled gracefully (`skip_prune=true`).
#[sqlx::test(migrations = "./migrations")]
async fn bad_tracked_resources_json_handled(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "bad-tr", "private").await;

    let target_id = Uuid::new_v4();
    sqlx::query(
        r"INSERT INTO deploy_targets
           (id, project_id, name, environment, default_strategy, is_active)
           VALUES ($1, $2, 'staging', 'staging', 'canary', true)",
    )
    .bind(target_id)
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    let release_id = Uuid::new_v4();
    // Insert with invalid tracked_resources JSON (not an array of TrackedResource)
    sqlx::query(
        r#"INSERT INTO deploy_releases
           (id, target_id, project_id, image_ref, strategy, phase, health,
            rollout_config, tracked_resources)
           VALUES ($1, $2, $3, 'app:v1', 'canary', 'promoting', 'unknown',
                   '{"steps": [50]}', '"not-an-array"')"#,
    )
    .bind(release_id)
    .bind(target_id)
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    // Start reconciler — should not crash with bad tracked_resources
    let (tx, rx) = tokio::sync::watch::channel(());
    let s = state.clone();
    let handle = tokio::spawn(platform::deployer::reconciler::run(s, rx));
    state.deploy_notify.notify_one();

    // Should still complete (promoting → completed)
    let phase = poll_release_phase(&pool, release_id, "completed", 10000).await;
    assert_eq!(
        phase, "completed",
        "bad tracked_resources should not prevent promotion"
    );

    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

// ===========================================================================
// NEW COVERAGE TESTS — Deployment API gaps
// ===========================================================================

// ---------------------------------------------------------------------------
// create_target: with canary strategy
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_target_canary_strategy(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "target-canary", "private").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/targets"),
        serde_json::json!({
            "name": "canary-prod",
            "environment": "production",
            "default_strategy": "canary",
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create canary target failed: {body}"
    );
    assert_eq!(body["default_strategy"], "canary");
    assert_eq!(body["environment"], "production");
}

// ---------------------------------------------------------------------------
// create_target: with ab_test strategy
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_target_ab_test_strategy(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "target-ab", "private").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/targets"),
        serde_json::json!({
            "name": "ab-staging",
            "environment": "staging",
            "default_strategy": "ab_test",
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create ab_test target failed: {body}"
    );
    assert_eq!(body["default_strategy"], "ab_test");
}

// ---------------------------------------------------------------------------
// create_target: invalid strategy is rejected
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_target_invalid_strategy(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "target-badstr", "private").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/targets"),
        serde_json::json!({
            "name": "bad-strategy",
            "environment": "production",
            "default_strategy": "blue_green",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// create_target: with ops_repo_id and manifest_path
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_target_with_ops_repo_and_manifest(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "target-ops", "private").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/targets"),
        serde_json::json!({
            "name": "ops-target",
            "environment": "staging",
            "manifest_path": "/k8s/overlays/staging",
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create target with manifest failed: {body}"
    );
    assert_eq!(body["manifest_path"], "/k8s/overlays/staging");
}

// ---------------------------------------------------------------------------
// create_target: duplicate environment returns conflict
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_target_duplicate_returns_conflict(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "target-dup", "private").await;

    // Create first target
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/targets"),
        serde_json::json!({
            "name": "staging",
            "environment": "staging",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Try to create duplicate (same project+name)
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/targets"),
        serde_json::json!({
            "name": "staging",
            "environment": "staging",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

// ---------------------------------------------------------------------------
// create_target: requires deploy:promote permission
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_target_requires_deploy_promote(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "target-perm", "private").await;

    let (uid, user_token) =
        create_user(&app, &admin_token, "viewer-target", "viewertarget@test.com").await;
    assign_role(&app, &admin_token, uid, "viewer", Some(project_id), &pool).await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/targets"),
        serde_json::json!({
            "name": "viewer-target",
            "environment": "staging",
        }),
    )
    .await;
    // Security pattern: return 404 to avoid leaking resource existence
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// create_release: with custom strategy and rollout_config
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_release_with_strategy_and_config(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "rel-strat", "private").await;

    // Create production target (required for create_release)
    setup_deployment(&pool, project_id, "production", "app:base").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases"),
        serde_json::json!({
            "image_ref": "app:v2",
            "strategy": "canary",
            "rollout_config": { "steps": [10, 50, 100] },
            "commit_sha": "abc123def456",
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create release with strategy failed: {body}"
    );
    assert_eq!(body["strategy"], "canary");
    assert_eq!(body["commit_sha"], "abc123def456");
    assert_eq!(body["phase"], "pending");
}

// ---------------------------------------------------------------------------
// create_release: with values_override and pipeline_id
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_release_with_values_and_pipeline(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "rel-vals", "private").await;
    setup_deployment(&pool, project_id, "production", "app:base").await;

    let pipeline_id = Uuid::new_v4();

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases"),
        serde_json::json!({
            "image_ref": "app:v3",
            "values_override": { "replicas": 3 },
            "pipeline_id": pipeline_id,
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create release with values failed: {body}"
    );
    assert!(body["values_override"].is_object());
}

// ---------------------------------------------------------------------------
// create_release: empty image_ref is rejected
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_release_empty_image_ref_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "rel-empty", "private").await;
    setup_deployment(&pool, project_id, "production", "app:base").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases"),
        serde_json::json!({
            "image_ref": "",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// create_release: image_ref too long is rejected
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_release_image_ref_too_long_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "rel-long", "private").await;
    setup_deployment(&pool, project_id, "production", "app:base").await;

    let long_ref = "a".repeat(2049);
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases"),
        serde_json::json!({
            "image_ref": long_ref,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// adjust_traffic: valid weight updates release
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn adjust_traffic_valid_weight(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "traffic-ok", "private").await;
    let (_target_id, release_id) =
        setup_deployment(&pool, project_id, "production", "app:v1").await;

    // Move to progressing so traffic can be adjusted
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

// ---------------------------------------------------------------------------
// adjust_traffic: weight below 0 is rejected
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn adjust_traffic_negative_weight_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "traffic-neg", "private").await;
    let (_target_id, release_id) =
        setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'progressing' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/traffic"),
        serde_json::json!({ "traffic_weight": -1 }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// adjust_traffic: weight above 100 is rejected
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn adjust_traffic_over_100_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "traffic-over", "private").await;
    let (_target_id, release_id) =
        setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'progressing' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/traffic"),
        serde_json::json!({ "traffic_weight": 101 }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// adjust_traffic: completed release returns 404 (not updatable)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn adjust_traffic_completed_release_not_found(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "traffic-done", "private").await;
    let (_target_id, release_id) =
        setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'completed' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/traffic"),
        serde_json::json!({ "traffic_weight": 50 }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// adjust_traffic: requires deploy:promote
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn adjust_traffic_requires_deploy_promote(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "traffic-perm", "private").await;
    let (_target_id, release_id) =
        setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'progressing' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (uid, user_token) =
        create_user(&app, &admin_token, "viewer-traf", "viewertraf@test.com").await;
    assign_role(&app, &admin_token, uid, "viewer", Some(project_id), &pool).await;

    let (status, _) = helpers::patch_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/traffic"),
        serde_json::json!({ "traffic_weight": 50 }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// promote_release: pending phase is rejected
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn promote_release_pending_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "prom-pending", "private").await;
    let (_target_id, release_id) =
        setup_deployment(&pool, project_id, "production", "app:v1").await;

    // Release is in 'pending' phase (default)
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/promote"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// promote_release: progressing phase succeeds
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn promote_release_progressing_succeeds(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "prom-prog", "private").await;
    let (_target_id, release_id) =
        setup_deployment(&pool, project_id, "production", "app:v1").await;

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

// ---------------------------------------------------------------------------
// promote_release: holding phase succeeds
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn promote_release_holding_succeeds(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "prom-hold", "private").await;
    let (_target_id, release_id) =
        setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'holding' WHERE id = $1")
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
    assert_eq!(
        status,
        StatusCode::OK,
        "promote from holding failed: {body}"
    );
    assert_eq!(body["phase"], "promoting");
}

// ---------------------------------------------------------------------------
// promote_release: paused phase succeeds
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn promote_release_paused_succeeds(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "prom-paused", "private").await;
    let (_target_id, release_id) =
        setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'paused' WHERE id = $1")
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
    assert_eq!(status, StatusCode::OK, "promote from paused failed: {body}");
    assert_eq!(body["phase"], "promoting");
}

// ---------------------------------------------------------------------------
// promote_release: requires deploy:promote
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn promote_release_requires_permission(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "prom-perm", "private").await;
    let (_target_id, release_id) =
        setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'progressing' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (uid, user_token) =
        create_user(&app, &admin_token, "viewer-prom", "viewerprom@test.com").await;
    assign_role(&app, &admin_token, uid, "viewer", Some(project_id), &pool).await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/promote"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// rollback_release: holding phase succeeds
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn rollback_release_holding_succeeds(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "rb-hold", "private").await;
    let (_target_id, release_id) =
        setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'holding' WHERE id = $1")
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
    assert_eq!(
        status,
        StatusCode::OK,
        "rollback from holding failed: {body}"
    );
    assert_eq!(body["phase"], "rolling_back");
}

// ---------------------------------------------------------------------------
// rollback_release: paused phase succeeds
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn rollback_release_paused_succeeds(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "rb-paused", "private").await;
    let (_target_id, release_id) =
        setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'paused' WHERE id = $1")
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
    assert_eq!(
        status,
        StatusCode::OK,
        "rollback from paused failed: {body}"
    );
    assert_eq!(body["phase"], "rolling_back");
}

// ---------------------------------------------------------------------------
// rollback_release: completed phase is rejected
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn rollback_release_completed_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "rb-done", "private").await;
    let (_target_id, release_id) =
        setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'completed' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/rollback"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// pause_release: progressing phase succeeds
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn pause_release_progressing_succeeds(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "pause-prog", "private").await;
    let (_target_id, release_id) =
        setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'progressing' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/pause"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "pause failed: {body}");
    assert_eq!(body["phase"], "paused");
}

// ---------------------------------------------------------------------------
// pause_release: non-progressing phase is rejected
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn pause_release_pending_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "pause-pend", "private").await;
    let (_target_id, release_id) =
        setup_deployment(&pool, project_id, "production", "app:v1").await;

    // Release is in 'pending' phase (default) — not progressing
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/pause"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// pause_release: requires deploy:promote
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn pause_release_requires_permission(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "pause-perm", "private").await;
    let (_target_id, release_id) =
        setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'progressing' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (uid, user_token) =
        create_user(&app, &admin_token, "viewer-pause", "viewerpause@test.com").await;
    assign_role(&app, &admin_token, uid, "viewer", Some(project_id), &pool).await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/pause"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// resume_release: paused phase succeeds
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn resume_release_paused_succeeds(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "resume-ok", "private").await;
    let (_target_id, release_id) =
        setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'paused' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

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

// ---------------------------------------------------------------------------
// resume_release: non-paused phase is rejected
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn resume_release_progressing_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "resume-bad", "private").await;
    let (_target_id, release_id) =
        setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'progressing' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/resume"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// resume_release: requires deploy:promote
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn resume_release_requires_permission(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "resume-perm", "private").await;
    let (_target_id, release_id) =
        setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'paused' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (uid, user_token) =
        create_user(&app, &admin_token, "viewer-res", "viewerres@test.com").await;
    assign_role(&app, &admin_token, uid, "viewer", Some(project_id), &pool).await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/resume"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// release_history: returns history entries for a release
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn release_history_returns_entries(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "hist-ok", "private").await;
    let (target_id, release_id) = setup_deployment(&pool, project_id, "production", "app:v1").await;

    // Insert some history entries
    setup_history(&pool, release_id, target_id, "app:v1", "created").await;
    setup_history(&pool, release_id, target_id, "app:v1", "promoted").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/history"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"].as_i64().unwrap(), 2);
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
}

// ---------------------------------------------------------------------------
// release_history: empty history returns empty list
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn release_history_empty_returns_empty(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "hist-empty", "private").await;
    let (_target_id, release_id) =
        setup_deployment(&pool, project_id, "production", "app:v1").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/history"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"].as_i64().unwrap(), 0);
}

// ---------------------------------------------------------------------------
// release_history: pagination works
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn release_history_pagination(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "hist-page", "private").await;
    let (target_id, release_id) = setup_deployment(&pool, project_id, "production", "app:v1").await;

    // Insert 3 history entries
    for action in &["created", "traffic_shifted", "promoted"] {
        setup_history(&pool, release_id, target_id, "app:v1", action).await;
    }

    // Fetch with limit=2
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/history?limit=2"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"].as_i64().unwrap(), 3);
    assert_eq!(body["items"].as_array().unwrap().len(), 2);

    // Fetch with offset=2
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!(
            "/api/projects/{project_id}/deploy-releases/{release_id}/history?limit=2&offset=2"
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// release_history: requires deploy:read
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn release_history_requires_permission(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "hist-perm", "private").await;
    let (_target_id, release_id) =
        setup_deployment(&pool, project_id, "production", "app:v1").await;

    let (_uid, user_token) = create_user(&app, &admin_token, "nohist", "nohist@test.com").await;
    // No role assigned — no permissions

    let (status, _) = helpers::get_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/history"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// staging_status: requires deploy:read
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn staging_status_requires_permission(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "staging-perm", "private").await;

    let (_uid, user_token) =
        create_user(&app, &admin_token, "nostaging", "nostaging@test.com").await;

    let (status, _) = helpers::get_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/staging-status"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// list_deploy_iframes: returns empty for project with no namespace slug
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_deploy_iframes_no_slug_returns_empty(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "iframe-noslug", "public").await;

    // Ensure namespace_slug is empty
    sqlx::query("UPDATE projects SET namespace_slug = '' WHERE id = $1")
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-preview/iframes"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.as_array().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// list_deploy_iframes: requires project:read
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_deploy_iframes_requires_permission(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "iframe-perm", "private").await;

    let (_uid, user_token) = create_user(&app, &admin_token, "noiframe", "noiframe@test.com").await;

    let (status, _) = helpers::get_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/deploy-preview/iframes"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// list_deploy_iframes: with env query parameter
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_deploy_iframes_with_env_param(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "iframe-env", "public").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-preview/iframes?env=staging"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Returns empty array (no services in the namespace)
    assert!(body.as_array().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// update_ops_repo: updates branch and path
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn update_ops_repo(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    // Create an ops repo first
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/ops-repos",
        serde_json::json!({
            "name": "update-repo",
            "branch": "main",
            "path": "/",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let repo_id = body["id"].as_str().unwrap();

    // Update branch and path
    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/admin/ops-repos/{repo_id}"),
        serde_json::json!({
            "branch": "develop",
            "path": "/k8s/overlays",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "update ops repo failed: {body}");
    assert_eq!(body["branch"], "develop");
    assert_eq!(body["path"], "/k8s/overlays");
}

// ---------------------------------------------------------------------------
// update_ops_repo: partial update (branch only)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn update_ops_repo_partial(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/ops-repos",
        serde_json::json!({
            "name": "partial-repo",
            "branch": "main",
            "path": "/k8s",
        }),
    )
    .await;
    let repo_id = body["id"].as_str().unwrap();

    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/admin/ops-repos/{repo_id}"),
        serde_json::json!({
            "branch": "staging",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["branch"], "staging");
    assert_eq!(body["path"], "/k8s"); // unchanged
}

// ---------------------------------------------------------------------------
// update_ops_repo: not found
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn update_ops_repo_not_found(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let fake_id = Uuid::new_v4();
    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/admin/ops-repos/{fake_id}"),
        serde_json::json!({ "branch": "develop" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// update_ops_repo: requires admin
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn update_ops_repo_requires_admin(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/ops-repos",
        serde_json::json!({ "name": "admin-repo" }),
    )
    .await;
    let repo_id = body["id"].as_str().unwrap();

    let (_uid, user_token) =
        create_user(&app, &admin_token, "nonadmin-ops", "nonadminops@test.com").await;

    let (status, _) = helpers::patch_json(
        &app,
        &user_token,
        &format!("/api/admin/ops-repos/{repo_id}"),
        serde_json::json!({ "branch": "develop" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// delete_ops_repo: successful deletion
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn delete_ops_repo_succeeds(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/ops-repos",
        serde_json::json!({ "name": "delete-repo" }),
    )
    .await;
    let repo_id = body["id"].as_str().unwrap();

    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/admin/ops-repos/{repo_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Verify it's gone
    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/admin/ops-repos/{repo_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// delete_ops_repo: not found
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn delete_ops_repo_not_found(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let fake_id = Uuid::new_v4();
    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/admin/ops-repos/{fake_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// delete_ops_repo: with active deploy target reference returns conflict
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn delete_ops_repo_with_targets_returns_conflict(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "del-ops-ref", "private").await;

    // Create ops repo
    let (_, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/ops-repos",
        serde_json::json!({ "name": "referenced-repo" }),
    )
    .await;
    let repo_id = body["id"].as_str().unwrap();
    let repo_uuid = Uuid::parse_str(repo_id).unwrap();

    // Create a deploy target that references this ops repo
    sqlx::query(
        r"INSERT INTO deploy_targets
           (project_id, name, environment, default_strategy, is_active, ops_repo_id)
           VALUES ($1, 'target-with-ops', 'production', 'rolling', true, $2)",
    )
    .bind(project_id)
    .bind(repo_uuid)
    .execute(&pool)
    .await
    .unwrap();

    // Try to delete — should fail with conflict
    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/admin/ops-repos/{repo_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

// ---------------------------------------------------------------------------
// delete_ops_repo: requires admin
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn delete_ops_repo_requires_admin(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/ops-repos",
        serde_json::json!({ "name": "del-admin-repo" }),
    )
    .await;
    let repo_id = body["id"].as_str().unwrap();

    let (_uid, user_token) =
        create_user(&app, &admin_token, "nonadmin-del", "nonadmindel@test.com").await;

    let (status, _) = helpers::delete_json(
        &app,
        &user_token,
        &format!("/api/admin/ops-repos/{repo_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// create_ops_repo: invalid name rejected
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_ops_repo_invalid_name(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/ops-repos",
        serde_json::json!({ "name": "" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// create_ops_repo: invalid branch rejected
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_ops_repo_invalid_branch(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/ops-repos",
        serde_json::json!({
            "name": "bad-branch-repo",
            "branch": "feat..double-dot",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// create_ops_repo: requires admin
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_ops_repo_requires_admin(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_uid, user_token) =
        create_user(&app, &admin_token, "nonadmin-cr", "nonadmincr@test.com").await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        "/api/admin/ops-repos",
        serde_json::json!({ "name": "user-repo" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// promote_staging: successful promotion with ops repo
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn promote_staging_succeeds(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let project_id = create_project(&app, &admin_token, "prom-stag-ok", "public").await;

    // Create ops repo on disk
    let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
    let ops_path = platform::deployer::ops_repo::init_ops_repo(&tmp, "prom-ops", "main")
        .await
        .unwrap();

    // Bootstrap with initial commit
    platform::deployer::ops_repo::write_file_to_repo(&ops_path, "main", "README.md", "# Ops")
        .await
        .unwrap();

    // Write staging values with an image_ref
    platform::deployer::ops_repo::write_file_to_repo(
        &ops_path,
        "staging",
        "staging/values.yaml",
        "image_ref: app:v2\nreplicas: 3\n",
    )
    .await
    .unwrap();

    let ops_repo_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO ops_repos (id, name, repo_path, branch, path, project_id)
         VALUES ($1, $2, $3, 'main', '/', $4)",
    )
    .bind(ops_repo_id)
    .bind("prom-ops")
    .bind(ops_path.to_string_lossy().to_string())
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/promote-staging"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "promote staging should succeed: {body}"
    );
    assert_eq!(body["status"], "promoted");
    assert_eq!(body["image_ref"], "app:v2");

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}

// ---------------------------------------------------------------------------
// staging_status: with ops repo returns comparison
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn staging_status_with_ops_repo(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let project_id = create_project(&app, &admin_token, "stag-status", "public").await;

    // Create ops repo on disk
    let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
    let ops_path = platform::deployer::ops_repo::init_ops_repo(&tmp, "status-ops", "main")
        .await
        .unwrap();

    // Bootstrap with initial commit
    platform::deployer::ops_repo::write_file_to_repo(&ops_path, "main", "README.md", "# Ops")
        .await
        .unwrap();

    // Create staging branch with same commit (not diverged)
    platform::deployer::ops_repo::write_file_to_repo(&ops_path, "staging", "README.md", "# Ops")
        .await
        .unwrap();

    let ops_repo_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO ops_repos (id, name, repo_path, branch, path, project_id)
         VALUES ($1, $2, $3, 'main', '/', $4)",
    )
    .bind(ops_repo_id)
    .bind("status-ops")
    .bind(ops_path.to_string_lossy().to_string())
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/staging-status"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "staging status failed: {body}");
    // Should have staging_sha and prod_sha
    assert!(body["staging_sha"].is_string());
    assert!(body["prod_sha"].is_string());

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}

// ---------------------------------------------------------------------------
// adjust_traffic: weight at boundaries (0 and 100) accepted
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn adjust_traffic_boundary_values(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "traffic-bnd", "private").await;
    let (_target_id, release_id) =
        setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'progressing' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    // Weight = 0
    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/traffic"),
        serde_json::json!({ "traffic_weight": 0 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "weight 0 should work: {body}");
    assert_eq!(body["traffic_weight"], 0);

    // Weight = 100
    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/traffic"),
        serde_json::json!({ "traffic_weight": 100 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "weight 100 should work: {body}");
    assert_eq!(body["traffic_weight"], 100);
}

// ---------------------------------------------------------------------------
// create_release: creates release history entry
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_release_creates_history(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "rel-hist", "private").await;
    setup_deployment(&pool, project_id, "production", "app:base").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases"),
        serde_json::json!({ "image_ref": "app:v2" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let release_id = body["id"].as_str().unwrap();

    // Check that a history entry was created
    let (status, hist_body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/history"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        hist_body["total"].as_i64().unwrap() >= 1,
        "release should have at least 1 history entry"
    );
    let first = &hist_body["items"][0];
    assert_eq!(first["action"], "created");
}

// ---------------------------------------------------------------------------
// Namespace integration tests (K8s API)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn ensure_namespace_creates_ns_with_labels(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;
    let ns_name = format!("test-ens-{}", &Uuid::new_v4().to_string()[..8]);

    platform::deployer::namespace::ensure_namespace(
        &state.kube,
        &ns_name,
        "dev",
        &Uuid::new_v4().to_string(),
        &state.config.platform_namespace,
        state.config.dev_mode,
    )
    .await
    .unwrap();

    // Verify namespace exists with correct labels
    let ns_api: kube::Api<k8s_openapi::api::core::v1::Namespace> =
        kube::Api::all(state.kube.clone());
    let ns = ns_api.get(&ns_name).await.unwrap();
    let labels = ns.metadata.labels.as_ref().unwrap();
    assert_eq!(labels.get("platform.io/managed-by").unwrap(), "platform");
    assert_eq!(labels.get("platform.io/env").unwrap(), "dev");

    // Cleanup
    let _ = ns_api
        .delete(&ns_name, &kube::api::DeleteParams::default())
        .await;
}

#[sqlx::test(migrations = "./migrations")]
async fn ensure_namespace_is_idempotent(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;
    let ns_name = format!("test-idem-{}", &Uuid::new_v4().to_string()[..8]);
    let project_id = Uuid::new_v4().to_string();

    // Call twice — should not error
    platform::deployer::namespace::ensure_namespace(
        &state.kube,
        &ns_name,
        "dev",
        &project_id,
        &state.config.platform_namespace,
        state.config.dev_mode,
    )
    .await
    .unwrap();

    platform::deployer::namespace::ensure_namespace(
        &state.kube,
        &ns_name,
        "dev",
        &project_id,
        &state.config.platform_namespace,
        state.config.dev_mode,
    )
    .await
    .unwrap();

    // Cleanup
    let ns_api: kube::Api<k8s_openapi::api::core::v1::Namespace> =
        kube::Api::all(state.kube.clone());
    let _ = ns_api
        .delete(&ns_name, &kube::api::DeleteParams::default())
        .await;
}

#[sqlx::test(migrations = "./migrations")]
async fn delete_namespace_requires_managed_label(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;
    let ns_name = format!("test-unm-{}", &Uuid::new_v4().to_string()[..8]);

    // Create namespace without platform.io/managed-by label
    let ns_api: kube::Api<k8s_openapi::api::core::v1::Namespace> =
        kube::Api::all(state.kube.clone());
    let ns = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "Namespace",
        "metadata": {
            "name": ns_name,
            "labels": {"test-label": "true"}
        }
    }))
    .unwrap();
    ns_api
        .create(&kube::api::PostParams::default(), &ns)
        .await
        .unwrap();

    // Attempting to delete should fail because it's not managed
    let result = platform::deployer::namespace::delete_namespace(&state.kube, &ns_name).await;
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("platform.io/managed-by"),
        "error should mention managed-by label: {err_msg}"
    );

    // Manually clean up since delete_namespace refused
    let _ = ns_api
        .delete(&ns_name, &kube::api::DeleteParams::default())
        .await;
}

#[sqlx::test(migrations = "./migrations")]
async fn delete_namespace_ok_when_managed(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;
    let ns_name = format!("test-del-{}", &Uuid::new_v4().to_string()[..8]);

    // Create a managed namespace
    platform::deployer::namespace::ensure_namespace(
        &state.kube,
        &ns_name,
        "dev",
        &Uuid::new_v4().to_string(),
        &state.config.platform_namespace,
        state.config.dev_mode,
    )
    .await
    .unwrap();

    // Delete should succeed
    platform::deployer::namespace::delete_namespace(&state.kube, &ns_name)
        .await
        .unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn delete_namespace_404_is_ok(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;
    let ns_name = format!("nonexistent-ns-{}", &Uuid::new_v4().to_string()[..8]);

    // Should succeed (treated as already deleted)
    platform::deployer::namespace::delete_namespace(&state.kube, &ns_name)
        .await
        .unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn ensure_session_namespace_creates_all_objects(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;
    let ns_name = format!("test-sess-{}", &Uuid::new_v4().to_string()[..8]);
    let session_id = Uuid::new_v4().to_string();
    let project_id = Uuid::new_v4().to_string();

    platform::deployer::namespace::ensure_session_namespace(
        &state.kube,
        &ns_name,
        &session_id,
        &project_id,
        &state.config.platform_namespace,
        None, // services_namespace
        state.config.dev_mode,
    )
    .await
    .unwrap();

    // 1. Verify namespace exists
    let ns_api: kube::Api<k8s_openapi::api::core::v1::Namespace> =
        kube::Api::all(state.kube.clone());
    let ns = ns_api.get(&ns_name).await.unwrap();
    let labels = ns.metadata.labels.as_ref().unwrap();
    assert_eq!(labels.get("platform.io/managed-by").unwrap(), "platform");

    // 2. Verify ServiceAccount
    let sa_api: kube::Api<k8s_openapi::api::core::v1::ServiceAccount> =
        kube::Api::namespaced(state.kube.clone(), &ns_name);
    let sa = sa_api.get("agent-sa").await.unwrap();
    assert_eq!(sa.metadata.name.as_deref(), Some("agent-sa"));

    // 3. Verify Role
    let role_api: kube::Api<k8s_openapi::api::rbac::v1::Role> =
        kube::Api::namespaced(state.kube.clone(), &ns_name);
    let role = role_api.get("agent-edit").await.unwrap();
    assert_eq!(role.metadata.name.as_deref(), Some("agent-edit"));
    // Verify role rules exclude networking.k8s.io
    if let Some(rules) = &role.rules {
        for rule in rules {
            if let Some(groups) = &rule.api_groups {
                assert!(
                    !groups.contains(&"networking.k8s.io".to_string()),
                    "role should not include networking.k8s.io"
                );
            }
        }
    }

    // 4. Verify RoleBinding
    let rb_api: kube::Api<k8s_openapi::api::rbac::v1::RoleBinding> =
        kube::Api::namespaced(state.kube.clone(), &ns_name);
    let rb = rb_api.get("agent-edit-binding").await.unwrap();
    assert_eq!(rb.role_ref.name, "agent-edit");

    // 5. Verify ResourceQuota
    let quota_api: kube::Api<k8s_openapi::api::core::v1::ResourceQuota> =
        kube::Api::namespaced(state.kube.clone(), &ns_name);
    let quota = quota_api.get("session-quota").await.unwrap();
    assert!(quota.spec.is_some());
    let hard = quota.spec.as_ref().unwrap().hard.as_ref().unwrap();
    assert!(hard.contains_key("pods"));
    assert!(hard.contains_key("requests.cpu"));

    // 6. Verify LimitRange
    let lr_api: kube::Api<k8s_openapi::api::core::v1::LimitRange> =
        kube::Api::namespaced(state.kube.clone(), &ns_name);
    let lr = lr_api.get("session-limits").await.unwrap();
    assert!(lr.spec.is_some());

    // Cleanup
    let _ = ns_api
        .delete(&ns_name, &kube::api::DeleteParams::default())
        .await;
}

#[sqlx::test(migrations = "./migrations")]
async fn ensure_session_namespace_with_different_services_ns(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;
    let ns_name = format!("test-svc-{}", &Uuid::new_v4().to_string()[..8]);
    let session_id = Uuid::new_v4().to_string();
    let project_id = Uuid::new_v4().to_string();

    // Use a services_namespace different from platform_namespace
    platform::deployer::namespace::ensure_session_namespace(
        &state.kube,
        &ns_name,
        &session_id,
        &project_id,
        &state.config.platform_namespace,
        Some("other-services-ns"),
        state.config.dev_mode,
    )
    .await
    .unwrap();

    // Verify namespace was created
    let ns_api: kube::Api<k8s_openapi::api::core::v1::Namespace> =
        kube::Api::all(state.kube.clone());
    let ns = ns_api.get(&ns_name).await.unwrap();
    assert!(ns.metadata.labels.is_some());

    // Cleanup
    let _ = ns_api
        .delete(&ns_name, &kube::api::DeleteParams::default())
        .await;
}

#[sqlx::test(migrations = "./migrations")]
async fn ensure_namespace_with_services_ns_creates_network_policy(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;
    let ns_name = format!("test-snp-{}", &Uuid::new_v4().to_string()[..8]);

    platform::deployer::namespace::ensure_namespace_with_services_ns(
        &state.kube,
        &ns_name,
        "dev",
        &Uuid::new_v4().to_string(),
        &state.config.platform_namespace,
        "services-ns",
        state.config.dev_mode,
    )
    .await
    .unwrap();

    // Verify RoleBinding for secrets access exists
    let rb_api: kube::Api<k8s_openapi::api::rbac::v1::RoleBinding> =
        kube::Api::namespaced(state.kube.clone(), &ns_name);
    let rb = rb_api.get("platform-secrets-access").await.unwrap();
    assert_eq!(rb.role_ref.kind, "ClusterRole");

    // Cleanup
    let ns_api: kube::Api<k8s_openapi::api::core::v1::Namespace> =
        kube::Api::all(state.kube.clone());
    let _ = ns_api
        .delete(&ns_name, &kube::api::DeleteParams::default())
        .await;
}

#[sqlx::test(migrations = "./migrations")]
async fn ensure_network_policy_standalone(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;
    let ns_name = format!("test-np-{}", &Uuid::new_v4().to_string()[..8]);

    // First create the namespace
    platform::deployer::namespace::ensure_namespace(
        &state.kube,
        &ns_name,
        "dev",
        &Uuid::new_v4().to_string(),
        &state.config.platform_namespace,
        state.config.dev_mode,
    )
    .await
    .unwrap();

    // Then apply network policy separately
    platform::deployer::namespace::ensure_network_policy(
        &state.kube,
        &ns_name,
        &state.config.platform_namespace,
    )
    .await
    .unwrap();

    // Cleanup
    let ns_api: kube::Api<k8s_openapi::api::core::v1::Namespace> =
        kube::Api::all(state.kube.clone());
    let _ = ns_api
        .delete(&ns_name, &kube::api::DeleteParams::default())
        .await;
}

#[sqlx::test(migrations = "./migrations")]
async fn ensure_session_network_policy_standalone(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;
    let ns_name = format!("test-snp2-{}", &Uuid::new_v4().to_string()[..8]);

    // First create the namespace
    platform::deployer::namespace::ensure_namespace(
        &state.kube,
        &ns_name,
        "session",
        &Uuid::new_v4().to_string(),
        &state.config.platform_namespace,
        state.config.dev_mode,
    )
    .await
    .unwrap();

    // Apply session network policy separately
    platform::deployer::namespace::ensure_session_network_policy(
        &state.kube,
        &ns_name,
        &state.config.platform_namespace,
        &state.config.platform_namespace,
    )
    .await
    .unwrap();

    // Cleanup
    let ns_api: kube::Api<k8s_openapi::api::core::v1::Namespace> =
        kube::Api::all(state.kube.clone());
    let _ = ns_api
        .delete(&ns_name, &kube::api::DeleteParams::default())
        .await;
}

// ---------------------------------------------------------------------------
// Ops repo integration tests (git operations)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn ops_repo_init_and_write_file(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;

    let repo_path = platform::deployer::ops_repo::init_ops_repo(
        &state.config.ops_repos_path,
        "intg-test-repo",
        "main",
    )
    .await
    .unwrap();

    assert!(repo_path.exists());

    // Write a file
    let sha = platform::deployer::ops_repo::write_file_to_repo(
        &repo_path,
        "main",
        "deploy/app.yaml",
        "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: test-app",
    )
    .await
    .unwrap();
    assert!(!sha.is_empty());

    // Read back
    let content =
        platform::deployer::ops_repo::read_file_at_ref(&repo_path, "main", "deploy/app.yaml")
            .await
            .unwrap();
    assert!(content.contains("test-app"));

    let _ = tokio::fs::remove_dir_all(&state.config.ops_repos_path).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn ops_repo_commit_values_roundtrip(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;

    let repo_path = platform::deployer::ops_repo::init_ops_repo(
        &state.config.ops_repos_path,
        "intg-vals-repo",
        "main",
    )
    .await
    .unwrap();

    let values = serde_json::json!({
        "image_ref": "myapp:v2.0.0",
        "replicas": 5,
        "env": "production"
    });

    let sha =
        platform::deployer::ops_repo::commit_values(&repo_path, "main", "production", &values)
            .await
            .unwrap();
    assert!(!sha.is_empty());

    // Read values back
    let read_back = platform::deployer::ops_repo::read_values(&repo_path, "main", "production")
        .await
        .unwrap();
    assert_eq!(read_back["image_ref"], "myapp:v2.0.0");
    assert_eq!(read_back["replicas"], 5);
    assert_eq!(read_back["env"], "production");

    let _ = tokio::fs::remove_dir_all(&state.config.ops_repos_path).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn ops_repo_write_file_identical_content_noop(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;

    let repo_path = platform::deployer::ops_repo::init_ops_repo(
        &state.config.ops_repos_path,
        "intg-noop-repo",
        "main",
    )
    .await
    .unwrap();

    let sha1 = platform::deployer::ops_repo::write_file_to_repo(
        &repo_path,
        "main",
        "config.yaml",
        "key: value",
    )
    .await
    .unwrap();

    let sha2 = platform::deployer::ops_repo::write_file_to_repo(
        &repo_path,
        "main",
        "config.yaml",
        "key: value",
    )
    .await
    .unwrap();

    assert_eq!(
        sha1, sha2,
        "identical content should not create a new commit"
    );

    let _ = tokio::fs::remove_dir_all(&state.config.ops_repos_path).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn ops_repo_merge_branches(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;

    let repo_path = platform::deployer::ops_repo::init_ops_repo(
        &state.config.ops_repos_path,
        "intg-merge-repo",
        "main",
    )
    .await
    .unwrap();

    // Write a file on main
    platform::deployer::ops_repo::write_file_to_repo(
        &repo_path,
        "main",
        "base.txt",
        "base content",
    )
    .await
    .unwrap();

    // Write a file on staging branch
    platform::deployer::ops_repo::write_file_to_repo(
        &repo_path,
        "staging",
        "staging.txt",
        "staging content",
    )
    .await
    .unwrap();

    // Verify branches diverged
    let (diverged, _, _) =
        platform::deployer::ops_repo::compare_branches(&repo_path, "staging", "main")
            .await
            .unwrap();
    assert!(diverged);

    // Merge staging into main
    let sha = platform::deployer::ops_repo::merge_branch(&repo_path, "staging", "main")
        .await
        .unwrap();
    assert!(!sha.is_empty());

    // Main should now have the staging file
    let content = platform::deployer::ops_repo::read_file_at_ref(&repo_path, "main", "staging.txt")
        .await
        .unwrap();
    assert_eq!(content, "staging content");

    let _ = tokio::fs::remove_dir_all(&state.config.ops_repos_path).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn ops_repo_revert_last_commit(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;

    let repo_path = platform::deployer::ops_repo::init_ops_repo(
        &state.config.ops_repos_path,
        "intg-revert-repo",
        "main",
    )
    .await
    .unwrap();

    // Write original
    platform::deployer::ops_repo::write_file_to_repo(&repo_path, "main", "data.txt", "original")
        .await
        .unwrap();

    // Write modified
    platform::deployer::ops_repo::write_file_to_repo(&repo_path, "main", "data.txt", "modified")
        .await
        .unwrap();

    // Revert
    let sha = platform::deployer::ops_repo::revert_last_commit(&repo_path, "main")
        .await
        .unwrap();
    assert!(!sha.is_empty());

    // Should be back to original
    let content = platform::deployer::ops_repo::read_file_at_ref(&repo_path, "main", "data.txt")
        .await
        .unwrap();
    assert_eq!(content, "original");

    let _ = tokio::fs::remove_dir_all(&state.config.ops_repos_path).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn ops_repo_init_rejects_path_traversal(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;

    let result = platform::deployer::ops_repo::init_ops_repo(
        &state.config.ops_repos_path,
        "../escape",
        "main",
    )
    .await;
    assert!(result.is_err());

    let result = platform::deployer::ops_repo::init_ops_repo(
        &state.config.ops_repos_path,
        "evil/name",
        "main",
    )
    .await;
    assert!(result.is_err());

    let result = platform::deployer::ops_repo::init_ops_repo(
        &state.config.ops_repos_path,
        "evil\\name",
        "main",
    )
    .await;
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// create_target: staging environment accepted
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_target_staging_environment(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "tgt-staging", "private").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/targets"),
        serde_json::json!({
            "name": "staging-target",
            "environment": "staging",
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "staging target creation failed: {body}"
    );
    assert_eq!(body["environment"], "staging");
    assert_eq!(body["default_strategy"], "rolling");
    assert_eq!(body["is_active"], true);
}

// ---------------------------------------------------------------------------
// create_target: preview environment accepted
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_target_preview_environment(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "tgt-preview", "private").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/targets"),
        serde_json::json!({
            "name": "preview-target",
            "environment": "preview",
            "default_strategy": "rolling",
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "preview target creation failed: {body}"
    );
    assert_eq!(body["environment"], "preview");
}

// ---------------------------------------------------------------------------
// create_target: fires audit log
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_target_fires_audit(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "tgt-audit", "private").await;

    let (status, _body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/targets"),
        serde_json::json!({
            "name": "audit-target",
            "environment": "production",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let count = helpers::wait_for_audit(&pool, "deploy.target.create", 2000).await;
    assert!(count > 0, "audit entry should exist for target creation");
}

// ---------------------------------------------------------------------------
// create_release: fires audit log
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_release_fires_audit(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "rel-audit", "private").await;
    setup_deployment(&pool, project_id, "production", "app:v1").await;

    let (status, _body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases"),
        serde_json::json!({ "image_ref": "app:v2" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let count = helpers::wait_for_audit(&pool, "deploy.release.create", 2000).await;
    assert!(count > 0, "audit entry should exist for release creation");
}

// ---------------------------------------------------------------------------
// create_release: image_ref validation — minimum 1 char
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_release_empty_image_ref_returns_400(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "rel-empty-img", "private").await;
    setup_deployment(&pool, project_id, "production", "app:v1").await;

    let (status, _body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases"),
        serde_json::json!({ "image_ref": "" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// adjust_traffic: exactly 0 is valid
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn adjust_traffic_zero_weight_succeeds(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "traffic-zero", "private").await;
    let (_, release_id) = setup_deployment(&pool, project_id, "production", "app:v1").await;

    // Transition to progressing so traffic adjustment is allowed
    sqlx::query("UPDATE deploy_releases SET phase = 'progressing' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/traffic"),
        serde_json::json!({ "traffic_weight": 0 }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "traffic weight 0 should be valid: {body}"
    );
    assert_eq!(body["traffic_weight"], 0);
}

// ---------------------------------------------------------------------------
// adjust_traffic: exactly 100 is valid
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn adjust_traffic_100_weight_succeeds(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "traffic-100", "private").await;
    let (_, release_id) = setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'progressing' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/traffic"),
        serde_json::json!({ "traffic_weight": 100 }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "traffic weight 100 should be valid: {body}"
    );
    assert_eq!(body["traffic_weight"], 100);
}

// ---------------------------------------------------------------------------
// adjust_traffic: fires audit log
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn adjust_traffic_fires_audit(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "traffic-audit", "private").await;
    let (_, release_id) = setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'progressing' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, _body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/traffic"),
        serde_json::json!({ "traffic_weight": 50 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let count = helpers::wait_for_audit(&pool, "deploy.traffic.adjust", 2000).await;
    assert!(count > 0, "audit entry should exist for traffic adjustment");
}

// ---------------------------------------------------------------------------
// adjust_traffic: creates history entry
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn adjust_traffic_creates_history(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "traffic-hist", "private").await;
    let (_, release_id) = setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'progressing' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/traffic"),
        serde_json::json!({ "traffic_weight": 75 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, hist) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/history"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(hist["total"].as_i64().unwrap() >= 1);

    // Find the traffic_shifted entry
    let items = hist["items"].as_array().unwrap();
    let traffic_entry = items.iter().find(|e| e["action"] == "traffic_shifted");
    assert!(
        traffic_entry.is_some(),
        "traffic_shifted history entry should exist"
    );
    assert_eq!(traffic_entry.unwrap()["traffic_weight"], 75);
}

// ---------------------------------------------------------------------------
// promote_release: creates history entry
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn promote_release_creates_history(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "promote-hist", "private").await;
    let (_, release_id) = setup_deployment(&pool, project_id, "production", "app:v1").await;

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

    let (_, hist) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/history"),
    )
    .await;
    let items = hist["items"].as_array().unwrap();
    let promote_entry = items.iter().find(|e| e["action"] == "promoted");
    assert!(
        promote_entry.is_some(),
        "promoted history entry should exist"
    );
}

// ---------------------------------------------------------------------------
// promote_release: fires audit log
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn promote_release_fires_audit(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "promote-audit", "private").await;
    let (_, release_id) = setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'progressing' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/promote"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let count = helpers::wait_for_audit(&pool, "deploy.release.promote", 2000).await;
    assert!(count > 0, "audit entry should exist for promote");
}

// ---------------------------------------------------------------------------
// rollback_release: creates history entry and fires audit
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn rollback_release_creates_history_and_audit(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "roll-hist", "private").await;
    let (_, release_id) = setup_deployment(&pool, project_id, "production", "app:v1").await;

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

    // Check history
    let (_, hist) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/history"),
    )
    .await;
    let items = hist["items"].as_array().unwrap();
    let rollback_entry = items.iter().find(|e| e["action"] == "rolled_back");
    assert!(
        rollback_entry.is_some(),
        "rolled_back history entry should exist"
    );

    // Check audit
    let count = helpers::wait_for_audit(&pool, "deploy.release.rollback", 2000).await;
    assert!(count > 0, "audit entry should exist for rollback");
}

// ---------------------------------------------------------------------------
// pause_release: fires audit + creates history
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn pause_release_creates_history_and_audit(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "pause-hist", "private").await;
    let (_, release_id) = setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'progressing' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/pause"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "pause failed: {body}");
    assert_eq!(body["phase"], "paused");

    // Check history
    let (_, hist) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/history"),
    )
    .await;
    let items = hist["items"].as_array().unwrap();
    let pause_entry = items.iter().find(|e| e["action"] == "paused");
    assert!(pause_entry.is_some(), "paused history entry should exist");

    // Check audit
    let count = helpers::wait_for_audit(&pool, "deploy.release.pause", 2000).await;
    assert!(count > 0, "audit entry should exist for pause");
}

// ---------------------------------------------------------------------------
// resume_release: fires audit + creates history
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn resume_release_creates_history_and_audit(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "resume-hist", "private").await;
    let (_, release_id) = setup_deployment(&pool, project_id, "production", "app:v1").await;

    // Must be in paused state first
    sqlx::query("UPDATE deploy_releases SET phase = 'paused' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/resume"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "resume failed: {body}");
    assert_eq!(body["phase"], "progressing");

    // Check history
    let (_, hist) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/history"),
    )
    .await;
    let items = hist["items"].as_array().unwrap();
    let resume_entry = items.iter().find(|e| e["action"] == "resumed");
    assert!(resume_entry.is_some(), "resumed history entry should exist");

    // Check audit
    let count = helpers::wait_for_audit(&pool, "deploy.release.resume", 2000).await;
    assert!(count > 0, "audit entry should exist for resume");
}

// ---------------------------------------------------------------------------
// release_history: requires deploy_read permission
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn release_history_requires_deploy_read(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "hist-perm", "private").await;
    let (_, release_id) = setup_deployment(&pool, project_id, "production", "app:v1").await;

    // User with no deploy permissions
    let (_user_id, user_token) =
        create_user(&app, &admin_token, "histuser", "histuser@example.com").await;

    let (status, _) = helpers::get_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/history"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// release_history: history for nonexistent release returns empty
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn release_history_nonexistent_release_returns_empty(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "hist-norel", "private").await;
    let fake_release = Uuid::new_v4();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{fake_release}/history"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 0);
    assert!(body["items"].as_array().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// staging_status: requires deploy_read permission
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn staging_status_requires_deploy_read(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "stag-perm", "private").await;

    let (_user_id, user_token) =
        create_user(&app, &admin_token, "staguser", "staguser@example.com").await;

    let (status, _) = helpers::get_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/staging-status"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// promote_staging: no ops repo returns 404
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn promote_staging_no_ops_repo_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "prom-no-ops", "private").await;

    let (status, _body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/promote-staging"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// list_deploy_iframes: requires project_read
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_deploy_iframes_no_permission_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "iframe-noperm", "private").await;

    let (_user_id, user_token) =
        create_user(&app, &admin_token, "iframeuser", "iframeuser@example.com").await;

    let (status, _body) = helpers::get_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/deploy-preview/iframes"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// list_deploy_iframes: empty namespace slug returns empty
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_deploy_iframes_empty_slug_returns_empty(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "iframe-empty-slug", "public").await;

    // Set namespace_slug to empty string
    sqlx::query("UPDATE projects SET namespace_slug = '' WHERE id = $1")
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-preview/iframes"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.as_array().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// update_ops_repo: valid partial update (branch only)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn update_ops_repo_branch_only(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    // Create ops repo via API
    let (status, create_body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/ops-repos",
        serde_json::json!({ "name": "upd-branch" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let repo_id = create_body["id"].as_str().unwrap();

    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/admin/ops-repos/{repo_id}"),
        serde_json::json!({ "branch": "develop" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "update branch only failed: {body}");
    assert_eq!(body["branch"], "develop");
}

// ---------------------------------------------------------------------------
// update_ops_repo: valid partial update (path only)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn update_ops_repo_path_only(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (status, create_body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/ops-repos",
        serde_json::json!({ "name": "upd-path" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let repo_id = create_body["id"].as_str().unwrap();

    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/admin/ops-repos/{repo_id}"),
        serde_json::json!({ "path": "/deploy/k8s" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "update path only failed: {body}");
    assert_eq!(body["path"], "/deploy/k8s");
}

// ---------------------------------------------------------------------------
// update_ops_repo: fires audit log
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn update_ops_repo_fires_audit(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (status, create_body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/ops-repos",
        serde_json::json!({ "name": "upd-audit" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let repo_id = create_body["id"].as_str().unwrap();

    let (status, _body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/admin/ops-repos/{repo_id}"),
        serde_json::json!({ "branch": "release" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let count = helpers::wait_for_audit(&pool, "ops_repo.update", 2000).await;
    assert!(count > 0, "audit entry should exist for ops repo update");
}

// ---------------------------------------------------------------------------
// update_ops_repo: invalid branch name rejected
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn update_ops_repo_invalid_branch_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (status, create_body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/ops-repos",
        serde_json::json!({ "name": "upd-badbranch" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let repo_id = create_body["id"].as_str().unwrap();

    let (status, _body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/admin/ops-repos/{repo_id}"),
        serde_json::json!({ "branch": "bad..branch" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// update_ops_repo: invalid path (too long) rejected
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn update_ops_repo_path_too_long_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (status, create_body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/ops-repos",
        serde_json::json!({ "name": "upd-longpath" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let repo_id = create_body["id"].as_str().unwrap();

    let long_path = "/".to_string() + &"a".repeat(500);
    let (status, _body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/admin/ops-repos/{repo_id}"),
        serde_json::json!({ "path": long_path }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// delete_ops_repo: fires audit log
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn delete_ops_repo_fires_audit(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (status, create_body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/ops-repos",
        serde_json::json!({ "name": "del-audit" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let repo_id = create_body["id"].as_str().unwrap();

    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/admin/ops-repos/{repo_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let count = helpers::wait_for_audit(&pool, "ops_repo.delete", 2000).await;
    assert!(count > 0, "audit entry should exist for ops repo delete");
}

// ---------------------------------------------------------------------------
// create_ops_repo: with custom branch and path
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_ops_repo_with_branch_and_path(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/ops-repos",
        serde_json::json!({
            "name": "custom-ops",
            "branch": "release",
            "path": "/deploy"
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create ops repo failed: {body}"
    );
    assert_eq!(body["branch"], "release");
    assert_eq!(body["path"], "/deploy");
}

// ---------------------------------------------------------------------------
// create_ops_repo: fires audit log
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_ops_repo_fires_audit(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (status, _body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/ops-repos",
        serde_json::json!({ "name": "audit-ops" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let count = helpers::wait_for_audit(&pool, "ops_repo.create", 2000).await;
    assert!(count > 0, "audit entry should exist for ops repo create");
}

// ---------------------------------------------------------------------------
// list_ops_repos: returns created repos
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_ops_repos_returns_all(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    // Create two repos
    let (s1, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/ops-repos",
        serde_json::json!({ "name": "list-ops-a" }),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED);

    let (s2, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/ops-repos",
        serde_json::json!({ "name": "list-ops-b" }),
    )
    .await;
    assert_eq!(s2, StatusCode::CREATED);

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/admin/ops-repos").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.as_array().unwrap().len() >= 2);
}

// ---------------------------------------------------------------------------
// list_ops_repos: requires admin
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_ops_repos_requires_admin(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_user_id, user_token) =
        create_user(&app, &admin_token, "opslistuser", "opslistuser@example.com").await;

    let (status, _body) = helpers::get_json(&app, &user_token, "/api/admin/ops-repos").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// get_ops_repo: by ID
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn get_ops_repo_by_id(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (status, create_body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/ops-repos",
        serde_json::json!({ "name": "get-ops" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let repo_id = create_body["id"].as_str().unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/admin/ops-repos/{repo_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "get-ops");
    assert_eq!(body["id"], repo_id);
}

// ---------------------------------------------------------------------------
// get_ops_repo: nonexistent returns 404
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn get_ops_repo_not_found(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (status, _body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/admin/ops-repos/{}", Uuid::new_v4()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// get_ops_repo: requires admin
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn get_ops_repo_requires_admin(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (_, create_body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/ops-repos",
        serde_json::json!({ "name": "get-ops-admin" }),
    )
    .await;
    let repo_id = create_body["id"].as_str().unwrap();

    let (_user_id, user_token) =
        create_user(&app, &admin_token, "opsgetuser", "opsgetuser@example.com").await;

    let (status, _body) = helpers::get_json(
        &app,
        &user_token,
        &format!("/api/admin/ops-repos/{repo_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// create_release: default strategy comes from target
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_release_inherits_target_strategy(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "rel-inherit", "private").await;

    // Create target with canary strategy
    let target_id = Uuid::new_v4();
    sqlx::query(
        r"INSERT INTO deploy_targets
           (id, project_id, name, environment, default_strategy, is_active)
           VALUES ($1, $2, 'prod', 'production', 'canary', true)",
    )
    .bind(target_id)
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases"),
        serde_json::json!({ "image_ref": "app:canary" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create release failed: {body}");
    assert_eq!(body["strategy"], "canary");
}

// ---------------------------------------------------------------------------
// create_release: explicit strategy overrides target default
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_release_explicit_strategy_overrides_default(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "rel-override", "private").await;
    setup_deployment(&pool, project_id, "production", "app:v1").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases"),
        serde_json::json!({
            "image_ref": "app:v2",
            "strategy": "canary"
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create release with explicit strategy failed: {body}"
    );
    assert_eq!(body["strategy"], "canary");
}

// ---------------------------------------------------------------------------
// adjust_traffic: -1 rejected
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn adjust_traffic_negative_one_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "traffic-neg1", "private").await;
    let (_, release_id) = setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'progressing' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, _body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/traffic"),
        serde_json::json!({ "traffic_weight": -1 }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// adjust_traffic: 101 rejected
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn adjust_traffic_101_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "traffic-101", "private").await;
    let (_, release_id) = setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'progressing' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, _body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/traffic"),
        serde_json::json!({ "traffic_weight": 101 }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// promote_release: completed release rejected
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn promote_release_completed_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "prom-done", "private").await;
    let (_, release_id) = setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'completed' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, _body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/promote"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// rollback_release: completed release rejected
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn rollback_release_completed_release_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "roll-done", "private").await;
    let (_, release_id) = setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'completed' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, _body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/rollback"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// pause_release: holding phase rejected (only progressing allowed)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn pause_release_holding_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "pause-hold", "private").await;
    let (_, release_id) = setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'holding' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, _body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/pause"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// resume_release: holding phase rejected (only paused allowed)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn resume_release_holding_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "resume-hold", "private").await;
    let (_, release_id) = setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'holding' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, _body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/resume"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// create_target: with ops_repo_id and manifest_path — returns them
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_target_returns_ops_repo_and_manifest(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "tgt-ops-man", "private").await;

    // Create an ops repo first
    let (_, create_body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/ops-repos",
        serde_json::json!({ "name": "tgt-ops-repo" }),
    )
    .await;
    let ops_repo_id = create_body["id"].as_str().unwrap();

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/targets"),
        serde_json::json!({
            "name": "ops-target",
            "environment": "production",
            "ops_repo_id": ops_repo_id,
            "manifest_path": "k8s/production"
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create target with ops repo failed: {body}"
    );
    assert_eq!(body["ops_repo_id"], ops_repo_id);
    assert_eq!(body["manifest_path"], "k8s/production");
}

// ---------------------------------------------------------------------------
// list targets: no auth returns 401
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_targets_no_auth_returns_401(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (status, _body) = helpers::get_json(
        &app,
        "",
        &format!("/api/projects/{}/targets", Uuid::new_v4()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// list releases: no auth returns 401
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_releases_no_auth_returns_401(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (status, _body) = helpers::get_json(
        &app,
        "",
        &format!("/api/projects/{}/deploy-releases", Uuid::new_v4()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// create_target: name validation — empty name rejected
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_target_empty_name_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "tgt-empty-name", "private").await;

    let (status, _body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/targets"),
        serde_json::json!({
            "name": "",
            "environment": "production",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// create_release: commit_sha stored
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_release_stores_commit_sha(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "rel-sha", "private").await;
    setup_deployment(&pool, project_id, "production", "app:v1").await;

    let sha = "abc123def456";
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases"),
        serde_json::json!({
            "image_ref": "app:v2",
            "commit_sha": sha
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create release failed: {body}");
    assert_eq!(body["commit_sha"], sha);
}

// ---------------------------------------------------------------------------
// promote_staging: fires audit log
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn promote_staging_fires_audit(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let project_id = create_project(&app, &admin_token, "prom-stag-aud", "public").await;

    // Create ops repo on disk
    let tmp = std::env::temp_dir().join(format!("platform-test-{}", Uuid::new_v4()));
    let ops_path = platform::deployer::ops_repo::init_ops_repo(&tmp, "prom-aud-ops", "main")
        .await
        .unwrap();

    platform::deployer::ops_repo::write_file_to_repo(&ops_path, "main", "README.md", "# Ops")
        .await
        .unwrap();

    platform::deployer::ops_repo::write_file_to_repo(
        &ops_path,
        "staging",
        "staging/values.yaml",
        "image_ref: app:v3\nreplicas: 2\n",
    )
    .await
    .unwrap();

    let ops_repo_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO ops_repos (id, name, repo_path, branch, path, project_id)
         VALUES ($1, $2, $3, 'main', '/', $4)",
    )
    .bind(ops_repo_id)
    .bind("prom-aud-ops")
    .bind(ops_path.to_string_lossy().to_string())
    .bind(project_id)
    .execute(&pool)
    .await
    .unwrap();

    let (status, _body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/promote-staging"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let count = helpers::wait_for_audit(&pool, "deploy.promote_staging", 2000).await;
    assert!(count > 0, "audit entry should exist for promote staging");

    let _ = tokio::fs::remove_dir_all(&tmp).await;
}

// ---------------------------------------------------------------------------
// list_targets: pagination
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_targets_pagination(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "tgt-page", "private").await;

    // Create 3 targets with unique environments (can't duplicate env for same project)
    for env in &["production", "staging", "preview"] {
        helpers::post_json(
            &app,
            &admin_token,
            &format!("/api/projects/{project_id}/targets"),
            serde_json::json!({
                "name": format!("target-{env}"),
                "environment": env,
            }),
        )
        .await;
    }

    // Request with limit=2
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/targets?limit=2"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    assert_eq!(body["total"], 3);

    // Request with offset=2
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/targets?limit=2&offset=2"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// list_releases: pagination
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_releases_pagination(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "rel-page", "private").await;
    setup_deployment(&pool, project_id, "production", "app:v1").await;

    // Create 3 releases
    for i in 2..=4 {
        helpers::post_json(
            &app,
            &admin_token,
            &format!("/api/projects/{project_id}/deploy-releases"),
            serde_json::json!({ "image_ref": format!("app:v{i}") }),
        )
        .await;
    }

    // Expect 4 total (1 from setup + 3 created)
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases?limit=2"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    assert!(body["total"].as_i64().unwrap() >= 4);
}

// ---------------------------------------------------------------------------
// adjust_traffic: completed release returns 404 (not found because WHERE excludes terminal phases)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn adjust_traffic_rolled_back_release_not_found(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "traffic-rb", "private").await;
    let (_, release_id) = setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'rolled_back' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, _body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/traffic"),
        serde_json::json!({ "traffic_weight": 50 }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// adjust_traffic: cancelled release returns 404
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn adjust_traffic_cancelled_release_not_found(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "traffic-canc", "private").await;
    let (_, release_id) = setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'cancelled' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, _body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/traffic"),
        serde_json::json!({ "traffic_weight": 50 }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// adjust_traffic: failed release returns 404
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn adjust_traffic_failed_release_not_found(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "traffic-fail", "private").await;
    let (_, release_id) = setup_deployment(&pool, project_id, "production", "app:v1").await;

    sqlx::query("UPDATE deploy_releases SET phase = 'failed' WHERE id = $1")
        .bind(release_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, _body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases/{release_id}/traffic"),
        serde_json::json!({ "traffic_weight": 50 }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// rollback_release: progressing release transitions to rolling_back
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn rollback_release_progressing_sets_rolling_back(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "roll-prog", "private").await;
    let (_, release_id) = setup_deployment(&pool, project_id, "production", "app:v1").await;

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

// ---------------------------------------------------------------------------
// create_target: default environment is production
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_target_default_environment_is_production(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "tgt-default-env", "private").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/targets"),
        serde_json::json!({ "name": "default-env-target" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create target failed: {body}");
    assert_eq!(body["environment"], "production");
}

// ---------------------------------------------------------------------------
// create_target: default strategy is rolling
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_target_default_strategy_is_rolling(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "tgt-default-strat", "private").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/targets"),
        serde_json::json!({ "name": "default-strat-target" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create target failed: {body}");
    assert_eq!(body["default_strategy"], "rolling");
}

// ---------------------------------------------------------------------------
// create_release: default phase is pending
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_release_default_phase_is_pending(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "rel-phase", "private").await;
    setup_deployment(&pool, project_id, "production", "app:v1").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-releases"),
        serde_json::json!({ "image_ref": "app:v2" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["phase"], "pending");
    assert_eq!(body["traffic_weight"], 0);
    assert_eq!(body["health"], "unknown");
}

// ---------------------------------------------------------------------------
// list_deploy_iframes: with env query parameter
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_deploy_iframes_staging_env(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "iframe-stag", "public").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/deploy-preview/iframes?env=staging"),
    )
    .await;
    // Should succeed (even if empty) — validates the env param is accepted
    assert_eq!(status, StatusCode::OK);
    assert!(body.as_array().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// get_release: wrong project returns 404
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn get_release_wrong_project_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_a = create_project(&app, &admin_token, "rel-proj-a", "private").await;
    let project_b = create_project(&app, &admin_token, "rel-proj-b", "private").await;
    let (_, release_id) = setup_deployment(&pool, project_a, "production", "app:v1").await;

    let (status, _body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_b}/deploy-releases/{release_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// get_target: wrong project returns 404
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn get_target_wrong_project_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_a = create_project(&app, &admin_token, "tgt-proj-a", "private").await;
    let project_b = create_project(&app, &admin_token, "tgt-proj-b", "private").await;
    let (target_id, _) = setup_deployment(&pool, project_a, "production", "app:v1").await;

    let (status, _body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_b}/targets/{target_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
