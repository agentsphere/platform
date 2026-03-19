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
               p.owner_id as "owner_id!",
               pl.triggered_by,
               pl.version,
               pl.trigger as "trigger!: String",
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

    // Use the project owner for git/registry auth tokens — `triggered_by` may be
    // an ephemeral agent user whose identity gets cleaned up before the pipeline
    // finishes (race condition: reaper deletes agent tokens while Kaniko is still pushing).
    let auth_user_id = pipeline.owner_id;

    // Create a short-lived git auth token for HTTP clone (scoped to this project)
    let git_token =
        create_git_auth_token(state, pipeline_id, project_id, Some(auth_user_id)).await?;

    // Create a short-lived OTLP token for pipeline pods to emit telemetry
    let otlp_token = create_pipeline_otlp_token(state, project_id, pipeline_id).await;

    let meta = PipelineMeta {
        git_ref: pipeline.git_ref,
        commit_sha: pipeline.commit_sha,
        version: pipeline.version,
        project_name: pipeline.project_name,
        repo_clone_url,
        git_auth_token: git_token.0,
        namespace: state
            .config
            .project_namespace(&pipeline.namespace_slug, "dev"),
        trigger_type: pipeline.trigger,
        namespace_slug: pipeline.namespace_slug,
        otlp_token,
    };

    // Ensure project namespace exists (lazy creation for DB-only projects)
    ensure_project_namespace(state, &meta.namespace, project_id).await?;

    // Create registry auth Secret if registry is configured
    let registry_creds = if state.config.registry_url.is_some() {
        match create_registry_secret(state, pipeline_id, auth_user_id, &meta.namespace).await {
            Ok(creds) => Some(creds),
            Err(e) => {
                tracing::warn!(error = %e, "failed to create registry secret, continuing without");
                None
            }
        }
    } else {
        None
    };

    let pipeline_svc = format!("pipeline/{}", meta.project_name);
    emit_pipeline_log(
        &state.pool,
        project_id,
        &pipeline_svc,
        "info",
        &format!("Pipeline started (trigger: {})", meta.trigger_type),
        Some(serde_json::json!({"pipeline_id": pipeline_id.to_string(), "trigger": meta.trigger_type})),
    )
    .await;

    let registry_secret_name = registry_creds.as_ref().map(|(name, _)| name.as_str());
    let all_succeeded =
        run_all_steps(state, pipeline_id, project_id, &meta, registry_secret_name).await?;

    // Clean up registry auth Secret + token
    if let Some((_, ref token_hash)) = registry_creds {
        cleanup_registry_secret(state, pipeline_id, token_hash, &meta.namespace).await;
    }

    // Clean up git auth token
    cleanup_git_auth_token(state, &git_token.1).await;

    finalize_pipeline(state, pipeline_id, project_id, all_succeeded, &pipeline_svc).await
}

/// Finalize a pipeline run: update status, trigger post-run actions, fire webhooks.
async fn finalize_pipeline(
    state: &AppState,
    pipeline_id: Uuid,
    project_id: Uuid,
    all_succeeded: bool,
    pipeline_svc: &str,
) -> Result<(), PipelineError> {
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
        crate::api::merge_requests::try_auto_merge(state, project_id).await;
    }

    fire_build_webhook(&state.pool, project_id, pipeline_id, final_status).await;

    let log_level = if all_succeeded { "info" } else { "error" };
    emit_pipeline_log(
        &state.pool,
        project_id,
        pipeline_svc,
        log_level,
        &format!("Pipeline {final_status}"),
        Some(serde_json::json!({"pipeline_id": pipeline_id.to_string(), "status": final_status})),
    )
    .await;

    tracing::info!(%pipeline_id, status = final_status, "pipeline finished");
    Ok(())
}

/// Parameters extracted from pipeline + project join query.
struct PipelineMeta {
    git_ref: String,
    commit_sha: Option<String>,
    version: Option<String>,
    project_name: String,
    repo_clone_url: String,
    /// Short-lived API token for authenticating git clone via `GIT_ASKPASS`.
    git_auth_token: String,
    /// K8s namespace for this pipeline's pods (e.g. `{slug}-dev`).
    namespace: String,
    /// How the pipeline was triggered: push, mr, tag, api.
    trigger_type: String,
    /// Namespace slug for the project (needed for deploy-test).
    namespace_slug: String,
    /// Short-lived OTLP token for pipeline pods to send telemetry.
    otlp_token: Option<String>,
}

/// A pipeline step row loaded from the database.
#[allow(dead_code)] // `gate` stored for API response; read via DB query in pipelines.rs
struct StepRow {
    id: Uuid,
    step_order: i32,
    name: String,
    image: String,
    commands: Vec<String>,
    condition_events: Vec<String>,
    condition_branches: Vec<String>,
    deploy_test: Option<serde_json::Value>,
    depends_on: Vec<String>,
    environment: Option<serde_json::Value>,
    gate: bool,
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
        SELECT id, step_order, name, image, commands,
               condition_events, condition_branches,
               deploy_test, depends_on, environment, gate
        FROM pipeline_steps
        WHERE pipeline_id = $1
        ORDER BY step_order ASC
        "#,
        pipeline_id,
    )
    .fetch_all(&state.pool)
    .await?;

    // Resolve pipeline secrets once for the entire pipeline
    let pipeline_secrets = resolve_pipeline_secrets(state, project_id).await;

    // Check if DAG mode: at least one step has non-empty depends_on
    let has_deps = steps.iter().any(|s| !s.depends_on.is_empty());

    if has_deps {
        run_steps_dag(
            state,
            pipeline_id,
            project_id,
            pipeline,
            registry_secret,
            &steps,
            &pipeline_secrets,
        )
        .await
    } else {
        run_steps_sequential(
            state,
            pipeline_id,
            project_id,
            pipeline,
            registry_secret,
            &steps,
            &pipeline_secrets,
        )
        .await
    }
}

/// Sequential execution (backward compat: no step has `depends_on`).
#[allow(clippy::too_many_arguments)]
async fn run_steps_sequential(
    state: &AppState,
    pipeline_id: Uuid,
    project_id: Uuid,
    pipeline: &PipelineMeta,
    registry_secret: Option<&str>,
    steps: &[StepRow],
    secrets: &[(String, String)],
) -> Result<bool, PipelineError> {
    let pods: Api<Pod> = Api::namespaced(state.kube.clone(), &pipeline.namespace);
    let branch = extract_branch(&pipeline.git_ref);

    for step in steps {
        if is_cancelled(&state.pool, pipeline_id).await? {
            skip_remaining_steps(&state.pool, pipeline_id).await?;
            return Ok(false);
        }

        let condition = step_condition_from_row(step);
        if !super::definition::step_matches(condition.as_ref(), &pipeline.trigger_type, branch) {
            tracing::info!(
                step = %step.name,
                trigger = %pipeline.trigger_type,
                %branch,
                "step skipped (condition not matched)"
            );
            sqlx::query!(
                "UPDATE pipeline_steps SET status = 'skipped' WHERE id = $1",
                step.id
            )
            .execute(&state.pool)
            .await?;
            continue;
        }

        let succeeded = execute_step_dispatch(
            state,
            &pods,
            pipeline_id,
            project_id,
            pipeline,
            step,
            registry_secret,
            secrets,
        )
        .await?;

        if !succeeded {
            skip_remaining_after(&state.pool, pipeline_id, step.step_order).await?;
            return Ok(false);
        }
    }

    Ok(true)
}

/// DAG-based parallel execution.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn run_steps_dag(
    state: &AppState,
    pipeline_id: Uuid,
    project_id: Uuid,
    pipeline: &PipelineMeta,
    registry_secret: Option<&str>,
    steps: &[StepRow],
    secrets: &[(String, String)],
) -> Result<bool, PipelineError> {
    use std::collections::{HashMap, HashSet};
    use tokio::task::JoinSet;

    let branch = extract_branch(&pipeline.git_ref);
    let max_parallel = state.config.pipeline_max_parallel;
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(max_parallel));

    // Build name → index map and adjacency
    let name_to_idx: HashMap<&str, usize> = steps
        .iter()
        .enumerate()
        .map(|(i, s)| (s.name.as_str(), i))
        .collect();

    let n = steps.len();
    let mut in_degree = vec![0usize; n];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];

    for (i, step) in steps.iter().enumerate() {
        for dep_name in &step.depends_on {
            if let Some(&dep_idx) = name_to_idx.get(dep_name.as_str()) {
                in_degree[i] += 1;
                dependents[dep_idx].push(i);
            }
        }
    }

    let mut ready: Vec<usize> = (0..n).filter(|&i| in_degree[i] == 0).collect();
    let mut join_set: JoinSet<(usize, Result<bool, PipelineError>)> = JoinSet::new();
    let mut completed: HashSet<usize> = HashSet::new();
    let mut skipped: HashSet<usize> = HashSet::new();
    let mut any_failure = false;

    loop {
        // Spawn ready steps
        while let Some(idx) = ready.pop() {
            if skipped.contains(&idx) {
                completed.insert(idx);
                continue;
            }

            if is_cancelled(&state.pool, pipeline_id).await? {
                skip_remaining_steps(&state.pool, pipeline_id).await?;
                return Ok(false);
            }

            let step = &steps[idx];

            // Check per-step condition
            let condition = step_condition_from_row(step);
            if !super::definition::step_matches(condition.as_ref(), &pipeline.trigger_type, branch)
            {
                tracing::info!(
                    step = %step.name,
                    trigger = %pipeline.trigger_type,
                    %branch,
                    "step skipped (condition not matched)"
                );
                sqlx::query!(
                    "UPDATE pipeline_steps SET status = 'skipped' WHERE id = $1",
                    step.id
                )
                .execute(&state.pool)
                .await?;
                completed.insert(idx);
                // Release dependents even for skipped steps (they still "completed")
                for &dep_idx in &dependents[idx] {
                    in_degree[dep_idx] -= 1;
                    if in_degree[dep_idx] == 0 {
                        ready.push(dep_idx);
                    }
                }
                continue;
            }

            // Spawn execution in JoinSet
            let state = state.clone();
            let sem = semaphore.clone();
            let namespace = pipeline.namespace.clone();
            let step_id = step.id;
            let step_name = step.name.clone();
            let step_image = step.image.clone();
            let step_commands = step.commands.clone();
            let step_deploy_test = step.deploy_test.clone();
            let step_env = step.environment.clone();
            let meta_clone = PipelineMeta {
                git_ref: pipeline.git_ref.clone(),
                commit_sha: pipeline.commit_sha.clone(),
                version: pipeline.version.clone(),
                project_name: pipeline.project_name.clone(),
                repo_clone_url: pipeline.repo_clone_url.clone(),
                git_auth_token: pipeline.git_auth_token.clone(),
                namespace: pipeline.namespace.clone(),
                trigger_type: pipeline.trigger_type.clone(),
                namespace_slug: pipeline.namespace_slug.clone(),
                otlp_token: pipeline.otlp_token.clone(),
            };
            let secrets = secrets.to_vec();
            let registry_secret = registry_secret.map(String::from);

            join_set.spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                let step_row = StepRow {
                    id: step_id,
                    step_order: 0, // not used in dispatch
                    name: step_name,
                    image: step_image,
                    commands: step_commands,
                    condition_events: vec![],
                    condition_branches: vec![],
                    deploy_test: step_deploy_test,
                    depends_on: vec![],
                    environment: step_env,
                    gate: false,
                };
                let pods: Api<Pod> = Api::namespaced(state.kube.clone(), &namespace);
                let result = execute_step_dispatch(
                    &state,
                    &pods,
                    pipeline_id,
                    project_id,
                    &meta_clone,
                    &step_row,
                    registry_secret.as_deref(),
                    &secrets,
                )
                .await;
                (idx, result)
            });
        }

        // Wait for next completion
        let Some(result) = join_set.join_next().await else {
            break; // No more tasks
        };

        let (idx, step_result) = match result {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "step task panicked");
                any_failure = true;
                continue;
            }
        };

        completed.insert(idx);

        let succeeded = match step_result {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, step = %steps[idx].name, "step execution error");
                false
            }
        };

        if succeeded {
            // Release dependents
            for &dep_idx in &dependents[idx] {
                in_degree[dep_idx] -= 1;
                if in_degree[dep_idx] == 0 && !skipped.contains(&dep_idx) {
                    ready.push(dep_idx);
                }
            }
        } else {
            any_failure = true;
            // Mark all transitive dependents as skipped
            mark_transitive_dependents_skipped(idx, &dependents, &mut skipped, &completed);
            // Skip those steps in DB
            for &s_idx in &skipped {
                if !completed.contains(&s_idx) {
                    let _ = sqlx::query!(
                        "UPDATE pipeline_steps SET status = 'skipped' WHERE id = $1 AND status = 'pending'",
                        steps[s_idx].id
                    )
                    .execute(&state.pool)
                    .await;
                }
            }
        }
    }

    Ok(!any_failure)
}

/// Mark all transitive dependents of a failed step as skipped.
fn mark_transitive_dependents_skipped(
    failed_idx: usize,
    dependents: &[Vec<usize>],
    skipped: &mut std::collections::HashSet<usize>,
    completed: &std::collections::HashSet<usize>,
) {
    let mut stack = vec![failed_idx];
    while let Some(idx) = stack.pop() {
        for &dep_idx in &dependents[idx] {
            if !completed.contains(&dep_idx) && skipped.insert(dep_idx) {
                stack.push(dep_idx);
            }
        }
    }
}

/// Extract branch name from git ref.
fn extract_branch(git_ref: &str) -> &str {
    git_ref
        .strip_prefix("refs/heads/")
        .or_else(|| git_ref.strip_prefix("refs/tags/"))
        .unwrap_or(git_ref)
}

/// Dispatch a step to the right executor (deploy-test or regular).
#[allow(clippy::too_many_arguments)]
async fn execute_step_dispatch(
    state: &AppState,
    pods: &Api<Pod>,
    pipeline_id: Uuid,
    project_id: Uuid,
    pipeline: &PipelineMeta,
    step: &StepRow,
    registry_secret: Option<&str>,
    secrets: &[(String, String)],
) -> Result<bool, PipelineError> {
    if step.deploy_test.is_some() {
        execute_deploy_test_step(
            state,
            pipeline_id,
            project_id,
            pipeline,
            step,
            registry_secret,
            secrets,
        )
        .await
    } else {
        execute_single_step(
            state,
            pods,
            pipeline_id,
            project_id,
            pipeline,
            step,
            registry_secret,
            secrets,
        )
        .await
    }
}

/// Build a `StepCondition` from a `StepRow`'s stored arrays.
/// Returns `None` if both arrays are empty (= always run).
fn step_condition_from_row(step: &StepRow) -> Option<super::definition::StepCondition> {
    if step.condition_events.is_empty() && step.condition_branches.is_empty() {
        return None;
    }
    Some(super::definition::StepCondition {
        events: step.condition_events.clone(),
        branches: step.condition_branches.clone(),
    })
}

/// Execute one pipeline step as a K8s pod. Returns true on success.
#[allow(clippy::too_many_arguments)]
async fn execute_single_step(
    state: &AppState,
    pods: &Api<Pod>,
    pipeline_id: Uuid,
    project_id: Uuid,
    pipeline: &PipelineMeta,
    step: &StepRow,
    registry_secret: Option<&str>,
    secrets: &[(String, String)],
) -> Result<bool, PipelineError> {
    let env_vars = build_env_vars_full(
        state,
        pipeline_id,
        project_id,
        pipeline,
        &step.name,
        secrets,
        step.environment.as_ref(),
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

    let step_svc = format!("pipeline/{}/{}", pipeline.project_name, step.name);

    sqlx::query!(
        "UPDATE pipeline_steps SET status = 'running' WHERE id = $1",
        step.id
    )
    .execute(&state.pool)
    .await?;

    emit_pipeline_log(
        &state.pool,
        project_id,
        &step_svc,
        "info",
        &format!("Step '{}' started (image: {})", step.name, step.image),
        Some(serde_json::json!({"pipeline_id": pipeline_id.to_string(), "step": step.name})),
    )
    .await;

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

            let log_level = if exit_code == 0 { "info" } else { "error" };
            emit_pipeline_log(
                &state.pool,
                project_id,
                &step_svc,
                log_level,
                &format!("Step '{}' {status} ({duration_ms}ms)", step.name),
                Some(serde_json::json!({
                    "pipeline_id": pipeline_id.to_string(),
                    "step": step.name,
                    "exit_code": exit_code,
                    "duration_ms": duration_ms,
                })),
            )
            .await;

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

            emit_pipeline_log(
                &state.pool,
                project_id,
                &step_svc,
                "error",
                &format!("Step '{}' failed: {e}", step.name),
                Some(serde_json::json!({
                    "pipeline_id": pipeline_id.to_string(),
                    "step": step.name,
                    "duration_ms": duration_ms,
                })),
            )
            .await;

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
/// Default step timeout: 15 minutes.
const DEFAULT_STEP_TIMEOUT_SECS: u64 = 900;

async fn wait_for_pod(pods: &Api<Pod>, pod_name: &str) -> Result<i32, PipelineError> {
    wait_for_pod_with_timeout(pods, pod_name, DEFAULT_STEP_TIMEOUT_SECS).await
}

async fn wait_for_pod_with_timeout(
    pods: &Api<Pod>,
    pod_name: &str,
    timeout_secs: u64,
) -> Result<i32, PipelineError> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(PipelineError::Other(anyhow::anyhow!(
                "pod {pod_name} timed out after {timeout_secs}s"
            )));
        }

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
            "Pending" | "Running" => {
                // Detect unrecoverable container states while pod phase is still Pending/Running
                if let Some(reason) = detect_unrecoverable_container(status) {
                    tracing::warn!(pod = pod_name, %reason, "pod in unrecoverable state");
                    return Err(PipelineError::Other(anyhow::anyhow!(
                        "pod {pod_name} failed: {reason}"
                    )));
                }
            }
            other => {
                tracing::warn!(pod = pod_name, phase = other, "unexpected pod phase");
            }
        }
    }
}

/// Check container statuses for unrecoverable waiting/error states.
/// Returns a human-readable reason if the pod will never succeed.
fn detect_unrecoverable_container(
    status: &k8s_openapi::api::core::v1::PodStatus,
) -> Option<String> {
    let containers = status.container_statuses.as_ref()?;
    if let Some(reason) = check_container_statuses(containers, "") {
        return Some(reason);
    }
    let init_containers = status.init_container_statuses.as_ref()?;
    check_container_statuses(init_containers, "init container ")
}

/// Check a list of container statuses for unrecoverable states.
fn check_container_statuses(
    statuses: &[k8s_openapi::api::core::v1::ContainerStatus],
    prefix: &str,
) -> Option<String> {
    for cs in statuses {
        if let Some(state) = &cs.state
            && let Some(waiting) = &state.waiting
        {
            let reason = waiting.reason.as_deref().unwrap_or("");
            match reason {
                "ImagePullBackOff" | "ErrImagePull" | "InvalidImageName" => {
                    let msg = waiting.message.as_deref().unwrap_or("image pull failed");
                    return Some(format!("{prefix}{reason}: {msg}"));
                }
                "CreateContainerConfigError" => {
                    let msg = waiting
                        .message
                        .as_deref()
                        .unwrap_or("container config error");
                    return Some(format!("{prefix}{reason}: {msg}"));
                }
                _ => {}
            }
        }
        if cs.restart_count >= 3
            && cs
                .state
                .as_ref()
                .and_then(|s| s.waiting.as_ref())
                .and_then(|w| w.reason.as_deref())
                == Some("CrashLoopBackOff")
        {
            return Some(format!(
                "{prefix}CrashLoopBackOff after {} restarts",
                cs.restart_count
            ));
        }
    }
    None
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

/// Env var names that must not be overridden by project secrets in pipeline pods.
const RESERVED_PIPELINE_ENV_VARS: &[&str] = &[
    "PLATFORM_PROJECT_ID",
    "PLATFORM_PROJECT_NAME",
    "PIPELINE_ID",
    "STEP_NAME",
    "COMMIT_REF",
    "COMMIT_BRANCH",
    "COMMIT_SHA",
    "SHORT_SHA",
    "IMAGE_TAG",
    "PROJECT",
    "VERSION",
    "REGISTRY",
    "DOCKER_CONFIG",
    "PIPELINE_TRIGGER",
    "GIT_AUTH_TOKEN",
    "GIT_ASKPASS",
    "PLATFORM_SECRET_NAMES",
    "PATH",
    "OTEL_EXPORTER_OTLP_ENDPOINT",
    "OTEL_SERVICE_NAME",
    "OTEL_RESOURCE_ATTRIBUTES",
    "OTEL_EXPORTER_OTLP_HEADERS",
];

fn is_reserved_pipeline_env_var(name: &str) -> bool {
    RESERVED_PIPELINE_ENV_VARS.contains(&name)
}

/// Resolve project secrets scoped to pipeline/agent/all for injection into pipeline pods.
async fn resolve_pipeline_secrets(state: &AppState, project_id: Uuid) -> Vec<(String, String)> {
    let Some(master_key_hex) = state.config.master_key.as_deref() else {
        return Vec::new();
    };
    let Ok(master_key) = crate::secrets::engine::parse_master_key(master_key_hex) else {
        return Vec::new();
    };
    match crate::secrets::engine::query_scoped_secrets(
        &state.pool,
        &master_key,
        project_id,
        &["pipeline", "agent", "all"],
        None,
    )
    .await
    {
        Ok(secrets) => secrets,
        Err(e) => {
            tracing::warn!(error = %e, %project_id, "failed to resolve pipeline secrets");
            Vec::new()
        }
    }
}

/// Create a short-lived OTLP API token for a pipeline run.
/// Returns `None` on failure (non-fatal — pipeline runs without telemetry).
async fn create_pipeline_otlp_token(
    state: &AppState,
    project_id: Uuid,
    pipeline_id: Uuid,
) -> Option<String> {
    let owner_id: Uuid = match sqlx::query_scalar!(
        "SELECT owner_id FROM projects WHERE id = $1 AND is_active = true",
        project_id,
    )
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(id)) => id,
        _ => return None,
    };

    let (raw_token, token_hash) = crate::auth::token::generate_api_token();
    let name = format!("otlp-pipeline-{}", &pipeline_id.to_string()[..8]);
    let scopes = vec!["observe:write".to_string()];

    if let Err(e) = sqlx::query(
        "INSERT INTO api_tokens (user_id, name, token_hash, scopes, project_id, expires_at)
           VALUES ($1, $2, $3, $4::text[], $5, now() + interval '4 hours')",
    )
    .bind(owner_id)
    .bind(&name)
    .bind(&token_hash)
    .bind(&scopes)
    .bind(project_id)
    .execute(&state.pool)
    .await
    {
        tracing::warn!(error = %e, %pipeline_id, "failed to create pipeline OTLP token");
        return None;
    }

    Some(raw_token)
}

/// Write a pipeline execution event to the observe `log_entries` table.
/// Best-effort — failures are logged but do not affect pipeline execution.
async fn emit_pipeline_log(
    pool: &PgPool,
    project_id: Uuid,
    service: &str,
    level: &str,
    message: &str,
    attributes: Option<serde_json::Value>,
) {
    if let Err(e) = sqlx::query(
        "INSERT INTO log_entries (project_id, service, level, message, attributes)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(project_id)
    .bind(service)
    .bind(level)
    .bind(message)
    .bind(attributes)
    .execute(pool)
    .await
    {
        tracing::debug!(error = %e, "failed to emit pipeline observe log");
    }
}

/// Build env vars with secrets and step-level environment merged in.
#[allow(clippy::too_many_arguments)]
fn build_env_vars_full(
    state: &AppState,
    pipeline_id: Uuid,
    project_id: Uuid,
    meta: &PipelineMeta,
    step_name: &str,
    secrets: &[(String, String)],
    step_environment: Option<&serde_json::Value>,
) -> Vec<EnvVar> {
    // 1. Platform vars (lowest priority)
    let mut vars = build_env_vars_core(
        pipeline_id,
        project_id,
        &meta.project_name,
        &meta.git_ref,
        meta.commit_sha.as_deref(),
        step_name,
        state.config.registry_url.as_deref(),
        meta.version.as_deref(),
        &meta.trigger_type,
    );

    // 2. OTEL env vars (so pipeline steps can emit telemetry)
    vars.push(env_var(
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        &state.config.platform_api_url,
    ));
    vars.push(env_var(
        "OTEL_SERVICE_NAME",
        &format!("{}/{step_name}", meta.project_name),
    ));
    vars.push(env_var(
        "OTEL_RESOURCE_ATTRIBUTES",
        &format!("platform.project_id={project_id}"),
    ));
    if let Some(ref token) = meta.otlp_token {
        vars.push(env_var(
            "OTEL_EXPORTER_OTLP_HEADERS",
            &format!("Authorization=Bearer {token}"),
        ));
    }

    // 3. Project secrets (skip reserved names)
    let mut secret_names = Vec::new();
    for (key, val) in secrets {
        if !is_reserved_pipeline_env_var(key) {
            vars.push(env_var(key, val));
            secret_names.push(key.as_str());
        }
    }
    if !secret_names.is_empty() {
        vars.push(env_var("PLATFORM_SECRET_NAMES", &secret_names.join(",")));
    }

    // 4. Step-level environment (highest priority — can override secrets)
    if let Some(env_json) = step_environment
        && let Some(map) = env_json.as_object()
    {
        let existing_pairs: Vec<(String, String)> = vars
            .iter()
            .filter_map(|ev| Some((ev.name.clone(), ev.value.as_ref()?.clone())))
            .collect();
        for (key, val) in map {
            if let Some(v) = val.as_str() {
                let expanded = super::definition::expand_step_env(v, &existing_pairs);
                vars.push(env_var(key, &expanded));
            }
        }
    }

    vars
}

/// Core env var builder with no dependency on `AppState`.
#[allow(clippy::too_many_arguments)]
fn build_env_vars_core(
    pipeline_id: Uuid,
    project_id: Uuid,
    project_name: &str,
    git_ref: &str,
    commit_sha: Option<&str>,
    step_name: &str,
    registry_url: Option<&str>,
    version: Option<&str>,
    trigger_type: &str,
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
        env_var("PIPELINE_TRIGGER", trigger_type),
    ];

    if let Some(sha) = commit_sha {
        vars.push(env_var("COMMIT_SHA", sha));
        let short_sha = &sha[..sha.len().min(7)];
        vars.push(env_var("SHORT_SHA", short_sha));
        vars.push(env_var("IMAGE_TAG", &format!("sha-{short_sha}")));
    }

    vars.push(env_var("VERSION", version.unwrap_or("")));

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
// Deploy-test step execution
// ---------------------------------------------------------------------------

/// Drop guard that deletes a test namespace when it goes out of scope.
struct TestNamespaceGuard {
    kube: kube::Client,
    namespace: String,
}

impl Drop for TestNamespaceGuard {
    fn drop(&mut self) {
        let kube = self.kube.clone();
        let ns = self.namespace.clone();
        tokio::spawn(async move {
            let namespaces: Api<k8s_openapi::api::core::v1::Namespace> = Api::all(kube);
            if let Err(e) = namespaces.delete(&ns, &DeleteParams::default()).await {
                tracing::warn!(error = %e, %ns, "failed to delete test namespace");
            } else {
                tracing::info!(%ns, "test namespace deleted");
            }
        });
    }
}

/// Execute a deploy-test step: deploy app to temp namespace, run test pod, clean up.
#[allow(clippy::too_many_lines)]
async fn execute_deploy_test_step(
    state: &AppState,
    pipeline_id: Uuid,
    project_id: Uuid,
    pipeline: &PipelineMeta,
    step: &StepRow,
    _registry_secret: Option<&str>,
    secrets: &[(String, String)],
) -> Result<bool, PipelineError> {
    let dt: super::definition::DeployTestDef =
        serde_json::from_value(step.deploy_test.clone().unwrap_or_default()).map_err(|e| {
            PipelineError::InvalidDefinition(format!("invalid deploy_test config: {e}"))
        })?;

    // Build env vars for variable expansion
    let mut env_pairs: Vec<(String, String)> = build_env_vars_full(
        state,
        pipeline_id,
        project_id,
        pipeline,
        &step.name,
        secrets,
        step.environment.as_ref(),
    )
    .iter()
    .filter_map(|ev| Some((ev.name.clone(), ev.value.as_ref()?.clone())))
    .collect();

    // Override REGISTRY with the node-visible URL for test_image expansion.
    // The default REGISTRY points to the push URL (e.g. host.docker.internal:55251)
    // but containerd on Kind nodes needs the DaemonSet proxy URL (e.g. localhost:48773).
    if let Some(node_reg) = node_registry_url(&state.config)
        && let Some(pair) = env_pairs.iter_mut().find(|(k, _)| k == "REGISTRY")
    {
        pair.1 = node_reg.to_string();
    }

    // Expand env vars in test_image
    let test_image = super::definition::expand_step_env(&dt.test_image, &env_pairs);

    // Mark step as running
    sqlx::query!(
        "UPDATE pipeline_steps SET status = 'running' WHERE id = $1",
        step.id
    )
    .execute(&state.pool)
    .await?;

    let start = std::time::Instant::now();

    // 1. Create temp namespace
    let ns_name = format!(
        "{}-test-{}",
        pipeline.namespace_slug,
        &pipeline_id.to_string()[..8]
    );
    crate::deployer::namespace::ensure_namespace(
        &state.kube,
        &ns_name,
        "test",
        &project_id.to_string(),
    )
    .await
    .map_err(|e| PipelineError::Other(e.into()))?;

    // Guard ensures cleanup even on early return
    let _ns_guard = TestNamespaceGuard {
        kube: state.kube.clone(),
        namespace: ns_name.clone(),
    };

    // 2. Create registry pull secret in test namespace
    crate::deployer::reconciler::ensure_registry_pull_secret_for(
        state,
        project_id,
        pipeline_id,
        &ns_name,
    )
    .await;

    // 3. Read + render deploy manifests from project repo
    let manifests_path = dt.manifests.as_deref().unwrap_or("deploy/");
    let branch = extract_branch(&pipeline.git_ref);

    // Get repo path from DB
    let repo_path: Option<String> =
        sqlx::query_scalar("SELECT repo_path FROM projects WHERE id = $1 AND is_active = true")
            .bind(project_id)
            .fetch_optional(&state.pool)
            .await
            .ok()
            .flatten();

    let repo_path = repo_path
        .ok_or_else(|| PipelineError::Other(anyhow::anyhow!("project repo_path not found")))?;

    // If path ends with `/`, read all YAML files from the directory;
    // otherwise read a single file (backward compat).
    let manifest_content = if manifests_path.ends_with('/') {
        super::trigger::read_dir_at_ref(std::path::Path::new(&repo_path), branch, manifests_path)
            .await
            .ok_or_else(|| {
                PipelineError::InvalidDefinition(format!(
                    "deploy manifests directory '{manifests_path}' not found or empty at ref '{branch}'"
                ))
            })?
    } else {
        super::trigger::read_file_at_ref(std::path::Path::new(&repo_path), branch, manifests_path)
            .await
            .ok_or_else(|| {
                PipelineError::InvalidDefinition(format!(
                    "deploy manifest '{manifests_path}' not found at ref '{branch}'"
                ))
            })?
    };

    // Determine app image ref (use node registry URL for containerd pulls)
    let registry = node_registry_url(&state.config).unwrap_or("localhost:5000");
    let commit_sha = pipeline.commit_sha.as_deref().unwrap_or("latest");
    let app_image_ref = format!("{registry}/{}/app:{commit_sha}", pipeline.project_name);

    // Render manifests with test environment
    let vars = crate::deployer::renderer::RenderVars {
        image_ref: app_image_ref,
        project_name: pipeline.project_name.clone(),
        environment: "test".into(),
        values: serde_json::json!({}),
        platform_api_url: state.config.platform_api_url.clone(),
    };
    let rendered = crate::deployer::renderer::render(&manifest_content, &vars)
        .map_err(|e| PipelineError::Other(e.into()))?;

    // 4. Apply manifests to test namespace
    if let Err(e) =
        crate::deployer::applier::apply_with_tracking(&state.kube, &rendered, &ns_name, None).await
    {
        tracing::error!(error = %e, %ns_name, "failed to apply deploy manifests");
        let duration_ms = i32::try_from(start.elapsed().as_millis()).unwrap_or(i32::MAX);
        sqlx::query!(
            "UPDATE pipeline_steps SET status = 'failure', duration_ms = $2 WHERE id = $1",
            step.id,
            duration_ms,
        )
        .execute(&state.pool)
        .await?;
        return Ok(false);
    }

    // 5. Wait for deployment to become ready
    if let Err(e) = wait_for_deployment_ready(&state.kube, &ns_name, dt.readiness_timeout).await {
        tracing::error!(error = %e, %ns_name, "app deployment did not become ready");
        // Capture app logs for debugging
        capture_deployment_logs(state, &ns_name, pipeline_id, &step.name).await;
        let duration_ms = i32::try_from(start.elapsed().as_millis()).unwrap_or(i32::MAX);
        sqlx::query!(
            "UPDATE pipeline_steps SET status = 'failure', duration_ms = $2 WHERE id = $1",
            step.id,
            duration_ms,
        )
        .execute(&state.pool)
        .await?;
        return Ok(false);
    }

    // 6. Create + run test pod
    let test_pod_name = format!("test-{}", &pipeline_id.to_string()[..8]);
    let test_pods: Api<Pod> = Api::namespaced(state.kube.clone(), &ns_name);

    let mut test_env = vec![
        env_var("APP_HOST", &format!("{}-app", pipeline.project_name)),
        env_var("APP_PORT", "8080"),
    ];
    // Add standard pipeline env vars (including secrets + step env)
    test_env.extend(build_env_vars_full(
        state,
        pipeline_id,
        project_id,
        pipeline,
        &step.name,
        secrets,
        step.environment.as_ref(),
    ));

    let test_commands = if dt.commands.is_empty() {
        None
    } else {
        let script = dt.commands.join(" && ");
        Some(vec!["sh".into(), "-c".into(), script])
    };

    let test_pod = Pod {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(test_pod_name.clone()),
            labels: Some(std::collections::BTreeMap::from([
                ("platform.io/pipeline".into(), pipeline_id.to_string()),
                ("platform.io/step".into(), "deploy-test".into()),
            ])),
            ..Default::default()
        },
        spec: Some(PodSpec {
            restart_policy: Some("Never".into()),
            image_pull_secrets: Some(vec![LocalObjectReference {
                name: "platform-registry-pull".into(),
            }]),
            containers: vec![Container {
                name: "test".into(),
                image: Some(test_image),
                command: test_commands,
                env: Some(test_env),
                resources: Some(k8s_openapi::api::core::v1::ResourceRequirements {
                    limits: Some(std::collections::BTreeMap::from([
                        ("cpu".into(), Quantity("1".into())),
                        ("memory".into(), Quantity("1Gi".into())),
                    ])),
                    requests: Some(std::collections::BTreeMap::from([
                        ("cpu".into(), Quantity("250m".into())),
                        ("memory".into(), Quantity("256Mi".into())),
                    ])),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    };

    test_pods.create(&PostParams::default(), &test_pod).await?;

    // 7. Wait for test pod to complete
    let exit_code = wait_for_pod(&test_pods, &test_pod_name).await?;

    // 8. Capture test pod logs
    let test_log_params = LogParams {
        container: Some("test".into()),
        ..Default::default()
    };
    if let Ok(logs) = test_pods.logs(&test_pod_name, &test_log_params).await {
        let path = format!("logs/pipelines/{pipeline_id}/{}-test.log", step.name);
        if let Err(e) = state.minio.write(&path, logs.into_bytes()).await {
            tracing::error!(error = %e, %path, "failed to write test logs to MinIO");
        }
    }

    // 9. Capture app logs for debugging (especially if tests failed)
    if exit_code != 0 {
        capture_deployment_logs(state, &ns_name, pipeline_id, &step.name).await;
    }

    // Clean up test pod
    let _ = test_pods
        .delete(&test_pod_name, &DeleteParams::default())
        .await;

    let duration_ms = i32::try_from(start.elapsed().as_millis()).unwrap_or(i32::MAX);
    let status = if exit_code == 0 { "success" } else { "failure" };
    let log_ref = format!("logs/pipelines/{pipeline_id}/{}-test.log", step.name);
    sqlx::query!(
        r#"UPDATE pipeline_steps SET status = $2, exit_code = $3, duration_ms = $4, log_ref = $5 WHERE id = $1"#,
        step.id,
        status,
        exit_code,
        duration_ms,
        log_ref,
    )
    .execute(&state.pool)
    .await?;

    // Namespace cleanup happens automatically via _ns_guard drop
    Ok(exit_code == 0)
}

/// Wait for at least one deployment in the namespace to have `ready_replicas >= 1`.
async fn wait_for_deployment_ready(
    kube: &kube::Client,
    namespace: &str,
    timeout_secs: u32,
) -> Result<(), PipelineError> {
    use k8s_openapi::api::apps::v1::Deployment;

    let deployments: Api<Deployment> = Api::namespaced(kube.clone(), namespace);
    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs.into());

    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(PipelineError::Other(anyhow::anyhow!(
                "deployment in {namespace} did not become ready within {timeout_secs}s"
            )));
        }

        tokio::time::sleep(std::time::Duration::from_secs(3)).await;

        if let Ok(deploy_list) = deployments.list(&ListParams::default()).await {
            if deploy_list.items.is_empty() {
                continue;
            }
            let all_ready = deploy_list.items.iter().all(|d| {
                d.status
                    .as_ref()
                    .and_then(|s| s.ready_replicas)
                    .unwrap_or(0)
                    >= 1
            });
            if all_ready {
                return Ok(());
            }
        }
    }
}

/// Capture logs from all pods in a namespace for debugging deploy-test failures.
async fn capture_deployment_logs(
    state: &AppState,
    namespace: &str,
    pipeline_id: Uuid,
    step_name: &str,
) {
    let pods: Api<Pod> = Api::namespaced(state.kube.clone(), namespace);
    let Ok(pod_list) = pods.list(&ListParams::default()).await else {
        return;
    };

    for pod in &pod_list.items {
        let Some(pod_name) = &pod.metadata.name else {
            continue;
        };
        let log_params = LogParams::default();
        if let Ok(logs) = pods.logs(pod_name, &log_params).await {
            let path = format!("logs/pipelines/{pipeline_id}/{step_name}-app-{pod_name}.log");
            if let Err(e) = state.minio.write(&path, logs.into_bytes()).await {
                tracing::warn!(error = %e, %path, "failed to write app logs");
            }
        }
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
    let image_ref = format!("{registry}/{name}/app:{tag}");

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
    let dev_image_ref = format!("{registry}/{name}/dev:{tag}");

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
        format!("{registry}/{project_name}/app:{tag}")
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

    /// Test helper: wraps `build_env_vars_core` with default trigger_type "push".
    #[allow(clippy::too_many_arguments)]
    fn test_env_vars(
        pipeline_id: Uuid,
        project_id: Uuid,
        project_name: &str,
        git_ref: &str,
        commit_sha: Option<&str>,
        step_name: &str,
        registry_url: Option<&str>,
        version: Option<&str>,
    ) -> Vec<EnvVar> {
        build_env_vars_core(
            pipeline_id,
            project_id,
            project_name,
            git_ref,
            commit_sha,
            step_name,
            registry_url,
            version,
            "push",
        )
    }

    #[test]
    fn env_vars_include_all_seven_standard_vars() {
        let vars = test_env_vars(
            Uuid::nil(),
            Uuid::nil(),
            "my-project",
            "refs/heads/main",
            None,
            "build",
            None,
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
        let vars = test_env_vars(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "refs/heads/main",
            Some("abc123"),
            "test",
            None,
            None,
        );
        assert_eq!(find_env(&vars, "COMMIT_SHA"), Some("abc123".into()));
    }

    #[test]
    fn env_vars_commit_sha_absent_when_none() {
        let vars = test_env_vars(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "refs/heads/main",
            None,
            "test",
            None,
            None,
        );
        assert!(find_env(&vars, "COMMIT_SHA").is_none());
    }

    #[test]
    fn env_vars_registry_present_when_configured() {
        let vars = test_env_vars(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "main",
            None,
            "test",
            Some("registry.example.com"),
            None,
        );
        assert_eq!(
            find_env(&vars, "REGISTRY"),
            Some("registry.example.com".into())
        );
    }

    #[test]
    fn env_vars_registry_absent_when_none() {
        let vars = test_env_vars(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "main",
            None,
            "test",
            None,
            None,
        );
        assert!(find_env(&vars, "REGISTRY").is_none());
    }

    #[test]
    fn env_vars_branch_strips_refs_heads_prefix() {
        let vars = test_env_vars(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "refs/heads/feature/login",
            None,
            "test",
            None,
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
        let vars = test_env_vars(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "main",
            None,
            "test",
            None,
            None,
        );
        assert_eq!(find_env(&vars, "COMMIT_BRANCH"), Some("main".into()));
    }

    #[test]
    fn env_vars_include_pipeline_trigger() {
        let vars = build_env_vars_core(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "main",
            None,
            "test",
            None,
            None,
            "mr",
        );
        assert_eq!(find_env(&vars, "PIPELINE_TRIGGER"), Some("mr".into()));
    }

    #[test]
    fn env_vars_pipeline_trigger_push() {
        let vars = test_env_vars(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "main",
            None,
            "test",
            None,
            None,
        );
        assert_eq!(find_env(&vars, "PIPELINE_TRIGGER"), Some("push".into()));
    }

    // -- step_condition_from_row --

    #[test]
    fn step_condition_from_row_empty_is_none() {
        let row = StepRow {
            id: Uuid::nil(),
            step_order: 0,
            name: "test".into(),
            image: "alpine".into(),
            commands: vec![],
            condition_events: vec![],
            condition_branches: vec![],
            deploy_test: None,
            depends_on: vec![],
            environment: None,
            gate: false,
        };
        assert!(step_condition_from_row(&row).is_none());
    }

    #[test]
    fn step_condition_from_row_with_events() {
        let row = StepRow {
            id: Uuid::nil(),
            step_order: 0,
            name: "test".into(),
            image: "alpine".into(),
            commands: vec![],
            condition_events: vec!["mr".into()],
            condition_branches: vec![],
            deploy_test: None,
            depends_on: vec![],
            environment: None,
            gate: false,
        };
        let cond = step_condition_from_row(&row).unwrap();
        assert_eq!(cond.events, vec!["mr"]);
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
        assert_eq!(r, "registry.example.com/my-app/app:abc123");
    }

    #[test]
    fn image_ref_latest_tag() {
        let r = build_image_ref("localhost:5000", "proj", "latest");
        assert_eq!(r, "localhost:5000/proj/app:latest");
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
        let vars = test_env_vars(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "main",
            None,
            "test",
            Some("registry.example.com"),
            None,
        );
        assert_eq!(
            find_env(&vars, "DOCKER_CONFIG"),
            Some("/kaniko/.docker".into())
        );
    }

    #[test]
    fn env_vars_docker_config_absent_when_no_registry() {
        let vars = test_env_vars(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "main",
            None,
            "test",
            None,
            None,
        );
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
        let vars = test_env_vars(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "refs/tags/v1.0.0",
            None,
            "test",
            None,
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
        let vars = test_env_vars(
            Uuid::nil(),
            Uuid::nil(),
            "My-App-v2",
            "main",
            None,
            "build",
            None,
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
        let vars = test_env_vars(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "main",
            None,
            "deploy-production",
            None,
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

    // -- reserved pipeline env vars --

    #[test]
    fn reserved_pipeline_env_vars_blocks_known() {
        assert!(is_reserved_pipeline_env_var("PIPELINE_ID"));
        assert!(is_reserved_pipeline_env_var("COMMIT_SHA"));
        assert!(is_reserved_pipeline_env_var("PATH"));
        assert!(is_reserved_pipeline_env_var("PLATFORM_SECRET_NAMES"));
    }

    #[test]
    fn reserved_pipeline_env_vars_allows_custom() {
        assert!(!is_reserved_pipeline_env_var("DATABASE_URL"));
        assert!(!is_reserved_pipeline_env_var("MY_SECRET"));
        assert!(!is_reserved_pipeline_env_var("API_KEY"));
    }

    // -- extract_branch --

    #[test]
    fn extract_branch_from_refs_heads() {
        assert_eq!(extract_branch("refs/heads/main"), "main");
        assert_eq!(extract_branch("refs/heads/feature/login"), "feature/login");
    }

    #[test]
    fn extract_branch_from_refs_tags() {
        assert_eq!(extract_branch("refs/tags/v1.0"), "v1.0");
    }

    #[test]
    fn extract_branch_bare_ref() {
        assert_eq!(extract_branch("main"), "main");
    }
}
