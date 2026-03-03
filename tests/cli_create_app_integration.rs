//! Integration tests for the CLI-based create-app flow.
//!
//! These tests verify the create-app API endpoint behavior including
//! permissions, rate limiting, session metadata, and error handling.
//!
//! Tests that require a mock CLI subprocess are marked with `#[ignore]`
//! and require `CLAUDE_CLI_PATH` to point to `tests/fixtures/mock-claude-cli.sh`.
//! Run them via: `just test-mock-cli`

mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;

use helpers::{
    assign_role, create_user, get_json, post_json, set_user_api_key, test_router, test_state,
};

// ---------------------------------------------------------------------------
// API-level tests (no mock CLI needed)
// ---------------------------------------------------------------------------

/// CLI create-app session has execution_mode = 'cli_subprocess' and uses_pubsub = true.
#[sqlx::test(migrations = "./migrations")]
async fn cli_create_app_session_metadata(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (user_id, token) = create_user(&app, &admin_token, "clidev3", "clidev3@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;
    set_user_api_key(&pool, user_id).await;

    let (status, body) = post_json(
        &app,
        &token,
        "/api/create-app",
        serde_json::json!({"description": "Build something"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let session_id = body["id"].as_str().unwrap();
    let row: (String, bool) = sqlx::query_as(
        "SELECT execution_mode, uses_pubsub FROM agent_sessions WHERE id = $1::uuid",
    )
    .bind(session_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.0, "cli_subprocess");
    assert!(row.1, "uses_pubsub should be true");
}

/// No credentials → create-app returns 400.
#[sqlx::test(migrations = "./migrations")]
async fn cli_create_app_no_credentials(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (user_id, token) = create_user(&app, &admin_token, "nocred", "nocred@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;
    // No API key set

    let (status, body) = post_json(
        &app,
        &token,
        "/api/create-app",
        serde_json::json!({"description": "Build something"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let error = body["error"].as_str().unwrap_or("");
    assert!(
        error.contains("API key"),
        "expected API key error, got: {error}"
    );
}

/// Viewer cannot use create-app.
#[sqlx::test(migrations = "./migrations")]
async fn cli_create_app_viewer_forbidden(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (user_id, token) = create_user(&app, &admin_token, "viewer2", "viewer2@test.com").await;
    assign_role(&app, &admin_token, user_id, "viewer", None, &pool).await;

    let (status, _body) = post_json(
        &app,
        &token,
        "/api/create-app",
        serde_json::json!({"description": "Build something"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Empty description → 400.
#[sqlx::test(migrations = "./migrations")]
async fn cli_create_app_empty_description_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (user_id, token) = create_user(&app, &admin_token, "clidev5", "clidev5@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let (status, _body) = post_json(
        &app,
        &token,
        "/api/create-app",
        serde_json::json!({"description": ""}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Rate limiting on create-app endpoint.
#[sqlx::test(migrations = "./migrations")]
async fn cli_create_app_rate_limited(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (user_id, token) = create_user(&app, &admin_token, "clidev6", "clidev6@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;
    set_user_api_key(&pool, user_id).await;

    // Create 5 sessions (should succeed)
    for i in 0..5 {
        let (status, _) = post_json(
            &app,
            &token,
            "/api/create-app",
            serde_json::json!({"description": format!("App {i}")}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "session {i} should succeed");
    }

    // 6th should be rate limited
    let (status, _body) = post_json(
        &app,
        &token,
        "/api/create-app",
        serde_json::json!({"description": "One too many"}),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}

// ---------------------------------------------------------------------------
// Mock CLI tests (require CLAUDE_CLI_PATH pointing to mock-claude-cli.sh)
// ---------------------------------------------------------------------------

/// CLI create-app with text-only response (no tools).
///
/// Requires: `CLAUDE_CLI_PATH=tests/fixtures/mock-claude-cli.sh`
#[sqlx::test(migrations = "./migrations")]
#[ignore]
async fn cli_create_app_text_only(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (user_id, token) = create_user(&app, &admin_token, "mcdev1", "mcdev1@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;
    set_user_api_key(&pool, user_id).await;

    let (status, body) = post_json(
        &app,
        &token,
        "/api/create-app",
        serde_json::json!({"description": "Build me a blog"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create-app failed: {body}");
    assert_eq!(body["status"].as_str(), Some("running"));

    // Wait briefly for the background tool loop to complete
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Session should still exist
    let session_id = body["id"].as_str().unwrap();
    let (status, _session) = get_json(&app, &token, &format!("/api/sessions/{session_id}")).await;
    assert_eq!(status, StatusCode::OK);
}

/// CLI create-app with tool request triggers server-side execution.
///
/// Requires: `CLAUDE_CLI_PATH=tests/fixtures/mock-claude-cli.sh`
///           `MOCK_CLI_RESPONSE_FILE` pointing to a JSON with create_project tool
#[sqlx::test(migrations = "./migrations")]
#[ignore]
async fn cli_create_app_creates_project(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (user_id, token) = create_user(&app, &admin_token, "mcdev2", "mcdev2@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;
    set_user_api_key(&pool, user_id).await;

    let (status, body) = post_json(
        &app,
        &token,
        "/api/create-app",
        serde_json::json!({"description": "Create a blog with Postgres"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create-app failed: {body}");

    // Wait for tool loop to complete
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Verify the project was actually created in the database
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT name FROM projects WHERE owner_id = $1 AND is_active = true LIMIT 1",
    )
    .bind(user_id)
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert!(
        row.is_some(),
        "a project should have been created by the tool loop"
    );
}
