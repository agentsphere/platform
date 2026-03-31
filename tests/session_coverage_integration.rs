//! Additional integration tests for `src/api/sessions.rs` coverage gaps.
//!
//! Covers: validate_provider_config paths, iframes listing, progress endpoint,
//! global SSE events auth, create-app permission checks, send_message validation,
//! truncate_prompt edge cases.

mod helpers;

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use helpers::{create_project, create_user, test_router, test_state};

/// Insert a session row directly.
async fn insert_session(
    pool: &PgPool,
    project_id: Uuid,
    user_id: Uuid,
    prompt: &str,
    status: &str,
) -> Uuid {
    insert_session_with_ns(pool, project_id, user_id, prompt, status, None).await
}

async fn insert_session_with_ns(
    pool: &PgPool,
    project_id: Uuid,
    user_id: Uuid,
    prompt: &str,
    status: &str,
    session_namespace: Option<&str>,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, session_namespace)
         VALUES ($1, $2, $3, $4, $5, 'claude-code', $6)",
    )
    .bind(id)
    .bind(project_id)
    .bind(user_id)
    .bind(prompt)
    .bind(status)
    .bind(session_namespace)
    .execute(pool)
    .await
    .expect("insert session");
    id
}

async fn get_admin_id(app: &axum::Router, token: &str) -> Uuid {
    let (_, body) = helpers::get_json(app, token, "/api/auth/me").await;
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

// ---------------------------------------------------------------------------
// Send message: project-scoped validation
// ---------------------------------------------------------------------------

/// Send message to project-scoped session with empty content fails.
#[sqlx::test(migrations = "./migrations")]
async fn send_message_empty_content_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "msg-empty", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id, "test", "running").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/message"),
        json!({ "content": "" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Send message to wrong project returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn send_message_wrong_project_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_a = create_project(&app, &admin_token, "msg-prj-a", "private").await;
    let project_b = create_project(&app, &admin_token, "msg-prj-b", "private").await;
    let session_id = insert_session(&pool, project_a, admin_id, "in A", "running").await;

    // Try to send message under project B
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_b}/sessions/{session_id}/message"),
        json!({ "content": "wrong project" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Non-owner cannot send messages on project-scoped session.
#[sqlx::test(migrations = "./migrations")]
async fn send_message_non_owner_forbidden(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "msg-forbid", "public").await;
    let session_id = insert_session(&pool, project_id, admin_id, "admin only", "running").await;

    // User without project:write
    let (user_id, user_token) =
        create_user(&app, &admin_token, "msg-nowr", "msgnowr@test.com").await;
    helpers::assign_role(&app, &admin_token, user_id, "viewer", None, &pool).await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/message"),
        json!({ "content": "hi" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Iframes listing
// ---------------------------------------------------------------------------

/// Iframes for nonexistent session returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn list_iframes_nonexistent_session(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "iframe-404", "private").await;
    let fake_session = Uuid::new_v4();

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{fake_session}/iframes"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Iframes for session in wrong project returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn list_iframes_wrong_project(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_a = create_project(&app, &admin_token, "iframe-a", "private").await;
    let project_b = create_project(&app, &admin_token, "iframe-b", "private").await;
    let session_id = insert_session(&pool, project_a, admin_id, "iframes test", "running").await;

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_b}/sessions/{session_id}/iframes"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Iframes for session without namespace returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn list_iframes_no_namespace(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "iframe-nons", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id, "no ns", "running").await;

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/iframes"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Progress endpoint
// ---------------------------------------------------------------------------

/// Progress endpoint with no progress messages returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn progress_no_messages_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "prog-empty", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id, "no progress", "running").await;

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/progress"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Progress endpoint with a progress_update message returns the latest.
#[sqlx::test(migrations = "./migrations")]
async fn progress_returns_latest_update(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "prog-latest", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id, "has progress", "running").await;

    // Insert a progress_update message
    sqlx::query(
        "INSERT INTO agent_messages (session_id, role, content) VALUES ($1, 'progress_update', 'Step 1 complete')",
    )
    .bind(session_id)
    .execute(&pool)
    .await
    .unwrap();

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/progress"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["message"], "Step 1 complete");
}

// ---------------------------------------------------------------------------
// Global send_message validation
// ---------------------------------------------------------------------------

/// Global send_message with content too long is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn send_message_global_content_too_long(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "msg-long", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id, "long msg", "running").await;

    let long_content = "x".repeat(100_001);
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/sessions/{session_id}/message"),
        json!({ "content": long_content }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Create-app permission checks
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// SSE events permission checks
// ---------------------------------------------------------------------------

/// SSE events for session in wrong project returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn sse_events_wrong_project_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_a = create_project(&app, &admin_token, "sse-a", "private").await;
    let project_b = create_project(&app, &admin_token, "sse-b", "private").await;
    let session_id = insert_session(&pool, project_a, admin_id, "sse test", "running").await;

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_b}/sessions/{session_id}/events"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// SSE events for nonexistent session returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn sse_events_nonexistent_session_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "sse-none", "private").await;
    let fake_id = Uuid::new_v4();

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{fake_id}/events"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Global SSE events for nonexistent session returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn sse_events_global_nonexistent_session_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let fake_id = Uuid::new_v4();
    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/sessions/{fake_id}/events"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Global SSE events: non-owner cannot access.
#[sqlx::test(migrations = "./migrations")]
async fn sse_events_global_non_owner_forbidden(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "sse-forbid", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id, "admin only sse", "running").await;

    let (_uid, user_token) =
        create_user(&app, &admin_token, "sse-noown", "ssenoown@test.com").await;

    let (status, _) = helpers::get_json(
        &app,
        &user_token,
        &format!("/api/sessions/{session_id}/events"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Stop session non-owner check
// ---------------------------------------------------------------------------

/// Non-owner without project:write cannot stop session.
#[sqlx::test(migrations = "./migrations")]
async fn stop_session_non_owner_forbidden(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "stop-forbid", "public").await;
    let session_id = insert_session(&pool, project_id, admin_id, "admin session", "running").await;

    let (user_id, user_token) =
        create_user(&app, &admin_token, "stop-nowr", "stopnowr@test.com").await;
    helpers::assign_role(&app, &admin_token, user_id, "viewer", None, &pool).await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/stop"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Update session edge cases
// ---------------------------------------------------------------------------

/// Updating nonexistent session returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn update_nonexistent_session_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let fake_id = Uuid::new_v4();
    let project_id = create_project(&app, &admin_token, "upd-sess-404", "private").await;

    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/sessions/{fake_id}"),
        json!({ "project_id": project_id }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
