// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Health checks, subsystem probes, and background health loop.
//!
//! Types for the health snapshot (subsystem status, pod failures, etc.) and
//! probe functions for Postgres, Valkey, `MinIO`, Kubernetes, git repos, secrets
//! engine, and registry. The background [`run`] loop periodically builds a
//! snapshot and publishes it to Valkey for SSE subscribers.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::Row;
use tracing::Instrument;
use uuid::Uuid;

use crate::state::OperatorState;

// Re-export `TaskRegistry` from platform-types so consumers don't need a
// separate dep just for the registry.
pub use platform_types::health::TaskRegistry;

// ---------------------------------------------------------------------------
// Subsystem status
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubsystemStatus {
    Healthy,
    Degraded,
    Unhealthy,
    Unknown,
}

impl SubsystemStatus {
    /// Returns the worst of two statuses (Unhealthy > Degraded > Unknown > Healthy).
    #[must_use]
    pub fn worst(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unhealthy, _) | (_, Self::Unhealthy) => Self::Unhealthy,
            (Self::Degraded, _) | (_, Self::Degraded) => Self::Degraded,
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            _ => Self::Healthy,
        }
    }

    /// Compute the overall status from a list of statuses.
    pub fn aggregate(statuses: &[Self]) -> Self {
        statuses.iter().copied().fold(Self::Healthy, Self::worst)
    }
}

// ---------------------------------------------------------------------------
// Subsystem check result
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct SubsystemCheck {
    pub name: String,
    pub status: SubsystemStatus,
    pub latency_ms: u64,
    pub message: Option<String>,
    pub checked_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Background task health
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct BackgroundTaskHealth {
    pub name: String,
    pub status: SubsystemStatus,
    pub last_heartbeat: Option<DateTime<Utc>>,
    pub success_count: u64,
    pub failure_count: u64,
    pub last_error: Option<String>,
}

// ---------------------------------------------------------------------------
// Pod failure types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct RecentPodFailure {
    pub id: Uuid,
    pub project_id: Option<Uuid>,
    pub project_name: Option<String>,
    pub pod_name: Option<String>,
    pub kind: String,
    pub error: Option<String>,
    pub failed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PodFailureSummary {
    pub total_failed_24h: i64,
    pub agent_failures: i64,
    pub pipeline_failures: i64,
    pub recent_failures: Vec<RecentPodFailure>,
}

// ---------------------------------------------------------------------------
// Health snapshot
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct HealthSnapshot {
    pub overall: SubsystemStatus,
    pub subsystems: Vec<SubsystemCheck>,
    pub background_tasks: Vec<BackgroundTaskHealth>,
    pub pod_failures: PodFailureSummary,
    pub uptime_seconds: u64,
    pub checked_at: DateTime<Utc>,
}

impl Default for HealthSnapshot {
    fn default() -> Self {
        Self {
            overall: SubsystemStatus::Unknown,
            subsystems: Vec::new(),
            background_tasks: Vec::new(),
            pod_failures: PodFailureSummary {
                total_failed_24h: 0,
                agent_failures: 0,
                pipeline_failures: 0,
                recent_failures: Vec::new(),
            },
            uptime_seconds: 0,
            checked_at: Utc::now(),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Measure latency in ms, capped at `u64::MAX`.
fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Convert a [`platform_types::health::TaskSnapshot`] into the operator's
/// [`BackgroundTaskHealth`] representation.
fn task_snapshot_to_health(snap: &platform_types::health::TaskSnapshot) -> BackgroundTaskHealth {
    let status = match snap.status {
        platform_types::health::TaskStatus::Healthy => SubsystemStatus::Healthy,
        platform_types::health::TaskStatus::Degraded => SubsystemStatus::Degraded,
        platform_types::health::TaskStatus::Unhealthy => SubsystemStatus::Unhealthy,
    };
    BackgroundTaskHealth {
        name: snap.name.clone(),
        status,
        last_heartbeat: snap.last_heartbeat,
        success_count: snap.success_count,
        failure_count: snap.failure_count,
        last_error: snap.last_error.clone(),
    }
}

// ---------------------------------------------------------------------------
// Individual probes
// ---------------------------------------------------------------------------

async fn check_postgres(pool: &sqlx::PgPool) -> SubsystemCheck {
    let start = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(pool),
    )
    .await;
    let latency_ms = elapsed_ms(start);

    let (status, message) = match result {
        Ok(Ok(_)) => {
            let s = if latency_ms < 50 {
                SubsystemStatus::Healthy
            } else {
                SubsystemStatus::Degraded
            };
            (s, None)
        }
        Ok(Err(e)) => (SubsystemStatus::Unhealthy, Some(e.to_string())),
        Err(_) => (
            SubsystemStatus::Unhealthy,
            Some("timeout (>2s)".to_string()),
        ),
    };

    SubsystemCheck {
        name: "postgres".into(),
        status,
        latency_ms,
        message,
        checked_at: Utc::now(),
    }
}

async fn check_valkey(valkey: &fred::clients::Pool) -> SubsystemCheck {
    use fred::interfaces::ClientLike;

    let start = Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(2), valkey.ping::<String>(None)).await;
    let latency_ms = elapsed_ms(start);

    let (status, message) = match result {
        Ok(Ok(_)) => {
            let s = if latency_ms < 50 {
                SubsystemStatus::Healthy
            } else {
                SubsystemStatus::Degraded
            };
            (s, None)
        }
        Ok(Err(e)) => (SubsystemStatus::Unhealthy, Some(e.to_string())),
        Err(_) => (
            SubsystemStatus::Unhealthy,
            Some("timeout (>2s)".to_string()),
        ),
    };

    SubsystemCheck {
        name: "valkey".into(),
        status,
        latency_ms,
        message,
        checked_at: Utc::now(),
    }
}

async fn check_minio(minio: &opendal::Operator) -> SubsystemCheck {
    let start = Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(2), minio.stat("_health")).await;
    let latency_ms = elapsed_ms(start);

    let (status, message) = match result {
        Ok(Ok(_)) => (SubsystemStatus::Healthy, None),
        Ok(Err(e)) if e.kind() == opendal::ErrorKind::NotFound => {
            // NotFound means the service is reachable
            let s = if latency_ms < 200 {
                SubsystemStatus::Healthy
            } else {
                SubsystemStatus::Degraded
            };
            (s, None)
        }
        Ok(Err(e)) => (SubsystemStatus::Unhealthy, Some(e.to_string())),
        Err(_) => (
            SubsystemStatus::Unhealthy,
            Some("timeout (>2s)".to_string()),
        ),
    };

    SubsystemCheck {
        name: "minio".into(),
        status,
        latency_ms,
        message,
        checked_at: Utc::now(),
    }
}

async fn check_kubernetes(kube: &kube::Client) -> SubsystemCheck {
    let start = Instant::now();
    let result: Result<Result<_, kube::Error>, _> =
        tokio::time::timeout(Duration::from_secs(2), kube.apiserver_version()).await;
    let latency_ms = elapsed_ms(start);

    let (status, message) = match result {
        Ok(Ok(_)) => {
            let s = if latency_ms < 500 {
                SubsystemStatus::Healthy
            } else {
                SubsystemStatus::Degraded
            };
            (s, None)
        }
        Ok(Err(e)) => (SubsystemStatus::Unhealthy, Some(e.to_string())),
        Err(_) => (
            SubsystemStatus::Unhealthy,
            Some("timeout (>2s)".to_string()),
        ),
    };

    SubsystemCheck {
        name: "kubernetes".into(),
        status,
        latency_ms,
        message,
        checked_at: Utc::now(),
    }
}

fn check_git_repos(path: &std::path::Path) -> SubsystemCheck {
    let start = Instant::now();
    let (status, message) = if path.exists() {
        (SubsystemStatus::Healthy, None)
    } else {
        (
            SubsystemStatus::Unhealthy,
            Some(format!("path not found: {}", path.display())),
        )
    };
    let latency_ms = elapsed_ms(start);

    SubsystemCheck {
        name: "git_repos".into(),
        status,
        latency_ms,
        message,
        checked_at: Utc::now(),
    }
}

fn check_secrets_engine(master_key: Option<&String>, dev_mode: bool) -> SubsystemCheck {
    let (status, message) = if master_key.is_some() {
        (SubsystemStatus::Healthy, None)
    } else if dev_mode {
        (SubsystemStatus::Healthy, Some("dev mode (auto key)".into()))
    } else {
        (
            SubsystemStatus::Unhealthy,
            Some("PLATFORM_MASTER_KEY not set".into()),
        )
    };

    SubsystemCheck {
        name: "secrets".into(),
        status,
        latency_ms: 0,
        message,
        checked_at: Utc::now(),
    }
}

fn check_registry(registry_url: Option<&String>) -> SubsystemCheck {
    let (status, message) = if registry_url.is_some() {
        (SubsystemStatus::Healthy, None)
    } else {
        (
            SubsystemStatus::Degraded,
            Some("registry not configured".into()),
        )
    };

    SubsystemCheck {
        name: "registry".into(),
        status,
        latency_ms: 0,
        message,
        checked_at: Utc::now(),
    }
}

// ---------------------------------------------------------------------------
// Pod failure aggregation
// ---------------------------------------------------------------------------

async fn query_pod_failures(pool: &sqlx::PgPool) -> PodFailureSummary {
    let agent_failures: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_sessions WHERE status = 'failed' AND finished_at > now() - interval '24 hours'",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(0);

    let pipeline_failures: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pipelines WHERE status = 'failure' AND finished_at > now() - interval '24 hours'",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(0);

    // Recent failures (top 20, UNION agent + pipeline)
    let recent = sqlx::query(
        r"
        (
            SELECT s.id, s.project_id, p.name as project_name, s.pod_name,
                   'agent' as kind, NULL as error, s.finished_at as failed_at
            FROM agent_sessions s
            LEFT JOIN projects p ON p.id = s.project_id
            WHERE s.status = 'failed' AND s.finished_at > now() - interval '24 hours'
            ORDER BY s.finished_at DESC LIMIT 10
        )
        UNION ALL
        (
            SELECT pi.id, pi.project_id, p.name as project_name, NULL as pod_name,
                   'pipeline' as kind, NULL as error, pi.finished_at as failed_at
            FROM pipelines pi
            LEFT JOIN projects p ON p.id = pi.project_id
            WHERE pi.status = 'failure' AND pi.finished_at > now() - interval '24 hours'
            ORDER BY pi.finished_at DESC LIMIT 10
        )
        ORDER BY failed_at DESC
        LIMIT 20
        ",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let recent_failures: Vec<RecentPodFailure> = recent
        .into_iter()
        .map(|r| RecentPodFailure {
            id: r.get("id"),
            project_id: r.get("project_id"),
            project_name: r.get("project_name"),
            pod_name: r.get("pod_name"),
            kind: r.get("kind"),
            error: r.get("error"),
            failed_at: r.get("failed_at"),
        })
        .collect();

    PodFailureSummary {
        total_failed_24h: agent_failures + pipeline_failures,
        agent_failures,
        pipeline_failures,
        recent_failures,
    }
}

// ---------------------------------------------------------------------------
// Background health loop
// ---------------------------------------------------------------------------

/// Background task that periodically checks all subsystems and updates the
/// shared health snapshot.
pub async fn run(state: OperatorState, cancel: tokio_util::sync::CancellationToken) {
    tracing::info!("health check loop started");
    let start_time = Instant::now();
    let interval_secs = state.config.health_check_interval_secs;
    let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));

    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                tracing::info!("health check loop shutting down");
                break;
            }
            _ = interval.tick() => {
                let iter_trace_id = uuid::Uuid::new_v4().to_string().replace('-', "");
                let span = tracing::info_span!(
                    "task_iteration",
                    task_name = "health_checks",
                    trace_id = %iter_trace_id,
                    source = "system",
                );
                async {
                    let snapshot = build_snapshot(&state, start_time).await;

                    // Publish to Valkey for SSE subscribers
                    if let Ok(json) = serde_json::to_string(&snapshot) {
                        let _: Result<(), _> = fred::interfaces::PubsubInterface::publish::<(), _, _>(
                            state.valkey.next(),
                            "health:stream",
                            json,
                        ).await;
                    }

                    // Update shared snapshot
                    if let Ok(mut snap) = state.health.write() {
                        *snap = snapshot;
                    }
                }.instrument(span).await;
            }
        }
    }
}

/// Run all probes and build a complete health snapshot.
async fn build_snapshot(state: &OperatorState, start_time: Instant) -> HealthSnapshot {
    // Run async probes concurrently
    let (pg, vk, minio, k8s) = tokio::join!(
        check_postgres(&state.pool),
        check_valkey(&state.valkey),
        check_minio(&state.minio),
        check_kubernetes(&state.kube),
    );

    // Sync probes
    let git = check_git_repos(&state.config.git_repos_path);
    let secrets = check_secrets_engine(state.config.master_key.as_ref(), state.config.dev_mode);
    let registry = check_registry(state.config.registry_url.as_ref());

    let subsystems = vec![pg, vk, minio, k8s, git, secrets, registry];
    let overall =
        SubsystemStatus::aggregate(&subsystems.iter().map(|s| s.status).collect::<Vec<_>>());

    let background_tasks: Vec<BackgroundTaskHealth> = state
        .task_registry
        .snapshot()
        .iter()
        .map(task_snapshot_to_health)
        .collect();
    let pod_failures = query_pod_failures(&state.pool).await;
    let uptime_seconds = start_time.elapsed().as_secs();

    HealthSnapshot {
        overall,
        subsystems,
        background_tasks,
        pod_failures,
        uptime_seconds,
        checked_at: Utc::now(),
    }
}

/// Quick readiness check: Postgres + Valkey + `MinIO`.
pub async fn is_ready(state: &OperatorState) -> bool {
    if let Ok(snap) = state.health.read() {
        let age = Utc::now() - snap.checked_at;
        // 15s cache -- must be <= K8s probe period
        if age.num_seconds() < 15 {
            let required = ["postgres", "valkey", "minio"];
            return required.iter().all(|name| {
                snap.subsystems.iter().any(|s| {
                    s.name == *name
                        && matches!(
                            s.status,
                            SubsystemStatus::Healthy | SubsystemStatus::Degraded
                        )
                })
            });
        }
    }
    // Stale or missing snapshot -- run live probes
    let (pg, vk, minio) = tokio::join!(
        check_postgres(&state.pool),
        check_valkey(&state.valkey),
        check_minio(&state.minio),
    );
    let ok = |s: &SubsystemCheck| {
        matches!(
            s.status,
            SubsystemStatus::Healthy | SubsystemStatus::Degraded
        )
    };
    ok(&pg) && ok(&vk) && ok(&minio)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    // -- SubsystemStatus --

    #[test]
    fn subsystem_status_worst_of() {
        assert_eq!(
            SubsystemStatus::Healthy.worst(SubsystemStatus::Healthy),
            SubsystemStatus::Healthy
        );
        assert_eq!(
            SubsystemStatus::Healthy.worst(SubsystemStatus::Degraded),
            SubsystemStatus::Degraded
        );
        assert_eq!(
            SubsystemStatus::Degraded.worst(SubsystemStatus::Unhealthy),
            SubsystemStatus::Unhealthy
        );
        assert_eq!(
            SubsystemStatus::Unknown.worst(SubsystemStatus::Healthy),
            SubsystemStatus::Unknown
        );
        assert_eq!(
            SubsystemStatus::Unknown.worst(SubsystemStatus::Unhealthy),
            SubsystemStatus::Unhealthy
        );
    }

    #[test]
    fn subsystem_status_aggregate_all_healthy() {
        let statuses = vec![SubsystemStatus::Healthy, SubsystemStatus::Healthy];
        assert_eq!(
            SubsystemStatus::aggregate(&statuses),
            SubsystemStatus::Healthy
        );
    }

    #[test]
    fn subsystem_status_aggregate_mixed() {
        let statuses = vec![
            SubsystemStatus::Healthy,
            SubsystemStatus::Degraded,
            SubsystemStatus::Healthy,
        ];
        assert_eq!(
            SubsystemStatus::aggregate(&statuses),
            SubsystemStatus::Degraded
        );
    }

    #[test]
    fn subsystem_status_aggregate_unhealthy_wins() {
        let statuses = vec![
            SubsystemStatus::Healthy,
            SubsystemStatus::Unhealthy,
            SubsystemStatus::Degraded,
        ];
        assert_eq!(
            SubsystemStatus::aggregate(&statuses),
            SubsystemStatus::Unhealthy
        );
    }

    #[test]
    fn subsystem_status_aggregate_empty() {
        assert_eq!(SubsystemStatus::aggregate(&[]), SubsystemStatus::Healthy);
    }

    #[test]
    fn subsystem_status_worst_symmetry() {
        let variants = [
            SubsystemStatus::Healthy,
            SubsystemStatus::Degraded,
            SubsystemStatus::Unhealthy,
            SubsystemStatus::Unknown,
        ];
        for a in &variants {
            for b in &variants {
                assert_eq!(
                    a.worst(*b),
                    b.worst(*a),
                    "worst({a:?}, {b:?}) != worst({b:?}, {a:?})"
                );
            }
        }
    }

    #[test]
    fn subsystem_status_aggregate_single_unhealthy() {
        assert_eq!(
            SubsystemStatus::aggregate(&[SubsystemStatus::Unhealthy]),
            SubsystemStatus::Unhealthy
        );
    }

    #[test]
    fn subsystem_status_aggregate_single_healthy() {
        assert_eq!(
            SubsystemStatus::aggregate(&[SubsystemStatus::Healthy]),
            SubsystemStatus::Healthy
        );
    }

    #[test]
    fn subsystem_status_aggregate_all_unknown() {
        let statuses = vec![SubsystemStatus::Unknown, SubsystemStatus::Unknown];
        assert_eq!(
            SubsystemStatus::aggregate(&statuses),
            SubsystemStatus::Unknown
        );
    }

    #[test]
    fn subsystem_status_worst_degraded_unknown() {
        assert_eq!(
            SubsystemStatus::Degraded.worst(SubsystemStatus::Unknown),
            SubsystemStatus::Degraded
        );
    }

    #[test]
    fn subsystem_status_copy_clone() {
        let s = SubsystemStatus::Healthy;
        let s2 = s;
        #[allow(clippy::clone_on_copy)]
        let s3 = s.clone();
        assert_eq!(s, s2);
        assert_eq!(s, s3);
    }

    #[test]
    fn subsystem_status_debug() {
        let debug = format!("{:?}", SubsystemStatus::Unhealthy);
        assert_eq!(debug, "Unhealthy");
    }

    #[test]
    fn subsystem_status_serialization() {
        assert_eq!(
            serde_json::to_string(&SubsystemStatus::Healthy).unwrap(),
            "\"healthy\""
        );
        assert_eq!(
            serde_json::to_string(&SubsystemStatus::Degraded).unwrap(),
            "\"degraded\""
        );
        assert_eq!(
            serde_json::to_string(&SubsystemStatus::Unhealthy).unwrap(),
            "\"unhealthy\""
        );
        assert_eq!(
            serde_json::to_string(&SubsystemStatus::Unknown).unwrap(),
            "\"unknown\""
        );
    }

    // -- HealthSnapshot --

    #[test]
    fn health_snapshot_default() {
        let snap = HealthSnapshot::default();
        assert_eq!(snap.overall, SubsystemStatus::Unknown);
        assert!(snap.subsystems.is_empty());
        assert!(snap.background_tasks.is_empty());
        assert_eq!(snap.pod_failures.total_failed_24h, 0);
    }

    #[test]
    fn health_snapshot_default_has_recent_checked_at() {
        let snap = HealthSnapshot::default();
        let age = Utc::now() - snap.checked_at;
        assert!(age.num_seconds() < 2, "default checked_at should be recent");
    }

    #[test]
    fn health_snapshot_default_uptime_is_zero() {
        let snap = HealthSnapshot::default();
        assert_eq!(snap.uptime_seconds, 0);
    }

    #[test]
    fn health_snapshot_serialization() {
        let snap = HealthSnapshot::default();
        let json = serde_json::to_value(&snap).unwrap();
        assert_eq!(json["overall"], "unknown");
        assert_eq!(json["uptime_seconds"], 0);
        assert!(json["subsystems"].is_array());
        assert!(json["background_tasks"].is_array());
    }

    // -- SubsystemCheck --

    #[test]
    fn subsystem_check_serialization() {
        let check = SubsystemCheck {
            name: "test".into(),
            status: SubsystemStatus::Healthy,
            latency_ms: 42,
            message: Some("ok".into()),
            checked_at: Utc::now(),
        };
        let json = serde_json::to_value(&check).unwrap();
        assert_eq!(json["name"], "test");
        assert_eq!(json["status"], "healthy");
        assert_eq!(json["latency_ms"], 42);
        assert_eq!(json["message"], "ok");
    }

    // -- BackgroundTaskHealth --

    #[test]
    fn background_task_health_serialization() {
        let task = BackgroundTaskHealth {
            name: "test-task".into(),
            status: SubsystemStatus::Degraded,
            last_heartbeat: Some(Utc::now()),
            success_count: 10,
            failure_count: 2,
            last_error: Some("timeout".into()),
        };
        let json = serde_json::to_value(&task).unwrap();
        assert_eq!(json["name"], "test-task");
        assert_eq!(json["status"], "degraded");
        assert_eq!(json["success_count"], 10);
        assert_eq!(json["failure_count"], 2);
        assert_eq!(json["last_error"], "timeout");
    }

    // -- PodFailureSummary / RecentPodFailure --

    #[test]
    fn pod_failure_summary_serialization() {
        let summary = PodFailureSummary {
            total_failed_24h: 5,
            agent_failures: 3,
            pipeline_failures: 2,
            recent_failures: vec![RecentPodFailure {
                id: Uuid::nil(),
                project_id: Some(Uuid::nil()),
                project_name: Some("demo".into()),
                pod_name: Some("demo-pod-abc".into()),
                kind: "agent".into(),
                error: Some("OOMKilled".into()),
                failed_at: Utc::now(),
            }],
        };
        let json = serde_json::to_value(&summary).unwrap();
        assert_eq!(json["total_failed_24h"], 5);
        assert_eq!(json["agent_failures"], 3);
        assert_eq!(json["pipeline_failures"], 2);
        assert_eq!(json["recent_failures"].as_array().unwrap().len(), 1);
        assert_eq!(json["recent_failures"][0]["kind"], "agent");
        assert_eq!(json["recent_failures"][0]["error"], "OOMKilled");
    }

    #[test]
    fn recent_pod_failure_debug() {
        let failure = RecentPodFailure {
            id: Uuid::nil(),
            project_id: None,
            project_name: None,
            pod_name: None,
            kind: "pipeline".into(),
            error: None,
            failed_at: Utc::now(),
        };
        let debug = format!("{failure:?}");
        assert!(debug.contains("pipeline"));
    }

    // -- elapsed_ms --

    #[test]
    fn elapsed_ms_returns_small_value() {
        let start = Instant::now();
        let ms = elapsed_ms(start);
        assert!(ms < 100);
    }

    #[test]
    fn elapsed_ms_returns_zero_or_near_zero() {
        let start = Instant::now();
        let ms = elapsed_ms(start);
        assert!(ms < 10, "elapsed_ms should be near zero, got {ms}");
    }

    // -- check_git_repos --

    #[test]
    fn check_git_repos_path_exists() {
        let result = check_git_repos(std::path::Path::new("/tmp"));
        assert_eq!(result.name, "git_repos");
        assert_eq!(result.status, SubsystemStatus::Healthy);
        assert!(result.message.is_none());
    }

    #[test]
    fn check_git_repos_path_missing() {
        let result = check_git_repos(std::path::Path::new("/nonexistent/path/xyz"));
        assert_eq!(result.name, "git_repos");
        assert_eq!(result.status, SubsystemStatus::Unhealthy);
        assert!(result.message.unwrap().contains("path not found"));
    }

    #[test]
    fn check_git_repos_has_zero_latency_essentially() {
        let result = check_git_repos(std::path::Path::new("/tmp"));
        assert!(
            result.latency_ms < 100,
            "git_repos check should be fast, got {}ms",
            result.latency_ms
        );
        assert!(result.checked_at <= chrono::Utc::now());
    }

    #[test]
    fn check_git_repos_missing_has_error_message() {
        let result = check_git_repos(std::path::Path::new("/this/path/does/not/exist/abc123"));
        assert_eq!(result.status, SubsystemStatus::Unhealthy);
        let msg = result.message.unwrap();
        assert!(
            msg.contains("/this/path/does/not/exist/abc123"),
            "error message should contain the path, got: {msg}"
        );
    }

    // -- check_secrets_engine --

    #[test]
    fn check_secrets_engine_with_key() {
        let key = "my-secret-key".to_string();
        let result = check_secrets_engine(Some(&key), false);
        assert_eq!(result.name, "secrets");
        assert_eq!(result.status, SubsystemStatus::Healthy);
        assert!(result.message.is_none());
    }

    #[test]
    fn check_secrets_engine_dev_mode() {
        let result = check_secrets_engine(None, true);
        assert_eq!(result.status, SubsystemStatus::Healthy);
        assert_eq!(result.message.as_deref(), Some("dev mode (auto key)"));
    }

    #[test]
    fn check_secrets_engine_no_key_no_dev() {
        let result = check_secrets_engine(None, false);
        assert_eq!(result.status, SubsystemStatus::Unhealthy);
        assert!(result.message.unwrap().contains("PLATFORM_MASTER_KEY"));
    }

    #[test]
    fn check_secrets_engine_has_zero_latency() {
        let key = "test-key".to_string();
        let result = check_secrets_engine(Some(&key), false);
        assert_eq!(
            result.latency_ms, 0,
            "secrets engine check is synchronous and should report 0 latency"
        );
    }

    #[test]
    fn check_secrets_engine_dev_mode_with_key_prefers_key() {
        let key = "test-key".to_string();
        let result = check_secrets_engine(Some(&key), true);
        assert_eq!(result.status, SubsystemStatus::Healthy);
        assert!(
            result.message.is_none(),
            "should not show dev mode message when key is present"
        );
    }

    // -- check_registry --

    #[test]
    fn check_registry_configured() {
        let url = "registry.example.com:5000".to_string();
        let result = check_registry(Some(&url));
        assert_eq!(result.name, "registry");
        assert_eq!(result.status, SubsystemStatus::Healthy);
        assert!(result.message.is_none());
    }

    #[test]
    fn check_registry_not_configured() {
        let result = check_registry(None);
        assert_eq!(result.name, "registry");
        assert_eq!(result.status, SubsystemStatus::Degraded);
        assert!(result.message.unwrap().contains("not configured"));
    }

    #[test]
    fn check_registry_has_zero_latency() {
        let url = "registry.example.com:5000".to_string();
        let result = check_registry(Some(&url));
        assert_eq!(result.latency_ms, 0);
    }

    #[test]
    fn check_registry_not_configured_has_zero_latency() {
        let result = check_registry(None);
        assert_eq!(result.latency_ms, 0);
    }

    // -- task_snapshot_to_health --

    #[test]
    fn task_snapshot_to_health_converts_healthy() {
        let snap = platform_types::health::TaskSnapshot {
            name: "test".into(),
            status: platform_types::health::TaskStatus::Healthy,
            last_heartbeat: Some(Utc::now()),
            success_count: 5,
            failure_count: 0,
            last_error: None,
        };
        let health = task_snapshot_to_health(&snap);
        assert_eq!(health.name, "test");
        assert_eq!(health.status, SubsystemStatus::Healthy);
        assert_eq!(health.success_count, 5);
        assert_eq!(health.failure_count, 0);
        assert!(health.last_error.is_none());
    }

    #[test]
    fn task_snapshot_to_health_converts_degraded() {
        let snap = platform_types::health::TaskSnapshot {
            name: "degraded-task".into(),
            status: platform_types::health::TaskStatus::Degraded,
            last_heartbeat: Some(Utc::now()),
            success_count: 3,
            failure_count: 1,
            last_error: Some("timeout".into()),
        };
        let health = task_snapshot_to_health(&snap);
        assert_eq!(health.status, SubsystemStatus::Degraded);
        assert_eq!(health.last_error.as_deref(), Some("timeout"));
    }

    #[test]
    fn task_snapshot_to_health_converts_unhealthy() {
        let snap = platform_types::health::TaskSnapshot {
            name: "dead-task".into(),
            status: platform_types::health::TaskStatus::Unhealthy,
            last_heartbeat: None,
            success_count: 0,
            failure_count: 10,
            last_error: Some("crash".into()),
        };
        let health = task_snapshot_to_health(&snap);
        assert_eq!(health.status, SubsystemStatus::Unhealthy);
        assert!(health.last_heartbeat.is_none());
    }
}
