//! Valkey-based internal event bus for platform events.
//!
//! Events are published as JSON to a Valkey pub/sub channel. A background
//! subscriber loop dispatches events to typed handlers.

use std::path::PathBuf;

use fred::interfaces::PubsubInterface;
use fred::prelude::*;
use serde::{Deserialize, Serialize};
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
                        let payload: String = match message.value.convert() {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::warn!(error = %e, "failed to convert message payload");
                                continue;
                            }
                        };
                        let state = state.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_event(&state, &payload).await {
                                tracing::error!(error = %e, "event handler failed");
                            }
                        });
                    }
                    Err(e) => {
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
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Pipeline built an image → sync deploy/ → commit values to ops repo → publish `OpsRepoUpdated`.
async fn handle_image_built(
    state: &AppState,
    project_id: Uuid,
    environment: &str,
    image_ref: &str,
    pipeline_id: Uuid,
    _triggered_by: Option<Uuid>,
) -> anyhow::Result<()> {
    // Look up project info and auto-created ops repo (Phase 2: 1:1 project → ops repo)
    let project = sqlx::query!(
        "SELECT name, repo_path FROM projects WHERE id = $1 AND is_active = true",
        project_id,
    )
    .fetch_optional(&state.pool)
    .await?;

    let Some(project) = project else {
        tracing::warn!(%project_id, "image_built: project not found or inactive, skipping");
        return Ok(());
    };
    let project_name = project.name.clone();

    let ops_repo = sqlx::query!(
        "SELECT id, repo_path, branch FROM ops_repos WHERE project_id = $1",
        project_id,
    )
    .fetch_optional(&state.pool)
    .await?;

    // Sync deploy/ from project repo to ops repo (best-effort)
    if let Some(ops) = &ops_repo
        && let Some(repo_path) = &project.repo_path
    {
        sync_deploy_to_ops(state, repo_path, &ops.repo_path, &ops.branch, pipeline_id).await;
    }

    // Find or create deployment row; get linked ops_repo_id (if any)
    let fallback_ops_id = ops_repo.as_ref().map(|o| o.id);
    let Some(ops_repo_id) =
        upsert_deployment(state, project_id, environment, image_ref, fallback_ops_id).await?
    else {
        return Ok(());
    };

    // Ops repo linked — commit values and update deployment directly.
    // R5: Don't publish OpsRepoUpdated (avoids double-write + double-notify).
    // OpsRepoUpdated is only for external ops repo pushes.
    let ops = sqlx::query!(
        "SELECT repo_path, branch FROM ops_repos WHERE id = $1",
        ops_repo_id,
    )
    .fetch_one(&state.pool)
    .await?;

    let repo_path = PathBuf::from(&ops.repo_path);

    let values = serde_json::json!({
        "image_ref": image_ref,
        "project_name": project_name,
        "environment": environment,
    });

    let commit_sha =
        crate::deployer::ops_repo::commit_values(&repo_path, &ops.branch, environment, &values)
            .await?;

    // Single DB update: set image_ref + pending + commit_sha
    sqlx::query!(
        r#"UPDATE deployments SET image_ref = $3, current_status = 'pending', current_sha = $4
           WHERE project_id = $1 AND environment = $2"#,
        project_id,
        environment,
        image_ref,
        commit_sha,
    )
    .execute(&state.pool)
    .await?;

    tracing::info!(
        %project_id,
        %ops_repo_id,
        %commit_sha,
        %image_ref,
        "ops repo updated with new image"
    );

    state.deploy_notify.notify_one();

    Ok(())
}

/// Find or create deployment row. Returns `Some(ops_repo_id)` if an ops repo
/// is linked (caller should commit values), or `None` if handled inline.
async fn upsert_deployment(
    state: &AppState,
    project_id: Uuid,
    environment: &str,
    image_ref: &str,
    fallback_ops_id: Option<Uuid>,
) -> anyhow::Result<Option<Uuid>> {
    let deployment = sqlx::query!(
        "SELECT ops_repo_id FROM deployments WHERE project_id = $1 AND environment = $2",
        project_id,
        environment,
    )
    .fetch_optional(&state.pool)
    .await?;

    let Some(deployment) = deployment else {
        tracing::info!(%project_id, %environment, %image_ref, "no deployment found, creating default");
        sqlx::query!(
            r#"INSERT INTO deployments (project_id, environment, image_ref, ops_repo_id, desired_status, current_status)
               VALUES ($1, $2, $3, $4, 'active', 'pending')
               ON CONFLICT (project_id, environment)
               DO UPDATE SET image_ref = $3, ops_repo_id = COALESCE($4, deployments.ops_repo_id),
                             desired_status = 'active', current_status = 'pending'"#,
            project_id, environment, image_ref, fallback_ops_id,
        )
        .execute(&state.pool)
        .await?;
        state.deploy_notify.notify_one();
        return Ok(None);
    };

    let Some(ops_repo_id) = deployment.ops_repo_id else {
        sqlx::query!(
            r#"UPDATE deployments SET image_ref = $3, current_status = 'pending',
               ops_repo_id = COALESCE($4, ops_repo_id)
               WHERE project_id = $1 AND environment = $2"#,
            project_id,
            environment,
            image_ref,
            fallback_ops_id,
        )
        .execute(&state.pool)
        .await?;
        state.deploy_notify.notify_one();
        return Ok(None);
    };

    Ok(Some(ops_repo_id))
}

/// Sync the `deploy/` directory from the project repo to the ops repo.
/// Best-effort: logs warnings on failure but doesn't block the deployment.
async fn sync_deploy_to_ops(
    state: &AppState,
    project_repo_path: &str,
    ops_repo_path: &str,
    ops_branch: &str,
    pipeline_id: Uuid,
) {
    let Some(commit_sha) = get_pipeline_commit_sha(state, pipeline_id).await else {
        tracing::debug!(%pipeline_id, "no commit SHA for pipeline, skipping deploy/ sync");
        return;
    };

    let project_path = PathBuf::from(project_repo_path);
    let ops_path = PathBuf::from(ops_repo_path);

    if let Err(e) = crate::deployer::ops_repo::sync_from_project_repo(
        &project_path,
        &ops_path,
        ops_branch,
        &commit_sha,
    )
    .await
    {
        tracing::warn!(error = %e, "failed to sync deploy/ to ops repo");
    }
}

/// Get the commit SHA from a pipeline run for deploy/ sync.
async fn get_pipeline_commit_sha(state: &AppState, pipeline_id: Uuid) -> Option<String> {
    sqlx::query_scalar!(
        "SELECT commit_sha FROM pipelines WHERE id = $1",
        pipeline_id,
    )
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten()
    .flatten()
}

/// Ops repo was updated → update deployment DB row → wake deployer.
async fn handle_ops_repo_updated(
    state: &AppState,
    project_id: Uuid,
    environment: &str,
    commit_sha: &str,
    image_ref: &str,
) -> anyhow::Result<()> {
    sqlx::query!(
        r#"UPDATE deployments
           SET image_ref = $3, current_status = 'pending', current_sha = $4
           WHERE project_id = $1 AND environment = $2"#,
        project_id,
        environment,
        image_ref,
        commit_sha,
    )
    .execute(&state.pool)
    .await?;

    state.deploy_notify.notify_one();

    tracing::info!(
        %project_id,
        %environment,
        %image_ref,
        "deployment marked pending from ops repo update"
    );

    Ok(())
}

/// Manual deploy request → commit values to ops repo → mark pending → wake deployer.
async fn handle_deploy_requested(
    state: &AppState,
    project_id: Uuid,
    environment: &str,
    image_ref: &str,
    _requested_by: Option<Uuid>,
) -> anyhow::Result<()> {
    // Same flow as image_built but triggered manually
    handle_image_built(state, project_id, environment, image_ref, Uuid::nil(), None).await
}

/// Rollback request → revert ops repo commit → read reverted values → update DB → wake deployer.
async fn handle_rollback_requested(
    state: &AppState,
    project_id: Uuid,
    environment: &str,
    _requested_by: Option<Uuid>,
) -> anyhow::Result<()> {
    let deployment = sqlx::query!(
        "SELECT ops_repo_id FROM deployments WHERE project_id = $1 AND environment = $2",
        project_id,
        environment,
    )
    .fetch_optional(&state.pool)
    .await?;

    let Some(deployment) = deployment else {
        tracing::warn!(%project_id, %environment, "rollback: no deployment found");
        return Ok(());
    };

    if let Some(ops_repo_id) = deployment.ops_repo_id {
        let ops_repo = sqlx::query!(
            "SELECT repo_path, branch FROM ops_repos WHERE id = $1",
            ops_repo_id,
        )
        .fetch_one(&state.pool)
        .await?;

        let repo_path = PathBuf::from(&ops_repo.repo_path);

        // Revert the last commit in the ops repo
        let new_sha =
            crate::deployer::ops_repo::revert_last_commit(&repo_path, &ops_repo.branch).await?;

        // Read the reverted values to get the old image_ref
        let reverted_values =
            crate::deployer::ops_repo::read_values(&repo_path, &ops_repo.branch, environment)
                .await?;

        let old_image_ref = reverted_values["image_ref"].as_str().unwrap_or("unknown");

        // Update DB to match
        sqlx::query!(
            r#"UPDATE deployments
               SET image_ref = $3, current_status = 'pending', current_sha = $4,
                   desired_status = 'active'
               WHERE project_id = $1 AND environment = $2"#,
            project_id,
            environment,
            old_image_ref,
            new_sha,
        )
        .execute(&state.pool)
        .await?;

        tracing::info!(
            %project_id,
            %environment,
            %old_image_ref,
            "ops repo reverted, deployment marked pending"
        );
    } else {
        // No ops repo — fall back to DB-based rollback (legacy path)
        // The reconciler's handle_rollback will pick this up
        sqlx::query!(
            "UPDATE deployments SET desired_status = 'rollback', current_status = 'pending' WHERE project_id = $1 AND environment = $2",
            project_id,
            environment,
        )
        .execute(&state.pool)
        .await?;
    }

    state.deploy_notify.notify_one();

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
                image_ref: "registry/app-dev:latest".into(),
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
                    image_ref: "registry/app-dev:abc123".into(),
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
                    rule_id: a_rid,
                    project_id: a_pid,
                    severity: a_sev,
                    value: a_val,
                    message: a_msg,
                    alert_name: a_name,
                },
                PlatformEvent::AlertFired {
                    rule_id: b_rid,
                    project_id: b_pid,
                    severity: b_sev,
                    value: b_val,
                    message: b_msg,
                    alert_name: b_name,
                },
            ) => {
                assert_eq!(a_rid, b_rid);
                assert_eq!(a_pid, b_pid);
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
