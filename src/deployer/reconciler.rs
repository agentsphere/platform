use std::time::Duration;

use uuid::Uuid;

use crate::store::AppState;

use super::error::DeployerError;
use super::{applier, ops_repo, renderer};

// ---------------------------------------------------------------------------
// Background reconciliation loop
// ---------------------------------------------------------------------------

/// Background task that polls for pending deployments and reconciles them.
pub async fn run(state: AppState, mut shutdown: tokio::sync::watch::Receiver<()>) {
    tracing::info!("deployer reconciler started");

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                tracing::info!("deployer reconciler shutting down");
                break;
            }
            () = tokio::time::sleep(Duration::from_secs(10)) => {
                if let Err(e) = reconcile(&state).await {
                    tracing::error!(error = %e, "error polling pending deployments");
                }
            }
        }
    }
}

/// Find deployments needing reconciliation and spawn tasks for each.
async fn reconcile(state: &AppState) -> Result<(), DeployerError> {
    let pending = sqlx::query!(
        r#"
        SELECT d.id, d.project_id, d.environment, d.ops_repo_id,
               d.manifest_path, d.image_ref, d.values_override,
               d.desired_status, d.current_status, d.deployed_by,
               p.name as "project_name!: String"
        FROM deployments d
        JOIN projects p ON p.id = d.project_id AND p.is_active = true
        WHERE (d.desired_status = 'active' AND d.current_status IN ('pending', 'failed'))
           OR (d.desired_status = 'rollback' AND d.current_status != 'syncing')
           OR (d.desired_status = 'stopped' AND d.current_status NOT IN ('healthy', 'syncing'))
        LIMIT 5
        "#,
    )
    .fetch_all(&state.pool)
    .await?;

    for row in pending {
        let state = state.clone();
        let deployment = PendingDeployment {
            id: row.id,
            project_id: row.project_id,
            environment: row.environment,
            ops_repo_id: row.ops_repo_id,
            manifest_path: row.manifest_path,
            image_ref: row.image_ref,
            values_override: row.values_override,
            desired_status: row.desired_status,
            deployed_by: row.deployed_by,
            project_name: row.project_name,
        };

        tokio::spawn(async move {
            if let Err(e) = reconcile_one(&state, &deployment).await {
                tracing::error!(
                    error = %e,
                    deployment_id = %deployment.id,
                    "reconciliation failed"
                );
                mark_failed(
                    &state,
                    deployment.id,
                    deployment.deployed_by,
                    &e.to_string(),
                )
                .await;
            }
        });
    }

    Ok(())
}

struct PendingDeployment {
    id: Uuid,
    project_id: Uuid,
    environment: String,
    ops_repo_id: Option<Uuid>,
    manifest_path: Option<String>,
    image_ref: String,
    values_override: Option<serde_json::Value>,
    desired_status: String,
    deployed_by: Option<Uuid>,
    project_name: String,
}

// ---------------------------------------------------------------------------
// Single deployment reconciliation
// ---------------------------------------------------------------------------

/// Claim and reconcile a single deployment. Uses optimistic locking to prevent
/// double-processing.
async fn reconcile_one(
    state: &AppState,
    deployment: &PendingDeployment,
) -> Result<(), DeployerError> {
    // Claim with optimistic lock
    let claimed = sqlx::query_scalar!(
        r#"
        UPDATE deployments SET current_status = 'syncing'
        WHERE id = $1 AND current_status != 'syncing'
        RETURNING id
        "#,
        deployment.id,
    )
    .fetch_optional(&state.pool)
    .await?;

    if claimed.is_none() {
        tracing::debug!(deployment_id = %deployment.id, "deployment already being processed");
        return Ok(());
    }

    match deployment.desired_status.as_str() {
        "active" => handle_active(state, deployment).await,
        "rollback" => handle_rollback(state, deployment).await,
        "stopped" => handle_stopped(state, deployment).await,
        other => {
            tracing::warn!(deployment_id = %deployment.id, desired = other, "unknown desired_status");
            Ok(())
        }
    }
}

/// Deploy the current `image_ref` using ops repo manifests.
async fn handle_active(
    state: &AppState,
    deployment: &PendingDeployment,
) -> Result<(), DeployerError> {
    let (rendered, sha) = render_manifests(state, deployment).await?;
    let applied = applier::apply(&state.kube, &rendered, &state.config.pipeline_namespace).await?;

    // Wait for health if a Deployment resource was applied
    if let Some(deploy_name) = applier::find_deployment_name(&applied) {
        applier::wait_healthy(
            &state.kube,
            &state.config.pipeline_namespace,
            deploy_name,
            Duration::from_secs(300),
        )
        .await?;
    }

    finalize_success(state, deployment, sha.as_deref(), "deploy").await?;
    fire_webhook(state, deployment, "deployed").await;
    Ok(())
}

/// Rollback to the previous successful `image_ref`.
async fn handle_rollback(
    state: &AppState,
    deployment: &PendingDeployment,
) -> Result<(), DeployerError> {
    // Find the previous successful deployment (skip the most recent)
    let prev = sqlx::query_scalar!(
        r#"
        SELECT image_ref FROM deployment_history
        WHERE deployment_id = $1 AND status = 'success' AND action = 'deploy'
        ORDER BY created_at DESC LIMIT 1 OFFSET 1
        "#,
        deployment.id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or(DeployerError::NoPreviousDeployment)?;

    // Update the deployment's image_ref to the rollback target
    sqlx::query!(
        "UPDATE deployments SET image_ref = $2 WHERE id = $1",
        deployment.id,
        prev,
    )
    .execute(&state.pool)
    .await?;

    // Create a modified deployment with the rollback image
    let rollback_deployment = PendingDeployment {
        image_ref: prev,
        ..copy_deployment_fields(deployment)
    };

    let (rendered, sha) = render_manifests(state, &rollback_deployment).await?;
    let applied = applier::apply(&state.kube, &rendered, &state.config.pipeline_namespace).await?;

    if let Some(deploy_name) = applier::find_deployment_name(&applied) {
        applier::wait_healthy(
            &state.kube,
            &state.config.pipeline_namespace,
            deploy_name,
            Duration::from_secs(300),
        )
        .await?;
    }

    // Reset desired_status to active after successful rollback
    sqlx::query!(
        "UPDATE deployments SET desired_status = 'active' WHERE id = $1",
        deployment.id,
    )
    .execute(&state.pool)
    .await?;

    finalize_success(state, &rollback_deployment, sha.as_deref(), "rollback").await?;
    fire_webhook(state, deployment, "rolled_back").await;
    Ok(())
}

/// Stop the deployment by scaling to 0 replicas.
async fn handle_stopped(
    state: &AppState,
    deployment: &PendingDeployment,
) -> Result<(), DeployerError> {
    // Derive deployment name from project_name + environment
    let deploy_name = format!("{}-{}", deployment.project_name, deployment.environment);

    applier::scale(
        &state.kube,
        &state.config.pipeline_namespace,
        &deploy_name,
        0,
    )
    .await?;

    finalize_success(state, deployment, None, "stop").await?;
    fire_webhook(state, deployment, "stopped").await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Render manifests from ops repo template or generate a basic deployment manifest.
async fn render_manifests(
    state: &AppState,
    deployment: &PendingDeployment,
) -> Result<(String, Option<String>), DeployerError> {
    if let Some(ops_repo_id) = deployment.ops_repo_id {
        let sha = ops_repo::sync_repo(
            &state.pool,
            &state.valkey,
            &state.config.ops_repos_path,
            ops_repo_id,
        )
        .await?;

        // Look up ops repo details for path resolution
        let repo = sqlx::query!(
            "SELECT name, path FROM ops_repos WHERE id = $1",
            ops_repo_id,
        )
        .fetch_one(&state.pool)
        .await?;

        let manifest_file = deployment.manifest_path.as_deref().unwrap_or("deploy.yaml");

        let template_path = ops_repo::resolve_manifest_path(
            &state.config.ops_repos_path,
            &repo.name,
            &repo.path,
            manifest_file,
        )?;

        let template_content = tokio::fs::read_to_string(&template_path)
            .await
            .map_err(|e| {
                DeployerError::RenderFailed(format!(
                    "failed to read template {}: {e}",
                    template_path.display()
                ))
            })?;

        let vars = renderer::RenderVars {
            image_ref: deployment.image_ref.clone(),
            project_name: deployment.project_name.clone(),
            environment: deployment.environment.clone(),
            values: deployment
                .values_override
                .clone()
                .unwrap_or(serde_json::json!({})),
        };

        let rendered = renderer::render(&template_content, &vars)?;
        Ok((rendered, Some(sha)))
    } else {
        // No ops repo: generate a basic deployment manifest
        let manifest = generate_basic_manifest(deployment);
        Ok((manifest, None))
    }
}

/// Generate a minimal K8s Deployment manifest when no ops repo is configured.
fn generate_basic_manifest(deployment: &PendingDeployment) -> String {
    let name = format!("{}-{}", deployment.project_name, deployment.environment);
    format!(
        "apiVersion: apps/v1\n\
         kind: Deployment\n\
         metadata:\n\
         \x20 name: {name}\n\
         spec:\n\
         \x20 replicas: 1\n\
         \x20 selector:\n\
         \x20   matchLabels:\n\
         \x20     app: {name}\n\
         \x20 template:\n\
         \x20   metadata:\n\
         \x20     labels:\n\
         \x20       app: {name}\n\
         \x20   spec:\n\
         \x20     containers:\n\
         \x20     - name: app\n\
         \x20       image: {}\n\
         \x20       ports:\n\
         \x20       - containerPort: 8080\n",
        deployment.image_ref,
    )
}

/// Update deployment status to healthy and write a success history entry.
async fn finalize_success(
    state: &AppState,
    deployment: &PendingDeployment,
    sha: Option<&str>,
    action: &str,
) -> Result<(), DeployerError> {
    sqlx::query!(
        r#"
        UPDATE deployments
        SET current_status = 'healthy', deployed_at = now(), current_sha = $2
        WHERE id = $1
        "#,
        deployment.id,
        sha,
    )
    .execute(&state.pool)
    .await?;

    sqlx::query!(
        r#"
        INSERT INTO deployment_history
            (deployment_id, image_ref, ops_repo_sha, action, status, deployed_by)
        VALUES ($1, $2, $3, $4, 'success', $5)
        "#,
        deployment.id,
        deployment.image_ref,
        sha,
        action,
        deployment.deployed_by,
    )
    .execute(&state.pool)
    .await?;

    tracing::info!(
        deployment_id = %deployment.id,
        %action,
        image = %deployment.image_ref,
        "deployment reconciled successfully"
    );
    Ok(())
}

/// Mark a deployment as failed and record a failure history entry.
async fn mark_failed(
    state: &AppState,
    deployment_id: Uuid,
    deployed_by: Option<Uuid>,
    message: &str,
) {
    let _ = sqlx::query!(
        "UPDATE deployments SET current_status = 'failed' WHERE id = $1",
        deployment_id,
    )
    .execute(&state.pool)
    .await;

    let _ = sqlx::query!(
        r#"
        INSERT INTO deployment_history
            (deployment_id, image_ref, action, status, deployed_by, message)
        VALUES ($1, '', 'deploy', 'failure', $2, $3)
        "#,
        deployment_id,
        deployed_by,
        message,
    )
    .execute(&state.pool)
    .await;
}

async fn fire_webhook(state: &AppState, deployment: &PendingDeployment, action: &str) {
    let payload = serde_json::json!({
        "action": action,
        "project_id": deployment.project_id,
        "environment": deployment.environment,
        "image_ref": deployment.image_ref,
    });
    crate::api::webhooks::fire_webhooks(&state.pool, deployment.project_id, "deploy", &payload)
        .await;
}

fn copy_deployment_fields(d: &PendingDeployment) -> PendingDeployment {
    PendingDeployment {
        id: d.id,
        project_id: d.project_id,
        environment: d.environment.clone(),
        ops_repo_id: d.ops_repo_id,
        manifest_path: d.manifest_path.clone(),
        image_ref: d.image_ref.clone(),
        values_override: d.values_override.clone(),
        desired_status: d.desired_status.clone(),
        deployed_by: d.deployed_by,
        project_name: d.project_name.clone(),
    }
}
