// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

// Types used by later phases (reconciler rewrite, analysis loop, gateway, API).
#![cfg_attr(not(test), allow(dead_code))]

use std::fmt;

// ---------------------------------------------------------------------------
// Release phase state machine
// ---------------------------------------------------------------------------

/// Phase of a release deployment.
///
/// ```text
/// pending ──► progressing ──► holding ──► progressing (auto-retry loop)
///   │              │            │  │
///   │              │            │  └─ max_failures ──► rolling_back
///   │              │            │
///   │              ├──► paused ──► progressing (manual resume)
///   │              │
///   │              └──────────────► promoting ──► completed
///   │
///   ▼              ▼            ▼
/// cancelled    rolling_back  rolling_back ──► rolled_back
///
/// Any non-terminal ──► failed (unrecoverable error)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleasePhase {
    Pending,
    Progressing,
    Holding,
    Paused,
    Promoting,
    Completed,
    RollingBack,
    RolledBack,
    Cancelled,
    Failed,
}

impl ReleasePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Progressing => "progressing",
            Self::Holding => "holding",
            Self::Paused => "paused",
            Self::Promoting => "promoting",
            Self::Completed => "completed",
            Self::RollingBack => "rolling_back",
            Self::RolledBack => "rolled_back",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "progressing" => Some(Self::Progressing),
            "holding" => Some(Self::Holding),
            "paused" => Some(Self::Paused),
            "promoting" => Some(Self::Promoting),
            "completed" => Some(Self::Completed),
            "rolling_back" => Some(Self::RollingBack),
            "rolled_back" => Some(Self::RolledBack),
            "cancelled" => Some(Self::Cancelled),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::RolledBack | Self::Cancelled | Self::Failed
        )
    }

    /// Check whether transitioning from `self` to `next` is valid.
    pub fn can_transition_to(self, next: Self) -> bool {
        if self.is_terminal() {
            return false;
        }
        // Any non-terminal can transition to Failed (unrecoverable error)
        if next == Self::Failed {
            return true;
        }
        matches!(
            (self, next),
            // Start (Completed is fast-path for rolling deploys)
            (Self::Pending, Self::Progressing | Self::Completed | Self::Cancelled)
            // Normal flow
            | (Self::Progressing, Self::Holding | Self::Paused | Self::Promoting | Self::RollingBack)
            // Hold → retry or escalate
            | (Self::Holding, Self::Progressing | Self::RollingBack)
            // Manual pause/resume
            | (Self::Paused, Self::Progressing | Self::Cancelled | Self::RollingBack)
            // Promote finalization
            | (Self::Promoting, Self::Completed)
            // Rollback finalization
            | (Self::RollingBack, Self::RolledBack)
        )
    }
}

impl fmt::Display for ReleasePhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ReleasePhase {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "progressing" => Ok(Self::Progressing),
            "holding" => Ok(Self::Holding),
            "paused" => Ok(Self::Paused),
            "promoting" => Ok(Self::Promoting),
            "completed" => Ok(Self::Completed),
            "rolling_back" => Ok(Self::RollingBack),
            "rolled_back" => Ok(Self::RolledBack),
            "cancelled" => Ok(Self::Cancelled),
            "failed" => Ok(Self::Failed),
            other => anyhow::bail!("unknown release phase: {other}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Deploy strategy
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeployStrategy {
    Rolling,
    Canary,
    AbTest,
}

impl DeployStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rolling => "rolling",
            Self::Canary => "canary",
            Self::AbTest => "ab_test",
        }
    }
}

impl fmt::Display for DeployStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for DeployStrategy {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "rolling" => Ok(Self::Rolling),
            "canary" => Ok(Self::Canary),
            "ab_test" => Ok(Self::AbTest),
            other => anyhow::bail!("unknown deploy strategy: {other}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Analysis verdict
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisVerdict {
    Running,
    Pass,
    Fail,
    Inconclusive,
    Cancelled,
}

impl AnalysisVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Inconclusive => "inconclusive",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Running | Self::Inconclusive)
    }
}

impl fmt::Display for AnalysisVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for AnalysisVerdict {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "running" => Ok(Self::Running),
            "pass" => Ok(Self::Pass),
            "fail" => Ok(Self::Fail),
            "inconclusive" => Ok(Self::Inconclusive),
            "cancelled" => Ok(Self::Cancelled),
            other => anyhow::bail!("unknown analysis verdict: {other}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Release health
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseHealth {
    Unknown,
    Healthy,
    Degraded,
    Unhealthy,
}

impl ReleaseHealth {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Unhealthy => "unhealthy",
        }
    }
}

impl fmt::Display for ReleaseHealth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ReleaseHealth {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "unknown" => Ok(Self::Unknown),
            "healthy" => Ok(Self::Healthy),
            "degraded" => Ok(Self::Degraded),
            "unhealthy" => Ok(Self::Unhealthy),
            other => anyhow::bail!("unknown release health: {other}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Canary config (stored as JSONB in rollout_config)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CanaryRolloutConfig {
    pub stable_service: String,
    pub canary_service: String,
    pub steps: Vec<u32>,
    #[serde(default = "default_interval")]
    pub interval: u32,
    #[serde(default = "default_min_requests")]
    pub min_requests: u64,
    #[serde(default = "default_max_failures")]
    pub max_failures: u32,
    #[serde(default)]
    pub progress_gates: Vec<MetricGate>,
    #[serde(default)]
    pub rollback_triggers: Vec<MetricGate>,
}

fn default_interval() -> u32 {
    120
}
fn default_min_requests() -> u64 {
    100
}
fn default_max_failures() -> u32 {
    3
}

/// A metric gate for progress or rollback decisions.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MetricGate {
    pub metric: String,
    /// Custom metric name (when metric == "custom").
    #[serde(default)]
    pub name: Option<String>,
    pub condition: String,
    pub threshold: f64,
    /// Aggregation function (avg, sum, max, min, count). Default: avg.
    #[serde(default = "default_aggregation")]
    pub aggregation: String,
    /// Evaluation window in seconds. Default: 120.
    #[serde(default = "default_window")]
    pub window: i32,
}

fn default_aggregation() -> String {
    "avg".into()
}
fn default_window() -> i32 {
    120
}

// ---------------------------------------------------------------------------
// A/B test config (stored as JSONB in rollout_config)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AbTestRolloutConfig {
    pub control_service: String,
    pub treatment_service: String,
    #[serde(rename = "match")]
    pub match_rule: AbTestMatch,
    pub success_metric: String,
    pub success_condition: String,
    #[serde(default = "default_ab_duration")]
    pub duration: u64,
    #[serde(default = "default_ab_min_samples")]
    pub min_samples: u64,
}

fn default_ab_duration() -> u64 {
    86400
}
fn default_ab_min_samples() -> u64 {
    1000
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AbTestMatch {
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- ReleasePhase state machine --

    #[test]
    fn pending_can_progress() {
        assert!(ReleasePhase::Pending.can_transition_to(ReleasePhase::Progressing));
    }

    #[test]
    fn pending_can_cancel() {
        assert!(ReleasePhase::Pending.can_transition_to(ReleasePhase::Cancelled));
    }

    #[test]
    fn pending_can_complete_rolling_fast_path() {
        assert!(ReleasePhase::Pending.can_transition_to(ReleasePhase::Completed));
    }

    #[test]
    fn pending_cannot_promote() {
        assert!(!ReleasePhase::Pending.can_transition_to(ReleasePhase::Promoting));
    }

    #[test]
    fn progressing_can_hold() {
        assert!(ReleasePhase::Progressing.can_transition_to(ReleasePhase::Holding));
    }

    #[test]
    fn progressing_can_pause() {
        assert!(ReleasePhase::Progressing.can_transition_to(ReleasePhase::Paused));
    }

    #[test]
    fn progressing_can_promote() {
        assert!(ReleasePhase::Progressing.can_transition_to(ReleasePhase::Promoting));
    }

    #[test]
    fn progressing_can_rollback() {
        assert!(ReleasePhase::Progressing.can_transition_to(ReleasePhase::RollingBack));
    }

    #[test]
    fn holding_can_resume() {
        assert!(ReleasePhase::Holding.can_transition_to(ReleasePhase::Progressing));
    }

    #[test]
    fn holding_can_escalate_to_rollback() {
        assert!(ReleasePhase::Holding.can_transition_to(ReleasePhase::RollingBack));
    }

    #[test]
    fn paused_can_resume() {
        assert!(ReleasePhase::Paused.can_transition_to(ReleasePhase::Progressing));
    }

    #[test]
    fn paused_can_cancel() {
        assert!(ReleasePhase::Paused.can_transition_to(ReleasePhase::Cancelled));
    }

    #[test]
    fn paused_can_rollback() {
        assert!(ReleasePhase::Paused.can_transition_to(ReleasePhase::RollingBack));
    }

    #[test]
    fn promoting_can_complete() {
        assert!(ReleasePhase::Promoting.can_transition_to(ReleasePhase::Completed));
    }

    #[test]
    fn rolling_back_can_finish() {
        assert!(ReleasePhase::RollingBack.can_transition_to(ReleasePhase::RolledBack));
    }

    #[test]
    fn any_non_terminal_can_fail() {
        let non_terminal = [
            ReleasePhase::Pending,
            ReleasePhase::Progressing,
            ReleasePhase::Holding,
            ReleasePhase::Paused,
            ReleasePhase::Promoting,
            ReleasePhase::RollingBack,
        ];
        for phase in non_terminal {
            assert!(
                phase.can_transition_to(ReleasePhase::Failed),
                "{phase} should be able to fail"
            );
        }
    }

    #[test]
    fn terminal_states_cannot_transition() {
        let terminal = [
            ReleasePhase::Completed,
            ReleasePhase::RolledBack,
            ReleasePhase::Cancelled,
            ReleasePhase::Failed,
        ];
        let all = [
            ReleasePhase::Pending,
            ReleasePhase::Progressing,
            ReleasePhase::Holding,
            ReleasePhase::Paused,
            ReleasePhase::Promoting,
            ReleasePhase::Completed,
            ReleasePhase::RollingBack,
            ReleasePhase::RolledBack,
            ReleasePhase::Cancelled,
            ReleasePhase::Failed,
        ];
        for t in terminal {
            for next in all {
                assert!(
                    !t.can_transition_to(next),
                    "{t} should not transition to {next}"
                );
            }
        }
    }

    #[test]
    fn is_terminal_correct() {
        assert!(!ReleasePhase::Pending.is_terminal());
        assert!(!ReleasePhase::Progressing.is_terminal());
        assert!(!ReleasePhase::Holding.is_terminal());
        assert!(!ReleasePhase::Paused.is_terminal());
        assert!(!ReleasePhase::Promoting.is_terminal());
        assert!(!ReleasePhase::RollingBack.is_terminal());
        assert!(ReleasePhase::Completed.is_terminal());
        assert!(ReleasePhase::RolledBack.is_terminal());
        assert!(ReleasePhase::Cancelled.is_terminal());
        assert!(ReleasePhase::Failed.is_terminal());
    }

    // -- Roundtrips --

    #[test]
    fn release_phase_roundtrip() {
        let all = [
            "pending",
            "progressing",
            "holding",
            "paused",
            "promoting",
            "completed",
            "rolling_back",
            "rolled_back",
            "cancelled",
            "failed",
        ];
        for s in all {
            let phase: ReleasePhase = s.parse().unwrap();
            assert_eq!(phase.as_str(), s);
            assert_eq!(phase.to_string(), s);
        }
    }

    #[test]
    fn deploy_strategy_roundtrip() {
        for s in ["rolling", "canary", "ab_test"] {
            let strategy: DeployStrategy = s.parse().unwrap();
            assert_eq!(strategy.as_str(), s);
        }
    }

    #[test]
    fn analysis_verdict_roundtrip() {
        for s in ["running", "pass", "fail", "inconclusive", "cancelled"] {
            let v: AnalysisVerdict = s.parse().unwrap();
            assert_eq!(v.as_str(), s);
        }
    }

    #[test]
    fn release_health_roundtrip() {
        for s in ["unknown", "healthy", "degraded", "unhealthy"] {
            let h: ReleaseHealth = s.parse().unwrap();
            assert_eq!(h.as_str(), s);
        }
    }

    #[test]
    fn unknown_phase_errors() {
        assert!("bogus".parse::<ReleasePhase>().is_err());
    }

    #[test]
    fn unknown_strategy_errors() {
        assert!("bogus".parse::<DeployStrategy>().is_err());
    }

    // -- Serde --

    #[test]
    fn release_phase_serde_roundtrip() {
        let phase = ReleasePhase::RollingBack;
        let json = serde_json::to_string(&phase).unwrap();
        assert_eq!(json, "\"rolling_back\"");
        let parsed: ReleasePhase = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, phase);
    }

    #[test]
    fn deploy_strategy_serde_roundtrip() {
        let s = DeployStrategy::AbTest;
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "\"ab_test\"");
        let parsed: DeployStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, s);
    }

    // -- Canary config --

    #[test]
    fn canary_config_serde() {
        let config = CanaryRolloutConfig {
            stable_service: "api-stable".into(),
            canary_service: "api-canary".into(),
            steps: vec![5, 20, 50, 80, 100],
            interval: 120,
            min_requests: 100,
            max_failures: 3,
            progress_gates: vec![MetricGate {
                metric: "error_rate".into(),
                name: None,
                condition: "lt".into(),
                threshold: 0.05,
                aggregation: "avg".into(),
                window: 120,
            }],
            rollback_triggers: vec![MetricGate {
                metric: "error_rate".into(),
                name: None,
                condition: "gt".into(),
                threshold: 0.50,
                aggregation: "avg".into(),
                window: 120,
            }],
        };
        let json = serde_json::to_value(&config).unwrap();
        let parsed: CanaryRolloutConfig = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.steps, vec![5, 20, 50, 80, 100]);
        assert_eq!(parsed.progress_gates.len(), 1);
        assert_eq!(parsed.rollback_triggers.len(), 1);
    }

    #[test]
    fn canary_config_defaults() {
        let json = serde_json::json!({
            "stable_service": "s",
            "canary_service": "c",
            "steps": [10, 100],
        });
        let config: CanaryRolloutConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.interval, 120);
        assert_eq!(config.min_requests, 100);
        assert_eq!(config.max_failures, 3);
        assert!(config.progress_gates.is_empty());
        assert!(config.rollback_triggers.is_empty());
    }

    // -- A/B test config --

    #[test]
    fn ab_test_config_serde() {
        let json = serde_json::json!({
            "control_service": "checkout-control",
            "treatment_service": "checkout-treatment",
            "match": {
                "headers": {
                    "x-experiment": "treatment"
                }
            },
            "success_metric": "custom/conversion_rate",
            "success_condition": "gt",
        });
        let config: AbTestRolloutConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.control_service, "checkout-control");
        assert_eq!(config.duration, 86400);
        assert_eq!(config.min_samples, 1000);
        assert_eq!(
            config.match_rule.headers.get("x-experiment"),
            Some(&"treatment".to_string())
        );
    }

    // -- AnalysisVerdict --

    #[test]
    fn analysis_verdict_is_terminal() {
        assert!(!AnalysisVerdict::Running.is_terminal());
        assert!(AnalysisVerdict::Pass.is_terminal());
        assert!(AnalysisVerdict::Fail.is_terminal());
        assert!(!AnalysisVerdict::Inconclusive.is_terminal());
        assert!(AnalysisVerdict::Cancelled.is_terminal());
    }

    #[test]
    fn unknown_analysis_verdict_errors() {
        assert!("bogus".parse::<AnalysisVerdict>().is_err());
    }

    #[test]
    fn unknown_release_health_errors() {
        assert!("bogus".parse::<ReleaseHealth>().is_err());
    }

    #[test]
    fn analysis_verdict_display() {
        assert_eq!(AnalysisVerdict::Running.to_string(), "running");
        assert_eq!(AnalysisVerdict::Pass.to_string(), "pass");
        assert_eq!(AnalysisVerdict::Fail.to_string(), "fail");
        assert_eq!(AnalysisVerdict::Inconclusive.to_string(), "inconclusive");
        assert_eq!(AnalysisVerdict::Cancelled.to_string(), "cancelled");
    }

    #[test]
    fn release_health_display() {
        assert_eq!(ReleaseHealth::Unknown.to_string(), "unknown");
        assert_eq!(ReleaseHealth::Healthy.to_string(), "healthy");
        assert_eq!(ReleaseHealth::Degraded.to_string(), "degraded");
        assert_eq!(ReleaseHealth::Unhealthy.to_string(), "unhealthy");
    }

    #[test]
    fn deploy_strategy_display() {
        assert_eq!(DeployStrategy::Rolling.to_string(), "rolling");
        assert_eq!(DeployStrategy::Canary.to_string(), "canary");
        assert_eq!(DeployStrategy::AbTest.to_string(), "ab_test");
    }

    #[test]
    fn release_phase_display() {
        assert_eq!(ReleasePhase::Pending.to_string(), "pending");
        assert_eq!(ReleasePhase::RollingBack.to_string(), "rolling_back");
    }

    #[test]
    fn analysis_verdict_serde_roundtrip() {
        for v in [
            AnalysisVerdict::Running,
            AnalysisVerdict::Pass,
            AnalysisVerdict::Fail,
            AnalysisVerdict::Inconclusive,
            AnalysisVerdict::Cancelled,
        ] {
            let json = serde_json::to_string(&v).unwrap();
            let parsed: AnalysisVerdict = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, v);
        }
    }

    #[test]
    fn release_health_serde_roundtrip() {
        for h in [
            ReleaseHealth::Unknown,
            ReleaseHealth::Healthy,
            ReleaseHealth::Degraded,
            ReleaseHealth::Unhealthy,
        ] {
            let json = serde_json::to_string(&h).unwrap();
            let parsed: ReleaseHealth = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, h);
        }
    }

    // -- ReleasePhase parse edge cases --

    #[test]
    fn release_phase_parse_returns_none_for_unknown() {
        assert!(ReleasePhase::parse("nonexistent").is_none());
        assert!(ReleasePhase::parse("").is_none());
    }

    #[test]
    fn release_phase_parse_all_variants() {
        let variants = [
            ("pending", ReleasePhase::Pending),
            ("progressing", ReleasePhase::Progressing),
            ("holding", ReleasePhase::Holding),
            ("paused", ReleasePhase::Paused),
            ("promoting", ReleasePhase::Promoting),
            ("completed", ReleasePhase::Completed),
            ("rolling_back", ReleasePhase::RollingBack),
            ("rolled_back", ReleasePhase::RolledBack),
            ("cancelled", ReleasePhase::Cancelled),
            ("failed", ReleasePhase::Failed),
        ];
        for (s, expected) in variants {
            assert_eq!(ReleasePhase::parse(s), Some(expected), "parse({s})");
        }
    }

    // -- Negative transition tests --

    #[test]
    fn pending_cannot_hold() {
        assert!(!ReleasePhase::Pending.can_transition_to(ReleasePhase::Holding));
    }

    #[test]
    fn pending_cannot_rollback() {
        assert!(!ReleasePhase::Pending.can_transition_to(ReleasePhase::RollingBack));
    }

    #[test]
    fn pending_cannot_pause() {
        assert!(!ReleasePhase::Pending.can_transition_to(ReleasePhase::Paused));
    }

    #[test]
    fn pending_cannot_rolled_back() {
        assert!(!ReleasePhase::Pending.can_transition_to(ReleasePhase::RolledBack));
    }

    #[test]
    fn progressing_cannot_complete() {
        assert!(!ReleasePhase::Progressing.can_transition_to(ReleasePhase::Completed));
    }

    #[test]
    fn progressing_cannot_cancel() {
        assert!(!ReleasePhase::Progressing.can_transition_to(ReleasePhase::Cancelled));
    }

    #[test]
    fn progressing_cannot_rolled_back() {
        assert!(!ReleasePhase::Progressing.can_transition_to(ReleasePhase::RolledBack));
    }

    #[test]
    fn holding_cannot_complete() {
        assert!(!ReleasePhase::Holding.can_transition_to(ReleasePhase::Completed));
    }

    #[test]
    fn holding_cannot_pause() {
        assert!(!ReleasePhase::Holding.can_transition_to(ReleasePhase::Paused));
    }

    #[test]
    fn holding_cannot_cancel() {
        assert!(!ReleasePhase::Holding.can_transition_to(ReleasePhase::Cancelled));
    }

    #[test]
    fn holding_cannot_promote() {
        assert!(!ReleasePhase::Holding.can_transition_to(ReleasePhase::Promoting));
    }

    #[test]
    fn paused_cannot_hold() {
        assert!(!ReleasePhase::Paused.can_transition_to(ReleasePhase::Holding));
    }

    #[test]
    fn paused_cannot_complete() {
        assert!(!ReleasePhase::Paused.can_transition_to(ReleasePhase::Completed));
    }

    #[test]
    fn paused_cannot_promote() {
        assert!(!ReleasePhase::Paused.can_transition_to(ReleasePhase::Promoting));
    }

    #[test]
    fn promoting_cannot_rollback() {
        assert!(!ReleasePhase::Promoting.can_transition_to(ReleasePhase::RollingBack));
    }

    #[test]
    fn promoting_cannot_progress() {
        assert!(!ReleasePhase::Promoting.can_transition_to(ReleasePhase::Progressing));
    }

    #[test]
    fn rolling_back_cannot_progress() {
        assert!(!ReleasePhase::RollingBack.can_transition_to(ReleasePhase::Progressing));
    }

    #[test]
    fn rolling_back_cannot_complete() {
        assert!(!ReleasePhase::RollingBack.can_transition_to(ReleasePhase::Completed));
    }

    // -- MetricGate defaults --

    #[test]
    fn metric_gate_defaults() {
        let json = serde_json::json!({
            "metric": "error_rate",
            "condition": "lt",
            "threshold": 0.05,
        });
        let gate: MetricGate = serde_json::from_value(json).unwrap();
        assert_eq!(gate.aggregation, "avg");
        assert_eq!(gate.window, 120);
        assert!(gate.name.is_none());
    }

    #[test]
    fn metric_gate_with_custom_name() {
        let json = serde_json::json!({
            "metric": "custom",
            "name": "my_metric",
            "condition": "gt",
            "threshold": 100.0,
            "aggregation": "sum",
            "window": 300,
        });
        let gate: MetricGate = serde_json::from_value(json).unwrap();
        assert_eq!(gate.name.as_deref(), Some("my_metric"));
        assert_eq!(gate.aggregation, "sum");
        assert_eq!(gate.window, 300);
    }

    // -- AbTestRolloutConfig --

    #[test]
    fn ab_test_config_defaults() {
        let json = serde_json::json!({
            "control_service": "ctrl",
            "treatment_service": "treat",
            "match": {"headers": {}},
            "success_metric": "cr",
            "success_condition": "gt",
        });
        let config: AbTestRolloutConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.duration, 86400);
        assert_eq!(config.min_samples, 1000);
        assert!(config.match_rule.headers.is_empty());
    }

    #[test]
    fn ab_test_config_custom_duration_and_samples() {
        let json = serde_json::json!({
            "control_service": "c",
            "treatment_service": "t",
            "match": {"headers": {}},
            "success_metric": "m",
            "success_condition": "gt",
            "duration": 3600,
            "min_samples": 500,
        });
        let config: AbTestRolloutConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.duration, 3600);
        assert_eq!(config.min_samples, 500);
    }

    // -- CanaryRolloutConfig edge cases --

    #[test]
    fn canary_config_empty_steps() {
        let json = serde_json::json!({
            "stable_service": "s",
            "canary_service": "c",
            "steps": [],
        });
        let config: CanaryRolloutConfig = serde_json::from_value(json).unwrap();
        assert!(config.steps.is_empty());
    }

    #[test]
    fn canary_config_with_all_fields() {
        let json = serde_json::json!({
            "stable_service": "stable",
            "canary_service": "canary",
            "steps": [5, 10, 25, 50, 100],
            "interval": 60,
            "min_requests": 50,
            "max_failures": 5,
            "progress_gates": [{
                "metric": "latency_p99",
                "condition": "lt",
                "threshold": 200.0,
                "aggregation": "max",
                "window": 60,
            }],
            "rollback_triggers": [{
                "metric": "custom",
                "name": "panic_count",
                "condition": "gt",
                "threshold": 0.0,
                "aggregation": "count",
                "window": 300,
            }],
        });
        let config: CanaryRolloutConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.steps, vec![5, 10, 25, 50, 100]);
        assert_eq!(config.interval, 60);
        assert_eq!(config.min_requests, 50);
        assert_eq!(config.max_failures, 5);
        assert_eq!(config.progress_gates.len(), 1);
        assert_eq!(config.progress_gates[0].aggregation, "max");
        assert_eq!(config.rollback_triggers.len(), 1);
        assert_eq!(
            config.rollback_triggers[0].name.as_deref(),
            Some("panic_count")
        );
    }

    #[test]
    fn metric_gate_custom_name() {
        let json = serde_json::json!({
            "metric": "custom",
            "name": "custom/latency_p99",
            "condition": "lt",
            "threshold": 200.0,
            "aggregation": "max",
            "window": 300,
        });
        let gate: MetricGate = serde_json::from_value(json).unwrap();
        assert_eq!(gate.metric, "custom");
        assert_eq!(gate.name.as_deref(), Some("custom/latency_p99"));
        assert_eq!(gate.aggregation, "max");
        assert_eq!(gate.window, 300);
    }

    #[test]
    fn release_health_unknown_roundtrip() {
        let h: ReleaseHealth = "unknown".parse().unwrap();
        assert_eq!(h.as_str(), "unknown");
        assert_eq!(h.to_string(), "unknown");
    }

    #[test]
    fn release_health_unknown_parse_errors() {
        assert!("bogus".parse::<ReleaseHealth>().is_err());
    }

    #[test]
    fn analysis_verdict_parse_errors() {
        assert!("bogus".parse::<AnalysisVerdict>().is_err());
    }

    #[test]
    fn release_phase_parse_unknown_returns_none() {
        assert!(ReleasePhase::parse("unknown_phase").is_none());
        assert!(ReleasePhase::parse("").is_none());
    }

    #[test]
    fn promoting_cannot_hold() {
        assert!(!ReleasePhase::Promoting.can_transition_to(ReleasePhase::Holding));
    }

    #[test]
    fn release_health_all_variants_serde() {
        for (s, variant) in [
            ("unknown", ReleaseHealth::Unknown),
            ("healthy", ReleaseHealth::Healthy),
            ("degraded", ReleaseHealth::Degraded),
            ("unhealthy", ReleaseHealth::Unhealthy),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, format!("\"{s}\""));
            let parsed: ReleaseHealth = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant);
        }
    }

    #[test]
    fn canary_config_full_roundtrip() {
        let config = CanaryRolloutConfig {
            stable_service: "api-stable".into(),
            canary_service: "api-canary".into(),
            steps: vec![10, 50, 100],
            interval: 60,
            min_requests: 500,
            max_failures: 5,
            progress_gates: vec![
                MetricGate {
                    metric: "error_rate".into(),
                    name: None,
                    condition: "lt".into(),
                    threshold: 0.01,
                    aggregation: "avg".into(),
                    window: 120,
                },
                MetricGate {
                    metric: "custom".into(),
                    name: Some("latency_p99".into()),
                    condition: "lt".into(),
                    threshold: 200.0,
                    aggregation: "max".into(),
                    window: 300,
                },
            ],
            rollback_triggers: vec![],
        };
        let json = serde_json::to_value(&config).unwrap();
        let parsed: CanaryRolloutConfig = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.stable_service, "api-stable");
        assert_eq!(parsed.canary_service, "api-canary");
        assert_eq!(parsed.steps, vec![10, 50, 100]);
        assert_eq!(parsed.interval, 60);
        assert_eq!(parsed.min_requests, 500);
        assert_eq!(parsed.max_failures, 5);
        assert_eq!(parsed.progress_gates.len(), 2);
        assert_eq!(
            parsed.progress_gates[1].name.as_deref(),
            Some("latency_p99")
        );
    }

    #[test]
    fn ab_test_config_custom_duration() {
        let json = serde_json::json!({
            "control_service": "ctrl",
            "treatment_service": "treat",
            "match": { "headers": {} },
            "success_metric": "conversion_rate",
            "success_condition": "gt",
            "duration": 3600,
            "min_samples": 50,
        });
        let config: AbTestRolloutConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.duration, 3600);
        assert_eq!(config.min_samples, 50);
    }

    #[test]
    fn ab_test_match_empty_headers() {
        let json = serde_json::json!({ "headers": {} });
        let m: AbTestMatch = serde_json::from_value(json).unwrap();
        assert!(m.headers.is_empty());
    }

    #[test]
    fn ab_test_match_multiple_headers() {
        let json = serde_json::json!({
            "headers": {
                "x-experiment": "treatment",
                "x-variant": "B"
            }
        });
        let m: AbTestMatch = serde_json::from_value(json).unwrap();
        assert_eq!(m.headers.len(), 2);
        assert_eq!(m.headers.get("x-experiment"), Some(&"treatment".into()));
        assert_eq!(m.headers.get("x-variant"), Some(&"B".into()));
    }
}
