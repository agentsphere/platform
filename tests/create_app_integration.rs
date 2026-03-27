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

    let (user_id, _token) = create_user(&app, &admin_token, "ca-create", "cacreate@test.com").await;
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

    let (user_id, _token) = create_user(&app, &admin_token, "ca-dup", "cadup@test.com").await;
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

    let (user_id, _token) =
        create_user(&app, &admin_token, "ca-badname", "cabadname@test.com").await;
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

    let (user_id, _token) =
        create_user(&app, &admin_token, "ca-noprompt", "canoprompt@test.com").await;
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

    let (user_id, _token) =
        create_user(&app, &admin_token, "ca-sendmsg", "casendmsg@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let project_id = helpers::create_project(&app, &_token, "send-msg-proj", "private").await;

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

    let (user_id, _token) =
        create_user(&app, &admin_token, "ca-wrongpar", "cawrongpar@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let project_id = helpers::create_project(&app, &_token, "wrong-par-proj", "private").await;

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

    let (user_id, _token) = create_user(&app, &admin_token, "ca-notrun", "canotrun@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let project_id = helpers::create_project(&app, &_token, "notrun-proj", "private").await;

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

    let (user_id, _token) =
        create_user(&app, &admin_token, "ca-progress", "caprogress@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let project_id = helpers::create_project(&app, &_token, "progress-proj", "private").await;

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

    let (user_id, _token) =
        create_user(&app, &admin_token, "ca-chkwrong", "cachkwrong@test.com").await;
    assign_role(&app, &admin_token, user_id, "developer", None, &pool).await;

    let project_id = helpers::create_project(&app, &_token, "chk-wrong-proj", "private").await;

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
