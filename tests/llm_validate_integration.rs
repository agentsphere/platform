//! Integration tests for `src/agent/llm_validate.rs` — LLM provider validation.
//!
//! Tests exercise `run_validation()` with the mock CLI subprocess. The mock CLI
//! (at `CLAUDE_CLI_PATH`) emits canned NDJSON with `"Hello!"` text and empty
//! tools, which causes test_connection to pass but test_output_format and
//! test_session_memory to fail (the expected behavior with a mock).
//!
//! These tests call `run_validation` directly (it's `pub`) rather than going
//! through the SSE API endpoint, isolating the validation logic from HTTP/SSE
//! transport concerns.

mod helpers;

use std::collections::HashMap;

use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use platform::agent::llm_validate::{
    TestResult, TestStatus, ValidationEvent, build_provider_extra_env, run_validation,
};

/// Helper: create an `llm_provider_configs` row directly in the DB.
/// Returns `(config_id, user_id)`.
async fn seed_provider_config(pool: &PgPool) -> (Uuid, Uuid) {
    // Get the admin user_id
    let admin_row: (Uuid,) = sqlx::query_as("SELECT id FROM users WHERE name = 'admin'")
        .fetch_one(pool)
        .await
        .expect("admin user must exist");
    let user_id = admin_row.0;

    // Encrypt a minimal config blob
    let master_key = platform::secrets::engine::parse_master_key(
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .unwrap();

    let env_vars: HashMap<String, String> = HashMap::from([
        ("AWS_ACCESS_KEY_ID".into(), "AKIATEST123456".into()),
        ("AWS_SECRET_ACCESS_KEY".into(), "testsecretkey12345".into()),
    ]);

    let config_id = platform::secrets::llm_providers::create_config(
        pool,
        &master_key,
        user_id,
        "bedrock",
        "Test Bedrock",
        &env_vars,
        Some("claude-sonnet-4-20250514"),
    )
    .await
    .expect("create llm provider config");

    (config_id, user_id)
}

/// Collect all validation events from a channel into a Vec.
async fn collect_events(mut rx: mpsc::Receiver<ValidationEvent>) -> Vec<ValidationEvent> {
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    events
}

/// Extract non-running test results from events.
fn test_results(events: &[ValidationEvent]) -> Vec<&TestResult> {
    events
        .iter()
        .filter_map(|e| {
            if let ValidationEvent::Test(t) = e {
                if !matches!(t.status, TestStatus::Running) {
                    return Some(t);
                }
            }
            None
        })
        .collect()
}

/// Extract running test events.
fn running_events(events: &[ValidationEvent]) -> Vec<&TestResult> {
    events
        .iter()
        .filter_map(|e| {
            if let ValidationEvent::Test(t) = e {
                if matches!(t.status, TestStatus::Running) {
                    return Some(t);
                }
            }
            None
        })
        .collect()
}

// ---------------------------------------------------------------------------
// run_validation — mock CLI returns "Hello!" with empty tools
// ---------------------------------------------------------------------------

/// run_validation with mock CLI: test 1 (connection) passes, tests 2+3 fail.
/// Mock CLI returns "Hello!" (non-empty) for connection but empty tools and
/// no `answer` field, causing output_format and session_memory to fail.
#[sqlx::test(migrations = "./migrations")]
async fn run_validation_mock_cli_partial_pass(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state_with_cli(pool.clone(), true).await;
    let _ = state; // ensure state is built (sets CLAUDE_CLI_PATH)

    let (config_id, user_id) = seed_provider_config(&pool).await;

    let (api_key, extra_env) = build_provider_extra_env(
        "bedrock",
        &HashMap::from([
            ("AWS_ACCESS_KEY_ID".into(), "AKIATEST".into()),
            ("AWS_SECRET_ACCESS_KEY".into(), "secret".into()),
        ]),
    );

    let (tx, rx) = mpsc::channel(32);
    let cancel = CancellationToken::new();

    run_validation(
        &pool,
        config_id,
        user_id,
        api_key,
        extra_env,
        Some("claude-sonnet-4-20250514".into()),
        tx,
        cancel,
    )
    .await;

    let events = collect_events(rx).await;

    // Should have: 3x running + 3x result + 1x done = 7 events
    assert!(
        events.len() >= 7,
        "expected at least 7 events, got {}",
        events.len()
    );

    // Check we got a Done event
    let done = events
        .iter()
        .find(|e| matches!(e, ValidationEvent::Done { .. }));
    assert!(done.is_some(), "should have a Done event");

    let results = test_results(&events);
    assert_eq!(results.len(), 3, "should have 3 test results");

    // Test 1 (connection) should pass — mock returns "Hello!" which is non-empty
    assert_eq!(results[0].test, 1);
    assert_eq!(results[0].name, "connection");
    assert!(
        matches!(results[0].status, TestStatus::Passed),
        "test 1 (connection) should pass, got: {:?} detail: {}",
        results[0].status,
        results[0].detail,
    );

    // Test 2 (output_format) should fail — mock returns empty tools
    assert_eq!(results[1].test, 2);
    assert_eq!(results[1].name, "output_format");
    assert!(
        matches!(results[1].status, TestStatus::Failed),
        "test 2 (output_format) should fail, got: {:?} detail: {}",
        results[1].status,
        results[1].detail,
    );

    // Test 3 (session_memory) should fail — answer field missing
    assert_eq!(results[2].test, 3);
    assert_eq!(results[2].name, "session_memory");
    assert!(
        matches!(results[2].status, TestStatus::Failed),
        "test 3 (session_memory) should fail, got: {:?} detail: {}",
        results[2].status,
        results[2].detail,
    );

    // Not all passed
    if let Some(ValidationEvent::Done { all_passed }) = done {
        assert!(!all_passed, "not all tests should pass with mock CLI");
    }

    // DB should be updated to 'invalid'
    let row: (String,) =
        sqlx::query_as("SELECT validation_status FROM llm_provider_configs WHERE id = $1")
            .bind(config_id)
            .fetch_one(&pool)
            .await
            .expect("config should exist");
    assert_eq!(row.0, "invalid", "validation_status should be 'invalid'");
}

/// run_validation with immediate cancellation — no tests execute.
#[sqlx::test(migrations = "./migrations")]
async fn run_validation_cancelled_immediately(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state_with_cli(pool.clone(), true).await;
    let _ = state;

    let (config_id, user_id) = seed_provider_config(&pool).await;

    let (tx, rx) = mpsc::channel(32);
    let cancel = CancellationToken::new();

    // Cancel before running
    cancel.cancel();

    run_validation(&pool, config_id, user_id, None, vec![], None, tx, cancel).await;

    let events = collect_events(rx).await;

    // No test events should be produced (cancelled before any test ran)
    let test_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, ValidationEvent::Test(_)))
        .collect();
    assert_eq!(
        test_events.len(),
        0,
        "no tests should run when cancelled immediately"
    );

    // Done event should still be sent
    let done = events
        .iter()
        .find(|e| matches!(e, ValidationEvent::Done { .. }));
    assert!(
        done.is_some(),
        "Done event should still be sent after cancellation"
    );
}

/// run_validation sends a Running event before each test result.
#[sqlx::test(migrations = "./migrations")]
async fn run_validation_sends_running_events(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state_with_cli(pool.clone(), true).await;
    let _ = state;

    let (config_id, user_id) = seed_provider_config(&pool).await;

    let (api_key, extra_env) = build_provider_extra_env(
        "bedrock",
        &HashMap::from([
            ("AWS_ACCESS_KEY_ID".into(), "AKIATEST".into()),
            ("AWS_SECRET_ACCESS_KEY".into(), "secret".into()),
        ]),
    );

    let (tx, rx) = mpsc::channel(32);
    let cancel = CancellationToken::new();

    run_validation(
        &pool, config_id, user_id, api_key, extra_env, None, tx, cancel,
    )
    .await;

    let events = collect_events(rx).await;
    let running = running_events(&events);

    assert_eq!(
        running.len(),
        3,
        "should have 3 running events (one per test)"
    );

    // Verify running events for tests 1, 2, 3 with correct names
    assert_eq!(running[0].test, 1);
    assert_eq!(running[0].name, "connection");
    assert!(running[0].detail.is_empty());

    assert_eq!(running[1].test, 2);
    assert_eq!(running[1].name, "output_format");

    assert_eq!(running[2].test, 3);
    assert_eq!(running[2].name, "session_memory");
}

/// run_validation updates DB status to 'invalid' and sets last_validated_at.
#[sqlx::test(migrations = "./migrations")]
async fn run_validation_updates_db_status(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state_with_cli(pool.clone(), true).await;
    let _ = state;

    let (config_id, user_id) = seed_provider_config(&pool).await;

    // Verify initial status is 'untested'
    let row: (String,) =
        sqlx::query_as("SELECT validation_status FROM llm_provider_configs WHERE id = $1")
            .bind(config_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row.0, "untested");

    let (tx, rx) = mpsc::channel(32);
    let cancel = CancellationToken::new();

    run_validation(&pool, config_id, user_id, None, vec![], None, tx, cancel).await;

    // Drain events
    let _events = collect_events(rx).await;

    // Check DB was updated to 'invalid'
    let row: (String,) =
        sqlx::query_as("SELECT validation_status FROM llm_provider_configs WHERE id = $1")
            .bind(config_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        row.0, "invalid",
        "should update to 'invalid' when tests fail"
    );

    // last_validated_at should be set
    let row: (Option<chrono::DateTime<chrono::Utc>>,) =
        sqlx::query_as("SELECT last_validated_at FROM llm_provider_configs WHERE id = $1")
            .bind(config_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(row.0.is_some(), "last_validated_at should be set");
}

/// run_validation handles dropped receiver gracefully (client disconnect).
/// The function returns early without panicking when tx.send() fails.
#[sqlx::test(migrations = "./migrations")]
async fn run_validation_receiver_dropped(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state_with_cli(pool.clone(), true).await;
    let _ = state;

    let (config_id, user_id) = seed_provider_config(&pool).await;

    let (tx, rx) = mpsc::channel(1); // Small buffer
    drop(rx); // Simulate client disconnect

    // Should return early without panicking
    run_validation(
        &pool,
        config_id,
        user_id,
        None,
        vec![],
        None,
        tx,
        CancellationToken::new(),
    )
    .await;
    // If we reach here, test passes
}

/// Event ordering: events arrive as running1, result1, running2, result2, running3, result3, done.
#[sqlx::test(migrations = "./migrations")]
async fn run_validation_event_ordering(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state_with_cli(pool.clone(), true).await;
    let _ = state;

    let (config_id, user_id) = seed_provider_config(&pool).await;

    let (api_key, extra_env) = build_provider_extra_env(
        "bedrock",
        &HashMap::from([
            ("AWS_ACCESS_KEY_ID".into(), "AKIATEST".into()),
            ("AWS_SECRET_ACCESS_KEY".into(), "secret".into()),
        ]),
    );

    let (tx, rx) = mpsc::channel(32);
    let cancel = CancellationToken::new();

    run_validation(
        &pool,
        config_id,
        user_id,
        api_key,
        extra_env,
        Some("claude-sonnet-4-20250514".into()),
        tx,
        cancel,
    )
    .await;

    let events = collect_events(rx).await;

    // Verify interleaving: running, result, running, result, running, result, done
    let mut test_num = 0u8;
    let mut expect_running = true;

    for event in &events {
        match event {
            ValidationEvent::Test(t) => {
                if expect_running {
                    test_num += 1;
                    assert!(
                        matches!(t.status, TestStatus::Running),
                        "expected Running for test {}, got {:?}",
                        test_num,
                        t.status
                    );
                    assert_eq!(t.test, test_num);
                    expect_running = false;
                } else {
                    assert!(
                        !matches!(t.status, TestStatus::Running),
                        "expected result for test {}, got Running",
                        test_num
                    );
                    assert_eq!(t.test, test_num);
                    expect_running = true;
                }
            }
            ValidationEvent::Done { .. } => {
                assert_eq!(test_num, 3, "Done should come after all 3 tests");
            }
        }
    }
}

/// Verify test result detail strings contain meaningful messages.
#[sqlx::test(migrations = "./migrations")]
async fn run_validation_result_details(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state_with_cli(pool.clone(), true).await;
    let _ = state;

    let (config_id, user_id) = seed_provider_config(&pool).await;

    let (api_key, extra_env) = build_provider_extra_env(
        "bedrock",
        &HashMap::from([
            ("AWS_ACCESS_KEY_ID".into(), "AKIATEST".into()),
            ("AWS_SECRET_ACCESS_KEY".into(), "secret".into()),
        ]),
    );

    let (tx, rx) = mpsc::channel(32);
    let cancel = CancellationToken::new();

    run_validation(
        &pool, config_id, user_id, api_key, extra_env, None, tx, cancel,
    )
    .await;

    let events = collect_events(rx).await;
    let results = test_results(&events);

    // Test 1 passes — detail should mention endpoint/response
    assert!(
        !results[0].detail.is_empty(),
        "passed test should have detail text"
    );

    // Test 2 fails — detail should mention tools or structured output
    assert!(
        !results[1].detail.is_empty(),
        "failed test should have detail text"
    );

    // Test 3 fails — detail should mention turn or answer
    assert!(
        !results[2].detail.is_empty(),
        "failed test should have detail text"
    );
}

/// run_validation with no api_key and empty extra_env still runs all tests.
/// The mock CLI does not require real credentials.
#[sqlx::test(migrations = "./migrations")]
async fn run_validation_no_credentials(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state_with_cli(pool.clone(), true).await;
    let _ = state;

    let (config_id, user_id) = seed_provider_config(&pool).await;

    let (tx, rx) = mpsc::channel(32);
    let cancel = CancellationToken::new();

    // No api_key, no extra_env, no model
    run_validation(&pool, config_id, user_id, None, vec![], None, tx, cancel).await;

    let events = collect_events(rx).await;
    let results = test_results(&events);

    // All 3 tests should have run (mock CLI doesn't need real creds)
    assert_eq!(results.len(), 3, "all 3 tests should run");
}

/// run_validation with a model override passes it to CLI opts.
#[sqlx::test(migrations = "./migrations")]
async fn run_validation_with_model(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state_with_cli(pool.clone(), true).await;
    let _ = state;

    let (config_id, user_id) = seed_provider_config(&pool).await;

    let (tx, rx) = mpsc::channel(32);
    let cancel = CancellationToken::new();

    run_validation(
        &pool,
        config_id,
        user_id,
        None,
        vec![],
        Some("claude-opus-4-20250514".into()),
        tx,
        cancel,
    )
    .await;

    let events = collect_events(rx).await;
    let results = test_results(&events);

    // Regardless of model, all 3 tests should produce results
    assert_eq!(results.len(), 3);

    // Done event present
    let done = events
        .iter()
        .any(|e| matches!(e, ValidationEvent::Done { .. }));
    assert!(done);
}

// ---------------------------------------------------------------------------
// API endpoint smoke tests
// ---------------------------------------------------------------------------

/// GET /api/users/me/llm-providers/{id}/validate returns SSE response.
#[sqlx::test(migrations = "./migrations")]
async fn validate_provider_api_returns_sse(pool: PgPool) {
    let (state, admin_token) = helpers::test_state_with_cli(pool.clone(), true).await;
    let app = helpers::test_router(state);

    let (config_id, _user_id) = seed_provider_config(&pool).await;

    // SSE endpoint — use raw request (helpers::get_json expects JSON body)
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(&format!("/api/users/me/llm-providers/{config_id}/validate"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .header("Accept", "text/event-stream")
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
    assert_eq!(
        resp.status(),
        axum::http::StatusCode::OK,
        "SSE endpoint should return 200"
    );

    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.contains("text/event-stream"),
        "expected text/event-stream, got: {ct}"
    );
}

/// GET validate with non-existent config returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn validate_provider_not_found(pool: PgPool) {
    let (state, admin_token) = helpers::test_state_with_cli(pool.clone(), true).await;
    let app = helpers::test_router(state);

    let fake_id = Uuid::new_v4();
    let (status, _body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/users/me/llm-providers/{fake_id}/validate"),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
}

/// GET validate without auth returns 401.
#[sqlx::test(migrations = "./migrations")]
async fn validate_provider_unauthorized(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state_with_cli(pool.clone(), true).await;
    let app = helpers::test_router(state);

    let (config_id, _user_id) = seed_provider_config(&pool).await;

    let (status, _body) = helpers::get_json(
        &app,
        "",
        &format!("/api/users/me/llm-providers/{config_id}/validate"),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED);
}

/// Validate another user's config returns 404 (security: no existence leakage).
#[sqlx::test(migrations = "./migrations")]
async fn validate_provider_wrong_user(pool: PgPool) {
    let (state, admin_token) = helpers::test_state_with_cli(pool.clone(), true).await;
    let app = helpers::test_router(state);

    let (config_id, _user_id) = seed_provider_config(&pool).await;

    // Create a different user
    let (_other_id, other_token) =
        helpers::create_user(&app, &admin_token, "otherval", "otherval@test.com").await;

    // Other user tries to validate admin's config
    let (status, _body) = helpers::get_json(
        &app,
        &other_token,
        &format!("/api/users/me/llm-providers/{config_id}/validate"),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::NOT_FOUND,
        "should return 404 for another user's config"
    );
}
