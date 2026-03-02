//! Integration tests for the "Create App" flow (Phase E).

mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;

use helpers::{
    assign_role, create_project, create_user, patch_json, post_json, set_user_api_key, test_router,
    test_state,
};

// ---------------------------------------------------------------------------
// Create App endpoint
// ---------------------------------------------------------------------------

/// Create a project-less session via /api/create-app.
#[sqlx::test(migrations = "./migrations")]
async fn create_app_session(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_user_id, token) = create_user(&app, &admin_token, "dev1", "dev1@test.com").await;
    assign_role(&app, &admin_token, _user_id, "developer", None, &pool).await;
    set_user_api_key(&pool, _user_id).await;

    let (status, body) = post_json(
        &app,
        &token,
        "/api/create-app",
        serde_json::json!({
            "description": "I want to build a REST API with auth and a Postgres database"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "create-app failed: {body}");
    assert_eq!(body["status"].as_str(), Some("running"));
    // project_id should be null for create-app sessions
    assert!(
        body["project_id"].is_null(),
        "project_id should be null: {body}"
    );
    assert!(body["id"].as_str().is_some());
}

/// Create app with empty description is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn create_app_empty_description_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (user_id, token) = create_user(&app, &admin_token, "dev2", "dev2@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let (status, _body) = post_json(
        &app,
        &token,
        "/api/create-app",
        serde_json::json!({ "description": "" }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Viewer (without project:write + agent:run) cannot create app.
#[sqlx::test(migrations = "./migrations")]
async fn create_app_requires_permissions(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (user_id, token) = create_user(&app, &admin_token, "viewer1", "viewer1@test.com").await;
    assign_role(&app, &admin_token, user_id, "viewer", None, &pool).await;

    let (status, _body) = post_json(
        &app,
        &token,
        "/api/create-app",
        serde_json::json!({ "description": "Build something" }),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Session update endpoint
// ---------------------------------------------------------------------------

/// Link a project-less session to a project via PATCH.
#[sqlx::test(migrations = "./migrations")]
async fn update_session_link_project(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (user_id, token) = create_user(&app, &admin_token, "dev3", "dev3@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;
    set_user_api_key(&pool, user_id).await;

    // Create a project-less session
    let (status, session_body) = post_json(
        &app,
        &token,
        "/api/create-app",
        serde_json::json!({ "description": "Build a blog" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let session_id = session_body["id"].as_str().unwrap();
    assert!(session_body["project_id"].is_null());

    // Create a project to link to
    let project_id = create_project(&app, &token, "my-blog", "private").await;

    // Link session to project
    let (status, updated) = patch_json(
        &app,
        &token,
        &format!("/api/sessions/{session_id}"),
        serde_json::json!({ "project_id": project_id }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "update session failed: {updated}");
    assert_eq!(
        updated["project_id"].as_str(),
        Some(project_id.to_string().as_str()),
    );
}

/// Non-owner cannot update session.
#[sqlx::test(migrations = "./migrations")]
async fn update_session_non_owner_forbidden(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (user_id, token) = create_user(&app, &admin_token, "dev4", "dev4@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;
    set_user_api_key(&pool, user_id).await;
    let (_other_id, other_token) = create_user(&app, &admin_token, "dev5", "dev5@test.com").await;
    assign_role(&app, &admin_token, _other_id, "developer", None, &pool).await;

    // dev4 creates session
    let (status, session_body) = post_json(
        &app,
        &token,
        "/api/create-app",
        serde_json::json!({ "description": "My app" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let session_id = session_body["id"].as_str().unwrap();

    let project_id = create_project(&app, &token, "test-proj", "private").await;

    // dev5 tries to update dev4's session
    let (status, _body) = patch_json(
        &app,
        &other_token,
        &format!("/api/sessions/{session_id}"),
        serde_json::json!({ "project_id": project_id }),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Rate limiting on create-app.
#[sqlx::test(migrations = "./migrations")]
async fn create_app_rate_limited(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (user_id, token) = create_user(&app, &admin_token, "dev6", "dev6@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;
    set_user_api_key(&pool, user_id).await;

    // Create 5 sessions (should succeed)
    for i in 0..5 {
        let (status, _) = post_json(
            &app,
            &token,
            "/api/create-app",
            serde_json::json!({ "description": format!("App {i}") }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "session {i} should succeed");
    }

    // 6th should be rate limited
    let (status, _body) = post_json(
        &app,
        &token,
        "/api/create-app",
        serde_json::json!({ "description": "One too many" }),
    )
    .await;

    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}

// ---------------------------------------------------------------------------
// New tests for create-app agent tooling
// ---------------------------------------------------------------------------

/// Create-app without API key → error mentioning API key.
#[sqlx::test(migrations = "./migrations")]
async fn create_app_without_api_key_fails(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (user_id, token) = create_user(&app, &admin_token, "nokey", "nokey@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;
    // Note: NOT calling set_user_api_key

    let (status, body) = post_json(
        &app,
        &token,
        "/api/create-app",
        serde_json::json!({ "description": "Build something" }),
    )
    .await;

    // Should fail with 400 because no API key is configured (user or global)
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "expected 400 without API key, got {status}: {body}"
    );
    let error_msg = body["error"].as_str().unwrap_or("");
    assert!(
        error_msg.contains("API key"),
        "expected error about API key, got: {error_msg}"
    );
}

/// After create-app, session has pod_name=null (in-process, not K8s).
#[sqlx::test(migrations = "./migrations")]
async fn create_app_session_is_inprocess(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (user_id, token) = create_user(&app, &admin_token, "dev7", "dev7@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;
    set_user_api_key(&pool, user_id).await;

    let (status, body) = post_json(
        &app,
        &token,
        "/api/create-app",
        serde_json::json!({ "description": "Build a REST API" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // In-process sessions should have no pod_name
    assert!(
        body["pod_name"].is_null(),
        "in-process session should have null pod_name: {body}"
    );
}
