//! Integration tests for Valkey pub/sub + ACL in the agent session pipeline.
//!
//! Validates the pub/sub pipeline WITHOUT K8s pods by simulating what
//! agent-runner does: creating a `fred::Client` with scoped ACL credentials
//! and publishing/subscribing directly.

mod helpers;

use fred::interfaces::{ClientLike, EventInterface, PubsubInterface};
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

use platform::agent::provider::{ProgressEvent, ProgressKind};
use platform::agent::{pubsub_bridge, valkey_acl};

/// Insert an `agent_sessions` row directly (needed for FK on `agent_messages`).
async fn insert_session(pool: &PgPool, project_id: Uuid, user_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, provider, execution_mode, uses_pubsub)
         VALUES ($1, $2, $3, 'test prompt', 'running', 'claude-code', 'pod', true)",
    )
    .bind(id)
    .bind(project_id)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("insert session");
    id
}

/// Create a fred Client using scoped ACL credentials (simulates agent-runner).
///
/// `valkey_url` is the test-accessible Valkey URL (e.g. `redis://127.0.0.1:55438`).
/// We extract the host:port from it and combine with the ACL username/password,
/// because `creds.url` uses `valkey_agent_host` which is for in-cluster pods
/// (e.g. `host.docker.internal:6379`) and is not resolvable from the test host.
async fn create_scoped_client(
    creds: &valkey_acl::SessionValkeyCredentials,
    valkey_url: &str,
) -> fred::clients::Client {
    // Extract host:port from the admin Valkey URL (e.g. "redis://127.0.0.1:55438")
    let host_port = valkey_url.strip_prefix("redis://").unwrap_or(valkey_url);
    let scoped_url = format!(
        "redis://{}:{}@{}",
        creds.username, creds.password, host_port
    );
    let config = fred::types::config::Config::from_url(&scoped_url).expect("parse url");
    let client = fred::clients::Client::new(config, None, None, None);
    client.init().await.expect("init scoped client");
    client
}

// ---------------------------------------------------------------------------
// ACL tests
// ---------------------------------------------------------------------------

/// Scoped client can publish to its session's events channel; admin subscriber receives.
#[sqlx::test(migrations = "./migrations")]
async fn acl_scoped_publish_subscribe(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let session_id = Uuid::new_v4();

    let creds =
        valkey_acl::create_session_acl(&state.valkey, session_id, &state.config.valkey_agent_host)
            .await
            .expect("create ACL");

    assert!(creds.username.contains(&session_id.to_string()));
    assert_eq!(creds.password.len(), 64);

    // Platform admin subscribes to events channel
    let events_ch = valkey_acl::events_channel(session_id);
    let subscriber = state.valkey.next().clone_new();
    subscriber.init().await.expect("init subscriber");
    subscriber.subscribe(&events_ch).await.expect("subscribe");
    let mut rx = subscriber.message_rx();

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Scoped client publishes (agent-runner → platform)
    let scoped = create_scoped_client(&creds, &state.config.valkey_url).await;
    let event = ProgressEvent {
        kind: ProgressKind::Text,
        message: "hello from agent".into(),
        metadata: None,
    };
    let json = serde_json::to_string(&event).unwrap();
    scoped
        .publish::<(), _, _>(&events_ch, &json)
        .await
        .expect("scoped publish");

    // Platform receives the event
    let msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timeout")
        .expect("recv");
    let payload: String = msg.value.convert().unwrap();
    let received: ProgressEvent = serde_json::from_str(&payload).unwrap();
    assert_eq!(received.kind, ProgressKind::Text);
    assert_eq!(received.message, "hello from agent");

    let _ = subscriber.unsubscribe(&events_ch).await;
    valkey_acl::delete_session_acl(&state.valkey, session_id)
        .await
        .expect("cleanup");
}

/// Scoped client subscribes to input channel; platform sends prompt.
#[sqlx::test(migrations = "./migrations")]
async fn acl_scoped_subscribe_input(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let session_id = Uuid::new_v4();

    let creds =
        valkey_acl::create_session_acl(&state.valkey, session_id, &state.config.valkey_agent_host)
            .await
            .expect("create ACL");

    let scoped = create_scoped_client(&creds, &state.config.valkey_url).await;

    // Scoped client subscribes to input channel (agent-runner listens for prompts)
    let input_ch = valkey_acl::input_channel(session_id);
    let scoped_sub = scoped.clone_new();
    scoped_sub.init().await.expect("init scoped subscriber");
    scoped_sub.subscribe(&input_ch).await.expect("subscribe");
    let mut rx = scoped_sub.message_rx();

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Platform publishes prompt via admin connection
    pubsub_bridge::publish_prompt(&state.valkey, session_id, "fix the bug")
        .await
        .expect("publish prompt");

    // Scoped client receives it
    let msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timeout")
        .expect("recv");
    let payload: String = msg.value.convert().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(parsed["type"], "prompt");
    assert_eq!(parsed["content"], "fix the bug");

    let _ = scoped_sub.unsubscribe(&input_ch).await;
    valkey_acl::delete_session_acl(&state.valkey, session_id)
        .await
        .expect("cleanup");
}

/// Scoped client for session A cannot publish to session B's channels.
///
/// Valkey ACL blocks publish with an error. Subscribe silently ignores
/// unauthorized channels (no error, but no messages delivered), so we
/// only test the publish boundary here.
#[sqlx::test(migrations = "./migrations")]
async fn acl_isolation_blocks_cross_session(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let session_a = Uuid::new_v4();
    let session_b = Uuid::new_v4();

    let creds_a =
        valkey_acl::create_session_acl(&state.valkey, session_a, &state.config.valkey_agent_host)
            .await
            .expect("create ACL A");

    let scoped_a = create_scoped_client(&creds_a, &state.config.valkey_url).await;

    // Publish to own session's channel → should succeed
    let own_ch = valkey_acl::events_channel(session_a);
    scoped_a
        .publish::<(), _, _>(&own_ch, "allowed")
        .await
        .expect("publish to own channel should succeed");

    // Attempt to publish to session B's events channel → should fail
    let other_ch = valkey_acl::events_channel(session_b);
    let result = scoped_a.publish::<(), _, _>(&other_ch, "intruder").await;
    assert!(
        result.is_err(),
        "scoped client should not publish to another session's channel"
    );

    valkey_acl::delete_session_acl(&state.valkey, session_a)
        .await
        .expect("cleanup");
}

/// Delete ACL → new connection with same credentials fails.
#[sqlx::test(migrations = "./migrations")]
async fn acl_cleanup_revokes_access(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let session_id = Uuid::new_v4();

    let creds =
        valkey_acl::create_session_acl(&state.valkey, session_id, &state.config.valkey_agent_host)
            .await
            .expect("create ACL");

    // Verify scoped client works before deletion
    let scoped = create_scoped_client(&creds, &state.config.valkey_url).await;
    let events_ch = valkey_acl::events_channel(session_id);
    scoped
        .publish::<(), _, _>(&events_ch, "works")
        .await
        .expect("should work before deletion");

    // Delete ACL
    valkey_acl::delete_session_acl(&state.valkey, session_id)
        .await
        .expect("delete ACL");

    // New connection with the same credentials should fail
    let host_port = state
        .config
        .valkey_url
        .strip_prefix("redis://")
        .unwrap_or(&state.config.valkey_url);
    let scoped_url = format!(
        "redis://{}:{}@{}",
        creds.username, creds.password, host_port
    );
    let config = fred::types::config::Config::from_url(&scoped_url).expect("parse url");
    let new_client = fred::clients::Client::new(config, None, None, None);
    let result = new_client.init().await;
    assert!(result.is_err(), "connection should fail after ACL deletion");
}

/// Deleting a non-existent ACL succeeds; double-delete succeeds.
#[sqlx::test(migrations = "./migrations")]
async fn acl_delete_idempotent(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let session_id = Uuid::new_v4();

    // Delete non-existent — should succeed
    valkey_acl::delete_session_acl(&state.valkey, session_id)
        .await
        .expect("delete non-existent");

    // Create then delete twice
    valkey_acl::create_session_acl(&state.valkey, session_id, &state.config.valkey_agent_host)
        .await
        .expect("create ACL");

    valkey_acl::delete_session_acl(&state.valkey, session_id)
        .await
        .expect("delete once");
    valkey_acl::delete_session_acl(&state.valkey, session_id)
        .await
        .expect("delete twice");
}

// ---------------------------------------------------------------------------
// Pub/sub bridge tests
// ---------------------------------------------------------------------------

/// `subscribe_session_events` receives events and closes on terminal event.
#[sqlx::test(migrations = "./migrations")]
async fn subscribe_session_events_receives_events(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let session_id = Uuid::new_v4();

    let mut rx = pubsub_bridge::subscribe_session_events(&state.valkey, session_id)
        .await
        .expect("subscribe");

    tokio::time::sleep(Duration::from_millis(50)).await;

    let events = vec![
        ProgressEvent {
            kind: ProgressKind::Thinking,
            message: "hmm".into(),
            metadata: None,
        },
        ProgressEvent {
            kind: ProgressKind::ToolCall,
            message: "Read".into(),
            metadata: None,
        },
        ProgressEvent {
            kind: ProgressKind::Text,
            message: "result".into(),
            metadata: None,
        },
        ProgressEvent {
            kind: ProgressKind::Completed,
            message: "done".into(),
            metadata: None,
        },
    ];

    for event in &events {
        pubsub_bridge::publish_event(&state.valkey, session_id, event)
            .await
            .expect("publish");
    }

    // Receive all 4 events
    for expected in &events {
        let received = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("timeout waiting for event")
            .expect("channel closed");
        assert_eq!(received.kind, expected.kind);
        assert_eq!(received.message, expected.message);
    }

    // After Completed, the background task exits and channel closes
    let next = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;
    assert!(
        next.is_err() || next.unwrap().is_none(),
        "channel should close after terminal event"
    );
}

/// `subscribe_session_events` exits on Error event too.
#[sqlx::test(migrations = "./migrations")]
async fn subscribe_session_events_exits_on_error(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let session_id = Uuid::new_v4();

    let mut rx = pubsub_bridge::subscribe_session_events(&state.valkey, session_id)
        .await
        .expect("subscribe");

    tokio::time::sleep(Duration::from_millis(50)).await;

    let event = ProgressEvent {
        kind: ProgressKind::Error,
        message: "fatal error".into(),
        metadata: None,
    };
    pubsub_bridge::publish_event(&state.valkey, session_id, &event)
        .await
        .expect("publish");

    let received = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timeout")
        .expect("channel closed");
    assert_eq!(received.kind, ProgressKind::Error);

    let next = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;
    assert!(
        next.is_err() || next.unwrap().is_none(),
        "channel should close after Error event"
    );
}

/// Persistence subscriber writes events to `agent_messages` and exits on Completed.
#[sqlx::test(migrations = "./migrations")]
async fn persistence_subscriber_persists_and_exits(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());

    // Create project + session (needed for FK on agent_messages)
    let project_id = helpers::create_project(&app, &admin_token, "pubsub-persist", "public").await;
    let admin_id: (Uuid,) = sqlx::query_as("SELECT id FROM users WHERE name = 'admin'")
        .fetch_one(&pool)
        .await
        .expect("admin user");
    let session_id = insert_session(&pool, project_id, admin_id.0).await;

    // Spawn persistence subscriber (blocks until subscription established)
    let handle =
        pubsub_bridge::spawn_persistence_subscriber(pool.clone(), state.valkey.clone(), session_id)
            .await;

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Publish a Text event then a Completed event
    let text_event = ProgressEvent {
        kind: ProgressKind::Text,
        message: "hello world".into(),
        metadata: Some(serde_json::json!({"file": "test.rs"})),
    };
    pubsub_bridge::publish_event(&state.valkey, session_id, &text_event)
        .await
        .expect("publish text");

    let completed_event = ProgressEvent {
        kind: ProgressKind::Completed,
        message: "done".into(),
        metadata: None,
    };
    pubsub_bridge::publish_event(&state.valkey, session_id, &completed_event)
        .await
        .expect("publish completed");

    // Subscriber should exit after Completed
    let result = tokio::time::timeout(Duration::from_secs(5), handle).await;
    assert!(
        result.is_ok(),
        "persistence subscriber should exit on Completed"
    );

    // Verify agent_messages rows were inserted
    let rows: Vec<(String, String, Option<serde_json::Value>)> = sqlx::query_as(
        "SELECT role, content, metadata FROM agent_messages WHERE session_id = $1 ORDER BY created_at",
    )
    .bind(session_id)
    .fetch_all(&pool)
    .await
    .expect("fetch messages");

    assert_eq!(rows.len(), 2, "should have 2 messages (text + completed)");
    assert_eq!(rows[0].0, "text");
    assert_eq!(rows[0].1, "hello world");
    assert!(rows[0].2.is_some(), "text event should have metadata");
    assert_eq!(rows[1].0, "completed");
    assert_eq!(rows[1].1, "done");
}

/// Persistence subscriber exits on Error event.
#[sqlx::test(migrations = "./migrations")]
async fn persistence_subscriber_exits_on_error(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state.clone());

    let project_id = helpers::create_project(&app, &admin_token, "pubsub-err", "public").await;
    let admin_id: (Uuid,) = sqlx::query_as("SELECT id FROM users WHERE name = 'admin'")
        .fetch_one(&pool)
        .await
        .expect("admin user");
    let session_id = insert_session(&pool, project_id, admin_id.0).await;

    let handle =
        pubsub_bridge::spawn_persistence_subscriber(pool.clone(), state.valkey.clone(), session_id)
            .await;

    tokio::time::sleep(Duration::from_millis(50)).await;

    let event = ProgressEvent {
        kind: ProgressKind::Error,
        message: "something went wrong".into(),
        metadata: None,
    };
    pubsub_bridge::publish_event(&state.valkey, session_id, &event)
        .await
        .expect("publish");

    let result = tokio::time::timeout(Duration::from_secs(5), handle).await;
    assert!(
        result.is_ok(),
        "persistence subscriber should exit on Error"
    );

    // Verify the error message was persisted
    let count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM agent_messages WHERE session_id = $1 AND role = 'error'",
    )
    .bind(session_id)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(count.0, 1, "error event should be persisted");
}

/// `publish_control` reaches scoped subscriber on input channel.
#[sqlx::test(migrations = "./migrations")]
async fn publish_control_reaches_input_channel(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let session_id = Uuid::new_v4();

    let creds =
        valkey_acl::create_session_acl(&state.valkey, session_id, &state.config.valkey_agent_host)
            .await
            .expect("create ACL");

    let scoped = create_scoped_client(&creds, &state.config.valkey_url).await;
    let input_ch = valkey_acl::input_channel(session_id);
    let scoped_sub = scoped.clone_new();
    scoped_sub.init().await.expect("init scoped subscriber");
    scoped_sub.subscribe(&input_ch).await.expect("subscribe");
    let mut rx = scoped_sub.message_rx();

    tokio::time::sleep(Duration::from_millis(50)).await;

    pubsub_bridge::publish_control(&state.valkey, session_id, "interrupt")
        .await
        .expect("publish control");

    let msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timeout")
        .expect("recv");
    let payload: String = msg.value.convert().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(parsed["type"], "control");
    assert_eq!(parsed["control_type"], "interrupt");

    let _ = scoped_sub.unsubscribe(&input_ch).await;
    valkey_acl::delete_session_acl(&state.valkey, session_id)
        .await
        .expect("cleanup");
}
