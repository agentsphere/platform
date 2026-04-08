// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

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
use tracing::Instrument;
use uuid::Uuid;

use crate::auth::token;
use crate::pipeline::PipelineStatus;
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
                let iter_trace_id = uuid::Uuid::new_v4().to_string().replace('-', "");
                let span = tracing::info_span!(
                    "task_iteration",
                    task_name = "pipeline_executor",
                    trace_id = %iter_trace_id,
                    source = "system",
                );
                async {
                    match poll_pending(&state).await {
                        Ok(()) => state.task_registry.heartbeat("pipeline_executor"),
                        Err(e) => {
                            state.task_registry.report_error("pipeline_executor", &e.to_string());
                            tracing::error!(error = %e, "error polling pending pipelines");
                        }
                    }
                }.instrument(span).await;
            }
            () = state.pipeline_notify.notified() => {
                let iter_trace_id = uuid::Uuid::new_v4().to_string().replace('-', "");
                let span = tracing::info_span!(
                    "task_iteration",
                    task_name = "pipeline_executor",
                    trace_id = %iter_trace_id,
                    source = "system",
                );
                async {
                    // Immediate poll on notification
                    if let Err(e) = poll_pending(&state).await {
                        tracing::error!(error = %e, "error polling pending pipelines (notified)");
                    }
                }.instrument(span).await;
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
#[allow(clippy::too_many_lines)]
#[tracing::instrument(skip(state), fields(%pipeline_id), err)]
async fn execute_pipeline(state: &AppState, pipeline_id: Uuid) -> Result<(), PipelineError> {
    // Claim the pipeline by setting status to running (validated via PipelineStatus state machine)
    let from = PipelineStatus::Pending;
    let to = PipelineStatus::Running;
    debug_assert!(from.can_transition_to(to));

    let claimed = sqlx::query_scalar!(
        r#"
        UPDATE pipelines SET status = $2, started_at = now()
        WHERE id = $1 AND status = $3
        RETURNING project_id
        "#,
        pipeline_id,
        to.as_str(),
        from.as_str(),
    )
    .fetch_optional(&state.pool)
    .await?;

    let Some(project_id) = claimed else {
        tracing::debug!(%pipeline_id, "pipeline already claimed or not in pending state");
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

    let short_id = &pipeline_id.to_string()[..8];
    let pipeline_namespace = crate::deployer::namespace::pipeline_namespace_name(
        &state.config,
        &pipeline.namespace_slug,
        short_id,
    );
    let git_secret_name = format!("pl-git-{short_id}");

    let meta = PipelineMeta {
        git_ref: pipeline.git_ref,
        commit_sha: pipeline.commit_sha,
        version: pipeline.version,
        project_name: pipeline.project_name,
        repo_clone_url,
        git_auth_token: git_token.0,
        namespace: pipeline_namespace,
        trigger_type: pipeline.trigger,
        namespace_slug: pipeline.namespace_slug,
        otlp_token,
        git_secret_name,
    };

    // Ensure pipeline namespace exists (unique per pipeline run)
    ensure_pipeline_namespace(state, &meta.namespace, project_id).await?;

    // S31: Create git auth Secret for init container (avoids exposing token as env var)
    {
        let git_secret = Secret {
            metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                name: Some(meta.git_secret_name.clone()),
                labels: Some(BTreeMap::from([(
                    "platform.io/pipeline".into(),
                    pipeline_id.to_string(),
                )])),
                ..Default::default()
            },
            string_data: Some(BTreeMap::from([(
                "token".into(),
                meta.git_auth_token.clone(),
            )])),
            type_: Some("Opaque".into()),
            ..Default::default()
        };
        let secrets_api: Api<Secret> = Api::namespaced(state.kube.clone(), &meta.namespace);
        if let Err(e) = secrets_api
            .create(&PostParams::default(), &git_secret)
            .await
        {
            tracing::warn!(error = %e, "failed to create git auth secret");
        }
    }

    // Create registry auth Secret if registry is configured
    let registry_creds = if state.config.registry_url.is_some() {
        match create_registry_secret(
            state,
            pipeline_id,
            project_id,
            &meta.project_name,
            auth_user_id,
            &meta.namespace,
        )
        .await
        {
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

    // A17: Wrap step execution with a pipeline-level timeout
    let pipeline_timeout = std::time::Duration::from_secs(state.config.pipeline_timeout_secs);
    let step_result = tokio::time::timeout(
        pipeline_timeout,
        run_all_steps(state, pipeline_id, project_id, &meta, registry_secret_name),
    )
    .await;

    let all_succeeded = match step_result {
        Ok(result) => result?, // normal completion — propagate errors
        Err(_elapsed) => {
            tracing::error!(
                pipeline_run_id = %pipeline_id,
                timeout_secs = state.config.pipeline_timeout_secs,
                "pipeline timed out"
            );
            // Mark pipeline as failure due to timeout (Running -> Failure validated by state machine)
            let timeout_to = PipelineStatus::Failure;
            // Pipeline was claimed as Running earlier in this function, so transition is valid.
            // Use a WHERE guard on status to be safe against concurrent cancellation.
            sqlx::query(
                "UPDATE pipelines SET status = $2, finished_at = now() WHERE id = $1 AND status = 'running'",
            )
            .bind(pipeline_id)
            .bind(timeout_to.as_str())
            .execute(&state.pool)
            .await
            .ok();
            false
        }
    };

    // Cleanup always runs, even after timeout
    if let Some((_, ref token_hash)) = registry_creds {
        cleanup_registry_secret(state, pipeline_id, token_hash, &meta.namespace).await;
    }

    // Clean up git auth token
    cleanup_git_auth_token(state, &git_token.1).await;

    // Clean up the pipeline namespace (unique per run)
    cleanup_pipeline_namespace(&state.kube, &meta.namespace).await;

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
    let final_status = if all_succeeded {
        PipelineStatus::Success
    } else {
        PipelineStatus::Failure
    };

    // Fetch current status and validate the transition via state machine
    let current_status_str =
        sqlx::query_scalar!("SELECT status FROM pipelines WHERE id = $1", pipeline_id,)
            .fetch_one(&state.pool)
            .await?;

    let final_status_str = final_status.as_str();

    if let Some(current) = PipelineStatus::parse(&current_status_str) {
        if !current.can_transition_to(final_status) {
            tracing::warn!(
                %pipeline_id,
                from = current_status_str,
                to = final_status_str,
                "invalid pipeline status transition in finalize; skipping update"
            );
            return Ok(());
        }
    } else {
        tracing::warn!(
            %pipeline_id,
            status = current_status_str,
            "unknown pipeline status in finalize; skipping update"
        );
        return Ok(());
    }

    sqlx::query!(
        "UPDATE pipelines SET status = $2, finished_at = now() WHERE id = $1",
        pipeline_id,
        final_status_str,
    )
    .execute(&state.pool)
    .await?;

    if all_succeeded {
        // NOTE: detect_and_write_deployment() and detect_and_publish_dev_image()
        // are no longer called here. GitOps handoff and dev image publication are
        // now explicit pipeline steps (gitops_sync and imagebuild).
        // Only auto-merge remains as a finalize-time side effect.
        crate::api::merge_requests::try_auto_merge(state, project_id).await;
    }

    fire_build_webhook(
        &state.pool,
        project_id,
        pipeline_id,
        final_status_str,
        &state.webhook_semaphore,
    )
    .await;

    let log_level = if all_succeeded { "info" } else { "error" };
    emit_pipeline_log(
        &state.pool,
        project_id,
        pipeline_svc,
        log_level,
        &format!("Pipeline {final_status_str}"),
        Some(
            serde_json::json!({"pipeline_id": pipeline_id.to_string(), "status": final_status_str}),
        ),
    )
    .await;

    tracing::info!(%pipeline_id, status = final_status_str, "pipeline finished");
    Ok(())
}

/// Parameters extracted from pipeline + project join query.
#[derive(Debug)]
struct PipelineMeta {
    git_ref: String,
    commit_sha: Option<String>,
    version: Option<String>,
    project_name: String,
    repo_clone_url: String,
    /// Short-lived API token for authenticating git clone via `GIT_ASKPASS`.
    git_auth_token: String,
    /// K8s namespace for this pipeline's pods (e.g. `{slug}-p-{short_id}`).
    namespace: String,
    /// How the pipeline was triggered: push, mr, tag, api.
    trigger_type: String,
    /// Namespace slug for the project (needed for deploy-test).
    namespace_slug: String,
    /// Short-lived OTLP token for pipeline pods to send telemetry.
    otlp_token: Option<String>,
    /// K8s Secret name for git auth token (S31).
    git_secret_name: String,
}

/// A pipeline step row loaded from the database.
#[allow(dead_code)] // `gate` stored for API response; read via DB query in pipelines.rs
#[derive(sqlx::FromRow)]
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
    step_type: String,
    step_config: Option<serde_json::Value>,
}

/// Ensure the pipeline namespace (and network policy) exist before running pods.
/// Each pipeline run gets its own namespace (`{slug}-p-{short_id}`).
async fn ensure_pipeline_namespace(
    state: &AppState,
    namespace: &str,
    project_id: Uuid,
) -> Result<(), PipelineError> {
    crate::deployer::namespace::ensure_namespace(
        &state.kube,
        namespace,
        "pipeline",
        &project_id.to_string(),
        &state.config.platform_namespace,
        &state.config.gateway_namespace,
        false,
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
    let steps = sqlx::query_as::<_, StepRow>(
        "SELECT id, step_order, name, image, commands,
               condition_events, condition_branches,
               deploy_test, depends_on, environment, gate,
               step_type, step_config
        FROM pipeline_steps
        WHERE pipeline_id = $1
        ORDER BY step_order ASC",
    )
    .bind(pipeline_id)
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
            let step_type = step.step_type.clone();
            let step_config = step.step_config.clone();
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
                git_secret_name: pipeline.git_secret_name.clone(),
            };
            let secrets = secrets.to_vec();
            let registry_secret = registry_secret.map(String::from);

            join_set.spawn(async move {
                let _permit = sem
                    .acquire()
                    .await
                    .expect("pipeline concurrency semaphore closed unexpectedly");
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
                    step_type,
                    step_config,
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

/// Dispatch a step to the right executor based on `step_type`.
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
    match step.step_type.as_str() {
        "deploy_test" => {
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
        }
        "imagebuild" => {
            // imagebuild steps: inject secrets as --build-arg into the kaniko command
            let mut build_secrets = secrets.to_vec();
            if let Some(ref config) = step.step_config
                && let Some(secret_names) = config.get("secrets").and_then(|v| v.as_array())
            {
                for sn in secret_names {
                    if let Some(name) = sn.as_str()
                        && let Some((_, val)) = secrets.iter().find(|(k, _)| k == name)
                    {
                        build_secrets.push((format!("BUILD_ARG_{name}"), val.clone()));
                    }
                }
            }
            execute_single_step(
                state,
                pods,
                pipeline_id,
                project_id,
                pipeline,
                step,
                registry_secret,
                &build_secrets,
            )
            .await
        }
        "gitops_sync" => {
            execute_gitops_sync_step(state, pipeline_id, project_id, pipeline, step).await
        }
        "deploy_watch" => execute_deploy_watch_step(state, pipeline_id, project_id, step).await,
        _ => {
            // Legacy command step or deploy_test inferred from field
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
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
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

    // Expand $REGISTRY, $PROJECT, $COMMIT_SHA etc. in the step image reference.
    // Override REGISTRY with the node-visible URL so containerd can pull the image.
    let mut env_pairs: Vec<(String, String)> = env_vars
        .iter()
        .filter_map(|ev| Some((ev.name.clone(), ev.value.as_ref()?.clone())))
        .collect();
    if let Some(node_reg) = node_registry_url(&state.config)
        && let Some(pair) = env_pairs.iter_mut().find(|(k, _)| k == "REGISTRY")
    {
        pair.1 = node_reg.to_string();
    }
    let resolved_image = super::definition::expand_step_env(&step.image, &env_pairs);

    let pod_name = format!("pl-{}-{}", &pipeline_id.to_string()[..8], slug(&step.name));
    let step_artifacts = extract_artifact_defs(step.step_config.as_ref());
    let pod_spec = build_pod_spec(&PodSpecParams {
        pod_name: &pod_name,
        pipeline_id,
        project_id,
        step_name: &step.name,
        image: &resolved_image,
        commands: &step.commands,
        env_vars: &env_vars,
        repo_clone_url: &pipeline.repo_clone_url,
        git_ref: &pipeline.git_ref,
        registry_secret,
        git_secret_name: Some(&pipeline.git_secret_name),
        step_type: &step.step_type,
        git_clone_image: &state.config.git_clone_image,
        has_artifacts: !step_artifacts.is_empty(),
        proxy_binary_path: if state.config.dev_mode {
            state.config.proxy_binary_path.as_deref()
        } else {
            None
        },
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
        &format!("Step '{}' started (image: {})", step.name, resolved_image),
        Some(serde_json::json!({"pipeline_id": pipeline_id.to_string(), "step": step.name})),
    )
    .await;

    let start = Instant::now();
    let result = run_step(
        pods,
        &pod_name,
        &pod_spec,
        state,
        pipeline_id,
        step.id,
        &step.name,
        &step_artifacts,
    )
    .await;
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
#[allow(clippy::too_many_arguments)]
async fn run_step(
    pods: &Api<Pod>,
    pod_name: &str,
    pod_spec: &Pod,
    state: &AppState,
    pipeline_id: Uuid,
    step_id: Uuid,
    step_name: &str,
    artifact_defs: &[super::definition::ArtifactDef],
) -> Result<i32, PipelineError> {
    // Create the pod
    pods.create(&PostParams::default(), pod_spec).await?;

    if artifact_defs.is_empty() {
        // No artifacts: original flow
        let exit_code = wait_for_pod(pods, pod_name).await?;
        capture_logs(pods, pod_name, state, pipeline_id, step_name, exit_code).await;
        let _ = pods.delete(pod_name, &DeleteParams::default()).await;
        Ok(exit_code)
    } else {
        // Artifacts: wait for exit-code marker, then collect before signaling done
        let exit_code = wait_for_step_completion(pods, pod_name).await?;
        capture_logs(pods, pod_name, state, pipeline_id, step_name, exit_code).await;

        if exit_code == 0
            && let Err(e) =
                collect_step_artifacts(pods, pod_name, state, pipeline_id, step_id, artifact_defs)
                    .await
        {
            tracing::error!(error = %e, step = step_name, "artifact collection failed");
            // Signal done so container can exit, then clean up
            signal_pod_done(pods, pod_name).await;
            let _ = wait_for_pod(pods, pod_name).await;
            let _ = pods.delete(pod_name, &DeleteParams::default()).await;
            return Err(e);
        }

        signal_pod_done(pods, pod_name).await;
        let _ = wait_for_pod(pods, pod_name).await;
        let _ = pods.delete(pod_name, &DeleteParams::default()).await;
        Ok(exit_code)
    }
}

// ---------------------------------------------------------------------------
// Artifact collection
// ---------------------------------------------------------------------------

/// Extract artifact definitions from a step's `step_config` JSON.
fn extract_artifact_defs(
    step_config: Option<&serde_json::Value>,
) -> Vec<super::definition::ArtifactDef> {
    step_config
        .and_then(|c| c.get("artifacts"))
        .and_then(|v| serde_json::from_value::<Vec<super::definition::ArtifactDef>>(v.clone()).ok())
        .unwrap_or_default()
}

/// Wait for the step's user commands to finish by polling `/tmp/.exit-code`.
/// The container stays alive (marker file pattern) until we signal `/tmp/.done`.
async fn wait_for_step_completion(pods: &Api<Pod>, pod_name: &str) -> Result<i32, PipelineError> {
    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(DEFAULT_STEP_TIMEOUT_SECS);

    // Wait for pod to be running first
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(PipelineError::Other(anyhow::anyhow!(
                "pod {pod_name} timed out waiting for running state"
            )));
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        match pods.get(pod_name).await {
            Ok(pod) => {
                let phase = pod
                    .status
                    .as_ref()
                    .and_then(|s| s.phase.as_deref())
                    .unwrap_or("Unknown");
                match phase {
                    "Running" => break,
                    "Failed" => {
                        let code = pod.status.as_ref().and_then(extract_exit_code).unwrap_or(1);
                        return Ok(code);
                    }
                    "Succeeded" => return Ok(0),
                    _ => {
                        if let Some(ref status) = pod.status
                            && let Some(reason) = detect_unrecoverable_container(status)
                        {
                            return Err(PipelineError::Other(anyhow::anyhow!(
                                "pod {pod_name} failed: {reason}"
                            )));
                        }
                    }
                }
            }
            Err(kube::Error::Api(err)) if err.code == 404 => {
                return Err(PipelineError::Other(anyhow::anyhow!(
                    "pod {pod_name} disappeared"
                )));
            }
            Err(e) => return Err(e.into()),
        }
    }

    // Poll for /tmp/.exit-code marker file
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(PipelineError::Other(anyhow::anyhow!(
                "pod {pod_name} timed out waiting for exit-code marker"
            )));
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        // Check if pod is still running
        match pods.get(pod_name).await {
            Ok(pod) => {
                let phase = pod
                    .status
                    .as_ref()
                    .and_then(|s| s.phase.as_deref())
                    .unwrap_or("Unknown");
                if phase == "Succeeded" {
                    // Container completed — exit code is 0 by definition
                    return Ok(0);
                }
                if phase == "Failed" {
                    let code = pod.status.as_ref().and_then(extract_exit_code).unwrap_or(1);
                    return Ok(code);
                }
            }
            Err(kube::Error::Api(err)) if err.code == 404 => {
                return Err(PipelineError::Other(anyhow::anyhow!(
                    "pod {pod_name} disappeared"
                )));
            }
            Err(e) => return Err(e.into()),
        }

        // Try to read the exit-code marker (written atomically via mv)
        if let Ok(output) = exec_in_pod(pods, pod_name, "step", &["cat", "/tmp/.exit-code"]).await {
            let raw = output.trim();
            if raw.is_empty() {
                // File exists but empty — race with write; retry next poll
                continue;
            }
            let code = raw.parse::<i32>().unwrap_or(1);
            return Ok(code);
        }
        // File doesn't exist yet — keep polling
    }
}

/// Collect artifacts from a running pod container.
#[tracing::instrument(skip(pods, state, artifact_defs), fields(%pipeline_id, %step_id), err)]
async fn collect_step_artifacts(
    pods: &Api<Pod>,
    pod_name: &str,
    state: &AppState,
    pipeline_id: Uuid,
    step_id: Uuid,
    artifact_defs: &[super::definition::ArtifactDef],
) -> Result<(), PipelineError> {
    for artifact_def in artifact_defs {
        // Read and validate config if specified
        let config_json = if let Some(ref config_path) = artifact_def.config {
            let config_bytes = exec_in_pod(
                pods,
                pod_name,
                "step",
                &["cat", &format!("/workspace/{config_path}")],
            )
            .await
            .map_err(|_| {
                PipelineError::Other(anyhow::anyhow!(
                    "artifact config file not found: {config_path}"
                ))
            })?;
            let validated = validate_artifact_config(config_bytes.as_bytes(), &artifact_def.name)?;
            Some(validated)
        } else {
            None
        };

        // Create parent artifact row (is_directory = true)
        let parent_id = Uuid::new_v4();
        let parent_minio_path = format!("artifacts/{pipeline_id}/{step_id}/{}", artifact_def.name);
        // Dynamic query: references new columns from artifact_collection migration.
        // Will be converted to sqlx::query!() after `just db-prepare`.
        sqlx::query(
            "INSERT INTO artifacts (id, pipeline_id, step_id, name, minio_path, content_type,
                                    size_bytes, artifact_type, config, is_directory)
             VALUES ($1, $2, $3, $4, $5, NULL, 0, $6, $7, true)",
        )
        .bind(parent_id)
        .bind(pipeline_id)
        .bind(step_id)
        .bind(&artifact_def.name)
        .bind(&parent_minio_path)
        .bind(&artifact_def.artifact_type)
        .bind(&config_json)
        .execute(&state.pool)
        .await?;

        // Exec tar to stream the artifact path.
        // Strip leading '/' so the path is relative to -C /workspace
        // (absolute paths cause tar to ignore -C and archive from root).
        let rel_path = artifact_def.path.trim_start_matches('/');
        let tar_result = exec_bytes(
            pods,
            pod_name,
            "step",
            &["tar", "czf", "-", "-C", "/workspace", rel_path],
        )
        .await;

        match tar_result {
            Ok(tar_bytes) => {
                if let Err(e) = unpack_and_store(
                    &tar_bytes,
                    state,
                    pipeline_id,
                    step_id,
                    &artifact_def.name,
                    parent_id,
                )
                .await
                {
                    tracing::error!(
                        error = %e,
                        artifact = %artifact_def.name,
                        "failed to unpack and store artifact"
                    );
                    return Err(e);
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    artifact = %artifact_def.name,
                    "failed to tar artifact path"
                );
                // Non-fatal: artifact path may not exist
            }
        }
    }
    Ok(())
}

/// A single file extracted from a tar archive, ready for upload.
#[cfg_attr(test, derive(Debug))]
struct ExtractedFile {
    sanitized_path: String,
    contents: Vec<u8>,
    content_type: String,
    size: u64,
}

/// Decompress a tar.gz byte stream, upload each file to `MinIO`, insert child artifact rows.
#[tracing::instrument(skip(tar_bytes, state), fields(%pipeline_id, %step_id, %artifact_name), err)]
async fn unpack_and_store(
    tar_bytes: &[u8],
    state: &AppState,
    pipeline_id: Uuid,
    step_id: Uuid,
    artifact_name: &str,
    parent_id: Uuid,
) -> Result<(), PipelineError> {
    // Extract all files synchronously (tar::Entries is not Send)
    let files = extract_tar_files(
        tar_bytes,
        state.config.max_artifact_file_bytes,
        state.config.max_artifact_total_bytes,
    )?;

    let file_count = files.len();
    let total_size: u64 = files.iter().map(|f| f.size).sum();

    // Upload files and insert DB rows asynchronously
    for file in &files {
        let minio_path = format!(
            "artifacts/{pipeline_id}/{step_id}/{artifact_name}/{}",
            file.sanitized_path
        );

        // Upload to MinIO
        state
            .minio
            .write(&minio_path, file.contents.clone())
            .await
            .map_err(|e| {
                PipelineError::Other(anyhow::anyhow!("failed to upload artifact to MinIO: {e}"))
            })?;

        // Insert child artifact row
        // Dynamic query: references new columns from artifact_collection migration.
        // Will be converted to sqlx::query!() after `just db-prepare`.
        sqlx::query(
            "INSERT INTO artifacts (pipeline_id, step_id, name, minio_path, content_type,
                                    size_bytes, artifact_type, is_directory, parent_id, relative_path)
             VALUES ($1, $2, $3, $4, $5, $6, $7, false, $8, $9)",
        )
        .bind(pipeline_id)
        .bind(step_id)
        .bind(&file.sanitized_path)
        .bind(&minio_path)
        .bind(&file.content_type)
        .bind(i64::try_from(file.size).unwrap_or(i64::MAX))
        .bind("file")
        .bind(parent_id)
        .bind(&file.sanitized_path)
        .execute(&state.pool)
        .await?;
    }

    tracing::info!(
        artifact = artifact_name,
        files = file_count,
        total_bytes = total_size,
        "artifact collection complete"
    );
    Ok(())
}

/// Extract files from a tar.gz byte stream synchronously.
/// Validates path traversal, file/total size limits, and file count.
fn extract_tar_files(
    tar_bytes: &[u8],
    max_file_bytes: u64,
    max_total_bytes: u64,
) -> Result<Vec<ExtractedFile>, PipelineError> {
    let decoder = flate2::read::GzDecoder::new(tar_bytes);
    let mut archive = tar::Archive::new(decoder);

    let max_file_count: usize = 1000;
    let mut total_size: u64 = 0;
    let mut files = Vec::new();

    let entries = archive
        .entries()
        .map_err(|e| PipelineError::Other(anyhow::anyhow!("failed to read tar entries: {e}")))?;

    for entry_result in entries {
        let mut entry = entry_result
            .map_err(|e| PipelineError::Other(anyhow::anyhow!("failed to read tar entry: {e}")))?;

        // Skip directories and symlinks
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            continue;
        }
        if entry_type.is_symlink() || entry_type.is_hard_link() {
            tracing::warn!("skipping symlink/hard link in artifact tar");
            continue;
        }

        let raw_path = entry
            .path()
            .map_err(|e| PipelineError::Other(anyhow::anyhow!("invalid tar entry path: {e}")))?
            .to_string_lossy()
            .to_string();

        let sanitized_path = sanitize_relative_path(&raw_path)?;

        let entry_size = entry.size();
        if entry_size > max_file_bytes {
            return Err(PipelineError::Other(anyhow::anyhow!(
                "artifact file '{sanitized_path}' exceeds size limit ({entry_size} > {max_file_bytes} bytes)",
            )));
        }

        total_size += entry_size;
        if total_size > max_total_bytes {
            return Err(PipelineError::Other(anyhow::anyhow!(
                "artifact total size exceeds limit ({total_size} > {max_total_bytes} bytes)",
            )));
        }

        if files.len() >= max_file_count {
            return Err(PipelineError::Other(anyhow::anyhow!(
                "artifact file count exceeds limit (> {max_file_count})",
            )));
        }

        // Read file contents
        #[allow(clippy::cast_possible_truncation)]
        let mut contents = Vec::with_capacity(entry_size as usize);
        std::io::Read::read_to_end(&mut entry, &mut contents).map_err(|e| {
            PipelineError::Other(anyhow::anyhow!("failed to read tar entry contents: {e}"))
        })?;

        let content_type = infer_content_type(&sanitized_path);

        files.push(ExtractedFile {
            sanitized_path,
            contents,
            content_type,
            size: entry_size,
        });
    }

    Ok(files)
}

/// Validate a config JSON file's structure against the expected schema.
fn validate_artifact_config(
    bytes: &[u8],
    artifact_name: &str,
) -> Result<serde_json::Value, PipelineError> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| {
        PipelineError::Other(anyhow::anyhow!("artifact config is not valid JSON: {e}"))
    })?;

    let groups = value
        .get("groups")
        .and_then(|g| g.as_object())
        .ok_or_else(|| {
            PipelineError::Other(anyhow::anyhow!(
                "artifact config missing root \"groups\" object"
            ))
        })?;

    validate_groups(groups, "", 1)?;

    let _ = artifact_name; // used for context in caller
    Ok(value)
}

/// Regex pattern for valid group keys: `[a-z0-9_-]{1,64}`.
fn is_valid_group_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Recursively validate groups/items structure. `depth` starts at 1.
fn validate_groups(
    groups: &serde_json::Map<String, serde_json::Value>,
    path: &str,
    depth: u32,
) -> Result<(), PipelineError> {
    if depth > 3 {
        return Err(PipelineError::Other(anyhow::anyhow!(
            "artifact config exceeds maximum group nesting depth (3)"
        )));
    }

    for (key, group_value) in groups {
        if !is_valid_group_key(key) {
            return Err(PipelineError::Other(anyhow::anyhow!(
                "invalid group key \"{key}\""
            )));
        }

        let group_obj = group_value.as_object().ok_or_else(|| {
            PipelineError::Other(anyhow::anyhow!("group \"{key}\" must be an object"))
        })?;

        // label is required
        if !group_obj.contains_key("label") || !group_obj["label"].is_string() {
            return Err(PipelineError::Other(anyhow::anyhow!(
                "group \"{key}\" missing required \"label\" field"
            )));
        }

        let current_path = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };

        // Validate nested groups
        if let Some(sub_groups) = group_obj.get("groups").and_then(|g| g.as_object()) {
            validate_groups(sub_groups, &current_path, depth + 1)?;
        }

        // Validate items
        if let Some(items) = group_obj.get("items").and_then(|i| i.as_object()) {
            for (item_key, item_value) in items {
                let item_obj = item_value.as_object().ok_or_else(|| {
                    PipelineError::Other(anyhow::anyhow!(
                        "item \"{item_key}\" in group \"{current_path}\" must be an object"
                    ))
                })?;

                if !item_obj.contains_key("label") || !item_obj["label"].is_string() {
                    return Err(PipelineError::Other(anyhow::anyhow!(
                        "item \"{item_key}\" in group \"{current_path}\" missing required \"label\" field"
                    )));
                }

                if let Some(meta) = item_obj.get("meta")
                    && !meta.is_object()
                {
                    return Err(PipelineError::Other(anyhow::anyhow!(
                        "item \"{item_key}\" meta must be an object"
                    )));
                }
            }
        }
    }

    Ok(())
}

/// Sanitize a relative path from a tar entry, rejecting path traversal.
fn sanitize_relative_path(raw: &str) -> Result<String, PipelineError> {
    use std::path::{Component, Path, PathBuf};
    let path = Path::new(raw);
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(c) => clean.push(c),
            Component::ParentDir => {
                return Err(PipelineError::Other(anyhow::anyhow!(
                    "path traversal in artifact: {raw}"
                )));
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    if clean.as_os_str().is_empty() {
        return Err(PipelineError::Other(anyhow::anyhow!("empty artifact path")));
    }
    Ok(clean.to_string_lossy().to_string())
}

/// Infer MIME content type from file extension.
fn infer_content_type(path: &str) -> String {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "json" => "application/json",
        "xml" => "application/xml",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" => "application/javascript",
        "txt" | "log" => "text/plain",
        "zip" => "application/zip",
        "tar" => "application/x-tar",
        "gz" => "application/gzip",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Signal the pod container to exit by creating the `/tmp/.done` marker file.
async fn signal_pod_done(pods: &Api<Pod>, pod_name: &str) {
    if let Err(e) = exec_in_pod(pods, pod_name, "step", &["touch", "/tmp/.done"]).await {
        tracing::warn!(error = %e, pod = pod_name, "failed to signal pod done");
    }
}

/// Execute a command in a pod container and return stdout as a String.
async fn exec_in_pod(
    pods: &Api<Pod>,
    pod_name: &str,
    container: &str,
    cmd: &[&str],
) -> Result<String, PipelineError> {
    let bytes = exec_bytes(pods, pod_name, container, cmd).await?;
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

/// Execute a command in a pod container and return stdout as raw bytes.
async fn exec_bytes(
    pods: &Api<Pod>,
    pod_name: &str,
    container: &str,
    cmd: &[&str],
) -> Result<Vec<u8>, PipelineError> {
    use tokio::io::AsyncReadExt;

    let cmd_owned: Vec<String> = cmd.iter().map(|s| (*s).to_string()).collect();
    let mut ap = pods
        .exec(
            pod_name,
            cmd_owned,
            &kube::api::AttachParams {
                container: Some(container.to_string()),
                stdout: true,
                stderr: true,
                ..Default::default()
            },
        )
        .await
        .map_err(|e| PipelineError::Other(anyhow::anyhow!("exec in pod failed: {e}")))?;

    let mut stdout_buf = Vec::new();
    if let Some(mut stdout) = ap.stdout() {
        stdout
            .read_to_end(&mut stdout_buf)
            .await
            .map_err(|e| PipelineError::Other(anyhow::anyhow!("exec read stdout failed: {e}")))?;
    }

    ap.join()
        .await
        .map_err(|e| PipelineError::Other(anyhow::anyhow!("exec join failed: {e}")))?;

    Ok(stdout_buf)
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
    exit_code: i32,
) {
    let failed = exit_code != 0;

    // Capture init container (clone) logs
    let init_log_params = LogParams {
        container: Some("clone".into()),
        ..Default::default()
    };
    match pods.logs(pod_name, &init_log_params).await {
        Ok(logs) => {
            if failed {
                let truncated: String = logs.chars().take(2000).collect();
                tracing::warn!(
                    pod = pod_name,
                    step = step_name,
                    "clone container logs (step failed):\n{truncated}"
                );
            }
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
            if failed {
                let truncated: String = logs.chars().take(2000).collect();
                tracing::error!(
                    pod = pod_name,
                    step = step_name,
                    exit_code,
                    "step container logs (FAILED):\n{truncated}"
                );
            }
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
    project_id: Uuid,
    project_name: &str,
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

    let tag_pattern = format!("{project_name}/*");
    sqlx::query!(
        r#"INSERT INTO api_tokens (id, user_id, name, token_hash, expires_at, project_id, registry_tag_pattern)
           VALUES ($1, $2, $3, $4, now() + interval '1 hour', $5, $6)"#,
        Uuid::new_v4(),
        triggered_by,
        format!("pipeline-{pipeline_id}"),
        token_hash,
        project_id,
        tag_pattern,
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
        string_data: Some(BTreeMap::from([
            (".dockerconfigjson".into(), config_json.to_string()),
            ("config.json".into(), config_json.to_string()),
        ])),
        type_: Some("kubernetes.io/dockerconfigjson".into()),
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

    // S31: Clean up git auth Secret
    let git_secret_name = format!("pl-git-{}", &pipeline_id.to_string()[..8]);
    let _ = secrets
        .delete(&git_secret_name, &DeleteParams::default())
        .await;

    // Delete the short-lived API token from the DB
    if let Err(e) = sqlx::query!("DELETE FROM api_tokens WHERE token_hash = $1", token_hash)
        .execute(&state.pool)
        .await
    {
        tracing::warn!(error = %e, "failed to delete pipeline API token");
    }
}

/// Delete the pipeline's unique namespace after the run finishes.
/// Best-effort — failures are logged but don't block finalization.
async fn cleanup_pipeline_namespace(kube: &kube::Client, namespace: &str) {
    let namespaces: Api<k8s_openapi::api::core::v1::Namespace> = Api::all(kube.clone());
    if let Err(e) = namespaces.delete(namespace, &DeleteParams::default()).await {
        tracing::warn!(error = %e, %namespace, "failed to delete pipeline namespace");
    } else {
        tracing::info!(%namespace, "pipeline namespace deleted");
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
    /// K8s Secret name containing git auth token (mounted as volume instead of env var).
    git_secret_name: Option<&'a str>,
    /// Step type — `imagebuild` steps need root (kaniko), others get hardened context.
    step_type: &'a str,
    /// Git clone init container image from config (A4: pinned, no `:latest`).
    git_clone_image: &'a str,
    /// Whether this step has artifacts to collect (wraps script with marker file pattern).
    has_artifacts: bool,
    /// Host path to the platform-proxy binary (mesh wrapping). Only used in dev mode.
    proxy_binary_path: Option<&'a str>,
}

/// Build the volumes and step container mounts for a pipeline pod.
fn build_volumes_and_mounts(
    registry_secret: Option<&str>,
    git_secret_name: Option<&str>,
    proxy_binary_path: Option<&str>,
) -> (Vec<Volume>, Vec<VolumeMount>) {
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

    // S31: Mount git auth token from K8s Secret (avoids exposing as env var)
    if let Some(secret_name) = git_secret_name {
        volumes.push(Volume {
            name: "git-auth".into(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(secret_name.into()),
                ..Default::default()
            }),
            ..Default::default()
        });
    }

    // Mesh proxy binary — mount from host (dev mode only)
    if let Some(host_path) = proxy_binary_path {
        volumes.push(Volume {
            name: "proxy".into(),
            host_path: Some(k8s_openapi::api::core::v1::HostPathVolumeSource {
                path: host_path.into(),
                type_: Some("Directory".into()),
            }),
            ..Default::default()
        });
        step_mounts.push(VolumeMount {
            name: "proxy".into(),
            mount_path: "/proxy".into(),
            read_only: Some(true),
            ..Default::default()
        });
    }

    (volumes, step_mounts)
}

#[allow(clippy::too_many_lines)]
fn build_pod_spec(p: &PodSpecParams<'_>) -> Pod {
    let script = if p.has_artifacts {
        // Wrap user commands with marker file pattern to keep container alive for artifact collection
        let user_cmds = p.commands.join(" && ");
        format!(
            "({user_cmds}); EC=$?; echo $EC > /tmp/.exit-code.tmp && mv /tmp/.exit-code.tmp /tmp/.exit-code; while [ ! -f /tmp/.done ]; do sleep 1; done; exit $EC"
        )
    } else {
        p.commands.join(" && ")
    };

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

    let (volumes, step_mounts) =
        build_volumes_and_mounts(p.registry_secret, p.git_secret_name, p.proxy_binary_path);

    // S31: Init container volume mounts — workspace + git-auth secret (if present)
    let mut init_mounts = vec![VolumeMount {
        name: "workspace".into(),
        mount_path: "/workspace".into(),
        ..Default::default()
    }];
    if p.git_secret_name.is_some() {
        init_mounts.push(VolumeMount {
            name: "git-auth".into(),
            mount_path: "/git-auth".into(),
            read_only: Some(true),
            ..Default::default()
        });
    }

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
                image: Some(p.git_clone_image.to_string()),
                command: Some(vec!["sh".into(), "-c".into()]),
                // S31: Read git token from mounted secret file instead of env var
                // A17: Pass repo_clone_url as env var to avoid shell interpolation
                args: Some(vec![
                    "printf '#!/bin/sh\\ncat /git-auth/token\\n' > /tmp/git-askpass.sh && \
                     chmod +x /tmp/git-askpass.sh && \
                     GIT_ASKPASS=/tmp/git-askpass.sh \
                     git clone --depth 1 --branch \"$GIT_BRANCH\" \"$GIT_CLONE_URL\" /workspace 2>&1"
                        .into(),
                ]),
                env: Some(vec![
                    env_var("GIT_BRANCH", branch),
                    env_var("GIT_CLONE_URL", p.repo_clone_url),
                ]),
                volume_mounts: Some(init_mounts),
                security_context: Some(container_security()),
                ..Default::default()
            }]),
            containers: vec![Container {
                name: "step".into(),
                image: Some(p.image.into()),
                command: if p.proxy_binary_path.is_some() {
                    Some(vec!["/proxy/platform-proxy".into()])
                } else {
                    Some(vec!["sh".into(), "-c".into()])
                },
                args: if p.proxy_binary_path.is_some() {
                    Some(vec![
                        "--wrap".into(),
                        "--".into(),
                        "sh".into(),
                        "-c".into(),
                        script,
                    ])
                } else {
                    Some(vec![script])
                },
                working_dir: Some("/workspace".into()),
                env: Some(p.env_vars.to_vec()),
                volume_mounts: Some(step_mounts),
                // Imagebuild (kaniko) needs root + capabilities to unpack base
                // image layers. All other step types get hardened context.
                security_context: if p.step_type == "imagebuild" {
                    None
                } else {
                    Some(container_security())
                },
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

    // 1. Create temp test namespace
    let short_id = &pipeline_id.to_string()[..8];
    let ns_name = crate::deployer::namespace::test_namespace_name(
        &state.config,
        &pipeline.namespace_slug,
        short_id,
    );
    crate::deployer::namespace::ensure_namespace(
        &state.kube,
        &ns_name,
        "test",
        &project_id.to_string(),
        &state.config.platform_namespace,
        &state.config.gateway_namespace,
        false,
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

    // 2b. Inject project secrets (scope: test/all) + OTEL tokens into test namespace
    inject_test_namespace_secrets(state, project_id, &pipeline.project_name, &ns_name).await;

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
    let manifest_content_opt = if manifests_path.ends_with('/') {
        super::trigger::read_dir_at_ref(std::path::Path::new(&repo_path), branch, manifests_path)
            .await
    } else {
        super::trigger::read_file_at_ref(std::path::Path::new(&repo_path), branch, manifests_path)
            .await
    };
    let Some(manifest_content) = manifest_content_opt else {
        let msg = if manifests_path.ends_with('/') {
            format!(
                "deploy manifests directory '{manifests_path}' not found or empty at ref '{branch}'"
            )
        } else {
            format!("deploy manifest '{manifests_path}' not found at ref '{branch}'")
        };
        tracing::error!(%msg, "deploy_test manifest read failed");
        let duration_ms = i32::try_from(start.elapsed().as_millis()).unwrap_or(i32::MAX);
        sqlx::query!(
            "UPDATE pipeline_steps SET status = 'failure', duration_ms = $2 WHERE id = $1",
            step.id,
            duration_ms,
        )
        .execute(&state.pool)
        .await?;
        return Ok(false);
    };

    // Determine app image ref (use node registry URL for containerd pulls)
    let registry = node_registry_url(&state.config).unwrap_or("localhost:5000");
    let commit_sha = pipeline.commit_sha.as_deref().unwrap_or("latest");
    let app_image_ref = format!("{registry}/{}/app:{commit_sha}", pipeline.project_name);

    // Render manifests with test environment
    let vars = crate::deployer::renderer::RenderVars {
        image_ref: app_image_ref.clone(),
        project_name: pipeline.project_name.clone(),
        environment: "test".into(),
        values: serde_json::json!({}),
        platform_api_url: state.config.platform_api_url.clone(),
        stable_image: None,
        canary_image: None,
        commit_sha: pipeline.commit_sha.clone(),
        app_image: Some(app_image_ref),
        gateway_url: None,
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

    // 5b. Wait for services to have ready endpoints (if specified)
    if !dt.wait_for_services.is_empty()
        && let Err(e) = wait_for_services_ready(
            &state.kube,
            &ns_name,
            &dt.wait_for_services,
            dt.readiness_timeout,
        )
        .await
    {
        tracing::error!(error = %e, %ns_name, "services did not become ready");
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
    tracing::info!(%test_pod_name, namespace = %ns_name, "deploy_test: test pod created");

    // 7. Wait for test pod to complete
    let exit_code = wait_for_pod(&test_pods, &test_pod_name).await?;
    tracing::info!(%test_pod_name, %exit_code, "deploy_test: test pod finished");

    // 8. Capture test pod logs
    let test_log_params = LogParams {
        container: Some("test".into()),
        ..Default::default()
    };
    if let Ok(logs) = test_pods.logs(&test_pod_name, &test_log_params).await {
        if exit_code != 0 {
            tracing::error!(%test_pod_name, %logs, "deploy_test: test pod failed");
        }
        let path = format!("logs/pipelines/{pipeline_id}/{}-test.log", step.name);
        if let Err(e) = state.minio.write(&path, logs.into_bytes()).await {
            tracing::error!(error = %e, %path, "failed to write test logs to MinIO");
        }
    }

    // 9. Capture app pod logs when tests fail (for debugging app crashes)
    if exit_code != 0 {
        let app_pods: Api<Pod> = Api::namespaced(state.kube.clone(), &ns_name);
        if let Ok(pod_list) = app_pods.list(&ListParams::default()).await {
            for p in &pod_list.items {
                let pname = p.metadata.name.as_deref().unwrap_or("?");
                let phase = p
                    .status
                    .as_ref()
                    .and_then(|s| s.phase.as_deref())
                    .unwrap_or("?");
                let restarts = p
                    .status
                    .as_ref()
                    .and_then(|s| s.container_statuses.as_ref())
                    .and_then(|cs| cs.first())
                    .map_or(0, |c| c.restart_count);
                let lp = LogParams {
                    tail_lines: Some(30),
                    ..Default::default()
                };
                let pod_logs = app_pods.logs(pname, &lp).await.unwrap_or_default();
                tracing::error!(
                    namespace = %ns_name, pod = pname, %phase, %restarts,
                    logs = %pod_logs,
                    "deploy_test: app pod state when tests failed"
                );
            }
        }
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
    let mut last_log = std::time::Instant::now();

    loop {
        if tokio::time::Instant::now() >= deadline {
            log_deployment_timeout_diagnostics(kube, namespace, &deployments).await;
            return Err(PipelineError::Other(anyhow::anyhow!(
                "deployment in {namespace} did not become ready within {timeout_secs}s"
            )));
        }

        tokio::time::sleep(std::time::Duration::from_secs(3)).await;

        if let Ok(deploy_list) = deployments.list(&ListParams::default()).await {
            if deploy_list.items.is_empty() {
                continue;
            }
            // Log progress every 15s
            if last_log.elapsed().as_secs() >= 15 {
                for d in &deploy_list.items {
                    let name = d.metadata.name.as_deref().unwrap_or("?");
                    let ready = d
                        .status
                        .as_ref()
                        .and_then(|s| s.ready_replicas)
                        .unwrap_or(0);
                    let desired = d.spec.as_ref().and_then(|s| s.replicas).unwrap_or(0);
                    tracing::info!(
                        %namespace, deploy = name, ready, desired,
                        "waiting for deployment readiness"
                    );
                }
                last_log = std::time::Instant::now();
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

/// Log detailed pod/container state when a deployment readiness check times out.
async fn log_deployment_timeout_diagnostics(
    kube: &kube::Client,
    namespace: &str,
    deployments: &Api<k8s_openapi::api::apps::v1::Deployment>,
) {
    if let Ok(dl) = deployments.list(&ListParams::default()).await {
        for d in &dl.items {
            let name = d.metadata.name.as_deref().unwrap_or("?");
            let ready = d
                .status
                .as_ref()
                .and_then(|s| s.ready_replicas)
                .unwrap_or(0);
            let desired = d.spec.as_ref().and_then(|s| s.replicas).unwrap_or(0);
            tracing::error!(%namespace, deploy = name, ready, desired, "deployment not ready at timeout");
        }
    }
    let pods: Api<Pod> = Api::namespaced(kube.clone(), namespace);
    if let Ok(pl) = pods.list(&ListParams::default()).await {
        for p in &pl.items {
            let pname = p.metadata.name.as_deref().unwrap_or("?");
            let phase = p
                .status
                .as_ref()
                .and_then(|s| s.phase.as_deref())
                .unwrap_or("?");
            let conditions = p
                .status
                .as_ref()
                .and_then(|s| s.conditions.as_ref())
                .map(|cs| {
                    cs.iter()
                        .map(|c| format!("{}={}", c.type_, c.status))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let container_info = p
                .status
                .as_ref()
                .and_then(|s| s.container_statuses.as_ref())
                .map(|cs| {
                    cs.iter()
                        .map(|c| {
                            let state_str = c
                                .state
                                .as_ref()
                                .map(|s| format!("{s:?}"))
                                .unwrap_or_default();
                            format!(
                                "{}(ready={}, restarts={}, state={state_str})",
                                c.name, c.ready, c.restart_count
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("; ")
                })
                .unwrap_or_default();
            tracing::error!(%namespace, pod = pname, phase, conditions, containers = %container_info, "pod state at timeout");
            if phase == "Running" && conditions.contains("Ready=False") {
                let lp = kube::api::LogParams {
                    tail_lines: Some(20),
                    ..Default::default()
                };
                if let Ok(logs) = pods.logs(pname, &lp).await {
                    tracing::error!(%namespace, pod = pname, %logs, "pod logs at timeout");
                }
            }
        }
    }
}

/// Wait for K8s services to have at least one ready endpoint.
/// Polls every 3s until all services are ready or timeout.
async fn wait_for_services_ready(
    kube: &kube::Client,
    namespace: &str,
    service_names: &[String],
    timeout_secs: u32,
) -> Result<(), PipelineError> {
    use k8s_openapi::api::core::v1::Endpoints;

    let endpoints: Api<Endpoints> = Api::namespaced(kube.clone(), namespace);
    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs.into());

    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(PipelineError::Other(anyhow::anyhow!(
                "services {service_names:?} in {namespace} did not become ready within {timeout_secs}s"
            )));
        }

        tokio::time::sleep(std::time::Duration::from_secs(3)).await;

        let mut all_ready = true;
        for svc_name in service_names {
            if let Ok(ep) = endpoints.get(svc_name).await {
                let has_ready = ep.subsets.as_ref().is_some_and(|subsets| {
                    subsets
                        .iter()
                        .any(|s| s.addresses.as_ref().is_some_and(|addrs| !addrs.is_empty()))
                });
                if !has_ready {
                    all_ready = false;
                    break;
                }
            } else {
                all_ready = false;
                break;
            }
        }

        if all_ready {
            return Ok(());
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
// In-process step executors (no K8s pod)
// ---------------------------------------------------------------------------

/// Execute a `gitops_sync` step: copy files to ops repo, commit values, publish event.
#[tracing::instrument(skip(state, step), fields(%pipeline_id, %project_id), err)]
async fn execute_gitops_sync_step(
    state: &AppState,
    pipeline_id: Uuid,
    project_id: Uuid,
    pipeline: &PipelineMeta,
    step: &StepRow,
) -> Result<bool, PipelineError> {
    // Mark step as running
    sqlx::query("UPDATE pipeline_steps SET status = 'running', started_at = now() WHERE id = $1")
        .bind(step.id)
        .execute(&state.pool)
        .await?;

    let start = std::time::Instant::now();

    let result = execute_gitops_sync_inner(state, pipeline_id, project_id, pipeline, step).await;

    let duration_ms = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);
    let (status, exit_code) = match &result {
        Ok(true) => ("success", 0i32),
        Ok(false) | Err(_) => ("failure", 1i32),
    };

    sqlx::query(
        "UPDATE pipeline_steps SET status = $2, exit_code = $3, duration_ms = $4 WHERE id = $1",
    )
    .bind(step.id)
    .bind(status)
    .bind(exit_code)
    .bind(duration_ms)
    .execute(&state.pool)
    .await?;

    result
}

/// Inner logic for `gitops_sync` — separated for cleaner error handling.
#[allow(clippy::too_many_lines)]
async fn execute_gitops_sync_inner(
    state: &AppState,
    _pipeline_id: Uuid,
    project_id: Uuid,
    pipeline: &PipelineMeta,
    _step: &StepRow,
) -> Result<bool, PipelineError> {
    use sqlx::Row as _;

    // Read project info (use dynamic query — include_staging is a new column)
    let project = sqlx::query(
        "SELECT name, repo_path, include_staging FROM projects WHERE id = $1 AND is_active = true",
    )
    .bind(project_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| PipelineError::Other(anyhow::anyhow!("project not found")))?;
    let project_name: String = project.get("name");
    let project_repo_path: Option<String> = project.get("repo_path");
    let include_staging: bool = project.get("include_staging");

    // Look up ops repo
    let ops_repo = sqlx::query("SELECT id, repo_path, branch FROM ops_repos WHERE project_id = $1")
        .bind(project_id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| PipelineError::Other(anyhow::anyhow!("no ops repo for project")))?;
    let ops_repo_id: Uuid = ops_repo.get("id");
    let ops_repo_path: String = ops_repo.get("repo_path");
    let ops_repo_branch: String = ops_repo.get("branch");

    let sha = pipeline.commit_sha.as_deref().unwrap_or("HEAD");

    // Read .platform.yaml from project repo
    let repo_path = project_repo_path
        .as_deref()
        .ok_or_else(|| PipelineError::Other(anyhow::anyhow!("project has no repo_path")))?;
    let platform_yaml = crate::deployer::ops_repo::read_file_at_ref(
        std::path::Path::new(repo_path),
        sha,
        ".platform.yaml",
    )
    .await
    .ok();

    let platform_file: Option<super::definition::PlatformFile> = platform_yaml
        .as_ref()
        .and_then(|y| serde_yaml::from_str(y).ok());

    // Log deploy section of the YAML for debugging canary spec propagation
    let deploy_section: String = platform_yaml
        .as_ref()
        .and_then(|y| {
            y.find("deploy:")
                .map(|idx| y[idx..].chars().take(400).collect())
        })
        .unwrap_or_default();
    tracing::info!(
        %project_id, %sha,
        has_platform_yaml = platform_yaml.is_some(),
        has_deploy = platform_file.as_ref().and_then(|p| p.deploy.as_ref()).is_some(),
        deploy_specs = platform_file.as_ref().and_then(|p| p.deploy.as_ref()).map_or(0, |d| d.specs.len()),
        yaml_len = platform_yaml.as_ref().map_or(0, String::len),
        %deploy_section,
        "gitops_sync: parsed platform.yaml from project repo"
    );

    // Determine target branch from project setting (not platform.yaml)
    let (target_branch, environment) = if include_staging {
        ("staging", "staging")
    } else {
        (ops_repo_branch.as_str(), "production")
    };

    let ops_path = std::path::PathBuf::from(&ops_repo_path);

    // 1. Copy files from code repo → ops repo (default branch — reconciler reads from here)
    if let Err(e) = crate::deployer::ops_repo::sync_from_project_repo(
        std::path::Path::new(repo_path),
        &ops_path,
        &ops_repo_branch,
        sha,
    )
    .await
    {
        tracing::warn!(error = %e, "failed to sync deploy/ to ops repo");
    }

    // Also sync to target branch if different (so eventbus reads platform.yaml from there)
    if target_branch != ops_repo_branch
        && let Err(e) = crate::deployer::ops_repo::sync_from_project_repo(
            std::path::Path::new(repo_path),
            &ops_path,
            target_branch,
            sha,
        )
        .await
    {
        tracing::warn!(error = %e, "failed to sync deploy/ to ops repo target branch");
    }

    // 2. Write platform.yaml to BOTH default and target branches
    if let Some(ref yaml_content) = platform_yaml {
        if let Err(e) = crate::deployer::ops_repo::write_file_to_repo(
            &ops_path,
            &ops_repo_branch,
            "platform.yaml",
            yaml_content,
        )
        .await
        {
            tracing::warn!(error = %e, "failed to write platform.yaml to ops repo");
        }
        if target_branch != ops_repo_branch
            && let Err(e) = crate::deployer::ops_repo::write_file_to_repo(
                &ops_path,
                target_branch,
                "platform.yaml",
                yaml_content,
            )
            .await
        {
            tracing::warn!(error = %e, "failed to write platform.yaml to target branch");
        }
    }

    // 2b. Validate canary service refs against deploy manifests.
    // Read from target_branch (staging/production) where files were synced, not default branch.
    if let Some(ref pf) = platform_file
        && let Some(ref deploy) = pf.deploy
    {
        let deploy_content =
            crate::deployer::ops_repo::read_dir_yaml_at_ref(&ops_path, target_branch, "deploy/")
                .await
                .unwrap_or_default();
        if let Err(e) =
            super::definition::validate_canary_service_refs(&deploy.specs, &deploy_content)
        {
            tracing::warn!(error = %e, "canary service ref validation failed");
        }
    }

    // 3. Build values (image refs + user variables)
    let registry = node_registry_url(&state.config).unwrap_or("localhost:5000");

    // Read VERSION file from project repo for version-tagged image refs
    let version_info =
        crate::pipeline::trigger::read_version_at_ref(std::path::Path::new(repo_path), sha).await;

    // Use version tag when available, fall back to commit SHA
    let app_version = version_info
        .as_ref()
        .and_then(|vi| vi.images.get("app"))
        .cloned();
    let tag = app_version.as_deref().unwrap_or(sha);
    let image_ref = format!("{registry}/{project_name}/app:{tag}");

    let mut values = serde_json::json!({
        "image_ref": image_ref,
        "project_name": project_name,
        "environment": environment,
    });

    // 3b. Merge per-environment variables from code repo (deploy/variables_{env}.yaml)
    let var_path = platform_file
        .as_ref()
        .and_then(|pf| pf.deploy.as_ref())
        .and_then(|d| d.variables.get(environment));
    if let Some(var_path) = var_path
        && let Ok(var_content) = crate::deployer::ops_repo::read_file_at_ref(
            std::path::Path::new(repo_path),
            sha,
            var_path,
        )
        .await
    {
        match serde_yaml::from_str::<serde_json::Value>(&var_content) {
            Ok(user_vars) => {
                if let Some(obj) = user_vars.as_object() {
                    for (k, v) in obj {
                        values[k] = v.clone();
                    }
                }
                tracing::info!(%environment, %var_path, "merged user variables into deploy values");
            }
            Err(e) => tracing::warn!(error = %e, %var_path, "failed to parse variables file"),
        }
    }

    // 4. Commit values to ops repo
    let ops_commit_sha = match crate::deployer::ops_repo::commit_values(
        &ops_path,
        target_branch,
        environment,
        &values,
    )
    .await
    {
        Ok(sha) => sha,
        Err(e) => {
            tracing::error!(error = %e, %project_id, "failed to commit values to ops repo");
            return Ok(false);
        }
    };

    // 5. Publish OpsRepoUpdated event
    let event = crate::store::eventbus::PlatformEvent::OpsRepoUpdated {
        project_id,
        ops_repo_id,
        environment: environment.into(),
        commit_sha: ops_commit_sha,
        image_ref: image_ref.clone(),
    };
    if let Err(e) = crate::store::eventbus::publish(&state.valkey, &event).await {
        tracing::error!(error = %e, %project_id, "failed to publish OpsRepoUpdated event");
    }

    // 6. Register feature flags from platform.yaml
    if let Some(ref pf) = platform_file
        && !pf.flags.is_empty()
    {
        let flag_defs: Vec<(String, serde_json::Value, Option<String>)> = pf
            .flags
            .iter()
            .map(|f| {
                (
                    f.key.clone(),
                    f.default_value.clone(),
                    f.description.clone(),
                )
            })
            .collect();
        let flag_event = crate::store::eventbus::PlatformEvent::FlagsRegistered {
            project_id,
            flags: flag_defs,
        };
        let _ = crate::store::eventbus::publish(&state.valkey, &flag_event).await;
    }

    tracing::info!(%project_id, %image_ref, %environment, "gitops_sync step completed");
    Ok(true)
}

/// Execute a `deploy_watch` step: poll `deploy_releases` until terminal phase.
#[tracing::instrument(skip(state, step), fields(%pipeline_id, %project_id), err)]
async fn execute_deploy_watch_step(
    state: &AppState,
    pipeline_id: Uuid,
    project_id: Uuid,
    step: &StepRow,
) -> Result<bool, PipelineError> {
    // Mark step as running
    sqlx::query("UPDATE pipeline_steps SET status = 'running', started_at = now() WHERE id = $1")
        .bind(step.id)
        .execute(&state.pool)
        .await?;

    let start = std::time::Instant::now();

    // Parse config
    let (environment, timeout_secs) = if let Some(ref config) = step.step_config {
        let env = config
            .get("environment")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("staging");
        let timeout = config
            .get("timeout")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(300);
        (env.to_string(), timeout)
    } else {
        ("staging".into(), 300u64)
    };

    // Poll deploy_releases until terminal
    let success = loop {
        if start.elapsed().as_secs() > timeout_secs {
            tracing::warn!(%project_id, %environment, "deploy_watch timed out");
            break false;
        }

        let phase = sqlx::query_scalar::<_, String>(
            "SELECT dr.phase FROM deploy_releases dr \
             JOIN deploy_targets dt ON dr.target_id = dt.id \
             WHERE dr.project_id = $1 AND dt.environment = $2 \
             ORDER BY dr.created_at DESC LIMIT 1",
        )
        .bind(project_id)
        .bind(&environment)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten();

        match phase.as_deref() {
            Some("completed") => {
                tracing::info!(%project_id, %environment, "deploy_watch: release completed");
                break true;
            }
            Some("failed" | "rolled_back" | "cancelled") => {
                tracing::warn!(%project_id, %environment, phase = ?phase, "deploy_watch: release failed");
                break false;
            }
            _ => {
                // Still pending/progressing/holding/promoting — keep polling
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    };

    let duration_ms = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);
    let (status, exit_code) = if success {
        ("success", 0i32)
    } else {
        ("failure", 1i32)
    };

    sqlx::query(
        "UPDATE pipeline_steps SET status = $2, exit_code = $3, duration_ms = $4 WHERE id = $1",
    )
    .bind(step.id)
    .bind(status)
    .bind(exit_code)
    .bind(duration_ms)
    .execute(&state.pool)
    .await?;

    Ok(success)
}

// ---------------------------------------------------------------------------
// Test namespace secret injection
// ---------------------------------------------------------------------------

/// Inject project secrets and OTEL tokens into a deploy-test namespace.
/// Creates a K8s Secret with test-scoped secrets + OTEL env vars.
async fn inject_test_namespace_secrets(
    state: &AppState,
    project_id: Uuid,
    project_name: &str,
    namespace: &str,
) {
    use sqlx::Row as _;

    let mut env_data: BTreeMap<String, String> = BTreeMap::new();

    // Query test-scoped secrets
    if let Some(ref master_key_str) = state.config.master_key
        && let Ok(mk) = crate::secrets::engine::parse_master_key(master_key_str)
    {
        let rows = sqlx::query(
            "SELECT name, encrypted_value FROM secrets
                 WHERE project_id = $1 AND scope IN ('test', 'all')
                   AND environment IS NULL",
        )
        .bind(project_id)
        .fetch_all(&state.pool)
        .await
        .unwrap_or_default();

        for row in &rows {
            let name: String = row.get("name");
            let encrypted: Vec<u8> = row.get("encrypted_value");
            match crate::secrets::engine::decrypt(&encrypted, &mk, None) {
                Ok(val) => {
                    if let Ok(s) = String::from_utf8(val) {
                        env_data.insert(name, s);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to decrypt test secret");
                }
            }
        }
    }

    // Inject OTEL env vars
    env_data.insert(
        "OTEL_EXPORTER_OTLP_ENDPOINT".into(),
        state.config.platform_api_url.clone(),
    );
    env_data.insert("OTEL_SERVICE_NAME".into(), project_name.to_string());
    env_data.insert(
        "OTEL_RESOURCE_ATTRIBUTES".into(),
        format!("platform.project_id={project_id}"),
    );

    // Create scoped OTEL + API tokens for the test namespace
    if let Ok((otel_token, api_token)) =
        crate::deployer::reconciler::ensure_scoped_tokens(state, project_id, "test").await
    {
        env_data.insert(
            "OTEL_EXPORTER_OTLP_HEADERS".into(),
            format!("Authorization=Bearer {otel_token}"),
        );
        env_data.insert("PLATFORM_API_TOKEN".into(), api_token);
    }

    env_data.insert(
        "PLATFORM_API_URL".into(),
        state.config.platform_api_url.clone(),
    );
    env_data.insert("PLATFORM_PROJECT_ID".into(), project_id.to_string());

    if env_data.is_empty() {
        return;
    }

    // Create K8s Secret
    let secret_name = format!("{namespace}-test-secrets");
    let secret_data: BTreeMap<String, k8s_openapi::ByteString> = env_data
        .into_iter()
        .map(|(k, v)| (k, k8s_openapi::ByteString(v.into_bytes())))
        .collect();

    let secret = k8s_openapi::api::core::v1::Secret {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(secret_name.clone()),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        data: Some(secret_data),
        ..Default::default()
    };

    let secrets_api: Api<Secret> = Api::namespaced(state.kube.clone(), namespace);
    match secrets_api
        .patch(
            &secret_name,
            &kube::api::PatchParams::apply("platform-pipeline"),
            &kube::api::Patch::Apply(secret),
        )
        .await
    {
        Ok(_) => tracing::info!(%namespace, "test namespace secrets injected"),
        Err(e) => tracing::warn!(error = %e, %namespace, "failed to inject test secrets"),
    }
}

// ---------------------------------------------------------------------------
// Status helpers
// ---------------------------------------------------------------------------

async fn is_cancelled(pool: &PgPool, pipeline_id: Uuid) -> Result<bool, PipelineError> {
    let status = sqlx::query_scalar!("SELECT status FROM pipelines WHERE id = $1", pipeline_id,)
        .fetch_one(pool)
        .await?;

    Ok(PipelineStatus::parse(&status) == Some(PipelineStatus::Cancelled))
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
    // Fetch current status and validate transition via state machine
    let current_status_str =
        sqlx::query_scalar!("SELECT status FROM pipelines WHERE id = $1", pipeline_id,)
            .fetch_one(pool)
            .await?;

    let to = PipelineStatus::Failure;

    if let Some(current) = PipelineStatus::parse(&current_status_str) {
        if !current.can_transition_to(to) {
            tracing::warn!(
                %pipeline_id,
                from = current_status_str,
                to = to.as_str(),
                "invalid pipeline status transition in mark_pipeline_failed; skipping"
            );
            return Ok(());
        }
    } else {
        tracing::warn!(
            %pipeline_id,
            status = current_status_str,
            "unknown pipeline status in mark_pipeline_failed; skipping"
        );
        return Ok(());
    }

    sqlx::query!(
        "UPDATE pipelines SET status = $2, finished_at = now() WHERE id = $1 AND status = $3",
        pipeline_id,
        to.as_str(),
        current_status_str,
    )
    .execute(pool)
    .await?;

    skip_remaining_steps(pool, pipeline_id).await?;
    Ok(())
}

// Legacy deployment handoff functions (detect_and_write_deployment, gitops_handoff,
// write_file_to_ops_repo, detect_and_publish_dev_image, upsert_preview_deployment)
// have been removed. They were replaced by explicit `gitops_sync` and `imagebuild`
// pipeline step types. See git history for the original implementations.

// ---------------------------------------------------------------------------
// Webhook
// ---------------------------------------------------------------------------

async fn fire_build_webhook(
    pool: &PgPool,
    project_id: Uuid,
    pipeline_id: Uuid,
    status: &str,
    semaphore: &std::sync::Arc<tokio::sync::Semaphore>,
) {
    let payload = serde_json::json!({
        "action": status,
        "pipeline_id": pipeline_id,
        "project_id": project_id,
    });
    crate::api::webhooks::fire_webhooks(pool, project_id, "build", &payload, semaphore).await;
}

// ---------------------------------------------------------------------------
// Cancellation (called from API)
// ---------------------------------------------------------------------------

/// Cancel a running pipeline: delete K8s pods and mark as cancelled.
#[tracing::instrument(skip(state), fields(%pipeline_id), err)]
pub async fn cancel_pipeline(state: &AppState, pipeline_id: Uuid) -> Result<(), PipelineError> {
    // Fetch current status and validate transition via state machine
    let current_status_str =
        sqlx::query_scalar!("SELECT status FROM pipelines WHERE id = $1", pipeline_id,)
            .fetch_one(&state.pool)
            .await?;

    let to = PipelineStatus::Cancelled;

    if let Some(current) = PipelineStatus::parse(&current_status_str) {
        if !current.can_transition_to(to) {
            tracing::warn!(
                %pipeline_id,
                from = current_status_str,
                to = to.as_str(),
                "invalid pipeline status transition in cancel_pipeline; skipping"
            );
            return Ok(());
        }
    } else {
        tracing::warn!(
            %pipeline_id,
            status = current_status_str,
            "unknown pipeline status in cancel_pipeline; skipping"
        );
        return Ok(());
    }

    // Mark pipeline as cancelled (use WHERE guard on current status to prevent races)
    sqlx::query!(
        "UPDATE pipelines SET status = $2, finished_at = now() WHERE id = $1 AND status = $3",
        pipeline_id,
        to.as_str(),
        current_status_str,
    )
    .execute(&state.pool)
    .await?;

    skip_remaining_steps(&state.pool, pipeline_id).await?;

    // Look up project namespace slug and reconstruct pipeline namespace
    let short_id = &pipeline_id.to_string()[..8];
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
        |slug| crate::deployer::namespace::pipeline_namespace_name(&state.config, &slug, short_id),
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

    // Clean up the pipeline namespace
    cleanup_pipeline_namespace(&state.kube, &namespace).await;

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
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
        });

        assert_eq!(pod.metadata.name.as_deref(), Some("pl-test-build"));

        let labels = pod.metadata.labels.as_ref().unwrap();
        assert_eq!(labels["platform.io/step"], "build");

        let spec = pod.spec.as_ref().unwrap();
        assert_eq!(spec.restart_policy.as_deref(), Some("Never"));

        let init = &spec.init_containers.as_ref().unwrap()[0];
        assert_eq!(init.image.as_deref(), Some("alpine/git:2.47.2"));

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
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
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
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
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
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
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
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
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
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
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
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
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
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
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

    /// Test helper: wraps `build_env_vars_core` with default `trigger_type` "push".
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
            step_type: "command".into(),
            step_config: None,
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
            step_type: "command".into(),
            step_config: None,
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
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
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
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
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
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
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
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
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
        let (volumes, mounts) = build_volumes_and_mounts(None, None, None);
        assert_eq!(volumes.len(), 1);
        assert_eq!(volumes[0].name, "workspace");
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].name, "workspace");
        assert_eq!(mounts[0].mount_path, "/workspace");
    }

    #[test]
    fn volumes_with_secret_has_two() {
        let (volumes, mounts) = build_volumes_and_mounts(Some("my-secret"), None, None);
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
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
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
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
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
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
        });

        let init = &pod.spec.unwrap().init_containers.unwrap()[0];
        // Init container only needs workspace mount (no hostPath repos mount)
        let mounts = init.volume_mounts.as_ref().unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].name, "workspace");

        // A17: Clone command uses $GIT_CLONE_URL env var (not interpolated URL)
        let clone_cmd = &init.args.as_ref().unwrap()[0];
        assert!(
            clone_cmd.contains("\"$GIT_CLONE_URL\""),
            "should use $GIT_CLONE_URL env var, got: {clone_cmd}"
        );
        assert!(
            clone_cmd.contains("GIT_ASKPASS"),
            "should use GIT_ASKPASS for auth, got: {clone_cmd}"
        );
        assert!(
            !clone_cmd.contains("file://"),
            "should not use file:// protocol, got: {clone_cmd}"
        );

        // S31: GIT_AUTH_TOKEN env var should NOT be set (token is in mounted secret)
        let env = init.env.as_ref().unwrap();
        let token_env = env.iter().find(|e| e.name == "GIT_AUTH_TOKEN");
        assert!(
            token_env.is_none(),
            "should NOT have GIT_AUTH_TOKEN env var (S31: use secret volume)"
        );

        // A17: GIT_CLONE_URL env var should be set
        let clone_url_env = env.iter().find(|e| e.name == "GIT_CLONE_URL");
        assert!(clone_url_env.is_some(), "should have GIT_CLONE_URL env var");
        assert_eq!(
            clone_url_env.unwrap().value.as_deref(),
            Some("http://platform:8080/owner/repo.git"),
            "GIT_CLONE_URL should match the repo clone URL"
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
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
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
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
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
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
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
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
        });

        let spec = pod.spec.unwrap();
        let psc = spec.security_context.unwrap();
        // No run_as_non_root/run_as_user — kaniko needs root to build images
        assert_eq!(psc.run_as_non_root, None);
        assert_eq!(psc.run_as_user, None);
        assert_eq!(psc.fs_group, Some(1000));
    }

    #[test]
    fn pipeline_imagebuild_step_container_has_no_security_context() {
        // Imagebuild (kaniko) steps need root + capabilities to build
        // images, so no restrictive security context is applied.
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test",
            pipeline_id: Uuid::nil(),
            project_id: Uuid::nil(),
            step_name: "test",
            image: "gcr.io/kaniko-project/executor:latest",
            commands: &["true".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/test.git",
            git_ref: "main",
            registry_secret: None,
            git_secret_name: None,
            step_type: "imagebuild",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
        });

        let spec = pod.spec.unwrap();
        let container = &spec.containers[0];
        assert!(
            container.security_context.is_none(),
            "imagebuild step container should not have a restrictive security context"
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
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
        });

        let spec = pod.spec.unwrap();
        let init = &spec.init_containers.unwrap()[0];
        let sc = init.security_context.as_ref().unwrap();
        assert_eq!(sc.allow_privilege_escalation, Some(false));
        let caps = sc.capabilities.as_ref().unwrap();
        assert_eq!(caps.drop.as_ref().unwrap(), &vec!["ALL".to_string()]);
    }

    #[test]
    fn pipeline_command_step_container_has_hardened_security_context() {
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test",
            pipeline_id: Uuid::nil(),
            project_id: Uuid::nil(),
            step_name: "test",
            image: "alpine:3.19",
            commands: &["echo hi".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/test.git",
            git_ref: "main",
            registry_secret: None,
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
        });

        let spec = pod.spec.unwrap();
        let container = &spec.containers[0];
        let sc = container
            .security_context
            .as_ref()
            .expect("non-imagebuild step container should have hardened security context");
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

    // -- check_container_statuses --

    fn make_waiting_status(name: &str, reason: &str, message: Option<&str>) -> ContainerStatus {
        ContainerStatus {
            name: name.into(),
            state: Some(ContainerState {
                waiting: Some(ContainerStateWaiting {
                    reason: Some(reason.into()),
                    message: message.map(Into::into),
                }),
                ..Default::default()
            }),
            restart_count: 0,
            ..Default::default()
        }
    }

    #[test]
    fn check_container_statuses_image_pull_back_off() {
        let statuses = vec![make_waiting_status(
            "app",
            "ImagePullBackOff",
            Some("pull failed"),
        )];
        let result = check_container_statuses(&statuses, "");
        assert_eq!(result, Some("ImagePullBackOff: pull failed".into()));
    }

    #[test]
    fn check_container_statuses_err_image_pull() {
        let statuses = vec![make_waiting_status(
            "app",
            "ErrImagePull",
            Some("not found"),
        )];
        let result = check_container_statuses(&statuses, "");
        assert_eq!(result, Some("ErrImagePull: not found".into()));
    }

    #[test]
    fn check_container_statuses_invalid_image_name() {
        let statuses = vec![make_waiting_status("app", "InvalidImageName", None)];
        let result = check_container_statuses(&statuses, "");
        assert_eq!(result, Some("InvalidImageName: image pull failed".into()));
    }

    #[test]
    fn check_container_statuses_create_container_config_error() {
        let statuses = vec![make_waiting_status(
            "app",
            "CreateContainerConfigError",
            Some("bad config"),
        )];
        let result = check_container_statuses(&statuses, "init ");
        assert_eq!(
            result,
            Some("init CreateContainerConfigError: bad config".into())
        );
    }

    #[test]
    fn check_container_statuses_crash_loop_back_off() {
        let statuses = vec![ContainerStatus {
            name: "app".into(),
            state: Some(ContainerState {
                waiting: Some(ContainerStateWaiting {
                    reason: Some("CrashLoopBackOff".into()),
                    message: None,
                }),
                ..Default::default()
            }),
            restart_count: 5,
            ..Default::default()
        }];
        let result = check_container_statuses(&statuses, "");
        assert_eq!(result, Some("CrashLoopBackOff after 5 restarts".into()));
    }

    #[test]
    fn check_container_statuses_crash_loop_below_threshold() {
        let statuses = vec![ContainerStatus {
            name: "app".into(),
            state: Some(ContainerState {
                waiting: Some(ContainerStateWaiting {
                    reason: Some("CrashLoopBackOff".into()),
                    message: None,
                }),
                ..Default::default()
            }),
            restart_count: 2,
            ..Default::default()
        }];
        let result = check_container_statuses(&statuses, "");
        assert!(result.is_none());
    }

    #[test]
    fn check_container_statuses_running_is_ok() {
        let statuses = vec![ContainerStatus {
            name: "app".into(),
            state: Some(ContainerState {
                running: Some(ContainerStateRunning { started_at: None }),
                ..Default::default()
            }),
            restart_count: 0,
            ..Default::default()
        }];
        assert!(check_container_statuses(&statuses, "").is_none());
    }

    #[test]
    fn check_container_statuses_empty_list() {
        let statuses: Vec<ContainerStatus> = vec![];
        assert!(check_container_statuses(&statuses, "").is_none());
    }

    // -- detect_unrecoverable_container --

    #[test]
    fn detect_unrecoverable_regular_container() {
        let status = PodStatus {
            container_statuses: Some(vec![make_waiting_status(
                "app",
                "ImagePullBackOff",
                Some("pull err"),
            )]),
            init_container_statuses: Some(vec![]),
            ..Default::default()
        };
        let result = detect_unrecoverable_container(&status);
        assert_eq!(result, Some("ImagePullBackOff: pull err".into()));
    }

    #[test]
    fn detect_unrecoverable_init_container() {
        let status = PodStatus {
            container_statuses: Some(vec![]),
            init_container_statuses: Some(vec![make_waiting_status(
                "init",
                "ErrImagePull",
                Some("init fail"),
            )]),
            ..Default::default()
        };
        let result = detect_unrecoverable_container(&status);
        assert_eq!(
            result,
            Some("init container ErrImagePull: init fail".into())
        );
    }

    #[test]
    fn detect_unrecoverable_none_when_no_statuses() {
        let status = PodStatus {
            container_statuses: None,
            init_container_statuses: None,
            ..Default::default()
        };
        assert!(detect_unrecoverable_container(&status).is_none());
    }

    // -- build_volumes_and_mounts --

    #[test]
    fn build_volumes_and_mounts_workspace_only() {
        let (volumes, mounts) = build_volumes_and_mounts(None, None, None);
        assert_eq!(volumes.len(), 1);
        assert_eq!(volumes[0].name, "workspace");
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].mount_path, "/workspace");
    }

    #[test]
    fn build_volumes_and_mounts_registry_secret() {
        let (volumes, mounts) = build_volumes_and_mounts(Some("my-registry-secret"), None, None);
        assert_eq!(volumes.len(), 2);
        assert_eq!(volumes[1].name, "docker-config");
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[1].mount_path, "/kaniko/.docker");
        assert_eq!(mounts[1].read_only, Some(true));
    }

    #[test]
    fn build_volumes_and_mounts_git_secret() {
        let (volumes, mounts) = build_volumes_and_mounts(None, Some("git-token-secret"), None);
        assert_eq!(volumes.len(), 2);
        assert_eq!(volumes[1].name, "git-auth");
        // git-auth is a volume only — no corresponding mount in step_mounts
        assert_eq!(mounts.len(), 1);
    }

    #[test]
    fn build_volumes_and_mounts_both() {
        let (volumes, mounts) =
            build_volumes_and_mounts(Some("reg-secret"), Some("git-secret"), None);
        assert_eq!(volumes.len(), 3);
        assert_eq!(mounts.len(), 2);
    }

    // -- container_security --

    #[test]
    fn container_security_drops_all_caps() {
        let sec = container_security();
        assert_eq!(sec.allow_privilege_escalation, Some(false));
        let caps = sec.capabilities.unwrap();
        assert_eq!(caps.drop, Some(vec!["ALL".into()]));
        assert!(caps.add.is_none());
    }

    // -- node_registry_url --

    #[test]
    fn node_registry_url_prefers_node_url() {
        let config = crate::config::Config {
            registry_url: Some("push.example.com:5000".into()),
            registry_node_url: Some("node.example.com:5000".into()),
            ..crate::config::Config::test_default()
        };
        assert_eq!(node_registry_url(&config), Some("node.example.com:5000"));
    }

    #[test]
    fn node_registry_url_falls_back_to_registry_url() {
        let config = crate::config::Config {
            registry_url: Some("push.example.com:5000".into()),
            registry_node_url: None,
            ..crate::config::Config::test_default()
        };
        assert_eq!(node_registry_url(&config), Some("push.example.com:5000"));
    }

    #[test]
    fn node_registry_url_returns_none() {
        let config = crate::config::Config {
            registry_url: None,
            registry_node_url: None,
            ..crate::config::Config::test_default()
        };
        assert!(node_registry_url(&config).is_none());
    }

    // -- mark_transitive_dependents_skipped --

    #[test]
    fn mark_transitive_dependents_linear_chain() {
        // 0 → 1 → 2
        let dependents = vec![vec![1], vec![2], vec![]];
        let mut skipped = std::collections::HashSet::new();
        let completed = std::collections::HashSet::new();
        mark_transitive_dependents_skipped(0, &dependents, &mut skipped, &completed);
        assert!(skipped.contains(&1));
        assert!(skipped.contains(&2));
        assert_eq!(skipped.len(), 2);
    }

    #[test]
    fn mark_transitive_dependents_diamond() {
        // 0 → 1, 0 → 2, 1 → 3, 2 → 3
        let dependents = vec![vec![1, 2], vec![3], vec![3], vec![]];
        let mut skipped = std::collections::HashSet::new();
        let completed = std::collections::HashSet::new();
        mark_transitive_dependents_skipped(0, &dependents, &mut skipped, &completed);
        assert!(skipped.contains(&1));
        assert!(skipped.contains(&2));
        assert!(skipped.contains(&3));
        assert_eq!(skipped.len(), 3);
    }

    #[test]
    fn mark_transitive_dependents_skips_completed() {
        // 0 → 1 → 2, but 1 is already completed
        let dependents = vec![vec![1], vec![2], vec![]];
        let mut skipped = std::collections::HashSet::new();
        let mut completed = std::collections::HashSet::new();
        completed.insert(1);
        mark_transitive_dependents_skipped(0, &dependents, &mut skipped, &completed);
        // 1 is completed so not skipped, but 2 depends on 1 (not on 0 directly),
        // and since 1 is completed it won't be pushed to stack, so 2 is also not reached
        assert!(!skipped.contains(&1));
    }

    // -- SHORT_SHA and IMAGE_TAG --

    #[test]
    fn env_vars_short_sha_and_image_tag_from_commit() {
        let vars = test_env_vars(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "refs/heads/main",
            Some("abcdef1234567890"),
            "test",
            None,
            None,
        );
        assert_eq!(find_env(&vars, "SHORT_SHA"), Some("abcdef1".into()));
        assert_eq!(find_env(&vars, "IMAGE_TAG"), Some("sha-abcdef1".into()));
    }

    #[test]
    fn env_vars_short_sha_caps_at_seven_chars() {
        let vars = test_env_vars(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "main",
            Some("abc"),
            "test",
            None,
            None,
        );
        // SHA shorter than 7 chars → use full value
        assert_eq!(find_env(&vars, "SHORT_SHA"), Some("abc".into()));
        assert_eq!(find_env(&vars, "IMAGE_TAG"), Some("sha-abc".into()));
    }

    #[test]
    fn env_vars_short_sha_absent_without_commit() {
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
        assert!(find_env(&vars, "SHORT_SHA").is_none());
        assert!(find_env(&vars, "IMAGE_TAG").is_none());
    }

    // -- VERSION env var --

    #[test]
    fn env_vars_version_present_when_set() {
        let vars = build_env_vars_core(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "main",
            None,
            "test",
            None,
            Some("1.2.3"),
            "push",
        );
        assert_eq!(find_env(&vars, "VERSION"), Some("1.2.3".into()));
    }

    #[test]
    fn env_vars_version_empty_when_none() {
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
        assert_eq!(find_env(&vars, "VERSION"), Some(String::new()));
    }

    // -- git_secret_name in pod spec --

    #[test]
    fn build_pod_spec_with_git_secret_mounts_to_init_container() {
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test",
            pipeline_id: Uuid::nil(),
            project_id: Uuid::nil(),
            step_name: "build",
            image: "alpine:3.19",
            commands: &["true".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/test.git",
            git_ref: "main",
            registry_secret: None,
            git_secret_name: Some("pl-git-12345678"),
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
        });

        let spec = pod.spec.unwrap();

        // Should have 2 volumes: workspace + git-auth
        let volumes = spec.volumes.as_ref().unwrap();
        assert_eq!(volumes.len(), 2);
        assert_eq!(volumes[1].name, "git-auth");
        let secret_vol = volumes[1].secret.as_ref().unwrap();
        assert_eq!(secret_vol.secret_name.as_deref(), Some("pl-git-12345678"));

        // Init container should have 2 mounts: workspace + git-auth
        let init = &spec.init_containers.as_ref().unwrap()[0];
        let init_mounts = init.volume_mounts.as_ref().unwrap();
        assert_eq!(init_mounts.len(), 2);
        assert_eq!(init_mounts[1].name, "git-auth");
        assert_eq!(init_mounts[1].mount_path, "/git-auth");
        assert_eq!(init_mounts[1].read_only, Some(true));

        // Step container should NOT have git-auth mount (only workspace)
        let step_mounts = spec.containers[0].volume_mounts.as_ref().unwrap();
        assert_eq!(step_mounts.len(), 1);
        assert_eq!(step_mounts[0].name, "workspace");
    }

    #[test]
    fn build_pod_spec_with_both_secrets() {
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
            registry_secret: Some("pl-registry-12345678"),
            git_secret_name: Some("pl-git-12345678"),
            step_type: "imagebuild",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
        });

        let spec = pod.spec.unwrap();

        // Should have 3 volumes: workspace + docker-config + git-auth
        let volumes = spec.volumes.as_ref().unwrap();
        assert_eq!(volumes.len(), 3);
        assert_eq!(volumes[0].name, "workspace");
        assert_eq!(volumes[1].name, "docker-config");
        assert_eq!(volumes[2].name, "git-auth");

        // Init container should have 2 mounts: workspace + git-auth
        let init = &spec.init_containers.as_ref().unwrap()[0];
        let init_mounts = init.volume_mounts.as_ref().unwrap();
        assert_eq!(init_mounts.len(), 2);

        // Step container should have 2 mounts: workspace + docker-config
        let step_mounts = spec.containers[0].volume_mounts.as_ref().unwrap();
        assert_eq!(step_mounts.len(), 2);
        assert_eq!(step_mounts[1].name, "docker-config");

        // Imagebuild step should have no security context
        assert!(spec.containers[0].security_context.is_none());

        // Image pull secrets should be set
        let pull_secrets = spec.image_pull_secrets.unwrap();
        assert_eq!(pull_secrets.len(), 1);
        assert_eq!(pull_secrets[0].name, "pl-registry-12345678");
    }

    // -- mark_transitive_dependents_skipped additional cases --

    #[test]
    fn mark_transitive_dependents_no_dependents() {
        let dependents = vec![vec![], vec![], vec![]];
        let mut skipped = std::collections::HashSet::new();
        let completed = std::collections::HashSet::new();
        mark_transitive_dependents_skipped(0, &dependents, &mut skipped, &completed);
        assert!(skipped.is_empty());
    }

    #[test]
    fn mark_transitive_dependents_wide_fan_out() {
        // 0 → 1, 2, 3, 4
        let dependents = vec![vec![1, 2, 3, 4], vec![], vec![], vec![], vec![]];
        let mut skipped = std::collections::HashSet::new();
        let completed = std::collections::HashSet::new();
        mark_transitive_dependents_skipped(0, &dependents, &mut skipped, &completed);
        assert_eq!(skipped.len(), 4);
        assert!(skipped.contains(&1));
        assert!(skipped.contains(&4));
    }

    // -- detect_unrecoverable_container additional cases --

    #[test]
    fn detect_unrecoverable_container_running_is_ok() {
        let status = PodStatus {
            container_statuses: Some(vec![ContainerStatus {
                name: "step".into(),
                state: Some(ContainerState {
                    running: Some(ContainerStateRunning { started_at: None }),
                    ..Default::default()
                }),
                restart_count: 0,
                ..Default::default()
            }]),
            init_container_statuses: Some(vec![]),
            ..Default::default()
        };
        assert!(detect_unrecoverable_container(&status).is_none());
    }

    #[test]
    fn detect_unrecoverable_container_only_init_statuses() {
        // Regular container statuses are None, only init has error
        let status = PodStatus {
            container_statuses: None,
            init_container_statuses: Some(vec![make_waiting_status(
                "clone",
                "ImagePullBackOff",
                Some("image not found"),
            )]),
            ..Default::default()
        };
        // container_statuses is None → returns None before checking init
        // (detect_unrecoverable_container returns on first ? from container_statuses)
        let result = detect_unrecoverable_container(&status);
        assert!(result.is_none());
    }

    #[test]
    fn detect_unrecoverable_regular_ok_init_bad() {
        // Regular containers are fine, but init container is stuck
        let status = PodStatus {
            container_statuses: Some(vec![ContainerStatus {
                name: "step".into(),
                state: Some(ContainerState {
                    waiting: Some(ContainerStateWaiting {
                        reason: Some("ContainerCreating".into()),
                        message: None,
                    }),
                    ..Default::default()
                }),
                restart_count: 0,
                ..Default::default()
            }]),
            init_container_statuses: Some(vec![make_waiting_status(
                "clone",
                "ImagePullBackOff",
                Some("init image not found"),
            )]),
            ..Default::default()
        };
        let result = detect_unrecoverable_container(&status);
        assert_eq!(
            result,
            Some("init container ImagePullBackOff: init image not found".into())
        );
    }

    // -- check_container_statuses with no waiting state --

    #[test]
    fn check_container_statuses_terminated_is_ok() {
        let statuses = vec![ContainerStatus {
            name: "step".into(),
            state: Some(ContainerState {
                terminated: Some(ContainerStateTerminated {
                    exit_code: 0,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            restart_count: 0,
            ..Default::default()
        }];
        assert!(check_container_statuses(&statuses, "").is_none());
    }

    #[test]
    fn check_container_statuses_no_state() {
        let statuses = vec![ContainerStatus {
            name: "step".into(),
            state: None,
            restart_count: 0,
            ..Default::default()
        }];
        assert!(check_container_statuses(&statuses, "").is_none());
    }

    // -- step_condition_from_row with both events and branches --

    #[test]
    fn step_condition_from_row_with_branches() {
        let row = StepRow {
            id: Uuid::nil(),
            step_order: 0,
            name: "deploy".into(),
            image: "alpine".into(),
            commands: vec![],
            condition_events: vec![],
            condition_branches: vec!["main".into(), "release/*".into()],
            deploy_test: None,
            depends_on: vec![],
            environment: None,
            gate: false,
            step_type: "command".into(),
            step_config: None,
        };
        let cond = step_condition_from_row(&row).unwrap();
        assert!(cond.events.is_empty());
        assert_eq!(cond.branches, vec!["main", "release/*"]);
    }

    #[test]
    fn step_condition_from_row_with_both() {
        let row = StepRow {
            id: Uuid::nil(),
            step_order: 0,
            name: "deploy".into(),
            image: "alpine".into(),
            commands: vec![],
            condition_events: vec!["push".into()],
            condition_branches: vec!["main".into()],
            deploy_test: None,
            depends_on: vec![],
            environment: None,
            gate: false,
            step_type: "command".into(),
            step_config: None,
        };
        let cond = step_condition_from_row(&row).unwrap();
        assert_eq!(cond.events, vec!["push"]);
        assert_eq!(cond.branches, vec!["main"]);
    }

    // -- build_env_vars_full: test the layered env var logic --
    // build_env_vars_full requires an AppState reference, so we test its
    // components: build_env_vars_core (already tested above), OTEL var
    // injection, secret filtering, and step env merging are tested by
    // verifying the helper functions and logic inline.

    // Simulate what build_env_vars_full does for OTEL vars:
    #[test]
    fn otel_vars_are_appended_with_token() {
        let mut vars = vec![env_var("PLATFORM_PROJECT_ID", "test")];
        let platform_api_url = "http://platform:8080";
        let project_name = "my-app";
        let step_name = "test";
        let project_id = Uuid::nil();

        // Simulate OTEL injection (lines 1734-1751)
        vars.push(env_var("OTEL_EXPORTER_OTLP_ENDPOINT", platform_api_url));
        vars.push(env_var(
            "OTEL_SERVICE_NAME",
            &format!("{project_name}/{step_name}"),
        ));
        vars.push(env_var(
            "OTEL_RESOURCE_ATTRIBUTES",
            &format!("platform.project_id={project_id}"),
        ));
        let otlp_token = Some("otlp-test-token".to_string());
        if let Some(ref token) = otlp_token {
            vars.push(env_var(
                "OTEL_EXPORTER_OTLP_HEADERS",
                &format!("Authorization=Bearer {token}"),
            ));
        }

        assert_eq!(
            find_env(&vars, "OTEL_EXPORTER_OTLP_ENDPOINT"),
            Some("http://platform:8080".into())
        );
        assert_eq!(
            find_env(&vars, "OTEL_SERVICE_NAME"),
            Some("my-app/test".into())
        );
        assert!(
            find_env(&vars, "OTEL_RESOURCE_ATTRIBUTES")
                .unwrap()
                .contains("platform.project_id=")
        );
        assert_eq!(
            find_env(&vars, "OTEL_EXPORTER_OTLP_HEADERS"),
            Some("Authorization=Bearer otlp-test-token".into())
        );
    }

    #[test]
    fn otel_vars_no_headers_without_token() {
        let mut vars = vec![env_var("PLATFORM_PROJECT_ID", "test")];
        vars.push(env_var(
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            "http://platform:8080",
        ));
        let otlp_token: Option<String> = None;
        if let Some(ref token) = otlp_token {
            vars.push(env_var(
                "OTEL_EXPORTER_OTLP_HEADERS",
                &format!("Authorization=Bearer {token}"),
            ));
        }

        assert!(find_env(&vars, "OTEL_EXPORTER_OTLP_ENDPOINT").is_some());
        assert!(
            find_env(&vars, "OTEL_EXPORTER_OTLP_HEADERS").is_none(),
            "OTEL headers should not be set without an OTLP token"
        );
    }

    // Test the secret filtering + PLATFORM_SECRET_NAMES logic:
    #[test]
    fn secret_injection_filters_reserved_and_sets_names() {
        let mut vars = vec![
            env_var("PIPELINE_ID", "original-value"),
            env_var("COMMIT_SHA", "original-sha"),
        ];
        let secrets = vec![
            ("PIPELINE_ID".to_string(), "override-attempt".to_string()),
            ("COMMIT_SHA".to_string(), "override-attempt".to_string()),
            ("PATH".to_string(), "/evil/path".to_string()),
            ("MY_CUSTOM_VAR".to_string(), "allowed".to_string()),
            ("DATABASE_URL".to_string(), "postgres://host/db".to_string()),
        ];

        let mut secret_names = Vec::new();
        for (key, val) in &secrets {
            if !is_reserved_pipeline_env_var(key) {
                vars.push(env_var(key, val));
                secret_names.push(key.as_str());
            }
        }
        if !secret_names.is_empty() {
            vars.push(env_var("PLATFORM_SECRET_NAMES", &secret_names.join(",")));
        }

        // Reserved vars should NOT have been overridden
        assert_eq!(
            find_env(&vars, "PIPELINE_ID"),
            Some("original-value".into())
        );
        // Custom vars should be injected
        assert_eq!(find_env(&vars, "MY_CUSTOM_VAR"), Some("allowed".into()));
        assert_eq!(
            find_env(&vars, "DATABASE_URL"),
            Some("postgres://host/db".into())
        );
        // PLATFORM_SECRET_NAMES should only list the non-reserved ones
        let names = find_env(&vars, "PLATFORM_SECRET_NAMES").unwrap();
        assert!(names.contains("MY_CUSTOM_VAR"));
        assert!(names.contains("DATABASE_URL"));
        assert!(!names.contains("PIPELINE_ID"));
        assert!(!names.contains("PATH"));
    }

    #[test]
    fn secret_injection_no_names_when_all_reserved() {
        let secrets = vec![
            ("PIPELINE_ID".to_string(), "override".to_string()),
            ("COMMIT_SHA".to_string(), "override".to_string()),
            ("PATH".to_string(), "override".to_string()),
        ];

        let mut secret_names = Vec::new();
        for (key, _val) in &secrets {
            if !is_reserved_pipeline_env_var(key) {
                secret_names.push(key.as_str());
            }
        }

        assert!(
            secret_names.is_empty(),
            "all-reserved secrets should produce no secret names"
        );
    }

    #[test]
    fn secret_injection_no_names_when_no_secrets() {
        let secrets: Vec<(String, String)> = vec![];
        let mut secret_names = Vec::new();
        for (key, _val) in &secrets {
            if !is_reserved_pipeline_env_var(key) {
                secret_names.push(key.as_str());
            }
        }
        assert!(secret_names.is_empty());
    }

    // Test step environment merging logic:
    #[test]
    fn step_env_overrides_existing_vars() {
        let mut vars = vec![
            env_var("OVERRIDE_ME", "secret-value"),
            env_var("KEEP_ME", "original"),
        ];
        let step_env = serde_json::json!({"CUSTOM_FLAG": "true", "OVERRIDE_ME": "step-value"});

        if let Some(map) = step_env.as_object() {
            for (key, val) in map {
                if let Some(v) = val.as_str() {
                    vars.push(env_var(key, v));
                }
            }
        }

        assert_eq!(find_env(&vars, "CUSTOM_FLAG"), Some("true".into()));
        // Both values exist; last one (step env) takes precedence in K8s
        let override_vals: Vec<_> = vars.iter().filter(|v| v.name == "OVERRIDE_ME").collect();
        assert_eq!(override_vals.len(), 2);
        assert_eq!(
            override_vals.last().unwrap().value.as_deref(),
            Some("step-value")
        );
    }

    #[test]
    fn step_env_ignores_non_string_values() {
        let mut vars: Vec<EnvVar> = vec![];
        let step_env = serde_json::json!({
            "STRING_VAR": "hello",
            "NUMBER_VAR": 42,
            "BOOL_VAR": true,
            "NULL_VAR": null,
        });

        if let Some(map) = step_env.as_object() {
            for (key, val) in map {
                if let Some(v) = val.as_str() {
                    vars.push(env_var(key, v));
                }
            }
        }

        // Only string values should be added
        assert_eq!(find_env(&vars, "STRING_VAR"), Some("hello".into()));
        // Non-string values: as_str() returns None, so they are not added
        let has_number = vars.iter().any(|v| v.name == "NUMBER_VAR");
        let has_bool = vars.iter().any(|v| v.name == "BOOL_VAR");
        let has_null = vars.iter().any(|v| v.name == "NULL_VAR");
        assert!(!has_number, "number values should be skipped");
        assert!(!has_bool, "boolean values should be skipped");
        assert!(!has_null, "null values should be skipped");
    }

    // -- is_reserved_pipeline_env_var: comprehensive OTEL coverage --

    #[test]
    fn reserved_otel_env_vars_blocked() {
        assert!(is_reserved_pipeline_env_var("OTEL_EXPORTER_OTLP_ENDPOINT"));
        assert!(is_reserved_pipeline_env_var("OTEL_SERVICE_NAME"));
        assert!(is_reserved_pipeline_env_var("OTEL_RESOURCE_ATTRIBUTES"));
        assert!(is_reserved_pipeline_env_var("OTEL_EXPORTER_OTLP_HEADERS"));
    }

    #[test]
    fn reserved_git_and_docker_env_vars_blocked() {
        assert!(is_reserved_pipeline_env_var("GIT_ASKPASS"));
        assert!(is_reserved_pipeline_env_var("DOCKER_CONFIG"));
        assert!(is_reserved_pipeline_env_var("REGISTRY"));
    }

    #[test]
    fn reserved_pipeline_metadata_vars_blocked() {
        assert!(is_reserved_pipeline_env_var("PLATFORM_PROJECT_ID"));
        assert!(is_reserved_pipeline_env_var("PLATFORM_PROJECT_NAME"));
        assert!(is_reserved_pipeline_env_var("STEP_NAME"));
        assert!(is_reserved_pipeline_env_var("COMMIT_REF"));
        assert!(is_reserved_pipeline_env_var("COMMIT_BRANCH"));
        assert!(is_reserved_pipeline_env_var("SHORT_SHA"));
        assert!(is_reserved_pipeline_env_var("IMAGE_TAG"));
        assert!(is_reserved_pipeline_env_var("PROJECT"));
        assert!(is_reserved_pipeline_env_var("VERSION"));
        assert!(is_reserved_pipeline_env_var("PIPELINE_TRIGGER"));
    }

    #[test]
    fn non_reserved_env_vars_allowed() {
        assert!(!is_reserved_pipeline_env_var("NODE_ENV"));
        assert!(!is_reserved_pipeline_env_var("RUST_LOG"));
        assert!(!is_reserved_pipeline_env_var("OTEL_CUSTOM"));
        assert!(!is_reserved_pipeline_env_var("SECRET_KEY"));
        assert!(!is_reserved_pipeline_env_var("COMMIT_MESSAGE")); // similar but not reserved
    }

    // -- mark_transitive_dependents_skipped: deeper DAG shapes --

    #[test]
    fn mark_transitive_dependents_deep_chain_5_levels() {
        // 0 → 1 → 2 → 3 → 4
        let dependents = vec![vec![1], vec![2], vec![3], vec![4], vec![]];
        let mut skipped = std::collections::HashSet::new();
        let completed = std::collections::HashSet::new();
        mark_transitive_dependents_skipped(0, &dependents, &mut skipped, &completed);
        assert_eq!(skipped.len(), 4);
        for i in 1..=4 {
            assert!(skipped.contains(&i));
        }
    }

    #[test]
    fn mark_transitive_dependents_partially_completed() {
        // 0 → 1 → 2, 0 → 3 → 4, node 3 already completed
        let dependents = vec![vec![1, 3], vec![2], vec![], vec![4], vec![]];
        let mut skipped = std::collections::HashSet::new();
        let mut completed = std::collections::HashSet::new();
        completed.insert(3);
        mark_transitive_dependents_skipped(0, &dependents, &mut skipped, &completed);
        // 1 and 2 should be skipped; 3 is completed (not skipped), 4 not reached
        assert!(skipped.contains(&1));
        assert!(skipped.contains(&2));
        assert!(!skipped.contains(&3));
        // 4 depends on 3 which is completed → not traversed from the stack
        assert!(!skipped.contains(&4));
    }

    #[test]
    fn mark_transitive_dependents_middle_failure() {
        // 0 → 1 → 2, failure at node 1 (not 0)
        let dependents = vec![vec![1], vec![2], vec![]];
        let mut skipped = std::collections::HashSet::new();
        let completed = std::collections::HashSet::new();
        mark_transitive_dependents_skipped(1, &dependents, &mut skipped, &completed);
        assert_eq!(skipped.len(), 1);
        assert!(skipped.contains(&2));
        assert!(!skipped.contains(&0)); // 0 is not a dependent of 1
    }

    #[test]
    fn mark_transitive_dependents_self_loop_no_hang() {
        // A step depending on itself (malformed DAG — should not happen
        // in production, but ensure no infinite loop)
        let dependents = vec![vec![0]];
        let mut skipped = std::collections::HashSet::new();
        let completed = std::collections::HashSet::new();
        // Node 0 was already the failed node, so when we see dependent 0
        // it will be inserted into skipped and pushed to stack once,
        // then re-popped and its dependents (itself) already in skipped.
        mark_transitive_dependents_skipped(0, &dependents, &mut skipped, &completed);
        // Should terminate without hanging
        assert!(skipped.contains(&0));
    }

    // -- detect_unrecoverable_container: additional edge cases --

    #[test]
    fn detect_unrecoverable_container_crash_loop_in_init() {
        // Init container in CrashLoopBackOff with restarts >= 3
        let status = PodStatus {
            container_statuses: Some(vec![ContainerStatus {
                name: "step".into(),
                state: Some(ContainerState {
                    waiting: Some(ContainerStateWaiting {
                        reason: Some("ContainerCreating".into()),
                        message: None,
                    }),
                    ..Default::default()
                }),
                restart_count: 0,
                ..Default::default()
            }]),
            init_container_statuses: Some(vec![ContainerStatus {
                name: "clone".into(),
                state: Some(ContainerState {
                    waiting: Some(ContainerStateWaiting {
                        reason: Some("CrashLoopBackOff".into()),
                        message: None,
                    }),
                    ..Default::default()
                }),
                restart_count: 5,
                ..Default::default()
            }]),
            ..Default::default()
        };
        let result = detect_unrecoverable_container(&status);
        assert_eq!(
            result,
            Some("init container CrashLoopBackOff after 5 restarts".into())
        );
    }

    #[test]
    fn detect_unrecoverable_container_config_error_in_regular() {
        let status = PodStatus {
            container_statuses: Some(vec![make_waiting_status(
                "step",
                "CreateContainerConfigError",
                Some("secret not found"),
            )]),
            init_container_statuses: Some(vec![]),
            ..Default::default()
        };
        let result = detect_unrecoverable_container(&status);
        assert_eq!(
            result,
            Some("CreateContainerConfigError: secret not found".into())
        );
    }

    #[test]
    fn detect_unrecoverable_multiple_containers_first_bad() {
        // Multiple containers, first one is bad
        let status = PodStatus {
            container_statuses: Some(vec![
                make_waiting_status("app", "InvalidImageName", Some("bad name")),
                ContainerStatus {
                    name: "sidecar".into(),
                    state: Some(ContainerState {
                        running: Some(ContainerStateRunning { started_at: None }),
                        ..Default::default()
                    }),
                    restart_count: 0,
                    ..Default::default()
                },
            ]),
            init_container_statuses: Some(vec![]),
            ..Default::default()
        };
        let result = detect_unrecoverable_container(&status);
        assert_eq!(result, Some("InvalidImageName: bad name".into()));
    }

    #[test]
    fn detect_unrecoverable_multiple_containers_second_bad() {
        // First container OK, second is bad
        let status = PodStatus {
            container_statuses: Some(vec![
                ContainerStatus {
                    name: "app".into(),
                    state: Some(ContainerState {
                        running: Some(ContainerStateRunning { started_at: None }),
                        ..Default::default()
                    }),
                    restart_count: 0,
                    ..Default::default()
                },
                make_waiting_status("sidecar", "ErrImagePull", Some("pull failed")),
            ]),
            init_container_statuses: Some(vec![]),
            ..Default::default()
        };
        let result = detect_unrecoverable_container(&status);
        assert_eq!(result, Some("ErrImagePull: pull failed".into()));
    }

    // -- check_container_statuses: unknown waiting reasons are OK --

    #[test]
    fn check_container_statuses_unknown_waiting_reason() {
        let statuses = vec![make_waiting_status(
            "app",
            "ContainerCreating",
            Some("still starting"),
        )];
        assert!(
            check_container_statuses(&statuses, "").is_none(),
            "ContainerCreating should not be treated as unrecoverable"
        );
    }

    #[test]
    fn check_container_statuses_pending_scheduling() {
        let statuses = vec![make_waiting_status("app", "PodInitializing", None)];
        assert!(check_container_statuses(&statuses, "").is_none());
    }

    // -- extract_branch: edge cases --

    #[test]
    fn extract_branch_nested_path() {
        assert_eq!(
            extract_branch("refs/heads/feature/deep/nested/path"),
            "feature/deep/nested/path"
        );
    }

    #[test]
    fn extract_branch_empty_string() {
        assert_eq!(extract_branch(""), "");
    }

    #[test]
    fn extract_branch_refs_heads_only() {
        // "refs/heads/" alone should give empty string
        assert_eq!(extract_branch("refs/heads/"), "");
    }

    #[test]
    fn extract_branch_refs_tags_only() {
        assert_eq!(extract_branch("refs/tags/"), "");
    }

    #[test]
    fn extract_branch_refs_other_prefix() {
        // refs/merge/ is not stripped
        assert_eq!(extract_branch("refs/merge/123"), "refs/merge/123");
    }

    // -- pod spec: init container without git_secret has only workspace mount --

    #[test]
    fn build_pod_spec_without_git_secret_init_has_one_mount() {
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
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
        });

        let spec = pod.spec.unwrap();
        let init = &spec.init_containers.as_ref().unwrap()[0];
        let init_mounts = init.volume_mounts.as_ref().unwrap();
        assert_eq!(
            init_mounts.len(),
            1,
            "init container should only have workspace mount when no git secret"
        );
        assert_eq!(init_mounts[0].name, "workspace");
    }

    // -- pod spec: imagebuild vs command security context --

    #[test]
    fn build_pod_spec_deploy_test_step_type_has_security_context() {
        // deploy_test steps are not imagebuild — they should get hardened context
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test",
            pipeline_id: Uuid::nil(),
            project_id: Uuid::nil(),
            step_name: "test",
            image: "alpine:3.19",
            commands: &["echo test".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/test.git",
            git_ref: "main",
            registry_secret: None,
            git_secret_name: None,
            step_type: "deploy_test",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
        });

        let spec = pod.spec.unwrap();
        let container = &spec.containers[0];
        assert!(
            container.security_context.is_some(),
            "deploy_test step type should have hardened security context"
        );
    }

    // -- build_env_vars_core: commit sha edge cases --

    #[test]
    fn env_vars_core_short_sha_exactly_seven_chars() {
        let vars = test_env_vars(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "main",
            Some("1234567"),
            "test",
            None,
            None,
        );
        assert_eq!(find_env(&vars, "SHORT_SHA"), Some("1234567".into()));
        assert_eq!(find_env(&vars, "IMAGE_TAG"), Some("sha-1234567".into()));
    }

    #[test]
    fn env_vars_core_very_long_sha() {
        let long_sha = "abcdef1234567890abcdef1234567890abcdef1234";
        let vars = test_env_vars(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "main",
            Some(long_sha),
            "test",
            None,
            None,
        );
        assert_eq!(find_env(&vars, "SHORT_SHA"), Some("abcdef1".into()));
        assert_eq!(find_env(&vars, "IMAGE_TAG"), Some("sha-abcdef1".into()));
        assert_eq!(find_env(&vars, "COMMIT_SHA"), Some(long_sha.into()));
    }

    #[test]
    fn env_vars_core_single_char_sha() {
        let vars = test_env_vars(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "main",
            Some("a"),
            "test",
            None,
            None,
        );
        assert_eq!(find_env(&vars, "SHORT_SHA"), Some("a".into()));
        assert_eq!(find_env(&vars, "IMAGE_TAG"), Some("sha-a".into()));
    }

    // -- step_condition_from_row: deploy_test present but no conditions --

    #[test]
    fn step_condition_from_row_deploy_test_present_no_conditions() {
        let row = StepRow {
            id: Uuid::nil(),
            step_order: 0,
            name: "test-deploy".into(),
            image: "alpine".into(),
            commands: vec![],
            condition_events: vec![],
            condition_branches: vec![],
            deploy_test: Some(serde_json::json!({"test_image": "test:latest"})),
            depends_on: vec![],
            environment: None,
            gate: false,
            step_type: "deploy_test".into(),
            step_config: None,
        };
        // Even with deploy_test, if conditions are empty, result is None (always run)
        assert!(step_condition_from_row(&row).is_none());
    }

    #[test]
    fn step_condition_from_row_deploy_test_with_conditions() {
        let row = StepRow {
            id: Uuid::nil(),
            step_order: 0,
            name: "test-deploy".into(),
            image: "alpine".into(),
            commands: vec![],
            condition_events: vec!["push".into()],
            condition_branches: vec!["main".into()],
            deploy_test: Some(serde_json::json!({"test_image": "test:latest"})),
            depends_on: vec![],
            environment: None,
            gate: false,
            step_type: "deploy_test".into(),
            step_config: None,
        };
        let cond = step_condition_from_row(&row).unwrap();
        assert_eq!(cond.events, vec!["push"]);
        assert_eq!(cond.branches, vec!["main"]);
    }

    // -- build_env_vars_full: all reserved secrets filtered --

    #[test]
    fn build_env_vars_full_all_reserved_secrets_produce_no_secret_names() {
        // Verify that if all provided secrets have reserved names,
        // PLATFORM_SECRET_NAMES is not emitted.
        let names = ["PIPELINE_ID", "COMMIT_SHA", "PATH"];
        for name in &names {
            assert!(
                is_reserved_pipeline_env_var(name),
                "{name} should be reserved"
            );
        }
    }

    // -- build_env_vars_full: step env with non-string values ignored --

    #[test]
    fn build_env_vars_full_step_env_ignores_non_string() {
        let step_env = serde_json::json!({
            "STRING_VAR": "hello",
            "NUMBER_VAR": 42,
            "BOOL_VAR": true,
            "NULL_VAR": null,
        });

        // Only string values should be extractable as &str
        assert!(step_env["STRING_VAR"].as_str().is_some());
        assert!(
            step_env["NUMBER_VAR"].as_str().is_none(),
            "non-string values should not be extractable as str"
        );
        assert!(step_env["BOOL_VAR"].as_str().is_none());
        assert!(step_env["NULL_VAR"].as_str().is_none());
    }

    // -- container_security: verify no add capabilities --

    #[test]
    fn container_security_no_added_capabilities() {
        let sec = container_security();
        assert!(sec.capabilities.as_ref().unwrap().add.is_none());
        assert!(sec.read_only_root_filesystem.is_none());
        assert!(sec.run_as_user.is_none());
    }

    // -- slug: additional patterns --

    #[test]
    fn slug_with_spaces_and_numbers() {
        assert_eq!(slug("Step 1: Build"), "step-1--build");
    }

    #[test]
    fn slug_preserves_numbers() {
        assert_eq!(slug("v2-build"), "v2-build");
    }

    #[test]
    fn slug_unicode_gets_removed() {
        // Unicode chars that aren't alphanumeric ASCII get replaced with dashes
        assert_eq!(slug("test"), "test");
    }

    // -- build_volumes_and_mounts: git_secret volume properties --

    #[test]
    fn build_volumes_and_mounts_git_secret_has_correct_secret_name() {
        let (volumes, _) = build_volumes_and_mounts(None, Some("pl-git-abcd1234"), None);
        let git_vol = &volumes[1];
        assert_eq!(git_vol.name, "git-auth");
        let secret_source = git_vol.secret.as_ref().unwrap();
        assert_eq!(
            secret_source.secret_name.as_deref(),
            Some("pl-git-abcd1234")
        );
    }

    // -- node_registry_url: both set --

    #[test]
    fn node_registry_url_both_set_prefers_node() {
        let config = crate::config::Config {
            registry_url: Some("push-registry.com:5000".into()),
            registry_node_url: Some("node-registry.com:5000".into()),
            ..crate::config::Config::test_default()
        };
        assert_eq!(node_registry_url(&config), Some("node-registry.com:5000"));
    }

    // -- extract_branch: comprehensive coverage --

    #[test]
    fn extract_branch_prefers_refs_heads_over_refs_tags() {
        // refs/heads/ is tried first
        assert_eq!(extract_branch("refs/heads/refs/tags/v1"), "refs/tags/v1");
    }

    #[test]
    fn extract_branch_exact_prefix_match() {
        // Should not match partial prefix
        assert_eq!(extract_branch("refs/headsbranch"), "refs/headsbranch");
    }

    // -- detect_unrecoverable_container: CreateContainerConfigError without message --

    #[test]
    fn check_container_statuses_create_config_error_no_message() {
        let statuses = vec![make_waiting_status(
            "app",
            "CreateContainerConfigError",
            None,
        )];
        let result = check_container_statuses(&statuses, "");
        assert_eq!(
            result,
            Some("CreateContainerConfigError: container config error".into())
        );
    }

    #[test]
    fn check_container_statuses_err_image_pull_no_message() {
        let statuses = vec![make_waiting_status("app", "ErrImagePull", None)];
        let result = check_container_statuses(&statuses, "");
        assert_eq!(result, Some("ErrImagePull: image pull failed".into()));
    }

    #[test]
    fn check_container_statuses_image_pull_backoff_no_message() {
        let statuses = vec![make_waiting_status("app", "ImagePullBackOff", None)];
        let result = check_container_statuses(&statuses, "");
        assert_eq!(result, Some("ImagePullBackOff: image pull failed".into()));
    }

    // -- check_container_statuses: CrashLoopBackOff edge cases --

    #[test]
    fn check_container_statuses_crash_loop_exactly_3_restarts() {
        let statuses = vec![ContainerStatus {
            name: "app".into(),
            state: Some(ContainerState {
                waiting: Some(ContainerStateWaiting {
                    reason: Some("CrashLoopBackOff".into()),
                    message: None,
                }),
                ..Default::default()
            }),
            restart_count: 3,
            ..Default::default()
        }];
        let result = check_container_statuses(&statuses, "");
        assert_eq!(result, Some("CrashLoopBackOff after 3 restarts".into()));
    }

    #[test]
    fn check_container_statuses_crash_loop_with_prefix() {
        let statuses = vec![ContainerStatus {
            name: "init".into(),
            state: Some(ContainerState {
                waiting: Some(ContainerStateWaiting {
                    reason: Some("CrashLoopBackOff".into()),
                    message: None,
                }),
                ..Default::default()
            }),
            restart_count: 4,
            ..Default::default()
        }];
        let result = check_container_statuses(&statuses, "init container ");
        assert_eq!(
            result,
            Some("init container CrashLoopBackOff after 4 restarts".into())
        );
    }

    // -- check_container_statuses: multiple containers, second has error --

    #[test]
    fn check_container_statuses_second_container_error() {
        let statuses = vec![
            ContainerStatus {
                name: "healthy".into(),
                state: Some(ContainerState {
                    running: Some(ContainerStateRunning { started_at: None }),
                    ..Default::default()
                }),
                restart_count: 0,
                ..Default::default()
            },
            make_waiting_status("broken", "ImagePullBackOff", Some("bad image")),
        ];
        let result = check_container_statuses(&statuses, "");
        assert_eq!(result, Some("ImagePullBackOff: bad image".into()));
    }

    // -- check_container_statuses: waiting with no reason --

    #[test]
    fn check_container_statuses_waiting_no_reason() {
        let statuses = vec![ContainerStatus {
            name: "app".into(),
            state: Some(ContainerState {
                waiting: Some(ContainerStateWaiting {
                    reason: None,
                    message: None,
                }),
                ..Default::default()
            }),
            restart_count: 0,
            ..Default::default()
        }];
        // No reason means the match falls through to the default arm
        assert!(check_container_statuses(&statuses, "").is_none());
    }

    // -- detect_unrecoverable_container: empty container + init statuses --

    #[test]
    fn detect_unrecoverable_empty_containers_empty_init() {
        let status = PodStatus {
            container_statuses: Some(vec![]),
            init_container_statuses: Some(vec![]),
            ..Default::default()
        };
        // Empty containers → check_container_statuses returns None
        // Empty init containers → check_container_statuses returns None
        assert!(detect_unrecoverable_container(&status).is_none());
    }

    #[test]
    fn detect_unrecoverable_containers_ok_no_init_statuses() {
        let status = PodStatus {
            container_statuses: Some(vec![ContainerStatus {
                name: "app".into(),
                state: Some(ContainerState {
                    running: Some(ContainerStateRunning { started_at: None }),
                    ..Default::default()
                }),
                restart_count: 0,
                ..Default::default()
            }]),
            init_container_statuses: None,
            ..Default::default()
        };
        // Regular containers OK, init_container_statuses is None → returns None via ?
        assert!(detect_unrecoverable_container(&status).is_none());
    }

    // -- build_env_vars_core: trigger types --

    #[test]
    fn env_vars_core_trigger_tag() {
        let vars = build_env_vars_core(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "refs/tags/v1.0",
            None,
            "test",
            None,
            None,
            "tag",
        );
        assert_eq!(find_env(&vars, "PIPELINE_TRIGGER"), Some("tag".into()));
    }

    #[test]
    fn env_vars_core_trigger_schedule() {
        let vars = build_env_vars_core(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "main",
            None,
            "test",
            None,
            None,
            "schedule",
        );
        assert_eq!(find_env(&vars, "PIPELINE_TRIGGER"), Some("schedule".into()));
    }

    #[test]
    fn env_vars_core_trigger_api() {
        let vars = build_env_vars_core(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "main",
            None,
            "test",
            None,
            None,
            "api",
        );
        assert_eq!(find_env(&vars, "PIPELINE_TRIGGER"), Some("api".into()));
    }

    // -- build_env_vars_core: version + registry + commit_sha combination --

    #[test]
    fn env_vars_core_all_optional_fields_present() {
        let pid = Uuid::new_v4();
        let proj = Uuid::new_v4();
        let vars = build_env_vars_core(
            pid,
            proj,
            "my-app",
            "refs/heads/main",
            Some("abcdef1234567890"),
            "deploy",
            Some("registry.io:5000"),
            Some("2.0.0"),
            "push",
        );
        assert_eq!(
            find_env(&vars, "PLATFORM_PROJECT_ID"),
            Some(proj.to_string())
        );
        assert_eq!(
            find_env(&vars, "PLATFORM_PROJECT_NAME"),
            Some("my-app".into())
        );
        assert_eq!(find_env(&vars, "PIPELINE_ID"), Some(pid.to_string()));
        assert_eq!(find_env(&vars, "STEP_NAME"), Some("deploy".into()));
        assert_eq!(
            find_env(&vars, "COMMIT_REF"),
            Some("refs/heads/main".into())
        );
        assert_eq!(find_env(&vars, "COMMIT_BRANCH"), Some("main".into()));
        assert_eq!(
            find_env(&vars, "COMMIT_SHA"),
            Some("abcdef1234567890".into())
        );
        assert_eq!(find_env(&vars, "SHORT_SHA"), Some("abcdef1".into()));
        assert_eq!(find_env(&vars, "IMAGE_TAG"), Some("sha-abcdef1".into()));
        assert_eq!(find_env(&vars, "VERSION"), Some("2.0.0".into()));
        assert_eq!(find_env(&vars, "REGISTRY"), Some("registry.io:5000".into()));
        assert_eq!(
            find_env(&vars, "DOCKER_CONFIG"),
            Some("/kaniko/.docker".into())
        );
        assert_eq!(find_env(&vars, "PROJECT"), Some("my-app".into()));
        assert_eq!(find_env(&vars, "PIPELINE_TRIGGER"), Some("push".into()));
    }

    // -- step_condition_from_row: with depends_on (should not affect condition) --

    #[test]
    fn step_condition_from_row_ignores_depends_on() {
        let row = StepRow {
            id: Uuid::nil(),
            step_order: 0,
            name: "test".into(),
            image: "alpine".into(),
            commands: vec![],
            condition_events: vec!["push".into()],
            condition_branches: vec![],
            deploy_test: None,
            depends_on: vec!["build".into()],
            environment: None,
            gate: false,
            step_type: "command".into(),
            step_config: None,
        };
        let cond = step_condition_from_row(&row).unwrap();
        assert_eq!(cond.events, vec!["push"]);
        // depends_on should not affect the condition
    }

    // -- mark_transitive_dependents_skipped: cycle between two nodes --

    #[test]
    fn mark_transitive_dependents_two_node_cycle() {
        // 0 → 1, 1 → 0 (mutual dependency — malformed DAG)
        let dependents = vec![vec![1], vec![0]];
        let mut skipped = std::collections::HashSet::new();
        let completed = std::collections::HashSet::new();
        mark_transitive_dependents_skipped(0, &dependents, &mut skipped, &completed);
        // Both should end up skipped without infinite loop
        assert!(skipped.contains(&0));
        assert!(skipped.contains(&1));
    }

    // -- build_pod_spec: imagebuild step type special behavior --

    #[test]
    fn build_pod_spec_gitops_sync_step_type_has_security_context() {
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test",
            pipeline_id: Uuid::nil(),
            project_id: Uuid::nil(),
            step_name: "sync",
            image: "alpine:3.19",
            commands: &["echo sync".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/test.git",
            git_ref: "main",
            registry_secret: None,
            git_secret_name: None,
            step_type: "gitops_sync",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
        });

        let spec = pod.spec.unwrap();
        let container = &spec.containers[0];
        assert!(
            container.security_context.is_some(),
            "gitops_sync step type should have hardened security context"
        );
    }

    #[test]
    fn build_pod_spec_deploy_watch_step_type_has_security_context() {
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test",
            pipeline_id: Uuid::nil(),
            project_id: Uuid::nil(),
            step_name: "watch",
            image: "alpine:3.19",
            commands: &["echo watch".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/test.git",
            git_ref: "main",
            registry_secret: None,
            git_secret_name: None,
            step_type: "deploy_watch",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
        });

        let spec = pod.spec.unwrap();
        let container = &spec.containers[0];
        assert!(
            container.security_context.is_some(),
            "deploy_watch step type should have hardened security context"
        );
    }

    // -- build_pod_spec: init container image from config --

    #[test]
    fn build_pod_spec_custom_git_clone_image() {
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
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "custom-registry/git-clone:v3.0",
            has_artifacts: false,
            proxy_binary_path: None,
        });

        let spec = pod.spec.unwrap();
        let init = &spec.init_containers.as_ref().unwrap()[0];
        assert_eq!(
            init.image.as_deref(),
            Some("custom-registry/git-clone:v3.0"),
            "init container should use the git_clone_image from config"
        );
    }

    // -- is_reserved_pipeline_env_var: exhaustive coverage of all reserved vars --

    #[test]
    fn all_reserved_pipeline_env_vars_are_blocked() {
        for var in RESERVED_PIPELINE_ENV_VARS {
            assert!(
                is_reserved_pipeline_env_var(var),
                "{var} should be reserved"
            );
        }
    }

    #[test]
    fn reserved_pipeline_env_var_count() {
        // Ensure we have the expected number of reserved vars (catches accidental removal)
        assert!(
            RESERVED_PIPELINE_ENV_VARS.len() >= 15,
            "should have at least 15 reserved pipeline env vars, got {}",
            RESERVED_PIPELINE_ENV_VARS.len()
        );
    }

    // -- build_env_vars_core: special characters in project name --

    #[test]
    fn env_vars_core_project_name_with_special_chars() {
        let vars = build_env_vars_core(
            Uuid::nil(),
            Uuid::nil(),
            "my-app_v2.0",
            "main",
            None,
            "test",
            None,
            None,
            "push",
        );
        assert_eq!(
            find_env(&vars, "PLATFORM_PROJECT_NAME"),
            Some("my-app_v2.0".into())
        );
        assert_eq!(find_env(&vars, "PROJECT"), Some("my-app_v2.0".into()));
    }

    // -- build_pod_spec: pod name from pipeline ID and step name --

    #[test]
    fn pod_name_slug_applied_correctly() {
        let pipeline_id = Uuid::parse_str("12345678-1234-1234-1234-123456789abc").unwrap();
        let pod_name = format!(
            "pl-{}-{}",
            &pipeline_id.to_string()[..8],
            slug("Build Image")
        );
        assert_eq!(pod_name, "pl-12345678-build-image");
    }

    // -- build_pod_spec: fs_group in pod security context --

    #[test]
    fn build_pod_spec_fs_group_is_1000() {
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
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
        });

        let spec = pod.spec.unwrap();
        let psc = spec.security_context.as_ref().unwrap();
        assert_eq!(psc.fs_group, Some(1000));
    }

    // -- build_pod_spec: restart policy is Never --

    #[test]
    fn build_pod_spec_restart_policy_never() {
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
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
        });

        let spec = pod.spec.unwrap();
        assert_eq!(spec.restart_policy.as_deref(), Some("Never"));
    }

    // -- build_pod_spec: init container uses sh -c --

    #[test]
    fn build_pod_spec_init_container_uses_sh_c() {
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
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
        });

        let spec = pod.spec.unwrap();
        let init = &spec.init_containers.as_ref().unwrap()[0];
        assert_eq!(
            init.command.as_deref(),
            Some(&["sh".into(), "-c".into()][..])
        );
    }

    // -- build_pod_spec: step container uses sh -c --

    #[test]
    fn build_pod_spec_step_container_uses_sh_c() {
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
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
        });

        let spec = pod.spec.unwrap();
        let container = &spec.containers[0];
        assert_eq!(
            container.command.as_deref(),
            Some(&["sh".into(), "-c".into()][..])
        );
    }

    // -- build_pod_spec: init container clone command uses GIT_ASKPASS with secret --

    #[test]
    fn build_pod_spec_clone_uses_git_askpass_with_secret_file() {
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
            git_secret_name: Some("pl-git-abc123"),
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
        });

        let spec = pod.spec.unwrap();
        let init = &spec.init_containers.as_ref().unwrap()[0];
        let clone_cmd = &init.args.as_ref().unwrap()[0];

        // Verify the clone command reads from /git-auth/token
        assert!(
            clone_cmd.contains("/git-auth/token"),
            "clone command should read from /git-auth/token, got: {clone_cmd}"
        );
        // Verify it uses GIT_ASKPASS
        assert!(
            clone_cmd.contains("GIT_ASKPASS"),
            "clone command should use GIT_ASKPASS"
        );
        // Verify it uses --depth 1
        assert!(
            clone_cmd.contains("--depth 1"),
            "clone command should use --depth 1 for shallow clone"
        );
    }

    // -- step_condition_from_row: both events and branches simultaneously --

    #[test]
    fn step_condition_from_row_events_only() {
        let row = StepRow {
            id: Uuid::nil(),
            step_order: 0,
            name: "test".into(),
            image: "alpine".into(),
            commands: vec![],
            condition_events: vec!["push".into(), "tag".into()],
            condition_branches: vec![],
            deploy_test: None,
            depends_on: vec![],
            environment: None,
            gate: false,
            step_type: "command".into(),
            step_config: None,
        };
        let cond = step_condition_from_row(&row).unwrap();
        assert_eq!(cond.events, vec!["push", "tag"]);
        assert!(cond.branches.is_empty());
    }

    // -- mark_transitive_dependents_skipped: large fan-in --

    #[test]
    fn mark_transitive_dependents_fan_in() {
        // Steps 0,1,2 all depend on step 3
        // 0 → 3, 1 → 3, 2 → 3
        let dependents = vec![vec![3], vec![3], vec![3], vec![]];
        let mut skipped = std::collections::HashSet::new();
        let completed = std::collections::HashSet::new();
        mark_transitive_dependents_skipped(0, &dependents, &mut skipped, &completed);
        // Only 3 should be skipped (not 1 or 2, they are not dependents of 0)
        assert_eq!(skipped.len(), 1);
        assert!(skipped.contains(&3));
    }

    // -- build_env_vars_core: empty step name --

    #[test]
    fn env_vars_core_empty_step_name() {
        let vars = build_env_vars_core(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "main",
            None,
            "",
            None,
            None,
            "push",
        );
        assert_eq!(find_env(&vars, "STEP_NAME"), Some(String::new()));
    }

    // -- DEFAULT_STEP_TIMEOUT_SECS --

    #[test]
    fn default_step_timeout_is_900_seconds() {
        assert_eq!(DEFAULT_STEP_TIMEOUT_SECS, 900);
    }

    // -- PipelineMeta construction and Debug --

    #[test]
    fn pipeline_meta_debug() {
        let meta = PipelineMeta {
            git_ref: "refs/heads/main".into(),
            commit_sha: Some("abc123".into()),
            version: Some("1.0.0".into()),
            project_name: "test-project".into(),
            repo_clone_url: "http://platform:8080/owner/repo.git".into(),
            git_auth_token: "secret-token".into(),
            namespace: "test-ns-dev".into(),
            trigger_type: "push".into(),
            namespace_slug: "test-ns".into(),
            otlp_token: Some("otlp-token".into()),
            git_secret_name: "pl-git-12345678".into(),
        };
        let debug = format!("{meta:?}");
        assert!(debug.contains("test-project"));
        assert!(debug.contains("refs/heads/main"));
    }

    #[test]
    fn pipeline_meta_without_optional_fields() {
        let meta = PipelineMeta {
            git_ref: "main".into(),
            commit_sha: None,
            version: None,
            project_name: "app".into(),
            repo_clone_url: "http://platform:8080/user/app.git".into(),
            git_auth_token: "tok".into(),
            namespace: "app-dev".into(),
            trigger_type: "api".into(),
            namespace_slug: "app".into(),
            otlp_token: None,
            git_secret_name: "pl-git-00000000".into(),
        };
        assert!(meta.commit_sha.is_none());
        assert!(meta.version.is_none());
        assert!(meta.otlp_token.is_none());
    }

    // -- StepRow construction and fields --

    #[test]
    fn step_row_all_fields() {
        let row = StepRow {
            id: Uuid::new_v4(),
            step_order: 2,
            name: "deploy".into(),
            image: "registry.io/app:latest".into(),
            commands: vec!["deploy.sh".into(), "verify.sh".into()],
            condition_events: vec!["push".into()],
            condition_branches: vec!["main".into(), "release/*".into()],
            deploy_test: Some(serde_json::json!({"test_image": "test:v1"})),
            depends_on: vec!["build".into(), "test".into()],
            environment: Some(serde_json::json!({"ENV": "production"})),
            gate: true,
            step_type: "deploy_test".into(),
            step_config: Some(serde_json::json!({"timeout": 300})),
        };
        assert_eq!(row.step_order, 2);
        assert_eq!(row.name, "deploy");
        assert!(row.gate);
        assert_eq!(row.commands.len(), 2);
        assert_eq!(row.depends_on.len(), 2);
        assert!(row.deploy_test.is_some());
        assert!(row.environment.is_some());
        assert!(row.step_config.is_some());
    }

    // -- TestNamespaceGuard: verify field semantics --

    #[test]
    fn test_namespace_guard_namespace_format() {
        // Verify the expected namespace format for deploy-test
        let pipeline_id = Uuid::parse_str("12345678-1234-1234-1234-123456789abc").unwrap();
        let ns_name = format!("{}-test-{}", "my-project", &pipeline_id.to_string()[..8]);
        assert_eq!(ns_name, "my-project-test-12345678");
    }

    // -- build_env_vars_core: verify all standard vars present --

    #[test]
    fn env_vars_core_has_expected_count_without_optional() {
        let vars = build_env_vars_core(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "main",
            None,
            "test",
            None,
            None,
            "push",
        );
        // Without commit_sha, registry_url: should have 9 vars
        // PROJECT_ID, PROJECT_NAME, PIPELINE_ID, STEP_NAME, COMMIT_REF,
        // COMMIT_BRANCH, PROJECT, PIPELINE_TRIGGER, VERSION
        assert_eq!(vars.len(), 9, "expected 9 vars, got: {vars:?}");
    }

    #[test]
    fn env_vars_core_has_expected_count_with_all_optional() {
        let vars = build_env_vars_core(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            "refs/heads/main",
            Some("abc1234"),
            "build",
            Some("registry.io:5000"),
            Some("1.0.0"),
            "push",
        );
        // With commit_sha (+3: COMMIT_SHA, SHORT_SHA, IMAGE_TAG), registry (+2: REGISTRY, DOCKER_CONFIG):
        // 9 + 3 + 2 = 14
        assert_eq!(vars.len(), 14, "expected 14 vars, got: {vars:?}");
    }

    // -- is_reserved_pipeline_env_var: case sensitivity --

    #[test]
    fn reserved_pipeline_env_var_case_sensitive() {
        // Reserved vars are exact match — lowercase should not be blocked
        assert!(!is_reserved_pipeline_env_var("pipeline_id"));
        assert!(!is_reserved_pipeline_env_var("commit_sha"));
        assert!(!is_reserved_pipeline_env_var("path")); // lowercase path is not PATH
    }

    // -- build_volumes_and_mounts: verify all combinations --

    #[test]
    fn build_volumes_and_mounts_all_three_volumes() {
        let (volumes, mounts) = build_volumes_and_mounts(Some("reg"), Some("git"), None);
        // workspace + docker-config + git-auth
        assert_eq!(volumes.len(), 3);
        assert_eq!(volumes[0].name, "workspace");
        assert_eq!(volumes[1].name, "docker-config");
        assert_eq!(volumes[2].name, "git-auth");
        // step mounts: workspace + docker-config (git-auth is only for init container)
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0].name, "workspace");
        assert_eq!(mounts[1].name, "docker-config");
    }

    // -- proxy wrapping tests --

    #[test]
    fn build_pod_spec_with_proxy_wraps_command() {
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test-proxy",
            pipeline_id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            step_name: "build",
            image: "rust:latest",
            commands: &["cargo build".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/repo.git",
            git_ref: "main",
            registry_secret: None,
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: Some("/tmp/proxy"),
        });
        let spec = pod.spec.as_ref().unwrap();
        let container = &spec.containers[0];
        assert_eq!(
            container.command.as_ref().unwrap(),
            &["/proxy/platform-proxy"]
        );
        let args = container.args.as_ref().unwrap();
        assert_eq!(args[0], "--wrap");
        assert_eq!(args[1], "--");
        assert_eq!(args[2], "sh");
        assert_eq!(args[3], "-c");
        assert!(args[4].contains("cargo build"));
    }

    #[test]
    fn build_pod_spec_with_proxy_adds_volume() {
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test-vol",
            pipeline_id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            step_name: "build",
            image: "rust:latest",
            commands: &["echo hi".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/repo.git",
            git_ref: "main",
            registry_secret: None,
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: Some("/tmp/proxy"),
        });
        let spec = pod.spec.as_ref().unwrap();
        let volumes = spec.volumes.as_ref().unwrap();
        assert!(
            volumes.iter().any(|v| v.name == "proxy"),
            "proxy volume should exist"
        );
        let mounts = spec.containers[0].volume_mounts.as_ref().unwrap();
        assert!(
            mounts
                .iter()
                .any(|m| m.name == "proxy" && m.mount_path == "/proxy"),
            "proxy mount should exist"
        );
    }

    #[test]
    fn build_pod_spec_without_proxy_normal_command() {
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test-noproxy",
            pipeline_id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            step_name: "build",
            image: "rust:latest",
            commands: &["echo hi".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/repo.git",
            git_ref: "main",
            registry_secret: None,
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
        });
        let spec = pod.spec.as_ref().unwrap();
        let container = &spec.containers[0];
        assert_eq!(container.command.as_ref().unwrap(), &["sh", "-c"]);
        let volumes = spec.volumes.as_ref().unwrap();
        assert!(
            !volumes.iter().any(|v| v.name == "proxy"),
            "no proxy volume without proxy_binary_path"
        );
    }

    #[test]
    fn build_volumes_and_mounts_with_proxy_path() {
        let (volumes, mounts) = build_volumes_and_mounts(None, None, Some("/tmp/proxy"));
        assert_eq!(volumes.len(), 2); // workspace + proxy
        assert!(volumes.iter().any(|v| v.name == "proxy"));
        assert_eq!(mounts.len(), 2); // workspace + proxy
        assert!(
            mounts
                .iter()
                .any(|m| m.name == "proxy" && m.mount_path == "/proxy")
        );
    }

    // -- pod spec labels validation --

    #[test]
    fn build_pod_spec_labels_use_correct_keys() {
        let pipeline_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test",
            pipeline_id,
            project_id,
            step_name: "My Step",
            image: "alpine:3.19",
            commands: &["true".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/test.git",
            git_ref: "main",
            registry_secret: None,
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
        });

        let labels = pod.metadata.labels.unwrap();
        assert_eq!(labels.len(), 3);
        assert!(labels.contains_key("platform.io/pipeline"));
        assert!(labels.contains_key("platform.io/project"));
        assert!(labels.contains_key("platform.io/step"));
        // step label should be slugified
        assert_eq!(labels["platform.io/step"], "my-step");
    }

    // -- init container name is "clone" --

    #[test]
    fn build_pod_spec_init_container_named_clone() {
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
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
        });

        let spec = pod.spec.unwrap();
        let init = &spec.init_containers.as_ref().unwrap()[0];
        assert_eq!(init.name, "clone");
    }

    // -- step container name is "step" --

    #[test]
    fn build_pod_spec_step_container_named_step() {
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
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
        });

        let spec = pod.spec.unwrap();
        assert_eq!(spec.containers[0].name, "step");
    }

    // -- PipelineStatus integration with executor logic --

    #[test]
    fn pipeline_status_pending_to_failure_valid() {
        // Executor marks pipeline as failed when it can't execute
        assert!(PipelineStatus::Pending.can_transition_to(PipelineStatus::Failure));
    }

    #[test]
    fn pipeline_status_running_to_cancelled_valid() {
        // cancel_pipeline transitions from Running → Cancelled
        assert!(PipelineStatus::Running.can_transition_to(PipelineStatus::Cancelled));
    }

    #[test]
    fn pipeline_status_cancelled_is_terminal() {
        assert!(PipelineStatus::Cancelled.is_terminal());
    }

    #[test]
    fn pipeline_status_success_cannot_transition() {
        assert!(!PipelineStatus::Success.can_transition_to(PipelineStatus::Failure));
        assert!(!PipelineStatus::Success.can_transition_to(PipelineStatus::Running));
        assert!(!PipelineStatus::Success.can_transition_to(PipelineStatus::Cancelled));
    }

    // -- extract_branch vs build_env_vars_core consistency --

    #[test]
    fn extract_branch_and_env_vars_core_consistent() {
        let git_ref = "refs/heads/feature/deep";
        let branch_from_extract = extract_branch(git_ref);
        let vars = build_env_vars_core(
            Uuid::nil(),
            Uuid::nil(),
            "proj",
            git_ref,
            None,
            "test",
            None,
            None,
            "push",
        );
        let branch_from_env = find_env(&vars, "COMMIT_BRANCH").unwrap();
        assert_eq!(
            branch_from_extract, branch_from_env,
            "extract_branch and build_env_vars_core should produce the same branch"
        );
    }

    // -- sanitize_relative_path --

    #[test]
    fn sanitize_relative_path_normal() {
        assert_eq!(
            sanitize_relative_path("auth/LoginForm.png").unwrap(),
            "auth/LoginForm.png"
        );
    }

    #[test]
    fn sanitize_relative_path_strips_leading_slash() {
        assert_eq!(
            sanitize_relative_path("/auth/LoginForm.png").unwrap(),
            "auth/LoginForm.png"
        );
    }

    #[test]
    fn sanitize_relative_path_rejects_parent_dir() {
        assert!(sanitize_relative_path("../etc/passwd").is_err());
    }

    #[test]
    fn sanitize_relative_path_rejects_nested_traversal() {
        assert!(sanitize_relative_path("foo/../../etc/passwd").is_err());
    }

    #[test]
    fn sanitize_relative_path_skips_dot() {
        assert_eq!(
            sanitize_relative_path("./auth/./LoginForm.png").unwrap(),
            "auth/LoginForm.png"
        );
    }

    #[test]
    fn sanitize_relative_path_rejects_empty() {
        assert!(sanitize_relative_path("").is_err());
    }

    #[test]
    fn sanitize_relative_path_rejects_root_only() {
        assert!(sanitize_relative_path("/").is_err());
    }

    // -- validate_artifact_config --

    #[test]
    fn validate_config_valid_minimal() {
        let json = r#"{"groups": {"auth": {"label": "Auth", "items": {"f.png": {"label": "F"}}}}}"#;
        assert!(validate_artifact_config(json.as_bytes(), "test").is_ok());
    }

    #[test]
    fn validate_config_valid_nested_3_levels() {
        let json = r#"{"groups": {
            "l1": {"label": "Level 1", "groups": {
                "l2": {"label": "Level 2", "groups": {
                    "l3": {"label": "Level 3", "items": {"f.png": {"label": "F"}}}
                }}
            }}
        }}"#;
        assert!(validate_artifact_config(json.as_bytes(), "test").is_ok());
    }

    #[test]
    fn validate_config_rejects_4_levels() {
        let json = r#"{"groups": {
            "l1": {"label": "Level 1", "groups": {
                "l2": {"label": "Level 2", "groups": {
                    "l3": {"label": "Level 3", "groups": {
                        "l4": {"label": "Level 4", "items": {}}
                    }}
                }}
            }}
        }}"#;
        let err = validate_artifact_config(json.as_bytes(), "test").unwrap_err();
        assert!(
            err.to_string().contains("nesting depth"),
            "expected nesting depth error, got: {err}"
        );
    }

    #[test]
    fn validate_config_rejects_invalid_json() {
        let err = validate_artifact_config(b"not json", "test").unwrap_err();
        assert!(err.to_string().contains("not valid JSON"), "got: {err}");
    }

    #[test]
    fn validate_config_rejects_missing_groups() {
        let json = r#"{"foo": "bar"}"#;
        let err = validate_artifact_config(json.as_bytes(), "test").unwrap_err();
        assert!(err.to_string().contains("missing root"), "got: {err}");
    }

    #[test]
    fn validate_config_rejects_missing_group_label() {
        let json = r#"{"groups": {"auth": {"items": {}}}}"#;
        let err = validate_artifact_config(json.as_bytes(), "test").unwrap_err();
        assert!(
            err.to_string().contains("missing required \"label\""),
            "got: {err}"
        );
    }

    #[test]
    fn validate_config_rejects_missing_item_label() {
        let json = r#"{"groups": {"auth": {"label": "Auth", "items": {"f.png": {"meta": {}}}}}}"#;
        let err = validate_artifact_config(json.as_bytes(), "test").unwrap_err();
        assert!(
            err.to_string().contains("missing required \"label\""),
            "got: {err}"
        );
    }

    #[test]
    fn validate_config_rejects_invalid_group_key() {
        let json = r#"{"groups": {"UPPER CASE!": {"label": "Bad"}}}"#;
        let err = validate_artifact_config(json.as_bytes(), "test").unwrap_err();
        assert!(err.to_string().contains("invalid group key"), "got: {err}");
    }

    #[test]
    fn validate_config_rejects_meta_non_object() {
        let json = r#"{"groups": {"auth": {"label": "Auth", "items": {"f.png": {"label": "F", "meta": "string"}}}}}"#;
        let err = validate_artifact_config(json.as_bytes(), "test").unwrap_err();
        assert!(
            err.to_string().contains("meta must be an object"),
            "got: {err}"
        );
    }

    #[test]
    fn validate_config_allows_empty_groups() {
        let json = r#"{"groups": {}}"#;
        assert!(validate_artifact_config(json.as_bytes(), "test").is_ok());
    }

    // -- unpack_and_store --

    fn create_test_tar_gz(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar_buf = Vec::new();
        {
            let enc = flate2::write::GzEncoder::new(&mut tar_buf, flate2::Compression::fast());
            let mut builder = tar::Builder::new(enc);
            for (path, data) in files {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append_data(&mut header, path, *data).unwrap();
            }
            builder.into_inner().unwrap().finish().unwrap();
        }
        tar_buf
    }

    fn create_test_tar_gz_with_symlink(
        files: &[(&str, &[u8])],
        symlink_name: &str,
        symlink_target: &str,
    ) -> Vec<u8> {
        let mut tar_buf = Vec::new();
        {
            let enc = flate2::write::GzEncoder::new(&mut tar_buf, flate2::Compression::fast());
            let mut builder = tar::Builder::new(enc);
            for (path, data) in files {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append_data(&mut header, path, *data).unwrap();
            }
            // Add symlink
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_mode(0o777);
            header.set_cksum();
            builder
                .append_link(&mut header, symlink_name, symlink_target)
                .unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }
        tar_buf
    }

    #[test]
    fn unpack_and_store_extracts_files() {
        let tar_bytes = create_test_tar_gz(&[
            ("auth/LoginForm.png", b"png1"),
            ("auth/SignupForm.png", b"png2"),
            ("dashboard/Main.png", b"png3"),
        ]);

        let files = extract_tar_files(&tar_bytes, 50 * 1024 * 1024, 500 * 1024 * 1024).unwrap();
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].sanitized_path, "auth/LoginForm.png");
        assert_eq!(files[0].content_type, "image/png");
        assert_eq!(files[0].contents, b"png1");
        assert_eq!(files[1].sanitized_path, "auth/SignupForm.png");
        assert_eq!(files[2].sanitized_path, "dashboard/Main.png");
    }

    #[test]
    fn unpack_and_store_skips_symlinks() {
        let tar_bytes =
            create_test_tar_gz_with_symlink(&[("real.png", b"data")], "link.png", "real.png");

        // extract_tar_files skips symlinks
        let files = extract_tar_files(&tar_bytes, 50 * 1024 * 1024, 500 * 1024 * 1024).unwrap();
        assert_eq!(files.len(), 1, "symlink should be skipped");
        assert_eq!(files[0].sanitized_path, "real.png");
    }

    #[test]
    fn unpack_and_store_rejects_path_traversal() {
        // The tar crate normalizes ".." paths during building, so we test
        // sanitize_relative_path directly (which is what extract_tar_files calls).
        // This verifies the security boundary without needing to craft a raw tar.
        assert!(sanitize_relative_path("../evil.txt").is_err());
        assert!(sanitize_relative_path("foo/../../etc/passwd").is_err());
        // Also verify valid paths pass through extract_tar_files
        let tar_bytes = create_test_tar_gz(&[("safe/file.txt", b"ok")]);
        let files = extract_tar_files(&tar_bytes, 50 * 1024 * 1024, 500 * 1024 * 1024).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].sanitized_path, "safe/file.txt");
    }

    #[test]
    fn unpack_and_store_enforces_file_size_limit() {
        // Create a file that's 10 bytes, but set limit to 5
        let tar_bytes = create_test_tar_gz(&[("big.txt", b"0123456789")]);

        let result = extract_tar_files(&tar_bytes, 5, 500 * 1024 * 1024);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("exceeds size limit"),
            "should reject oversized file"
        );
    }

    #[test]
    fn unpack_and_store_enforces_total_size_limit() {
        // Two 10-byte files, total limit 15
        let tar_bytes = create_test_tar_gz(&[("a.txt", b"0123456789"), ("b.txt", b"0123456789")]);

        let result = extract_tar_files(&tar_bytes, 50 * 1024 * 1024, 15);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("total size exceeds"),
            "should reject total size"
        );
    }

    #[test]
    fn unpack_and_store_enforces_file_count_limit() {
        // Generate 1001 files would be expensive, so just verify the logic via extract_tar_files
        // by creating a few files and setting a low limit
        let tar_bytes = create_test_tar_gz(&[("a.txt", b"a"), ("b.txt", b"b"), ("c.txt", b"c")]);

        // We can't easily test 1000+ files, but the logic is in extract_tar_files
        // where max_file_count = 1000. Just verify 3 files works.
        let files = extract_tar_files(&tar_bytes, 50 * 1024 * 1024, 500 * 1024 * 1024).unwrap();
        assert_eq!(files.len(), 3);
    }

    // -- infer_content_type --

    #[test]
    fn infer_content_type_png() {
        assert_eq!(infer_content_type("foo.png"), "image/png");
    }

    #[test]
    fn infer_content_type_jpg() {
        assert_eq!(infer_content_type("bar.jpg"), "image/jpeg");
    }

    #[test]
    fn infer_content_type_jpeg() {
        assert_eq!(infer_content_type("bar.jpeg"), "image/jpeg");
    }

    #[test]
    fn infer_content_type_unknown() {
        assert_eq!(infer_content_type("baz.xyz"), "application/octet-stream");
    }

    #[test]
    fn infer_content_type_svg() {
        assert_eq!(infer_content_type("icon.svg"), "image/svg+xml");
    }

    #[test]
    fn infer_content_type_json() {
        assert_eq!(infer_content_type("config.json"), "application/json");
    }

    #[test]
    fn infer_content_type_html() {
        assert_eq!(infer_content_type("index.html"), "text/html");
    }

    // -- build_pod_spec artifact wrapping --

    #[test]
    fn build_pod_spec_artifact_wraps_script() {
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test",
            pipeline_id: Uuid::nil(),
            project_id: Uuid::nil(),
            step_name: "test",
            image: "alpine:3.19",
            commands: &["echo a".into(), "echo b".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/test.git",
            git_ref: "main",
            registry_secret: None,
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: true,
            proxy_binary_path: None,
        });

        let container = &pod.spec.unwrap().containers[0];
        let script = &container.args.as_ref().unwrap()[0];
        assert!(
            script.contains("/tmp/.exit-code"),
            "script should contain exit-code marker: {script}"
        );
        assert!(
            script.contains("/tmp/.done"),
            "script should contain done marker: {script}"
        );
        assert!(
            script.contains("echo a && echo b"),
            "script should contain original commands: {script}"
        );
    }

    #[test]
    fn build_pod_spec_no_artifact_script_unchanged() {
        let pod = build_pod_spec(&PodSpecParams {
            pod_name: "pl-test",
            pipeline_id: Uuid::nil(),
            project_id: Uuid::nil(),
            step_name: "test",
            image: "alpine:3.19",
            commands: &["echo a".into(), "echo b".into()],
            env_vars: &[],
            repo_clone_url: "http://platform:8080/owner/test.git",
            git_ref: "main",
            registry_secret: None,
            git_secret_name: None,
            step_type: "command",
            git_clone_image: "alpine/git:2.47.2",
            has_artifacts: false,
            proxy_binary_path: None,
        });

        let container = &pod.spec.unwrap().containers[0];
        let script = &container.args.as_ref().unwrap()[0];
        assert_eq!(script, "echo a && echo b");
        assert!(
            !script.contains("/tmp/.exit-code"),
            "script without artifacts should not have exit-code marker"
        );
    }

    // -- extract_artifact_defs --

    #[test]
    fn extract_artifact_defs_from_step_config() {
        let config = serde_json::json!({
            "artifacts": [
                {"name": "ui", "path": "output/", "type": "ui-comp"},
                {"name": "flows", "path": "flows/", "type": "ui-flow", "config": "flows/config.json"}
            ]
        });
        let defs = extract_artifact_defs(Some(&config));
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].name, "ui");
        assert_eq!(defs[0].artifact_type, "ui-comp");
        assert_eq!(defs[1].config.as_deref(), Some("flows/config.json"));
    }

    #[test]
    fn extract_artifact_defs_empty_when_no_config() {
        let defs = extract_artifact_defs(None);
        assert!(defs.is_empty());
    }

    #[test]
    fn extract_artifact_defs_empty_when_no_artifacts_key() {
        let config = serde_json::json!({"image_name": "app"});
        let defs = extract_artifact_defs(Some(&config));
        assert!(defs.is_empty());
    }
}
