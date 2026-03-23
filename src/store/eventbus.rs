//! Valkey-based internal event bus for platform events.
//!
//! Events are published as JSON to a Valkey pub/sub channel. A background
//! subscriber loop dispatches events to typed handlers.

use std::path::PathBuf;

use fred::interfaces::PubsubInterface;
use fred::prelude::*;
use serde::{Deserialize, Serialize};
use tracing::Instrument;
use uuid::Uuid;

use crate::store::AppState;

/// The Valkey channel used for all platform events.
const CHANNEL: &str = "platform:events";

// ---------------------------------------------------------------------------
// Event types
// ---------------------------------------------------------------------------

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
        weights: std::collections::HashMap<String, u32>,
    },
    /// Feature flags registered from pipeline (key + `default_value`).
    FlagsRegistered {
        project_id: Uuid,
        flags: Vec<(String, serde_json::Value)>,
    },
}

// ---------------------------------------------------------------------------
// Publisher
// ---------------------------------------------------------------------------

/// Publish an event to the platform event bus.
pub async fn publish(valkey: &fred::clients::Pool, event: &PlatformEvent) -> anyhow::Result<()> {
    let json = serde_json::to_string(event)?;
    valkey.next().publish::<(), _, _>(CHANNEL, json).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Subscriber background loop
// ---------------------------------------------------------------------------

/// Background task: subscribe to platform events and dispatch to handlers.
pub async fn run(state: AppState, mut shutdown: tokio::sync::watch::Receiver<()>) {
    tracing::info!("event bus subscriber started");

    // Create a dedicated subscriber client from the pool's config
    let subscriber = state.valkey.next().clone_new();
    if let Err(e) = subscriber.init().await {
        tracing::error!(error = %e, "failed to init event bus subscriber");
        return;
    }

    if let Err(e) = subscriber.subscribe(CHANNEL).await {
        tracing::error!(error = %e, "failed to subscribe to {CHANNEL}");
        return;
    }

    let mut message_rx = subscriber.message_rx();
    state.task_registry.register("event_bus", 30);

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                tracing::info!("event bus subscriber shutting down");
                let _ = subscriber.unsubscribe(CHANNEL).await;
                break;
            }
            msg = message_rx.recv() => {
                match msg {
                    Ok(message) => {
                        state.task_registry.heartbeat("event_bus");
                        let payload: String = match message.value.convert() {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::warn!(error = %e, "failed to convert message payload");
                                continue;
                            }
                        };
                        let state = state.clone();
                        let iter_trace_id = uuid::Uuid::new_v4().to_string().replace('-', "");
                        let span = tracing::info_span!(
                            "task_iteration",
                            task_name = "event_bus",
                            trace_id = %iter_trace_id,
                            source = "system",
                        );
                        tokio::spawn(async move {
                            if let Err(e) = handle_event(&state, &payload).await {
                                tracing::error!(error = %e, "event handler failed");
                            }
                        }.instrument(span));
                    }
                    Err(e) => {
                        state.task_registry.report_error("event_bus", &e.to_string());
                        tracing::error!(error = %e, "event bus recv error");
                        // Reconnect pause
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Event dispatch
// ---------------------------------------------------------------------------

pub async fn handle_event(state: &AppState, payload: &str) -> anyhow::Result<()> {
    let event: PlatformEvent = serde_json::from_str(payload)?;
    tracing::debug!(?event, "handling platform event");

    match event {
        PlatformEvent::ImageBuilt {
            project_id,
            environment,
            image_ref,
            pipeline_id,
            triggered_by,
        } => {
            handle_image_built(
                state,
                project_id,
                &environment,
                &image_ref,
                pipeline_id,
                triggered_by,
            )
            .await
        }
        PlatformEvent::OpsRepoUpdated {
            project_id,
            environment,
            commit_sha,
            image_ref,
            ..
        } => {
            handle_ops_repo_updated(state, project_id, &environment, &commit_sha, &image_ref).await
        }
        PlatformEvent::DeployRequested {
            project_id,
            environment,
            image_ref,
            requested_by,
        } => {
            handle_deploy_requested(state, project_id, &environment, &image_ref, requested_by).await
        }
        PlatformEvent::RollbackRequested {
            project_id,
            environment,
            requested_by,
        } => handle_rollback_requested(state, project_id, &environment, requested_by).await,
        PlatformEvent::DevImageBuilt {
            project_id,
            image_ref,
            pipeline_id,
        } => handle_dev_image_built(state, project_id, &image_ref, pipeline_id).await,
        PlatformEvent::AlertFired {
            rule_id,
            project_id,
            severity,
            value,
            message,
            alert_name,
        } => {
            handle_alert_fired(
                state,
                rule_id,
                project_id,
                &severity,
                value,
                &message,
                &alert_name,
            )
            .await
        }
        // New progressive delivery events — wake reconciler, no special handler needed
        PlatformEvent::ReleaseCreated { .. }
        | PlatformEvent::ReleasePromoted { .. }
        | PlatformEvent::ReleaseRolledBack { .. }
        | PlatformEvent::TrafficShifted { .. } => {
            state.deploy_notify.notify_one();
            Ok(())
        }
        PlatformEvent::FlagsRegistered { project_id, flags } => {
            handle_flags_registered(state, project_id, &flags).await
        }
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Legacy: pipeline now writes to ops repo directly and publishes `OpsRepoUpdated`.
/// This handler is kept for backward compatibility but is a no-op.
#[allow(clippy::unused_async)]
async fn handle_image_built(
    _state: &AppState,
    _project_id: Uuid,
    _environment: &str,
    _image_ref: &str,
    _pipeline_id: Uuid,
    _triggered_by: Option<Uuid>,
) -> anyhow::Result<()> {
    tracing::debug!("ImageBuilt event received (legacy, no-op)");
    Ok(())
}

/// Upsert a deploy target for (project, environment). Returns the target ID.
async fn upsert_deploy_target_simple(
    state: &AppState,
    project_id: Uuid,
    environment: &str,
    ops_repo_id: Option<Uuid>,
) -> anyhow::Result<Uuid> {
    let existing = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM deploy_targets
         WHERE project_id = $1 AND environment = $2 AND branch_slug IS NULL AND is_active = true",
    )
    .bind(project_id)
    .bind(environment)
    .fetch_optional(&state.pool)
    .await?;

    if let Some(id) = existing {
        if let Some(ops_id) = ops_repo_id {
            sqlx::query(
                "UPDATE deploy_targets SET ops_repo_id = COALESCE(ops_repo_id, $2) WHERE id = $1",
            )
            .bind(id)
            .bind(ops_id)
            .execute(&state.pool)
            .await?;
        }
        return Ok(id);
    }

    let id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO deploy_targets (project_id, name, environment, ops_repo_id)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (project_id, environment, branch_slug) DO UPDATE SET
             ops_repo_id = COALESCE(deploy_targets.ops_repo_id, $4)
         RETURNING id",
    )
    .bind(project_id)
    .bind(environment)
    .bind(environment)
    .bind(ops_repo_id)
    .fetch_one(&state.pool)
    .await?;

    Ok(id)
}

/// Resolve deploy config (strategy + `rollout_config`) from parsed platform file specs.
fn resolve_deploy_config_from_specs(
    pf: &crate::pipeline::definition::PlatformFile,
) -> (serde_json::Value, Option<String>) {
    let Some(ref deploy) = pf.deploy else {
        return (serde_json::json!({}), None);
    };

    if let Some(spec) = deploy.specs.first() {
        let strategy = spec.deploy_type.clone();
        let config = if let Some(ref canary) = spec.canary {
            serde_json::to_value(canary).unwrap_or_default()
        } else if let Some(ref ab) = spec.ab_test {
            serde_json::to_value(ab).unwrap_or_default()
        } else {
            serde_json::json!({})
        };
        (config, Some(strategy))
    } else {
        (serde_json::json!({}), None)
    }
}

/// Ops repo was updated → read platform.yaml → create release with strategy → register flags → wake deployer.
async fn handle_ops_repo_updated(
    state: &AppState,
    project_id: Uuid,
    environment: &str,
    commit_sha: &str,
    image_ref: &str,
) -> anyhow::Result<()> {
    // 1. Read platform.yaml from ops repo (for deploy specs + flags)
    let ops_repo = sqlx::query!(
        "SELECT id, repo_path, branch FROM ops_repos WHERE project_id = $1",
        project_id,
    )
    .fetch_optional(&state.pool)
    .await?;

    let platform_file = if let Some(ref ops) = ops_repo {
        let ops_path = std::path::PathBuf::from(&ops.repo_path);
        let branch = if environment == "staging" {
            "staging"
        } else {
            &ops.branch
        };
        match crate::deployer::ops_repo::read_file_at_ref(&ops_path, branch, "platform.yaml").await
        {
            Ok(content) => serde_yaml::from_str(&content).ok(),
            Err(_) => None,
        }
    } else {
        None
    };

    // 2. Resolve deploy config (strategy + rollout_config) from platform.yaml specs
    let (rollout_config, strategy_override) = if let Some(ref pf) = platform_file {
        resolve_deploy_config_from_specs(pf)
    } else {
        (serde_json::json!({}), None)
    };

    // 3. Find or create deploy target
    let ops_repo_id = ops_repo.as_ref().map(|o| o.id);
    let target_id =
        upsert_deploy_target_simple(state, project_id, environment, ops_repo_id).await?;

    // 4. Create release with strategy + rollout_config
    let release_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO deploy_releases (target_id, project_id, image_ref, commit_sha, strategy, rollout_config)
         VALUES ($1, $2, $3, $4, COALESCE($5, (SELECT default_strategy FROM deploy_targets WHERE id = $1)), $6)
         RETURNING id",
    )
    .bind(target_id)
    .bind(project_id)
    .bind(image_ref)
    .bind(commit_sha)
    .bind(&strategy_override)
    .bind(&rollout_config)
    .fetch_one(&state.pool)
    .await?;

    // 5. Register feature flags from platform.yaml
    if let Some(ref pf) = platform_file
        && !pf.flags.is_empty()
    {
        let flag_defs: Vec<(String, serde_json::Value)> = pf
            .flags
            .iter()
            .map(|f| (f.key.clone(), f.default_value.clone()))
            .collect();
        handle_flags_registered_inner(state, project_id, &flag_defs).await;
    }

    // 6. Wake reconciler
    state.deploy_notify.notify_one();

    tracing::info!(%project_id, %environment, %image_ref, %release_id, "release created from ops repo update");
    Ok(())
}

/// Manual deploy request → commit to ops repo → publish `OpsRepoUpdated`.
async fn handle_deploy_requested(
    state: &AppState,
    project_id: Uuid,
    environment: &str,
    image_ref: &str,
    _requested_by: Option<Uuid>,
) -> anyhow::Result<()> {
    let ops_repo = sqlx::query!(
        "SELECT id, repo_path, branch FROM ops_repos WHERE project_id = $1",
        project_id,
    )
    .fetch_optional(&state.pool)
    .await?;

    let Some(ops) = ops_repo else {
        tracing::warn!(%project_id, "deploy_requested: no ops repo found");
        return Ok(());
    };

    let project_name = sqlx::query_scalar!("SELECT name FROM projects WHERE id = $1", project_id)
        .fetch_optional(&state.pool)
        .await?
        .unwrap_or_default();

    let ops_path = PathBuf::from(&ops.repo_path);
    let values = serde_json::json!({
        "image_ref": image_ref,
        "project_name": project_name,
        "environment": environment,
    });

    let commit_sha =
        crate::deployer::ops_repo::commit_values(&ops_path, &ops.branch, environment, &values)
            .await?;

    let _ = publish(
        &state.valkey,
        &PlatformEvent::OpsRepoUpdated {
            project_id,
            ops_repo_id: ops.id,
            environment: environment.into(),
            commit_sha,
            image_ref: image_ref.into(),
        },
    )
    .await;

    Ok(())
}

/// Rollback request → revert ops repo commit → publish `OpsRepoUpdated`.
async fn handle_rollback_requested(
    state: &AppState,
    project_id: Uuid,
    environment: &str,
    _requested_by: Option<Uuid>,
) -> anyhow::Result<()> {
    let ops_repo = sqlx::query!(
        "SELECT id, repo_path, branch FROM ops_repos WHERE project_id = $1",
        project_id,
    )
    .fetch_optional(&state.pool)
    .await?;

    let Some(ops) = ops_repo else {
        tracing::warn!(%project_id, "rollback: no ops repo found");
        return Ok(());
    };

    let ops_path = PathBuf::from(&ops.repo_path);
    let new_sha = crate::deployer::ops_repo::revert_last_commit(&ops_path, &ops.branch).await?;

    let reverted_values =
        crate::deployer::ops_repo::read_values(&ops_path, &ops.branch, environment).await?;
    let old_image = reverted_values["image_ref"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();

    tracing::info!(%project_id, %environment, %old_image, %new_sha, "ops repo reverted for rollback");

    let _ = publish(
        &state.valkey,
        &PlatformEvent::OpsRepoUpdated {
            project_id,
            ops_repo_id: ops.id,
            environment: environment.into(),
            commit_sha: new_sha,
            image_ref: old_image,
        },
    )
    .await;

    Ok(())
}

/// A dev image was built from `Dockerfile.dev` → update the project's `agent_image`.
#[tracing::instrument(skip(state), fields(%project_id, %pipeline_id), err)]
async fn handle_dev_image_built(
    state: &AppState,
    project_id: Uuid,
    image_ref: &str,
    pipeline_id: Uuid,
) -> anyhow::Result<()> {
    let result = sqlx::query!(
        "UPDATE projects SET agent_image = $2, updated_at = now() WHERE id = $1 AND is_active = true",
        project_id,
        image_ref,
    )
    .execute(&state.pool)
    .await?;

    if result.rows_affected() == 0 {
        tracing::warn!(%project_id, "dev_image_built: project not found or inactive");
        return Ok(());
    }

    tracing::info!(
        %project_id,
        %image_ref,
        %pipeline_id,
        "project agent_image updated from Dockerfile.dev build"
    );

    Ok(())
}

/// An alert fired → optionally spawn an ops agent to investigate.
///
/// Rate limiting:
/// 1. Skip non-project alerts (global alerts don't spawn agents)
/// 2. Severity gate: only `critical` and `warning` spawn agents
/// 3. Per-alert cooldown: 15-minute TTL in Valkey prevents duplicate spawns
/// 4. Per-project concurrent limit: max 3 active ops sessions
#[tracing::instrument(skip(state), fields(%rule_id, ?project_id, %severity), err)]
async fn handle_alert_fired(
    state: &AppState,
    rule_id: Uuid,
    project_id: Option<Uuid>,
    severity: &str,
    value: Option<f64>,
    message: &str,
    alert_name: &str,
) -> anyhow::Result<()> {
    // 1. Skip non-project alerts
    let Some(project_id) = project_id else {
        tracing::debug!(%rule_id, "alert has no project_id, skipping ops agent spawn");
        return Ok(());
    };

    // 2. Severity gate — only warning/critical spawn agents
    if severity != "critical" && severity != "warning" {
        tracing::debug!(%rule_id, %severity, "alert severity below threshold, skipping ops agent");
        return Ok(());
    }

    // 3. Per-alert cooldown (15 min)
    let cooldown_key = format!("alert-agent:{project_id}:{rule_id}");
    let exists: bool = state.valkey.next().exists(&cooldown_key).await?;
    if exists {
        tracing::debug!(%rule_id, %project_id, "alert agent cooldown active, skipping");
        return Ok(());
    }

    // 4. Per-project concurrent limit (max 3 ops agents)
    // Count sessions that are either still provisioning (agent_user_id IS NULL)
    // or have an active agent user with the ops role.
    let active_ops: Option<i64> = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_sessions s
         WHERE s.project_id = $1 AND s.status IN ('pending', 'running')
         AND (
             s.agent_user_id IS NULL
             OR EXISTS (
                 SELECT 1 FROM user_roles ur
                 JOIN roles r ON r.id = ur.role_id
                 WHERE ur.user_id = s.agent_user_id AND r.name = 'agent-ops'
             )
         )",
    )
    .bind(project_id)
    .fetch_one(&state.pool)
    .await?;

    if active_ops.unwrap_or(0) >= 3 {
        tracing::warn!(%project_id, active_ops, "ops agent concurrent limit reached, skipping");
        return Ok(());
    }

    // 5. Attempt spawn (admin lookup, cooldown set, agent creation)
    spawn_ops_agent(
        state, project_id, rule_id, alert_name, severity, value, message,
    )
    .await
}

/// Look up admin user, set cooldown, and spawn the ops agent pod.
/// Separated from `handle_alert_fired` to keep both under clippy's line limit.
#[tracing::instrument(skip(state, message), fields(%project_id, %rule_id), err)]
async fn spawn_ops_agent(
    state: &AppState,
    project_id: Uuid,
    rule_id: Uuid,
    alert_name: &str,
    severity: &str,
    value: Option<f64>,
    message: &str,
) -> anyhow::Result<()> {
    let cooldown_key = format!("alert-agent:{project_id}:{rule_id}");
    // Look up admin user as spawner (ops agents are system-initiated)
    let admin_id: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM users WHERE name = 'admin' AND is_active = true")
            .fetch_optional(&state.pool)
            .await?;

    let Some(admin_id) = admin_id else {
        tracing::error!("no active admin user found, cannot spawn ops agent");
        return Ok(());
    };

    // Set cooldown atomically via NX — if key already exists, another handler won
    let was_set: Option<String> = state
        .valkey
        .next()
        .set(
            &cooldown_key,
            "1",
            Some(fred::types::Expiration::EX(900)),
            Some(fred::types::SetOptions::NX),
            false,
        )
        .await?;

    if was_set.is_none() {
        tracing::debug!(%rule_id, %project_id, "cooldown race lost, another handler is spawning");
        return Ok(());
    }

    // Build agent prompt
    let value_str = value.map_or("absent".to_string(), |v| format!("{v}"));
    let prompt = format!(
        "Alert '{alert_name}' fired (severity: {severity}).\n\
         Metric value: {value_str}. Message: {message}.\n\n\
         Investigate:\n\
         1. Query recent error logs and traces for this project\n\
         2. Check deployment history — was there a recent deploy?\n\
         3. Review recent git commits for potential causes\n\
         4. Create an issue with your diagnosis and proposed remediation\n\
         5. If the fix is obvious and safe, describe the exact code change needed",
    );

    match crate::agent::service::create_session(
        state,
        admin_id,
        project_id,
        &prompt,
        "claude-code",
        None,
        None,
        crate::agent::AgentRoleName::Ops,
        None, // Alert-triggered sessions have no parent
    )
    .await
    {
        Ok(session) => {
            tracing::info!(
                %rule_id,
                %project_id,
                session_id = %session.id,
                "ops agent spawned for alert"
            );
        }
        Err(e) => {
            // Set a shorter cooldown on failure (3 min backoff instead of full 15 min)
            let _: Result<(), _> = state
                .valkey
                .next()
                .set::<(), _, _>(
                    &cooldown_key,
                    "1",
                    Some(fred::types::Expiration::EX(180)),
                    None,
                    false,
                )
                .await;
            tracing::error!(error = %e, %rule_id, %project_id, "failed to spawn ops agent");
        }
    }

    Ok(())
}

/// Inner flag registration logic, shared by `handle_flags_registered` and `handle_ops_repo_updated`.
/// Registers new flags and prunes flags not in the current set.
async fn handle_flags_registered_inner(
    state: &AppState,
    project_id: Uuid,
    flags: &[(String, serde_json::Value)],
) {
    for (key, default_value) in flags {
        let _ = sqlx::query(
            "INSERT INTO feature_flags (project_id, key, default_value, flag_type)
             VALUES ($1, $2, $3, 'boolean')
             ON CONFLICT (key, project_id, environment) DO NOTHING",
        )
        .bind(project_id)
        .bind(key)
        .bind(default_value)
        .execute(&state.pool)
        .await;
    }

    // Prune flags not in the current platform.yaml.
    // Only delete flags that have no rules/overrides (never user-configured).
    if !flags.is_empty() {
        let current_keys: Vec<&str> = flags.iter().map(|(k, _)| k.as_str()).collect();
        let deleted = sqlx::query(
            "DELETE FROM feature_flags
             WHERE project_id = $1
               AND key != ALL($2)
               AND environment IS NULL
               AND NOT EXISTS (SELECT 1 FROM feature_flag_rules WHERE flag_id = feature_flags.id)
               AND NOT EXISTS (SELECT 1 FROM feature_flag_overrides WHERE flag_id = feature_flags.id)",
        )
        .bind(project_id)
        .bind(&current_keys as &[&str])
        .execute(&state.pool)
        .await;

        if let Ok(result) = deleted {
            let count = result.rows_affected();
            if count > 0 {
                tracing::info!(%project_id, pruned = count, "pruned stale feature flags");
            }
        }
    }
}

/// Register feature flags from `.platform.yaml` — upserts defaults, never overwrites user-toggled state.
#[tracing::instrument(skip(state), fields(%project_id), err)]
async fn handle_flags_registered(
    state: &AppState,
    project_id: Uuid,
    flags: &[(String, serde_json::Value)],
) -> anyhow::Result<()> {
    handle_flags_registered_inner(state, project_id, flags).await;
    tracing::info!(
        count = flags.len(),
        "registered feature flags from pipeline"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_serialization_roundtrip() {
        let event = PlatformEvent::ImageBuilt {
            project_id: Uuid::nil(),
            environment: "production".into(),
            image_ref: "registry/app:v1".into(),
            pipeline_id: Uuid::nil(),
            triggered_by: None,
        };

        let json = serde_json::to_string(&event).unwrap();
        let parsed: PlatformEvent = serde_json::from_str(&json).unwrap();

        match parsed {
            PlatformEvent::ImageBuilt {
                environment,
                image_ref,
                ..
            } => {
                assert_eq!(environment, "production");
                assert_eq!(image_ref, "registry/app:v1");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn all_event_variants_serialize() {
        let events = vec![
            PlatformEvent::ImageBuilt {
                project_id: Uuid::nil(),
                environment: "staging".into(),
                image_ref: "img:v1".into(),
                pipeline_id: Uuid::nil(),
                triggered_by: Some(Uuid::nil()),
            },
            PlatformEvent::OpsRepoUpdated {
                project_id: Uuid::nil(),
                ops_repo_id: Uuid::nil(),
                environment: "production".into(),
                commit_sha: "abc123".into(),
                image_ref: "img:v1".into(),
            },
            PlatformEvent::DeployRequested {
                project_id: Uuid::nil(),
                environment: "production".into(),
                image_ref: "img:v2".into(),
                requested_by: None,
            },
            PlatformEvent::RollbackRequested {
                project_id: Uuid::nil(),
                environment: "production".into(),
                requested_by: Some(Uuid::nil()),
            },
            PlatformEvent::DevImageBuilt {
                project_id: Uuid::nil(),
                image_ref: "registry/app/dev:latest".into(),
                pipeline_id: Uuid::nil(),
            },
            PlatformEvent::AlertFired {
                rule_id: Uuid::nil(),
                project_id: Some(Uuid::nil()),
                severity: "critical".into(),
                value: Some(95.5),
                message: "Alert condition met".into(),
                alert_name: "High CPU".into(),
            },
            PlatformEvent::ReleaseCreated {
                target_id: Uuid::nil(),
                release_id: Uuid::nil(),
                project_id: Uuid::nil(),
                image_ref: "img:v1".into(),
                strategy: "canary".into(),
            },
            PlatformEvent::ReleasePromoted {
                release_id: Uuid::nil(),
                project_id: Uuid::nil(),
                image_ref: "img:v1".into(),
            },
            PlatformEvent::ReleaseRolledBack {
                release_id: Uuid::nil(),
                project_id: Uuid::nil(),
                reason: "gate failure".into(),
            },
            PlatformEvent::TrafficShifted {
                release_id: Uuid::nil(),
                project_id: Uuid::nil(),
                weights: [("stable".into(), 80), ("canary".into(), 20)]
                    .into_iter()
                    .collect(),
            },
            PlatformEvent::FlagsRegistered {
                project_id: Uuid::nil(),
                flags: vec![
                    ("feature_a".into(), serde_json::json!(false)),
                    ("feature_b".into(), serde_json::json!(true)),
                ],
            },
        ];

        for event in &events {
            let json = serde_json::to_string(event).unwrap();
            let _: PlatformEvent = serde_json::from_str(&json).unwrap();
        }
    }

    #[test]
    fn event_tagged_type_field() {
        let event = PlatformEvent::ImageBuilt {
            project_id: Uuid::nil(),
            environment: "production".into(),
            image_ref: "img:v1".into(),
            pipeline_id: Uuid::nil(),
            triggered_by: None,
        };

        let json: serde_json::Value = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "ImageBuilt");
    }

    #[test]
    fn all_event_types_have_correct_tag() {
        let cases = vec![
            (
                PlatformEvent::ImageBuilt {
                    project_id: Uuid::nil(),
                    environment: "prod".into(),
                    image_ref: "img:v1".into(),
                    pipeline_id: Uuid::nil(),
                    triggered_by: None,
                },
                "ImageBuilt",
            ),
            (
                PlatformEvent::OpsRepoUpdated {
                    project_id: Uuid::nil(),
                    ops_repo_id: Uuid::nil(),
                    environment: "prod".into(),
                    commit_sha: "abc".into(),
                    image_ref: "img:v1".into(),
                },
                "OpsRepoUpdated",
            ),
            (
                PlatformEvent::DeployRequested {
                    project_id: Uuid::nil(),
                    environment: "prod".into(),
                    image_ref: "img:v1".into(),
                    requested_by: None,
                },
                "DeployRequested",
            ),
            (
                PlatformEvent::RollbackRequested {
                    project_id: Uuid::nil(),
                    environment: "prod".into(),
                    requested_by: None,
                },
                "RollbackRequested",
            ),
            (
                PlatformEvent::DevImageBuilt {
                    project_id: Uuid::nil(),
                    image_ref: "registry/app/dev:abc123".into(),
                    pipeline_id: Uuid::nil(),
                },
                "DevImageBuilt",
            ),
            (
                PlatformEvent::AlertFired {
                    rule_id: Uuid::nil(),
                    project_id: Some(Uuid::nil()),
                    severity: "warning".into(),
                    value: Some(42.0),
                    message: "Alert condition met".into(),
                    alert_name: "Error Rate".into(),
                },
                "AlertFired",
            ),
            (
                PlatformEvent::ReleaseCreated {
                    target_id: Uuid::nil(),
                    release_id: Uuid::nil(),
                    project_id: Uuid::nil(),
                    image_ref: "img:v1".into(),
                    strategy: "rolling".into(),
                },
                "ReleaseCreated",
            ),
            (
                PlatformEvent::FlagsRegistered {
                    project_id: Uuid::nil(),
                    flags: vec![("flag_a".into(), serde_json::json!(false))],
                },
                "FlagsRegistered",
            ),
        ];

        for (event, expected_type) in cases {
            let json: serde_json::Value = serde_json::to_value(&event).unwrap();
            assert_eq!(
                json["type"], expected_type,
                "wrong type tag for {expected_type}"
            );
        }
    }

    #[test]
    fn invalid_json_rejected_by_handle_event() {
        // handle_event is async, so we use a mini runtime
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            // We can't construct a full AppState in unit tests, but we CAN test
            // that invalid JSON is rejected before it tries to access state.
            let result: Result<PlatformEvent, _> = serde_json::from_str("not valid json");
            assert!(result.is_err());

            let result: Result<PlatformEvent, _> = serde_json::from_str("{}");
            assert!(result.is_err(), "missing 'type' field should fail");

            let result: Result<PlatformEvent, _> =
                serde_json::from_str(r#"{"type":"UnknownEvent"}"#);
            assert!(result.is_err(), "unknown event type should fail");
        });
    }

    #[test]
    fn event_deserialization_with_all_fields() {
        let json = r#"{
            "type": "ImageBuilt",
            "project_id": "00000000-0000-0000-0000-000000000001",
            "environment": "staging",
            "image_ref": "registry.io/app:abc123",
            "pipeline_id": "00000000-0000-0000-0000-000000000002",
            "triggered_by": "00000000-0000-0000-0000-000000000003"
        }"#;
        let event: PlatformEvent = serde_json::from_str(json).unwrap();
        match event {
            PlatformEvent::ImageBuilt {
                project_id,
                environment,
                image_ref,
                pipeline_id,
                triggered_by,
            } => {
                assert_ne!(project_id, Uuid::nil());
                assert_eq!(environment, "staging");
                assert_eq!(image_ref, "registry.io/app:abc123");
                assert_ne!(pipeline_id, Uuid::nil());
                assert!(triggered_by.is_some());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn ops_repo_updated_deserialization() {
        let json = r#"{
            "type": "OpsRepoUpdated",
            "project_id": "00000000-0000-0000-0000-000000000001",
            "ops_repo_id": "00000000-0000-0000-0000-000000000002",
            "environment": "production",
            "commit_sha": "abc123def456",
            "image_ref": "registry/app:v2"
        }"#;
        let event: PlatformEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, PlatformEvent::OpsRepoUpdated { .. }));
    }

    #[test]
    fn rollback_requested_with_optional_fields() {
        // triggered_by is optional (None case)
        let json = r#"{
            "type": "RollbackRequested",
            "project_id": "00000000-0000-0000-0000-000000000001",
            "environment": "production",
            "requested_by": null
        }"#;
        let event: PlatformEvent = serde_json::from_str(json).unwrap();
        match event {
            PlatformEvent::RollbackRequested { requested_by, .. } => {
                assert!(requested_by.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn alert_fired_serialization_roundtrip() {
        let event = PlatformEvent::AlertFired {
            rule_id: Uuid::new_v4(),
            project_id: Some(Uuid::new_v4()),
            severity: "critical".into(),
            value: Some(99.8),
            message: "Alert condition met".into(),
            alert_name: "CPU Usage High".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: PlatformEvent = serde_json::from_str(&json).unwrap();
        match (event, parsed) {
            (
                PlatformEvent::AlertFired {
                    rule_id: a_rule,
                    project_id: a_proj,
                    severity: a_sev,
                    value: a_val,
                    message: a_msg,
                    alert_name: a_name,
                },
                PlatformEvent::AlertFired {
                    rule_id: b_rule,
                    project_id: b_proj,
                    severity: b_sev,
                    value: b_val,
                    message: b_msg,
                    alert_name: b_name,
                },
            ) => {
                assert_eq!(a_rule, b_rule);
                assert_eq!(a_proj, b_proj);
                assert_eq!(a_sev, b_sev);
                assert_eq!(a_val, b_val);
                assert_eq!(a_msg, b_msg);
                assert_eq!(a_name, b_name);
            }
            _ => panic!("wrong variants"),
        }
    }

    #[test]
    fn alert_fired_with_null_project_and_value() {
        let json = r#"{
            "type": "AlertFired",
            "rule_id": "00000000-0000-0000-0000-000000000001",
            "project_id": null,
            "severity": "info",
            "value": null,
            "message": "Metric absent",
            "alert_name": "Heartbeat"
        }"#;
        let event: PlatformEvent = serde_json::from_str(json).unwrap();
        match event {
            PlatformEvent::AlertFired {
                project_id, value, ..
            } => {
                assert!(project_id.is_none());
                assert!(value.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn deploy_requested_roundtrip() {
        let event = PlatformEvent::DeployRequested {
            project_id: Uuid::new_v4(),
            environment: "staging".into(),
            image_ref: "registry/app:v3".into(),
            requested_by: Some(Uuid::new_v4()),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: PlatformEvent = serde_json::from_str(&json).unwrap();
        match (event, parsed) {
            (
                PlatformEvent::DeployRequested {
                    project_id: a_pid,
                    environment: a_env,
                    image_ref: a_img,
                    requested_by: a_by,
                },
                PlatformEvent::DeployRequested {
                    project_id: b_pid,
                    environment: b_env,
                    image_ref: b_img,
                    requested_by: b_by,
                },
            ) => {
                assert_eq!(a_pid, b_pid);
                assert_eq!(a_env, b_env);
                assert_eq!(a_img, b_img);
                assert_eq!(a_by, b_by);
            }
            _ => panic!("wrong variants"),
        }
    }
}
