//! Integration tests for the in-process agent (create-app flow).
//!
//! Uses `MockAnthropicServer` to simulate the Anthropic Messages API,
//! exercising the full `run_turn` → `execute_tool` → save pipeline
//! without making real API calls.

mod helpers;
mod mock_anthropic;

use axum::http::StatusCode;
use mock_anthropic::{ContentBlock, MockAnthropicServer, MockResponse};
use sqlx::PgPool;
use uuid::Uuid;

use platform::agent::inprocess;
use platform::agent::provider::{ProgressEvent, ProgressKind};

// ---------------------------------------------------------------------------
// Helper: create a session wired to the mock server
// ---------------------------------------------------------------------------

/// Create an in-process session with a mock Anthropic server.
/// Inserts the DB row and handle with `api_url` pointing at the mock.
async fn setup_inprocess_session(
    state: &platform::store::AppState,
    user_id: Uuid,
    mock: &MockAnthropicServer,
    description: &str,
) -> Uuid {
    // Store API key for user
    helpers::set_user_api_key(&state.pool, user_id).await;

    // Create handle with mock URL
    let mut handle = inprocess::InProcessHandle::new("sk-ant-test-dummy-key".into(), None, user_id);
    handle.api_url = Some(mock.url());

    let session_id = Uuid::new_v4();

    // Insert DB row
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, provider, status) \
         VALUES ($1, $2, $3, 'claude-code', 'running')",
    )
    .bind(session_id)
    .bind(user_id)
    .bind(description)
    .execute(&state.pool)
    .await
    .expect("insert session");

    // Store handle
    {
        let mut sessions = state.inprocess_sessions.write().unwrap();
        sessions.insert(session_id, handle.clone());
    }

    // Save first user message to DB and history
    sqlx::query("INSERT INTO agent_messages (session_id, role, content) VALUES ($1, 'user', $2)")
        .bind(session_id)
        .bind(description)
        .execute(&state.pool)
        .await
        .expect("insert first message");

    {
        let mut msgs = handle.messages.write().await;
        msgs.push(platform::agent::anthropic::ChatMessage::user(description));
    }

    session_id
}

/// Ensure the git_repos_path and ops_repos_path directories exist.
fn ensure_temp_dirs(state: &platform::store::AppState) {
    std::fs::create_dir_all(&state.config.git_repos_path).ok();
    std::fs::create_dir_all(&state.config.ops_repos_path).ok();
}

/// Collect all progress events from a broadcast receiver (non-blocking drain).
fn drain_events(rx: &mut tokio::sync::broadcast::Receiver<ProgressEvent>) -> Vec<ProgressEvent> {
    let mut events = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(event) => events.push(event),
            Err(_) => break,
        }
    }
    events
}

/// Get admin user_id from the DB.
async fn get_admin_id(pool: &PgPool) -> Uuid {
    let row: (Uuid,) = sqlx::query_as("SELECT id FROM users WHERE name = 'admin'")
        .fetch_one(pool)
        .await
        .expect("admin must exist");
    row.0
}

/// Wait for a Completed event on the broadcast channel (with timeout).
async fn wait_for_completion(
    rx: &mut tokio::sync::broadcast::Receiver<ProgressEvent>,
) -> Vec<ProgressEvent> {
    let mut events = Vec::new();
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(event)) => {
                let done =
                    event.kind == ProgressKind::Completed || event.kind == ProgressKind::Error;
                events.push(event);
                if done {
                    break;
                }
            }
            Ok(Err(_)) => break, // channel closed
            Err(_) => break,     // timeout
        }
    }
    // Drain any remaining
    events.extend(drain_events(rx));
    events
}

// ===========================================================================
// Tests
// ===========================================================================

// --- Text-only conversation (exercises run_turn → save_assistant_text) ---

#[sqlx::test(migrations = "./migrations")]
async fn inprocess_text_response(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let mock = MockAnthropicServer::start().await;
    let admin_id = get_admin_id(&state.pool).await;

    mock.enqueue(MockResponse::text("Hello! How can I help you?"))
        .await;

    let session_id = setup_inprocess_session(&state, admin_id, &mock, "Build me an app").await;
    let handle = {
        let sessions = state.inprocess_sessions.read().unwrap();
        sessions.get(&session_id).cloned().unwrap()
    };
    let mut rx = handle.subscribe();

    // Call the REAL run_turn via the pub(crate) wrapper
    inprocess::run_turn_for_session(&state, session_id)
        .await
        .unwrap();

    // Verify text event was emitted
    let events = drain_events(&mut rx);
    assert!(
        events.iter().any(|e| e.kind == ProgressKind::Text),
        "should have Text progress event"
    );
    assert!(
        events.iter().any(|e| e.kind == ProgressKind::Completed),
        "should have Completed progress event"
    );

    // Verify message saved to DB
    let count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM agent_messages WHERE session_id = $1 AND role = 'assistant'",
    )
    .bind(session_id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert!(count.0 >= 1, "assistant message should be in DB");

    // Verify request was captured
    let requests = mock.requests().await;
    assert_eq!(requests.len(), 1);
    assert!(requests[0].stream, "should request streaming");
    assert!(requests[0].max_tokens > 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn inprocess_followup_message(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let mock = MockAnthropicServer::start().await;
    let admin_id = get_admin_id(&state.pool).await;

    // First turn: text response
    mock.enqueue(MockResponse::text("What framework?")).await;
    // Second turn: text response
    mock.enqueue(MockResponse::text("Great choice!")).await;

    let session_id = setup_inprocess_session(&state, admin_id, &mock, "Build me a blog").await;

    // First turn via real run_turn
    inprocess::run_turn_for_session(&state, session_id)
        .await
        .unwrap();

    // Add follow-up message
    let handle = {
        let sessions = state.inprocess_sessions.read().unwrap();
        sessions.get(&session_id).cloned().unwrap()
    };
    {
        let mut msgs = handle.messages.write().await;
        msgs.push(platform::agent::anthropic::ChatMessage::user("Use React"));
    }
    sqlx::query(
        "INSERT INTO agent_messages (session_id, role, content) VALUES ($1, 'user', 'Use React')",
    )
    .bind(session_id)
    .execute(&state.pool)
    .await
    .unwrap();

    // Second turn via real run_turn
    inprocess::run_turn_for_session(&state, session_id)
        .await
        .unwrap();

    // Verify two API calls were made
    let requests = mock.requests().await;
    assert_eq!(requests.len(), 2);

    // Second request should have more messages
    let second_msgs = requests[1].messages.as_array().unwrap();
    assert!(
        second_msgs.len() > 1,
        "second call should include conversation history"
    );
}

// --- Tool-use (exercises run_turn → execute_tool → execute_create_project) ---

#[sqlx::test(migrations = "./migrations")]
async fn inprocess_tool_use_creates_tool_call_event(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let mock = MockAnthropicServer::start().await;
    let admin_id = get_admin_id(&state.pool).await;
    ensure_temp_dirs(&state);

    // First response: tool_use (create_project)
    mock.enqueue(MockResponse::tool_use(vec![ContentBlock::ToolUse {
        id: "toolu_01".into(),
        name: "create_project".into(),
        input: serde_json::json!({"name": "test-tool-app"}),
    }]))
    .await;
    // Second response: text (ends the loop)
    mock.enqueue(MockResponse::text("Project created successfully!"))
        .await;

    let session_id = setup_inprocess_session(&state, admin_id, &mock, "Create a project").await;
    let handle = {
        let sessions = state.inprocess_sessions.read().unwrap();
        sessions.get(&session_id).cloned().unwrap()
    };
    let mut rx = handle.subscribe();

    // Real run_turn: will execute create_project tool, then loop for text
    inprocess::run_turn_for_session(&state, session_id)
        .await
        .unwrap();

    let events = drain_events(&mut rx);
    assert!(
        events
            .iter()
            .any(|e| e.kind == ProgressKind::ToolCall && e.message == "create_project"),
        "should have ToolCall event for create_project, got: {events:?}"
    );
    assert!(
        events.iter().any(|e| e.kind == ProgressKind::ToolResult),
        "should have ToolResult event"
    );

    // Verify project was actually created in DB
    let project_count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM projects WHERE name = 'test-tool-app'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(project_count.0, 1, "project should exist in DB");

    // Session should be linked to the project
    let linked: Option<(Option<Uuid>,)> =
        sqlx::query_as("SELECT project_id FROM agent_sessions WHERE id = $1")
            .bind(session_id)
            .fetch_optional(&state.pool)
            .await
            .unwrap();
    assert!(
        linked.unwrap().0.is_some(),
        "session should be linked to project"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn inprocess_text_then_tool_use(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let mock = MockAnthropicServer::start().await;
    let admin_id = get_admin_id(&state.pool).await;
    ensure_temp_dirs(&state);

    // Response with text followed by tool_use
    mock.enqueue(MockResponse::text_then_tools(
        "Let me create that for you.",
        vec![ContentBlock::ToolUse {
            id: "toolu_02".into(),
            name: "create_project".into(),
            input: serde_json::json!({"name": "text-then-tool"}),
        }],
    ))
    .await;
    // Follow-up text to end the loop
    mock.enqueue(MockResponse::text("All done!")).await;

    let session_id = setup_inprocess_session(&state, admin_id, &mock, "Blog app").await;
    let handle = {
        let sessions = state.inprocess_sessions.read().unwrap();
        sessions.get(&session_id).cloned().unwrap()
    };
    let mut rx = handle.subscribe();

    inprocess::run_turn_for_session(&state, session_id)
        .await
        .unwrap();

    let events = drain_events(&mut rx);
    // Should have both text and tool events
    assert!(events.iter().any(|e| e.kind == ProgressKind::Text));
    assert!(events.iter().any(|e| e.kind == ProgressKind::ToolCall));
    assert!(events.iter().any(|e| e.kind == ProgressKind::ToolResult));

    // Text before tools should be saved to DB (save_assistant_text_only_db path)
    let msgs: Vec<(String,)> = sqlx::query_as(
        "SELECT content FROM agent_messages WHERE session_id = $1 AND role = 'assistant' ORDER BY created_at",
    )
    .bind(session_id)
    .fetch_all(&state.pool)
    .await
    .unwrap();
    assert!(
        msgs.iter().any(|m| m.0.contains("Let me create that")),
        "text before tools should be saved to DB"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn inprocess_multiple_tool_use_blocks(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let mock = MockAnthropicServer::start().await;
    let admin_id = get_admin_id(&state.pool).await;
    ensure_temp_dirs(&state);

    // First: create_project tool
    mock.enqueue(MockResponse::tool_use(vec![ContentBlock::ToolUse {
        id: "toolu_a".into(),
        name: "create_project".into(),
        input: serde_json::json!({"name": "multi-tool-proj"}),
    }]))
    .await;

    // After project is created, model asks to create ops repo.
    // We need to know the project_id, but the mock can't know it ahead of time.
    // Instead, test the "unknown tool" error path with a second tool.
    mock.enqueue(MockResponse::tool_use(vec![ContentBlock::ToolUse {
        id: "toolu_b".into(),
        name: "unknown_tool".into(),
        input: serde_json::json!({}),
    }]))
    .await;
    // End with text
    mock.enqueue(MockResponse::text("Done with tools!")).await;

    let session_id = setup_inprocess_session(&state, admin_id, &mock, "Multi-tool test").await;
    let handle = {
        let sessions = state.inprocess_sessions.read().unwrap();
        sessions.get(&session_id).cloned().unwrap()
    };
    let mut rx = handle.subscribe();

    inprocess::run_turn_for_session(&state, session_id)
        .await
        .unwrap();

    let events = drain_events(&mut rx);
    let tool_calls: Vec<_> = events
        .iter()
        .filter(|e| e.kind == ProgressKind::ToolCall)
        .collect();
    assert!(
        tool_calls.len() >= 2,
        "should have at least 2 ToolCall events, got {}: {tool_calls:?}",
        tool_calls.len()
    );

    // The unknown_tool should produce an error result
    let tool_results: Vec<_> = events
        .iter()
        .filter(|e| e.kind == ProgressKind::ToolResult)
        .collect();
    assert!(
        tool_results.iter().any(|e| e.message.contains("error")),
        "unknown tool should produce error result"
    );
}

// --- Error handling ---

#[sqlx::test(migrations = "./migrations")]
async fn inprocess_api_error_propagates(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let mock = MockAnthropicServer::start().await;
    let admin_id = get_admin_id(&state.pool).await;

    mock.enqueue(MockResponse::error(429, "rate limit exceeded"))
        .await;

    let session_id = setup_inprocess_session(&state, admin_id, &mock, "test").await;

    // run_turn_for_session should propagate the error
    let result = inprocess::run_turn_for_session(&state, session_id).await;
    assert!(result.is_err(), "should return error on 429");
}

#[sqlx::test(migrations = "./migrations")]
async fn inprocess_thinking_delta_emits_event(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let mock = MockAnthropicServer::start().await;
    let admin_id = get_admin_id(&state.pool).await;

    mock.enqueue(MockResponse {
        content_blocks: vec![
            ContentBlock::Thinking("Let me think about this...".into()),
            ContentBlock::Text("Here's my answer.".into()),
        ],
        stop_reason: "end_turn".into(),
        status_code: 200,
        error_message: None,
    })
    .await;

    let session_id = setup_inprocess_session(&state, admin_id, &mock, "think test").await;
    let handle = {
        let sessions = state.inprocess_sessions.read().unwrap();
        sessions.get(&session_id).cloned().unwrap()
    };
    let mut rx = handle.subscribe();

    inprocess::run_turn_for_session(&state, session_id)
        .await
        .unwrap();

    let events = drain_events(&mut rx);
    assert!(
        events.iter().any(|e| e.kind == ProgressKind::Thinking),
        "should have Thinking progress event"
    );
    assert!(
        events.iter().any(|e| e.kind == ProgressKind::Text),
        "should also have Text progress event"
    );
}

// --- Session management ---

#[sqlx::test(migrations = "./migrations")]
async fn inprocess_subscribe_and_remove(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let mock = MockAnthropicServer::start().await;
    let admin_id = get_admin_id(&state.pool).await;

    let session_id = setup_inprocess_session(&state, admin_id, &mock, "lifecycle test").await;

    // Subscribe works
    let sub = inprocess::subscribe(&state, session_id);
    assert!(sub.is_some(), "subscribe should return a receiver");

    // Remove session
    inprocess::remove_session(&state, session_id);

    // Subscribe returns None after removal
    let sub = inprocess::subscribe(&state, session_id);
    assert!(sub.is_none(), "subscribe should return None after removal");

    // Session handle is gone
    let sessions = state.inprocess_sessions.read().unwrap();
    assert!(
        !sessions.contains_key(&session_id),
        "handle should be removed"
    );
}

// --- Request validation ---

#[sqlx::test(migrations = "./migrations")]
async fn inprocess_request_matches_anthropic_contract(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let mock = MockAnthropicServer::start().await;
    let admin_id = get_admin_id(&state.pool).await;

    mock.enqueue(MockResponse::text("ok")).await;

    let session_id = setup_inprocess_session(&state, admin_id, &mock, "contract test").await;
    inprocess::run_turn_for_session(&state, session_id)
        .await
        .unwrap();

    let requests = mock.requests().await;
    assert_eq!(requests.len(), 1);
    let req = &requests[0];

    // Anthropic API contract requirements
    assert!(req.stream, "must request stream:true");
    assert!(req.max_tokens > 0, "must specify max_tokens");
    assert!(!req.model.is_empty(), "must specify model");

    // Messages should be an array with at least the user message
    let msgs = req.messages.as_array().expect("messages must be array");
    assert!(!msgs.is_empty(), "messages must not be empty");
    assert_eq!(msgs[0]["role"], "user");

    // Tools should be an array of tool definitions
    let tools = req.tools.as_array().expect("tools must be array");
    assert!(!tools.is_empty(), "tools must not be empty");
    for tool in tools {
        assert!(tool.get("name").is_some(), "each tool needs name");
        assert!(tool.get("input_schema").is_some(), "each tool needs schema");
    }
}

// --- No API key ---

#[sqlx::test(migrations = "./migrations")]
async fn inprocess_no_api_key_returns_error(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let admin_id = get_admin_id(&state.pool).await;

    // Don't set API key — should fail
    let result = inprocess::create_inprocess_session(
        &state,
        admin_id,
        "test without key",
        "claude-code",
        None,
    )
    .await;

    assert!(result.is_err(), "should fail without API key");
    let err = format!("{:?}", result.unwrap_err());
    assert!(
        err.contains("API key") || err.contains("api_key") || err.contains("Anthropic"),
        "error should mention API key: {err}"
    );
}

// --- Create-app session via the real create_inprocess_session (end-to-end) ---

#[sqlx::test(migrations = "./migrations")]
async fn inprocess_create_session_via_api(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());
    let admin_id = get_admin_id(&state.pool).await;

    // Set API key for admin
    helpers::set_user_api_key(&state.pool, admin_id).await;

    // The background turn will fail (mock not set up for /api/create-app route),
    // but the session should still be created in DB.
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/create-app",
        serde_json::json!({"description": "A todo app"}),
    )
    .await;

    // Should succeed with session creation
    assert_eq!(status, StatusCode::CREATED, "create-app failed: {body}");
    assert!(body.get("id").is_some(), "should return id");

    // Session should exist in DB
    let session_id = body["id"].as_str().unwrap();
    let sid = Uuid::parse_str(session_id).unwrap();
    let row: Option<(String,)> = sqlx::query_as("SELECT status FROM agent_sessions WHERE id = $1")
        .bind(sid)
        .fetch_optional(&state.pool)
        .await
        .unwrap();
    assert!(row.is_some(), "session should be in DB");
    assert_eq!(row.unwrap().0, "running");
}

// --- create_inprocess_session with mock URL (exercises full lifecycle) ---

#[sqlx::test(migrations = "./migrations")]
async fn inprocess_create_session_with_mock(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let mock = MockAnthropicServer::start().await;
    let admin_id = get_admin_id(&state.pool).await;

    // Set API key for admin
    helpers::set_user_api_key(&state.pool, admin_id).await;

    // Enqueue a text response for the background task
    mock.enqueue(MockResponse::text("Sure, let me help!")).await;

    // Use the REAL create_inprocess_session with mock URL
    let session_id = inprocess::create_inprocess_session(
        &state,
        admin_id,
        "Build me a blog",
        "claude-code",
        Some(&mock.url()),
    )
    .await
    .unwrap();

    // Subscribe and wait for completion from the background task
    let mut rx = {
        let sessions = state.inprocess_sessions.read().unwrap();
        sessions.get(&session_id).unwrap().subscribe()
    };

    let events = wait_for_completion(&mut rx).await;
    assert!(
        events.iter().any(|e| e.kind == ProgressKind::Text),
        "background turn should emit Text event"
    );
    assert!(
        events.iter().any(|e| e.kind == ProgressKind::Completed),
        "background turn should emit Completed event"
    );

    // Verify session in DB
    let row: (String,) = sqlx::query_as("SELECT status FROM agent_sessions WHERE id = $1")
        .bind(session_id)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(row.0, "running");

    // Verify API call was made
    let requests = mock.requests().await;
    assert_eq!(requests.len(), 1);
}

// --- send_inprocess_message (exercises the public follow-up API) ---

#[sqlx::test(migrations = "./migrations")]
async fn inprocess_send_message_triggers_turn(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let mock = MockAnthropicServer::start().await;
    let admin_id = get_admin_id(&state.pool).await;

    // Set up a session with initial text response
    mock.enqueue(MockResponse::text("First response")).await;
    let session_id = setup_inprocess_session(&state, admin_id, &mock, "initial").await;
    inprocess::run_turn_for_session(&state, session_id)
        .await
        .unwrap();

    // Enqueue response for the follow-up
    mock.enqueue(MockResponse::text("Follow-up response")).await;

    // Subscribe before sending
    let mut rx = {
        let sessions = state.inprocess_sessions.read().unwrap();
        sessions.get(&session_id).unwrap().subscribe()
    };

    // Use the REAL send_inprocess_message
    inprocess::send_inprocess_message(&state, session_id, "follow up question")
        .await
        .unwrap();

    // Wait for the background task to complete
    let events = wait_for_completion(&mut rx).await;
    assert!(
        events.iter().any(|e| e.kind == ProgressKind::Text),
        "follow-up should emit Text event"
    );

    // Verify user message saved to DB
    let count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM agent_messages WHERE session_id = $1 AND role = 'user'",
    )
    .bind(session_id)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert!(count.0 >= 2, "should have at least 2 user messages");
}

#[sqlx::test(migrations = "./migrations")]
async fn inprocess_send_message_nonexistent_session(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;

    let result = inprocess::send_inprocess_message(&state, Uuid::new_v4(), "hello").await;
    assert!(result.is_err(), "should fail for nonexistent session");
}

// --- Conversation history accumulates ---

#[sqlx::test(migrations = "./migrations")]
async fn inprocess_conversation_history_grows(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let mock = MockAnthropicServer::start().await;
    let admin_id = get_admin_id(&state.pool).await;

    mock.enqueue(MockResponse::text("Response 1")).await;
    mock.enqueue(MockResponse::text("Response 2")).await;

    let session_id = setup_inprocess_session(&state, admin_id, &mock, "history test").await;

    // First turn
    inprocess::run_turn_for_session(&state, session_id)
        .await
        .unwrap();

    // Add second user message
    let handle = {
        let sessions = state.inprocess_sessions.read().unwrap();
        sessions.get(&session_id).cloned().unwrap()
    };
    {
        let mut msgs = handle.messages.write().await;
        msgs.push(platform::agent::anthropic::ChatMessage::user(
            "Follow up question",
        ));
    }

    // Second turn
    inprocess::run_turn_for_session(&state, session_id)
        .await
        .unwrap();

    // Verify conversation history has grown
    let messages = handle.messages.read().await;
    // user + assistant + user + assistant = 4
    assert!(
        messages.len() >= 4,
        "history should have at least 4 messages, got {}",
        messages.len()
    );

    // Verify the second API call got the full history
    let requests = mock.requests().await;
    assert_eq!(requests.len(), 2);
    let second_msgs = requests[1].messages.as_array().unwrap();
    assert!(
        second_msgs.len() >= 3,
        "second request should include prior history, got {}",
        second_msgs.len()
    );
}

// --- Tool input is correctly parsed from SSE chunks ---

#[sqlx::test(migrations = "./migrations")]
async fn inprocess_tool_input_parsed_from_chunks(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let mock = MockAnthropicServer::start().await;
    let admin_id = get_admin_id(&state.pool).await;
    ensure_temp_dirs(&state);

    let complex_input = serde_json::json!({
        "name": "chunk-parse-app",
        "display_name": "Chunk Parse App",
        "description": "A complex application with many features"
    });

    // Tool use response
    mock.enqueue(MockResponse::tool_use(vec![ContentBlock::ToolUse {
        id: "toolu_complex".into(),
        name: "create_project".into(),
        input: complex_input.clone(),
    }]))
    .await;
    // Follow-up text to end loop
    mock.enqueue(MockResponse::text("Created!")).await;

    let session_id = setup_inprocess_session(&state, admin_id, &mock, "chunk test").await;
    let handle = {
        let sessions = state.inprocess_sessions.read().unwrap();
        sessions.get(&session_id).cloned().unwrap()
    };
    let mut rx = handle.subscribe();

    inprocess::run_turn_for_session(&state, session_id)
        .await
        .unwrap();

    // Verify tool call was parsed with correct input
    let events = drain_events(&mut rx);
    let tool_call = events
        .iter()
        .find(|e| e.kind == ProgressKind::ToolCall)
        .expect("should have ToolCall event");
    assert_eq!(tool_call.message, "create_project");

    // Project should be created with the parsed display_name
    let project: Option<(String,)> =
        sqlx::query_as("SELECT display_name FROM projects WHERE name = 'chunk-parse-app'")
            .fetch_optional(&state.pool)
            .await
            .unwrap();
    assert_eq!(
        project.unwrap().0,
        "Chunk Parse App",
        "display_name should be parsed from chunks"
    );
}

// --- Tool execution: create_project auto-creates ops repo ---

#[sqlx::test(migrations = "./migrations")]
async fn inprocess_create_project_auto_creates_ops_repo(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let mock = MockAnthropicServer::start().await;
    let admin_id = get_admin_id(&state.pool).await;
    ensure_temp_dirs(&state);

    // create_project now automatically sets up infra including ops repo
    mock.enqueue(MockResponse::tool_use(vec![ContentBlock::ToolUse {
        id: "toolu_proj".into(),
        name: "create_project".into(),
        input: serde_json::json!({"name": "ops-repo-test"}),
    }]))
    .await;
    mock.enqueue(MockResponse::text("Project created.")).await;

    let session_id = setup_inprocess_session(&state, admin_id, &mock, "ops repo test").await;

    inprocess::run_turn_for_session(&state, session_id)
        .await
        .unwrap();

    // Verify ops repo was auto-created by setup_project_infrastructure
    let ops_count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM ops_repos WHERE name = 'ops-repo-test-ops'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(
        ops_count.0, 1,
        "ops repo should be auto-created with project"
    );
}

// --- Tool execution: spawn_coding_agent (exercises error path — namespace not created in K8s) ---

#[sqlx::test(migrations = "./migrations")]
async fn inprocess_spawn_agent_tool_error_handled(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let mock = MockAnthropicServer::start().await;
    let admin_id = get_admin_id(&state.pool).await;

    // Insert project directly via SQL — deliberately skip setup_project_infrastructure
    // so the K8s namespace "spawn-err-test-dev" does NOT exist.
    let project_id = Uuid::new_v4();
    let workspace_id: Uuid =
        sqlx::query_scalar("SELECT id FROM workspaces WHERE owner_id = $1 LIMIT 1")
            .bind(admin_id)
            .fetch_optional(&state.pool)
            .await
            .unwrap()
            .unwrap_or_else(|| {
                // No workspace yet — will be created below
                Uuid::new_v4()
            });

    // Ensure workspace exists
    sqlx::query(
        "INSERT INTO workspaces (id, name, owner_id) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
    )
    .bind(workspace_id)
    .bind(format!("ws-{workspace_id}"))
    .bind(admin_id)
    .execute(&state.pool)
    .await
    .unwrap();

    // Fetch the actual workspace id after potential conflict
    let workspace_id: Uuid =
        sqlx::query_scalar("SELECT id FROM workspaces WHERE owner_id = $1 LIMIT 1")
            .bind(admin_id)
            .fetch_one(&state.pool)
            .await
            .unwrap();

    sqlx::query(
        "INSERT INTO projects (id, name, owner_id, visibility, repo_path, workspace_id, namespace_slug) \
         VALUES ($1, 'spawn-err-test', $2, 'private', '/tmp/fake-repo', $3, 'spawn-err-test')",
    )
    .bind(project_id)
    .bind(admin_id)
    .bind(workspace_id)
    .execute(&state.pool)
    .await
    .unwrap();

    // Now try spawn_coding_agent — will fail because namespace "spawn-err-test-dev"
    // doesn't exist in K8s (we skipped infrastructure setup).
    mock.enqueue(MockResponse::tool_use(vec![ContentBlock::ToolUse {
        id: "toolu_spawn".into(),
        name: "spawn_coding_agent".into(),
        input: serde_json::json!({
            "project_id": project_id.to_string(),
            "prompt": "Build a REST API"
        }),
    }]))
    .await;
    // Model gets error result, responds with text
    mock.enqueue(MockResponse::text("Sorry, agent spawn failed."))
        .await;

    let session_id = setup_inprocess_session(&state, admin_id, &mock, "spawn test").await;
    let handle = {
        let sessions = state.inprocess_sessions.read().unwrap();
        sessions.get(&session_id).cloned().unwrap()
    };
    let mut rx = handle.subscribe();

    inprocess::run_turn_for_session(&state, session_id)
        .await
        .unwrap();

    let events = drain_events(&mut rx);
    // Should have tool call and error result
    assert!(
        events
            .iter()
            .any(|e| e.kind == ProgressKind::ToolCall && e.message == "spawn_coding_agent"),
        "should have spawn_coding_agent ToolCall"
    );
    let error_result = events
        .iter()
        .find(|e| e.kind == ProgressKind::ToolResult && e.message.contains("error"));
    assert!(
        error_result.is_some(),
        "spawn should produce error result (namespace not in K8s): got events: {:?}",
        events
            .iter()
            .map(|e| format!("{:?}: {}", e.kind, e.message))
            .collect::<Vec<_>>(),
    );
}

// --- resolve_user_api_key path via create_inprocess_session ---

#[sqlx::test(migrations = "./migrations")]
async fn inprocess_resolve_api_key_success(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let mock = MockAnthropicServer::start().await;
    let admin_id = get_admin_id(&state.pool).await;

    // Set API key
    helpers::set_user_api_key(&state.pool, admin_id).await;

    // Enqueue response
    mock.enqueue(MockResponse::text("Key resolved!")).await;

    // create_inprocess_session calls resolve_user_api_key internally
    let session_id = inprocess::create_inprocess_session(
        &state,
        admin_id,
        "key test",
        "claude-code",
        Some(&mock.url()),
    )
    .await
    .unwrap();

    // Wait for background turn
    let mut rx = {
        let sessions = state.inprocess_sessions.read().unwrap();
        sessions.get(&session_id).unwrap().subscribe()
    };
    let events = wait_for_completion(&mut rx).await;
    assert!(
        events.iter().any(|e| e.kind == ProgressKind::Completed),
        "turn should complete successfully with resolved key"
    );
}
