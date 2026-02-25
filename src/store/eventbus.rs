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
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Pipeline built an image → commit values to ops repo → publish `OpsRepoUpdated`.
async fn handle_image_built(
    state: &AppState,
    project_id: Uuid,
    environment: &str,
    image_ref: &str,
    _pipeline_id: Uuid,
    _triggered_by: Option<Uuid>,
) -> anyhow::Result<()> {
    // Find the ops repo linked to this project's deployment
    let deployment = sqlx::query!(
        "SELECT ops_repo_id FROM deployments WHERE project_id = $1 AND environment = $2",
        project_id,
        environment,
    )
    .fetch_optional(&state.pool)
    .await?;

    let Some(deployment) = deployment else {
        // No deployment configured for this environment — create one without ops repo
        tracing::info!(
            %project_id,
            %environment,
            %image_ref,
            "no deployment found, creating default"
        );
        sqlx::query!(
            r#"INSERT INTO deployments (project_id, environment, image_ref, desired_status, current_status)
               VALUES ($1, $2, $3, 'active', 'pending')
               ON CONFLICT (project_id, environment)
               DO UPDATE SET image_ref = $3, desired_status = 'active', current_status = 'pending'"#,
            project_id,
            environment,
            image_ref,
        )
        .execute(&state.pool)
        .await?;
        state.deploy_notify.notify_one();
        return Ok(());
    };

    let Some(ops_repo_id) = deployment.ops_repo_id else {
        // Deployment exists but no ops repo — just update image_ref directly
        sqlx::query!(
            "UPDATE deployments SET image_ref = $3, current_status = 'pending' WHERE project_id = $1 AND environment = $2",
            project_id,
            environment,
            image_ref,
        )
        .execute(&state.pool)
        .await?;
        state.deploy_notify.notify_one();
        return Ok(());
    };

    // Look up the ops repo
    let ops_repo = sqlx::query!(
        "SELECT repo_path, branch FROM ops_repos WHERE id = $1",
        ops_repo_id,
    )
    .fetch_one(&state.pool)
    .await?;

    let repo_path = PathBuf::from(&ops_repo.repo_path);

    // Get project name for values
    let project_name = sqlx::query_scalar!(
        "SELECT name FROM projects WHERE id = $1 AND is_active = true",
        project_id,
    )
    .fetch_optional(&state.pool)
    .await?
    .unwrap_or_default();

    // Commit values to the ops repo
    let values = serde_json::json!({
        "image_ref": image_ref,
        "project_name": project_name,
        "environment": environment,
    });

    let commit_sha = crate::deployer::ops_repo::commit_values(
        &repo_path,
        &ops_repo.branch,
        environment,
        &values,
    )
    .await?;

    tracing::info!(
        %project_id,
        %ops_repo_id,
        %commit_sha,
        %image_ref,
        "ops repo updated with new image"
    );

    // Publish OpsRepoUpdated event
    publish(
        &state.valkey,
        &PlatformEvent::OpsRepoUpdated {
            project_id,
            ops_repo_id,
            environment: environment.to_owned(),
            commit_sha,
            image_ref: image_ref.to_owned(),
        },
    )
    .await?;

    Ok(())
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
}
