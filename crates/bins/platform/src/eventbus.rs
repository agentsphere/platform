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
use platform_types::WebhookDispatcher;
use platform_types::events::{EVENTS_CHANNEL, PlatformEvent};
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
async fn handle_event(state: &PlatformState, payload: &str) -> anyhow::Result<()> {
    let event: PlatformEvent = serde_json::from_str(payload)?;
    tracing::debug!(?event, "handling platform event");

    match event {
        // Deploy-related events wake the reconciler
        PlatformEvent::ReleaseCreated { .. }
        | PlatformEvent::ReleasePromoted { .. }
        | PlatformEvent::ReleaseRolledBack { .. }
        | PlatformEvent::TrafficShifted { .. }
        | PlatformEvent::OpsRepoUpdated { .. }
        | PlatformEvent::DeployRequested { .. }
        | PlatformEvent::RollbackRequested { .. } => {
            state.deploy_notify.notify_one();
        }

        // Pipeline events wake the executor
        PlatformEvent::PipelineQueued { .. } => {
            state.pipeline_notify.notify_one();
        }

        // Code push: trigger pipeline + fire webhooks
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

        // Tag push: fire webhooks
        PlatformEvent::TagPushed {
            project_id,
            user_name,
            tag_name,
            commit_sha,
            ..
        } => {
            let webhook = platform_webhook::WebhookDispatch::new(state.pool.clone());
            webhook
                .fire_webhooks(
                    project_id,
                    "tag",
                    &serde_json::json!({
                        "ref": format!("refs/tags/{tag_name}"),
                        "after": commit_sha,
                        "pusher": user_name,
                    }),
                )
                .await;
        }

        // MR branch synced: fire webhooks
        PlatformEvent::MrBranchSynced {
            project_id,
            branch,
            commit_sha,
            ..
        } => {
            let webhook = platform_webhook::WebhookDispatch::new(state.pool.clone());
            webhook
                .fire_webhooks(
                    project_id,
                    "mr",
                    &serde_json::json!({
                        "action": "synchronize",
                        "branch": branch,
                        "head_sha": commit_sha,
                    }),
                )
                .await;
        }

        // Agent session ended: fire webhook
        PlatformEvent::AgentSessionEnded {
            project_id,
            session_id,
            status,
        } => {
            let webhook = platform_webhook::WebhookDispatch::new(state.pool.clone());
            webhook
                .fire_webhooks(
                    project_id,
                    "agent",
                    &serde_json::json!({
                        "action": status,
                        "session_id": session_id,
                        "project_id": project_id,
                    }),
                )
                .await;
        }

        // Pipeline completed: auto-merge on success
        PlatformEvent::PipelineCompleted {
            project_id, status, ..
        } => {
            if status == "success" {
                let merger = platform_git::CliGitMerger::new();
                let repos_path = state.config.git.git_repos_path.clone();
                let handler =
                    platform_git::AutoMergeHandler::new(state.pool.clone(), merger, repos_path);
                use platform_types::MergeRequestHandler;
                handler.try_auto_merge(project_id).await;
            }
        }

        // Alert fired: dispatch notification
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

        // Remaining events — no-op
        _ => {}
    }

    Ok(())
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

/// Handle an alert firing: dispatch in-app notification to the project owner.
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

    // Insert in-app notification
    let subject = format!("[{severity}] {alert_name}");
    if let Err(e) = sqlx::query!(
        r#"INSERT INTO notifications (id, user_id, notification_type, subject, body, channel, ref_type, ref_id)
           VALUES (gen_random_uuid(), $1, 'alert', $2, $3, 'in_app', 'alert_rule', $4)"#,
        owner_id,
        subject,
        message,
        rule_id,
    )
    .execute(&state.pool)
    .await
    {
        tracing::error!(error = %e, %rule_id, "failed to insert alert notification");
    }
}
