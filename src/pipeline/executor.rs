use std::collections::BTreeMap;
use std::time::Instant;

use base64::Engine;
use k8s_openapi::api::core::v1::{
    Capabilities, Container, EmptyDirVolumeSource, EnvVar, LocalObjectReference, Pod,
    PodSecurityContext, PodSpec, Secret, SecretVolumeSource, SecurityContext, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::Api;
use kube::api::{DeleteParams, ListParams, LogParams, PostParams};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::token;
use crate::store::AppState;

use super::error::PipelineError;

// ---------------------------------------------------------------------------
// Background executor loop
// ---------------------------------------------------------------------------

/// Background task that polls for pending pipelines and executes them.
pub async fn run(state: AppState, mut shutdown: tokio::sync::watch::Receiver<()>) {
    tracing::info!("pipeline executor started");

    let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));

    state.task_registry.register("pipeline_executor", 10);

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                tracing::info!("pipeline executor shutting down");
                break;
            }
            _ = interval.tick() => {
                match poll_pending(&state).await {
                    Ok(()) => state.task_registry.heartbeat("pipeline_executor"),
                    Err(e) => {
                        state.task_registry.report_error("pipeline_executor", &e.to_string());
                        tracing::error!(error = %e, "error polling pending pipelines");
                    }
                }
            }
            () = state.pipeline_notify.notified() => {
                // Immediate poll on notification
                if let Err(e) = poll_pending(&state).await {
                    tracing::error!(error = %e, "error polling pending pipelines (notified)");
                }
                // Reset interval to avoid immediate double-poll
                interval.reset();
            }
        }
    }
}

/// Find pending pipelines and spawn execution tasks.
async fn poll_pending(state: &AppState) -> Result<(), PipelineError> {
    let pending = sqlx::query_scalar!(
        r#"
        SELECT id FROM pipelines
        WHERE status = 'pending'
        ORDER BY created_at ASC
        LIMIT 5
        "#,
    )
    .fetch_all(&state.pool)
    .await?;

    for pipeline_id in pending {
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = execute_pipeline(&state, pipeline_id).await {
                tracing::error!(error = %e, %pipeline_id, "pipeline execution failed");
                let _ = mark_pipeline_failed(&state.pool, pipeline_id).await;
            }
        });
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Pipeline execution
// ---------------------------------------------------------------------------

/// Execute a single pipeline: run each step as a K8s pod sequentially.
#[tracing::instrument(skip(state), fields(%pipeline_id), err)]
async fn execute_pipeline(state: &AppState, pipeline_id: Uuid) -> Result<(), PipelineError> {
    // Claim the pipeline by setting status to running
    let claimed = sqlx::query_scalar!(
        r#"
        UPDATE pipelines SET status = 'running', started_at = now()
        WHERE id = $1 AND status = 'pending'
        RETURNING project_id
        "#,
        pipeline_id,
    )
    .fetch_optional(&state.pool)
    .await?;

    let Some(project_id) = claimed else {
        tracing::debug!(%pipeline_id, "pipeline already claimed");
        return Ok(());
    };

    // Load pipeline metadata
    let pipeline = sqlx::query!(
        r#"
        SELECT pl.git_ref as "git_ref!: String",
               pl.commit_sha,
               pl.triggered_by,
               p.name as "project_name!: String",
               p.namespace_slug as "namespace_slug!: String",
               u.name as "owner_name!: String"
        FROM pipelines pl
        JOIN projects p ON p.id = pl.project_id
        JOIN users u ON u.id = p.owner_id
        WHERE pl.id = $1
        "#,
        pipeline_id,
    )
    .fetch_one(&state.pool)
    .await?;

    let platform_api_url = state.config.platform_api_url.trim_end_matches('/');
    let repo_clone_url = format!(
        "{}/{}/{}.git",
        platform_api_url, pipeline.owner_name, pipeline.project_name,
    );

    // Create a short-lived git auth token for HTTP clone (scoped to this project)
    let git_token =
        create_git_auth_token(state, pipeline_id, project_id, pipeline.triggered_by).await?;

    let meta = PipelineMeta {
        git_ref: pipeline.git_ref,
        commit_sha: pipeline.commit_sha,
        project_name: pipeline.project_name,
        repo_clone_url,
        git_auth_token: git_token.0,
        namespace: state
            .config
            .project_namespace(&pipeline.namespace_slug, "dev"),
    };

    // Ensure project namespace exists (lazy creation for DB-only projects)
    ensure_project_namespace(state, &meta.namespace, project_id).await?;

    // Create registry auth Secret if registry is configured and we know who triggered it
    let registry_creds = if state.config.registry_url.is_some() {
        if let Some(user_id) = pipeline.triggered_by {
            match create_registry_secret(state, pipeline_id, user_id, &meta.namespace).await {
                Ok(creds) => Some(creds),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to create registry secret, continuing without");
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    let registry_secret_name = registry_creds.as_ref().map(|(name, _)| name.as_str());
    let all_succeeded =
        run_all_steps(state, pipeline_id, project_id, &meta, registry_secret_name).await?;

    // Clean up registry auth Secret + token
    if let Some((_, ref token_hash)) = registry_creds {
        cleanup_registry_secret(state, pipeline_id, token_hash, &meta.namespace).await;
    }

    // Clean up git auth token
    cleanup_git_auth_token(state, &git_token.1).await;

    // Finalize pipeline
    let final_status = if all_succeeded { "success" } else { "failure" };
    sqlx::query!(
        "UPDATE pipelines SET status = $2, finished_at = now() WHERE id = $1",
        pipeline_id,
        final_status,
    )
    .execute(&state.pool)
    .await?;

    if all_succeeded {
        detect_and_write_deployment(state, pipeline_id, project_id).await;
        detect_and_publish_dev_image(state, pipeline_id, project_id).await;
    }

    fire_build_webhook(&state.pool, project_id, pipeline_id, final_status).await;
    tracing::info!(%pipeline_id, status = final_status, "pipeline finished");
    Ok(())
}

/// Parameters extracted from pipeline + project join query.
struct PipelineMeta {
    git_ref: String,
    commit_sha: Option<String>,
    project_name: String,
    repo_clone_url: String,
    /// Short-lived API token for authenticating git clone via `GIT_ASKPASS`.
    git_auth_token: String,
    /// K8s namespace for this pipeline's pods (e.g. `{slug}-dev`).
    namespace: String,
}

/// A pipeline step row loaded from the database.
struct StepRow {
    id: Uuid,
    step_order: i32,
    name: String,
    image: String,
    commands: Vec<String>,
}

/// Ensure the project's dev namespace (and network policy) exist before running pods.
/// Idempotent — no-op if the namespace was already created by `setup_project_infrastructure`.
async fn ensure_project_namespace(
    state: &AppState,
    namespace: &str,
    project_id: Uuid,
) -> Result<(), PipelineError> {
    crate::deployer::namespace::ensure_namespace(
        &state.kube,
        namespace,
        "dev",
        &project_id.to_string(),
    )
    .await
    .map_err(|e| PipelineError::Other(e.into()))?;
    if !state.config.dev_mode {
        let _ = crate::deployer::namespace::ensure_network_policy(
            &state.kube,
            namespace,
            &state.config.platform_namespace,
        )
        .await;
    }
    Ok(())
}

/// Run all steps for a pipeline. Returns true if all steps succeeded.
async fn run_all_steps(
    state: &AppState,
    pipeline_id: Uuid,
    project_id: Uuid,
    pipeline: &PipelineMeta,
    registry_secret: Option<&str>,
) -> Result<bool, PipelineError> {
    let steps = sqlx::query_as!(
        StepRow,
        r#"
        SELECT id, step_order, name, image, commands
        FROM pipeline_steps
        WHERE pipeline_id = $1
        ORDER BY step_order ASC
        "#,
        pipeline_id,
    )
    .fetch_all(&state.pool)
    .await?;

    let pods: Api<Pod> = Api::namespaced(state.kube.clone(), &pipeline.namespace);

    for step in &steps {
        if is_cancelled(&state.pool, pipeline_id).await? {
            skip_remaining_steps(&state.pool, pipeline_id).await?;
            return Ok(false);
        }

        let succeeded = execute_single_step(
            state,
            &pods,
            pipeline_id,
            project_id,
            pipeline,
            step,
            registry_secret,
        )
        .await?;

        if !succeeded {
            skip_remaining_after(&state.pool, pipeline_id, step.step_order).await?;
            return Ok(false);
        }
    }

    Ok(true)
}

/// Execute one pipeline step as a K8s pod. Returns true on success.
async fn execute_single_step(
    state: &AppState,
    pods: &Api<Pod>,
    pipeline_id: Uuid,
    project_id: Uuid,
    pipeline: &PipelineMeta,
    step: &StepRow,
    registry_secret: Option<&str>,
) -> Result<bool, PipelineError> {
    let env_vars = build_env_vars(
        state,
        pipeline_id,
        project_id,
        &pipeline.project_name,
        &pipeline.git_ref,
        pipeline.commit_sha.as_deref(),
        &step.name,
    );

    let pod_name = format!("pl-{}-{}", &pipeline_id.to_string()[..8], slug(&step.name));
    let pod_spec = build_pod_spec(&PodSpecParams {
        pod_name: &pod_name,
        pipeline_id,
        project_id,
        step_name: &step.name,
        image: &step.image,
        commands: &step.commands,
        env_vars: &env_vars,
        repo_clone_url: &pipeline.repo_clone_url,
        git_ref: &pipeline.git_ref,
        registry_secret,
        git_auth_token: &pipeline.git_auth_token,
    });

    sqlx::query!(
        "UPDATE pipeline_steps SET status = 'running' WHERE id = $1",
        step.id
    )
    .execute(&state.pool)
    .await?;

    let start = Instant::now();
    let result = run_step(pods, &pod_name, &pod_spec, state, pipeline_id, &step.name).await;
    let duration_ms = i32::try_from(start.elapsed().as_millis()).unwrap_or(i32::MAX);

    match result {
        Ok(exit_code) => {
            let status = if exit_code == 0 { "success" } else { "failure" };
            let log_ref = format!("logs/pipelines/{pipeline_id}/{}.log", step.name);
            sqlx::query!(
                r#"UPDATE pipeline_steps SET status = $2, exit_code = $3, duration_ms = $4, log_ref = $5 WHERE id = $1"#,
                step.id, status, exit_code, duration_ms, log_ref,
            )
            .execute(&state.pool)
            .await?;
            Ok(exit_code == 0)
        }
        Err(e) => {
            tracing::error!(error = %e, step = %step.name, "step execution error");
            sqlx::query!(
                "UPDATE pipeline_steps SET status = 'failure', duration_ms = $2 WHERE id = $1",
                step.id,
                duration_ms,
            )
            .execute(&state.pool)
            .await?;
            Ok(false)
        }
    }
}

// ---------------------------------------------------------------------------
// Pod execution
// ---------------------------------------------------------------------------

/// Create a K8s pod, wait for completion, capture logs, clean up. Returns exit code.
async fn run_step(
    pods: &Api<Pod>,
    pod_name: &str,
    pod_spec: &Pod,
    state: &AppState,
    pipeline_id: Uuid,
    step_name: &str,
) -> Result<i32, PipelineError> {
    // Create the pod
    pods.create(&PostParams::default(), pod_spec).await?;

    // Wait for pod to finish
    let exit_code = wait_for_pod(pods, pod_name).await?;

    // Capture logs to MinIO
    capture_logs(pods, pod_name, state, pipeline_id, step_name).await;

    // Clean up pod
    let _ = pods.delete(pod_name, &DeleteParams::default()).await;

    Ok(exit_code)
}

/// Poll pod status until it reaches a terminal phase.
async fn wait_for_pod(pods: &Api<Pod>, pod_name: &str) -> Result<i32, PipelineError> {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;

        let pod = match pods.get(pod_name).await {
            Ok(p) => p,
            Err(kube::Error::Api(err)) if err.code == 404 => {
                return Err(PipelineError::Other(anyhow::anyhow!(
                    "pod {pod_name} disappeared"
                )));
            }
            Err(e) => return Err(e.into()),
        };

        let Some(status) = &pod.status else {
            continue;
        };
        let phase = status.phase.as_deref().unwrap_or("Unknown");

        match phase {
            "Succeeded" => return Ok(0),
            "Failed" => {
                let exit_code = extract_exit_code(status).unwrap_or(1);
                return Ok(exit_code);
            }
            "Pending" | "Running" => {}
            other => {
                tracing::warn!(pod = pod_name, phase = other, "unexpected pod phase");
            }
        }
    }
}

/// Extract the exit code from the first container's termination state.
fn extract_exit_code(status: &k8s_openapi::api::core::v1::PodStatus) -> Option<i32> {
    status
        .container_statuses
        .as_ref()?
        .first()?
        .state
        .as_ref()?
        .terminated
        .as_ref()
        .map(|t| t.exit_code)
}

/// Capture pod logs and write them to `MinIO`.
async fn capture_logs(
    pods: &Api<Pod>,
    pod_name: &str,
    state: &AppState,
    pipeline_id: Uuid,
    step_name: &str,
) {
    // Capture init container (clone) logs for debugging
    let init_log_params = LogParams {
        container: Some("clone".into()),
        ..Default::default()
    };
    match pods.logs(pod_name, &init_log_params).await {
        Ok(logs) => {
            let path = format!("logs/pipelines/{pipeline_id}/{step_name}-clone.log");
            if let Err(e) = state.minio.write(&path, logs.into_bytes()).await {
                tracing::error!(error = %e, %path, "failed to write clone logs to MinIO");
            }
        }
        Err(e) => {
            tracing::debug!(error = %e, pod = pod_name, "no clone init container logs");
        }
    }

    // Capture main step container logs
    let log_params = LogParams {
        container: Some("step".into()),
        ..Default::default()
    };

    match pods.logs(pod_name, &log_params).await {
        Ok(logs) => {
            let path = format!("logs/pipelines/{pipeline_id}/{step_name}.log");
            if let Err(e) = state.minio.write(&path, logs.into_bytes()).await {
                tracing::error!(error = %e, %path, "failed to write logs to MinIO");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, pod = pod_name, "failed to read pod logs");
        }
    }
}

// ---------------------------------------------------------------------------
// Git auth token for HTTP clone
// ---------------------------------------------------------------------------

/// Create a short-lived API token so pipeline pods can clone via HTTP.
/// The token is scoped to the given `project_id` to limit blast radius.
/// Returns `(raw_token, token_hash)`.
async fn create_git_auth_token(
    state: &AppState,
    pipeline_id: Uuid,
    project_id: Uuid,
    triggered_by: Option<Uuid>,
) -> Result<(String, String), PipelineError> {
    // Use the triggering user, or fall back to looking up the project owner
    let user_id = if let Some(uid) = triggered_by {
        uid
    } else {
        sqlx::query_scalar!("SELECT owner_id FROM projects WHERE id = $1", project_id)
            .fetch_one(&state.pool)
            .await?
    };

    let (raw_token, token_hash) = token::generate_api_token();

    sqlx::query!(
        r#"INSERT INTO api_tokens (id, user_id, name, token_hash, project_id, expires_at)
           VALUES ($1, $2, $3, $4, $5, now() + interval '1 hour')"#,
        Uuid::new_v4(),
        user_id,
        format!("pipeline-git-{pipeline_id}"),
        token_hash,
        project_id,
    )
    .execute(&state.pool)
    .await?;

    tracing::debug!(%pipeline_id, %project_id, "created project-scoped git auth token for pipeline clone");
    Ok((raw_token, token_hash))
}

/// Clean up the short-lived git auth token after the pipeline finishes.
async fn cleanup_git_auth_token(state: &AppState, token_hash: &str) {
    if let Err(e) = sqlx::query!("DELETE FROM api_tokens WHERE token_hash = $1", token_hash)
        .execute(&state.pool)
        .await
    {
        tracing::warn!(error = %e, "failed to delete pipeline git auth token");
    }
}

// ---------------------------------------------------------------------------
// Registry auth Secret for pipeline pods
// ---------------------------------------------------------------------------

/// Create a short-lived API token and a K8s Secret containing Docker config
/// JSON so that Kaniko/buildah steps can authenticate with the platform registry.
///
/// Returns `(secret_name, token_hash)` — the token hash is needed to clean up
/// the DB row after the pipeline finishes.
async fn create_registry_secret(
    state: &AppState,
    pipeline_id: Uuid,
    triggered_by: Uuid,
    namespace: &str,
) -> Result<(String, String), PipelineError> {
    let registry_url = state
        .config
        .registry_url
        .as_deref()
        .ok_or_else(|| PipelineError::Other(anyhow::anyhow!("registry_url not configured")))?;

    // Create a short-lived API token (1 hour) for the triggering user
    let (raw_token, token_hash) = token::generate_api_token();

    sqlx::query!(
        r#"INSERT INTO api_tokens (id, user_id, name, token_hash, expires_at)
           VALUES ($1, $2, $3, $4, now() + interval '1 hour')"#,
        Uuid::new_v4(),
        triggered_by,
        format!("pipeline-{pipeline_id}"),
        token_hash,
    )
    .execute(&state.pool)
    .await?;

    // Look up the username for Docker config
    let user_name = sqlx::query_scalar!("SELECT name FROM users WHERE id = $1", triggered_by)
        .fetch_one(&state.pool)
        .await?;

    // Build Docker config JSON with auth for both registry URLs (push + pull)
    let basic_auth =
        base64::engine::general_purpose::STANDARD.encode(format!("{user_name}:{raw_token}"));
    let auth_entry = serde_json::json!({ "auth": basic_auth });
    let mut auths = serde_json::Map::new();
    auths.insert(registry_url.to_owned(), auth_entry.clone());
    // Also add node_registry_url if different (`DaemonSet` proxy for containerd pulls)
    if let Some(node_url) = node_registry_url(&state.config)
        && node_url != registry_url
    {
        auths.insert(node_url.to_owned(), auth_entry);
    }
    let config_json = serde_json::json!({ "auths": auths });

    let secret_name = format!("pl-registry-{}", &pipeline_id.to_string()[..8]);

    let secret = Secret {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(secret_name.clone()),
            labels: Some(BTreeMap::from([(
                "platform.io/pipeline".into(),
                pipeline_id.to_string(),
            )])),
            ..Default::default()
        },
        string_data: Some(BTreeMap::from([(
            "config.json".into(),
            config_json.to_string(),
        )])),
        type_: Some("Opaque".into()),
        ..Default::default()
    };

    let secrets: Api<Secret> = Api::namespaced(state.kube.clone(), namespace);
    secrets.create(&PostParams::default(), &secret).await?;

    tracing::debug!(%pipeline_id, %secret_name, "created registry auth secret");
    Ok((secret_name, token_hash))
}

/// Clean up the registry auth K8s Secret and the short-lived API token.
async fn cleanup_registry_secret(
    state: &AppState,
    pipeline_id: Uuid,
    token_hash: &str,
    namespace: &str,
) {
    let secret_name = format!("pl-registry-{}", &pipeline_id.to_string()[..8]);

    // Delete the K8s Secret
    let secrets: Api<Secret> = Api::namespaced(state.kube.clone(), namespace);
    if let Err(e) = secrets.delete(&secret_name, &DeleteParams::default()).await {
        tracing::warn!(error = %e, %secret_name, "failed to delete registry auth secret");
    }

    // Delete the short-lived API token from the DB
    if let Err(e) = sqlx::query!("DELETE FROM api_tokens WHERE token_hash = $1", token_hash)
        .execute(&state.pool)
        .await
    {
        tracing::warn!(error = %e, "failed to delete pipeline API token");
    }
}

// ---------------------------------------------------------------------------
// Pod spec builder
// ---------------------------------------------------------------------------

struct PodSpecParams<'a> {
    pod_name: &'a str,
    pipeline_id: Uuid,
    project_id: Uuid,
    step_name: &'a str,
    image: &'a str,
    commands: &'a [String],
    env_vars: &'a [EnvVar],
    /// HTTP clone URL (e.g. `http://platform:8080/owner/repo.git`).
    repo_clone_url: &'a str,
    git_ref: &'a str,
    /// K8s Secret name containing Docker config JSON for registry auth.
    registry_secret: Option<&'a str>,
    /// Short-lived API token for authenticating git clone via `GIT_ASKPASS`.
    git_auth_token: &'a str,
}

/// Build the volumes and step container mounts for a pipeline pod.
fn build_volumes_and_mounts(registry_secret: Option<&str>) -> (Vec<Volume>, Vec<VolumeMount>) {
    let mut step_mounts = vec![VolumeMount {
        name: "workspace".into(),
        mount_path: "/workspace".into(),
        ..Default::default()
    }];

    let mut volumes = vec![Volume {
        name: "workspace".into(),
        empty_dir: Some(EmptyDirVolumeSource::default()),
        ..Default::default()
    }];

    // If a registry auth Secret is provided, mount it as Docker config
    if let Some(secret_name) = registry_secret {
        volumes.push(Volume {
            name: "docker-config".into(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(secret_name.into()),
                ..Default::default()
            }),
            ..Default::default()
        });
        step_mounts.push(VolumeMount {
            name: "docker-config".into(),
            mount_path: "/kaniko/.docker".into(),
            read_only: Some(true),
            ..Default::default()
        });
    }

    (volumes, step_mounts)
}

fn build_pod_spec(p: &PodSpecParams<'_>) -> Pod {
    let script = p.commands.join(" && ");

    let labels = BTreeMap::from([
        ("platform.io/pipeline".into(), p.pipeline_id.to_string()),
        ("platform.io/step".into(), slug(p.step_name)),
        ("platform.io/project".into(), p.project_id.to_string()),
    ]);

    // Strip refs/heads/ prefix for git clone --branch
    let branch = p
        .git_ref
        .strip_prefix("refs/heads/")
        .or_else(|| p.git_ref.strip_prefix("refs/tags/"))
        .unwrap_or(p.git_ref);

    let (volumes, step_mounts) = build_volumes_and_mounts(p.registry_secret);

    Pod {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(p.pod_name.into()),
            labels: Some(labels),
            ..Default::default()
        },
        spec: Some(PodSpec {
            restart_policy: Some("Never".into()),
            // Note: no run_as_non_root/run_as_user — kaniko needs root to
            // build container images.  fs_group ensures shared volume perms.
            security_context: Some(PodSecurityContext {
                fs_group: Some(1000),
                ..Default::default()
            }),
            image_pull_secrets: p.registry_secret.map(|name| {
                vec![LocalObjectReference {
                    name: name.to_string(),
                }]
            }),
            init_containers: Some(vec![Container {
                name: "clone".into(),
                image: Some("alpine/git:latest".into()),
                command: Some(vec!["sh".into(), "-c".into()]),
                args: Some(vec![format!(
                    "printf '#!/bin/sh\\necho \"$GIT_AUTH_TOKEN\"\\n' > /tmp/git-askpass.sh && \
                     chmod +x /tmp/git-askpass.sh && \
                     GIT_ASKPASS=/tmp/git-askpass.sh \
                     git clone --depth 1 --branch \"$GIT_BRANCH\" {} /workspace 2>&1",
                    p.repo_clone_url,
                )]),
                env: Some(vec![
                    env_var("GIT_AUTH_TOKEN", p.git_auth_token),
                    env_var("GIT_BRANCH", branch),
                ]),
                volume_mounts: Some(vec![VolumeMount {
                    name: "workspace".into(),
                    mount_path: "/workspace".into(),
                    ..Default::default()
                }]),
                security_context: Some(container_security()),
                ..Default::default()
            }]),
            containers: vec![Container {
                name: "step".into(),
                image: Some(p.image.into()),
                command: Some(vec!["sh".into(), "-c".into()]),
                args: Some(vec![script]),
                working_dir: Some("/workspace".into()),
                env: Some(p.env_vars.to_vec()),
                volume_mounts: Some(step_mounts),
                // No restrictive security context on step containers — kaniko
                // and other build tools need root + capabilities (CHOWN, etc.)
                // to unpack base image layers and build containers.
                resources: Some(k8s_openapi::api::core::v1::ResourceRequirements {
                    limits: Some(BTreeMap::from([
                        ("cpu".into(), Quantity("1".into())),
                        ("memory".into(), Quantity("1Gi".into())),
                    ])),
                    requests: Some(BTreeMap::from([
                        ("cpu".into(), Quantity("250m".into())),
                        ("memory".into(), Quantity("256Mi".into())),
                    ])),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            volumes: Some(volumes),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Registry URL as seen from K8s nodes (for image refs in pod specs).
/// Prefers `registry_node_url` (`DaemonSet` proxy), falls back to `registry_url`.
fn node_registry_url(config: &crate::config::Config) -> Option<&str> {
    config
        .registry_node_url
        .as_deref()
        .or(config.registry_url.as_deref())
}

fn build_env_vars(
    state: &AppState,
    pipeline_id: Uuid,
    project_id: Uuid,
    project_name: &str,
    git_ref: &str,
    commit_sha: Option<&str>,
    step_name: &str,
) -> Vec<EnvVar> {
    build_env_vars_core(
        pipeline_id,
        project_id,
        project_name,
        git_ref,
        commit_sha,
        step_name,
        // REGISTRY env var for Kaniko push — uses the actual platform API URL
        // (not the node proxy) since Kaniko pushes from inside the pod.
        state.config.registry_url.as_deref(),
    )
}

/// Core env var builder with no dependency on `AppState`.
fn build_env_vars_core(
    pipeline_id: Uuid,
    project_id: Uuid,
    project_name: &str,
    git_ref: &str,
    commit_sha: Option<&str>,
    step_name: &str,
    registry_url: Option<&str>,
) -> Vec<EnvVar> {
    let branch = git_ref.strip_prefix("refs/heads/").unwrap_or(git_ref);

    let mut vars = vec![
        env_var("PLATFORM_PROJECT_ID", &project_id.to_string()),
        env_var("PLATFORM_PROJECT_NAME", project_name),
        env_var("PIPELINE_ID", &pipeline_id.to_string()),
        env_var("STEP_NAME", step_name),
        env_var("COMMIT_REF", git_ref),
        env_var("COMMIT_BRANCH", branch),
        env_var("PROJECT", project_name),
    ];

    if let Some(sha) = commit_sha {
        vars.push(env_var("COMMIT_SHA", sha));
    }

    if let Some(registry) = registry_url {
        vars.push(env_var("REGISTRY", registry));
        // Kaniko and buildah look for Docker config at $DOCKER_CONFIG/config.json
        vars.push(env_var("DOCKER_CONFIG", "/kaniko/.docker"));
    }

    vars
}

fn env_var(name: &str, value: &str) -> EnvVar {
    EnvVar {
        name: name.into(),
        value: Some(value.into()),
        ..Default::default()
    }
}

/// Hardened security context for all containers: drop all capabilities, no privilege escalation.
fn container_security() -> SecurityContext {
    SecurityContext {
        allow_privilege_escalation: Some(false),
        capabilities: Some(Capabilities {
            drop: Some(vec!["ALL".into()]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Status helpers
// ---------------------------------------------------------------------------

async fn is_cancelled(pool: &PgPool, pipeline_id: Uuid) -> Result<bool, PipelineError> {
    let status = sqlx::query_scalar!("SELECT status FROM pipelines WHERE id = $1", pipeline_id,)
        .fetch_one(pool)
        .await?;

    Ok(status == "cancelled")
}

async fn skip_remaining_steps(pool: &PgPool, pipeline_id: Uuid) -> Result<(), PipelineError> {
    sqlx::query!(
        "UPDATE pipeline_steps SET status = 'skipped' WHERE pipeline_id = $1 AND status = 'pending'",
        pipeline_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn skip_remaining_after(
    pool: &PgPool,
    pipeline_id: Uuid,
    after_order: i32,
) -> Result<(), PipelineError> {
    sqlx::query!(
        r#"
        UPDATE pipeline_steps SET status = 'skipped'
        WHERE pipeline_id = $1 AND step_order > $2 AND status = 'pending'
        "#,
        pipeline_id,
        after_order,
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn mark_pipeline_failed(pool: &PgPool, pipeline_id: Uuid) -> Result<(), PipelineError> {
    sqlx::query!(
        "UPDATE pipelines SET status = 'failure', finished_at = now() WHERE id = $1 AND status IN ('pending', 'running')",
        pipeline_id,
    )
    .execute(pool)
    .await?;

    skip_remaining_steps(pool, pipeline_id).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Deployment handoff
// ---------------------------------------------------------------------------

/// If any step used a kaniko-like image, publish an `ImageBuilt` event (for production)
/// or directly upsert a preview deployment (for non-main branches).
async fn detect_and_write_deployment(state: &AppState, pipeline_id: Uuid, project_id: Uuid) {
    let dev_step_name = super::trigger::DEV_IMAGE_STEP_NAME;
    let image_steps = sqlx::query!(
        r#"
        SELECT name, image FROM pipeline_steps
        WHERE pipeline_id = $1 AND status = 'success'
          AND image ILIKE '%kaniko%' AND name != $2
        "#,
        pipeline_id,
        dev_step_name,
    )
    .fetch_all(&state.pool)
    .await;

    let Ok(image_steps) = image_steps else {
        return;
    };

    if image_steps.is_empty() {
        return;
    }

    // Get the git_ref, commit SHA, and triggered_by for the pipeline
    let pipeline_meta = sqlx::query!(
        "SELECT git_ref, commit_sha, triggered_by FROM pipelines WHERE id = $1",
        pipeline_id,
    )
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();

    let Some(pipeline_meta) = pipeline_meta else {
        return;
    };

    let project_name = sqlx::query_scalar!("SELECT name FROM projects WHERE id = $1", project_id)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten();

    // Use node_registry_url for image refs (containerd pulls from this URL)
    let registry = node_registry_url(&state.config).unwrap_or("localhost:5000");
    let name = project_name.as_deref().unwrap_or("unknown");
    let tag = pipeline_meta.commit_sha.as_deref().unwrap_or("latest");
    let image_ref = format!("{registry}/{name}:{tag}");

    // Extract branch from git_ref
    let branch = pipeline_meta
        .git_ref
        .strip_prefix("refs/heads/")
        .unwrap_or(&pipeline_meta.git_ref);

    let is_main = matches!(branch, "main" | "master");

    if is_main {
        // Publish ImageBuilt event — the event bus handler will commit to
        // the ops repo and trigger deployment.
        let event = crate::store::eventbus::PlatformEvent::ImageBuilt {
            project_id,
            environment: "production".into(),
            image_ref: image_ref.clone(),
            pipeline_id,
            triggered_by: pipeline_meta.triggered_by,
        };
        if let Err(e) = crate::store::eventbus::publish(&state.valkey, &event).await {
            tracing::error!(error = %e, %project_id, "failed to publish ImageBuilt event");
        }

        tracing::info!(%project_id, %image_ref, "ImageBuilt event published from pipeline");
    } else {
        // Preview deployments bypass the event bus (no ops repo)
        if let Err(e) = upsert_preview_deployment(
            state,
            pipeline_id,
            project_id,
            branch,
            &image_ref,
            pipeline_meta.triggered_by,
        )
        .await
        {
            tracing::error!(error = %e, %project_id, %branch, "failed to upsert preview deployment");
        }
    }
}

/// If a `build-dev-image` step succeeded, publish a `DevImageBuilt` event so
/// the project's `agent_image` is updated.
#[tracing::instrument(skip(state), fields(%pipeline_id, %project_id))]
async fn detect_and_publish_dev_image(state: &AppState, pipeline_id: Uuid, project_id: Uuid) {
    let dev_step_name = super::trigger::DEV_IMAGE_STEP_NAME;
    let dev_step = sqlx::query_scalar!(
        r#"SELECT id FROM pipeline_steps
           WHERE pipeline_id = $1 AND status = 'success' AND name = $2"#,
        pipeline_id,
        dev_step_name,
    )
    .fetch_optional(&state.pool)
    .await;

    let Ok(Some(_)) = dev_step else { return };

    let commit_sha = sqlx::query_scalar!(
        "SELECT commit_sha FROM pipelines WHERE id = $1",
        pipeline_id,
    )
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten()
    .flatten();

    let project_name = sqlx::query_scalar!("SELECT name FROM projects WHERE id = $1", project_id)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten();

    // Use node_registry_url for image refs (containerd pulls from this URL)
    let registry = node_registry_url(&state.config).unwrap_or("localhost:5000");
    let name = project_name.as_deref().unwrap_or("unknown");
    let tag = commit_sha.as_deref().unwrap_or("latest");
    let dev_image_ref = format!("{registry}/{name}-dev:{tag}");

    let event = crate::store::eventbus::PlatformEvent::DevImageBuilt {
        project_id,
        image_ref: dev_image_ref.clone(),
        pipeline_id,
    };
    if let Err(e) = crate::store::eventbus::publish(&state.valkey, &event).await {
        tracing::error!(error = %e, %project_id, "failed to publish DevImageBuilt event");
    }

    tracing::info!(%project_id, %dev_image_ref, "DevImageBuilt event published from pipeline");
}

/// Create or update a preview deployment for a non-main branch.
#[tracing::instrument(skip(state), fields(%pipeline_id, %project_id, %branch), err)]
async fn upsert_preview_deployment(
    state: &AppState,
    pipeline_id: Uuid,
    project_id: Uuid,
    branch: &str,
    image_ref: &str,
    triggered_by: Option<Uuid>,
) -> Result<(), anyhow::Error> {
    let branch_slug = crate::pipeline::slugify_branch(branch);

    sqlx::query!(
        r#"INSERT INTO preview_deployments
            (project_id, branch, branch_slug, image_ref, pipeline_id, created_by)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (project_id, branch_slug) DO UPDATE SET
            image_ref = EXCLUDED.image_ref,
            pipeline_id = EXCLUDED.pipeline_id,
            desired_status = 'active',
            current_status = 'pending',
            expires_at = now() + (preview_deployments.ttl_hours || ' hours')::interval,
            updated_at = now()"#,
        project_id,
        branch,
        branch_slug,
        image_ref,
        pipeline_id,
        triggered_by,
    )
    .execute(&state.pool)
    .await?;

    tracing::info!(
        %project_id,
        %branch,
        slug = %branch_slug,
        image = %image_ref,
        "preview deployment upserted"
    );

    // Fire webhook for preview event
    crate::api::webhooks::fire_webhooks(
        &state.pool,
        project_id,
        "deploy",
        &serde_json::json!({
            "action": "preview_created",
            "branch": branch,
            "branch_slug": branch_slug,
            "image_ref": image_ref,
            "pipeline_id": pipeline_id,
        }),
    )
    .await;

    Ok(())
}

// ---------------------------------------------------------------------------
// Webhook
// ---------------------------------------------------------------------------

async fn fire_build_webhook(pool: &PgPool, project_id: Uuid, pipeline_id: Uuid, status: &str) {
    let payload = serde_json::json!({
        "action": status,
        "pipeline_id": pipeline_id,
        "project_id": project_id,
    });
    crate::api::webhooks::fire_webhooks(pool, project_id, "build", &payload).await;
}

// ---------------------------------------------------------------------------
// Cancellation (called from API)
// ---------------------------------------------------------------------------

/// Cancel a running pipeline: delete K8s pods and mark as cancelled.
#[tracing::instrument(skip(state), fields(%pipeline_id), err)]
pub async fn cancel_pipeline(state: &AppState, pipeline_id: Uuid) -> Result<(), PipelineError> {
    // Mark pipeline as cancelled
    sqlx::query!(
        "UPDATE pipelines SET status = 'cancelled', finished_at = now() WHERE id = $1 AND status IN ('pending', 'running')",
        pipeline_id,
    )
    .execute(&state.pool)
    .await?;

    skip_remaining_steps(&state.pool, pipeline_id).await?;

    // Look up project namespace for pod deletion
    let namespace = sqlx::query_scalar!(
        r#"
        SELECT p.namespace_slug as "namespace_slug!: String"
        FROM pipelines pl JOIN projects p ON p.id = pl.project_id
        WHERE pl.id = $1
        "#,
        pipeline_id,
    )
    .fetch_optional(&state.pool)
    .await?
    .map_or_else(
        || state.config.pipeline_namespace.clone(),
        |slug| state.config.project_namespace(&slug, "dev"),
    );

    // Delete running pods by label selector
    let pods: Api<Pod> = Api::namespaced(state.kube.clone(), &namespace);
    let label = format!("platform.io/pipeline={pipeline_id}");
    let lp = ListParams::default().labels(&label);

    if let Ok(pod_list) = pods.list(&lp).await {
        for pod in pod_list {
            if let Some(name) = pod.metadata.name {
                let _ = pods.delete(&name, &DeleteParams::default()).await;
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

use super::slug;

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{
        ContainerState, ContainerStateRunning, ContainerStateTerminated, ContainerStateWaiting,
        ContainerStatus, PodStatus,
    };

    // -- test-only helpers for kaniko detection / branch classification --

    fn is_kaniko_image(image: &str) -> bool {
        image.to_ascii_lowercase().contains("kaniko")
    }

    fn classify_branch(branch: &str) -> &'static str {
        if matches!(branch, "main" | "master") {
            "production"
        } else {
            "preview"
        }
    }

    fn build_image_ref(registry: &str, project_name: &str, tag: &str) -> String {
        format!("{registry}/{project_name}:{tag}")
    }

    // -- slug --

    #[test]
    fn slug_simple() {
        assert_eq!(slug("test"), "test");
    }

    #[test]
    fn slug_uppercase() {
        assert_eq!(slug("Build-Image"), "build-image");
    }

    #[test]
    fn slug_special_chars() {
        assert_eq!(slug("my step (1)"), "my-step--1");
    }

    #[test]
    fn slug_leading_trailing_special() {
        assert_eq!(slug("--test--"), "test");
    }

    #[test]
    fn slug_empty() {
        assert_eq!(slug(""), "");
    }

    #[test]
    fn slug_all_special() {
        assert_eq!(slug("!!!"), "");
    }

    // -- extract_exit_code --

    #[test]
    fn exit_code_from_terminated_container() {
        let status = PodStatus {
            container_statuses: Some(vec![ContainerStatus {
                name: "step".into(),
                ready: false,
                restart_count: 0,
                image: String::new(),
                image_id: String::new(),
                state: Some(ContainerState {
                    terminated: Some(ContainerStateTerminated {
                        exit_code: 42,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            ..Default::default()
        };
        assert_eq!(extract_exit_code(&status), Some(42));
    }

    #[test]
    fn exit_code_none_when_no_container_statuses() {
        let status = PodStatus {
            container_statuses: None,
            ..Default::default()
        };
        assert_eq!(extract_exit_code(&status), None);
    }

    #[test]
    fn exit_code_none_when_empty_statuses() {
        let status = PodStatus {
            container_statuses: Some(vec![]),
            ..Default::default()
        };
        assert_eq!(extract_exit_code(&status), None);
    }

    #[test]
    fn exit_code_none_when_no_terminated_state() {
        let status = PodStatus {
            container_statuses: Some(vec![ContainerStatus {
                name: "step".into(),
                ready: false,
                restart_count: 0,
                image: String::new(),
                image_id: String::new(),
                state: Some(ContainerState {
                    running: Some(ContainerStateRunning::default()),
                    terminated: None,
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            ..Default::default()
        };
        assert_eq!(extract_exit_code(&status), None);
    }

    #[test]
    fn exit_code_none_when_no_state() {
        let status = PodStatus {
            container_statuses: Some(vec![ContainerStatus {
                name: "step".into(),
                ready: false,
                restart_count: 0,
                image: String::new(),
                image_id: String::new(),
                state: None,
                ..Default::default()
            }]),
            ..Default::default()
        };
        assert_eq!(extract_exit_code(&status), None);
    }

    // -- build_pod_spec --

    #[test]
    fn build_pod_spec_structure() {
        let pipeline_id = Uuid::nil();
        let project_id = Uuid::nil();
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test-build",
            pipeline_id,
            project_id,
            step_name: "build",
            image: "rust:latest",
            commands: &["cargo build".into(), "cargo test".into()],
            env_vars: &[env_var("FOO", "bar")],
            repo_clone_url: "http://platform:8080/owner/repo.git",
            git_ref: "refs/heads/main",
            registry_secret: None,
            git_auth_token: "test-token",
        });

        assert_eq!(pod.metadata.name.as_deref(), Some("pl-test-build"));

        let labels = pod.metadata.labels.as_ref().unwrap();
        assert_eq!(labels["platform.io/step"], "build");

        let spec = pod.spec.as_ref().unwrap();
        assert_eq!(spec.restart_policy.as_deref(), Some("Never"));

        let init = &spec.init_containers.as_ref().unwrap()[0];
        assert_eq!(init.image.as_deref(), Some("alpine/git:latest"));

        let container = &spec.containers[0];
        assert_eq!(container.image.as_deref(), Some("rust:latest"));
        assert_eq!(
            container.args.as_ref().unwrap()[0],
            "cargo build && cargo test"
        );

        let limits = container
            .resources
            .as_ref()
            .unwrap()
            .limits
            .as_ref()
            .unwrap();
        assert_eq!(limits["cpu"], Quantity("1".into()));
        assert_eq!(limits["memory"], Quantity("1Gi".into()));

        let volumes = spec.volumes.as_ref().unwrap();
        assert_eq!(volumes.len(), 1);
        assert_eq!(volumes[0].name, "workspace");
    }

    #[test]
    fn build_pod_spec_strips_refs_heads_prefix() {
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test",
            pipeline_id: Uuid::nil(),
            project_id: Uuid::nil(),
            step_name: "test",
            image: "alpine:3.19",
            commands: &["echo hello".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/test.git",
            git_ref: "refs/heads/feature-branch",
            registry_secret: None,
            git_auth_token: "test-token",
        });

        let spec = pod.spec.unwrap();
        let init = &spec.init_containers.unwrap()[0];
        // Command references $GIT_BRANCH env var (not literal branch)
        let clone_cmd = &init.args.as_ref().unwrap()[0];
        assert!(
            clone_cmd.contains("$GIT_BRANCH"),
            "should reference $GIT_BRANCH env var, got: {clone_cmd}"
        );
        // GIT_BRANCH env var has the stripped value
        let env = init.env.as_ref().unwrap();
        let branch_env = env.iter().find(|e| e.name == "GIT_BRANCH").unwrap();
        assert_eq!(
            branch_env.value.as_deref(),
            Some("feature-branch"),
            "should strip refs/heads/ prefix"
        );
    }

    #[test]
    fn build_pod_spec_strips_refs_tags_prefix() {
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test",
            pipeline_id: Uuid::nil(),
            project_id: Uuid::nil(),
            step_name: "test",
            image: "alpine:3.19",
            commands: &["echo hello".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/test.git",
            git_ref: "refs/tags/v1.0",
            registry_secret: None,
            git_auth_token: "test-token",
        });

        let spec = pod.spec.unwrap();
        let init = &spec.init_containers.unwrap()[0];
        let env = init.env.as_ref().unwrap();
        let branch_env = env.iter().find(|e| e.name == "GIT_BRANCH").unwrap();
        assert_eq!(
            branch_env.value.as_deref(),
            Some("v1.0"),
            "should strip refs/tags/ prefix"
        );
    }

    #[test]
    fn build_pod_spec_bare_ref_used_as_is() {
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test",
            pipeline_id: Uuid::nil(),
            project_id: Uuid::nil(),
            step_name: "test",
            image: "alpine:3.19",
            commands: &["echo hello".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/test.git",
            git_ref: "main",
            registry_secret: None,
            git_auth_token: "test-token",
        });

        let spec = pod.spec.unwrap();
        let init = &spec.init_containers.unwrap()[0];
        let env = init.env.as_ref().unwrap();
        let branch_env = env.iter().find(|e| e.name == "GIT_BRANCH").unwrap();
        assert_eq!(
            branch_env.value.as_deref(),
            Some("main"),
            "bare ref should be used directly"
        );
    }

    #[test]
    fn build_pod_spec_empty_commands_produce_empty_script() {
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test",
            pipeline_id: Uuid::nil(),
            project_id: Uuid::nil(),
            step_name: "test",
            image: "alpine:3.19",
            commands: &[],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/test.git",
            git_ref: "main",
            registry_secret: None,
            git_auth_token: "test-token",
        });

        let container = &pod.spec.unwrap().containers[0];
        let script = &container.args.as_ref().unwrap()[0];
        assert!(
            script.is_empty(),
            "empty commands should produce empty script"
        );
    }

    #[test]
    fn build_pod_spec_resource_requests() {
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test",
            pipeline_id: Uuid::nil(),
            project_id: Uuid::nil(),
            step_name: "test",
            image: "alpine:3.19",
            commands: &["true".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/test.git",
            git_ref: "main",
            registry_secret: None,
            git_auth_token: "test-token",
        });

        let container = &pod.spec.unwrap().containers[0];
        let requests = container
            .resources
            .as_ref()
            .unwrap()
            .requests
            .as_ref()
            .unwrap();
        assert_eq!(requests["cpu"], Quantity("250m".into()));
        assert_eq!(requests["memory"], Quantity("256Mi".into()));
    }

    #[test]
    fn build_pod_spec_working_dir_is_workspace() {
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test",
            pipeline_id: Uuid::nil(),
            project_id: Uuid::nil(),
            step_name: "test",
            image: "alpine:3.19",
            commands: &["true".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/test.git",
            git_ref: "main",
            registry_secret: None,
            git_auth_token: "test-token",
        });

        let container = &pod.spec.unwrap().containers[0];
        assert_eq!(container.working_dir.as_deref(), Some("/workspace"));
    }

    #[test]
    fn build_pod_spec_labels_include_all_three() {
        let pipeline_id = Uuid::nil();
        let project_id = Uuid::max();
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test",
            pipeline_id,
            project_id,
            step_name: "build",
            image: "alpine:3.19",
            commands: &["true".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/test.git",
            git_ref: "main",
            registry_secret: None,
            git_auth_token: "test-token",
        });

        let labels = pod.metadata.labels.as_ref().unwrap();
        assert_eq!(labels["platform.io/pipeline"], pipeline_id.to_string());
        assert_eq!(labels["platform.io/project"], project_id.to_string());
        assert_eq!(labels["platform.io/step"], "build");
    }

    // -- build_env_vars_core --

    fn find_env(vars: &[EnvVar], name: &str) -> Option<String> {
        vars.iter()
            .find(|v| v.name == name)
            .and_then(|v| v.value.clone())
    }

    #[test]
    fn env_vars_include_all_seven_standard_vars() {
        let vars = build_env_vars_core(
            Uuid::nil(),
            Uuid::nil(),
            "my-project",
            "refs/heads/main",
            None,
            "build",
            None,
        );
        assert!(find_env(&vars, "PLATFORM_PROJECT_ID").is_some());
        assert!(find_env(&vars, "PLATFORM_PROJECT_NAME").is_some());
        assert!(find_env(&vars, "PIPELINE_ID").is_some());
        assert!(find_env(&vars, "STEP_NAME").is_some());
        assert!(find_env(&vars, "COMMIT_REF").is_some());
        assert!(find_env(&vars, "COMMIT_BRANCH").is_some());
        assert!(find_env(&vars, "PROJECT").is_some());
    }

    #[test]
    fn env_vars_commit_sha_present_when_some() {
        let vars = build_env_vars_core(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "refs/heads/main",
            Some("abc123"),
            "test",
            None,
        );
        assert_eq!(find_env(&vars, "COMMIT_SHA"), Some("abc123".into()));
    }

    #[test]
    fn env_vars_commit_sha_absent_when_none() {
        let vars = build_env_vars_core(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "refs/heads/main",
            None,
            "test",
            None,
        );
        assert!(find_env(&vars, "COMMIT_SHA").is_none());
    }

    #[test]
    fn env_vars_registry_present_when_configured() {
        let vars = build_env_vars_core(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "main",
            None,
            "test",
            Some("registry.example.com"),
        );
        assert_eq!(
            find_env(&vars, "REGISTRY"),
            Some("registry.example.com".into())
        );
    }

    #[test]
    fn env_vars_registry_absent_when_none() {
        let vars =
            build_env_vars_core(Uuid::nil(), Uuid::nil(), "proj", "main", None, "test", None);
        assert!(find_env(&vars, "REGISTRY").is_none());
    }

    #[test]
    fn env_vars_branch_strips_refs_heads_prefix() {
        let vars = build_env_vars_core(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "refs/heads/feature/login",
            None,
            "test",
            None,
        );
        assert_eq!(
            find_env(&vars, "COMMIT_BRANCH"),
            Some("feature/login".into())
        );
        assert_eq!(
            find_env(&vars, "COMMIT_REF"),
            Some("refs/heads/feature/login".into())
        );
    }

    #[test]
    fn env_vars_bare_ref_used_as_branch() {
        let vars =
            build_env_vars_core(Uuid::nil(), Uuid::nil(), "proj", "main", None, "test", None);
        assert_eq!(find_env(&vars, "COMMIT_BRANCH"), Some("main".into()));
    }

    // -- is_kaniko_image --

    #[test]
    fn detect_kaniko_image_standard() {
        assert!(is_kaniko_image("gcr.io/kaniko-project/executor:latest"));
    }

    #[test]
    fn detect_kaniko_image_case_insensitive() {
        assert!(is_kaniko_image("gcr.io/Kaniko-Project/executor:v1"));
    }

    #[test]
    fn detect_kaniko_image_substring() {
        assert!(is_kaniko_image("my-registry/kaniko-custom:v1"));
    }

    #[test]
    fn detect_kaniko_image_false_for_alpine() {
        assert!(!is_kaniko_image("alpine:3.19"));
    }

    #[test]
    fn detect_kaniko_image_false_for_rust() {
        assert!(!is_kaniko_image("rust:1.85-slim"));
    }

    // -- classify_branch --

    #[test]
    fn branch_main_classified_as_production() {
        assert_eq!(classify_branch("main"), "production");
    }

    #[test]
    fn branch_master_classified_as_production() {
        assert_eq!(classify_branch("master"), "production");
    }

    #[test]
    fn branch_feature_classified_as_preview() {
        assert_eq!(classify_branch("feature/login"), "preview");
    }

    #[test]
    fn branch_develop_classified_as_preview() {
        assert_eq!(classify_branch("develop"), "preview");
    }

    // -- build_image_ref --

    #[test]
    fn image_ref_format() {
        let r = build_image_ref("registry.example.com", "my-app", "abc123");
        assert_eq!(r, "registry.example.com/my-app:abc123");
    }

    #[test]
    fn image_ref_latest_tag() {
        let r = build_image_ref("localhost:5000", "proj", "latest");
        assert_eq!(r, "localhost:5000/proj:latest");
    }

    // -- registry secret mount --

    #[test]
    fn pod_spec_image_pull_secrets_set_when_registry_secret() {
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test",
            pipeline_id: Uuid::nil(),
            project_id: Uuid::nil(),
            step_name: "build",
            image: "gcr.io/kaniko-project/executor:latest",
            commands: &["true".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/test.git",
            git_ref: "main",
            registry_secret: Some("pl-registry-00000000"),
            git_auth_token: "test-token",
        });

        let spec = pod.spec.unwrap();
        let secrets = spec.image_pull_secrets.unwrap();
        assert_eq!(secrets.len(), 1);
        assert_eq!(secrets[0].name, "pl-registry-00000000");
    }

    #[test]
    fn pod_spec_image_pull_secrets_absent_without_registry_secret() {
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test",
            pipeline_id: Uuid::nil(),
            project_id: Uuid::nil(),
            step_name: "test",
            image: "alpine:3.19",
            commands: &["true".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/test.git",
            git_ref: "main",
            registry_secret: None,
            git_auth_token: "test-token",
        });

        let spec = pod.spec.unwrap();
        assert!(
            spec.image_pull_secrets.is_none(),
            "imagePullSecrets should be absent when no registry secret"
        );
    }

    #[test]
    fn pod_spec_without_registry_secret_has_one_volume() {
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test",
            pipeline_id: Uuid::nil(),
            project_id: Uuid::nil(),
            step_name: "test",
            image: "alpine:3.19",
            commands: &["true".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/test.git",
            git_ref: "main",
            registry_secret: None,
            git_auth_token: "test-token",
        });

        let spec = pod.spec.unwrap();
        assert_eq!(spec.volumes.as_ref().unwrap().len(), 1);
        let mounts = spec.containers[0].volume_mounts.as_ref().unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].name, "workspace");
    }

    #[test]
    fn pod_spec_with_registry_secret_adds_docker_config() {
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test",
            pipeline_id: Uuid::nil(),
            project_id: Uuid::nil(),
            step_name: "build",
            image: "gcr.io/kaniko-project/executor:latest",
            commands: &["true".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/test.git",
            git_ref: "main",
            registry_secret: Some("pl-registry-00000000"),
            git_auth_token: "test-token",
        });

        let spec = pod.spec.unwrap();

        // Should have 2 volumes: workspace + docker-config
        let volumes = spec.volumes.as_ref().unwrap();
        assert_eq!(volumes.len(), 2);
        assert_eq!(volumes[1].name, "docker-config");
        let secret_vol = volumes[1].secret.as_ref().unwrap();
        assert_eq!(
            secret_vol.secret_name.as_deref(),
            Some("pl-registry-00000000")
        );

        // Step container should have 2 mounts: workspace + docker-config
        let mounts = spec.containers[0].volume_mounts.as_ref().unwrap();
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[1].name, "docker-config");
        assert_eq!(mounts[1].mount_path, "/kaniko/.docker");
        assert_eq!(mounts[1].read_only, Some(true));
    }

    #[test]
    fn env_vars_docker_config_set_when_registry_configured() {
        let vars = build_env_vars_core(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "main",
            None,
            "test",
            Some("registry.example.com"),
        );
        assert_eq!(
            find_env(&vars, "DOCKER_CONFIG"),
            Some("/kaniko/.docker".into())
        );
    }

    #[test]
    fn env_vars_docker_config_absent_when_no_registry() {
        let vars =
            build_env_vars_core(Uuid::nil(), Uuid::nil(), "proj", "main", None, "test", None);
        assert!(find_env(&vars, "DOCKER_CONFIG").is_none());
    }

    // -- build_volumes_and_mounts --

    #[test]
    fn volumes_without_secret_has_one() {
        let (volumes, mounts) = build_volumes_and_mounts(None);
        assert_eq!(volumes.len(), 1);
        assert_eq!(volumes[0].name, "workspace");
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].name, "workspace");
        assert_eq!(mounts[0].mount_path, "/workspace");
    }

    #[test]
    fn volumes_with_secret_has_two() {
        let (volumes, mounts) = build_volumes_and_mounts(Some("my-secret"));
        assert_eq!(volumes.len(), 2);
        assert_eq!(volumes[1].name, "docker-config");
        let secret_vol = volumes[1].secret.as_ref().unwrap();
        assert_eq!(secret_vol.secret_name.as_deref(), Some("my-secret"));
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[1].name, "docker-config");
        assert_eq!(mounts[1].mount_path, "/kaniko/.docker");
        assert_eq!(mounts[1].read_only, Some(true));
    }

    // -- env_var helper --

    #[test]
    fn env_var_sets_name_and_value() {
        let e = env_var("FOO", "bar");
        assert_eq!(e.name, "FOO");
        assert_eq!(e.value, Some("bar".into()));
    }

    #[test]
    fn env_var_empty_value() {
        let e = env_var("EMPTY", "");
        assert_eq!(e.name, "EMPTY");
        assert_eq!(e.value, Some(String::new()));
    }

    // -- extract_exit_code additional cases --

    #[test]
    fn exit_code_zero_success() {
        let status = PodStatus {
            container_statuses: Some(vec![ContainerStatus {
                name: "step".into(),
                ready: false,
                restart_count: 0,
                image: String::new(),
                image_id: String::new(),
                state: Some(ContainerState {
                    terminated: Some(ContainerStateTerminated {
                        exit_code: 0,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            ..Default::default()
        };
        assert_eq!(extract_exit_code(&status), Some(0));
    }

    #[test]
    fn exit_code_137_oom_killed() {
        let status = PodStatus {
            container_statuses: Some(vec![ContainerStatus {
                name: "step".into(),
                ready: false,
                restart_count: 0,
                image: String::new(),
                image_id: String::new(),
                state: Some(ContainerState {
                    terminated: Some(ContainerStateTerminated {
                        exit_code: 137,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            ..Default::default()
        };
        assert_eq!(extract_exit_code(&status), Some(137));
    }

    #[test]
    fn exit_code_only_first_container() {
        // When multiple containers exist, only the first is checked
        let status = PodStatus {
            container_statuses: Some(vec![
                ContainerStatus {
                    name: "step".into(),
                    ready: false,
                    restart_count: 0,
                    image: String::new(),
                    image_id: String::new(),
                    state: Some(ContainerState {
                        terminated: Some(ContainerStateTerminated {
                            exit_code: 1,
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                ContainerStatus {
                    name: "sidecar".into(),
                    ready: false,
                    restart_count: 0,
                    image: String::new(),
                    image_id: String::new(),
                    state: Some(ContainerState {
                        terminated: Some(ContainerStateTerminated {
                            exit_code: 0,
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };
        assert_eq!(extract_exit_code(&status), Some(1));
    }

    #[test]
    fn exit_code_waiting_state_returns_none() {
        let status = PodStatus {
            container_statuses: Some(vec![ContainerStatus {
                name: "step".into(),
                ready: false,
                restart_count: 0,
                image: String::new(),
                image_id: String::new(),
                state: Some(ContainerState {
                    waiting: Some(ContainerStateWaiting::default()),
                    terminated: None,
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            ..Default::default()
        };
        assert_eq!(extract_exit_code(&status), None);
    }

    // -- pod spec additional edge cases --

    #[test]
    fn build_pod_spec_multiple_commands_joined() {
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test",
            pipeline_id: Uuid::nil(),
            project_id: Uuid::nil(),
            step_name: "test",
            image: "alpine:3.19",
            commands: &["echo a".into(), "echo b".into(), "echo c".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/test.git",
            git_ref: "main",
            registry_secret: None,
            git_auth_token: "test-token",
        });

        let container = &pod.spec.unwrap().containers[0];
        let script = &container.args.as_ref().unwrap()[0];
        assert_eq!(script, "echo a && echo b && echo c");
    }

    #[test]
    fn build_pod_spec_single_command() {
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test",
            pipeline_id: Uuid::nil(),
            project_id: Uuid::nil(),
            step_name: "test",
            image: "alpine:3.19",
            commands: &["cargo test".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/test.git",
            git_ref: "main",
            registry_secret: None,
            git_auth_token: "test-token",
        });

        let container = &pod.spec.unwrap().containers[0];
        let script = &container.args.as_ref().unwrap()[0];
        assert_eq!(script, "cargo test");
    }

    #[test]
    fn build_pod_spec_init_container_uses_http_clone() {
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test",
            pipeline_id: Uuid::nil(),
            project_id: Uuid::nil(),
            step_name: "test",
            image: "alpine:3.19",
            commands: &["true".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/repo.git",
            git_ref: "main",
            registry_secret: None,
            git_auth_token: "test-token",
        });

        let init = &pod.spec.unwrap().init_containers.unwrap()[0];
        // Init container only needs workspace mount (no hostPath repos mount)
        let mounts = init.volume_mounts.as_ref().unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].name, "workspace");

        // Clone command uses HTTP URL with GIT_ASKPASS
        let clone_cmd = &init.args.as_ref().unwrap()[0];
        assert!(
            clone_cmd.contains("http://platform:8080/owner/repo.git"),
            "should use HTTP clone URL, got: {clone_cmd}"
        );
        assert!(
            clone_cmd.contains("GIT_ASKPASS"),
            "should use GIT_ASKPASS for auth, got: {clone_cmd}"
        );
        assert!(
            !clone_cmd.contains("file://"),
            "should not use file:// protocol, got: {clone_cmd}"
        );

        // GIT_AUTH_TOKEN env var should be set
        let env = init.env.as_ref().unwrap();
        let token_env = env.iter().find(|e| e.name == "GIT_AUTH_TOKEN");
        assert!(token_env.is_some(), "should have GIT_AUTH_TOKEN env var");
        assert_eq!(
            token_env.unwrap().value.as_deref(),
            Some("test-token"),
            "GIT_AUTH_TOKEN should match the provided token"
        );

        // GIT_BRANCH env var should be set
        let branch_env = env.iter().find(|e| e.name == "GIT_BRANCH");
        assert!(branch_env.is_some(), "should have GIT_BRANCH env var");
        assert_eq!(
            branch_env.unwrap().value.as_deref(),
            Some("main"),
            "GIT_BRANCH should match the git ref"
        );
    }

    #[test]
    fn build_pod_spec_with_env_vars() {
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test",
            pipeline_id: Uuid::nil(),
            project_id: Uuid::nil(),
            step_name: "test",
            image: "alpine:3.19",
            commands: &["echo $FOO".into()],
            env_vars: &[env_var("FOO", "bar"), env_var("BAZ", "qux")],
            repo_clone_url: "http://platform:8080/owner/test.git",
            git_ref: "main",
            registry_secret: None,
            git_auth_token: "test-token",
        });

        let container = &pod.spec.unwrap().containers[0];
        let env = container.env.as_ref().unwrap();
        assert_eq!(env.len(), 2);
        assert_eq!(env[0].name, "FOO");
        assert_eq!(env[0].value, Some("bar".into()));
    }

    // -- env_vars_core more edge cases --

    #[test]
    fn env_vars_refs_tags_stripped_for_branch() {
        let vars = build_env_vars_core(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "refs/tags/v1.0.0",
            None,
            "test",
            None,
        );
        // refs/tags/ is NOT stripped by the branch logic — only refs/heads/ is
        assert_eq!(
            find_env(&vars, "COMMIT_BRANCH"),
            Some("refs/tags/v1.0.0".into())
        );
        assert_eq!(
            find_env(&vars, "COMMIT_REF"),
            Some("refs/tags/v1.0.0".into())
        );
    }

    #[test]
    fn env_vars_project_name_preserved_exactly() {
        let vars = build_env_vars_core(
            Uuid::nil(),
            Uuid::nil(),
            "My-App-v2",
            "main",
            None,
            "build",
            None,
        );
        assert_eq!(find_env(&vars, "PROJECT"), Some("My-App-v2".into()));
        assert_eq!(
            find_env(&vars, "PLATFORM_PROJECT_NAME"),
            Some("My-App-v2".into())
        );
    }

    #[test]
    fn env_vars_step_name_preserved() {
        let vars = build_env_vars_core(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "main",
            None,
            "deploy-production",
            None,
        );
        assert_eq!(
            find_env(&vars, "STEP_NAME"),
            Some("deploy-production".into())
        );
    }

    // -- HTTP clone security --

    #[test]
    fn init_container_no_token_in_clone_url() {
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test",
            pipeline_id: Uuid::nil(),
            project_id: Uuid::nil(),
            step_name: "test",
            image: "alpine:3.19",
            commands: &["true".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/test.git",
            git_ref: "main",
            registry_secret: None,
            git_auth_token: "secret-token-value",
        });

        let init = &pod.spec.unwrap().init_containers.unwrap()[0];
        let clone_cmd = &init.args.as_ref().unwrap()[0];
        assert!(
            !clone_cmd.contains("secret-token-value"),
            "token must not appear in clone command args"
        );
    }

    #[test]
    fn branch_passed_as_env_var_not_in_shell_args() {
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test",
            pipeline_id: Uuid::nil(),
            project_id: Uuid::nil(),
            step_name: "test",
            image: "alpine:3.19",
            commands: &["true".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/test.git",
            git_ref: "feat/$(malicious-cmd)",
            registry_secret: None,
            git_auth_token: "test-token",
        });

        let init = &pod.spec.unwrap().init_containers.unwrap()[0];
        let clone_cmd = &init.args.as_ref().unwrap()[0];
        // Branch name must NOT appear in the shell command (prevents injection)
        assert!(
            !clone_cmd.contains("$(malicious-cmd)"),
            "branch must not be interpolated into shell args, got: {clone_cmd}"
        );
        // Branch should be referenced via $GIT_BRANCH env var
        assert!(
            clone_cmd.contains("$GIT_BRANCH"),
            "should reference $GIT_BRANCH env var, got: {clone_cmd}"
        );
        // GIT_BRANCH env var should be set with the actual branch value
        let env = init.env.as_ref().unwrap();
        let branch_env = env.iter().find(|e| e.name == "GIT_BRANCH").unwrap();
        assert_eq!(branch_env.value.as_deref(), Some("feat/$(malicious-cmd)"));
    }

    // -- SecurityContext --

    #[test]
    fn pipeline_pod_security_context_runs_as_non_root() {
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test",
            pipeline_id: Uuid::nil(),
            project_id: Uuid::nil(),
            step_name: "test",
            image: "alpine:3.19",
            commands: &["true".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/test.git",
            git_ref: "main",
            registry_secret: None,
            git_auth_token: "test-token",
        });

        let spec = pod.spec.unwrap();
        let psc = spec.security_context.unwrap();
        // No run_as_non_root/run_as_user — kaniko needs root to build images
        assert_eq!(psc.run_as_non_root, None);
        assert_eq!(psc.run_as_user, None);
        assert_eq!(psc.fs_group, Some(1000));
    }

    #[test]
    fn pipeline_step_container_has_no_security_context() {
        // Step containers (e.g. kaniko) need root + capabilities to build
        // images, so no restrictive security context is applied.
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test",
            pipeline_id: Uuid::nil(),
            project_id: Uuid::nil(),
            step_name: "test",
            image: "alpine:3.19",
            commands: &["true".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/test.git",
            git_ref: "main",
            registry_secret: None,
            git_auth_token: "test-token",
        });

        let spec = pod.spec.unwrap();
        let container = &spec.containers[0];
        assert!(
            container.security_context.is_none(),
            "step container should not have a restrictive security context"
        );
    }

    #[test]
    fn pipeline_clone_container_drops_all_capabilities() {
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test",
            pipeline_id: Uuid::nil(),
            project_id: Uuid::nil(),
            step_name: "test",
            image: "alpine:3.19",
            commands: &["true".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/test.git",
            git_ref: "main",
            registry_secret: None,
            git_auth_token: "test-token",
        });

        let spec = pod.spec.unwrap();
        let init = &spec.init_containers.unwrap()[0];
        let sc = init.security_context.as_ref().unwrap();
        assert_eq!(sc.allow_privilege_escalation, Some(false));
        let caps = sc.capabilities.as_ref().unwrap();
        assert_eq!(caps.drop.as_ref().unwrap(), &vec!["ALL".to_string()]);
    }
}
