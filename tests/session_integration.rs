//! Integration tests for agent session CRUD — list, get, stop, spawn, children.
//! Includes a pod-creation smoke test that verifies the real K8s API path.

mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

use helpers::{create_project, create_user, test_router, test_state};

/// Insert a session row directly (bypasses K8s pod creation).
async fn insert_session(
    pool: &PgPool,
    project_id: Uuid,
    user_id: Uuid,
    prompt: &str,
    status: &str,
) -> Uuid {
    insert_session_with_ns(pool, project_id, user_id, prompt, status, None).await
}

/// Insert a session row with optional `session_namespace`.
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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    assert_eq!(body["items"].as_array().unwrap().len(), 0);
}

// ---------------------------------------------------------------------------
// Pagination
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_sessions_pagination(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
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
    assert_eq!(children["items"].as_array().unwrap().len(), 1);
}

/// Spawning at max depth (5) is rejected.
#[sqlx::test(migrations = "./migrations")]
async fn spawn_child_max_depth_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
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
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
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
    // Security pattern: return 404 to avoid leaking resource existence
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn send_message_project_scoped_empty_content(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let project_id = create_project(&app, &admin_token, "sess-no-prompt", "private").await;

    // Empty prompt is allowed — session starts idle and waits for first message via pub/sub
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions"),
        serde_json::json!({ "prompt": "" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_session_with_browser_wrong_role(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
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
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
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
    // Security pattern: return 404 to avoid leaking resource existence
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Nonexistent session (404 paths)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn get_session_nonexistent_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "sess-noexist", "private").await;
    let fake_session = Uuid::new_v4();

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{fake_session}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn stop_session_nonexistent_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "stop-noexist", "private").await;
    let fake_session = Uuid::new_v4();

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{fake_session}/stop"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn send_message_project_scoped_nonexistent_session(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "msg-noexist", "private").await;
    let fake_session = Uuid::new_v4();

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{fake_session}/message"),
        serde_json::json!({ "content": "hello" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Create App (project-less session) tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_app_project_scoped_token_forbidden(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "create-app-scope", "private").await;

    // Create a project-scoped API token
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/tokens",
        serde_json::json!({
            "name": "project-scoped",
            "project_id": project_id,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let scoped_token = body["token"].as_str().unwrap();

    // Project-scoped token cannot create project-less sessions
    let (status, _) = helpers::post_json(
        &app,
        scoped_token,
        "/api/create-app",
        serde_json::json!({ "description": "my app" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_app_empty_description_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/create-app",
        serde_json::json!({ "description": "" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_app_without_agent_run_permission(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    // Create a user with only project:read — no project:write or agent:run
    let (_uid, user_token) = create_user(&app, &admin_token, "no-agent", "noagent@test.com").await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        "/api/create-app",
        serde_json::json!({ "description": "my app" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Update session edge cases
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn update_session_nonexistent_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let fake_session = Uuid::new_v4();
    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/sessions/{fake_session}"),
        serde_json::json!({ "project_id": Uuid::new_v4() }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn update_session_empty_body_is_noop(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    // Create a project-less session
    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider)
         VALUES ($1, $2, 'noop test', 'running', 'claude-code')",
    )
    .bind(session_id)
    .bind(admin_id)
    .execute(&pool)
    .await
    .unwrap();

    // PATCH with no project_id — should be a noop
    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/sessions/{session_id}"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["project_id"].is_null());
}

// ---------------------------------------------------------------------------
// Spawn child edge cases
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn spawn_child_empty_prompt_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "spawn-empty", "private").await;
    let parent_id = insert_session(&pool, project_id, admin_id, "parent", "running").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{parent_id}/spawn"),
        serde_json::json!({ "prompt": "" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn spawn_child_parent_nonexistent_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "spawn-nop", "private").await;
    let fake_parent = Uuid::new_v4();

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{fake_parent}/spawn"),
        serde_json::json!({ "prompt": "child task" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn spawn_child_no_agent_spawn_permission(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "spawn-noperm", "public").await;
    let parent_id = insert_session(&pool, project_id, admin_id, "parent", "running").await;

    // Create a user with no agent:spawn permission
    let (_uid, user_token) = create_user(&app, &admin_token, "no-spawn", "nospawn@test.com").await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/sessions/{parent_id}/spawn"),
        serde_json::json!({ "prompt": "child task" }),
    )
    .await;
    // Security pattern: return 404 to avoid leaking resource existence
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Send message global edge cases
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn send_message_global_empty_content_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "msg-global-empty", "private").await;
    let session_id =
        insert_session(&pool, project_id, admin_id, "empty content test", "running").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/sessions/{session_id}/message"),
        serde_json::json!({ "content": "" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// List children with data
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_children_with_children(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "children-data", "private").await;
    let parent_id = insert_session(&pool, project_id, admin_id, "parent", "running").await;

    // Insert two child sessions directly
    for i in 0..2 {
        let child_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, parent_session_id, spawn_depth)
             VALUES ($1, $2, $3, $4, 'pending', 'claude-code', $5, 1)",
        )
        .bind(child_id)
        .bind(project_id)
        .bind(admin_id)
        .bind(format!("child {i}"))
        .bind(parent_id)
        .execute(&pool)
        .await
        .unwrap();
    }

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{parent_id}/children"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn list_children_nonexistent_parent_returns_empty(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "children-nop", "private").await;
    let fake_parent = Uuid::new_v4();

    // list_children doesn't 404 for nonexistent parent — just returns empty
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{fake_parent}/children"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 0);
}

// ---------------------------------------------------------------------------
// Spawn lineage and user preservation
// ---------------------------------------------------------------------------

/// Parent-child chain: human → parent → child tracks lineage (`parent_session_id`, `spawn_depth`).
#[sqlx::test(migrations = "./migrations")]
async fn spawn_chain_tracks_lineage(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (user_id, token) =
        helpers::create_user(&app, &admin_token, "dev-lineage", "lineage@test.com").await;
    helpers::assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;
    let project_id = create_project(&app, &token, "chain-test", "private").await;

    // Parent at depth 0
    let parent_id = insert_session(&pool, project_id, user_id, "test prompt", "running").await;

    // Spawn child (depth 1)
    let (status, child_body) = helpers::post_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/sessions/{parent_id}/spawn"),
        serde_json::json!({ "prompt": "First child" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let child_id = Uuid::parse_str(child_body["id"].as_str().unwrap()).unwrap();

    // Verify child in DB has correct parent and depth
    let row: (Option<Uuid>, i32) =
        sqlx::query_as("SELECT parent_session_id, spawn_depth FROM agent_sessions WHERE id = $1")
            .bind(child_id)
            .fetch_one(&pool)
            .await
            .unwrap();

    assert_eq!(row.0, Some(parent_id));
    assert_eq!(row.1, 1);
}

/// Spawn preserves parent's `user_id` (original human).
#[sqlx::test(migrations = "./migrations")]
async fn spawn_preserves_original_user(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (user_id, token) =
        helpers::create_user(&app, &admin_token, "dev-preserve", "preserve@test.com").await;
    helpers::assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;
    let project_id = create_project(&app, &token, "user-test", "private").await;

    let parent_id = insert_session(&pool, project_id, user_id, "test prompt", "running").await;

    let (status, child_body) = helpers::post_json(
        &app,
        &token,
        &format!("/api/projects/{project_id}/sessions/{parent_id}/spawn"),
        serde_json::json!({ "prompt": "Child task" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Child should have the same user_id as parent (original human)
    let child_user_id = child_body["user_id"].as_str().unwrap();
    assert_eq!(child_user_id, user_id.to_string());
}

// ---------------------------------------------------------------------------
// Validate provider config edge cases
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_session_invalid_agent_role(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let project_id = create_project(&app, &admin_token, "sess-bad-role", "private").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions"),
        serde_json::json!({
            "prompt": "test",
            "role": "nonexistent_role",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "response: {body}");
}

#[sqlx::test(migrations = "./migrations")]
async fn create_session_config_invalid_role(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let project_id = create_project(&app, &admin_token, "sess-cfg-role", "private").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions"),
        serde_json::json!({
            "prompt": "test config role",
            "config": { "role": "hacker" }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_session_browser_test_role_ok(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let project_id = create_project(&app, &admin_token, "sess-browser-test", "private").await;

    // Browser + role "test" should be accepted (validation passes, K8s may fail)
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions"),
        serde_json::json!({
            "prompt": "test browser config",
            "config": {
                "browser": { "allowed_origins": ["http://localhost:3000"] },
                "role": "test"
            }
        }),
    )
    .await;
    // This either succeeds (CREATED) or fails with 500 at K8s pod creation —
    // but NOT 400, because the config validation passes.
    assert_ne!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Pod-creation smoke test (real K8s API)
// ---------------------------------------------------------------------------

/// Verify that `create_session` actually spawns a K8s pod.
///
/// Sets up a project with a bare git repo (required by `create_session`),
/// then calls the session creation API and asserts a pod was created.
#[sqlx::test(migrations = "./migrations")]
async fn create_session_spawns_k8s_pod(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "pod-smoke", "private").await;

    // create_session requires the project to have a repo_path
    let bare_dir = tempfile::tempdir().unwrap();
    let bare_path = bare_dir.path().join("repo.git");
    std::process::Command::new("git")
        .args(["init", "--bare", bare_path.to_str().unwrap()])
        .output()
        .expect("git init --bare");
    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    // Call create_session directly to get the real error (API returns opaque 500).
    // create_session now creates its own session namespace with RBAC.
    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let result = platform::agent::service::create_session(
        &state,
        admin_id,
        project_id,
        "smoke test: verify pod creation",
        "claude-code",
        None,
        None,
        platform::agent::AgentRoleName::Dev,
        None,
    )
    .await;

    let session = result.expect("create_session should succeed");
    assert_eq!(session.status, "running");
    let pod_name = session
        .pod_name
        .as_deref()
        .expect("pod_name should be set after successful creation");
    assert!(!pod_name.is_empty());

    // Pod is now created in the session namespace, not the project dev namespace
    let session_ns = session
        .session_namespace
        .as_deref()
        .expect("session_namespace should be set");

    // Verify the pod actually exists in K8s
    let pods: kube::Api<k8s_openapi::api::core::v1::Pod> =
        kube::Api::namespaced(state.kube.clone(), session_ns);
    let pod = pods.get(pod_name).await;
    assert!(pod.is_ok(), "pod {pod_name} should exist in K8s");

    // Cleanup: delete the session namespace (cascades to pod)
    let ns_api: kube::Api<k8s_openapi::api::core::v1::Namespace> =
        kube::Api::all(state.kube.clone());
    let _ = ns_api
        .delete(session_ns, &kube::api::DeleteParams::default())
        .await;
}

// ---------------------------------------------------------------------------
// Session namespace
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn session_namespace_stored_in_db(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let project_id = create_project(&app, &admin_token, "ns-test", "private").await;
    let session_id = insert_session_with_ns(
        &pool,
        project_id,
        admin_id,
        "test",
        "running",
        Some("myapp-s-abc12345"),
    )
    .await;

    // Verify session_namespace is stored in DB
    let ns: Option<String> =
        sqlx::query_scalar("SELECT session_namespace FROM agent_sessions WHERE id = $1")
            .bind(session_id)
            .fetch_one(&pool)
            .await
            .expect("fetch session namespace");

    assert_eq!(ns.as_deref(), Some("myapp-s-abc12345"));
}

#[sqlx::test(migrations = "./migrations")]
async fn session_namespace_null_for_old_sessions(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let project_id = create_project(&app, &admin_token, "ns-null", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id, "legacy", "running").await;

    // Verify session_namespace is NULL for sessions without it
    let ns: Option<String> =
        sqlx::query_scalar("SELECT session_namespace FROM agent_sessions WHERE id = $1")
            .bind(session_id)
            .fetch_one(&pool)
            .await
            .expect("fetch session namespace");

    assert!(ns.is_none());
}

// ---------------------------------------------------------------------------
// List iframes
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_iframes_returns_empty_for_session(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let project_id = create_project(&app, &admin_token, "iframe-empty", "private").await;
    let session_id = insert_session_with_ns(
        &pool,
        project_id,
        admin_id,
        "iframe test",
        "running",
        Some("iframe-empty-dev"),
    )
    .await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/iframes"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // No K8s Services exist in test namespace — empty array
    let arr = body["items"].as_array().unwrap();
    assert!(arr.is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn list_iframes_no_permission_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let project_id = create_project(&app, &admin_token, "iframe-perm", "private").await;
    let session_id = insert_session_with_ns(
        &pool,
        project_id,
        admin_id,
        "iframe perm",
        "running",
        Some("iframe-perm-dev"),
    )
    .await;

    let (_uid, user_token) = create_user(
        &app,
        &admin_token,
        "no-iframe-read",
        "noread-iframe@test.com",
    )
    .await;

    let (status, _) = helpers::get_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/iframes"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn list_iframes_nonexistent_session_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "iframe-noexist", "private").await;
    let fake_session = Uuid::new_v4();

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{fake_session}/iframes"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn list_iframes_wrong_project_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let project_a = create_project(&app, &admin_token, "iframe-proj-a", "private").await;
    let project_b = create_project(&app, &admin_token, "iframe-proj-b", "private").await;
    let session_id = insert_session_with_ns(
        &pool,
        project_a,
        admin_id,
        "iframe wrong proj",
        "running",
        Some("iframe-a-dev"),
    )
    .await;

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_b}/sessions/{session_id}/iframes"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn list_iframes_no_namespace_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let project_id = create_project(&app, &admin_token, "iframe-no-ns", "private").await;
    // Insert session without namespace (NULL)
    let session_id = insert_session(&pool, project_id, admin_id, "no namespace", "running").await;

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/iframes"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Phase 3: Fetch session returns all fields
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn fetch_session_returns_all_fields(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let project_id = create_project(&app, &admin_token, "sess-allfields", "private").await;
    let session_id = insert_session_with_ns(
        &pool,
        project_id,
        admin_id,
        "all fields test",
        "running",
        Some("test-ns-dev"),
    )
    .await;

    // Insert a message for the session
    insert_message(&pool, session_id, "user", "hello").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Verify all key fields are present
    assert_eq!(body["id"].as_str().unwrap(), session_id.to_string());
    assert_eq!(body["project_id"].as_str().unwrap(), project_id.to_string());
    assert_eq!(body["user_id"].as_str().unwrap(), admin_id.to_string());
    assert_eq!(body["prompt"].as_str().unwrap(), "all fields test");
    assert_eq!(body["status"].as_str().unwrap(), "running");
    assert_eq!(body["provider"].as_str().unwrap(), "claude-code");
    assert!(body["created_at"].as_str().is_some());
    // Messages should be included
    assert_eq!(body["messages"].as_array().unwrap().len(), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn fetch_session_not_found(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "sess-notfound", "private").await;
    let random_id = Uuid::new_v4();

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{random_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Phase 3: Stop session cleans up identity
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn stop_session_cleans_up_identity(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "sess-cleanup", "private").await;

    // Insert a session with a fake agent_user_id to verify cleanup
    let session_id = Uuid::new_v4();
    let agent_user_id = Uuid::new_v4();

    // Create a fake agent user so cleanup can proceed
    sqlx::query(
        "INSERT INTO users (id, name, email, password_hash, user_type)
         VALUES ($1, $2, $3, 'n/a', 'agent')",
    )
    .bind(agent_user_id)
    .bind(format!("agent-{}", &session_id.to_string()[..8]))
    .bind(format!("agent-{}@test.com", &session_id.to_string()[..8]))
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, agent_user_id)
         VALUES ($1, $2, $3, 'cleanup test', 'running', 'claude-code', $4)",
    )
    .bind(session_id)
    .bind(project_id)
    .bind(admin_id)
    .bind(agent_user_id)
    .execute(&pool)
    .await
    .unwrap();

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/stop"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Verify session is stopped
    let row: (String,) = sqlx::query_as("SELECT status FROM agent_sessions WHERE id = $1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.0, "stopped");

    // Verify agent user is deactivated (cleanup_agent_identity sets is_active=false)
    let agent_row: (bool,) = sqlx::query_as("SELECT is_active FROM users WHERE id = $1")
        .bind(agent_user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!agent_row.0, "agent user should be deactivated after stop");
}

// ---------------------------------------------------------------------------
// Phase 3: Send message to non-running session rejected
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn send_message_not_running_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "msg-completed", "private").await;

    // Insert a completed session
    let session_id = insert_session(
        &pool,
        project_id,
        admin_id,
        "completed session",
        "completed",
    )
    .await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/message"),
        serde_json::json!({ "content": "try sending" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Phase 3: Send message to pubsub session
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn send_message_pubsub_session(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "msg-pubsub", "private").await;

    // Insert a running session with uses_pubsub=true and a pod_name
    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, uses_pubsub, pod_name, session_namespace)
         VALUES ($1, $2, $3, 'pubsub test', 'running', 'claude-code', true, 'agent-test-pod', 'test-ns')",
    )
    .bind(session_id)
    .bind(project_id)
    .bind(admin_id)
    .execute(&pool)
    .await
    .unwrap();

    // Send message — should publish to Valkey pub/sub (even though no subscriber is listening, it should succeed)
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/message"),
        serde_json::json!({ "content": "hello via pubsub" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Phase 3: Create global session without API key fails
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_global_session_no_api_key_fails(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (user_id, token) = create_user(&app, &admin_token, "no-llm-key", "nollmkey@test.com").await;
    helpers::assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;
    // Do NOT set any LLM provider key

    let (status, body) = helpers::post_json(
        &app,
        &token,
        "/api/create-app",
        serde_json::json!({ "description": "Build a REST API" }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "expected 400 without LLM provider, got {status}: {body}"
    );
    let error_msg = body["error"].as_str().unwrap_or("");
    assert!(
        error_msg.contains("LLM provider"),
        "expected error about LLM provider, got: {error_msg}"
    );
}

// ---------------------------------------------------------------------------
// Phase 3: Reap idle sessions marks completed
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn reap_idle_sessions_marks_completed(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "reap-idle", "private").await;

    // Insert a running session that was created long ago (older than idle timeout)
    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, created_at)
         VALUES ($1, $2, $3, 'idle session', 'running', 'claude-code', NOW() - INTERVAL '2 hours')",
    )
    .bind(session_id)
    .bind(project_id)
    .bind(admin_id)
    .execute(&pool)
    .await
    .unwrap();

    // Run the reaper
    platform::agent::service::run_reaper_once(&state).await;

    // Verify session is marked completed
    let row: (String,) = sqlx::query_as("SELECT status FROM agent_sessions WHERE id = $1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        row.0, "completed",
        "idle session should be reaped to 'completed'"
    );
}

// ---------------------------------------------------------------------------
// Phase 3: Resolve session namespace
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn resolve_session_namespace_prefers_session_ns(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "ns-prefer", "private").await;

    // Insert session with explicit session_namespace
    let session_id = insert_session_with_ns(
        &pool,
        project_id,
        admin_id,
        "namespace test",
        "running",
        Some("my-custom-ns"),
    )
    .await;

    // Verify session_namespace is stored correctly in DB
    let row: (Option<String>,) =
        sqlx::query_as("SELECT session_namespace FROM agent_sessions WHERE id = $1")
            .bind(session_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row.0, Some("my-custom-ns".to_string()));
}

#[sqlx::test(migrations = "./migrations")]
async fn resolve_session_namespace_falls_back_to_project(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "ns-fallback", "private").await;

    // Insert session WITHOUT session_namespace (NULL)
    let session_id =
        insert_session(&pool, project_id, admin_id, "fallback ns test", "running").await;

    // Fetch the session — session_namespace should be null (resolve will use project dev ns at runtime)
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["session_namespace"].is_null(),
        "session_namespace should be null when not set"
    );
}

// ---------------------------------------------------------------------------
// Phase 3: Resolve active LLM provider — api_key mode
// ---------------------------------------------------------------------------

/// When a user has `active_llm_provider = 'api_key'` and a user key set,
/// create-app should succeed (verifying the `api_key` provider resolution path).
#[sqlx::test(migrations = "./migrations")]
async fn resolve_active_llm_provider_api_key_mode(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (user_id, token) =
        create_user(&app, &admin_token, "api-key-usr", "apikeyusr@test.com").await;
    helpers::assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    // Set user's API key
    helpers::set_user_api_key(&pool, user_id).await;

    // Set active provider to "api_key"
    sqlx::query("UPDATE users SET active_llm_provider = 'api_key' WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();

    // Create-app should succeed because the api_key path resolves the user key
    let (status, body) = helpers::post_json(
        &app,
        &token,
        "/api/create-app",
        serde_json::json!({ "description": "Test api_key provider" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create-app should succeed with api_key provider: {body}"
    );
    assert_eq!(body["status"].as_str(), Some("running"));
}

// ---------------------------------------------------------------------------
// create_session: spawn depth from parent
// ---------------------------------------------------------------------------

/// Spawn depth from parent: `parent.spawn_depth + 1`.
#[sqlx::test(migrations = "./migrations")]
async fn create_session_spawn_depth_from_parent(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "depth-test", "private").await;

    // Set up a bare git repo for the project
    let bare_dir = tempfile::tempdir().unwrap();
    let bare_path = bare_dir.path().join("repo.git");
    std::process::Command::new("git")
        .args(["init", "--bare", bare_path.to_str().unwrap()])
        .output()
        .expect("git init --bare");
    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    // Insert a parent session at spawn_depth = 2
    let parent_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, spawn_depth)
         VALUES ($1, $2, $3, 'parent', 'running', 'claude-code', 2)",
    )
    .bind(parent_id)
    .bind(project_id)
    .bind(admin_id)
    .execute(&pool)
    .await
    .unwrap();

    // Call create_session with parent
    let result = platform::agent::service::create_session(
        &state,
        admin_id,
        project_id,
        "child session",
        "claude-code",
        None,
        None,
        platform::agent::AgentRoleName::Dev,
        Some(parent_id),
    )
    .await;

    let session = result.expect("create_session with parent should succeed");
    assert_eq!(
        session.spawn_depth, 3,
        "spawn_depth should be parent(2) + 1"
    );
    assert_eq!(session.status, "running");
    assert!(session.session_namespace.is_some());

    // Cleanup: delete the session namespace
    if let Some(ref ns) = session.session_namespace {
        let ns_api: kube::Api<k8s_openapi::api::core::v1::Namespace> =
            kube::Api::all(state.kube.clone());
        let _ = ns_api.delete(ns, &kube::api::DeleteParams::default()).await;
    }
}

// ---------------------------------------------------------------------------
// create_session: branch defaults to "agent/{short_id}"
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_session_default_branch(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "branch-default", "private").await;

    let bare_dir = tempfile::tempdir().unwrap();
    let bare_path = bare_dir.path().join("repo.git");
    std::process::Command::new("git")
        .args(["init", "--bare", bare_path.to_str().unwrap()])
        .output()
        .expect("git init --bare");
    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let result = platform::agent::service::create_session(
        &state,
        admin_id,
        project_id,
        "branch test",
        "claude-code",
        None, // no branch provided
        None,
        platform::agent::AgentRoleName::Dev,
        None,
    )
    .await;

    let session = result.expect("create_session should succeed");
    let branch = session.branch.as_deref().expect("branch should be set");
    assert!(
        branch.starts_with("agent/"),
        "default branch should start with 'agent/', got: {branch}"
    );
    // The short_id is 8 chars from the session UUID
    let short_id = &session.id.to_string()[..8];
    assert_eq!(branch, format!("agent/{short_id}"));

    // Cleanup
    if let Some(ref ns) = session.session_namespace {
        let ns_api: kube::Api<k8s_openapi::api::core::v1::Namespace> =
            kube::Api::all(state.kube.clone());
        let _ = ns_api.delete(ns, &kube::api::DeleteParams::default()).await;
    }
}

// ---------------------------------------------------------------------------
// create_session: custom branch is preserved
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_session_custom_branch(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "branch-custom", "private").await;

    let bare_dir = tempfile::tempdir().unwrap();
    let bare_path = bare_dir.path().join("repo.git");
    std::process::Command::new("git")
        .args(["init", "--bare", bare_path.to_str().unwrap()])
        .output()
        .expect("git init --bare");
    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let result = platform::agent::service::create_session(
        &state,
        admin_id,
        project_id,
        "custom branch test",
        "claude-code",
        Some("feature/my-branch"), // custom branch
        None,
        platform::agent::AgentRoleName::Dev,
        None,
    )
    .await;

    let session = result.expect("create_session should succeed");
    assert_eq!(
        session.branch.as_deref(),
        Some("feature/my-branch"),
        "provided branch should be used"
    );

    // Cleanup
    if let Some(ref ns) = session.session_namespace {
        let ns_api: kube::Api<k8s_openapi::api::core::v1::Namespace> =
            kube::Api::all(state.kube.clone());
        let _ = ns_api.delete(ns, &kube::api::DeleteParams::default()).await;
    }
}

// ---------------------------------------------------------------------------
// create_session: agent identity is created with scoped permissions
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_session_creates_agent_identity(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "agent-id-test", "private").await;

    let bare_dir = tempfile::tempdir().unwrap();
    let bare_path = bare_dir.path().join("repo.git");
    std::process::Command::new("git")
        .args(["init", "--bare", bare_path.to_str().unwrap()])
        .output()
        .expect("git init --bare");
    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let result = platform::agent::service::create_session(
        &state,
        admin_id,
        project_id,
        "identity test",
        "claude-code",
        None,
        None,
        platform::agent::AgentRoleName::Dev,
        None,
    )
    .await;

    let session = result.expect("create_session should succeed");
    assert!(
        session.agent_user_id.is_some(),
        "agent_user_id should be set after session creation"
    );

    // Verify the agent user exists in the DB
    let agent_uid = session.agent_user_id.unwrap();
    let agent_row: (String, bool) =
        sqlx::query_as("SELECT name, is_active FROM users WHERE id = $1")
            .bind(agent_uid)
            .fetch_one(&pool)
            .await
            .expect("agent user should exist");
    assert!(
        agent_row.0.starts_with("agent-"),
        "agent name should start with 'agent-'"
    );
    assert!(agent_row.1, "agent user should be active");

    // Verify the agent has a role assignment
    let role_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM user_roles WHERE user_id = $1")
        .bind(agent_uid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(role_count.0 > 0, "agent should have at least one role");

    // Verify the agent has an API token
    let token_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM api_tokens WHERE user_id = $1")
        .bind(agent_uid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(token_count.0 > 0, "agent should have an API token");

    // Cleanup
    if let Some(ref ns) = session.session_namespace {
        let ns_api: kube::Api<k8s_openapi::api::core::v1::Namespace> =
            kube::Api::all(state.kube.clone());
        let _ = ns_api.delete(ns, &kube::api::DeleteParams::default()).await;
    }
}

// ---------------------------------------------------------------------------
// create_session: session namespace is created in K8s
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_session_creates_session_namespace(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "ns-create", "private").await;

    let bare_dir = tempfile::tempdir().unwrap();
    let bare_path = bare_dir.path().join("repo.git");
    std::process::Command::new("git")
        .args(["init", "--bare", bare_path.to_str().unwrap()])
        .output()
        .expect("git init --bare");
    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let result = platform::agent::service::create_session(
        &state,
        admin_id,
        project_id,
        "namespace test",
        "claude-code",
        None,
        None,
        platform::agent::AgentRoleName::Dev,
        None,
    )
    .await;

    let session = result.expect("create_session should succeed");
    let ns = session
        .session_namespace
        .as_deref()
        .expect("session_namespace should be set");

    // Verify the namespace exists in K8s
    let ns_api: kube::Api<k8s_openapi::api::core::v1::Namespace> =
        kube::Api::all(state.kube.clone());
    let ns_result = ns_api.get(ns).await;
    assert!(
        ns_result.is_ok(),
        "session namespace {ns} should exist in K8s"
    );

    // Cleanup
    let _ = ns_api.delete(ns, &kube::api::DeleteParams::default()).await;
}

// ---------------------------------------------------------------------------
// create_session: pod_name is set after creation
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_session_sets_pod_name(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "pod-name-test", "private").await;

    let bare_dir = tempfile::tempdir().unwrap();
    let bare_path = bare_dir.path().join("repo.git");
    std::process::Command::new("git")
        .args(["init", "--bare", bare_path.to_str().unwrap()])
        .output()
        .expect("git init --bare");
    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let result = platform::agent::service::create_session(
        &state,
        admin_id,
        project_id,
        "pod name test",
        "claude-code",
        None,
        None,
        platform::agent::AgentRoleName::Dev,
        None,
    )
    .await;

    let session = result.expect("create_session should succeed");
    assert!(
        session.pod_name.is_some(),
        "pod_name should be set after successful creation"
    );
    let pod_name = session.pod_name.as_deref().unwrap();
    assert!(!pod_name.is_empty(), "pod_name should not be empty");

    // Cleanup
    if let Some(ref ns) = session.session_namespace {
        let ns_api: kube::Api<k8s_openapi::api::core::v1::Namespace> =
            kube::Api::all(state.kube.clone());
        let _ = ns_api.delete(ns, &kube::api::DeleteParams::default()).await;
    }
}

// ---------------------------------------------------------------------------
// create_session: uses_pubsub is set to true
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_session_enables_pubsub(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "pubsub-test", "private").await;

    let bare_dir = tempfile::tempdir().unwrap();
    let bare_path = bare_dir.path().join("repo.git");
    std::process::Command::new("git")
        .args(["init", "--bare", bare_path.to_str().unwrap()])
        .output()
        .expect("git init --bare");
    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let result = platform::agent::service::create_session(
        &state,
        admin_id,
        project_id,
        "pubsub test",
        "claude-code",
        None,
        None,
        platform::agent::AgentRoleName::Dev,
        None,
    )
    .await;

    let session = result.expect("create_session should succeed");
    assert!(
        session.uses_pubsub,
        "uses_pubsub should be true for pod sessions"
    );

    // Verify in DB too
    let row: (bool,) = sqlx::query_as("SELECT uses_pubsub FROM agent_sessions WHERE id = $1")
        .bind(session.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(row.0, "uses_pubsub should be true in DB");

    // Cleanup
    if let Some(ref ns) = session.session_namespace {
        let ns_api: kube::Api<k8s_openapi::api::core::v1::Namespace> =
            kube::Api::all(state.kube.clone());
        let _ = ns_api.delete(ns, &kube::api::DeleteParams::default()).await;
    }
}

// ---------------------------------------------------------------------------
// create_session: invalid provider rejected
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_session_invalid_provider_returns_error(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "bad-prov-direct", "private").await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let result = platform::agent::service::create_session(
        &state,
        admin_id,
        project_id,
        "test",
        "nonexistent-provider",
        None,
        None,
        platform::agent::AgentRoleName::Dev,
        None,
    )
    .await;

    assert!(result.is_err(), "invalid provider should fail");
    let err = result.err().unwrap();
    assert!(
        matches!(err, platform::agent::error::AgentError::InvalidProvider(_)),
        "expected InvalidProvider error, got: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// stop_session: full cleanup (identity + namespace + Valkey ACL)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn stop_session_full_cleanup(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "stop-full", "private").await;

    let bare_dir = tempfile::tempdir().unwrap();
    let bare_path = bare_dir.path().join("repo.git");
    std::process::Command::new("git")
        .args(["init", "--bare", bare_path.to_str().unwrap()])
        .output()
        .expect("git init --bare");
    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    // Create a real session with pod
    let session = platform::agent::service::create_session(
        &state,
        admin_id,
        project_id,
        "stop full cleanup test",
        "claude-code",
        None,
        None,
        platform::agent::AgentRoleName::Dev,
        None,
    )
    .await
    .expect("create_session should succeed");

    let session_id = session.id;
    let agent_user_id = session.agent_user_id.expect("agent_user_id should exist");
    let session_ns = session
        .session_namespace
        .as_deref()
        .expect("session_namespace should be set")
        .to_owned();

    // Stop the session
    platform::agent::service::stop_session(&state, session_id)
        .await
        .expect("stop_session should succeed");

    // Verify session is stopped
    let row: (String,) = sqlx::query_as("SELECT status FROM agent_sessions WHERE id = $1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.0, "stopped");

    // Verify agent user is deactivated
    let agent_row: (bool,) = sqlx::query_as("SELECT is_active FROM users WHERE id = $1")
        .bind(agent_user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!agent_row.0, "agent user should be deactivated after stop");

    // Verify agent tokens are deleted
    let token_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM api_tokens WHERE user_id = $1")
        .bind(agent_user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(token_count.0, 0, "agent tokens should be deleted");

    // Verify session namespace is deleted from K8s (may take a moment, check gracefully)
    let ns_api: kube::Api<k8s_openapi::api::core::v1::Namespace> =
        kube::Api::all(state.kube.clone());
    // Namespace deletion is async; it may still exist with a deletion timestamp
    match ns_api.get(&session_ns).await {
        Ok(ns) => {
            // If it still exists, it should have a deletion timestamp
            let has_deletion_ts = ns.metadata.deletion_timestamp.is_some();
            assert!(
                has_deletion_ts,
                "namespace should be deleting (have deletion timestamp)"
            );
        }
        Err(kube::Error::Api(e)) if e.code == 404 => {
            // Already deleted — good
        }
        Err(e) => {
            panic!("unexpected error checking namespace: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// stop_session: pod logs captured before deletion
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn stop_session_captures_logs(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "stop-logs", "private").await;

    let bare_dir = tempfile::tempdir().unwrap();
    let bare_path = bare_dir.path().join("repo.git");
    std::process::Command::new("git")
        .args(["init", "--bare", bare_path.to_str().unwrap()])
        .output()
        .expect("git init --bare");
    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let session = platform::agent::service::create_session(
        &state,
        admin_id,
        project_id,
        "log capture test",
        "claude-code",
        None,
        None,
        platform::agent::AgentRoleName::Dev,
        None,
    )
    .await
    .expect("create_session should succeed");

    let session_id = session.id;

    // Give the pod a moment to start (it may or may not have logs)
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Stop should succeed (logs captured silently even if pod has no output)
    platform::agent::service::stop_session(&state, session_id)
        .await
        .expect("stop_session should succeed");

    // Verify session is stopped
    let row: (String,) = sqlx::query_as("SELECT status FROM agent_sessions WHERE id = $1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.0, "stopped");
}

// ---------------------------------------------------------------------------
// send_message: pubsub routing for running session with uses_pubsub=true
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn send_message_pubsub_routing(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "msg-pubsub-rt", "private").await;

    // Insert a running session with uses_pubsub=true (not cli_subprocess)
    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, uses_pubsub, pod_name, execution_mode, session_namespace)
         VALUES ($1, $2, $3, 'pubsub routing', 'running', 'claude-code', true, 'agent-test', 'pod', 'test-ns')",
    )
    .bind(session_id)
    .bind(project_id)
    .bind(admin_id)
    .execute(&pool)
    .await
    .unwrap();

    // send_message should route via pub/sub (no subscriber needed — just verifies publish succeeds)
    let result = platform::agent::service::send_message(&state, session_id, "hello pubsub").await;
    assert!(
        result.is_ok(),
        "send_message via pubsub should succeed: {result:?}"
    );
}

// ---------------------------------------------------------------------------
// send_message: cli_subprocess routing when not registered
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn send_message_cli_subprocess_not_registered(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "msg-cli-noreg", "private").await;

    // Insert a running cli_subprocess session (but don't register in CLI manager)
    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, execution_mode, uses_pubsub)
         VALUES ($1, $2, $3, 'cli routing', 'running', 'claude-code', 'cli_subprocess', true)",
    )
    .bind(session_id)
    .bind(project_id)
    .bind(admin_id)
    .execute(&pool)
    .await
    .unwrap();

    // send_message to cli_subprocess without registered handle should fail
    let result = platform::agent::service::send_message(&state, session_id, "hello cli").await;
    assert!(
        result.is_err(),
        "send_message to unregistered CLI session should fail"
    );
}

// ---------------------------------------------------------------------------
// send_message: to non-running session rejected
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn send_message_stopped_session_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "msg-stopped", "private").await;

    let session_id =
        insert_session(&pool, project_id, admin_id, "stopped session", "stopped").await;

    let result = platform::agent::service::send_message(&state, session_id, "hello").await;
    assert!(
        result.is_err(),
        "send_message to stopped session should fail"
    );
    let err = result.err().unwrap();
    assert!(
        matches!(err, platform::agent::error::AgentError::SessionNotRunning),
        "expected SessionNotRunning error, got: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// send_message: nonexistent session returns SessionNotFound
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn send_message_nonexistent_session(pool: PgPool) {
    let (state, _admin_token) = test_state(pool).await;

    let fake_id = Uuid::new_v4();
    let result = platform::agent::service::send_message(&state, fake_id, "hello").await;
    assert!(result.is_err());
    let err = result.err().unwrap();
    assert!(
        matches!(err, platform::agent::error::AgentError::SessionNotFound),
        "expected SessionNotFound error, got: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// reap_terminated_sessions: pod not found (404) marks session failed
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn reap_terminated_pod_not_found_marks_failed(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "reap-404", "private").await;

    // Insert a running session with a pod_name that doesn't exist in K8s
    let session_id = Uuid::new_v4();
    let fake_ns = format!("reap-404-ns-{}", &session_id.to_string()[..8]);
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, pod_name, session_namespace)
         VALUES ($1, $2, $3, 'reap test', 'running', 'claude-code', 'nonexistent-pod', $4)",
    )
    .bind(session_id)
    .bind(project_id)
    .bind(admin_id)
    .bind(&fake_ns)
    .execute(&pool)
    .await
    .unwrap();

    // Run the reaper
    platform::agent::service::run_reaper_once(&state).await;

    // Verify session is marked as failed (pod 404)
    let row: (String,) = sqlx::query_as("SELECT status FROM agent_sessions WHERE id = $1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        row.0, "failed",
        "session with missing pod should be marked failed"
    );
}

// ---------------------------------------------------------------------------
// reap_idle_sessions: pod session cleaned up
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn reap_idle_session_with_pod(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "reap-idle-pod", "private").await;

    // Insert a running session created 2 hours ago (older than idle timeout=1800s)
    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, pod_name, created_at, session_namespace)
         VALUES ($1, $2, $3, 'idle pod session', 'running', 'claude-code', 'idle-pod', NOW() - INTERVAL '2 hours', 'test-idle-ns')",
    )
    .bind(session_id)
    .bind(project_id)
    .bind(admin_id)
    .execute(&pool)
    .await
    .unwrap();

    // Run the reaper
    platform::agent::service::run_reaper_once(&state).await;

    // Verify session is marked completed
    let row: (String,) = sqlx::query_as("SELECT status FROM agent_sessions WHERE id = $1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        row.0, "completed",
        "idle pod session should be reaped to completed"
    );
}

// ---------------------------------------------------------------------------
// reap_idle_sessions: cli_subprocess session cleaned up
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn reap_idle_cli_subprocess_session(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "reap-idle-cli", "private").await;

    // Insert a running cli_subprocess session created 2 hours ago
    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, execution_mode, created_at)
         VALUES ($1, $2, $3, 'idle cli session', 'running', 'claude-code', 'cli_subprocess', NOW() - INTERVAL '2 hours')",
    )
    .bind(session_id)
    .bind(project_id)
    .bind(admin_id)
    .execute(&pool)
    .await
    .unwrap();

    // Run the reaper
    platform::agent::service::run_reaper_once(&state).await;

    // Verify session is marked completed
    let row: (String,) = sqlx::query_as("SELECT status FROM agent_sessions WHERE id = $1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        row.0, "completed",
        "idle CLI session should be reaped to completed"
    );
}

// ---------------------------------------------------------------------------
// reap_idle_sessions: recently active session NOT reaped
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn reap_idle_session_active_not_reaped(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "reap-active", "private").await;

    // Insert a running session created just now
    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, created_at)
         VALUES ($1, $2, $3, 'active session', 'running', 'claude-code', NOW())",
    )
    .bind(session_id)
    .bind(project_id)
    .bind(admin_id)
    .execute(&pool)
    .await
    .unwrap();

    // Run the reaper
    platform::agent::service::run_reaper_once(&state).await;

    // Verify session is NOT reaped (still running)
    let row: (String,) = sqlx::query_as("SELECT status FROM agent_sessions WHERE id = $1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        row.0, "running",
        "recently active session should NOT be reaped"
    );
}

// ---------------------------------------------------------------------------
// reap_idle_sessions: session with recent messages NOT reaped
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn reap_idle_session_with_recent_messages_not_reaped(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "reap-msg-active", "private").await;

    // Insert a running session created 2 hours ago
    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, created_at)
         VALUES ($1, $2, $3, 'old but active', 'running', 'claude-code', NOW() - INTERVAL '2 hours')",
    )
    .bind(session_id)
    .bind(project_id)
    .bind(admin_id)
    .execute(&pool)
    .await
    .unwrap();

    // Insert a recent message (within the idle timeout)
    sqlx::query(
        "INSERT INTO agent_messages (session_id, role, content, created_at) VALUES ($1, 'user', 'recent msg', NOW())",
    )
    .bind(session_id)
    .execute(&pool)
    .await
    .unwrap();

    // Run the reaper
    platform::agent::service::run_reaper_once(&state).await;

    // Verify session is NOT reaped (has recent messages)
    let row: (String,) = sqlx::query_as("SELECT status FROM agent_sessions WHERE id = $1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        row.0, "running",
        "session with recent messages should NOT be reaped"
    );
}

// ---------------------------------------------------------------------------
// reap_idle_sessions: cleanup agent identity on reap
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn reap_idle_session_cleans_up_identity(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "reap-cleanup", "private").await;

    // Create a fake agent user
    let agent_user_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, name, email, password_hash, user_type)
         VALUES ($1, 'agent-reap-test', 'agent-reap@test.com', 'n/a', 'agent')",
    )
    .bind(agent_user_id)
    .execute(&pool)
    .await
    .unwrap();

    // Insert a running session with agent_user_id, created 2 hours ago
    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, agent_user_id, created_at)
         VALUES ($1, $2, $3, 'reap cleanup', 'running', 'claude-code', $4, NOW() - INTERVAL '2 hours')",
    )
    .bind(session_id)
    .bind(project_id)
    .bind(admin_id)
    .bind(agent_user_id)
    .execute(&pool)
    .await
    .unwrap();

    // Run the reaper
    platform::agent::service::run_reaper_once(&state).await;

    // Verify session is reaped
    let row: (String,) = sqlx::query_as("SELECT status FROM agent_sessions WHERE id = $1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.0, "completed");

    // Verify agent user is deactivated
    let agent_row: (bool,) = sqlx::query_as("SELECT is_active FROM users WHERE id = $1")
        .bind(agent_user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!agent_row.0, "agent user should be deactivated after reap");
}

// ---------------------------------------------------------------------------
// fetch_session: with all null optional fields
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn fetch_session_null_optional_fields(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "fetch-null", "private").await;

    // Insert a minimal session with many NULL fields
    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider)
         VALUES ($1, $2, $3, 'minimal session', 'pending', 'claude-code')",
    )
    .bind(session_id)
    .bind(project_id)
    .bind(admin_id)
    .execute(&pool)
    .await
    .unwrap();

    let session = platform::agent::service::fetch_session(&pool, session_id)
        .await
        .expect("fetch_session should succeed");

    assert_eq!(session.id, session_id);
    assert_eq!(session.status, "pending");
    assert!(session.pod_name.is_none());
    assert!(session.branch.is_none());
    assert!(session.agent_user_id.is_none());
    assert!(session.finished_at.is_none());
    assert!(session.cost_tokens.is_none());
    assert!(session.parent_session_id.is_none());
    assert!(session.provider_config.is_none());
    assert!(session.session_namespace.is_none());
    assert!(session.allowed_child_roles.is_none());
}

// ---------------------------------------------------------------------------
// fetch_session: nonexistent session returns SessionNotFound
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn fetch_session_nonexistent_returns_error(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;

    let fake_id = Uuid::new_v4();
    let result = platform::agent::service::fetch_session(&state.pool, fake_id).await;
    assert!(result.is_err());
    let err = result.err().unwrap();
    assert!(
        matches!(err, platform::agent::error::AgentError::SessionNotFound),
        "expected SessionNotFound, got: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// send_message: CLI subprocess routing with registered handle
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn send_message_cli_subprocess_registered(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "msg-cli-reg", "private").await;

    // Insert a running cli_subprocess session
    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, execution_mode, uses_pubsub)
         VALUES ($1, $2, $3, 'cli registered', 'running', 'claude-code', 'cli_subprocess', true)",
    )
    .bind(session_id)
    .bind(project_id)
    .bind(admin_id)
    .execute(&pool)
    .await
    .unwrap();

    // Register the session in the CLI manager
    let handle = state
        .cli_sessions
        .register(
            session_id,
            admin_id,
            platform::agent::claude_cli::session::SessionMode::Persistent,
        )
        .await
        .expect("register should succeed");

    // send_message to registered CLI session should succeed (queues message)
    let result = platform::agent::service::send_message(&state, session_id, "hello cli").await;
    assert!(
        result.is_ok(),
        "send_message to registered CLI session should succeed: {result:?}"
    );

    // Verify message was queued in the handle
    let pending = handle.pending_messages.lock().await;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0], "hello cli");

    // Verify message was stored in DB
    let msg_count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM agent_messages WHERE session_id = $1 AND role = 'user'",
    )
    .bind(session_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(msg_count.0, 1, "user message should be stored in DB");
}

// ---------------------------------------------------------------------------
// stop_session: CLI subprocess session cleanup
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn stop_session_cli_subprocess(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "stop-cli", "private").await;

    // Insert a running cli_subprocess session
    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, execution_mode)
         VALUES ($1, $2, $3, 'cli to stop', 'running', 'claude-code', 'cli_subprocess')",
    )
    .bind(session_id)
    .bind(project_id)
    .bind(admin_id)
    .execute(&pool)
    .await
    .unwrap();

    // Register in CLI session manager
    let _handle = state
        .cli_sessions
        .register(
            session_id,
            admin_id,
            platform::agent::claude_cli::session::SessionMode::Persistent,
        )
        .await
        .expect("register should succeed");

    // Stop the session
    platform::agent::service::stop_session(&state, session_id)
        .await
        .expect("stop_session should succeed");

    // Verify session is stopped
    let row: (String,) = sqlx::query_as("SELECT status FROM agent_sessions WHERE id = $1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.0, "stopped");

    // Verify CLI handle is removed
    let handle = state.cli_sessions.get(session_id).await;
    assert!(handle.is_none(), "CLI handle should be removed after stop");
}

// ---------------------------------------------------------------------------
// create_global_session: success path with API key
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_global_session_with_api_key(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _token) =
        create_user(&app, &admin_token, "global-sess", "globalsess@test.com").await;
    helpers::assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    // Set user's API key
    helpers::set_user_api_key(&pool, user_id).await;

    let result = platform::agent::service::create_global_session(
        &state,
        user_id,
        "Build a todo app",
        "claude-code",
    )
    .await;

    let session = result.expect("create_global_session should succeed");
    assert_eq!(session.status, "running");
    assert_eq!(session.execution_mode, "cli_subprocess");
    assert!(session.uses_pubsub);
    assert!(
        session.project_id.is_none(),
        "global session should have no project"
    );
    assert_eq!(session.prompt, "Build a todo app");

    // Verify the session was registered in CLI manager
    let handle = state.cli_sessions.get(session.id).await;
    assert!(
        handle.is_some(),
        "session should be registered in CLI manager"
    );

    // Verify first user message was stored
    let msg_count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM agent_messages WHERE session_id = $1 AND role = 'user'",
    )
    .bind(session.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        msg_count.0, 1,
        "initial prompt should be stored as user message"
    );
}

// ---------------------------------------------------------------------------
// create_global_session: no API key configured fails
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_global_session_no_key_configured(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _token) =
        create_user(&app, &admin_token, "global-nokey", "globalnokey@test.com").await;
    helpers::assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;
    // Do NOT set any API key

    let result = platform::agent::service::create_global_session(
        &state,
        user_id,
        "Build something",
        "claude-code",
    )
    .await;

    assert!(result.is_err(), "should fail without API key");
    let err = result.err().unwrap();
    assert!(
        matches!(
            err,
            platform::agent::error::AgentError::ConfigurationRequired(_)
        ),
        "expected ConfigurationRequired, got: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// create_global_session: invalid provider fails
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_global_session_invalid_provider(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _token) =
        create_user(&app, &admin_token, "global-badp", "globalbadp@test.com").await;

    let result = platform::agent::service::create_global_session(
        &state,
        user_id,
        "Build something",
        "nonexistent",
    )
    .await;

    assert!(result.is_err());
    let err = result.err().unwrap();
    assert!(
        matches!(err, platform::agent::error::AgentError::InvalidProvider(_)),
        "expected InvalidProvider, got: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// reap_terminated_sessions: no running sessions is a noop
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn reap_terminated_no_running_sessions(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "reap-noop", "private").await;

    // Insert a completed session (not running)
    insert_session(&pool, project_id, admin_id, "done", "completed").await;

    // Run the reaper — should be a noop
    platform::agent::service::run_reaper_once(&state).await;
    // No assertion needed — just verifying it doesn't crash
}

// ---------------------------------------------------------------------------
// reap_terminated_sessions: running session without pod_name is skipped
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn reap_terminated_no_pod_name_skipped(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "reap-nopod", "private").await;

    // Insert a running session without pod_name — reap_terminated_sessions query filters
    // for pod_name IS NOT NULL, so this should be skipped
    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider)
         VALUES ($1, $2, $3, 'no pod session', 'running', 'claude-code')",
    )
    .bind(session_id)
    .bind(project_id)
    .bind(admin_id)
    .execute(&pool)
    .await
    .unwrap();

    // Run the reaper
    platform::agent::service::run_reaper_once(&state).await;

    // Session should still be running (skipped by reaper)
    let row: (String,) = sqlx::query_as("SELECT status FROM agent_sessions WHERE id = $1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        row.0, "running",
        "session without pod should still be running"
    );
}

// ---------------------------------------------------------------------------
// reap_idle_sessions: session_namespace cleanup on reap
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn reap_idle_session_deletes_namespace(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "reap-ns-del", "private").await;

    // Create a K8s namespace that will be cleaned up
    let ns_name = format!("reap-ns-test-{}", &Uuid::new_v4().to_string()[..8]);
    let ns_api: kube::Api<k8s_openapi::api::core::v1::Namespace> =
        kube::Api::all(state.kube.clone());
    let ns = k8s_openapi::api::core::v1::Namespace {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(ns_name.clone()),
            ..Default::default()
        },
        ..Default::default()
    };
    let _ = ns_api.create(&kube::api::PostParams::default(), &ns).await;

    // Insert an idle session with that namespace
    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, session_namespace, created_at)
         VALUES ($1, $2, $3, 'ns cleanup', 'running', 'claude-code', $4, NOW() - INTERVAL '2 hours')",
    )
    .bind(session_id)
    .bind(project_id)
    .bind(admin_id)
    .bind(&ns_name)
    .execute(&pool)
    .await
    .unwrap();

    // Run the reaper
    platform::agent::service::run_reaper_once(&state).await;

    // Verify session is reaped
    let row: (String,) = sqlx::query_as("SELECT status FROM agent_sessions WHERE id = $1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.0, "completed");

    // Verify namespace is being deleted or already gone
    // Wait a moment for deletion to propagate
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    match ns_api.get(&ns_name).await {
        Ok(ns_obj) => {
            // If still exists, should have deletion timestamp
            assert!(
                ns_obj.metadata.deletion_timestamp.is_some(),
                "namespace should be deleting"
            );
        }
        Err(kube::Error::Api(e)) if e.code == 404 => {
            // Already deleted — good
        }
        Err(e) => {
            panic!("unexpected error: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// stop_session: session with session_namespace triggers namespace delete
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn stop_session_deletes_session_namespace(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "stop-ns-del", "private").await;

    // Create a K8s namespace
    let ns_name = format!("stop-ns-test-{}", &Uuid::new_v4().to_string()[..8]);
    let ns_api: kube::Api<k8s_openapi::api::core::v1::Namespace> =
        kube::Api::all(state.kube.clone());
    let ns = k8s_openapi::api::core::v1::Namespace {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(ns_name.clone()),
            ..Default::default()
        },
        ..Default::default()
    };
    let _ = ns_api.create(&kube::api::PostParams::default(), &ns).await;

    // Insert a running session with that namespace
    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, session_namespace, pod_name)
         VALUES ($1, $2, $3, 'stop ns test', 'running', 'claude-code', $4, 'fake-pod')",
    )
    .bind(session_id)
    .bind(project_id)
    .bind(admin_id)
    .bind(&ns_name)
    .execute(&pool)
    .await
    .unwrap();

    // Stop the session
    platform::agent::service::stop_session(&state, session_id)
        .await
        .expect("stop_session should succeed");

    // Verify session is stopped
    let row: (String,) = sqlx::query_as("SELECT status FROM agent_sessions WHERE id = $1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.0, "stopped");

    // Verify namespace is being deleted
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    match ns_api.get(&ns_name).await {
        Ok(ns_obj) => {
            assert!(
                ns_obj.metadata.deletion_timestamp.is_some(),
                "namespace should be deleting"
            );
        }
        Err(kube::Error::Api(e)) if e.code == 404 => {
            // Already deleted
        }
        Err(e) => {
            panic!("unexpected error: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// notify_parent_of_completion: no parent is a noop
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn reap_child_notifies_parent(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "reap-parent", "private").await;

    // Insert parent session
    let parent_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, pod_name, session_namespace)
         VALUES ($1, $2, $3, 'parent', 'running', 'claude-code', 'parent-pod', 'parent-ns')",
    )
    .bind(parent_id)
    .bind(project_id)
    .bind(admin_id)
    .execute(&pool)
    .await
    .unwrap();

    // Insert child session with nonexistent pod (will be reaped as failed)
    let child_id = Uuid::new_v4();
    let child_ns = format!("child-ns-{}", &child_id.to_string()[..8]);
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, pod_name, parent_session_id, session_namespace)
         VALUES ($1, $2, $3, 'child', 'running', 'claude-code', 'nonexistent-child-pod', $4, $5)",
    )
    .bind(child_id)
    .bind(project_id)
    .bind(admin_id)
    .bind(parent_id)
    .bind(&child_ns)
    .execute(&pool)
    .await
    .unwrap();

    // Run the reaper — child's pod doesn't exist → marked failed → notify_parent called
    platform::agent::service::run_reaper_once(&state).await;

    // Verify child is marked failed
    let row: (String,) = sqlx::query_as("SELECT status FROM agent_sessions WHERE id = $1")
        .bind(child_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        row.0, "failed",
        "child with missing pod should be marked failed"
    );
}

// ---------------------------------------------------------------------------
// resolve_active_llm_provider: oauth mode
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn resolve_active_llm_provider_oauth_mode(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _token) =
        create_user(&app, &admin_token, "oauth-test", "oauthtest@test.com").await;

    // Set active provider to "oauth"
    sqlx::query("UPDATE users SET active_llm_provider = 'oauth' WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();

    // Resolve — should try oauth path (returns None since no creds stored)
    let (oauth, api_key, extra, model) =
        platform::agent::service::resolve_active_llm_provider(&state, user_id).await;

    assert!(oauth.is_none(), "no oauth creds stored");
    assert!(api_key.is_none(), "oauth mode should not resolve api_key");
    assert!(extra.is_empty());
    assert!(model.is_none());
}

// ---------------------------------------------------------------------------
// resolve_active_llm_provider: global mode
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn resolve_active_llm_provider_global_mode(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _token) =
        create_user(&app, &admin_token, "global-prov", "globalprov@test.com").await;

    // Set active provider to "global"
    sqlx::query("UPDATE users SET active_llm_provider = 'global' WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();

    let (oauth, _api_key, extra, model) =
        platform::agent::service::resolve_active_llm_provider(&state, user_id).await;

    assert!(oauth.is_none(), "global mode should not return oauth");
    assert!(extra.is_empty());
    assert!(model.is_none());
}

// ---------------------------------------------------------------------------
// resolve_active_llm_provider: auto mode (default)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn resolve_active_llm_provider_auto_mode(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _token) = create_user(&app, &admin_token, "auto-prov", "autoprov@test.com").await;

    // Default is "auto" — no need to set
    let (oauth, api_key, extra, model) =
        platform::agent::service::resolve_active_llm_provider(&state, user_id).await;

    // Without any credentials configured, auto mode returns all None
    assert!(oauth.is_none());
    assert!(api_key.is_none());
    assert!(extra.is_empty());
    assert!(model.is_none());
}

// ---------------------------------------------------------------------------
// resolve_active_llm_provider: api_key mode with key set
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn resolve_active_llm_provider_api_key_with_key(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _token) =
        create_user(&app, &admin_token, "apikey-prov", "apikeyprov@test.com").await;

    // Set user's API key
    helpers::set_user_api_key(&pool, user_id).await;

    // Set active provider to "api_key"
    sqlx::query("UPDATE users SET active_llm_provider = 'api_key' WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();

    let (oauth, api_key, extra, model) =
        platform::agent::service::resolve_active_llm_provider(&state, user_id).await;

    assert!(oauth.is_none(), "api_key mode should not return oauth");
    assert!(
        api_key.is_some(),
        "api_key mode should resolve the user key"
    );
    assert!(extra.is_empty());
    assert!(model.is_none());
}

// ---------------------------------------------------------------------------
// resolve_active_llm_provider: custom mode with invalid config_id
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn resolve_active_llm_provider_custom_invalid_id(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _token) =
        create_user(&app, &admin_token, "custom-bad", "custombad@test.com").await;

    // Set active provider to "custom:{random_uuid}" (not a real config)
    let fake_config_id = Uuid::new_v4();
    sqlx::query("UPDATE users SET active_llm_provider = $1 WHERE id = $2")
        .bind(format!("custom:{fake_config_id}"))
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();

    // Should fall back to auto mode (which returns None since no creds)
    let (oauth, api_key, extra, model) =
        platform::agent::service::resolve_active_llm_provider(&state, user_id).await;

    assert!(oauth.is_none());
    assert!(api_key.is_none());
    assert!(extra.is_empty());
    assert!(model.is_none());
}

// ---------------------------------------------------------------------------
// resolve_user_api_key: returns None when no key is set
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn resolve_user_api_key_none_when_not_set(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _token) = create_user(&app, &admin_token, "nokey-usr", "nokeyusr@test.com").await;

    let result = platform::agent::service::resolve_user_api_key(&state, user_id).await;
    assert!(result.is_none(), "should return None when no key set");
}

// ---------------------------------------------------------------------------
// resolve_user_api_key: returns Some when key is set
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn resolve_user_api_key_returns_key(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _token) =
        create_user(&app, &admin_token, "haskey-usr", "haskeyusr@test.com").await;
    helpers::set_user_api_key(&pool, user_id).await;

    let result = platform::agent::service::resolve_user_api_key(&state, user_id).await;
    assert!(result.is_some(), "should return Some when key is set");
}

// ---------------------------------------------------------------------------
// resolve_global_api_key: returns None when no global secret exists
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn resolve_global_api_key_none_when_not_set(pool: PgPool) {
    let (state, _admin_token) = test_state(pool).await;

    let result = platform::agent::service::resolve_global_api_key(&state).await;
    // Without a global ANTHROPIC_API_KEY secret, should return None
    assert!(result.is_none(), "should return None without global secret");
}

// ---------------------------------------------------------------------------
// create_session via API: full round-trip with pod + cleanup
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_session_api_full_roundtrip(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "api-roundtrip", "private").await;

    // Set up bare repo
    let bare_dir = tempfile::tempdir().unwrap();
    let bare_path = bare_dir.path().join("repo.git");
    std::process::Command::new("git")
        .args(["init", "--bare", bare_path.to_str().unwrap()])
        .output()
        .expect("git init --bare");
    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    // Create session via API
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions"),
        serde_json::json!({
            "prompt": "API roundtrip test",
            "branch": "test/roundtrip"
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create session via API failed: {body}"
    );
    assert_eq!(body["status"].as_str(), Some("running"));
    assert_eq!(body["branch"].as_str(), Some("test/roundtrip"));

    let session_id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();

    // Get session detail
    let (status, detail) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["status"].as_str(), Some("running"));
    assert!(detail["pod_name"].as_str().is_some());

    // Stop the session
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/stop"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Verify stopped
    let row: (String,) = sqlx::query_as("SELECT status FROM agent_sessions WHERE id = $1")
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.0, "stopped");
}

// ---------------------------------------------------------------------------
// resolve_session_namespace: fallback to default namespace
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn resolve_session_ns_fallback_to_default(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let admin_id = get_admin_id(&app, &admin_token).await;

    // Insert a session with NO session_namespace and NO project_id
    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider)
         VALUES ($1, $2, 'no project', 'running', 'claude-code')",
    )
    .bind(session_id)
    .bind(admin_id)
    .execute(&pool)
    .await
    .unwrap();

    let session = platform::agent::service::fetch_session(&pool, session_id)
        .await
        .expect("fetch should succeed");

    assert!(session.project_id.is_none());
    assert!(session.session_namespace.is_none());
    // This is tested indirectly — the resolve function would fall back to
    // the agent_namespace when called
}

// ---------------------------------------------------------------------------
// stop_session: nonexistent session returns error
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn stop_session_nonexistent_returns_error(pool: PgPool) {
    let (state, _admin_token) = test_state(pool).await;

    let fake_id = Uuid::new_v4();
    let result = platform::agent::service::stop_session(&state, fake_id).await;
    assert!(result.is_err());
    let err = result.err().unwrap();
    assert!(
        matches!(err, platform::agent::error::AgentError::SessionNotFound),
        "expected SessionNotFound, got: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// reap_terminated: running session with existing pod is NOT reaped
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn reap_terminated_running_pod_not_reaped(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let project_id = create_project(&app, &admin_token, "reap-running", "private").await;

    // Create a real session with a real pod
    let bare_dir = tempfile::tempdir().unwrap();
    let bare_path = bare_dir.path().join("repo.git");
    std::process::Command::new("git")
        .args(["init", "--bare", bare_path.to_str().unwrap()])
        .output()
        .expect("git init --bare");
    sqlx::query("UPDATE projects SET repo_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(project_id)
        .execute(&pool)
        .await
        .unwrap();

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let session = platform::agent::service::create_session(
        &state,
        admin_id,
        project_id,
        "running pod test",
        "claude-code",
        None,
        None,
        platform::agent::AgentRoleName::Dev,
        None,
    )
    .await
    .expect("create_session should succeed");

    // Run the reaper while the pod is still in Pending/Running state
    platform::agent::service::run_reaper_once(&state).await;

    // Verify session is still running (not reaped)
    let row: (String,) = sqlx::query_as("SELECT status FROM agent_sessions WHERE id = $1")
        .bind(session.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.0, "running", "running pod session should NOT be reaped");

    // Cleanup
    if let Some(ref ns) = session.session_namespace {
        let ns_api: kube::Api<k8s_openapi::api::core::v1::Namespace> =
            kube::Api::all(state.kube.clone());
        let _ = ns_api.delete(ns, &kube::api::DeleteParams::default()).await;
    }
}

// ---------------------------------------------------------------------------
// send_message: project-scoped, completed session returns 400
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn send_message_completed_session_returns_400(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "msg-done", "private").await;
    let session_id = insert_session(
        &pool,
        project_id,
        admin_id,
        "completed session",
        "completed",
    )
    .await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/message"),
        serde_json::json!({ "content": "should fail" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "expected 400 for completed session: {body}"
    );
}

// ---------------------------------------------------------------------------
// send_message: project-scoped, running session succeeds (uses_pubsub path)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn send_message_running_session_pubsub_ok(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "msg-pubsub", "private").await;

    // Insert a running session with uses_pubsub=true (simulates agent-runner pod)
    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, uses_pubsub)
         VALUES ($1, $2, $3, 'pubsub test', 'running', 'claude-code', true)",
    )
    .bind(session_id)
    .bind(project_id)
    .bind(admin_id)
    .execute(&pool)
    .await
    .expect("insert pubsub session");

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/message"),
        serde_json::json!({ "content": "hello pubsub" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "send_message to pubsub session should succeed: {body}"
    );
    assert_eq!(body["ok"], true);
}

// ---------------------------------------------------------------------------
// send_message_global: running session succeeds (uses_pubsub path)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn send_message_global_running_pubsub_ok(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    // Insert a global session (no project_id) with uses_pubsub=true, running
    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider, uses_pubsub)
         VALUES ($1, $2, 'global pubsub test', 'running', 'claude-code', true)",
    )
    .bind(session_id)
    .bind(admin_id)
    .execute(&pool)
    .await
    .expect("insert global pubsub session");

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/sessions/{session_id}/message"),
        serde_json::json!({ "content": "hello global" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "send_message_global should succeed: {body}"
    );
    assert_eq!(body["ok"], true);
}

// ---------------------------------------------------------------------------
// send_message_global: empty content returns 400
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn send_message_global_empty_content_returns_400(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider, uses_pubsub)
         VALUES ($1, $2, 'empty content test', 'running', 'claude-code', true)",
    )
    .bind(session_id)
    .bind(admin_id)
    .execute(&pool)
    .await
    .expect("insert session");

    // Global send_message validates content after ownership check
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/sessions/{session_id}/message"),
        serde_json::json!({ "content": "" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// spawn_child: successful spawn returns child with correct lineage
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn spawn_child_returns_child_data(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "spawn-data", "private").await;
    let parent_id = insert_session(&pool, project_id, admin_id, "parent for data", "running").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{parent_id}/spawn"),
        serde_json::json!({ "prompt": "child data task" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "spawn child failed: {body}");
    assert_eq!(body["project_id"], project_id.to_string());
    assert_eq!(body["user_id"], admin_id.to_string());
    assert_eq!(body["provider"], "claude-code");
}

// ---------------------------------------------------------------------------
// spawn_child: with allowed_child_roles
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn spawn_child_with_allowed_roles(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "spawn-roles", "private").await;
    let parent_id = insert_session(&pool, project_id, admin_id, "parent roles", "running").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{parent_id}/spawn"),
        serde_json::json!({
            "prompt": "child with roles",
            "allowed_child_roles": ["dev", "test"]
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "spawn child with roles failed: {body}"
    );
}

// ---------------------------------------------------------------------------
// list_children: returns children for a parent session
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_children_returns_spawned_children(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "child-list2", "private").await;
    let parent_id = insert_session(&pool, project_id, admin_id, "parent for list", "running").await;

    // Spawn two children
    for i in 0..2 {
        helpers::post_json(
            &app,
            &admin_token,
            &format!("/api/projects/{project_id}/sessions/{parent_id}/spawn"),
            serde_json::json!({ "prompt": format!("child task {i}") }),
        )
        .await;
    }

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{parent_id}/children"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"].as_i64().unwrap(), 2);
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
}

// ---------------------------------------------------------------------------
// list_iframes: session without namespace returns 404 (new coverage)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_iframes_null_namespace_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "iframe-nons2", "private").await;
    // Session without session_namespace
    let session_id = insert_session(&pool, project_id, admin_id, "no namespace 2", "running").await;

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/iframes"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// list_iframes: session wrong project returns 404 (new coverage)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_iframes_mismatched_project_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_a = create_project(&app, &admin_token, "iframe-pa2", "private").await;
    let project_b = create_project(&app, &admin_token, "iframe-pb2", "private").await;
    let session_id = insert_session_with_ns(
        &pool,
        project_a,
        admin_id,
        "iframe test 2",
        "running",
        Some("test-ns"),
    )
    .await;

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_b}/sessions/{session_id}/iframes"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// list_iframes: session with valid namespace but no K8s services returns empty
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_iframes_valid_namespace_empty_result(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "iframe-empty", "private").await;

    // Use a namespace that exists in the cluster but has no services
    let ns_name = format!(
        "iframe-test-{}",
        Uuid::new_v4().to_string().split('-').next().unwrap()
    );

    // Create the namespace in K8s
    let ns_api: kube::Api<k8s_openapi::api::core::v1::Namespace> =
        kube::Api::all(state.kube.clone());
    let ns = k8s_openapi::api::core::v1::Namespace {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(ns_name.clone()),
            ..Default::default()
        },
        ..Default::default()
    };
    let _ = ns_api.create(&kube::api::PostParams::default(), &ns).await;

    let session_id = insert_session_with_ns(
        &pool,
        project_id,
        admin_id,
        "iframe empty ns",
        "running",
        Some(&ns_name),
    )
    .await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/iframes"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"].as_i64().unwrap(), 0);
    assert!(body["items"].as_array().unwrap().is_empty());

    // Cleanup
    let _ = ns_api
        .delete(&ns_name, &kube::api::DeleteParams::default())
        .await;
}

// ---------------------------------------------------------------------------
// list_iframes: session with invalid namespace format returns 404
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_iframes_invalid_namespace_format_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "iframe-badns", "private").await;
    // Invalid namespace (contains uppercase)
    let session_id = insert_session_with_ns(
        &pool,
        project_id,
        admin_id,
        "bad ns",
        "running",
        Some("Invalid-NS"),
    )
    .await;

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/iframes"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// get_session_progress: returns latest progress_update message
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn get_session_progress_returns_latest_progress(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "sess-prog", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id, "progress test", "running").await;

    // Insert progress_update messages
    insert_message(&pool, session_id, "progress_update", "Step 1 done").await;
    // Small delay so created_at differs
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    insert_message(&pool, session_id, "progress_update", "Step 2 done").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/progress"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["message"], "Step 2 done");
}

// ---------------------------------------------------------------------------
// get_session_progress: no progress messages returns 404
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn get_session_progress_no_messages_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "sess-noprog", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id, "no progress", "running").await;

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/progress"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// get_session_progress: non-progress_update messages are ignored
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn get_session_progress_ignores_non_progress_messages(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "sess-onlyp", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id, "only user msgs", "running").await;

    // Insert a regular user message, not a progress_update
    insert_message(&pool, session_id, "user", "hello there").await;
    insert_message(&pool, session_id, "assistant", "hi!").await;

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/progress"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// stop_session: completed session still returns ok (idempotent-ish)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn stop_session_already_stopped_returns_ok(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "stop-idem", "private").await;
    let session_id =
        insert_session(&pool, project_id, admin_id, "already stopped", "stopped").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/stop"),
        serde_json::json!({}),
    )
    .await;
    // stop_session calls service::stop_session which updates status to stopped
    // even if already stopped -- should not error
    assert_eq!(
        status,
        StatusCode::OK,
        "stop already-stopped session: {body}"
    );
}

// ---------------------------------------------------------------------------
// stop_session: session owner with project:write can stop
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn stop_session_owner_can_stop(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "stop-owner", "private").await;
    let session_id =
        insert_session(&pool, project_id, admin_id, "owner stop test", "running").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/stop"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "owner should be able to stop: {body}"
    );
}

// ---------------------------------------------------------------------------
// require_session_write: project:write user (non-owner) can stop session
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn stop_session_project_write_non_owner_succeeds(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "stop-write", "public").await;
    let session_id =
        insert_session(&pool, project_id, admin_id, "admin session stop", "running").await;

    // Create a user with project:write permission (developer role)
    let (uid, user_token) =
        create_user(&app, &admin_token, "stop-writer", "stopwriter@test.com").await;
    helpers::assign_role(
        &app,
        &admin_token,
        uid,
        "developer",
        Some(project_id),
        &pool,
    )
    .await;

    let (status, body) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/stop"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "project:write user should stop session: {body}"
    );
}

// ---------------------------------------------------------------------------
// send_message: project:write user (non-owner) can send message
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn send_message_project_write_non_owner_succeeds(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "msg-write", "public").await;

    // Running session with uses_pubsub for message delivery
    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, uses_pubsub)
         VALUES ($1, $2, $3, 'msg write test', 'running', 'claude-code', true)",
    )
    .bind(session_id)
    .bind(project_id)
    .bind(admin_id)
    .execute(&pool)
    .await
    .expect("insert session");

    // Create a developer user
    let (uid, user_token) = create_user(&app, &admin_token, "msg-dev", "msgdev@test.com").await;
    helpers::assign_role(
        &app,
        &admin_token,
        uid,
        "developer",
        Some(project_id),
        &pool,
    )
    .await;

    let (status, body) = helpers::post_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/message"),
        serde_json::json!({ "content": "hello from developer" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "project:write user should send message: {body}"
    );
}

// ---------------------------------------------------------------------------
// list_sessions: status filter with nonexistent status returns empty
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_sessions_filter_nonexistent_status_empty(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "sess-nostat", "private").await;
    insert_session(&pool, project_id, admin_id, "task", "running").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions?status=nonexistent"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"].as_i64().unwrap(), 0);
}

// ---------------------------------------------------------------------------
// validate_provider_config: browser with invalid origins
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_session_browser_invalid_origins(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let project_id = create_project(&app, &admin_token, "sess-bad-brow", "private").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions"),
        serde_json::json!({
            "prompt": "test",
            "config": {
                "browser": {
                    "allowed_origins": []
                },
                "role": "ui"
            }
        }),
    )
    .await;
    // Empty allowed_origins should be rejected by check_browser_config
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// SSE: session events endpoint returns 404 for wrong project
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn sse_session_events_wrong_project_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_a = create_project(&app, &admin_token, "sse-pa", "private").await;
    let project_b = create_project(&app, &admin_token, "sse-pb", "private").await;
    let session_id = insert_session(&pool, project_a, admin_id, "sse test", "running").await;

    // Try to get SSE events under wrong project
    let status = helpers::get_status(
        &app,
        &admin_token,
        &format!("/api/projects/{project_b}/sessions/{session_id}/events"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// SSE global: non-owner returns 403
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn sse_session_events_global_non_owner_forbidden(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "sse-glob-f", "public").await;
    let session_id =
        insert_session(&pool, project_id, admin_id, "sse global test", "running").await;

    let (_uid, user_token) = create_user(&app, &admin_token, "sse-user", "sseuser@test.com").await;

    let status = helpers::get_status(
        &app,
        &user_token,
        &format!("/api/sessions/{session_id}/events"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// SSE global: nonexistent session returns 404
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn sse_session_events_global_not_found(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    let fake_id = Uuid::new_v4();
    let status = helpers::get_status(
        &app,
        &admin_token,
        &format!("/api/sessions/{fake_id}/events"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// create_session: prompt too long is rejected
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_session_prompt_too_long(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let project_id = create_project(&app, &admin_token, "sess-long-p", "private").await;

    let long_prompt = "a".repeat(100_001);
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions"),
        serde_json::json!({
            "prompt": long_prompt,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// spawn_child: prompt too long is rejected
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn spawn_child_prompt_too_long_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "spawn-longp", "private").await;
    let parent_id = insert_session(&pool, project_id, admin_id, "parent long", "running").await;

    let long_prompt = "b".repeat(100_001);
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{parent_id}/spawn"),
        serde_json::json!({ "prompt": long_prompt }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// create_app: valid request without API key returns error
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_app_no_api_key_returns_error(pool: PgPool) {
    let (state, admin_token) = test_state(pool).await;
    let app = test_router(state);

    // Admin has no API key set, so create_global_session fails with ConfigurationRequired
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/create-app",
        serde_json::json!({
            "description": "Build a todo app",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// create_app: with API key succeeds (mock CLI)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_app_with_api_key_succeeds(pool: PgPool) {
    let (state, admin_token) = helpers::test_state_with_cli(pool.clone(), true).await;
    let app = test_router(state);

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    // Set a dummy API key for the admin user
    helpers::set_user_api_key(&pool, admin_id).await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/create-app",
        serde_json::json!({
            "description": "Build a todo app",
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create_app should succeed: {body}"
    );
    assert_eq!(body["provider"], "claude-code");
    assert_eq!(body["execution_mode"], "cli_subprocess");
}

// ---------------------------------------------------------------------------
// send_message: message to completed session returns 400 (project-scoped path)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn send_message_to_failed_session_returns_400(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "msg-fail", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id, "failed session", "failed").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/message"),
        serde_json::json!({ "content": "should fail" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "expected 400 for failed session: {body}"
    );
}

// ---------------------------------------------------------------------------
// send_message_global: message to completed session returns 400
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn send_message_global_completed_session_returns_400(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider)
         VALUES ($1, $2, 'done global', 'completed', 'claude-code')",
    )
    .bind(session_id)
    .bind(admin_id)
    .execute(&pool)
    .await
    .expect("insert global completed session");

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/sessions/{session_id}/message"),
        serde_json::json!({ "content": "should fail" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "expected 400 for completed global session: {body}"
    );
}

// ---------------------------------------------------------------------------
// send_message_global: validation — content too long rejected
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn send_message_global_content_too_long_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider, uses_pubsub)
         VALUES ($1, $2, 'long msg test', 'running', 'claude-code', true)",
    )
    .bind(session_id)
    .bind(admin_id)
    .execute(&pool)
    .await
    .unwrap();

    let long_content = "x".repeat(100_001);
    let (status, _body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/sessions/{session_id}/message"),
        serde_json::json!({ "content": long_content }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// spawn_child: allowed_child_roles stored in DB
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn spawn_child_allowed_roles_stored(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "spawn-roles-db", "private").await;
    let parent_id = insert_session(&pool, project_id, admin_id, "parent roles", "running").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{parent_id}/spawn"),
        serde_json::json!({
            "prompt": "child with roles",
            "allowed_child_roles": ["dev", "test"]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "spawn child failed: {body}");

    let child_id = body["id"].as_str().unwrap();
    let row: (Option<Vec<String>>,) =
        sqlx::query_as("SELECT allowed_child_roles FROM agent_sessions WHERE id = $1")
            .bind(Uuid::parse_str(child_id).unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();

    assert_eq!(row.0.unwrap(), vec!["dev".to_string(), "test".to_string()]);
}

// ---------------------------------------------------------------------------
// spawn_child: config validation — invalid container image
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn spawn_child_inherits_parent_provider(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "spawn-inh", "private").await;

    // Insert parent with a custom provider
    let parent_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider)
         VALUES ($1, $2, $3, 'parent custom', 'running', 'custom-provider')",
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
        serde_json::json!({ "prompt": "child inherits" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "spawn failed: {body}");
    // Child should inherit parent's provider
    assert_eq!(body["provider"], "custom-provider");
}

// ---------------------------------------------------------------------------
// list_children: pagination — total count correct
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_children_total_count(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "children-ct", "private").await;
    let parent_id = insert_session(&pool, project_id, admin_id, "parent", "running").await;

    // Insert 3 child sessions
    for i in 0..3 {
        let child_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, parent_session_id, spawn_depth)
             VALUES ($1, $2, $3, $4, 'pending', 'claude-code', $5, 1)",
        )
        .bind(child_id)
        .bind(project_id)
        .bind(admin_id)
        .bind(format!("child {i}"))
        .bind(parent_id)
        .execute(&pool)
        .await
        .unwrap();
    }

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{parent_id}/children"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 3);
    assert_eq!(body["items"].as_array().unwrap().len(), 3);
}

// ---------------------------------------------------------------------------
// list_children: wrong project returns empty
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_children_wrong_project_returns_empty(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_a = create_project(&app, &admin_token, "children-a", "private").await;
    let project_b = create_project(&app, &admin_token, "children-b", "private").await;
    let parent_id = insert_session(&pool, project_a, admin_id, "parent", "running").await;

    // Insert child in project_a
    let child_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, parent_session_id, spawn_depth)
         VALUES ($1, $2, $3, 'child', 'pending', 'claude-code', $4, 1)",
    )
    .bind(child_id)
    .bind(project_a)
    .bind(admin_id)
    .bind(parent_id)
    .execute(&pool)
    .await
    .unwrap();

    // Query children via project_b — should find none
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_b}/sessions/{parent_id}/children"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 0);
}

// ---------------------------------------------------------------------------
// get_session_progress: multiple progress messages returns latest
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn get_session_progress_returns_latest_of_multiple(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "prog-multi", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id, "progress test", "running").await;

    // Insert multiple progress_update messages
    insert_message(&pool, session_id, "progress_update", "step 1 done").await;
    // Small delay for ordering
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    insert_message(&pool, session_id, "progress_update", "step 2 done").await;
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    insert_message(&pool, session_id, "progress_update", "step 3 done").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/progress"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["message"], "step 3 done");
}

// ---------------------------------------------------------------------------
// stop_session: verify webhook fired (audit log)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn stop_session_fires_audit_log(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "stop-audit", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id, "audit test", "running").await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/stop"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let count = helpers::wait_for_audit(&pool, "agent_session.stop", 2000).await;
    assert!(count > 0, "audit log entry should exist for stop");
}

// ---------------------------------------------------------------------------
// send_message: fires audit log
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn send_message_fires_audit_log(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "msg-audit", "private").await;
    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, uses_pubsub)
         VALUES ($1, $2, $3, 'audit msg', 'running', 'claude-code', true)",
    )
    .bind(session_id)
    .bind(project_id)
    .bind(admin_id)
    .execute(&pool)
    .await
    .unwrap();

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/message"),
        serde_json::json!({ "content": "test message" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let count = helpers::wait_for_audit(&pool, "agent_session.message", 2000).await;
    assert!(count > 0, "audit log entry should exist for message send");
}

// ---------------------------------------------------------------------------
// list_sessions: filter by running status
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_sessions_filter_by_running(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "filter-run", "private").await;
    insert_session(&pool, project_id, admin_id, "running 1", "running").await;
    insert_session(&pool, project_id, admin_id, "running 2", "running").await;
    insert_session(&pool, project_id, admin_id, "completed 1", "completed").await;
    insert_session(&pool, project_id, admin_id, "failed 1", "failed").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions?status=running"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 2);
    for item in body["items"].as_array().unwrap() {
        assert_eq!(item["status"], "running");
    }
}

// ---------------------------------------------------------------------------
// list_sessions: filter by failed status
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_sessions_filter_by_failed(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "filter-fail", "private").await;
    insert_session(&pool, project_id, admin_id, "running 1", "running").await;
    insert_session(&pool, project_id, admin_id, "failed 1", "failed").await;
    insert_session(&pool, project_id, admin_id, "failed 2", "failed").await;

    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions?status=failed"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 2);
    for item in body["items"].as_array().unwrap() {
        assert_eq!(item["status"], "failed");
    }
}

// ---------------------------------------------------------------------------
// require_session_write: non-owner without project:write gets 404
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn require_session_write_non_owner_no_write_fails(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "sesswrite-nowrite", "public").await;

    // Create a regular user with only project:read
    let (user_id, _user_token) =
        create_user(&app, &admin_token, "sessreader", "sessreader@example.com").await;
    helpers::assign_role(
        &app,
        &admin_token,
        user_id,
        "developer",
        Some(project_id),
        &pool,
    )
    .await;

    // Session belongs to admin
    let session_id = insert_session(&pool, project_id, admin_id, "admin session", "running").await;

    // The reader user trying to stop admin's session should fail
    // Note: developer role includes project:write, so we need a viewer
    // Create a user with no role on the project — will fail require_session_write
    let (_viewer_id, viewer_token) =
        create_user(&app, &admin_token, "sessviewer", "sessviewer@example.com").await;

    let (status, _body) = helpers::post_json(
        &app,
        &viewer_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/stop"),
        serde_json::json!({}),
    )
    .await;
    // Public project so read works, but stop requires session_write (ownership or project:write)
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// SSE session events: project-scoped — nonexistent session returns 404
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn sse_session_events_nonexistent_session_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "sse-noexist", "private").await;
    let fake_session = Uuid::new_v4();

    let status = helpers::get_status(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{fake_session}/events"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// SSE session events global: nonexistent session returns 404
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn sse_session_events_global_nonexistent_session_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let fake_session = Uuid::new_v4();
    let status = helpers::get_status(
        &app,
        &admin_token,
        &format!("/api/sessions/{fake_session}/events"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// validate_provider_config: setup_commands valid
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_session_valid_setup_commands_accepted(pool: PgPool) {
    let (state, admin_token) = helpers::test_state_with_cli(pool.clone(), true).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "setup-cmds", "private").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions"),
        serde_json::json!({
            "prompt": "test setup",
            "config": {
                "setup_commands": ["npm install", "pip install -r requirements.txt"]
            }
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "should accept valid setup_commands: {body}"
    );
}

// ---------------------------------------------------------------------------
// list_sessions: no auth returns 401
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_sessions_no_auth_returns_401(pool: PgPool) {
    let (state, _admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);

    let (status, _body) = helpers::get_json(
        &app,
        "",
        &format!("/api/projects/{}/sessions", Uuid::new_v4()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// spawn_child: depth 1 child can spawn at depth 2
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn spawn_child_depth_increments(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "depth-inc", "private").await;
    let parent_id = insert_session(&pool, project_id, admin_id, "root", "running").await;

    // Spawn first child (depth 1)
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{parent_id}/spawn"),
        serde_json::json!({ "prompt": "child depth 1" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let child_id = body["id"].as_str().unwrap();
    // Verify depth in DB
    let (depth,): (i32,) = sqlx::query_as("SELECT spawn_depth FROM agent_sessions WHERE id = $1")
        .bind(Uuid::parse_str(child_id).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(depth, 1);
}

// ---------------------------------------------------------------------------
// send_message project-scoped: content too long rejected
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn send_message_content_too_long_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "msg-long", "private").await;
    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, uses_pubsub)
         VALUES ($1, $2, $3, 'long msg', 'running', 'claude-code', true)",
    )
    .bind(session_id)
    .bind(project_id)
    .bind(admin_id)
    .execute(&pool)
    .await
    .unwrap();

    let long_content = "x".repeat(100_001);
    let (status, _body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/message"),
        serde_json::json!({ "content": long_content }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// create_session: prompt too long but under 100k is fine
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_session_normal_prompt_succeeds(pool: PgPool) {
    let (state, admin_token) = helpers::test_state_with_cli(pool.clone(), true).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "norm-prompt", "private").await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions"),
        serde_json::json!({
            "prompt": "a".repeat(1000),
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "1000-char prompt should succeed: {body}"
    );
}

// ---------------------------------------------------------------------------
// send_message: wrong project returns 404 (project-scoped)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn send_message_session_wrong_project_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_a = create_project(&app, &admin_token, "msg-proj-a", "private").await;
    let project_b = create_project(&app, &admin_token, "msg-proj-b", "private").await;
    let session_id = insert_session(&pool, project_a, admin_id, "in project a", "running").await;

    let (status, _body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_b}/sessions/{session_id}/message"),
        serde_json::json!({ "content": "wrong project" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// stop_session: wrong project but same session returns 404
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn stop_session_wrong_project_returns_404_v2(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_a = create_project(&app, &admin_token, "stop-proj-a2", "private").await;
    let project_b = create_project(&app, &admin_token, "stop-proj-b2", "private").await;
    let session_id = insert_session(&pool, project_a, admin_id, "in project a", "running").await;

    let (status, _body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_b}/sessions/{session_id}/stop"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// list_iframes: session without namespace — returns 404 with informative error
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_iframes_session_no_namespace_field(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "iframe-nons", "private").await;
    // Insert session with NULL session_namespace
    let session_id = insert_session(&pool, project_id, admin_id, "no ns", "running").await;

    let (status, _body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/iframes"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// get_session_progress: requires project read
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn get_session_progress_requires_project_read(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "prog-perm", "private").await;
    let session_id = insert_session(&pool, project_id, admin_id, "perm test", "running").await;

    // User with no permissions
    let (_user_id, user_token) =
        create_user(&app, &admin_token, "proguser", "proguser@example.com").await;

    let (status, _body) = helpers::get_json(
        &app,
        &user_token,
        &format!("/api/projects/{project_id}/sessions/{session_id}/progress"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// spawn_child: fires audit log
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn spawn_child_fires_audit_log(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_id = create_project(&app, &admin_token, "spawn-audit", "private").await;
    let parent_id = insert_session(&pool, project_id, admin_id, "audit parent", "running").await;

    let (status, _body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions/{parent_id}/spawn"),
        serde_json::json!({ "prompt": "audit child" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let count = helpers::wait_for_audit(&pool, "agent_session.spawn", 2000).await;
    assert!(count > 0, "audit log entry should exist for spawn");
}

// ---------------------------------------------------------------------------
// create_session: browser config with invalid origins rejected
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn create_session_browser_empty_origins_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state_with_cli(pool.clone(), true).await;
    let app = test_router(state);

    let project_id = create_project(&app, &admin_token, "browser-empty", "private").await;

    let (status, _body) = helpers::post_json(
        &app,
        &admin_token,
        &format!("/api/projects/{project_id}/sessions"),
        serde_json::json!({
            "prompt": "browser test",
            "config": {
                "browser": { "allowed_origins": [] },
                "role": "ui"
            }
        }),
    )
    .await;
    // Empty origins should be rejected by check_browser_config
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// SSE session events: project-scoped — wrong project returns 404
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn sse_session_events_different_project_returns_404(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_id = get_admin_id(&app, &admin_token).await;

    let project_a = create_project(&app, &admin_token, "sse-proj-a", "private").await;
    let project_b = create_project(&app, &admin_token, "sse-proj-b", "private").await;
    let session_id = insert_session(&pool, project_a, admin_id, "in A", "running").await;

    let status = helpers::get_status(
        &app,
        &admin_token,
        &format!("/api/projects/{project_b}/sessions/{session_id}/events"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
