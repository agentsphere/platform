// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Integration tests for the health loop (`platform_operator::health`).
//!
//! Requires Postgres, Valkey, `MinIO`, and K8s (Kind cluster).

mod helpers;

use std::sync::Arc;

use chrono::Utc;
use fred::interfaces::{ClientLike, EventInterface, PubsubInterface};
use sqlx::PgPool;
use uuid::Uuid;

use platform_operator::health::{SubsystemCheck, SubsystemStatus};

// ---------------------------------------------------------------------------
// 1. health_run_populates_snapshot_with_all_subsystems
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../../../migrations")]
async fn health_run_populates_snapshot_with_all_subsystems(pool: PgPool) {
    let mut state = helpers::operator_state(pool).await;

    // Override health check interval to 1 second for fast tick
    let mut config = (*state.config).clone();
    config.health_check_interval_secs = 1;
    state.config = Arc::new(config);

    let cancel = tokio_util::sync::CancellationToken::new();
    let run_state = state.clone();
    let cancel_clone = cancel.clone();
    let handle = tokio::spawn(async move {
        platform_operator::health::run(run_state, cancel_clone).await;
    });

    // Wait for at least one tick to complete
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // Read the snapshot
    let snapshot = state.health.read().unwrap().clone();

    // Verify all 7 subsystems are present
    let names: Vec<&str> = snapshot
        .subsystems
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(names.contains(&"postgres"), "missing postgres in {names:?}");
    assert!(names.contains(&"valkey"), "missing valkey in {names:?}");
    assert!(names.contains(&"minio"), "missing minio in {names:?}");
    assert!(
        names.contains(&"kubernetes"),
        "missing kubernetes in {names:?}"
    );
    assert!(
        names.contains(&"git_repos"),
        "missing git_repos in {names:?}"
    );
    assert!(names.contains(&"secrets"), "missing secrets in {names:?}");
    assert!(names.contains(&"registry"), "missing registry in {names:?}");
    assert_eq!(snapshot.subsystems.len(), 7);

    // Postgres and Valkey should be healthy (real infra running)
    let pg = snapshot
        .subsystems
        .iter()
        .find(|s| s.name == "postgres")
        .unwrap();
    assert_eq!(pg.status, SubsystemStatus::Healthy);

    let vk = snapshot
        .subsystems
        .iter()
        .find(|s| s.name == "valkey")
        .unwrap();
    assert_eq!(vk.status, SubsystemStatus::Healthy);

    // MinIO and K8s should be healthy too (real infra from Kind cluster)
    let minio = snapshot
        .subsystems
        .iter()
        .find(|s| s.name == "minio")
        .unwrap();
    assert_eq!(minio.status, SubsystemStatus::Healthy);

    let k8s = snapshot
        .subsystems
        .iter()
        .find(|s| s.name == "kubernetes")
        .unwrap();
    assert_eq!(k8s.status, SubsystemStatus::Healthy);

    // Overall should not be Unknown (it was the default before run)
    assert_ne!(snapshot.overall, SubsystemStatus::Unknown);

    // Uptime should be > 0
    assert!(snapshot.uptime_seconds > 0);

    // checked_at should be recent
    let age = Utc::now() - snapshot.checked_at;
    assert!(age.num_seconds() < 10, "snapshot too old: {age}");

    // Shutdown the loop
    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

// ---------------------------------------------------------------------------
// 2. health_run_shutdown_signal_stops_loop
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../../../migrations")]
async fn health_run_shutdown_signal_stops_loop(pool: PgPool) {
    let mut state = helpers::operator_state(pool).await;

    // Use a short interval
    let mut config = (*state.config).clone();
    config.health_check_interval_secs = 1;
    state.config = Arc::new(config);

    let cancel = tokio_util::sync::CancellationToken::new();

    let run_state = state.clone();
    let cancel_clone = cancel.clone();
    let handle = tokio::spawn(async move {
        platform_operator::health::run(run_state, cancel_clone).await;
    });

    // Immediately send shutdown
    cancel.cancel();

    // The task should complete without panic within a reasonable time
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    assert!(result.is_ok(), "health loop did not shut down in time");
    assert!(
        result.unwrap().is_ok(),
        "health loop task panicked on shutdown"
    );
}

// ---------------------------------------------------------------------------
// 3. health_is_ready_falls_back_to_live_probes
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../../../migrations")]
async fn health_is_ready_falls_back_to_live_probes(pool: PgPool) {
    let state = helpers::operator_state(pool).await;

    // Set the snapshot's checked_at to 120s ago (stale — threshold is 60s)
    {
        let mut snap = state.health.write().unwrap();
        snap.checked_at = Utc::now() - chrono::Duration::seconds(120);
        // Leave subsystems empty — the snapshot is stale so is_ready should fallback
    }

    // is_ready should run live probes (PG + Valkey are real and healthy)
    let ready = platform_operator::health::is_ready(&state).await;
    assert!(ready, "is_ready should return true via live probes");
}

// ---------------------------------------------------------------------------
// 4. health_is_ready_returns_false_when_unhealthy
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../../../migrations")]
async fn health_is_ready_returns_false_when_unhealthy(pool: PgPool) {
    let state = helpers::operator_state(pool).await;

    // Inject a recent snapshot where valkey is unhealthy
    {
        let mut snap = state.health.write().unwrap();
        snap.checked_at = Utc::now(); // recent — will use cached
        snap.subsystems = vec![
            SubsystemCheck {
                name: "postgres".into(),
                status: SubsystemStatus::Healthy,
                latency_ms: 5,
                message: None,
                checked_at: Utc::now(),
            },
            SubsystemCheck {
                name: "valkey".into(),
                status: SubsystemStatus::Unhealthy,
                latency_ms: 0,
                message: Some("connection refused".into()),
                checked_at: Utc::now(),
            },
        ];
    }

    let ready = platform_operator::health::is_ready(&state).await;
    assert!(
        !ready,
        "is_ready should return false when valkey is unhealthy"
    );
}

// ---------------------------------------------------------------------------
// 5. health_query_pod_failures_empty_db
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../../../migrations")]
async fn health_query_pod_failures_empty_db(pool: PgPool) {
    let mut state = helpers::operator_state(pool).await;

    // Override interval for fast tick
    let mut config = (*state.config).clone();
    config.health_check_interval_secs = 1;
    state.config = Arc::new(config);

    let cancel = tokio_util::sync::CancellationToken::new();
    let run_state = state.clone();
    let cancel_clone = cancel.clone();
    let handle = tokio::spawn(async move {
        platform_operator::health::run(run_state, cancel_clone).await;
    });

    // Wait for at least one tick
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let snapshot = state.health.read().unwrap().clone();

    // With no failed agent sessions or pipelines, pod failures should be zero
    assert_eq!(snapshot.pod_failures.total_failed_24h, 0);
    assert_eq!(snapshot.pod_failures.agent_failures, 0);
    assert_eq!(snapshot.pod_failures.pipeline_failures, 0);
    assert!(snapshot.pod_failures.recent_failures.is_empty());

    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

// ---------------------------------------------------------------------------
// 6. health_query_pod_failures_with_data
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../../../migrations")]
async fn health_query_pod_failures_with_data(pool: PgPool) {
    let mut state = helpers::operator_state(pool.clone()).await;

    // Create prerequisite data: user, workspace, project
    let admin_id: (Uuid,) = sqlx::query_as("SELECT id FROM users WHERE name = 'admin'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let user_id = admin_id.0;

    let ws_id = Uuid::new_v4();
    sqlx::query("INSERT INTO workspaces (id, name, owner_id) VALUES ($1, $2, $3)")
        .bind(ws_id)
        .bind(format!("ws-health-{}", Uuid::new_v4()))
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();

    let project_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO projects (id, name, owner_id, visibility, repo_path, workspace_id, namespace_slug) \
         VALUES ($1, $2, $3, 'private', '/tmp/health-test', $4, $5)",
    )
    .bind(project_id)
    .bind(format!("health-proj-{}", Uuid::new_v4()))
    .bind(user_id)
    .bind(ws_id)
    .bind(format!("hproj-{}", &Uuid::new_v4().to_string()[..8]))
    .execute(&pool)
    .await
    .unwrap();

    // Insert a failed agent session (finished recently)
    sqlx::query(
        "INSERT INTO agent_sessions (id, project_id, user_id, prompt, status, pod_name, finished_at) \
         VALUES ($1, $2, $3, 'test prompt', 'failed', 'agent-pod-1', now() - interval '1 hour')",
    )
    .bind(Uuid::new_v4())
    .bind(project_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    // Insert a failed pipeline (finished recently)
    sqlx::query(
        "INSERT INTO pipelines (id, project_id, trigger, git_ref, status, triggered_by, finished_at) \
         VALUES ($1, $2, 'api', 'refs/heads/main', 'failure', $3, now() - interval '2 hours')",
    )
    .bind(Uuid::new_v4())
    .bind(project_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    // Override interval for fast tick
    let mut config = (*state.config).clone();
    config.health_check_interval_secs = 1;
    state.config = Arc::new(config);

    let cancel = tokio_util::sync::CancellationToken::new();
    let run_state = state.clone();
    let cancel_clone = cancel.clone();
    let handle = tokio::spawn(async move {
        platform_operator::health::run(run_state, cancel_clone).await;
    });

    // Wait for a tick
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let snapshot = state.health.read().unwrap().clone();

    // Should see at least 2 failures (1 agent + 1 pipeline)
    assert!(
        snapshot.pod_failures.total_failed_24h >= 2,
        "expected >= 2 total failures, got {}",
        snapshot.pod_failures.total_failed_24h
    );
    assert!(
        snapshot.pod_failures.agent_failures >= 1,
        "expected >= 1 agent failures, got {}",
        snapshot.pod_failures.agent_failures
    );
    assert!(
        snapshot.pod_failures.pipeline_failures >= 1,
        "expected >= 1 pipeline failures, got {}",
        snapshot.pod_failures.pipeline_failures
    );
    assert!(
        !snapshot.pod_failures.recent_failures.is_empty(),
        "recent_failures should be non-empty"
    );

    // Verify the recent failures have the right kinds
    let kinds: Vec<&str> = snapshot
        .pod_failures
        .recent_failures
        .iter()
        .map(|f| f.kind.as_str())
        .collect();
    assert!(
        kinds.contains(&"agent"),
        "recent failures should contain 'agent', got {kinds:?}"
    );
    assert!(
        kinds.contains(&"pipeline"),
        "recent failures should contain 'pipeline', got {kinds:?}"
    );

    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

// ---------------------------------------------------------------------------
// 7. health_check_minio_healthy
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../../../migrations")]
async fn health_check_minio_healthy(pool: PgPool) {
    let mut state = helpers::operator_state(pool).await;

    // Override interval for fast tick
    let mut config = (*state.config).clone();
    config.health_check_interval_secs = 1;
    state.config = Arc::new(config);

    let cancel = tokio_util::sync::CancellationToken::new();
    let run_state = state.clone();
    let cancel_clone = cancel.clone();
    let handle = tokio::spawn(async move {
        platform_operator::health::run(run_state, cancel_clone).await;
    });

    // Wait for a tick
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let snapshot = state.health.read().unwrap().clone();

    let minio = snapshot
        .subsystems
        .iter()
        .find(|s| s.name == "minio")
        .expect("minio subsystem should be present");
    assert_eq!(
        minio.status,
        SubsystemStatus::Healthy,
        "minio should be healthy, got {:?} with message {:?}",
        minio.status,
        minio.message
    );
    // Latency should be reasonable
    assert!(minio.latency_ms < 2000, "minio latency too high");

    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

// ---------------------------------------------------------------------------
// 8. health_run_publishes_to_valkey
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../../../migrations")]
async fn health_run_publishes_to_valkey(pool: PgPool) {
    let mut state = helpers::operator_state(pool).await;

    // Override interval for fast tick
    let mut config = (*state.config).clone();
    config.health_check_interval_secs = 1;
    state.config = Arc::new(config);

    // Create a dedicated subscriber client
    let subscriber = state.valkey.next().clone_new();
    subscriber.init().await.expect("subscriber init");
    subscriber
        .subscribe("health:stream")
        .await
        .expect("subscribe to health:stream");
    let mut msg_rx = subscriber.message_rx();

    // Start the health loop
    let cancel = tokio_util::sync::CancellationToken::new();
    let run_state = state.clone();
    let cancel_clone = cancel.clone();
    let handle = tokio::spawn(async move {
        platform_operator::health::run(run_state, cancel_clone).await;
    });

    // Wait for a message (with timeout)
    let msg = tokio::time::timeout(std::time::Duration::from_secs(10), msg_rx.recv())
        .await
        .expect("timed out waiting for health:stream message")
        .expect("message_rx recv failed");

    // Parse the message value as a string
    let payload: String = msg.value.convert().expect("convert to string");
    let json: serde_json::Value =
        serde_json::from_str(&payload).expect("health message should be valid JSON");

    // Verify it has the expected structure
    assert!(
        json["subsystems"].is_array(),
        "published JSON should contain 'subsystems' array"
    );
    assert!(
        json["overall"].is_string(),
        "published JSON should contain 'overall' string"
    );
    assert!(
        json["uptime_seconds"].is_number(),
        "published JSON should contain 'uptime_seconds' number"
    );
    assert!(
        json["checked_at"].is_string(),
        "published JSON should contain 'checked_at' string"
    );

    // Cleanup
    let _ = subscriber.unsubscribe("health:stream").await;
    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}

// ---------------------------------------------------------------------------
// 9. health_is_ready_with_recent_healthy_snapshot
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../../../migrations")]
async fn health_is_ready_with_recent_healthy_snapshot(pool: PgPool) {
    let state = helpers::operator_state(pool).await;

    // Inject a recent snapshot with postgres, valkey, and minio healthy
    {
        let mut snap = state.health.write().unwrap();
        snap.checked_at = Utc::now(); // recent
        snap.subsystems = vec![
            SubsystemCheck {
                name: "postgres".into(),
                status: SubsystemStatus::Healthy,
                latency_ms: 2,
                message: None,
                checked_at: Utc::now(),
            },
            SubsystemCheck {
                name: "valkey".into(),
                status: SubsystemStatus::Healthy,
                latency_ms: 1,
                message: None,
                checked_at: Utc::now(),
            },
            SubsystemCheck {
                name: "minio".into(),
                status: SubsystemStatus::Healthy,
                latency_ms: 1,
                message: None,
                checked_at: Utc::now(),
            },
        ];
    }

    let ready = platform_operator::health::is_ready(&state).await;
    assert!(
        ready,
        "is_ready should return true when cached snapshot shows healthy"
    );
}

// ---------------------------------------------------------------------------
// 10. health_is_ready_postgres_unhealthy
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../../../migrations")]
async fn health_is_ready_postgres_unhealthy(pool: PgPool) {
    let state = helpers::operator_state(pool).await;

    // Inject a recent snapshot where postgres is unhealthy
    {
        let mut snap = state.health.write().unwrap();
        snap.checked_at = Utc::now();
        snap.subsystems = vec![
            SubsystemCheck {
                name: "postgres".into(),
                status: SubsystemStatus::Unhealthy,
                latency_ms: 0,
                message: Some("connection refused".into()),
                checked_at: Utc::now(),
            },
            SubsystemCheck {
                name: "valkey".into(),
                status: SubsystemStatus::Healthy,
                latency_ms: 1,
                message: None,
                checked_at: Utc::now(),
            },
        ];
    }

    let ready = platform_operator::health::is_ready(&state).await;
    assert!(
        !ready,
        "is_ready should return false when postgres is unhealthy"
    );
}

// ---------------------------------------------------------------------------
// 11. health_is_ready_degraded_counts_as_not_ready
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../../../migrations")]
async fn health_is_ready_degraded_counts_as_not_ready(pool: PgPool) {
    let state = helpers::operator_state(pool).await;

    // Degraded is NOT Healthy, so is_ready should return false
    {
        let mut snap = state.health.write().unwrap();
        snap.checked_at = Utc::now();
        snap.subsystems = vec![
            SubsystemCheck {
                name: "postgres".into(),
                status: SubsystemStatus::Degraded,
                latency_ms: 55,
                message: None,
                checked_at: Utc::now(),
            },
            SubsystemCheck {
                name: "valkey".into(),
                status: SubsystemStatus::Healthy,
                latency_ms: 1,
                message: None,
                checked_at: Utc::now(),
            },
        ];
    }

    let ready = platform_operator::health::is_ready(&state).await;
    assert!(
        !ready,
        "is_ready should return false when postgres is degraded (not Healthy)"
    );
}

// ---------------------------------------------------------------------------
// 12. health_snapshot_includes_background_tasks
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../../../migrations")]
async fn health_snapshot_includes_background_tasks(pool: PgPool) {
    let mut state = helpers::operator_state(pool).await;

    // Register a background task
    state.task_registry.register("test-task", 60);
    state.task_registry.heartbeat("test-task");

    // Override interval for fast tick
    let mut config = (*state.config).clone();
    config.health_check_interval_secs = 1;
    state.config = Arc::new(config);

    let cancel = tokio_util::sync::CancellationToken::new();
    let run_state = state.clone();
    let cancel_clone = cancel.clone();
    let handle = tokio::spawn(async move {
        platform_operator::health::run(run_state, cancel_clone).await;
    });

    // Wait for a tick
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let snapshot = state.health.read().unwrap().clone();

    // Should have our registered task
    assert!(
        snapshot
            .background_tasks
            .iter()
            .any(|t| t.name == "test-task"),
        "snapshot should include registered background tasks: {:?}",
        snapshot.background_tasks
    );

    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
}
