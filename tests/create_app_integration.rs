//! Integration tests for the "Create App" flow (Phase E).
#![allow(clippy::doc_markdown)]

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

    let (user_id, token) = create_user(&app, &admin_token, "dev1", "dev1@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;
    set_user_api_key(&pool, user_id).await;

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
    let (other_id, other_token) = create_user(&app, &admin_token, "dev5", "dev5@test.com").await;
    assign_role(&app, &admin_token, other_id, "developer", None, &pool).await;

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

    // Should fail with 400 because no LLM provider is configured (user or global)
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

/// After create-app, session uses `cli_subprocess` execution mode with no pod.
#[sqlx::test(migrations = "./migrations")]
async fn create_app_session_is_cli_subprocess(pool: PgPool) {
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

    // CLI subprocess sessions should have no pod_name
    assert!(
        body["pod_name"].is_null(),
        "cli_subprocess session should have null pod_name: {body}"
    );

    // Verify execution mode is cli_subprocess
    let session_id = body["id"].as_str().unwrap();
    let row: (String,) =
        sqlx::query_as("SELECT execution_mode FROM agent_sessions WHERE id = $1::uuid")
            .bind(session_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row.0, "cli_subprocess");
}

// ---------------------------------------------------------------------------
// Phase 3: execute_create_project tool tests
// ---------------------------------------------------------------------------

/// `execute_create_app_tool` with `create_project` creates a project + bare repo,
/// sets namespace_slug, and links the session to the project.
#[sqlx::test(migrations = "./migrations")]
async fn execute_create_project_creates_project_and_repo(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _) = create_user(&app, &admin_token, "ca-create", "cacreate@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    // Insert a session row to link
    let session_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider, execution_mode)
         VALUES ($1, $2, 'test', 'running', 'claude-code', 'cli_subprocess')",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "create_project".into(),
        parameters: serde_json::json!({
            "name": "my-test-app",
            "description": "A test application"
        }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, session_id, user_id, &tool)
            .await;

    assert!(result.is_ok(), "create_project tool failed: {result:?}");
    let val = result.unwrap();
    assert_eq!(val["name"].as_str(), Some("my-test-app"));
    assert!(val["project_id"].as_str().is_some());
    assert!(val["namespace_slug"].as_str().is_some());

    // Verify project exists in DB
    let project_id = uuid::Uuid::parse_str(val["project_id"].as_str().unwrap()).unwrap();
    let row: (String,) = sqlx::query_as("SELECT name FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.0, "my-test-app");

    // Verify session is linked to the project
    let sess_row: (Option<uuid::Uuid>,) =
        sqlx::query_as("SELECT project_id FROM agent_sessions WHERE id = $1")
            .bind(session_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(sess_row.0, Some(project_id));
}

/// Creating a project with the same name twice should fail.
#[sqlx::test(migrations = "./migrations")]
async fn execute_create_project_duplicate_name_fails(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _) = create_user(&app, &admin_token, "ca-dup", "cadup@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    // First create succeeds
    let session_id_1 = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider, execution_mode)
         VALUES ($1, $2, 'test', 'running', 'claude-code', 'cli_subprocess')",
    )
    .bind(session_id_1)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "create_project".into(),
        parameters: serde_json::json!({ "name": "dup-app" }),
    };

    let result1 =
        platform::agent::create_app::execute_create_app_tool(&state, session_id_1, user_id, &tool)
            .await;
    assert!(result1.is_ok(), "first create should succeed: {result1:?}");

    // Second create with same name should fail
    let session_id_2 = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider, execution_mode)
         VALUES ($1, $2, 'test', 'running', 'claude-code', 'cli_subprocess')",
    )
    .bind(session_id_2)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let result2 =
        platform::agent::create_app::execute_create_app_tool(&state, session_id_2, user_id, &tool)
            .await;
    assert!(
        result2.is_err(),
        "second create with same name should fail: {result2:?}"
    );
    let err_msg = result2.unwrap_err();
    assert!(
        err_msg.contains("already exists"),
        "error should mention 'already exists': {err_msg}"
    );
}

/// Invalid project name (spaces) rejected by create_project tool.
#[sqlx::test(migrations = "./migrations")]
async fn execute_create_project_invalid_name_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _) = create_user(&app, &admin_token, "ca-badname", "cabadname@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let session_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider, execution_mode)
         VALUES ($1, $2, 'test', 'running', 'claude-code', 'cli_subprocess')",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "create_project".into(),
        parameters: serde_json::json!({ "name": "my bad name!" }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, session_id, user_id, &tool)
            .await;
    assert!(
        result.is_err(),
        "invalid name should be rejected: {result:?}"
    );
}

// ---------------------------------------------------------------------------
// Phase 3: spawn_coding_agent tool tests
// ---------------------------------------------------------------------------

/// Missing prompt field in spawn_coding_agent should fail.
#[sqlx::test(migrations = "./migrations")]
async fn execute_spawn_agent_missing_prompt_fails(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _) = create_user(&app, &admin_token, "ca-noprompt", "canoprompt@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let session_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider, execution_mode)
         VALUES ($1, $2, 'test', 'running', 'claude-code', 'cli_subprocess')",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "spawn_coding_agent".into(),
        parameters: serde_json::json!({
            "project_id": uuid::Uuid::new_v4().to_string()
            // missing "prompt"
        }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, session_id, user_id, &tool)
            .await;
    assert!(
        result.is_err(),
        "missing prompt should be rejected: {result:?}"
    );
    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("prompt"),
        "error should mention 'prompt': {err_msg}"
    );
}

// ---------------------------------------------------------------------------
// Phase 3: send_message_to_session tool tests
// ---------------------------------------------------------------------------

/// Send message from parent to a running child session should publish to Valkey.
#[sqlx::test(migrations = "./migrations")]
async fn execute_send_message_publishes_to_child(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, user_token) =
        create_user(&app, &admin_token, "ca-sendmsg", "casendmsg@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let project_id = helpers::create_project(&app, &user_token, "send-msg-proj", "private").await;

    // Create parent session
    let parent_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider)
         VALUES ($1, $2, $3, 'parent', 'running', 'claude-code')",
    )
    .bind(parent_id)
    .bind(project_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    // Create child session linked to parent
    let child_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, parent_session_id, spawn_depth)
         VALUES ($1, $2, $3, 'child', 'running', 'claude-code', $4, 1)",
    )
    .bind(child_id)
    .bind(project_id)
    .bind(user_id)
    .bind(parent_id)
    .execute(&pool)
    .await
    .unwrap();

    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "send_message_to_session".into(),
        parameters: serde_json::json!({
            "session_id": child_id.to_string(),
            "message": "hello from parent"
        }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, parent_id, user_id, &tool)
            .await;
    assert!(result.is_ok(), "send_message should succeed: {result:?}");
    let val = result.unwrap();
    assert_eq!(val["ok"].as_bool(), Some(true));
}

/// Wrong parent cannot send message to a child.
#[sqlx::test(migrations = "./migrations")]
async fn execute_send_message_wrong_parent_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, user_token) =
        create_user(&app, &admin_token, "ca-wrongpar", "cawrongpar@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let project_id = helpers::create_project(&app, &user_token, "wrong-par-proj", "private").await;

    // Create two parent sessions
    let parent_a = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider)
         VALUES ($1, $2, $3, 'parent-a', 'running', 'claude-code')",
    )
    .bind(parent_a)
    .bind(project_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let parent_b = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider)
         VALUES ($1, $2, $3, 'parent-b', 'running', 'claude-code')",
    )
    .bind(parent_b)
    .bind(project_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    // Create child linked to parent_a
    let child_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, parent_session_id, spawn_depth)
         VALUES ($1, $2, $3, 'child', 'running', 'claude-code', $4, 1)",
    )
    .bind(child_id)
    .bind(project_id)
    .bind(user_id)
    .bind(parent_a)
    .execute(&pool)
    .await
    .unwrap();

    // parent_b tries to send to child (should fail — child belongs to parent_a)
    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "send_message_to_session".into(),
        parameters: serde_json::json!({
            "session_id": child_id.to_string(),
            "message": "from wrong parent"
        }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, parent_b, user_id, &tool)
            .await;
    assert!(
        result.is_err(),
        "wrong parent should be rejected: {result:?}"
    );
    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("not a child"),
        "error should mention 'not a child': {err_msg}"
    );
}

/// Send message to a non-running child session should fail.
#[sqlx::test(migrations = "./migrations")]
async fn execute_send_message_not_running_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, user_token) =
        create_user(&app, &admin_token, "ca-notrun", "canotrun@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let project_id = helpers::create_project(&app, &user_token, "notrun-proj", "private").await;

    let parent_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider)
         VALUES ($1, $2, $3, 'parent', 'running', 'claude-code')",
    )
    .bind(parent_id)
    .bind(project_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    // Create a completed child
    let child_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, parent_session_id, spawn_depth)
         VALUES ($1, $2, $3, 'child', 'completed', 'claude-code', $4, 1)",
    )
    .bind(child_id)
    .bind(project_id)
    .bind(user_id)
    .bind(parent_id)
    .execute(&pool)
    .await
    .unwrap();

    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "send_message_to_session".into(),
        parameters: serde_json::json!({
            "session_id": child_id.to_string(),
            "message": "too late"
        }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, parent_id, user_id, &tool)
            .await;
    assert!(
        result.is_err(),
        "sending to completed child should fail: {result:?}"
    );
    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("not running"),
        "error should mention 'not running': {err_msg}"
    );
}

// ---------------------------------------------------------------------------
// Phase 3: check_session_progress tool tests
// ---------------------------------------------------------------------------

/// Check progress of a child session returns messages.
#[sqlx::test(migrations = "./migrations")]
async fn execute_check_progress_returns_messages(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, user_token) =
        create_user(&app, &admin_token, "ca-progress", "caprogress@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let project_id = helpers::create_project(&app, &user_token, "progress-proj", "private").await;

    let parent_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider)
         VALUES ($1, $2, $3, 'parent', 'running', 'claude-code')",
    )
    .bind(parent_id)
    .bind(project_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let child_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, parent_session_id, spawn_depth)
         VALUES ($1, $2, $3, 'child', 'running', 'claude-code', $4, 1)",
    )
    .bind(child_id)
    .bind(project_id)
    .bind(user_id)
    .bind(parent_id)
    .execute(&pool)
    .await
    .unwrap();

    // Insert some messages for the child
    sqlx::query(
        "INSERT INTO agent_messages (session_id, role, content) VALUES ($1, 'user', 'hello')",
    )
    .bind(child_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_messages (session_id, role, content) VALUES ($1, 'assistant', 'hi back')",
    )
    .bind(child_id)
    .execute(&pool)
    .await
    .unwrap();

    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "check_session_progress".into(),
        parameters: serde_json::json!({
            "session_id": child_id.to_string(),
        }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, parent_id, user_id, &tool)
            .await;
    assert!(result.is_ok(), "check_progress should succeed: {result:?}");
    let val = result.unwrap();
    assert_eq!(val["status"].as_str(), Some("running"));
    let messages = val["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"].as_str(), Some("user"));
    assert_eq!(messages[1]["role"].as_str(), Some("assistant"));
}

/// Wrong parent cannot check progress of someone else's child.
#[sqlx::test(migrations = "./migrations")]
async fn execute_check_progress_wrong_parent_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, user_token) =
        create_user(&app, &admin_token, "ca-chkwrong", "cachkwrong@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let project_id = helpers::create_project(&app, &user_token, "chk-wrong-proj", "private").await;

    let parent_a = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider)
         VALUES ($1, $2, $3, 'parent-a', 'running', 'claude-code')",
    )
    .bind(parent_a)
    .bind(project_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let parent_b = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider)
         VALUES ($1, $2, $3, 'parent-b', 'running', 'claude-code')",
    )
    .bind(parent_b)
    .bind(project_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let child_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, parent_session_id, spawn_depth)
         VALUES ($1, $2, $3, 'child', 'running', 'claude-code', $4, 1)",
    )
    .bind(child_id)
    .bind(project_id)
    .bind(user_id)
    .bind(parent_a)
    .execute(&pool)
    .await
    .unwrap();

    // parent_b tries to check child (belongs to parent_a)
    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "check_session_progress".into(),
        parameters: serde_json::json!({
            "session_id": child_id.to_string(),
        }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, parent_b, user_id, &tool)
            .await;
    assert!(
        result.is_err(),
        "wrong parent should be rejected: {result:?}"
    );
    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("not a child"),
        "error should mention 'not a child': {err_msg}"
    );
}

// ---------------------------------------------------------------------------
// execute_create_project: namespace collision retry
// ---------------------------------------------------------------------------

/// When two projects produce the same namespace_slug, the second gets a hash suffix.
#[sqlx::test(migrations = "./migrations")]
async fn execute_create_project_namespace_collision_retries(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _) = create_user(&app, &admin_token, "ca-nscoll", "canscoll@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    // Create first project
    let session_id_1 = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider, execution_mode)
         VALUES ($1, $2, 'test', 'running', 'claude-code', 'cli_subprocess')",
    )
    .bind(session_id_1)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let tool1 = platform::agent::cli_invoke::ToolRequest {
        name: "create_project".into(),
        parameters: serde_json::json!({ "name": "ns-test-app" }),
    };

    let result1 =
        platform::agent::create_app::execute_create_app_tool(&state, session_id_1, user_id, &tool1)
            .await;
    assert!(result1.is_ok(), "first create should succeed: {result1:?}");

    // Insert a blocker project with the slug that "ns-coll-test" would generate
    let target_slug = platform::deployer::namespace::slugify_namespace("ns-coll-test").unwrap();
    let workspace_id: uuid::Uuid =
        sqlx::query_scalar("SELECT workspace_id FROM projects ORDER BY created_at DESC LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();

    let blocking_project_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO projects (id, name, owner_id, visibility, repo_path, workspace_id, namespace_slug)
         VALUES ($1, 'ns-coll-blocker', $2, 'private', '/tmp/fake', $3, $4)",
    )
    .bind(blocking_project_id)
    .bind(user_id)
    .bind(workspace_id)
    .bind(&target_slug)
    .execute(&pool)
    .await
    .unwrap();

    // Now create "ns-coll-test" — slug collides with the blocker
    let session_id_2 = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider, execution_mode)
         VALUES ($1, $2, 'test', 'running', 'claude-code', 'cli_subprocess')",
    )
    .bind(session_id_2)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let tool2 = platform::agent::cli_invoke::ToolRequest {
        name: "create_project".into(),
        parameters: serde_json::json!({ "name": "ns-coll-test" }),
    };

    let result2 =
        platform::agent::create_app::execute_create_app_tool(&state, session_id_2, user_id, &tool2)
            .await;
    assert!(
        result2.is_ok(),
        "should succeed with hash suffix: {result2:?}"
    );
    let val2 = result2.unwrap();
    let slug2 = val2["namespace_slug"].as_str().unwrap();

    // The retried slug should differ from the target slug (has hash suffix)
    assert_ne!(
        slug2, &target_slug,
        "namespace_slug should have hash suffix after collision"
    );
    assert!(
        slug2.len() > target_slug.len(),
        "retried slug should be longer than original"
    );
}

// ---------------------------------------------------------------------------
// execute_create_project: branch protection auto-created
// ---------------------------------------------------------------------------

/// create_project should auto-create a branch protection rule for "main".
#[sqlx::test(migrations = "./migrations")]
async fn execute_create_project_branch_protection(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _) = create_user(&app, &admin_token, "ca-bp", "cabp@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let session_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider, execution_mode)
         VALUES ($1, $2, 'test', 'running', 'claude-code', 'cli_subprocess')",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "create_project".into(),
        parameters: serde_json::json!({ "name": "bp-test-app" }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, session_id, user_id, &tool)
            .await;
    assert!(result.is_ok(), "create_project should succeed: {result:?}");
    let val = result.unwrap();
    let project_id = uuid::Uuid::parse_str(val["project_id"].as_str().unwrap()).unwrap();

    // Verify branch protection rule exists for "main"
    let rule_count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM branch_protection_rules WHERE project_id = $1 AND pattern = 'main'",
    )
    .bind(project_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        rule_count.0, 1,
        "branch protection for 'main' should be auto-created"
    );
}

// ---------------------------------------------------------------------------
// execute_create_project: audit log
// ---------------------------------------------------------------------------

/// create_project should write an audit log entry.
#[sqlx::test(migrations = "./migrations")]
async fn execute_create_project_audit_logged(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _) = create_user(&app, &admin_token, "ca-audit", "caaudit@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let session_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider, execution_mode)
         VALUES ($1, $2, 'test', 'running', 'claude-code', 'cli_subprocess')",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "create_project".into(),
        parameters: serde_json::json!({ "name": "audit-test-app" }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, session_id, user_id, &tool)
            .await;
    assert!(result.is_ok(), "create_project should succeed: {result:?}");

    // Wait for audit log entry to appear (async write)
    let count = helpers::wait_for_audit(&pool, "project.create", 2000).await;
    assert!(count > 0, "audit log should contain 'project.create' entry");
}

// ---------------------------------------------------------------------------
// execute_create_project: with display_name and description
// ---------------------------------------------------------------------------

/// create_project with display_name and description stores them correctly.
#[sqlx::test(migrations = "./migrations")]
async fn execute_create_project_with_display_name(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _) = create_user(&app, &admin_token, "ca-dn", "cadn@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let session_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider, execution_mode)
         VALUES ($1, $2, 'test', 'running', 'claude-code', 'cli_subprocess')",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "create_project".into(),
        parameters: serde_json::json!({
            "name": "dn-test-app",
            "display_name": "My Display Name",
            "description": "A test description"
        }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, session_id, user_id, &tool)
            .await;
    assert!(result.is_ok(), "create_project should succeed: {result:?}");
    let val = result.unwrap();
    let project_id = uuid::Uuid::parse_str(val["project_id"].as_str().unwrap()).unwrap();

    // Verify display_name and description stored
    let row: (Option<String>, Option<String>) =
        sqlx::query_as("SELECT display_name, description FROM projects WHERE id = $1")
            .bind(project_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row.0.as_deref(), Some("My Display Name"));
    assert_eq!(row.1.as_deref(), Some("A test description"));
}

// ---------------------------------------------------------------------------
// execute_spawn_agent: missing project_id
// ---------------------------------------------------------------------------

/// Missing project_id in spawn_coding_agent should fail.
#[sqlx::test(migrations = "./migrations")]
async fn execute_spawn_agent_missing_project_id_fails(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _) = create_user(&app, &admin_token, "ca-nopid", "canopid@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let session_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider, execution_mode)
         VALUES ($1, $2, 'test', 'running', 'claude-code', 'cli_subprocess')",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "spawn_coding_agent".into(),
        parameters: serde_json::json!({
            "prompt": "Build a web app"
            // missing "project_id"
        }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, session_id, user_id, &tool)
            .await;
    assert!(
        result.is_err(),
        "missing project_id should be rejected: {result:?}"
    );
    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("project_id"),
        "error should mention 'project_id': {err_msg}"
    );
}

/// Invalid UUID for project_id in spawn_coding_agent should fail.
#[sqlx::test(migrations = "./migrations")]
async fn execute_spawn_agent_invalid_project_id_fails(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _) = create_user(&app, &admin_token, "ca-badpid", "cabadpid@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let session_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider, execution_mode)
         VALUES ($1, $2, 'test', 'running', 'claude-code', 'cli_subprocess')",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "spawn_coding_agent".into(),
        parameters: serde_json::json!({
            "project_id": "not-a-uuid",
            "prompt": "Build a web app"
        }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, session_id, user_id, &tool)
            .await;
    assert!(
        result.is_err(),
        "invalid UUID should be rejected: {result:?}"
    );
    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("UUID"),
        "error should mention 'UUID': {err_msg}"
    );
}

// ---------------------------------------------------------------------------
// execute_send_message: nonexistent child session
// ---------------------------------------------------------------------------

/// Sending a message to a nonexistent session should fail.
#[sqlx::test(migrations = "./migrations")]
async fn execute_send_message_nonexistent_child(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _) = create_user(&app, &admin_token, "ca-noexist", "canoexist@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let parent_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider)
         VALUES ($1, $2, 'parent', 'running', 'claude-code')",
    )
    .bind(parent_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let nonexistent_child = uuid::Uuid::new_v4();
    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "send_message_to_session".into(),
        parameters: serde_json::json!({
            "session_id": nonexistent_child.to_string(),
            "message": "hello"
        }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, parent_id, user_id, &tool)
            .await;
    assert!(result.is_err(), "nonexistent child should fail: {result:?}");
    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("not found"),
        "error should mention 'not found': {err_msg}"
    );
}

// ---------------------------------------------------------------------------
// execute_send_message: message length validation
// ---------------------------------------------------------------------------

/// Empty message should be rejected (min length 1).
#[sqlx::test(migrations = "./migrations")]
async fn execute_send_message_empty_message_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, user_token) =
        create_user(&app, &admin_token, "ca-emptymsg", "caemptymsg@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let project_id = helpers::create_project(&app, &user_token, "empty-msg-proj", "private").await;

    let parent_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider)
         VALUES ($1, $2, $3, 'parent', 'running', 'claude-code')",
    )
    .bind(parent_id)
    .bind(project_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let child_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, parent_session_id, spawn_depth)
         VALUES ($1, $2, $3, 'child', 'running', 'claude-code', $4, 1)",
    )
    .bind(child_id)
    .bind(project_id)
    .bind(user_id)
    .bind(parent_id)
    .execute(&pool)
    .await
    .unwrap();

    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "send_message_to_session".into(),
        parameters: serde_json::json!({
            "session_id": child_id.to_string(),
            "message": ""
        }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, parent_id, user_id, &tool)
            .await;
    assert!(
        result.is_err(),
        "empty message should be rejected: {result:?}"
    );
    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("message") || err_msg.contains("length"),
        "error should mention validation: {err_msg}"
    );
}

/// Overly long message (>100000 chars) should be rejected.
#[sqlx::test(migrations = "./migrations")]
async fn execute_send_message_too_long_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, user_token) =
        create_user(&app, &admin_token, "ca-longmsg", "calongmsg@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let project_id = helpers::create_project(&app, &user_token, "long-msg-proj", "private").await;

    let parent_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider)
         VALUES ($1, $2, $3, 'parent', 'running', 'claude-code')",
    )
    .bind(parent_id)
    .bind(project_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let child_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, parent_session_id, spawn_depth)
         VALUES ($1, $2, $3, 'child', 'running', 'claude-code', $4, 1)",
    )
    .bind(child_id)
    .bind(project_id)
    .bind(user_id)
    .bind(parent_id)
    .execute(&pool)
    .await
    .unwrap();

    let long_message = "x".repeat(100_001);
    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "send_message_to_session".into(),
        parameters: serde_json::json!({
            "session_id": child_id.to_string(),
            "message": long_message
        }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, parent_id, user_id, &tool)
            .await;
    assert!(
        result.is_err(),
        "too-long message should be rejected: {result:?}"
    );
}

/// Missing message field should be rejected.
#[sqlx::test(migrations = "./migrations")]
async fn execute_send_message_missing_message_field(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _) = create_user(&app, &admin_token, "ca-nomsgf", "canomsgf@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let parent_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider)
         VALUES ($1, $2, 'parent', 'running', 'claude-code')",
    )
    .bind(parent_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "send_message_to_session".into(),
        parameters: serde_json::json!({
            "session_id": uuid::Uuid::new_v4().to_string()
            // missing "message"
        }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, parent_id, user_id, &tool)
            .await;
    assert!(
        result.is_err(),
        "missing message field should be rejected: {result:?}"
    );
    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("message"),
        "error should mention 'message': {err_msg}"
    );
}

// ---------------------------------------------------------------------------
// execute_check_progress: custom limit and capping
// ---------------------------------------------------------------------------

/// check_session_progress with custom limit returns at most that many messages.
#[sqlx::test(migrations = "./migrations")]
async fn execute_check_progress_custom_limit(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, user_token) =
        create_user(&app, &admin_token, "ca-limit", "calimit@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let project_id = helpers::create_project(&app, &user_token, "limit-proj", "private").await;

    let parent_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider)
         VALUES ($1, $2, $3, 'parent', 'running', 'claude-code')",
    )
    .bind(parent_id)
    .bind(project_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let child_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, parent_session_id, spawn_depth)
         VALUES ($1, $2, $3, 'child', 'running', 'claude-code', $4, 1)",
    )
    .bind(child_id)
    .bind(project_id)
    .bind(user_id)
    .bind(parent_id)
    .execute(&pool)
    .await
    .unwrap();

    // Insert 5 messages
    for i in 0..5 {
        sqlx::query(
            "INSERT INTO agent_messages (session_id, role, content) VALUES ($1, 'user', $2)",
        )
        .bind(child_id)
        .bind(format!("message {i}"))
        .execute(&pool)
        .await
        .unwrap();
    }

    // Request with limit=2
    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "check_session_progress".into(),
        parameters: serde_json::json!({
            "session_id": child_id.to_string(),
            "limit": 2,
        }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, parent_id, user_id, &tool)
            .await;
    assert!(result.is_ok(), "check_progress should succeed: {result:?}");
    let val = result.unwrap();
    let messages = val["messages"].as_array().unwrap();
    assert_eq!(
        messages.len(),
        2,
        "should return exactly 2 messages with limit=2"
    );
}

/// check_session_progress with limit > 50 should cap at 50.
#[sqlx::test(migrations = "./migrations")]
async fn execute_check_progress_limit_capped(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, user_token) = create_user(&app, &admin_token, "ca-cap", "cacap@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let project_id = helpers::create_project(&app, &user_token, "cap-proj", "private").await;

    let parent_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider)
         VALUES ($1, $2, $3, 'parent', 'running', 'claude-code')",
    )
    .bind(parent_id)
    .bind(project_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let child_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, parent_session_id, spawn_depth)
         VALUES ($1, $2, $3, 'child', 'running', 'claude-code', $4, 1)",
    )
    .bind(child_id)
    .bind(project_id)
    .bind(user_id)
    .bind(parent_id)
    .execute(&pool)
    .await
    .unwrap();

    // Insert 3 messages
    for i in 0..3 {
        sqlx::query(
            "INSERT INTO agent_messages (session_id, role, content) VALUES ($1, 'user', $2)",
        )
        .bind(child_id)
        .bind(format!("msg {i}"))
        .execute(&pool)
        .await
        .unwrap();
    }

    // Request with limit=100 (should be capped to 50 internally)
    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "check_session_progress".into(),
        parameters: serde_json::json!({
            "session_id": child_id.to_string(),
            "limit": 100,
        }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, parent_id, user_id, &tool)
            .await;
    assert!(result.is_ok(), "check_progress should succeed: {result:?}");
    let val = result.unwrap();
    let messages = val["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3, "should return all 3 messages");
}

// ---------------------------------------------------------------------------
// execute_check_progress: returns cost_tokens and finished_at
// ---------------------------------------------------------------------------

/// check_session_progress returns cost_tokens and finished_at from the DB.
#[sqlx::test(migrations = "./migrations")]
async fn execute_check_progress_returns_cost_and_finished(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, user_token) = create_user(&app, &admin_token, "ca-cost", "cacost@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let project_id = helpers::create_project(&app, &user_token, "cost-proj", "private").await;

    let parent_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider)
         VALUES ($1, $2, $3, 'parent', 'running', 'claude-code')",
    )
    .bind(parent_id)
    .bind(project_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let child_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, parent_session_id, spawn_depth, cost_tokens, finished_at)
         VALUES ($1, $2, $3, 'child', 'completed', 'claude-code', $4, 1, 42000, now())",
    )
    .bind(child_id)
    .bind(project_id)
    .bind(user_id)
    .bind(parent_id)
    .execute(&pool)
    .await
    .unwrap();

    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "check_session_progress".into(),
        parameters: serde_json::json!({
            "session_id": child_id.to_string(),
        }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, parent_id, user_id, &tool)
            .await;
    assert!(result.is_ok(), "check_progress should succeed: {result:?}");
    let val = result.unwrap();
    assert_eq!(val["status"].as_str(), Some("completed"));
    assert_eq!(val["cost_tokens"].as_i64(), Some(42000));
    assert!(
        val["finished_at"].as_str().is_some(),
        "finished_at should be present: {val}"
    );
}

// ---------------------------------------------------------------------------
// execute_check_progress: nonexistent session
// ---------------------------------------------------------------------------

/// check_session_progress for a nonexistent session should fail.
#[sqlx::test(migrations = "./migrations")]
async fn execute_check_progress_nonexistent_session(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _) = create_user(&app, &admin_token, "ca-chkne", "cachkne@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let parent_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider)
         VALUES ($1, $2, 'parent', 'running', 'claude-code')",
    )
    .bind(parent_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let nonexistent = uuid::Uuid::new_v4();
    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "check_session_progress".into(),
        parameters: serde_json::json!({
            "session_id": nonexistent.to_string(),
        }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, parent_id, user_id, &tool)
            .await;
    assert!(
        result.is_err(),
        "nonexistent session should fail: {result:?}"
    );
    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("not found"),
        "error should mention 'not found': {err_msg}"
    );
}

// ---------------------------------------------------------------------------
// execute_create_app_tool: unknown tool returns error
// ---------------------------------------------------------------------------

/// Unknown tool name returns an error via execute_create_app_tool.
#[sqlx::test(migrations = "./migrations")]
async fn execute_unknown_tool_returns_error(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _) = create_user(&app, &admin_token, "ca-unknown", "caunknown@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let session_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider, execution_mode)
         VALUES ($1, $2, 'test', 'running', 'claude-code', 'cli_subprocess')",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "totally_bogus_tool".into(),
        parameters: serde_json::json!({}),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, session_id, user_id, &tool)
            .await;
    assert!(
        result.is_err(),
        "unknown tool should return error: {result:?}"
    );
    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("unknown tool"),
        "error should mention 'unknown tool': {err_msg}"
    );
}

// ---------------------------------------------------------------------------
// execute_create_project: missing name field
// ---------------------------------------------------------------------------

/// create_project without name field should fail.
#[sqlx::test(migrations = "./migrations")]
async fn execute_create_project_missing_name(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _) = create_user(&app, &admin_token, "ca-noname", "canoname@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let session_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider, execution_mode)
         VALUES ($1, $2, 'test', 'running', 'claude-code', 'cli_subprocess')",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "create_project".into(),
        parameters: serde_json::json!({ "description": "no name" }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, session_id, user_id, &tool)
            .await;
    assert!(
        result.is_err(),
        "missing name should be rejected: {result:?}"
    );
    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("name"),
        "error should mention 'name': {err_msg}"
    );
}

// ---------------------------------------------------------------------------
// execute_create_project: invalid display_name length
// ---------------------------------------------------------------------------

/// create_project with display_name > 255 chars should fail.
#[sqlx::test(migrations = "./migrations")]
async fn execute_create_project_display_name_too_long(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _) = create_user(&app, &admin_token, "ca-longdn", "calongdn@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let session_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider, execution_mode)
         VALUES ($1, $2, 'test', 'running', 'claude-code', 'cli_subprocess')",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let long_name = "x".repeat(256);
    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "create_project".into(),
        parameters: serde_json::json!({
            "name": "longdn-app",
            "display_name": long_name,
        }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, session_id, user_id, &tool)
            .await;
    assert!(
        result.is_err(),
        "display_name >255 should be rejected: {result:?}"
    );
    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("display_name"),
        "error should mention 'display_name': {err_msg}"
    );
}

// ---------------------------------------------------------------------------
// execute_check_progress: default limit (no limit provided)
// ---------------------------------------------------------------------------

/// check_session_progress without explicit limit uses default of 20.
#[sqlx::test(migrations = "./migrations")]
async fn execute_check_progress_default_limit(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, user_token) =
        create_user(&app, &admin_token, "ca-deflim", "cadeflim@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let project_id = helpers::create_project(&app, &user_token, "deflim-proj", "private").await;

    let parent_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider)
         VALUES ($1, $2, $3, 'parent', 'running', 'claude-code')",
    )
    .bind(parent_id)
    .bind(project_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let child_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, parent_session_id, spawn_depth)
         VALUES ($1, $2, $3, 'child', 'running', 'claude-code', $4, 1)",
    )
    .bind(child_id)
    .bind(project_id)
    .bind(user_id)
    .bind(parent_id)
    .execute(&pool)
    .await
    .unwrap();

    // Insert 25 messages to exceed default limit of 20
    for i in 0..25 {
        sqlx::query(
            "INSERT INTO agent_messages (session_id, role, content) VALUES ($1, 'user', $2)",
        )
        .bind(child_id)
        .bind(format!("msg {i}"))
        .execute(&pool)
        .await
        .unwrap();
    }

    // Request WITHOUT explicit limit (should default to 20)
    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "check_session_progress".into(),
        parameters: serde_json::json!({
            "session_id": child_id.to_string(),
        }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, parent_id, user_id, &tool)
            .await;
    assert!(result.is_ok(), "check_progress should succeed: {result:?}");
    let val = result.unwrap();
    let messages = val["messages"].as_array().unwrap();
    assert_eq!(
        messages.len(),
        20,
        "default limit should return 20 messages, got {}",
        messages.len()
    );
}

// ---------------------------------------------------------------------------
// execute_send_message: missing session_id field
// ---------------------------------------------------------------------------

/// send_message_to_session without session_id should fail.
#[sqlx::test(migrations = "./migrations")]
async fn execute_send_message_missing_session_id(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _) = create_user(&app, &admin_token, "ca-nosid", "canosid@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let parent_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider)
         VALUES ($1, $2, 'parent', 'running', 'claude-code')",
    )
    .bind(parent_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "send_message_to_session".into(),
        parameters: serde_json::json!({
            "message": "hello"
            // missing "session_id"
        }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, parent_id, user_id, &tool)
            .await;
    assert!(
        result.is_err(),
        "missing session_id should fail: {result:?}"
    );
    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("session_id"),
        "error should mention 'session_id': {err_msg}"
    );
}

// ---------------------------------------------------------------------------
// execute_check_progress: missing session_id field
// ---------------------------------------------------------------------------

/// check_session_progress without session_id should fail.
#[sqlx::test(migrations = "./migrations")]
async fn execute_check_progress_missing_session_id(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _) = create_user(&app, &admin_token, "ca-nosid2", "canosid2@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let parent_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider)
         VALUES ($1, $2, 'parent', 'running', 'claude-code')",
    )
    .bind(parent_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "check_session_progress".into(),
        parameters: serde_json::json!({
            // missing "session_id"
        }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, parent_id, user_id, &tool)
            .await;
    assert!(
        result.is_err(),
        "missing session_id should fail: {result:?}"
    );
    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("session_id"),
        "error should mention 'session_id': {err_msg}"
    );
}

// ---------------------------------------------------------------------------
// Additional tool coverage tests
// ---------------------------------------------------------------------------

/// `send_message_to_session` to nonexistent session returns error.
#[sqlx::test(migrations = "./migrations")]
async fn execute_send_message_nonexistent_session(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _) = create_user(&app, &admin_token, "ca-nosess", "canosess@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let manager_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider, execution_mode)
         VALUES ($1, $2, 'test', 'running', 'claude-code', 'cli_subprocess')",
    )
    .bind(manager_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "send_message_to_session".into(),
        parameters: serde_json::json!({
            "session_id": uuid::Uuid::new_v4().to_string(),
            "message": "hello worker"
        }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, manager_id, user_id, &tool)
            .await;
    assert!(
        result.is_err(),
        "nonexistent session should fail: {result:?}"
    );
    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("not found"),
        "error should mention not found: {err_msg}"
    );
}

/// `send_message_to_session` to an unrelated (non-child) session returns error.
#[sqlx::test(migrations = "./migrations")]
async fn execute_send_message_unrelated_session_rejected(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _) = create_user(&app, &admin_token, "ca-notchild", "canotchild@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let manager_id = uuid::Uuid::new_v4();
    let unrelated_id = uuid::Uuid::new_v4();

    for id in &[manager_id, unrelated_id] {
        sqlx::query(
            "INSERT INTO agent_sessions (id, user_id, prompt, status, provider, execution_mode)
             VALUES ($1, $2, 'test', 'running', 'claude-code', 'cli_subprocess')",
        )
        .bind(id)
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    }

    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "send_message_to_session".into(),
        parameters: serde_json::json!({
            "session_id": unrelated_id.to_string(),
            "message": "hello"
        }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, manager_id, user_id, &tool)
            .await;
    assert!(result.is_err(), "non-child session should fail: {result:?}");
    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("not a child"),
        "error should mention not a child: {err_msg}"
    );
}

/// `check_session_progress` for a non-child session returns error.
#[sqlx::test(migrations = "./migrations")]
async fn execute_check_progress_not_child(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _) = create_user(&app, &admin_token, "ca-chknotc", "cachknotc@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let parent_id = uuid::Uuid::new_v4();
    let unrelated_id = uuid::Uuid::new_v4();

    for id in &[parent_id, unrelated_id] {
        sqlx::query(
            "INSERT INTO agent_sessions (id, user_id, prompt, status, provider, execution_mode)
             VALUES ($1, $2, 'test', 'running', 'claude-code', 'cli_subprocess')",
        )
        .bind(id)
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    }

    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "check_session_progress".into(),
        parameters: serde_json::json!({
            "session_id": unrelated_id.to_string(),
        }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, parent_id, user_id, &tool)
            .await;
    assert!(result.is_err(), "unrelated session should fail: {result:?}");
    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("not a child"),
        "error should mention not a child: {err_msg}"
    );
}

/// `check_session_progress` returns messages and status for a valid child session.
#[sqlx::test(migrations = "./migrations")]
async fn execute_check_progress_returns_messages_and_status(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _) = create_user(&app, &admin_token, "ca-chkmsg", "cachkmsg@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let parent_id = uuid::Uuid::new_v4();
    let child_id = uuid::Uuid::new_v4();

    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider, execution_mode)
         VALUES ($1, $2, 'manager', 'running', 'claude-code', 'cli_subprocess')",
    )
    .bind(parent_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider, execution_mode, parent_session_id, spawn_depth)
         VALUES ($1, $2, 'worker', 'running', 'claude-code', 'pod', $3, 1)",
    )
    .bind(child_id)
    .bind(user_id)
    .bind(parent_id)
    .execute(&pool)
    .await
    .unwrap();

    // Insert messages for the child
    sqlx::query(
        "INSERT INTO agent_messages (session_id, role, content) VALUES ($1, 'user', 'hello')",
    )
    .bind(child_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO agent_messages (session_id, role, content) VALUES ($1, 'assistant', 'hi there')")
        .bind(child_id)
        .execute(&pool)
        .await
        .unwrap();

    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "check_session_progress".into(),
        parameters: serde_json::json!({
            "session_id": child_id.to_string(),
        }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, parent_id, user_id, &tool)
            .await;
    assert!(result.is_ok(), "check_progress should succeed: {result:?}");

    let val = result.unwrap();
    assert_eq!(val["status"].as_str(), Some("running"));
    assert_eq!(
        val["session_id"].as_str(),
        Some(child_id.to_string().as_str())
    );
    let messages = val["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
}

/// `send_message_to_session` to a non-running child session returns error.
#[sqlx::test(migrations = "./migrations")]
async fn execute_send_message_child_not_running(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _) =
        create_user(&app, &admin_token, "ca-childstop", "cachildstop@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let parent_id = uuid::Uuid::new_v4();
    let child_id = uuid::Uuid::new_v4();

    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider, execution_mode)
         VALUES ($1, $2, 'manager', 'running', 'claude-code', 'cli_subprocess')",
    )
    .bind(parent_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider, execution_mode, parent_session_id, spawn_depth)
         VALUES ($1, $2, 'worker', 'completed', 'claude-code', 'pod', $3, 1)",
    )
    .bind(child_id)
    .bind(user_id)
    .bind(parent_id)
    .execute(&pool)
    .await
    .unwrap();

    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "send_message_to_session".into(),
        parameters: serde_json::json!({
            "session_id": child_id.to_string(),
            "message": "hello"
        }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, parent_id, user_id, &tool)
            .await;
    assert!(result.is_err(), "stopped child should fail: {result:?}");
    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("not running"),
        "error should mention not running: {err_msg}"
    );
}

/// `create_project` with display_name and description succeeds.
#[sqlx::test(migrations = "./migrations")]
async fn execute_create_project_with_display_name_and_desc(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _) = create_user(&app, &admin_token, "ca-full", "cafull@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let session_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider, execution_mode)
         VALUES ($1, $2, 'test', 'running', 'claude-code', 'cli_subprocess')",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "create_project".into(),
        parameters: serde_json::json!({
            "name": "full-app",
            "display_name": "Full Application",
            "description": "A complete application with all features"
        }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, session_id, user_id, &tool)
            .await;
    assert!(
        result.is_ok(),
        "create_project with all fields should succeed: {result:?}"
    );

    let val = result.unwrap();
    assert_eq!(val["name"].as_str(), Some("full-app"));

    // Verify display_name and description in DB
    let project_id = uuid::Uuid::parse_str(val["project_id"].as_str().unwrap()).unwrap();
    let row: (Option<String>, Option<String>) =
        sqlx::query_as("SELECT display_name, description FROM projects WHERE id = $1")
            .bind(project_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row.0.as_deref(), Some("Full Application"));
    assert_eq!(
        row.1.as_deref(),
        Some("A complete application with all features")
    );
}

/// `spawn_coding_agent` with nonexistent project_id returns error.
#[sqlx::test(migrations = "./migrations")]
async fn execute_spawn_agent_nonexistent_project(pool: PgPool) {
    let (state, admin_token) = test_state(pool.clone()).await;
    let app = test_router(state.clone());

    let (user_id, _) = create_user(&app, &admin_token, "ca-noproj", "canoproj@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let session_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, status, provider, execution_mode)
         VALUES ($1, $2, 'test', 'running', 'claude-code', 'cli_subprocess')",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let tool = platform::agent::cli_invoke::ToolRequest {
        name: "spawn_coding_agent".into(),
        parameters: serde_json::json!({
            "project_id": uuid::Uuid::new_v4().to_string(),
            "prompt": "build it"
        }),
    };

    let result =
        platform::agent::create_app::execute_create_app_tool(&state, session_id, user_id, &tool)
            .await;
    assert!(
        result.is_err(),
        "nonexistent project should fail: {result:?}"
    );
}
