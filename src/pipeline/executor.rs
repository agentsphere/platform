use std::collections::BTreeMap;
use std::time::Instant;

use k8s_openapi::api::core::v1::{
    Container, EmptyDirVolumeSource, EnvVar, Pod, PodSpec, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::Api;
use kube::api::{DeleteParams, ListParams, LogParams, PostParams};
use sqlx::PgPool;
use uuid::Uuid;

use crate::store::AppState;

use super::error::PipelineError;

// ---------------------------------------------------------------------------
// Background executor loop
// ---------------------------------------------------------------------------

/// Background task that polls for pending pipelines and executes them.
pub async fn run(state: AppState, mut shutdown: tokio::sync::watch::Receiver<()>) {
    tracing::info!("pipeline executor started");

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                tracing::info!("pipeline executor shutting down");
                break;
            }
            () = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                if let Err(e) = poll_pending(&state).await {
                    tracing::error!(error = %e, "error polling pending pipelines");
                }
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
               p.name as "project_name!: String",
               p.repo_path as "repo_path!: String"
        FROM pipelines pl
        JOIN projects p ON p.id = pl.project_id
        WHERE pl.id = $1
        "#,
        pipeline_id,
    )
    .fetch_one(&state.pool)
    .await?;

    let meta = PipelineMeta {
        git_ref: pipeline.git_ref,
        commit_sha: pipeline.commit_sha,
        project_name: pipeline.project_name,
        repo_path: pipeline.repo_path,
    };

    let all_succeeded = run_all_steps(state, pipeline_id, project_id, &meta).await?;

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
    repo_path: String,
}

/// A pipeline step row loaded from the database.
struct StepRow {
    id: Uuid,
    step_order: i32,
    name: String,
    image: String,
    commands: Vec<String>,
}

/// Run all steps for a pipeline. Returns true if all steps succeeded.
async fn run_all_steps(
    state: &AppState,
    pipeline_id: Uuid,
    project_id: Uuid,
    pipeline: &PipelineMeta,
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

    let namespace = &state.config.pipeline_namespace;
    let pods: Api<Pod> = Api::namespaced(state.kube.clone(), namespace);

    for step in &steps {
        if is_cancelled(&state.pool, pipeline_id).await? {
            skip_remaining_steps(&state.pool, pipeline_id).await?;
            return Ok(false);
        }

        let succeeded =
            execute_single_step(state, &pods, pipeline_id, project_id, pipeline, step).await?;

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
        repo_path: &pipeline.repo_path,
        git_ref: &pipeline.git_ref,
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
    repo_path: &'a str,
    git_ref: &'a str,
}

fn build_pod_spec(p: &PodSpecParams<'_>) -> Pod {
    // Build the shell script from commands
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

    Pod {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(p.pod_name.into()),
            labels: Some(labels),
            ..Default::default()
        },
        spec: Some(PodSpec {
            restart_policy: Some("Never".into()),
            init_containers: Some(vec![Container {
                name: "clone".into(),
                image: Some("alpine/git:latest".into()),
                command: Some(vec!["sh".into(), "-c".into()]),
                args: Some(vec![format!(
                    "git clone --depth 1 --branch {branch} file://{} /workspace",
                    p.repo_path
                )]),
                volume_mounts: Some(vec![
                    VolumeMount {
                        name: "workspace".into(),
                        mount_path: "/workspace".into(),
                        ..Default::default()
                    },
                    VolumeMount {
                        name: "repos".into(),
                        mount_path: p.repo_path.into(),
                        read_only: Some(true),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            }]),
            containers: vec![Container {
                name: "step".into(),
                image: Some(p.image.into()),
                command: Some(vec!["sh".into(), "-c".into()]),
                args: Some(vec![script]),
                working_dir: Some("/workspace".into()),
                env: Some(p.env_vars.to_vec()),
                volume_mounts: Some(vec![VolumeMount {
                    name: "workspace".into(),
                    mount_path: "/workspace".into(),
                    ..Default::default()
                }]),
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
            volumes: Some(vec![
                Volume {
                    name: "workspace".into(),
                    empty_dir: Some(EmptyDirVolumeSource::default()),
                    ..Default::default()
                },
                Volume {
                    name: "repos".into(),
                    host_path: Some(k8s_openapi::api::core::v1::HostPathVolumeSource {
                        path: p.repo_path.into(),
                        type_: Some("Directory".into()),
                    }),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        }),
        ..Default::default()
    }
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

    if let Some(ref registry) = state.config.registry_url {
        vars.push(env_var("REGISTRY", registry));
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

/// If any step used a kaniko-like image, write/update the deployments table.
/// For main/master branches, creates a production deployment.
/// For other branches, creates a preview deployment.
async fn detect_and_write_deployment(state: &AppState, pipeline_id: Uuid, project_id: Uuid) {
    let image_steps = sqlx::query!(
        r#"
        SELECT name, image FROM pipeline_steps
        WHERE pipeline_id = $1 AND status = 'success' AND image ILIKE '%kaniko%'
        "#,
        pipeline_id,
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

    let registry = state
        .config
        .registry_url
        .as_deref()
        .unwrap_or("localhost:5000");
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
        // Upsert deployment for production environment
        let _ = sqlx::query!(
            r#"
            INSERT INTO deployments (project_id, environment, image_ref, desired_status, current_status)
            VALUES ($1, 'production', $2, 'active', 'pending')
            ON CONFLICT (project_id, environment)
            DO UPDATE SET image_ref = $2, desired_status = 'active', current_status = 'pending'
            "#,
            project_id,
            image_ref,
        )
        .execute(&state.pool)
        .await;

        tracing::info!(%project_id, %image_ref, "deployment updated from pipeline");
    } else {
        // Create/update preview deployment for non-main branches
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

    // Delete running pods by label selector
    let namespace = &state.config.pipeline_namespace;
    let pods: Api<Pod> = Api::namespaced(state.kube.clone(), namespace);
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
        ContainerState, ContainerStateTerminated, ContainerStatus, PodStatus,
    };

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
                    running: Some(Default::default()),
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
            repo_path: "/data/repos/owner/repo.git",
            git_ref: "refs/heads/main",
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
        assert_eq!(volumes.len(), 2);
        assert_eq!(volumes[0].name, "workspace");
        assert_eq!(volumes[1].name, "repos");
    }
}
