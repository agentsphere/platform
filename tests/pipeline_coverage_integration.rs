//! Additional integration tests for `src/api/pipelines.rs` coverage gaps.
//!
//! Covers: list with filters, get pipeline wrong project, cancel nonexistent,
//! step logs not found, artifacts listing, artifact download not found,
//! trigger validation, permission checks.

mod helpers;

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

/// Get admin user ID.
async fn get_admin_id(app: &axum::Router, token: &str) -> Uuid {
    let (_, body) = helpers::get_json(app, token, "/api/auth/me").await;
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

/// Insert a pipeline row directly.
async fn insert_pipeline(
    pool: &PgPool,
    project_id: Uuid,
    user_id: Uuid,
    status: &str,
    git_ref: &str,
    trigger: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO pipelines (id, project_id, trigger, git_ref, status, triggered_by)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(project_id)
    .bind(trigger)
    .bind(git_ref)
    .bind(status)
    .bind(user_id)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// Insert a pipeline step.
async fn insert_step(
    pool: &PgPool,
    pipeline_id: Uuid,
    project_id: Uuid,
    step_order: i32,
    name: &str,
    status: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO pipeline_steps (id, pipeline_id, project_id, step_order, name, image, status, gate, depends_on)
         VALUES ($1, $2, $3, $4, $5, 'alpine:latest', $6, false, '{}')",
    )
    .bind(id)
    .bind(pipeline_id)
    .bind(project_id)
    .bind(step_order)
    .bind(name)
    .bind(status)
    .execute(pool)
    .await
    .unwrap();
    id
}

// ---------------------------------------------------------------------------
// List pipelines
// ---------------------------------------------------------------------------

/// List pipelines with no data returns empty.
#[sqlx::test(migrations = "./migrations")]
async fn list_pipelines_empty(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "pl-empty", "public").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/pipelines"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 0);
    assert_eq!(body["items"].as_array().unwrap().len(), 0);
}

/// List pipelines with status filter.
#[sqlx::test(migrations = "./migrations")]
async fn list_pipelines_filter_by_status(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "pl-filter", "public").await;
    let admin_id = get_admin_id(&app, &admin_token).await;

    insert_pipeline(
        &pool,
        project_id,
        admin_id,
        "success",
        "refs/heads/main",
        "push",
    )
    .await;
    insert_pipeline(
        &pool,
        project_id,
        admin_id,
        "failure",
        "refs/heads/dev",
        "push",
    )
    .await;
    insert_pipeline(
        &pool,
        project_id,
        admin_id,
        "success",
        "refs/heads/feat",
        "api",
    )
    .await;

    // Filter by status=success
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/pipelines?status=success"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 2);

    // Filter by trigger=api
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/pipelines?trigger=api"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1);

    // Filter by git_ref
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/pipelines?git_ref=refs/heads/main"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1);
}

/// List pipelines with pagination.
#[sqlx::test(migrations = "./migrations")]
async fn list_pipelines_pagination(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "pl-page", "public").await;
    let admin_id = get_admin_id(&app, &admin_token).await;

    for i in 0..5 {
        insert_pipeline(
            &pool,
            project_id,
            admin_id,
            "success",
            &format!("refs/heads/feat-{i}"),
            "push",
        )
        .await;
    }

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/pipelines?limit=2&offset=0"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    assert_eq!(body["total"], 5);
}

// ---------------------------------------------------------------------------
// Get pipeline detail
// ---------------------------------------------------------------------------

/// Get pipeline detail includes steps.
#[sqlx::test(migrations = "./migrations")]
async fn get_pipeline_with_steps(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "pl-steps", "public").await;
    let admin_id = get_admin_id(&app, &admin_token).await;

    let pipeline_id = insert_pipeline(
        &pool,
        project_id,
        admin_id,
        "success",
        "refs/heads/main",
        "push",
    )
    .await;
    insert_step(&pool, pipeline_id, project_id, 1, "build", "success").await;
    insert_step(&pool, pipeline_id, project_id, 2, "test", "success").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/pipelines/{pipeline_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "success");
    assert_eq!(body["steps"].as_array().unwrap().len(), 2);
}

/// Get pipeline from wrong project returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn get_pipeline_wrong_project_returns_404(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_a = helpers::create_project(&app, &admin_token, "pl-prj-a", "public").await;
    let project_b = helpers::create_project(&app, &admin_token, "pl-prj-b", "public").await;
    let admin_id = get_admin_id(&app, &admin_token).await;

    let pipeline_id = insert_pipeline(
        &pool,
        project_a,
        admin_id,
        "success",
        "refs/heads/main",
        "push",
    )
    .await;

    // Try to get pipeline under wrong project
    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_b}/pipelines/{pipeline_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Get nonexistent pipeline returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn get_nonexistent_pipeline_returns_404(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "pl-404", "public").await;
    let fake_id = Uuid::new_v4();

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/pipelines/{fake_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Cancel pipeline
// ---------------------------------------------------------------------------

/// Cancel pipeline that doesn't belong to project returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn cancel_pipeline_wrong_project_returns_404(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_a = helpers::create_project(&app, &admin_token, "cancel-a", "public").await;
    let project_b = helpers::create_project(&app, &admin_token, "cancel-b", "public").await;
    let admin_id = get_admin_id(&app, &admin_token).await;

    let pipeline_id = insert_pipeline(
        &pool,
        project_a,
        admin_id,
        "pending",
        "refs/heads/main",
        "push",
    )
    .await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_b}/pipelines/{pipeline_id}/cancel"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Cancel nonexistent pipeline returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn cancel_nonexistent_pipeline_returns_404(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "cancel-none", "public").await;
    let fake_id = Uuid::new_v4();

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/pipelines/{fake_id}/cancel"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Step logs
// ---------------------------------------------------------------------------

/// Step logs for nonexistent step returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn step_logs_nonexistent_step_returns_404(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "logs-nostep", "public").await;
    let admin_id = get_admin_id(&app, &admin_token).await;

    let pipeline_id = insert_pipeline(
        &pool,
        project_id,
        admin_id,
        "success",
        "refs/heads/main",
        "push",
    )
    .await;
    let fake_step = Uuid::new_v4();

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/pipelines/{pipeline_id}/steps/{fake_step}/logs"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Step logs for pipeline in wrong project returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn step_logs_wrong_project_returns_404(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_a = helpers::create_project(&app, &admin_token, "logs-a", "public").await;
    let project_b = helpers::create_project(&app, &admin_token, "logs-b", "public").await;
    let admin_id = get_admin_id(&app, &admin_token).await;

    let pipeline_id = insert_pipeline(
        &pool,
        project_a,
        admin_id,
        "success",
        "refs/heads/main",
        "push",
    )
    .await;
    let step_id = insert_step(&pool, pipeline_id, project_a, 1, "build", "success").await;

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_b}/pipelines/{pipeline_id}/steps/{step_id}/logs"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Step logs for completed step without log_ref returns "No logs available".
#[sqlx::test(migrations = "./migrations")]
async fn step_logs_no_log_ref_returns_no_logs(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "logs-noref", "public").await;
    let admin_id = get_admin_id(&app, &admin_token).await;

    let pipeline_id = insert_pipeline(
        &pool,
        project_id,
        admin_id,
        "success",
        "refs/heads/main",
        "push",
    )
    .await;
    let step_id = insert_step(&pool, pipeline_id, project_id, 1, "build", "success").await;

    // Use a raw request to check body text (not JSON)
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let req = Request::builder()
        .method("GET")
        .uri(format!(
            "/api/projects/{project_id}/pipelines/{pipeline_id}/steps/{step_id}/logs"
        ))
        .header("Authorization", format!("Bearer {admin_token}"))
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&body_bytes);
    assert!(text.contains("No logs available"));
}

// ---------------------------------------------------------------------------
// Artifacts
// ---------------------------------------------------------------------------

/// List artifacts for pipeline in wrong project returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn list_artifacts_wrong_project_returns_404(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_a = helpers::create_project(&app, &admin_token, "art-a", "public").await;
    let project_b = helpers::create_project(&app, &admin_token, "art-b", "public").await;
    let admin_id = get_admin_id(&app, &admin_token).await;

    let pipeline_id = insert_pipeline(
        &pool,
        project_a,
        admin_id,
        "success",
        "refs/heads/main",
        "push",
    )
    .await;

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_b}/pipelines/{pipeline_id}/artifacts"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// List artifacts for valid pipeline returns empty array.
#[sqlx::test(migrations = "./migrations")]
async fn list_artifacts_empty(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "art-empty", "public").await;
    let admin_id = get_admin_id(&app, &admin_token).await;

    let pipeline_id = insert_pipeline(
        &pool,
        project_id,
        admin_id,
        "success",
        "refs/heads/main",
        "push",
    )
    .await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/pipelines/{pipeline_id}/artifacts"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 0);
}

/// Download nonexistent artifact returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn download_nonexistent_artifact_returns_404(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "art-dl-404", "public").await;
    let admin_id = get_admin_id(&app, &admin_token).await;

    let pipeline_id = insert_pipeline(
        &pool,
        project_id,
        admin_id,
        "success",
        "refs/heads/main",
        "push",
    )
    .await;
    let fake_art = Uuid::new_v4();

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!(
            "/api/projects/{project_id}/pipelines/{pipeline_id}/artifacts/{fake_art}/download"
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Permission checks
// ---------------------------------------------------------------------------

/// Non-member cannot list pipelines on private project.
#[sqlx::test(migrations = "./migrations")]
async fn list_pipelines_private_project_non_member(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "pl-priv", "private").await;
    let (_uid, user_token) =
        helpers::create_user(&app, &admin_token, "pl-noacc", "plnoacc@test.com").await;

    let (status, _) = helpers::get_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/pipelines"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Trigger pipeline with invalid branch name fails validation.
#[sqlx::test(migrations = "./migrations")]
async fn trigger_pipeline_invalid_ref_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "pl-bad-ref", "public").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/pipelines"),
        json!({ "git_ref": "refs/../../evil" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Trigger pipeline without write permission fails.
#[sqlx::test(migrations = "./migrations")]
async fn trigger_pipeline_requires_write(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "pl-trig-perm", "public").await;
    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "pl-viewer", "plviewer@test.com").await;
    helpers::assign_role(&app, &admin_token, user_id, "viewer", None, &pool).await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/pipelines"),
        json!({ "git_ref": "main" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Cancel pipeline requires write permission.
#[sqlx::test(migrations = "./migrations")]
async fn cancel_pipeline_requires_write(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let project_id = helpers::create_project(&app, &admin_token, "pl-can-perm", "public").await;
    let admin_id = get_admin_id(&app, &admin_token).await;

    let pipeline_id = insert_pipeline(
        &pool,
        project_id,
        admin_id,
        "pending",
        "refs/heads/main",
        "push",
    )
    .await;

    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "pl-can-view", "plcanview@test.com").await;
    helpers::assign_role(&app, &admin_token, user_id, "viewer", None, &pool).await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/pipelines/{pipeline_id}/cancel"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
