// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Platform event types for Valkey pub/sub communication.
//!
//! These types define the contract for inter-module communication via the
//! `platform:events` Valkey channel. Domain crates depend on these types
//! (not on each other) to publish or consume events.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The Valkey channel used for all platform events.
pub const EVENTS_CHANNEL: &str = "platform:events";

/// All platform events published via Valkey pub/sub.
///
/// Each variant is serialized as `{"type": "VariantName", ...fields}` using
/// `#[serde(tag = "type")]`. Subscribers deserialize and dispatch by variant.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PlatformEvent {
    /// A pipeline produced a new container image.
    ImageBuilt {
        project_id: Uuid,
        environment: String,
        image_ref: String,
        pipeline_id: Uuid,
        triggered_by: Option<Uuid>,
    },
    /// The ops repo was updated with new values (image ref, etc.).
    OpsRepoUpdated {
        project_id: Uuid,
        ops_repo_id: Uuid,
        environment: String,
        commit_sha: String,
        image_ref: String,
    },
    /// A deployment was requested via the API (manual trigger).
    DeployRequested {
        project_id: Uuid,
        environment: String,
        image_ref: String,
        requested_by: Option<Uuid>,
    },
    /// A rollback was requested via the API.
    RollbackRequested {
        project_id: Uuid,
        environment: String,
        requested_by: Option<Uuid>,
    },
    /// A pipeline built a custom dev image from `Dockerfile.dev`.
    DevImageBuilt {
        project_id: Uuid,
        image_ref: String,
        pipeline_id: Uuid,
    },
    /// An alert rule fired (condition held for `for_seconds`).
    AlertFired {
        rule_id: Uuid,
        project_id: Option<Uuid>,
        severity: String,
        value: Option<f64>,
        message: String,
        alert_name: String,
    },
    /// A release was created (for reconciler wake-up).
    ReleaseCreated {
        target_id: Uuid,
        release_id: Uuid,
        project_id: Uuid,
        image_ref: String,
        strategy: String,
    },
    /// A release was promoted (canary → 100% or staging → prod).
    ReleasePromoted {
        release_id: Uuid,
        project_id: Uuid,
        image_ref: String,
    },
    /// A release was rolled back.
    ReleaseRolledBack {
        release_id: Uuid,
        project_id: Uuid,
        reason: String,
    },
    /// Traffic weights were shifted on a release.
    TrafficShifted {
        release_id: Uuid,
        project_id: Uuid,
        weights: HashMap<String, u32>,
    },
    /// Feature flags registered from pipeline (key, `default_value`, description).
    FlagsRegistered {
        project_id: Uuid,
        flags: Vec<(String, serde_json::Value, Option<String>)>,
    },
    /// A pipeline was created and is queued for execution.
    PipelineQueued { pipeline_id: Uuid, project_id: Uuid },
    /// A code branch was pushed (triggers pipeline, webhooks, MR sync).
    CodePushed {
        project_id: Uuid,
        user_id: Uuid,
        user_name: String,
        repo_path: PathBuf,
        branch: String,
        commit_sha: Option<String>,
    },
    /// A tag was pushed (triggers tag pipeline + webhooks).
    TagPushed {
        project_id: Uuid,
        user_id: Uuid,
        user_name: String,
        repo_path: PathBuf,
        tag_name: String,
        commit_sha: Option<String>,
    },
    /// An MR's source branch was updated (triggers MR pipeline, dismisses stale reviews).
    MrBranchSynced {
        project_id: Uuid,
        user_id: Uuid,
        repo_path: PathBuf,
        branch: String,
        commit_sha: Option<String>,
    },
}

/// Publish a [`PlatformEvent`] to the Valkey event bus.
pub async fn publish(valkey: &fred::clients::Pool, event: &PlatformEvent) -> anyhow::Result<()> {
    use fred::interfaces::PubsubInterface;
    let json = serde_json::to_string(event)?;
    valkey
        .next()
        .publish::<(), _, _>(EVENTS_CHANNEL, json)
        .await?;
    Ok(())
}

/// Well-known Valkey channel name helpers.
pub mod channels {
    use uuid::Uuid;

    /// Main event bus channel.
    pub const EVENTS: &str = "platform:events";

    /// Per-project log streaming channel.
    pub fn logs(project_id: Uuid) -> String {
        format!("logs:{project_id}")
    }

    /// Per-session event streaming channel (agent → UI).
    pub fn session_events(session_id: Uuid) -> String {
        format!("session:{session_id}:events")
    }

    /// Per-session input channel (UI → agent).
    pub fn session_input(session_id: Uuid) -> String {
        format!("session:{session_id}:input")
    }

    /// Health status streaming channel.
    pub const HEALTH_STREAM: &str = "health:stream";

    /// Per-project per-rule alert agent channel.
    pub fn alert_agent(project_id: Uuid, rule_id: Uuid) -> String {
        format!("alert-agent:{project_id}:{rule_id}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_event_serde_roundtrip_image_built() {
        let event = PlatformEvent::ImageBuilt {
            project_id: Uuid::nil(),
            environment: "production".into(),
            image_ref: "registry/app:v1".into(),
            pipeline_id: Uuid::nil(),
            triggered_by: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"ImageBuilt""#));
        let parsed: PlatformEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, PlatformEvent::ImageBuilt { .. }));
    }

    #[test]
    fn platform_event_serde_roundtrip_alert_fired() {
        let event = PlatformEvent::AlertFired {
            rule_id: Uuid::nil(),
            project_id: Some(Uuid::nil()),
            severity: "critical".into(),
            value: Some(42.0),
            message: "CPU high".into(),
            alert_name: "high_cpu".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: PlatformEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, PlatformEvent::AlertFired { .. }));
    }

    #[test]
    fn platform_event_serde_roundtrip_traffic_shifted() {
        let mut weights = HashMap::new();
        weights.insert("canary".into(), 10);
        weights.insert("stable".into(), 90);
        let event = PlatformEvent::TrafficShifted {
            release_id: Uuid::nil(),
            project_id: Uuid::nil(),
            weights,
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: PlatformEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, PlatformEvent::TrafficShifted { .. }));
    }

    #[test]
    fn platform_event_serde_roundtrip_flags_registered() {
        let event = PlatformEvent::FlagsRegistered {
            project_id: Uuid::nil(),
            flags: vec![(
                "dark_mode".into(),
                serde_json::Value::Bool(false),
                Some("Toggle dark mode".into()),
            )],
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: PlatformEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, PlatformEvent::FlagsRegistered { .. }));
    }

    #[test]
    fn channel_helpers() {
        let pid = Uuid::nil();
        let sid = Uuid::nil();
        let rid = Uuid::nil();

        assert_eq!(channels::EVENTS, "platform:events");
        assert_eq!(channels::logs(pid), format!("logs:{pid}"));
        assert_eq!(
            channels::session_events(sid),
            format!("session:{sid}:events")
        );
        assert_eq!(channels::session_input(sid), format!("session:{sid}:input"));
        assert_eq!(channels::HEALTH_STREAM, "health:stream");
        assert_eq!(
            channels::alert_agent(pid, rid),
            format!("alert-agent:{pid}:{rid}")
        );
    }

    #[test]
    fn all_event_variants_serialize() {
        let events: Vec<PlatformEvent> = vec![
            PlatformEvent::ImageBuilt {
                project_id: Uuid::nil(),
                environment: "dev".into(),
                image_ref: "img:1".into(),
                pipeline_id: Uuid::nil(),
                triggered_by: None,
            },
            PlatformEvent::OpsRepoUpdated {
                project_id: Uuid::nil(),
                ops_repo_id: Uuid::nil(),
                environment: "dev".into(),
                commit_sha: "abc".into(),
                image_ref: "img:1".into(),
            },
            PlatformEvent::DeployRequested {
                project_id: Uuid::nil(),
                environment: "dev".into(),
                image_ref: "img:1".into(),
                requested_by: None,
            },
            PlatformEvent::RollbackRequested {
                project_id: Uuid::nil(),
                environment: "dev".into(),
                requested_by: None,
            },
            PlatformEvent::DevImageBuilt {
                project_id: Uuid::nil(),
                image_ref: "img:1".into(),
                pipeline_id: Uuid::nil(),
            },
            PlatformEvent::AlertFired {
                rule_id: Uuid::nil(),
                project_id: None,
                severity: "info".into(),
                value: None,
                message: "msg".into(),
                alert_name: "name".into(),
            },
            PlatformEvent::ReleaseCreated {
                target_id: Uuid::nil(),
                release_id: Uuid::nil(),
                project_id: Uuid::nil(),
                image_ref: "img:1".into(),
                strategy: "rolling".into(),
            },
            PlatformEvent::ReleasePromoted {
                release_id: Uuid::nil(),
                project_id: Uuid::nil(),
                image_ref: "img:1".into(),
            },
            PlatformEvent::ReleaseRolledBack {
                release_id: Uuid::nil(),
                project_id: Uuid::nil(),
                reason: "failed".into(),
            },
            PlatformEvent::TrafficShifted {
                release_id: Uuid::nil(),
                project_id: Uuid::nil(),
                weights: HashMap::new(),
            },
            PlatformEvent::FlagsRegistered {
                project_id: Uuid::nil(),
                flags: vec![],
            },
            PlatformEvent::PipelineQueued {
                pipeline_id: Uuid::nil(),
                project_id: Uuid::nil(),
            },
            PlatformEvent::CodePushed {
                project_id: Uuid::nil(),
                user_id: Uuid::nil(),
                user_name: "alice".into(),
                repo_path: PathBuf::from("/repos/test.git"),
                branch: "main".into(),
                commit_sha: Some("abc123".into()),
            },
            PlatformEvent::TagPushed {
                project_id: Uuid::nil(),
                user_id: Uuid::nil(),
                user_name: "alice".into(),
                repo_path: PathBuf::from("/repos/test.git"),
                tag_name: "v1.0.0".into(),
                commit_sha: None,
            },
            PlatformEvent::MrBranchSynced {
                project_id: Uuid::nil(),
                user_id: Uuid::nil(),
                repo_path: PathBuf::from("/repos/test.git"),
                branch: "feature/login".into(),
                commit_sha: Some("abc123".into()),
            },
        ];
        for event in &events {
            let json = serde_json::to_string(event).unwrap();
            let _parsed: PlatformEvent = serde_json::from_str(&json).unwrap();
        }
    }

    #[test]
    fn platform_event_serde_roundtrip_code_pushed() {
        let event = PlatformEvent::CodePushed {
            project_id: Uuid::nil(),
            user_id: Uuid::nil(),
            user_name: "alice".into(),
            repo_path: PathBuf::from("/repos/test.git"),
            branch: "feature/login".into(),
            commit_sha: Some("abc123def456".into()),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"CodePushed""#));
        let parsed: PlatformEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            PlatformEvent::CodePushed {
                branch,
                commit_sha,
                user_name,
                ..
            } => {
                assert_eq!(branch, "feature/login");
                assert_eq!(commit_sha.as_deref(), Some("abc123def456"));
                assert_eq!(user_name, "alice");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn platform_event_serde_roundtrip_code_pushed_no_sha() {
        let event = PlatformEvent::CodePushed {
            project_id: Uuid::nil(),
            user_id: Uuid::nil(),
            user_name: "bob".into(),
            repo_path: PathBuf::from("/repos/test.git"),
            branch: "main".into(),
            commit_sha: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: PlatformEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            PlatformEvent::CodePushed { commit_sha, .. } => {
                assert!(commit_sha.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn platform_event_serde_roundtrip_tag_pushed() {
        let event = PlatformEvent::TagPushed {
            project_id: Uuid::nil(),
            user_id: Uuid::nil(),
            user_name: "alice".into(),
            repo_path: PathBuf::from("/repos/test.git"),
            tag_name: "v2.0.0-rc.1".into(),
            commit_sha: Some("def456".into()),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"TagPushed""#));
        let parsed: PlatformEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            PlatformEvent::TagPushed {
                tag_name,
                commit_sha,
                user_name,
                ..
            } => {
                assert_eq!(tag_name, "v2.0.0-rc.1");
                assert_eq!(commit_sha.as_deref(), Some("def456"));
                assert_eq!(user_name, "alice");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn platform_event_serde_roundtrip_tag_pushed_no_sha() {
        let event = PlatformEvent::TagPushed {
            project_id: Uuid::nil(),
            user_id: Uuid::nil(),
            user_name: "bob".into(),
            repo_path: PathBuf::from("/repos/test.git"),
            tag_name: "v1.0.0".into(),
            commit_sha: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: PlatformEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            PlatformEvent::TagPushed { commit_sha, .. } => {
                assert!(commit_sha.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn platform_event_serde_roundtrip_mr_branch_synced() {
        let event = PlatformEvent::MrBranchSynced {
            project_id: Uuid::nil(),
            user_id: Uuid::nil(),
            repo_path: PathBuf::from("/repos/test.git"),
            branch: "feature/login".into(),
            commit_sha: Some("abc123def456".into()),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"MrBranchSynced""#));
        let parsed: PlatformEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            PlatformEvent::MrBranchSynced {
                branch, commit_sha, ..
            } => {
                assert_eq!(branch, "feature/login");
                assert_eq!(commit_sha.as_deref(), Some("abc123def456"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn platform_event_serde_roundtrip_pipeline_queued() {
        let event = PlatformEvent::PipelineQueued {
            pipeline_id: Uuid::nil(),
            project_id: Uuid::nil(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"PipelineQueued""#));
        let parsed: PlatformEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, PlatformEvent::PipelineQueued { .. }));
    }

    #[test]
    fn platform_event_serde_roundtrip_mr_branch_synced_no_sha() {
        let event = PlatformEvent::MrBranchSynced {
            project_id: Uuid::nil(),
            user_id: Uuid::nil(),
            repo_path: PathBuf::from("/repos/test.git"),
            branch: "main".into(),
            commit_sha: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: PlatformEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            PlatformEvent::MrBranchSynced { commit_sha, .. } => {
                assert!(commit_sha.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }
}
