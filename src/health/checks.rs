use std::time::{Duration, Instant};

use chrono::Utc;
use fred::interfaces::ClientLike;
use sqlx::Row;
use tracing::Instrument;

use crate::store::AppState;

use super::{HealthSnapshot, PodFailureSummary, RecentPodFailure, SubsystemCheck, SubsystemStatus};

/// Measure latency in ms, capped at `u64::MAX`.
fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
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
pub async fn run(state: AppState, mut shutdown: tokio::sync::watch::Receiver<()>) {
    tracing::info!("health check loop started");
    let start_time = Instant::now();
    let interval_secs = state.config.health_check_interval_secs;
    let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
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
async fn build_snapshot(state: &AppState, start_time: Instant) -> HealthSnapshot {
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

    let background_tasks = state.task_registry.snapshot();
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

/// Quick readiness check: only Postgres + Valkey.
pub async fn is_ready(state: &AppState) -> bool {
    if let Ok(snap) = state.health.read() {
        // If we have a recent snapshot, use it
        let age = Utc::now() - snap.checked_at;
        if age.num_seconds() < 60 {
            return snap
                .subsystems
                .iter()
                .any(|s| s.name == "postgres" && s.status == SubsystemStatus::Healthy)
                && snap
                    .subsystems
                    .iter()
                    .any(|s| s.name == "valkey" && s.status == SubsystemStatus::Healthy);
        }
    }
    // Fallback: run live probes
    let (pg, vk) = tokio::join!(check_postgres(&state.pool), check_valkey(&state.valkey),);
    pg.status == SubsystemStatus::Healthy && vk.status == SubsystemStatus::Healthy
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    // -- elapsed_ms --

    #[test]
    fn elapsed_ms_returns_small_value() {
        let start = Instant::now();
        let ms = elapsed_ms(start);
        // Should be very small (< 100ms) since we just created it
        assert!(ms < 100);
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

    // -- elapsed_ms edge cases --

    #[test]
    fn elapsed_ms_returns_zero_or_near_zero() {
        let start = Instant::now();
        let ms = elapsed_ms(start);
        // Should be less than 10ms since we just created the instant
        assert!(ms < 10, "elapsed_ms should be near zero, got {ms}");
    }

    // -- check_git_repos additional tests --

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

    // -- check_secrets_engine additional tests --

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
        // When master key is present, dev_mode doesn't matter
        let key = "test-key".to_string();
        let result = check_secrets_engine(Some(&key), true);
        assert_eq!(result.status, SubsystemStatus::Healthy);
        assert!(
            result.message.is_none(),
            "should not show dev mode message when key is present"
        );
    }

    // -- check_registry additional tests --

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
}
