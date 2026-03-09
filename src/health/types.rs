use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde::Serialize;
use ts_rs::TS;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Subsystem status
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[ts(export)]
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

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct SubsystemCheck {
    pub name: String,
    pub status: SubsystemStatus,
    #[ts(type = "number")]
    pub latency_ms: u64,
    pub message: Option<String>,
    pub checked_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Background task health
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct BackgroundTaskHealth {
    pub name: String,
    pub status: SubsystemStatus,
    pub last_heartbeat: Option<DateTime<Utc>>,
    #[ts(type = "number")]
    pub success_count: u64,
    #[ts(type = "number")]
    pub failure_count: u64,
    pub last_error: Option<String>,
}

// ---------------------------------------------------------------------------
// Pod failure types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct RecentPodFailure {
    pub id: Uuid,
    pub project_id: Option<Uuid>,
    pub project_name: Option<String>,
    pub pod_name: Option<String>,
    pub kind: String,
    pub error: Option<String>,
    pub failed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct PodFailureSummary {
    #[ts(type = "number")]
    pub total_failed_24h: i64,
    #[ts(type = "number")]
    pub agent_failures: i64,
    #[ts(type = "number")]
    pub pipeline_failures: i64,
    pub recent_failures: Vec<RecentPodFailure>,
}

// ---------------------------------------------------------------------------
// Health snapshot
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct HealthSnapshot {
    pub overall: SubsystemStatus,
    pub subsystems: Vec<SubsystemCheck>,
    pub background_tasks: Vec<BackgroundTaskHealth>,
    pub pod_failures: PodFailureSummary,
    #[ts(type = "number")]
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
// Task registry (in-memory heartbeat tracker)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct TaskHeartbeat {
    last_beat: Instant,
    last_beat_utc: DateTime<Utc>,
    success_count: u64,
    failure_count: u64,
    last_error: Option<String>,
    /// Expected interval in seconds. Task is "stale" if 3x this elapses.
    expected_interval_secs: u64,
}

#[derive(Debug, Clone)]
pub struct TaskRegistry {
    tasks: Arc<RwLock<HashMap<String, TaskHeartbeat>>>,
}

impl TaskRegistry {
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Record a successful heartbeat for a named task.
    pub fn heartbeat(&self, name: &str) {
        if let Ok(mut map) = self.tasks.write() {
            let entry = map.entry(name.to_owned()).or_insert_with(|| TaskHeartbeat {
                last_beat: Instant::now(),
                last_beat_utc: Utc::now(),
                success_count: 0,
                failure_count: 0,
                last_error: None,
                expected_interval_secs: 30,
            });
            entry.last_beat = Instant::now();
            entry.last_beat_utc = Utc::now();
            entry.success_count += 1;
        }
    }

    /// Record an error for a named task.
    pub fn report_error(&self, name: &str, err: &str) {
        if let Ok(mut map) = self.tasks.write() {
            let entry = map.entry(name.to_owned()).or_insert_with(|| TaskHeartbeat {
                last_beat: Instant::now(),
                last_beat_utc: Utc::now(),
                success_count: 0,
                failure_count: 0,
                last_error: None,
                expected_interval_secs: 30,
            });
            entry.last_beat = Instant::now();
            entry.last_beat_utc = Utc::now();
            entry.failure_count += 1;
            entry.last_error = Some(err.to_owned());
        }
    }

    /// Register a task with its expected interval (in seconds).
    pub fn register(&self, name: &str, expected_interval_secs: u64) {
        if let Ok(mut map) = self.tasks.write() {
            map.entry(name.to_owned()).or_insert_with(|| TaskHeartbeat {
                last_beat: Instant::now(),
                last_beat_utc: Utc::now(),
                success_count: 0,
                failure_count: 0,
                last_error: None,
                expected_interval_secs,
            });
        }
    }

    /// Build a snapshot of all tasks' health.
    pub fn snapshot(&self) -> Vec<BackgroundTaskHealth> {
        let Ok(map) = self.tasks.read() else {
            return Vec::new();
        };
        let now = Instant::now();
        let mut tasks: Vec<BackgroundTaskHealth> = map
            .iter()
            .map(|(name, hb)| {
                let stale_threshold = std::time::Duration::from_secs(hb.expected_interval_secs * 3);
                let elapsed = now.duration_since(hb.last_beat);
                let status = if elapsed > stale_threshold {
                    SubsystemStatus::Unhealthy
                } else if hb.last_error.is_some() {
                    SubsystemStatus::Degraded
                } else {
                    SubsystemStatus::Healthy
                };
                BackgroundTaskHealth {
                    name: name.clone(),
                    status,
                    last_heartbeat: Some(hb.last_beat_utc),
                    success_count: hb.success_count,
                    failure_count: hb.failure_count,
                    last_error: hb.last_error.clone(),
                }
            })
            .collect();
        // Sort unhealthy first, then by name
        tasks.sort_by(|a, b| {
            let order = |s: &SubsystemStatus| match s {
                SubsystemStatus::Unhealthy => 0,
                SubsystemStatus::Degraded => 1,
                SubsystemStatus::Unknown => 2,
                SubsystemStatus::Healthy => 3,
            };
            order(&a.status)
                .cmp(&order(&b.status))
                .then(a.name.cmp(&b.name))
        });
        tasks
    }
}

impl Default for TaskRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn task_registry_heartbeat_increments() {
        let registry = TaskRegistry::new();
        registry.heartbeat("test-task");
        registry.heartbeat("test-task");
        let snap = registry.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].name, "test-task");
        assert_eq!(snap[0].success_count, 2);
        assert_eq!(snap[0].failure_count, 0);
        assert_eq!(snap[0].status, SubsystemStatus::Healthy);
    }

    #[test]
    fn task_registry_report_error() {
        let registry = TaskRegistry::new();
        registry.heartbeat("task-a");
        registry.report_error("task-a", "connection refused");
        let snap = registry.snapshot();
        assert_eq!(snap[0].success_count, 1);
        assert_eq!(snap[0].failure_count, 1);
        assert_eq!(snap[0].last_error.as_deref(), Some("connection refused"));
        assert_eq!(snap[0].status, SubsystemStatus::Degraded);
    }

    #[test]
    fn task_registry_register_sets_interval() {
        let registry = TaskRegistry::new();
        registry.register("slow-task", 3600);
        let snap = registry.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].name, "slow-task");
        assert_eq!(snap[0].status, SubsystemStatus::Healthy);
    }

    #[test]
    fn task_registry_multiple_tasks() {
        let registry = TaskRegistry::new();
        registry.heartbeat("task-a");
        registry.heartbeat("task-b");
        registry.report_error("task-c", "err");
        let snap = registry.snapshot();
        assert_eq!(snap.len(), 3);
    }

    #[test]
    fn task_registry_snapshot_sorts_unhealthy_first() {
        let registry = TaskRegistry::new();
        registry.heartbeat("healthy-task");
        registry.report_error("degraded-task", "oops");
        let snap = registry.snapshot();
        // degraded should come before healthy
        assert_eq!(snap[0].name, "degraded-task");
        assert_eq!(snap[1].name, "healthy-task");
    }

    #[test]
    fn health_snapshot_default() {
        let snap = HealthSnapshot::default();
        assert_eq!(snap.overall, SubsystemStatus::Unknown);
        assert!(snap.subsystems.is_empty());
        assert!(snap.background_tasks.is_empty());
        assert_eq!(snap.pod_failures.total_failed_24h, 0);
    }
}
