// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Valkey-based event bus subscriber and dispatcher.
//!
//! Subscribes to `platform:events`, deserializes events, and dispatches
//! to domain crate handlers. Handles:
//! - Deploy events → wake reconciler
//! - `PipelineQueued` → wake executor
//! - `CodePushed` → trigger pipeline + fire webhooks
//! - `TagPushed` → fire webhooks
//! - `MrBranchSynced` → fire webhooks
//! - `AgentSessionEnded` → fire agent webhooks
//! - `PipelineCompleted` → auto-merge on success
//! - `AlertFired` → dispatch notifications

use fred::interfaces::PubsubInterface;
use fred::prelude::*;
use platform_types::events::{EVENTS_CHANNEL, PlatformEvent};
use platform_types::{
    MergeRequestHandler, NotificationDispatcher, NotifyParams, WebhookDispatcher,
};
use tracing::Instrument;

use crate::state::PlatformState;

/// Background task: subscribe to platform events and dispatch to handlers.
pub async fn run(state: PlatformState, cancel: tokio_util::sync::CancellationToken) {
    tracing::info!("event bus subscriber started");

    let subscriber = state.valkey.next().clone_new();
    if let Err(e) = subscriber.init().await {
        tracing::error!(error = %e, "failed to init event bus subscriber");
        return;
    }

    if let Err(e) = subscriber.subscribe(EVENTS_CHANNEL).await {
        tracing::error!(error = %e, "failed to subscribe to {EVENTS_CHANNEL}");
        return;
    }

    let mut message_rx = subscriber.message_rx();
    state.task_registry.register("event_bus", 30);

    let mut keepalive = tokio::time::interval(std::time::Duration::from_secs(25));
    keepalive.tick().await;

    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                tracing::info!("event bus subscriber shutting down");
                let _ = subscriber.unsubscribe(EVENTS_CHANNEL).await;
                break;
            }
            _ = keepalive.tick() => {
                state.task_registry.heartbeat("event_bus");
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
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                }
            }
        }
    }
}

/// Dispatch a deserialized event to domain handlers.
#[allow(clippy::too_many_lines)]
async fn handle_event(state: &PlatformState, payload: &str) -> anyhow::Result<()> {
    let event: PlatformEvent = serde_json::from_str(payload)?;
    tracing::debug!(?event, "handling platform event");

    match event {
        // Deploy-related events wake the reconciler
        PlatformEvent::ReleaseCreated { .. }
        | PlatformEvent::ReleaseRolledBack { .. }
        | PlatformEvent::TrafficShifted { .. }
        | PlatformEvent::DeployRequested { .. }
        | PlatformEvent::RollbackRequested { .. } => {
            state.deploy_notify.notify_one();
        }
        PlatformEvent::OpsRepoUpdated {
            project_id,
            ops_repo_id,
            environment,
            commit_sha,
            image_ref,
        } => {
            handle_ops_repo_updated(
                state,
                project_id,
                ops_repo_id,
                &environment,
                &commit_sha,
                &image_ref,
            )
            .await;
        }
        PlatformEvent::ReleasePromoted {
            release_id,
            project_id,
            ..
        } => {
            state.deploy_notify.notify_one();
            handle_demo_auto_promote(state, project_id, release_id).await;
        }
        PlatformEvent::PipelineQueued { .. } => {
            state.pipeline_notify.notify_one();
        }
        PlatformEvent::CodePushed {
            project_id,
            user_id,
            user_name,
            repo_path,
            branch,
            commit_sha,
        } => {
            handle_code_pushed(
                state,
                project_id,
                user_id,
                &user_name,
                &repo_path,
                &branch,
                commit_sha.as_deref(),
            )
            .await;
        }
        PlatformEvent::TagPushed {
            project_id,
            user_name,
            tag_name,
            commit_sha,
            ..
        } => {
            fire_webhook(state, project_id, "tag", &serde_json::json!({
                "ref": format!("refs/tags/{tag_name}"), "after": commit_sha, "pusher": user_name,
            })).await;
        }
        PlatformEvent::MrBranchSynced {
            project_id,
            branch,
            commit_sha,
            ..
        } => {
            fire_webhook(
                state,
                project_id,
                "mr",
                &serde_json::json!({
                    "action": "synchronize", "branch": branch, "head_sha": commit_sha,
                }),
            )
            .await;
        }
        PlatformEvent::AgentSessionEnded {
            project_id,
            session_id,
            status,
        } => {
            fire_webhook(
                state,
                project_id,
                "agent",
                &serde_json::json!({
                    "action": status, "session_id": session_id, "project_id": project_id,
                }),
            )
            .await;
        }
        PlatformEvent::PipelineCompleted {
            project_id, status, ..
        } => {
            handle_pipeline_completed(state, project_id, &status).await;
        }
        PlatformEvent::AlertFired {
            rule_id,
            project_id,
            severity,
            message,
            alert_name,
            ..
        } => {
            handle_alert_fired(state, rule_id, project_id, &severity, &message, &alert_name).await;
        }
        _ => {}
    }

    Ok(())
}

/// Fire webhooks for a project event.
async fn fire_webhook(
    state: &PlatformState,
    project_id: uuid::Uuid,
    event: &str,
    payload: &serde_json::Value,
) {
    let webhook = platform_webhook::WebhookDispatch::new(state.pool.clone());
    webhook.fire_webhooks(project_id, event, payload).await;
}

/// Auto-merge open MRs after a successful pipeline, then trigger push
/// pipelines for any branches that were updated by the merge.
async fn handle_pipeline_completed(state: &PlatformState, project_id: uuid::Uuid, status: &str) {
    if status != "success" {
        return;
    }

    // Snapshot which MRs are currently merged (to detect newly merged ones).
    let before: Vec<i32> = sqlx::query_scalar(
        "SELECT number FROM merge_requests WHERE project_id = $1 AND status = 'merged'",
    )
    .bind(project_id)
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();

    let merger = platform_git::CliGitMerger;
    let repos_path = state.config.git.git_repos_path.clone();
    let handler = platform_git::AutoMergeHandler::new(state.pool.clone(), merger, repos_path);
    handler.try_auto_merge(project_id).await;

    // Find MRs that were just merged (new entries since the snapshot).
    let after: Vec<(i32, String)> = sqlx::query_as(
        "SELECT number, target_branch FROM merge_requests WHERE project_id = $1 AND status = 'merged'",
    )
    .bind(project_id)
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();

    let newly_merged: Vec<&str> = after
        .iter()
        .filter(|(n, _)| !before.contains(n))
        .map(|(_, branch)| branch.as_str())
        .collect();

    if newly_merged.is_empty() {
        return;
    }

    // Trigger push pipeline for each target branch that received a merge.
    let info: Option<(uuid::Uuid, String, String)> = sqlx::query_as(
        "SELECT u.id, u.name, p.repo_path FROM projects p JOIN users u ON p.owner_id = u.id
         WHERE p.id = $1 AND p.is_active = true",
    )
    .bind(project_id)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();

    let Some((owner_id, owner_name, repo_path)) = info else {
        return;
    };

    for branch in newly_merged {
        // Resolve merge commit SHA
        let git = platform_git::CliGitRepo;
        let sha = platform_git::GitCoreRead::rev_parse(
            &git,
            std::path::Path::new(&repo_path),
            &format!("refs/heads/{branch}"),
        )
        .await
        .ok();

        handle_code_pushed(
            state,
            project_id,
            owner_id,
            &owner_name,
            std::path::Path::new(&repo_path),
            branch,
            sha.as_deref(),
        )
        .await;
    }
}

/// Handle a code push: trigger pipeline and fire push webhooks.
#[allow(clippy::too_many_arguments)]
async fn handle_code_pushed(
    state: &PlatformState,
    project_id: uuid::Uuid,
    user_id: uuid::Uuid,
    user_name: &str,
    repo_path: &std::path::Path,
    branch: &str,
    commit_sha: Option<&str>,
) {
    // 1. Fire push webhooks
    let webhook = platform_webhook::WebhookDispatch::new(state.pool.clone());
    webhook
        .fire_webhooks(
            project_id,
            "push",
            &serde_json::json!({
                "ref": format!("refs/heads/{branch}"),
                "after": commit_sha,
                "pusher": user_name,
            }),
        )
        .await;

    // 2. Trigger pipeline if .platform.yaml exists and matches
    let params = platform_pipeline::trigger::PushTriggerParams {
        project_id,
        user_id,
        repo_path: repo_path.to_path_buf(),
        branch: branch.to_string(),
        commit_sha: commit_sha.map(String::from),
    };
    match platform_pipeline::trigger::on_push(
        &state.pool,
        &params,
        &state.config.pipeline.kaniko_image,
    )
    .await
    {
        Ok(Some(pipeline_id)) => {
            tracing::info!(%pipeline_id, %project_id, %branch, "pipeline triggered by push");
            // Wake the executor
            state.pipeline_notify.notify_one();
        }
        Ok(None) => {
            tracing::debug!(%project_id, %branch, "no pipeline triggered for push");
        }
        Err(e) => {
            tracing::debug!(error = %e, %project_id, %branch, "pipeline trigger skipped");
        }
    }
}

/// Handle an alert firing: dispatch notification via `NotificationDispatcher`.
async fn handle_alert_fired(
    state: &PlatformState,
    rule_id: uuid::Uuid,
    project_id: Option<uuid::Uuid>,
    severity: &str,
    message: &str,
    alert_name: &str,
) {
    let Some(project_id) = project_id else {
        tracing::debug!(%rule_id, "alert fired without project_id, skipping notification");
        return;
    };

    // Look up project owner for notification target
    let owner_id = match sqlx::query_scalar!(
        r#"SELECT owner_id as "owner_id!" FROM projects WHERE id = $1 AND is_active = true"#,
        project_id
    )
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(id)) => id,
        Ok(None) => {
            tracing::warn!(%project_id, "project not found for alert notification");
            return;
        }
        Err(e) => {
            tracing::error!(error = %e, %project_id, "failed to look up project for alert");
            return;
        }
    };

    // Dispatch via NotificationDispatcher (handles rate limiting, routing, status tracking)
    let subject = format!("[{severity}] {alert_name}");
    if let Err(e) = state
        .notification_dispatcher
        .notify(NotifyParams {
            user_id: owner_id,
            notification_type: "alert",
            subject: &subject,
            body: Some(message),
            channel: "in_app",
            ref_type: Some("alert_rule"),
            ref_id: Some(rule_id),
        })
        .await
    {
        tracing::error!(error = %e, %rule_id, "failed to dispatch alert notification");
    }
}

// ---------------------------------------------------------------------------
// Ops repo updated → create release
// ---------------------------------------------------------------------------

/// Ops repo was updated → read platform.yaml → create deploy release → wake reconciler.
async fn handle_ops_repo_updated(
    state: &PlatformState,
    project_id: uuid::Uuid,
    ops_repo_id: uuid::Uuid,
    environment: &str,
    commit_sha: &str,
    image_ref: &str,
) {
    use sqlx::Row as _;

    // 1. Read platform.yaml from ops repo for deploy specs
    let ops_repo = sqlx::query("SELECT id, repo_path, branch FROM ops_repos WHERE id = $1")
        .bind(ops_repo_id)
        .fetch_optional(&state.pool)
        .await;

    let platform_file: Option<platform_pipeline::definition::PlatformFile> =
        if let Ok(Some(ref ops)) = ops_repo {
            let ops_path = std::path::PathBuf::from(ops.get::<String, _>("repo_path"));
            let branch: String = if environment == "staging" {
                "staging".into()
            } else {
                ops.get("branch")
            };
            match platform_ops_repo::read_file_at_ref(&ops_path, &branch, "platform.yaml").await {
                Ok(content) => serde_yaml::from_str(&content).ok(),
                Err(_) => None,
            }
        } else {
            None
        };

    // 2. Resolve deploy strategy from platform.yaml specs
    let (rollout_config, strategy_override) = if let Some(ref pf) = platform_file {
        resolve_deploy_config_from_specs(pf, environment)
    } else {
        (serde_json::json!({}), None)
    };

    // 3. Find or create deploy target
    let target_id = match upsert_deploy_target(state, project_id, environment, Some(ops_repo_id))
        .await
    {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, %project_id, %environment, "failed to upsert deploy target");
            return;
        }
    };

    // 4. Cancel any in-progress releases for this target (cancel-and-replace)
    let _ = sqlx::query(
        "UPDATE deploy_releases SET phase = 'cancelled', completed_at = now()
         WHERE target_id = $1 AND phase IN ('pending', 'progressing', 'holding', 'paused')",
    )
    .bind(target_id)
    .execute(&state.pool)
    .await;

    // 5. Create release
    let release_id = sqlx::query_scalar::<_, uuid::Uuid>(
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
    .await;

    match release_id {
        Ok(id) => {
            tracing::info!(%project_id, %environment, %image_ref, %id, "release created from ops repo update");
        }
        Err(e) => {
            tracing::error!(error = %e, %project_id, %environment, "failed to create release");
            return;
        }
    }

    // 6. Wake reconciler
    state.deploy_notify.notify_one();
}

/// Find or create a deploy target for a project + environment.
async fn upsert_deploy_target(
    state: &PlatformState,
    project_id: uuid::Uuid,
    environment: &str,
    ops_repo_id: Option<uuid::Uuid>,
) -> anyhow::Result<uuid::Uuid> {
    let existing = sqlx::query_scalar::<_, uuid::Uuid>(
        "SELECT id FROM deploy_targets
         WHERE project_id = $1 AND environment = $2 AND branch_slug IS NULL AND is_active = true",
    )
    .bind(project_id)
    .bind(environment)
    .fetch_optional(&state.pool)
    .await?;

    if let Some(id) = existing {
        if let Some(ops_id) = ops_repo_id {
            let _ = sqlx::query(
                "UPDATE deploy_targets SET ops_repo_id = COALESCE(ops_repo_id, $2) WHERE id = $1",
            )
            .bind(id)
            .bind(ops_id)
            .execute(&state.pool)
            .await;
        }
        return Ok(id);
    }

    let id = sqlx::query_scalar::<_, uuid::Uuid>(
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
    pf: &platform_pipeline::definition::PlatformFile,
    environment: &str,
) -> (serde_json::Value, Option<String>) {
    let Some(ref deploy) = pf.deploy else {
        return (serde_json::json!({}), None);
    };

    let Some(spec) = deploy.specs.first() else {
        return (serde_json::json!({}), None);
    };

    let default_stages = if spec.deploy_type == "canary" {
        vec!["staging".to_string()]
    } else {
        vec!["staging".to_string(), "production".to_string()]
    };
    let stages = spec.stages.as_deref().unwrap_or(&default_stages);

    if !stages.iter().any(|s| s == environment) {
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

// ---------------------------------------------------------------------------
// Demo project auto-promote
// ---------------------------------------------------------------------------

/// Demo project: auto-promote staging to production after rolling deploy completes,
/// then create PR2 after production completes.
async fn handle_demo_auto_promote(
    state: &PlatformState,
    project_id: uuid::Uuid,
    release_id: uuid::Uuid,
) {
    use sqlx::Row as _;

    // Check if this is the demo project
    let demo_project_id =
        match crate::demo::demo_project::get_setting(&state.pool, "demo_project_id").await {
            Ok(Some(val)) => val
                .as_str()
                .and_then(|s| s.parse::<uuid::Uuid>().ok())
                .or_else(|| serde_json::from_value::<uuid::Uuid>(val).ok()),
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
async fn demo_promote_staging_to_prod(state: &PlatformState, project_id: uuid::Uuid) {
    use sqlx::Row as _;

    // Check we haven't already promoted — set flag immediately to prevent duplicate
    // promotions from concurrent event deliveries (Valkey pub/sub can redeliver).
    let already = crate::demo::demo_project::get_setting(&state.pool, "demo_v1_prod_promoted")
        .await
        .ok()
        .flatten();
    if already.is_some() {
        return;
    }
    // Set flag NOW (before publishing OpsRepoUpdated) to close the race window.
    let _ = crate::demo::demo_project::upsert_setting(
        &state.pool,
        "demo_v1_prod_promoted",
        &serde_json::json!(true),
    )
    .await;

    tracing::info!(%project_id, "demo: auto-promoting v0.1 staging → production");

    let ops_repo = sqlx::query("SELECT id, repo_path, branch FROM ops_repos WHERE project_id = $1")
        .bind(project_id)
        .fetch_optional(&state.pool)
        .await;

    let Ok(Some(ops)) = ops_repo else { return };
    let ops_id: uuid::Uuid = ops.get("id");
    let ops_path_str: String = ops.get("repo_path");
    let ops_branch: String = ops.get("branch");
    let ops_path = std::path::PathBuf::from(&ops_path_str);

    // Read staging values for image_ref
    let Ok(staging_values) = platform_ops_repo::read_values(&ops_path, "staging", "staging").await
    else {
        return;
    };
    let image_ref = staging_values["image_ref"]
        .as_str()
        .unwrap_or("")
        .to_string();

    // Merge staging → production branch in the ops repo.
    let merge_result = platform_ops_repo::merge_branch(&ops_path, "staging", &ops_branch).await;

    if let Ok(new_sha) = merge_result {
        let _ = platform_types::events::publish(
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

/// Production rolling completed → spawn PR2 creation.
async fn demo_mark_prod_complete(state: &PlatformState, project_id: uuid::Uuid) {
    let already = crate::demo::demo_project::get_setting(&state.pool, "demo_v1_pr2_created")
        .await
        .ok()
        .flatten();
    if already.is_some() {
        return;
    }
    let _ = crate::demo::demo_project::upsert_setting(
        &state.pool,
        "demo_v1_pr2_created",
        &serde_json::json!(true),
    )
    .await;

    tracing::info!(%project_id, "demo: production v0.1 complete, creating PR2");

    // Get project owner for PR2 creation
    let owner_id: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT owner_id FROM projects WHERE id = $1")
            .bind(project_id)
            .fetch_optional(&state.pool)
            .await
            .ok()
            .flatten();

    if let Some(owner_id) = owner_id {
        let state = state.clone();
        tokio::spawn(async move {
            crate::demo::demo_project::create_demo_pr2(&state, project_id, owner_id).await;
        });
    }
}
