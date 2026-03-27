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
        PlatformEvent::ReleasePromoted {
            release_id,
            project_id,
            ..
        } => {
            state.deploy_notify.notify_one();
            // Demo auto-promotion: if this is a staging completion for the demo project,
            // auto-promote to production
            handle_demo_auto_promote(state, project_id, release_id).await;
            Ok(())
        }
        // New progressive delivery events — wake reconciler, no special handler needed
        PlatformEvent::ReleaseCreated { .. }
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
///
/// If the spec has a `stages` list and the current `environment` is not in it,
/// the strategy falls back to `"rolling"` with an empty config.
fn resolve_deploy_config_from_specs(
    pf: &crate::pipeline::definition::PlatformFile,
    environment: &str,
) -> (serde_json::Value, Option<String>) {
    let Some(ref deploy) = pf.deploy else {
        return (serde_json::json!({}), None);
    };

    let Some(spec) = deploy.specs.first() else {
        return (serde_json::json!({}), None);
    };

    // Check if this environment is in the spec's stages list.
    // Default stages: canary → [staging], ab_test → [staging, production], rolling → [staging, production]
    let default_stages = if spec.deploy_type == "canary" {
        vec!["staging".to_string()]
    } else {
        vec!["staging".to_string(), "production".to_string()]
    };
    let stages = spec.stages.as_deref().unwrap_or(&default_stages);
    tracing::debug!(
        deploy_type = %spec.deploy_type,
        ?stages,
        %environment,
        "resolve_deploy_config: checking stages"
    );

    if !stages.iter().any(|s| s == environment) {
        // This environment is not in the spec's stages — use rolling
        return (serde_json::json!({}), Some("rolling".into()));
    }

    let strategy = spec.deploy_type.clone();
    let config = if let Some(ref canary) = spec.canary {
        serde_json::to_value(canary).unwrap_or_default()
    } else if let Some(ref ab) = spec.ab_test {
        serde_json::to_value(ab).unwrap_or_default()
    } else {
        serde_json::json!({})
    };
    (config, Some(strategy))
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

    let platform_file: Option<crate::pipeline::definition::PlatformFile> = if let Some(ref ops) =
        ops_repo
    {
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
    let has_deploy_specs = match &platform_file {
        Some(pf) => pf.deploy.as_ref().map_or(0, |d| d.specs.len()),
        None => 0,
    };
    tracing::info!(
        %environment,
        platform_file_present = platform_file.is_some(),
        deploy_specs_count = has_deploy_specs,
        "resolving deploy config for release creation"
    );
    let (rollout_config, strategy_override) = if let Some(ref pf) = platform_file {
        resolve_deploy_config_from_specs(pf, environment)
    } else {
        (serde_json::json!({}), None)
    };
    tracing::info!(
        %environment,
        strategy = ?strategy_override,
        "resolved deploy strategy for release"
    );

    // 3. Find or create deploy target
    let ops_repo_id = ops_repo.as_ref().map(|o| o.id);
    let target_id =
        upsert_deploy_target_simple(state, project_id, environment, ops_repo_id).await?;

    // 3b. Cancel any in-progress releases for this target (cancel-and-replace)
    let cancelled = sqlx::query(
        "UPDATE deploy_releases SET phase = 'cancelled', completed_at = now()
         WHERE target_id = $1 AND phase IN ('pending', 'progressing', 'holding', 'paused')",
    )
    .bind(target_id)
    .execute(&state.pool)
    .await;
    if let Ok(result) = &cancelled {
        let count = result.rows_affected();
        if count > 0 {
            tracing::info!(%target_id, cancelled_count = count, "cancelled in-progress releases (superseded)");
        }
    }

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

/// Demo project: auto-promote staging to production after rolling deploy completes,
/// then create PR2 after production completes.
async fn handle_demo_auto_promote(state: &AppState, project_id: Uuid, release_id: Uuid) {
    use sqlx::Row as _;

    // Check if this is the demo project
    let demo_project_id =
        match crate::onboarding::presets::get_setting(&state.pool, "demo_project_id").await {
            Ok(Some(val)) => val
                .as_str()
                .and_then(|s| s.parse::<Uuid>().ok())
                .or_else(|| serde_json::from_value::<Uuid>(val).ok()),
            _ => None,
        };

    let Some(demo_id) = demo_project_id else {
        return;
    };
    if demo_id != project_id {
        return;
    }

    // Get the release details
    let release = sqlx::query(
        "SELECT dr.strategy, dt.environment FROM deploy_releases dr
         JOIN deploy_targets dt ON dr.target_id = dt.id
         WHERE dr.id = $1",
    )
    .bind(release_id)
    .fetch_optional(&state.pool)
    .await;

    let Ok(Some(row)) = release else { return };
    let strategy: String = row.get("strategy");
    let environment: String = row.get("environment");

    if environment == "staging" && strategy == "rolling" {
        demo_promote_staging_to_prod(state, project_id).await;
    } else if environment == "production" && strategy == "rolling" {
        demo_mark_prod_complete(state, project_id).await;
    }
}

/// Staging rolling completed → merge staging branch into prod and publish `OpsRepoUpdated`.
async fn demo_promote_staging_to_prod(state: &AppState, project_id: Uuid) {
    use sqlx::Row as _;

    // Check we haven't already promoted
    let already = crate::onboarding::presets::get_setting(&state.pool, "demo_v1_prod_promoted")
        .await
        .ok()
        .flatten();
    if already.is_some() {
        return;
    }

    tracing::info!(%project_id, "demo: auto-promoting v0.1 staging → production");

    let ops_repo = sqlx::query("SELECT id, repo_path, branch FROM ops_repos WHERE project_id = $1")
        .bind(project_id)
        .fetch_optional(&state.pool)
        .await;

    let Ok(Some(ops)) = ops_repo else { return };
    let ops_id: Uuid = ops.get("id");
    let ops_path_str: String = ops.get("repo_path");
    let ops_branch: String = ops.get("branch");
    let ops_path = std::path::PathBuf::from(&ops_path_str);

    // Read staging values for image_ref
    let Ok(staging_values) =
        crate::deployer::ops_repo::read_values(&ops_path, "staging", "staging").await
    else {
        return;
    };
    let image_ref = staging_values["image_ref"]
        .as_str()
        .unwrap_or("")
        .to_string();

    // Merge staging → production branch in the ops repo.
    // The per-repo mutex in ops_repo serializes this with concurrent gitops_sync writes.
    let merge_result =
        crate::deployer::ops_repo::merge_branch(&ops_path, "staging", &ops_branch).await;

    if let Ok(new_sha) = merge_result {
        let _ = publish(
            &state.valkey,
            &PlatformEvent::OpsRepoUpdated {
                project_id,
                ops_repo_id: ops_id,
                environment: "production".into(),
                commit_sha: new_sha,
                image_ref,
            },
        )
        .await;
    }
}

/// Production rolling completed → mark as promoted and spawn PR2 creation.
async fn demo_mark_prod_complete(state: &AppState, project_id: Uuid) {
    let already = crate::onboarding::presets::get_setting(&state.pool, "demo_v1_prod_promoted")
        .await
        .ok()
        .flatten();
    if already.is_some() {
        return;
    }

    let _ = crate::onboarding::presets::upsert_setting_pub(
        &state.pool,
        "demo_v1_prod_promoted",
        &serde_json::json!(true),
    )
    .await;

    tracing::info!(%project_id, "demo: production v0.1 complete, creating PR2");

    // Get project owner for PR2 creation
    let owner_id: Option<Uuid> = sqlx::query_scalar("SELECT owner_id FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten();

    if let Some(owner_id) = owner_id {
        let state = state.clone();
        tokio::spawn(async move {
            crate::onboarding::demo_project::create_demo_pr2(&state, project_id, owner_id).await;
        });
    }
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

    // -- resolve_deploy_config_from_specs --

    fn minimal_platform_file() -> crate::pipeline::definition::PlatformFile {
        crate::pipeline::definition::PlatformFile {
            pipeline: serde_json::from_value(serde_json::json!({
                "steps": []
            }))
            .unwrap(),
            flags: vec![],
            deploy: None,
        }
    }

    #[test]
    fn resolve_deploy_no_deploy_section() {
        let pf = minimal_platform_file();
        let (config, strategy) = resolve_deploy_config_from_specs(&pf, "production");
        assert_eq!(config, serde_json::json!({}));
        assert!(strategy.is_none());
    }

    #[test]
    fn resolve_deploy_empty_specs() {
        let mut pf = minimal_platform_file();
        pf.deploy = Some(
            serde_json::from_value(serde_json::json!({
                "specs": []
            }))
            .unwrap(),
        );
        let (config, strategy) = resolve_deploy_config_from_specs(&pf, "staging");
        assert_eq!(config, serde_json::json!({}));
        assert!(strategy.is_none());
    }

    #[test]
    fn resolve_deploy_canary_on_staging() {
        let mut pf = minimal_platform_file();
        pf.deploy = Some(
            serde_json::from_value(serde_json::json!({
                "specs": [{
                    "name": "web",
                    "type": "canary",
                    "canary": {
                        "stable_service": "web-stable",
                        "canary_service": "web-canary",
                        "steps": [10, 50, 100]
                    }
                }]
            }))
            .unwrap(),
        );
        let (config, strategy) = resolve_deploy_config_from_specs(&pf, "staging");
        assert_eq!(strategy, Some("canary".into()));
        assert!(config.get("stable_service").is_some());
    }

    #[test]
    fn resolve_deploy_canary_not_on_production() {
        // canary default stages = ["staging"] only
        let mut pf = minimal_platform_file();
        pf.deploy = Some(
            serde_json::from_value(serde_json::json!({
                "specs": [{
                    "name": "web",
                    "type": "canary",
                    "canary": {
                        "stable_service": "web-stable",
                        "canary_service": "web-canary",
                        "steps": [10, 50, 100]
                    }
                }]
            }))
            .unwrap(),
        );
        let (config, strategy) = resolve_deploy_config_from_specs(&pf, "production");
        // production not in default canary stages → falls back to rolling
        assert_eq!(config, serde_json::json!({}));
        assert_eq!(strategy, Some("rolling".into()));
    }

    #[test]
    fn resolve_deploy_rolling_defaults() {
        let mut pf = minimal_platform_file();
        pf.deploy = Some(
            serde_json::from_value(serde_json::json!({
                "specs": [{
                    "name": "web",
                    "type": "rolling"
                }]
            }))
            .unwrap(),
        );
        let (config, strategy) = resolve_deploy_config_from_specs(&pf, "production");
        // rolling default stages = ["staging", "production"]
        assert_eq!(strategy, Some("rolling".into()));
        assert_eq!(config, serde_json::json!({}));
    }

    #[test]
    fn resolve_deploy_ab_test_on_staging() {
        let mut pf = minimal_platform_file();
        pf.deploy = Some(
            serde_json::from_value(serde_json::json!({
                "specs": [{
                    "name": "web",
                    "type": "ab_test",
                    "ab_test": {
                        "control_service": "web-control",
                        "treatment_service": "web-treatment",
                        "match": { "headers": {"x-experiment": "true"} },
                        "success_metric": "conversion_rate",
                        "success_condition": "treatment > control"
                    }
                }]
            }))
            .unwrap(),
        );
        let (config, strategy) = resolve_deploy_config_from_specs(&pf, "staging");
        // ab_test default stages = ["staging", "production"]
        assert_eq!(strategy, Some("ab_test".into()));
        assert!(config.get("control_service").is_some());
        assert_eq!(config["control_service"], "web-control");
    }

    #[test]
    fn resolve_deploy_ab_test_on_production() {
        let mut pf = minimal_platform_file();
        pf.deploy = Some(
            serde_json::from_value(serde_json::json!({
                "specs": [{
                    "name": "web",
                    "type": "ab_test",
                    "ab_test": {
                        "control_service": "web-control",
                        "treatment_service": "web-treatment",
                        "match": { "headers": {} },
                        "success_metric": "latency",
                        "success_condition": "treatment < control"
                    }
                }]
            }))
            .unwrap(),
        );
        let (config, strategy) = resolve_deploy_config_from_specs(&pf, "production");
        assert_eq!(strategy, Some("ab_test".into()));
        assert_eq!(config["treatment_service"], "web-treatment");
    }

    #[test]
    fn resolve_deploy_custom_stages_includes_env() {
        let mut pf = minimal_platform_file();
        pf.deploy = Some(
            serde_json::from_value(serde_json::json!({
                "specs": [{
                    "name": "web",
                    "type": "canary",
                    "stages": ["production", "qa"],
                    "canary": {
                        "stable_service": "web-stable",
                        "canary_service": "web-canary",
                        "steps": [10, 50, 100]
                    }
                }]
            }))
            .unwrap(),
        );
        // "production" is in custom stages → should use canary
        let (config, strategy) = resolve_deploy_config_from_specs(&pf, "production");
        assert_eq!(strategy, Some("canary".into()));
        assert!(config.get("stable_service").is_some());
    }

    #[test]
    fn resolve_deploy_custom_stages_excludes_env() {
        let mut pf = minimal_platform_file();
        pf.deploy = Some(
            serde_json::from_value(serde_json::json!({
                "specs": [{
                    "name": "web",
                    "type": "canary",
                    "stages": ["qa"],
                    "canary": {
                        "stable_service": "web-stable",
                        "canary_service": "web-canary",
                        "steps": [10, 50, 100]
                    }
                }]
            }))
            .unwrap(),
        );
        // "staging" not in custom stages → falls back to rolling
        let (config, strategy) = resolve_deploy_config_from_specs(&pf, "staging");
        assert_eq!(strategy, Some("rolling".into()));
        assert_eq!(config, serde_json::json!({}));
    }

    #[test]
    fn resolve_deploy_rolling_not_in_stages() {
        // rolling with custom stages that exclude "staging"
        let mut pf = minimal_platform_file();
        pf.deploy = Some(
            serde_json::from_value(serde_json::json!({
                "specs": [{
                    "name": "web",
                    "type": "rolling",
                    "stages": ["production"]
                }]
            }))
            .unwrap(),
        );
        let (config, strategy) = resolve_deploy_config_from_specs(&pf, "staging");
        // staging not in custom stages → rolling fallback
        assert_eq!(strategy, Some("rolling".into()));
        assert_eq!(config, serde_json::json!({}));
    }

    #[test]
    fn resolve_deploy_no_canary_or_ab_config() {
        // Spec with type "canary" but no canary config struct → empty config
        let mut pf = minimal_platform_file();
        pf.deploy = Some(
            serde_json::from_value(serde_json::json!({
                "specs": [{
                    "name": "web",
                    "type": "canary",
                    "stages": ["staging"]
                }]
            }))
            .unwrap(),
        );
        let (config, strategy) = resolve_deploy_config_from_specs(&pf, "staging");
        assert_eq!(strategy, Some("canary".into()));
        // No canary config → empty object
        assert_eq!(config, serde_json::json!({}));
    }

    #[test]
    fn resolve_deploy_rolling_on_staging() {
        let mut pf = minimal_platform_file();
        pf.deploy = Some(
            serde_json::from_value(serde_json::json!({
                "specs": [{
                    "name": "web",
                    "type": "rolling"
                }]
            }))
            .unwrap(),
        );
        let (config, strategy) = resolve_deploy_config_from_specs(&pf, "staging");
        // rolling default stages = ["staging", "production"] — staging included
        assert_eq!(strategy, Some("rolling".into()));
        assert_eq!(config, serde_json::json!({}));
    }

    // -- Additional event serialization tests --

    #[test]
    fn traffic_shifted_weights_roundtrip() {
        let mut weights = std::collections::HashMap::new();
        weights.insert("stable".to_string(), 70u32);
        weights.insert("canary".to_string(), 30u32);

        let event = PlatformEvent::TrafficShifted {
            release_id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            weights: weights.clone(),
        };

        let json = serde_json::to_string(&event).unwrap();
        let parsed: PlatformEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            PlatformEvent::TrafficShifted {
                weights: parsed_w, ..
            } => {
                assert_eq!(parsed_w.len(), 2);
                assert_eq!(parsed_w["stable"], 70);
                assert_eq!(parsed_w["canary"], 30);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn flags_registered_roundtrip() {
        let event = PlatformEvent::FlagsRegistered {
            project_id: Uuid::new_v4(),
            flags: vec![
                ("dark_mode".into(), serde_json::json!(false)),
                ("max_retries".into(), serde_json::json!(5)),
            ],
        };

        let json = serde_json::to_string(&event).unwrap();
        let parsed: PlatformEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            PlatformEvent::FlagsRegistered { flags, .. } => {
                assert_eq!(flags.len(), 2);
                assert_eq!(flags[0].0, "dark_mode");
                assert_eq!(flags[0].1, serde_json::json!(false));
                assert_eq!(flags[1].0, "max_retries");
                assert_eq!(flags[1].1, serde_json::json!(5));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn release_promoted_roundtrip() {
        let release_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        let event = PlatformEvent::ReleasePromoted {
            release_id,
            project_id,
            image_ref: "img:promoted".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: PlatformEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            PlatformEvent::ReleasePromoted {
                release_id: r,
                project_id: p,
                image_ref,
            } => {
                assert_eq!(r, release_id);
                assert_eq!(p, project_id);
                assert_eq!(image_ref, "img:promoted");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn release_rolled_back_roundtrip() {
        let event = PlatformEvent::ReleaseRolledBack {
            release_id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            reason: "canary health check failure".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: PlatformEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            PlatformEvent::ReleaseRolledBack { reason, .. } => {
                assert_eq!(reason, "canary health check failure");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn release_created_roundtrip() {
        let event = PlatformEvent::ReleaseCreated {
            target_id: Uuid::new_v4(),
            release_id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            image_ref: "img:v1".into(),
            strategy: "ab_test".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: PlatformEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            PlatformEvent::ReleaseCreated {
                strategy,
                image_ref,
                ..
            } => {
                assert_eq!(strategy, "ab_test");
                assert_eq!(image_ref, "img:v1");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn dev_image_built_roundtrip() {
        let event = PlatformEvent::DevImageBuilt {
            project_id: Uuid::new_v4(),
            image_ref: "registry/app/dev:sha256".into(),
            pipeline_id: Uuid::new_v4(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: PlatformEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            PlatformEvent::DevImageBuilt { image_ref, .. } => {
                assert_eq!(image_ref, "registry/app/dev:sha256");
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
