//! Integration tests for manager agent approval flow — approve/reject actions,
//! session-approved tools, pending actions, and the full approval roundtrip.

mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

use helpers::{get_json, post_json, test_router, test_state};

/// Insert a manager session row directly into the DB (bypasses service layer).
async fn insert_manager_session(pool: &PgPool, user_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, execution_mode, uses_pubsub, provider)
         VALUES ($1, $2, 'Manager session', 'running', 'manager', true, 'claude-code')",
    )
    .bind(id)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("insert manager session");
    id
}

/// Get admin user ID from the /api/auth/me endpoint.
async fn get_admin_id(app: &axum::Router, token: &str) -> Uuid {
    let (_, me) = get_json(app, token, "/api/auth/me").await;
    Uuid::parse_str(me["id"].as_str().unwrap()).unwrap()
}

// ---------------------------------------------------------------------------
// Approval endpoints
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn approve_action_writes_to_valkey(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let admin_id = get_admin_id(&app, &admin_token).await;
    let session_id = insert_manager_session(&pool, admin_id).await;

    // POST approve_action
    let (status, _body) = post_json(
        &app,
        &admin_token,
        &format!("/api/manager/sessions/{session_id}/approve_action"),
        serde_json::json!({ "action_hash": "testhash123" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // GET approval check
    let (status, body) = get_json(
        &app,
        &admin_token,
        &format!("/api/manager/sessions/{session_id}/approval/testhash123"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["approved"], true);
}

#[sqlx::test(migrations = "./migrations")]
async fn approve_action_is_single_use(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let admin_id = get_admin_id(&app, &admin_token).await;
    let session_id = insert_manager_session(&pool, admin_id).await;

    // Approve
    let (status, _) = post_json(
        &app,
        &admin_token,
        &format!("/api/manager/sessions/{session_id}/approve_action"),
        serde_json::json!({ "action_hash": "singleuse" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // First check → true (consumed)
    let (_, body) = get_json(
        &app,
        &admin_token,
        &format!("/api/manager/sessions/{session_id}/approval/singleuse"),
    )
    .await;
    assert_eq!(body["approved"], true);

    // Second check → false (already consumed)
    let (_, body) = get_json(
        &app,
        &admin_token,
        &format!("/api/manager/sessions/{session_id}/approval/singleuse"),
    )
    .await;
    assert_eq!(body["approved"], false);
}

#[sqlx::test(migrations = "./migrations")]
async fn reject_action_writes_to_valkey(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let admin_id = get_admin_id(&app, &admin_token).await;
    let session_id = insert_manager_session(&pool, admin_id).await;

    // POST reject_action
    let (status, _) = post_json(
        &app,
        &admin_token,
        &format!("/api/manager/sessions/{session_id}/reject_action"),
        serde_json::json!({ "action_hash": "testhash456" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // GET rejection check
    let (_, body) = get_json(
        &app,
        &admin_token,
        &format!("/api/manager/sessions/{session_id}/rejection/testhash456"),
    )
    .await;
    assert_eq!(body["rejected"], true);
}

#[sqlx::test(migrations = "./migrations")]
async fn reject_action_is_single_use(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let admin_id = get_admin_id(&app, &admin_token).await;
    let session_id = insert_manager_session(&pool, admin_id).await;

    // Reject
    let (status, _) = post_json(
        &app,
        &admin_token,
        &format!("/api/manager/sessions/{session_id}/reject_action"),
        serde_json::json!({ "action_hash": "rejonce" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // First check → true (consumed)
    let (_, body) = get_json(
        &app,
        &admin_token,
        &format!("/api/manager/sessions/{session_id}/rejection/rejonce"),
    )
    .await;
    assert_eq!(body["rejected"], true);

    // Second check → false (already consumed)
    let (_, body) = get_json(
        &app,
        &admin_token,
        &format!("/api/manager/sessions/{session_id}/rejection/rejonce"),
    )
    .await;
    assert_eq!(body["rejected"], false);
}

#[sqlx::test(migrations = "./migrations")]
async fn approval_without_pending_still_works(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let admin_id = get_admin_id(&app, &admin_token).await;
    let session_id = insert_manager_session(&pool, admin_id).await;

    // Approve a hash that was never registered as pending
    let (status, _) = post_json(
        &app,
        &admin_token,
        &format!("/api/manager/sessions/{session_id}/approve_action"),
        serde_json::json!({ "action_hash": "never_pending" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Should still be approved
    let (_, body) = get_json(
        &app,
        &admin_token,
        &format!("/api/manager/sessions/{session_id}/approval/never_pending"),
    )
    .await;
    assert_eq!(body["approved"], true);
}

#[sqlx::test(migrations = "./migrations")]
async fn approve_tool_adds_to_session_set(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let admin_id = get_admin_id(&app, &admin_token).await;
    let session_id = insert_manager_session(&pool, admin_id).await;

    // Approve a tool
    let (status, _) = post_json(
        &app,
        &admin_token,
        &format!("/api/manager/sessions/{session_id}/approve_tool"),
        serde_json::json!({ "tool_name": "create_project" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // List approved tools
    let (_, body) = get_json(
        &app,
        &admin_token,
        &format!("/api/manager/sessions/{session_id}/approved_tools"),
    )
    .await;
    let tools = body["tools"].as_array().unwrap();
    assert!(
        tools.iter().any(|t| t.as_str() == Some("create_project")),
        "Expected 'create_project' in approved tools: {tools:?}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn approve_tool_accumulates(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let admin_id = get_admin_id(&app, &admin_token).await;
    let session_id = insert_manager_session(&pool, admin_id).await;

    // Approve two tools
    post_json(
        &app,
        &admin_token,
        &format!("/api/manager/sessions/{session_id}/approve_tool"),
        serde_json::json!({ "tool_name": "create_project" }),
    )
    .await;
    post_json(
        &app,
        &admin_token,
        &format!("/api/manager/sessions/{session_id}/approve_tool"),
        serde_json::json!({ "tool_name": "spawn_agent" }),
    )
    .await;

    // Both should be present
    let (_, body) = get_json(
        &app,
        &admin_token,
        &format!("/api/manager/sessions/{session_id}/approved_tools"),
    )
    .await;
    let tools: Vec<&str> = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t.as_str())
        .collect();
    assert!(tools.contains(&"create_project"), "missing create_project");
    assert!(tools.contains(&"spawn_agent"), "missing spawn_agent");
}

#[sqlx::test(migrations = "./migrations")]
async fn pending_action_returns_ok(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let admin_id = get_admin_id(&app, &admin_token).await;
    let session_id = insert_manager_session(&pool, admin_id).await;

    let (status, body) = post_json(
        &app,
        &admin_token,
        &format!("/api/manager/sessions/{session_id}/pending_action"),
        serde_json::json!({ "action_hash": "abc", "summary": "Create project" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
}

#[sqlx::test(migrations = "./migrations")]
async fn approval_check_nonexistent_returns_false(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let admin_id = get_admin_id(&app, &admin_token).await;
    let session_id = insert_manager_session(&pool, admin_id).await;

    let (status, body) = get_json(
        &app,
        &admin_token,
        &format!("/api/manager/sessions/{session_id}/approval/nonexistent"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["approved"], false);
}

#[sqlx::test(migrations = "./migrations")]
async fn rejection_check_nonexistent_returns_false(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let admin_id = get_admin_id(&app, &admin_token).await;
    let session_id = insert_manager_session(&pool, admin_id).await;

    let (status, body) = get_json(
        &app,
        &admin_token,
        &format!("/api/manager/sessions/{session_id}/rejection/nonexistent"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["rejected"], false);
}

// ---------------------------------------------------------------------------
// send_manager_message endpoint
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn manager_send_message_returns_ok(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let admin_id = get_admin_id(&app, &admin_token).await;
    let session_id = insert_manager_session(&pool, admin_id).await;

    let (status, body) = post_json(
        &app,
        &admin_token,
        &format!("/api/manager/sessions/{session_id}/message"),
        serde_json::json!({ "content": "list projects" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
}

#[sqlx::test(migrations = "./migrations")]
async fn manager_send_message_stopped_session_fails(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let admin_id = get_admin_id(&app, &admin_token).await;
    let session_id = insert_manager_session(&pool, admin_id).await;

    // Stop the session
    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/manager/sessions/{session_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Try to send a message to the stopped session
    let (status, _) = post_json(
        &app,
        &admin_token,
        &format!("/api/manager/sessions/{session_id}/message"),
        serde_json::json!({ "content": "list projects" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn manager_send_message_persists_user_message(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let admin_id = get_admin_id(&app, &admin_token).await;
    let session_id = insert_manager_session(&pool, admin_id).await;

    // Send a message
    let (status, _) = post_json(
        &app,
        &admin_token,
        &format!("/api/manager/sessions/{session_id}/message"),
        serde_json::json!({ "content": "hello manager" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Check agent_messages table for the persisted user message
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT role, content FROM agent_messages WHERE session_id = $1 AND role = 'user' LIMIT 1",
    )
    .bind(session_id)
    .fetch_optional(&pool)
    .await
    .expect("query messages");

    let (role, content) = row.expect("user message should exist");
    assert_eq!(role, "user");
    assert_eq!(content, "hello manager");
}

// ---------------------------------------------------------------------------
// Full approval roundtrip (via Valkey)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn approval_roundtrip_via_valkey(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let admin_id = get_admin_id(&app, &admin_token).await;
    let session_id = insert_manager_session(&pool, admin_id).await;

    // 1. Register a pending action
    let (status, _) = post_json(
        &app,
        &admin_token,
        &format!("/api/manager/sessions/{session_id}/pending_action"),
        serde_json::json!({ "action_hash": "round", "summary": "Test action" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 2. Not yet approved
    let (_, body) = get_json(
        &app,
        &admin_token,
        &format!("/api/manager/sessions/{session_id}/approval/round"),
    )
    .await;
    assert_eq!(body["approved"], false);

    // 3. Approve it
    let (status, _) = post_json(
        &app,
        &admin_token,
        &format!("/api/manager/sessions/{session_id}/approve_action"),
        serde_json::json!({ "action_hash": "round" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 4. Now it's approved (consumed on read)
    let (_, body) = get_json(
        &app,
        &admin_token,
        &format!("/api/manager/sessions/{session_id}/approval/round"),
    )
    .await;
    assert_eq!(body["approved"], true);

    // 5. Second check → consumed
    let (_, body) = get_json(
        &app,
        &admin_token,
        &format!("/api/manager/sessions/{session_id}/approval/round"),
    )
    .await;
    assert_eq!(body["approved"], false);
}
