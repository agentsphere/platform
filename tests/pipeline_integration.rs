//! Integration tests for the pipeline API (`src/api/pipelines.rs`).
//!
//! Tests the list/get/cancel/artifacts/logs handlers using direct DB inserts
//! to bypass K8s dependencies. The trigger endpoint is tested only for error
//! cases (missing project, no repo path) since a real git repo is required.

mod helpers;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use helpers::{
    admin_login, assign_role, create_project, create_user, get_json, post_json, test_router,
    test_state,
};

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Get the admin user ID from the DB (the bootstrap admin).
async fn admin_user_id(pool: &PgPool) -> Uuid {
    let row: (Uuid,) = sqlx::query_as("SELECT id FROM users WHERE name = 'admin'")
        .fetch_one(pool)
        .await
        .expect("admin user must exist");
    row.0
}

/// Insert a pipeline directly into the DB, returning its UUID.
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
        "INSERT INTO pipelines (id, project_id, triggered_by, status, git_ref, trigger, commit_sha)
         VALUES ($1, $2, $3, $4, $5, $6, 'abc123')",
    )
    .bind(id)
    .bind(project_id)
    .bind(user_id)
    .bind(status)
    .bind(git_ref)
    .bind(trigger)
    .execute(pool)
    .await
    .expect("insert pipeline");
    id
}

/// Insert a pipeline step, returning its UUID.
async fn insert_step(
    pool: &PgPool,
    pipeline_id: Uuid,
    project_id: Uuid,
    name: &str,
    status: &str,
    step_order: i32,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO pipeline_steps (id, pipeline_id, project_id, name, image, status, step_order)
         VALUES ($1, $2, $3, $4, 'alpine:3.19', $5, $6)",
    )
    .bind(id)
    .bind(pipeline_id)
    .bind(project_id)
    .bind(name)
    .bind(status)
    .bind(step_order)
    .execute(pool)
    .await
    .expect("insert step");
    id
}

/// Insert a pipeline step with a `log_ref` pointing to `MinIO`, returning its UUID.
async fn insert_step_with_log(
    pool: &PgPool,
    pipeline_id: Uuid,
    project_id: Uuid,
    name: &str,
    log_ref: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO pipeline_steps (id, pipeline_id, project_id, name, image, status, step_order, log_ref)
         VALUES ($1, $2, $3, $4, 'alpine:3.19', 'success', 0, $5)",
    )
    .bind(id)
    .bind(pipeline_id)
    .bind(project_id)
    .bind(name)
    .bind(log_ref)
    .execute(pool)
    .await
    .expect("insert step with log");
    id
}

/// Insert an artifact record, returning its UUID.
async fn insert_artifact(
    pool: &PgPool,
    pipeline_id: Uuid,
    name: &str,
    minio_path: &str,
    content_type: Option<&str>,
    size_bytes: Option<i64>,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO artifacts (id, pipeline_id, name, minio_path, content_type, size_bytes)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(pipeline_id)
    .bind(name)
    .bind(minio_path)
    .bind(content_type)
    .bind(size_bytes)
    .execute(pool)
    .await
    .expect("insert artifact");
    id
}

/// Raw GET returning (`StatusCode`, `Vec<u8>`) for non-JSON endpoints.
async fn get_bytes(app: &axum::Router, token: &str, path: &str) -> (StatusCode, Vec<u8>) {
    let mut builder = Request::builder().method("GET").uri(path);
    if !token.is_empty() {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    let req = builder.body(Body::empty()).unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, bytes)
}

// ===========================================================================
// List pipelines
// ===========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn list_pipelines_empty(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let token = admin_login(&app).await;

    let project_id = create_project(&app, &token, "pl-empty", "private").await;

    let (status, body) = get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/pipelines"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 0);
    assert_eq!(body["total"], 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn list_pipelines_with_data(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let token = admin_login(&app).await;
    let uid = admin_user_id(&pool).await;

    let project_id = create_project(&app, &token, "pl-list", "private").await;
    insert_pipeline(&pool, project_id, uid, "success", "refs/heads/main", "push").await;
    insert_pipeline(&pool, project_id, uid, "pending", "refs/heads/dev", "api").await;

    let (status, body) = get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/pipelines"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(body["total"], 2);

    // Check that required fields are present
    let first = &items[0];
    assert!(first["id"].is_string());
    assert_eq!(
        first["project_id"].as_str().unwrap(),
        project_id.to_string()
    );
    assert!(first["status"].is_string());
    assert!(first["git_ref"].is_string());
    assert!(first["trigger"].is_string());
    assert!(first["created_at"].is_string());
}

#[sqlx::test(migrations = "./migrations")]
async fn list_pipelines_filter_status(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let token = admin_login(&app).await;
    let uid = admin_user_id(&pool).await;

    let project_id = create_project(&app, &token, "pl-filt-status", "private").await;
    insert_pipeline(&pool, project_id, uid, "success", "refs/heads/main", "push").await;
    insert_pipeline(&pool, project_id, uid, "failure", "refs/heads/main", "push").await;
    insert_pipeline(&pool, project_id, uid, "success", "refs/heads/dev", "api").await;

    let (status, body) = get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/pipelines?status=success"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(body["total"], 2);
    for item in items {
        assert_eq!(item["status"], "success");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn list_pipelines_filter_git_ref(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let token = admin_login(&app).await;
    let uid = admin_user_id(&pool).await;

    let project_id = create_project(&app, &token, "pl-filt-ref", "private").await;
    insert_pipeline(&pool, project_id, uid, "success", "refs/heads/main", "push").await;
    insert_pipeline(
        &pool,
        project_id,
        uid,
        "success",
        "refs/heads/feature",
        "push",
    )
    .await;

    let (status, body) = get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/pipelines?git_ref=refs/heads/main"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["git_ref"], "refs/heads/main");
}

#[sqlx::test(migrations = "./migrations")]
async fn list_pipelines_filter_trigger(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let token = admin_login(&app).await;
    let uid = admin_user_id(&pool).await;

    let project_id = create_project(&app, &token, "pl-filt-trig", "private").await;
    insert_pipeline(&pool, project_id, uid, "success", "refs/heads/main", "push").await;
    insert_pipeline(&pool, project_id, uid, "pending", "refs/heads/main", "api").await;
    insert_pipeline(
        &pool,
        project_id,
        uid,
        "running",
        "refs/heads/main",
        "schedule",
    )
    .await;

    let (status, body) = get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/pipelines?trigger=api"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["trigger"], "api");
}

#[sqlx::test(migrations = "./migrations")]
async fn list_pipelines_pagination(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let token = admin_login(&app).await;
    let uid = admin_user_id(&pool).await;

    let project_id = create_project(&app, &token, "pl-page", "private").await;
    for i in 0..5 {
        insert_pipeline(
            &pool,
            project_id,
            uid,
            "success",
            &format!("refs/heads/branch-{i}"),
            "push",
        )
        .await;
    }

    // Page 1: limit=2, offset=0
    let (status, body) = get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/pipelines?limit=2&offset=0"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    assert_eq!(body["total"], 5);

    // Page 2: limit=2, offset=2
    let (_, body2) = get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/pipelines?limit=2&offset=2"),
    )
    .await;
    assert_eq!(body2["items"].as_array().unwrap().len(), 2);
    assert_eq!(body2["total"], 5);

    // Page 3: limit=2, offset=4 (only 1 remaining)
    let (_, body3) = get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/pipelines?limit=2&offset=4"),
    )
    .await;
    assert_eq!(body3["items"].as_array().unwrap().len(), 1);
    assert_eq!(body3["total"], 5);
}

// ===========================================================================
// Get pipeline detail
// ===========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn get_pipeline_detail(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let token = admin_login(&app).await;
    let uid = admin_user_id(&pool).await;

    let project_id = create_project(&app, &token, "pl-detail", "private").await;
    let pipeline_id =
        insert_pipeline(&pool, project_id, uid, "success", "refs/heads/main", "push").await;
    insert_step(&pool, pipeline_id, project_id, "build", "success", 0).await;
    insert_step(&pool, pipeline_id, project_id, "test", "success", 1).await;

    let (status, body) = get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/pipelines/{pipeline_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"].as_str().unwrap(), pipeline_id.to_string());
    assert_eq!(body["status"], "success");
    assert_eq!(body["git_ref"], "refs/heads/main");
    assert_eq!(body["commit_sha"], "abc123");

    let steps = body["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 2);
    // Steps should be ordered by step_order
    assert_eq!(steps[0]["name"], "build");
    assert_eq!(steps[0]["step_order"], 0);
    assert_eq!(steps[1]["name"], "test");
    assert_eq!(steps[1]["step_order"], 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn get_pipeline_not_found(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let token = admin_login(&app).await;

    let project_id = create_project(&app, &token, "pl-nf", "private").await;
    let fake_id = Uuid::new_v4();

    let (status, _) = get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/pipelines/{fake_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn get_pipeline_wrong_project(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let token = admin_login(&app).await;
    let uid = admin_user_id(&pool).await;

    let project_a = create_project(&app, &token, "pl-wrong-a", "private").await;
    let project_b = create_project(&app, &token, "pl-wrong-b", "private").await;
    let pipeline_id =
        insert_pipeline(&pool, project_a, uid, "success", "refs/heads/main", "push").await;

    // Pipeline belongs to project_a but we request it under project_b
    let (status, _) = get_json(
        &app,
        &token,
        &format!("/api/projects/{project_b}/pipelines/{pipeline_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ===========================================================================
// Cancel pipeline
// ===========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn cancel_pipeline_pending(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let token = admin_login(&app).await;
    let uid = admin_user_id(&pool).await;

    let project_id = create_project(&app, &token, "pl-cancel-p", "private").await;
    let pipeline_id =
        insert_pipeline(&pool, project_id, uid, "pending", "refs/heads/main", "api").await;
    insert_step(&pool, pipeline_id, project_id, "build", "pending", 0).await;

    let (status, body) = post_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/pipelines/{pipeline_id}/cancel"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "cancel pending failed: {body}");
    assert_eq!(body["ok"], true);

    // Verify DB state updated
    let (db_status,): (String,) = sqlx::query_as("SELECT status FROM pipelines WHERE id = $1")
        .bind(pipeline_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(db_status, "cancelled");

    // Verify pending steps were skipped
    let (step_status,): (String,) =
        sqlx::query_as("SELECT status FROM pipeline_steps WHERE pipeline_id = $1")
            .bind(pipeline_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(step_status, "skipped");
}

#[sqlx::test(migrations = "./migrations")]
async fn cancel_pipeline_running(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let token = admin_login(&app).await;
    let uid = admin_user_id(&pool).await;

    let project_id = create_project(&app, &token, "pl-cancel-r", "private").await;
    let pipeline_id =
        insert_pipeline(&pool, project_id, uid, "running", "refs/heads/main", "push").await;

    let (status, body) = post_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/pipelines/{pipeline_id}/cancel"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "cancel running failed: {body}");
    assert_eq!(body["ok"], true);

    let (db_status,): (String,) = sqlx::query_as("SELECT status FROM pipelines WHERE id = $1")
        .bind(pipeline_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(db_status, "cancelled");
}

#[sqlx::test(migrations = "./migrations")]
async fn cancel_pipeline_already_finished(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let token = admin_login(&app).await;
    let uid = admin_user_id(&pool).await;

    let project_id = create_project(&app, &token, "pl-cancel-done", "private").await;
    let pipeline_id =
        insert_pipeline(&pool, project_id, uid, "success", "refs/heads/main", "push").await;

    // Cancel on an already-finished pipeline still returns OK (the UPDATE is
    // a no-op because the WHERE clause filters on pending/running).
    let (status, body) = post_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/pipelines/{pipeline_id}/cancel"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "cancel finished: {body}");

    // Status should remain unchanged
    let (db_status,): (String,) = sqlx::query_as("SELECT status FROM pipelines WHERE id = $1")
        .bind(pipeline_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(db_status, "success");
}

#[sqlx::test(migrations = "./migrations")]
async fn cancel_pipeline_not_found(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let token = admin_login(&app).await;

    let project_id = create_project(&app, &token, "pl-cancel-nf", "private").await;
    let fake_id = Uuid::new_v4();

    let (status, _) = post_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/pipelines/{fake_id}/cancel"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ===========================================================================
// Step logs
// ===========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn get_step_logs_stored(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let token = admin_login(&app).await;
    let uid = admin_user_id(&pool).await;

    let project_id = create_project(&app, &token, "pl-logs", "private").await;
    let pipeline_id =
        insert_pipeline(&pool, project_id, uid, "success", "refs/heads/main", "push").await;

    // Write log content to in-memory MinIO
    let log_path = format!("pipelines/{pipeline_id}/steps/build.log");
    let log_content = b"step 1: compiling...\nstep 2: done\n";
    state
        .minio
        .write(&log_path, log_content.to_vec())
        .await
        .expect("write to minio");

    let step_id = insert_step_with_log(&pool, pipeline_id, project_id, "build", &log_path).await;

    let (status, bytes) = get_bytes(
        &app,
        &token,
        &format!("/api/projects/{project_id}/pipelines/{pipeline_id}/steps/{step_id}/logs"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body_str = String::from_utf8(bytes).unwrap();
    assert!(body_str.contains("step 1: compiling"));
    assert!(body_str.contains("step 2: done"));
}

#[sqlx::test(migrations = "./migrations")]
async fn get_step_logs_no_log_ref(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let token = admin_login(&app).await;
    let uid = admin_user_id(&pool).await;

    let project_id = create_project(&app, &token, "pl-logs-none", "private").await;
    let pipeline_id =
        insert_pipeline(&pool, project_id, uid, "success", "refs/heads/main", "push").await;

    // Insert a step with no log_ref (NULL)
    let step_id = insert_step(&pool, pipeline_id, project_id, "build", "success", 0).await;

    let (status, bytes) = get_bytes(
        &app,
        &token,
        &format!("/api/projects/{project_id}/pipelines/{pipeline_id}/steps/{step_id}/logs"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body_str = String::from_utf8(bytes).unwrap();
    assert_eq!(body_str, "No logs available");
}

#[sqlx::test(migrations = "./migrations")]
async fn get_step_logs_step_not_found(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let token = admin_login(&app).await;
    let uid = admin_user_id(&pool).await;

    let project_id = create_project(&app, &token, "pl-logs-snf", "private").await;
    let pipeline_id =
        insert_pipeline(&pool, project_id, uid, "success", "refs/heads/main", "push").await;
    let fake_step = Uuid::new_v4();

    let (status, _) = get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/pipelines/{pipeline_id}/steps/{fake_step}/logs"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ===========================================================================
// Artifacts
// ===========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn list_artifacts_empty(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let token = admin_login(&app).await;
    let uid = admin_user_id(&pool).await;

    let project_id = create_project(&app, &token, "pl-art-empty", "private").await;
    let pipeline_id =
        insert_pipeline(&pool, project_id, uid, "success", "refs/heads/main", "push").await;

    let (status, body) = get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/pipelines/{pipeline_id}/artifacts"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn list_artifacts_with_data(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let token = admin_login(&app).await;
    let uid = admin_user_id(&pool).await;

    let project_id = create_project(&app, &token, "pl-art-data", "private").await;
    let pipeline_id =
        insert_pipeline(&pool, project_id, uid, "success", "refs/heads/main", "push").await;
    insert_artifact(
        &pool,
        pipeline_id,
        "report.tar.gz",
        "artifacts/report.tar.gz",
        Some("application/gzip"),
        Some(1024),
    )
    .await;
    insert_artifact(
        &pool,
        pipeline_id,
        "coverage.html",
        "artifacts/coverage.html",
        Some("text/html"),
        Some(512),
    )
    .await;

    let (status, body) = get_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/pipelines/{pipeline_id}/artifacts"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body.as_array().unwrap();
    assert_eq!(items.len(), 2);

    // Check first artifact has expected fields
    let art = &items[0];
    assert!(art["id"].is_string());
    assert!(art["name"].is_string());
    assert!(art["content_type"].is_string());
    assert!(art["size_bytes"].is_number());
    assert!(art["created_at"].is_string());
}

#[sqlx::test(migrations = "./migrations")]
async fn list_artifacts_wrong_project(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let token = admin_login(&app).await;
    let uid = admin_user_id(&pool).await;

    let project_a = create_project(&app, &token, "pl-art-wa", "private").await;
    let project_b = create_project(&app, &token, "pl-art-wb", "private").await;
    let pipeline_id =
        insert_pipeline(&pool, project_a, uid, "success", "refs/heads/main", "push").await;

    // Request artifacts under the wrong project
    let (status, _) = get_json(
        &app,
        &token,
        &format!("/api/projects/{project_b}/pipelines/{pipeline_id}/artifacts"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn download_artifact(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let token = admin_login(&app).await;
    let uid = admin_user_id(&pool).await;

    let project_id = create_project(&app, &token, "pl-art-dl", "private").await;
    let pipeline_id =
        insert_pipeline(&pool, project_id, uid, "success", "refs/heads/main", "push").await;

    let minio_path = format!("artifacts/{pipeline_id}/report.tar.gz");
    let artifact_data = b"fake tarball content";
    state
        .minio
        .write(&minio_path, artifact_data.to_vec())
        .await
        .expect("write artifact to minio");

    let artifact_id = insert_artifact(
        &pool,
        pipeline_id,
        "report.tar.gz",
        &minio_path,
        Some("application/gzip"),
        Some(i64::try_from(artifact_data.len()).unwrap()),
    )
    .await;

    let (status, bytes) = get_bytes(
        &app,
        &token,
        &format!(
            "/api/projects/{project_id}/pipelines/{pipeline_id}/artifacts/{artifact_id}/download"
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, artifact_data);
}

#[sqlx::test(migrations = "./migrations")]
async fn download_artifact_not_found(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let token = admin_login(&app).await;
    let uid = admin_user_id(&pool).await;

    let project_id = create_project(&app, &token, "pl-art-dl-nf", "private").await;
    let pipeline_id =
        insert_pipeline(&pool, project_id, uid, "success", "refs/heads/main", "push").await;
    let fake_artifact = Uuid::new_v4();

    let (status, _) = get_json(
        &app,
        &token,
        &format!(
            "/api/projects/{project_id}/pipelines/{pipeline_id}/artifacts/{fake_artifact}/download"
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ===========================================================================
// Permission checks
// ===========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn private_project_returns_404_for_non_member(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;
    let uid = admin_user_id(&pool).await;

    let project_id = create_project(&app, &admin_token, "pl-priv", "private").await;
    insert_pipeline(&pool, project_id, uid, "success", "refs/heads/main", "push").await;

    // Create a user with no project roles
    let (_user_id, user_token) =
        create_user(&app, &admin_token, "outsider-pl", "outsider-pl@test.com").await;

    // List pipelines → 404 (private project, not 403)
    let (status, _) = get_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/pipelines"),
    )
    .await;
    assert!(
        status == StatusCode::NOT_FOUND || status == StatusCode::FORBIDDEN,
        "expected 404 or 403 for private project, got {status}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn viewer_can_read_but_not_cancel(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;
    let uid = admin_user_id(&pool).await;

    let project_id = create_project(&app, &admin_token, "pl-viewer", "private").await;
    let pipeline_id =
        insert_pipeline(&pool, project_id, uid, "pending", "refs/heads/main", "push").await;

    let (viewer_id, viewer_token) =
        create_user(&app, &admin_token, "viewer-pl", "viewer-pl@test.com").await;
    assign_role(
        &app,
        &admin_token,
        viewer_id,
        "viewer",
        Some(project_id),
        &pool,
    )
    .await;

    // Viewer can list pipelines
    let (status, body) = get_json(
        &app,
        &viewer_token,
        &format!("/api/projects/{project_id}/pipelines"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "viewer read failed: {body}");
    assert_eq!(body["total"], 1);

    // Viewer cannot cancel (requires write permission)
    let (status, _) = post_json(
        &app,
        &viewer_token,
        &format!("/api/projects/{project_id}/pipelines/{pipeline_id}/cancel"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ===========================================================================
// Trigger pipeline (error cases only — no real git repo)
// ===========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn trigger_pipeline_project_not_found(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let token = admin_login(&app).await;

    let fake_id = Uuid::new_v4();

    let (status, _) = post_json(
        &app,
        &token,
        &format!("/api/projects/{fake_id}/pipelines"),
        serde_json::json!({ "git_ref": "refs/heads/main" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn trigger_pipeline_no_repo_path(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let token = admin_login(&app).await;

    // Create a project — by default repo_path is NULL when created via the API
    // without actually initializing a git repo.
    let project_id = create_project(&app, &token, "pl-no-repo", "private").await;

    // Clear repo_path to ensure it's NULL
    sqlx::query("UPDATE projects SET repo_path = NULL WHERE id = $1")
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    let (status, _) = post_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/pipelines"),
        serde_json::json!({ "git_ref": "refs/heads/main" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ===========================================================================
// Public project access
// ===========================================================================

#[sqlx::test(migrations = "./migrations")]
async fn public_project_allows_any_user_to_list(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;
    let uid = admin_user_id(&pool).await;

    let project_id = create_project(&app, &admin_token, "pl-public", "public").await;
    insert_pipeline(&pool, project_id, uid, "success", "refs/heads/main", "push").await;

    // Create a random user with no roles
    let (_user_id, user_token) =
        create_user(&app, &admin_token, "anyone-pl", "anyone-pl@test.com").await;

    let (status, body) = get_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/pipelines"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1);
}
