//! Integration tests for agent session CRUD — list, get, stop, spawn, children.
//! NOTE: create_session requires K8s and is tested in E2E. Here we test the
//! read/write paths by inserting session data directly.

mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

use helpers::{admin_login, create_project, create_user, test_router, test_state};

/// Insert a session row directly (bypasses K8s pod creation).
async fn insert_session(
    pool: &PgPool,
    project_id: Uuid,
    user_id: Uuid,
    prompt: &str,
    status: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider)
         VALUES ($1, $2, $3, $4, $5, 'claude-code')",
    )
    .bind(id)
    .bind(project_id)
    .bind(user_id)
    .bind(prompt)
    .bind(status)
    .execute(pool)
    .await
    .expect("insert session");
    id
}

/// Insert a message for a session.
async fn insert_message(pool: &PgPool, session_id: Uuid, role: &str, content: &str) {
    sqlx::query("INSERT INTO agent_messages (session_id, role, content) VALUES ($1, $2, $3)")
        .bind(session_id)
        .bind(role)
        .bind(content)
        .execute(pool)
        .await
        .expect("insert message");
}

// ---------------------------------------------------------------------------
// List sessions
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_sessions_empty(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let project_id = create_project(&app, &admin_token, "sess-empty", "private").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"].as_i64().unwrap(), 0);
    assert_eq!(body["items"].as_array().unwrap().len(), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn list_sessions_with_data(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let project_id = create_project(&app, &admin_token, "sess-data", "private").await;

    insert_session(&pool, project_id, admin_id, "build the app", "running").await;
    insert_session(&pool, project_id, admin_id, "fix the bug", "completed").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"].as_i64().unwrap(), 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn list_sessions_filter_by_status(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let project_id = create_project(&app, &admin_token, "sess-filter", "private").await;

    insert_session(&pool, project_id, admin_id, "running task", "running").await;
    insert_session(&pool, project_id, admin_id, "done task", "completed").await;
    insert_session(&pool, project_id, admin_id, "another done", "completed").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions?status=completed"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"].as_i64().unwrap(), 2);

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions?status=running"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"].as_i64().unwrap(), 1);
}

// ---------------------------------------------------------------------------
// Get session detail
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn get_session_detail_includes_messages(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let project_id = create_project(&app, &admin_token, "sess-detail", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id, "detail test", "running").await;

    insert_message(&pool, session_id, "user", "hello agent").await;
    insert_message(&pool, session_id, "assistant", "hello human").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"].as_str().unwrap(), session_id.to_string());
    assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][1]["role"], "assistant");
}

#[sqlx::test(migrations = "./migrations")]
async fn get_session_wrong_project_returns_404(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let project_a = create_project(&app, &admin_token, "sess-proj-a", "private").await;
    let project_b = create_project(&app, &admin_token, "sess-proj-b", "private").await;
    let session_id = insert_session(&pool, project_a, admin_id, "wrong project", "running").await;

    // Try to get session under wrong project
    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_b}/sessions/{session_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Stop session
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn stop_session_updates_status(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let project_id = create_project(&app, &admin_token, "sess-stop", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id, "stop me", "running").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/stop"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "stop failed: {body}");

    // Verify the session status changed
    let row: (String,) = sqlx::query_as("SELECT status FROM agent_sessions WHERE id = $1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.0, "stopped");
}

#[sqlx::test(migrations = "./migrations")]
async fn stop_session_wrong_project_returns_404(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let project_a = create_project(&app, &admin_token, "stop-proj-a", "private").await;
    let project_b = create_project(&app, &admin_token, "stop-proj-b", "private").await;
    let session_id = insert_session(&pool, project_a, admin_id, "stop wrong", "running").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_b}/sessions/{session_id}/stop"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Permission checks
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_sessions_requires_project_read(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let project_id = create_project(&app, &admin_token, "sess-perm", "private").await;
    let (_uid, user_token) =
        create_user(&app, &admin_token, "no-read", "noread-sess@test.com").await;

    let (status, _) = helpers::get_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/sessions"),
    )
    .await;
    // Private project + no role = 404 (not 403, to avoid leaking existence)
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn list_children_empty(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let project_id = create_project(&app, &admin_token, "children-empty", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id, "no children", "running").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/children"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 0);
}

// ---------------------------------------------------------------------------
// Pagination
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_sessions_pagination(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let project_id = create_project(&app, &admin_token, "sess-page", "private").await;

    for i in 0..5 {
        insert_session(
            &pool,
            project_id,
            admin_id,
            &format!("task {i}"),
            "completed",
        )
        .await;
    }

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions?limit=2&offset=0"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    assert_eq!(body["total"].as_i64().unwrap(), 5);
}

// ---------------------------------------------------------------------------
// Spawn child session
// ---------------------------------------------------------------------------

/// Helper to get admin user ID from token.
async fn get_admin_id(app: &axum::Router, token: &str) -> Uuid {
    let (_, me) = helpers::get_json(app, token, "/api/auth/me").await;
    Uuid::parse_str(me["id"].as_str().unwrap()).unwrap()
}

/// Spawning a child session from a running parent succeeds.
#[sqlx::test(migrations = "./migrations")]
async fn spawn_child_session(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "spawn-proj", "private").await;
    let parent_id = insert_session(&pool, project_id, admin_id, "parent task", "running").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{parent_id}/spawn"),
        serde_json::json!({ "prompt": "child task" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "spawn child failed: {body}");
    assert_eq!(body["prompt"], "child task");
    assert_eq!(body["status"], "pending");

    // Verify child shows up in children list
    let (status, children) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{parent_id}/children"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(children.as_array().unwrap().len(), 1);
}

/// Spawning at max depth (5) is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn spawn_child_max_depth_rejected(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "spawn-depth", "private").await;

    // Insert parent at spawn_depth = 5 (max)
    let parent_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, spawn_depth)
         VALUES ($1, $2, $3, 'deep parent', 'running', 'claude-code', 5)",
    )
    .bind(parent_id)
    .bind(project_id)
    .bind(admin_id)
    .execute(&pool)
    .await
    .unwrap();

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{parent_id}/spawn"),
        serde_json::json!({ "prompt": "too deep" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "expected depth limit: {body}"
    );
}

/// Spawning under wrong project returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn spawn_child_wrong_project_404(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_a = create_project(&app, &admin_token, "spawn-a", "private").await;
    let project_b = create_project(&app, &admin_token, "spawn-b", "private").await;
    let parent_id = insert_session(&pool, project_a, admin_id, "parent in A", "running").await;

    // Try to spawn under project B
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_b}/sessions/{parent_id}/spawn"),
        serde_json::json!({ "prompt": "cross-project spawn" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Global send message
// ---------------------------------------------------------------------------

/// Send message to a non-running session returns 400.
#[sqlx::test(migrations = "./migrations")]
async fn send_message_global_not_running(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "msg-notrun", "private").await;
    let session_id =
        insert_session(&pool, project_id, admin_id, "stopped session", "stopped").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/sessions/{session_id}/message"),
        serde_json::json!({ "content": "hello" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Send message to nonexistent session returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn send_message_global_not_found(pool: PgPool) {
    let state = test_state(pool).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let fake_id = Uuid::new_v4();
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/sessions/{fake_id}/message"),
        serde_json::json!({ "content": "hello" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Non-owner cannot send a message via the global endpoint.
#[sqlx::test(migrations = "./migrations")]
async fn send_message_global_non_owner_forbidden(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "msg-forbid", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id, "admin session", "running").await;

    // Create a different user
    let (_uid, user_token) = create_user(&app, &admin_token, "msg-user", "msguser@test.com").await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/sessions/{session_id}/message"),
        serde_json::json!({ "content": "not my session" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Update session (link project)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn update_session_link_project(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;
    let admin_id = get_admin_id(&app, &admin_token).await;

    // Create a project-less session
    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider)
         VALUES ($1, $2, 'build app', 'running', 'claude-code')",
    )
    .bind(session_id)
    .bind(admin_id)
    .execute(&pool)
    .await
    .unwrap();

    let project_id = create_project(&app, &admin_token, "sess-link", "private").await;

    // Link the session to the project
    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/sessions/{session_id}"),
        serde_json::json!({ "project_id": project_id }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "update session failed: {body}");
    assert_eq!(body["project_id"].as_str().unwrap(), project_id.to_string());
}

#[sqlx::test(migrations = "./migrations")]
async fn update_session_non_owner_forbidden(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;
    let admin_id = get_admin_id(&app, &admin_token).await;

    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider)
         VALUES ($1, $2, 'my session', 'running', 'claude-code')",
    )
    .bind(session_id)
    .bind(admin_id)
    .execute(&pool)
    .await
    .unwrap();

    // Create another user
    let (_uid, user_token) =
        create_user(&app, &admin_token, "sess-other", "sessother@test.com").await;

    let project_id = create_project(&app, &admin_token, "sess-forbid", "private").await;

    // Other user cannot update admin's session
    let (status, _) = helpers::patch_json(
        &app,
        &user_token,
        &format!("/api/sessions/{session_id}"),
        serde_json::json!({ "project_id": project_id }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "./migrations")]
async fn update_session_invalid_project(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;
    let admin_id = get_admin_id(&app, &admin_token).await;

    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider)
         VALUES ($1, $2, 'link to bad', 'running', 'claude-code')",
    )
    .bind(session_id)
    .bind(admin_id)
    .execute(&pool)
    .await
    .unwrap();

    let fake_project = Uuid::new_v4();
    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/sessions/{session_id}"),
        serde_json::json!({ "project_id": fake_project }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Project-scoped send_message
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn send_message_project_scoped_wrong_project(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_a = create_project(&app, &admin_token, "msg-proj-a", "private").await;
    let project_b = create_project(&app, &admin_token, "msg-proj-b", "private").await;
    let session_id = insert_session(&pool, project_a, admin_id, "test msg", "running").await;

    // Send message under wrong project
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_b}/sessions/{session_id}/message"),
        serde_json::json!({ "content": "hello" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn send_message_project_scoped_non_owner(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "msg-perm", "public").await;
    let session_id = insert_session(&pool, project_id, admin_id, "admin session", "running").await;

    // Create a user with no project:write
    let (_uid, user_token) =
        create_user(&app, &admin_token, "msg-viewer", "msgviewer@test.com").await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/message"),
        serde_json::json!({ "content": "not mine" }),
    )
    .await;
    // Non-owner without project:write should be forbidden
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "./migrations")]
async fn send_message_project_scoped_empty_content(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "msg-empty", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id, "empty msg test", "running").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/message"),
        serde_json::json!({ "content": "" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Validate provider config edge cases (unit test)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_session_invalid_provider(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;
    let project_id = create_project(&app, &admin_token, "sess-bad-prov", "private").await;

    // Provider too long
    let long_provider = "a".repeat(51);
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions"),
        serde_json::json!({
            "prompt": "test",
            "provider": long_provider,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_session_empty_prompt(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;
    let project_id = create_project(&app, &admin_token, "sess-no-prompt", "private").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions"),
        serde_json::json!({ "prompt": "" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_session_with_browser_wrong_role(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;
    let project_id = create_project(&app, &admin_token, "sess-browser", "private").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions"),
        serde_json::json!({
            "prompt": "test with browser",
            "config": {
                "browser": { "allowed_origins": ["http://localhost:3000"] },
                "role": "dev"
            }
        }),
    )
    .await;
    // Browser access requires role "ui" or "test"
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn stop_session_non_owner_forbidden(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "sess-stop-perm", "public").await;
    let session_id =
        insert_session(&pool, project_id, admin_id, "admin's session", "running").await;

    let (_uid, user_token) =
        create_user(&app, &admin_token, "stop-other", "stopother@test.com").await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/stop"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
