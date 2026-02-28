use std::collections::BTreeMap;
use std::time::Duration;

use k8s_openapi::api::core::v1::Secret;
use kube::Api;
use kube::api::PostParams;
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

    let mut interval = tokio::time::interval(Duration::from_secs(10));

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                tracing::info!("deployer reconciler shutting down");
                break;
            }
            _ = interval.tick() => {
                if let Err(e) = reconcile(&state).await {
                    tracing::error!(error = %e, "error polling pending deployments");
                }
            }
            () = state.deploy_notify.notified() => {
                // Immediate poll on notification from event bus
                if let Err(e) = reconcile(&state).await {
                    tracing::error!(error = %e, "error polling pending deployments (notified)");
                }
                interval.reset();
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
               d.tracked_resources,
               p.name as "project_name!: String",
               p.namespace_slug as "namespace_slug!: String"
        FROM deployments d
        JOIN projects p ON p.id = d.project_id AND p.is_active = true
        WHERE (d.desired_status = 'active' AND d.current_status IN ('pending', 'failed'))
           OR (d.desired_status = 'rollback' AND d.current_status != 'syncing')
           OR (d.desired_status = 'stopped' AND d.current_status NOT IN ('stopped', 'syncing'))
        LIMIT 5
        "#,
    )
    .fetch_all(&state.pool)
    .await?;

    for row in pending {
        let state = state.clone();
        let (tracked, skip_prune) = match serde_json::from_value::<Vec<applier::TrackedResource>>(
            row.tracked_resources.clone(),
        ) {
            Ok(t) => (t, false),
            Err(e) => {
                tracing::warn!(
                    deployment_id = %row.id,
                    error = %e,
                    "failed to parse tracked_resources, skipping prune"
                );
                (Vec::new(), true)
            }
        };
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
            namespace_slug: row.namespace_slug,
            tracked_resources: tracked,
            skip_prune,
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

#[derive(Debug)]
pub struct PendingDeployment {
    pub id: Uuid,
    pub project_id: Uuid,
    pub environment: String,
    pub ops_repo_id: Option<Uuid>,
    pub manifest_path: Option<String>,
    pub image_ref: String,
    pub values_override: Option<serde_json::Value>,
    pub desired_status: String,
    pub deployed_by: Option<Uuid>,
    pub project_name: String,
    pub namespace_slug: String,
    pub tracked_resources: Vec<applier::TrackedResource>,
    /// When true, skip orphan pruning (`tracked_resources` failed to parse).
    pub skip_prune: bool,
}

/// Map environment name to K8s-safe suffix.
fn env_suffix(environment: &str) -> &str {
    match environment {
        "production" => "prod",
        other => other,
    }
}

/// Resolve the target K8s namespace for a deployment.
/// Maps `production` → `{slug}-prod`, `staging` → `{slug}-staging`, etc.
pub fn target_namespace(namespace_slug: &str, environment: &str) -> String {
    format!("{namespace_slug}-{}", env_suffix(environment))
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
    let ns = target_namespace(&deployment.namespace_slug, &deployment.environment);

    // Ensure the target namespace exists before applying
    crate::deployer::namespace::ensure_namespace(
        &state.kube,
        &deployment.namespace_slug,
        env_suffix(&deployment.environment),
        &deployment.project_id.to_string(),
    )
    .await?;

    // Inject project secrets as a K8s Secret before applying manifests
    inject_project_secrets(state, deployment, &ns).await;

    let (rendered, sha) = render_manifests(state, deployment).await?;

    // Build new resource inventory before applying
    let new_tracked = applier::build_tracked_inventory(&rendered, &ns);

    // Prune orphaned resources (removed from manifests since last apply).
    // Skip if tracked_resources failed to deserialize (R3: avoid accidental mass deletion).
    if !deployment.skip_prune {
        let orphans = applier::find_orphans(&deployment.tracked_resources, &new_tracked);
        if !orphans.is_empty() {
            let pruned = applier::prune_orphans(&state.kube, &orphans).await?;
            tracing::info!(pruned_count = pruned, "orphaned resources pruned");
        }
    }

    // Apply with tracking labels
    let applied =
        applier::apply_with_tracking(&state.kube, &rendered, &ns, Some(deployment.id)).await?;

    // Store the new resource inventory
    store_tracked_resources(state, deployment.id, &new_tracked).await?;

    // Wait for health if a Deployment resource was applied
    if let Some(deploy_name) = applier::find_deployment_name(&applied) {
        applier::wait_healthy(&state.kube, &ns, deploy_name, Duration::from_secs(300)).await?;
    }

    finalize_success(state, deployment, sha.as_deref(), "deploy").await?;
    fire_webhook(state, deployment, "deployed").await;
    Ok(())
}

/// Rollback to the previous successful `image_ref`.
/// If an ops repo is linked, reverts the last ops repo commit to restore
/// the previous values file, then re-deploys.
async fn handle_rollback(
    state: &AppState,
    deployment: &PendingDeployment,
) -> Result<(), DeployerError> {
    let rollback_image = if let Some(ops_repo_id) = deployment.ops_repo_id {
        // Ops-repo-centric rollback: revert the last commit
        let (repo_path, _sha, branch) = ops_repo::sync_repo(&state.pool, ops_repo_id).await?;

        let new_sha = ops_repo::revert_last_commit(&repo_path, &branch).await?;

        // Read the reverted values to get the old image_ref
        let reverted = ops_repo::read_values(&repo_path, &branch, &deployment.environment)
            .await
            .map_err(|_| DeployerError::NoPreviousDeployment)?;

        let old_image = reverted["image_ref"]
            .as_str()
            .ok_or(DeployerError::NoPreviousDeployment)?
            .to_owned();

        // Update DB to match the reverted ops repo state
        sqlx::query!(
            "UPDATE deployments SET image_ref = $2, current_sha = $3 WHERE id = $1",
            deployment.id,
            old_image,
            new_sha,
        )
        .execute(&state.pool)
        .await?;

        old_image
    } else {
        // Legacy DB-based rollback: look up previous image from history
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

        sqlx::query!(
            "UPDATE deployments SET image_ref = $2 WHERE id = $1",
            deployment.id,
            prev,
        )
        .execute(&state.pool)
        .await?;

        prev
    };

    // Create a modified deployment with the rollback image
    let rollback_deployment = PendingDeployment {
        image_ref: rollback_image,
        ..copy_deployment_fields(deployment)
    };

    let ns = target_namespace(&deployment.namespace_slug, &deployment.environment);
    let (rendered, sha) = render_manifests(state, &rollback_deployment).await?;
    let applied =
        applier::apply_with_tracking(&state.kube, &rendered, &ns, Some(deployment.id)).await?;

    // Update tracked resources for rollback
    let new_tracked = applier::build_tracked_inventory(&rendered, &ns);
    store_tracked_resources(state, deployment.id, &new_tracked).await?;

    if let Some(deploy_name) = applier::find_deployment_name(&applied) {
        applier::wait_healthy(&state.kube, &ns, deploy_name, Duration::from_secs(300)).await?;
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
    let ns = target_namespace(&deployment.namespace_slug, &deployment.environment);
    let deploy_name = format!("{}-{}", deployment.project_name, deployment.environment);

    applier::scale(&state.kube, &ns, &deploy_name, 0).await?;

    finalize_success(state, deployment, None, "stop").await?;
    fire_webhook(state, deployment, "stopped").await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Query deploy-scoped secrets for a project, decrypt, inject OTEL config,
/// and create/update a K8s Secret in the target namespace. Best-effort.
#[tracing::instrument(skip(state, deployment), fields(project_id = %deployment.project_id, %namespace))]
async fn inject_project_secrets(state: &AppState, deployment: &PendingDeployment, namespace: &str) {
    let mut data: BTreeMap<String, String> = BTreeMap::new();

    // Collect deploy-scoped secrets (requires master key)
    if let Some(master_key) = state
        .config
        .master_key
        .as_deref()
        .and_then(|k| crate::secrets::engine::parse_master_key(k).ok())
    {
        match crate::secrets::engine::query_scoped_secrets(
            &state.pool,
            &master_key,
            deployment.project_id,
            &["deploy", "all"],
            Some(&deployment.environment),
        )
        .await
        {
            Ok(secrets) => {
                for (name, value) in secrets {
                    data.insert(name, value);
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, project_id = %deployment.project_id, "failed to query deploy secrets");
            }
        }
    }

    // Inject OTEL configuration for automatic observability
    inject_otel_env_vars(state, deployment, &mut data).await;

    if data.is_empty() {
        return;
    }

    let secret_name = format!(
        "{}-{}-secrets",
        deployment.namespace_slug,
        env_suffix(&deployment.environment)
    );

    apply_k8s_secret(state, namespace, &secret_name, deployment.project_id, data).await;
}

/// Inject OTEL env vars into the secrets data map for automatic observability.
#[tracing::instrument(skip(state, data), fields(project_id = %deployment.project_id))]
async fn inject_otel_env_vars(
    state: &AppState,
    deployment: &PendingDeployment,
    data: &mut BTreeMap<String, String>,
) {
    data.insert(
        "OTEL_EXPORTER_OTLP_ENDPOINT".into(),
        state.config.platform_api_url.clone(),
    );
    data.insert("OTEL_SERVICE_NAME".into(), deployment.project_name.clone());

    // Auto-create or rotate a scoped OTLP token
    match ensure_otlp_token(state, deployment.project_id).await {
        Ok(raw_token) => {
            data.insert(
                "OTEL_EXPORTER_OTLP_HEADERS".into(),
                format!("Authorization=Bearer {raw_token}"),
            );
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                project_id = %deployment.project_id,
                "failed to create OTLP token, skipping OTEL auth header"
            );
        }
    }
}

/// Create or rotate a project-scoped OTLP API token.
///
/// - Scope: `["observe:write"]` (minimal — only allows OTLP ingest)
/// - Project-scoped hard boundary via `project_id`
/// - 365-day expiry; rotated on each deploy
///
/// Uses a transaction with insert-before-delete to avoid any window where
/// no valid token exists (R3 fix).
///
/// Returns the raw token string (never stored in plaintext after this).
#[tracing::instrument(skip(state), fields(%project_id), err)]
pub async fn ensure_otlp_token(state: &AppState, project_id: Uuid) -> anyhow::Result<String> {
    let mut tx = state.pool.begin().await?;

    // Find the project owner to assign the token to
    let owner_id = sqlx::query_scalar!(
        "SELECT owner_id FROM projects WHERE id = $1 AND is_active = true",
        project_id,
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| anyhow::anyhow!("project not found"))?;

    // Check for existing token (we need to delete it after inserting the new one)
    let existing = sqlx::query_scalar!(
        r#"
        SELECT id
        FROM api_tokens
        WHERE project_id = $1
          AND scopes @> ARRAY['observe:write']
          AND (expires_at IS NULL OR expires_at > now())
        ORDER BY created_at DESC
        LIMIT 1
        "#,
        project_id,
    )
    .fetch_optional(&mut *tx)
    .await?;

    // INSERT new token first — ensures a valid token always exists
    let (raw_token, token_hash) = crate::auth::token::generate_api_token();

    sqlx::query!(
        r#"
        INSERT INTO api_tokens (user_id, name, token_hash, scopes, project_id, expires_at)
        VALUES ($1, $2, $3, $4, $5, now() + interval '365 days')
        "#,
        owner_id,
        format!("otlp-auto-{project_id}"),
        token_hash,
        &["observe:write"] as &[&str],
        project_id,
    )
    .execute(&mut *tx)
    .await?;

    // THEN delete the old token to prevent accumulation
    if let Some(old_id) = existing
        && let Err(e) = sqlx::query!("DELETE FROM api_tokens WHERE id = $1", old_id)
            .execute(&mut *tx)
            .await
    {
        tracing::warn!(error = %e, token_id = %old_id, "failed to delete old OTLP token");
    }

    tx.commit().await?;

    tracing::info!(
        %project_id,
        "created OTLP auto-token for project"
    );

    Ok(raw_token)
}

/// Create or replace a K8s Secret in the given namespace.
#[tracing::instrument(skip(state, data), fields(%namespace, %secret_name, %project_id))]
async fn apply_k8s_secret(
    state: &AppState,
    namespace: &str,
    secret_name: &str,
    project_id: Uuid,
    data: BTreeMap<String, String>,
) {
    let k8s_secret = Secret {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(secret_name.to_owned()),
            labels: Some(BTreeMap::from([(
                "platform.io/project".into(),
                project_id.to_string(),
            )])),
            ..Default::default()
        },
        string_data: Some(data),
        type_: Some("Opaque".into()),
        ..Default::default()
    };

    let api: Api<Secret> = Api::namespaced(state.kube.clone(), namespace);
    match api.create(&PostParams::default(), &k8s_secret).await {
        Ok(_) => {
            tracing::info!(%secret_name, %namespace, "created deploy secrets K8s Secret");
        }
        Err(kube::Error::Api(err)) if err.code == 409 => {
            match api
                .replace(secret_name, &PostParams::default(), &k8s_secret)
                .await
            {
                Ok(_) => {
                    tracing::info!(%secret_name, %namespace, "updated deploy secrets K8s Secret");
                }
                Err(e) => {
                    tracing::warn!(error = %e, %secret_name, "failed to update deploy secrets");
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, %secret_name, "failed to create deploy secrets K8s Secret");
        }
    }
}

/// Render manifests from ops repo template or generate a basic deployment manifest.
///
/// When an ops repo is linked, reads the template from the repo's working tree
/// (via git show) and merges in values from the ops repo's values file. The
/// `image_ref` from the values file takes precedence over the DB value when
/// the ops repo is the source of truth.
async fn render_manifests(
    state: &AppState,
    deployment: &PendingDeployment,
) -> Result<(String, Option<String>), DeployerError> {
    if let Some(ops_repo_id) = deployment.ops_repo_id {
        let (repo_path, sha, branch) = ops_repo::sync_repo(&state.pool, ops_repo_id).await?;

        // Look up ops repo details for path resolution
        let repo = sqlx::query!(
            "SELECT name, path FROM ops_repos WHERE id = $1",
            ops_repo_id,
        )
        .fetch_one(&state.pool)
        .await?;

        let manifest_file = deployment.manifest_path.as_deref().unwrap_or("deploy.yaml");

        // Read the template from the bare repo
        let manifest_ref_path = if repo.path == "/" || repo.path.is_empty() {
            manifest_file.to_owned()
        } else {
            let subpath = repo.path.trim_matches('/');
            format!("{subpath}/{manifest_file}")
        };

        let template_content = ops_repo::read_file_at_ref(&repo_path, &branch, &manifest_ref_path)
            .await
            .map_err(|_| {
                DeployerError::RenderFailed(format!(
                    "failed to read template {manifest_ref_path} from ops repo"
                ))
            })?;

        // Try to read values from the ops repo; fall back to DB values
        let ops_values = ops_repo::read_values(&repo_path, &branch, &deployment.environment).await;

        let mut base_values = deployment
            .values_override
            .clone()
            .unwrap_or(serde_json::json!({}));

        // Merge ops repo values into the render context (ops repo takes precedence)
        if let Ok(repo_values) = ops_values
            && let (Some(base_obj), Some(repo_obj)) =
                (base_values.as_object_mut(), repo_values.as_object())
        {
            for (k, v) in repo_obj {
                base_obj.insert(k.clone(), v.clone());
            }
        }

        // image_ref: prefer ops repo values, then DB
        let image_ref = base_values
            .get("image_ref")
            .and_then(|v| v.as_str())
            .map_or_else(|| deployment.image_ref.clone(), String::from);

        let vars = renderer::RenderVars {
            image_ref,
            project_name: deployment.project_name.clone(),
            environment: deployment.environment.clone(),
            values: base_values,
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
pub async fn finalize_success(
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
pub async fn mark_failed(
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

/// Store the tracked resource inventory in the database.
async fn store_tracked_resources(
    state: &AppState,
    deployment_id: Uuid,
    tracked: &[applier::TrackedResource],
) -> Result<(), DeployerError> {
    let json = serde_json::to_value(tracked)
        .map_err(|e| DeployerError::Other(anyhow::anyhow!("serialize tracked: {e}")))?;

    sqlx::query!(
        "UPDATE deployments SET tracked_resources = $2 WHERE id = $1",
        deployment_id,
        json,
    )
    .execute(&state.pool)
    .await?;

    Ok(())
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
        namespace_slug: d.namespace_slug.clone(),
        tracked_resources: d.tracked_resources.clone(),
        skip_prune: d.skip_prune,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_deployment() -> PendingDeployment {
        PendingDeployment {
            id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            environment: "production".into(),
            ops_repo_id: None,
            manifest_path: None,
            image_ref: "registry.example.com/myapp:v1.2.3".into(),
            values_override: None,
            desired_status: "active".into(),
            deployed_by: Some(Uuid::new_v4()),
            project_name: "my-app".into(),
            namespace_slug: "my-app".into(),
            tracked_resources: Vec::new(),
            skip_prune: false,
        }
    }

    #[test]
    fn basic_manifest_has_correct_name() {
        let d = sample_deployment();
        let manifest = generate_basic_manifest(&d);
        assert!(manifest.contains("name: my-app-production"));
    }

    #[test]
    fn basic_manifest_has_correct_image() {
        let d = sample_deployment();
        let manifest = generate_basic_manifest(&d);
        assert!(manifest.contains("image: registry.example.com/myapp:v1.2.3"));
    }

    #[test]
    fn basic_manifest_is_valid_yaml() {
        let d = sample_deployment();
        let manifest = generate_basic_manifest(&d);
        let parsed: serde_yaml::Value =
            serde_yaml::from_str(&manifest).expect("manifest should be valid YAML");
        assert_eq!(parsed["kind"], "Deployment");
        assert_eq!(parsed["apiVersion"], "apps/v1");
        assert_eq!(parsed["spec"]["replicas"], 1);
    }

    #[test]
    fn basic_manifest_container_port_8080() {
        let d = sample_deployment();
        let manifest = generate_basic_manifest(&d);
        assert!(manifest.contains("containerPort: 8080"));
    }

    #[test]
    fn basic_manifest_selector_matches_labels() {
        let d = sample_deployment();
        let manifest = generate_basic_manifest(&d);
        let parsed: serde_yaml::Value = serde_yaml::from_str(&manifest).unwrap();
        let selector_label = &parsed["spec"]["selector"]["matchLabels"]["app"];
        let template_label = &parsed["spec"]["template"]["metadata"]["labels"]["app"];
        assert_eq!(selector_label, template_label);
    }

    #[test]
    fn copy_deployment_fields_preserves_all() {
        let original = sample_deployment();
        let copy = copy_deployment_fields(&original);
        assert_eq!(original.id, copy.id);
        assert_eq!(original.project_id, copy.project_id);
        assert_eq!(original.environment, copy.environment);
        assert_eq!(original.ops_repo_id, copy.ops_repo_id);
        assert_eq!(original.manifest_path, copy.manifest_path);
        assert_eq!(original.image_ref, copy.image_ref);
        assert_eq!(original.values_override, copy.values_override);
        assert_eq!(original.desired_status, copy.desired_status);
        assert_eq!(original.deployed_by, copy.deployed_by);
        assert_eq!(original.project_name, copy.project_name);
    }

    #[test]
    fn basic_manifest_with_special_chars_in_name() {
        let mut d = sample_deployment();
        d.project_name = "my-app-2".into();
        d.environment = "staging-01".into();
        let manifest = generate_basic_manifest(&d);
        assert!(manifest.contains("name: my-app-2-staging-01"));
        // Verify it's still valid YAML
        let _: serde_yaml::Value = serde_yaml::from_str(&manifest).unwrap();
    }

    #[test]
    fn basic_manifest_different_environments() {
        for env in &["production", "staging", "development", "preview-feat-123"] {
            let mut d = sample_deployment();
            d.environment = (*env).to_string();
            let manifest = generate_basic_manifest(&d);
            let expected_name = format!("name: my-app-{env}");
            assert!(
                manifest.contains(&expected_name),
                "manifest should contain '{expected_name}', got: {manifest}"
            );
            // Must be valid YAML
            let _: serde_yaml::Value = serde_yaml::from_str(&manifest).unwrap();
        }
    }

    #[test]
    fn basic_manifest_different_images() {
        for image in &[
            "nginx:latest",
            "registry.io/app:v1.0.0-rc1",
            "ghcr.io/org/repo:sha-abc123",
        ] {
            let mut d = sample_deployment();
            d.image_ref = (*image).to_string();
            let manifest = generate_basic_manifest(&d);
            assert!(manifest.contains(&format!("image: {image}")));
        }
    }

    #[test]
    fn copy_deployment_fields_independent_of_original() {
        let original = sample_deployment();
        let mut copy = copy_deployment_fields(&original);
        copy.image_ref = "modified:v2".into();
        copy.environment = "staging".into();
        // Original should be unchanged
        assert_eq!(original.image_ref, "registry.example.com/myapp:v1.2.3");
        assert_eq!(original.environment, "production");
    }

    #[test]
    fn pending_deployment_with_ops_repo() {
        let d = PendingDeployment {
            id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            environment: "production".into(),
            ops_repo_id: Some(Uuid::new_v4()),
            manifest_path: Some("deploy.yaml".into()),
            image_ref: "registry/app:v1".into(),
            values_override: Some(serde_json::json!({"replicas": 3})),
            desired_status: "active".into(),
            deployed_by: Some(Uuid::new_v4()),
            project_name: "my-project".into(),
            namespace_slug: "my-project".into(),
            tracked_resources: Vec::new(),
            skip_prune: false,
        };

        assert!(d.ops_repo_id.is_some());
        assert!(d.manifest_path.is_some());
        assert!(d.values_override.is_some());
        assert!(d.deployed_by.is_some());
    }

    #[test]
    fn pending_deployment_minimal() {
        let d = PendingDeployment {
            id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            environment: "production".into(),
            ops_repo_id: None,
            manifest_path: None,
            image_ref: "app:latest".into(),
            values_override: None,
            desired_status: "active".into(),
            deployed_by: None,
            project_name: "app".into(),
            namespace_slug: "app".into(),
            tracked_resources: Vec::new(),
            skip_prune: false,
        };

        assert!(d.ops_repo_id.is_none());
        assert!(d.manifest_path.is_none());
        assert!(d.values_override.is_none());
        assert!(d.deployed_by.is_none());
    }

    #[test]
    fn basic_manifest_container_name_is_app() {
        let d = sample_deployment();
        let manifest = generate_basic_manifest(&d);
        let parsed: serde_yaml::Value = serde_yaml::from_str(&manifest).unwrap();
        let container_name = &parsed["spec"]["template"]["spec"]["containers"][0]["name"];
        assert_eq!(container_name, "app");
    }

    #[test]
    fn copy_deployment_preserves_ops_repo_id() {
        let mut d = sample_deployment();
        d.ops_repo_id = Some(Uuid::new_v4());
        d.manifest_path = Some("custom/deploy.yaml".into());
        d.values_override = Some(serde_json::json!({"key": "value"}));
        let copy = copy_deployment_fields(&d);
        assert_eq!(d.ops_repo_id, copy.ops_repo_id);
        assert_eq!(d.manifest_path, copy.manifest_path);
        assert_eq!(d.values_override, copy.values_override);
    }

    #[test]
    fn basic_manifest_labels_consistent() {
        let d = sample_deployment();
        let manifest = generate_basic_manifest(&d);
        let parsed: serde_yaml::Value = serde_yaml::from_str(&manifest).unwrap();

        // selector.matchLabels.app must equal template.metadata.labels.app
        let selector = &parsed["spec"]["selector"]["matchLabels"]["app"];
        let template = &parsed["spec"]["template"]["metadata"]["labels"]["app"];
        let metadata_name = &parsed["metadata"]["name"];
        assert_eq!(selector, template);
        assert_eq!(selector.as_str(), metadata_name.as_str());
    }

    #[test]
    fn target_namespace_production() {
        assert_eq!(target_namespace("my-app", "production"), "my-app-prod");
    }

    #[test]
    fn target_namespace_staging() {
        assert_eq!(target_namespace("my-app", "staging"), "my-app-staging");
    }

    #[test]
    fn target_namespace_preview() {
        assert_eq!(
            target_namespace("my-app", "preview-feat-123"),
            "my-app-preview-feat-123"
        );
    }
}
