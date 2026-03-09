use std::time::Duration;

use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use kube::api::{AttachParams, DeleteParams, LogParams, PostParams};
use sqlx::PgPool;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::secrets::user_keys;
use crate::store::AppState;

use super::AgentRoleName;
use super::claude_code::ClaudeCodeProvider;
use super::error::AgentError;
use super::identity;
use super::provider::{
    AgentProvider, AgentSession, BuildPodParams, ProgressEvent, ProgressKind, ProviderConfig,
};

// ---------------------------------------------------------------------------
// Provider resolution
// ---------------------------------------------------------------------------

/// Resolve a provider by name. Currently only "claude-code" is supported.
pub fn get_provider(name: &str) -> Result<Box<dyn AgentProvider>, AgentError> {
    match name {
        "claude-code" => Ok(Box::new(ClaudeCodeProvider)),
        other => Err(AgentError::InvalidProvider(format!(
            "unknown provider: {other}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Session lifecycle
// ---------------------------------------------------------------------------

/// Create a new agent session: insert DB row, create identity, spawn K8s pod.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
#[tracing::instrument(skip(state, prompt, provider_config), fields(%user_id, %project_id, %agent_role), err)]
pub async fn create_session(
    state: &AppState,
    user_id: Uuid,
    project_id: Uuid,
    prompt: &str,
    provider_name: &str,
    branch: Option<&str>,
    provider_config: Option<serde_json::Value>,
    agent_role: AgentRoleName,
    parent_session_id: Option<Uuid>,
) -> Result<AgentSession, AgentError> {
    let provider = get_provider(provider_name)?;
    let config: ProviderConfig = provider_config
        .as_ref()
        .map(|v| serde_json::from_value(v.clone()).unwrap_or_default())
        .unwrap_or_default();

    // 1. Insert session row (pending)
    let session_id = Uuid::new_v4();
    let short_id = &session_id.to_string()[..8];
    let branch_name = branch.map_or_else(|| format!("agent/{short_id}"), String::from);

    // Compute spawn_depth from parent (if any)
    let spawn_depth: i32 = if let Some(pid) = parent_session_id {
        let parent_depth = sqlx::query_scalar!(
            r#"SELECT spawn_depth as "spawn_depth!" FROM agent_sessions WHERE id = $1"#,
            pid,
        )
        .fetch_optional(&state.pool)
        .await?
        .unwrap_or(0);
        parent_depth + 1
    } else {
        0
    };

    sqlx::query!(
        r#"
        INSERT INTO agent_sessions (id, project_id, user_id, prompt, provider, provider_config, branch, status, parent_session_id, spawn_depth)
        VALUES ($1, $2, $3, $4, $5, $6, $7, 'pending', $8, $9)
        "#,
        session_id,
        project_id,
        user_id,
        prompt,
        provider_name,
        provider_config.as_ref(),
        branch_name,
        parent_session_id,
        spawn_depth,
    )
    .execute(&state.pool)
    .await?;

    // 2. Look up project's workspace_id and namespace_slug for scope boundaries
    let project_info = sqlx::query!(
        "SELECT workspace_id, namespace_slug FROM projects WHERE id = $1 AND is_active = true",
        project_id,
    )
    .fetch_one(&state.pool)
    .await?;
    let workspace_id = project_info.workspace_id;
    let namespace = state
        .config
        .project_namespace(&project_info.namespace_slug, "dev");

    // Ensure project namespace exists (lazy creation for DB-only projects)
    crate::deployer::namespace::ensure_namespace(
        &state.kube,
        &namespace,
        "dev",
        &project_id.to_string(),
    )
    .await
    .map_err(|e| AgentError::Other(e.into()))?;
    if !state.config.dev_mode {
        let _ = crate::deployer::namespace::ensure_network_policy(
            &state.kube,
            &namespace,
            &state.config.platform_namespace,
        )
        .await;
    }

    // 3. Create ephemeral agent identity with role-based permissions
    let agent_identity = identity::create_agent_identity(
        &state.pool,
        &state.valkey,
        session_id,
        user_id,
        project_id,
        workspace_id,
        agent_role,
    )
    .await?;

    // Update session with agent_user_id
    sqlx::query!(
        "UPDATE agent_sessions SET agent_user_id = $2 WHERE id = $1",
        session_id,
        agent_identity.user_id,
    )
    .execute(&state.pool)
    .await?;

    // 3. Get repo clone URL and agent image for the project
    let platform_api_url = &state.config.platform_api_url;
    let (repo_clone_url, project_agent_image) =
        get_project_repo_info(&state.pool, project_id, platform_api_url).await?;

    // 4. Resolve auth: CLI subscription credentials > user API key > global platform secret
    let cli_oauth_token = resolve_cli_oauth_token(state, user_id).await;
    let user_api_key = if cli_oauth_token.is_some() {
        None // CLI OAuth takes priority; skip API key lookup
    } else {
        match resolve_user_api_key(state, user_id).await {
            Some(key) => Some(key),
            None => resolve_global_api_key(state).await,
        }
    };

    // 4b. Query project secrets scoped to agent/all
    let extra_env_vars = resolve_agent_secrets(state, project_id).await;

    // 4c. Create registry pull secret if registry is configured
    // Use registry_node_url (DaemonSet proxy) for image refs that containerd pulls;
    // fall back to registry_url for backward compatibility.
    let node_registry_url = state
        .config
        .registry_node_url
        .as_deref()
        .or(state.config.registry_url.as_deref());
    let registry_pull_secret = if let Some(reg_url) = node_registry_url {
        match crate::registry::pull_secret::create_pull_secret(
            &state.pool,
            &state.kube,
            reg_url,
            user_id,
            &namespace,
            "platform.io/session",
            &session_id.to_string(),
        )
        .await
        {
            Ok(result) => Some(result),
            Err(e) => {
                tracing::warn!(error = %e, "failed to create registry pull secret for agent, continuing without");
                None
            }
        }
    } else {
        None
    };

    // 5. Create per-session Valkey ACL for pub/sub isolation
    let valkey_creds = super::valkey_acl::create_session_acl(
        &state.valkey,
        session_id,
        &state.config.valkey_agent_host,
    )
    .await?;

    // 6. Build and create the K8s pod
    let session_for_pod = AgentSession {
        id: session_id,
        project_id: Some(project_id),
        user_id,
        agent_user_id: Some(agent_identity.user_id),
        prompt: prompt.to_owned(),
        status: "pending".to_owned(),
        branch: Some(branch_name.clone()),
        pod_name: None,
        provider: provider_name.to_owned(),
        provider_config,
        cost_tokens: None,
        created_at: chrono::Utc::now(),
        finished_at: None,
        parent_session_id: None,
        spawn_depth: 0,
        allowed_child_roles: None,
        execution_mode: "pod".to_owned(),
        uses_pubsub: true,
    };

    // Test/dev: mount host fixture path and set CLAUDE_CLI_PATH for mock CLI
    let host_mount_path = std::env::var("PLATFORM_HOST_MOUNT_PATH").ok();
    let claude_cli_path = std::env::var("CLAUDE_CLI_PATH").ok();

    let pod = provider.build_pod(BuildPodParams {
        session: &session_for_pod,
        config: &config,
        agent_api_token: &agent_identity.api_token,
        platform_api_url,
        repo_clone_url: &repo_clone_url,
        namespace: &namespace,
        project_agent_image: project_agent_image.as_deref(),
        anthropic_api_key: user_api_key.as_deref(),
        cli_oauth_token: cli_oauth_token.as_deref(),
        extra_env_vars: &extra_env_vars,
        registry_url: node_registry_url,
        registry_secret_name: registry_pull_secret
            .as_ref()
            .map(|s| s.secret_name.as_str()),
        valkey_url: Some(&valkey_creds.url),
        claude_cli_version: &state.config.claude_cli_version,
        host_mount_path: host_mount_path.as_deref(),
        claude_cli_path: claude_cli_path.as_deref(),
    })?;

    let pod_name = pod
        .metadata
        .name
        .clone()
        .unwrap_or_else(|| format!("agent-{short_id}"));

    // 7. Start persistence subscriber BEFORE pod creation so no pub/sub messages are lost.
    //    The subscriber connects and subscribes synchronously before returning.
    super::pubsub_bridge::spawn_persistence_subscriber(
        state.pool.clone(),
        state.valkey.clone(),
        session_id,
    )
    .await;

    // 8. Create the pod (subscriber is already listening)
    let pods: Api<Pod> = Api::namespaced(state.kube.clone(), &namespace);
    if let Err(e) = pods.create(&PostParams::default(), &pod).await {
        let _ = super::valkey_acl::delete_session_acl(&state.valkey, session_id).await;
        return Err(AgentError::PodCreationFailed(e.to_string()));
    }

    // 9. Update session to running with pod_name + uses_pubsub
    sqlx::query!(
        "UPDATE agent_sessions SET status = 'running', pod_name = $2, uses_pubsub = true WHERE id = $1",
        session_id,
        pod_name,
    )
    .execute(&state.pool)
    .await?;

    // 10. Return the complete session
    fetch_session(&state.pool, session_id).await
}

/// Send a message to a running agent session.
///
/// Routes via Valkey pub/sub for `uses_pubsub` sessions, otherwise falls back
/// to execution-mode-specific routing (`cli_subprocess`, pod stdin).
#[tracing::instrument(skip(state, content), fields(%session_id), err)]
pub async fn send_message(
    state: &AppState,
    session_id: Uuid,
    content: &str,
) -> Result<(), AgentError> {
    let session = fetch_session(&state.pool, session_id).await?;

    if session.status != "running" {
        return Err(AgentError::SessionNotRunning);
    }

    // Pub/sub path (agent-runner pods)
    if session.uses_pubsub {
        super::pubsub_bridge::publish_prompt(&state.valkey, session_id, content)
            .await
            .map_err(AgentError::Other)?;
        return Ok(());
    }

    // CLI subprocess routing
    if session.execution_mode == "cli_subprocess" {
        return send_cli_message(state, session_id, content).await;
    }

    // "pod" — fall through to existing pod attach logic

    let pod_name = session
        .pod_name
        .as_deref()
        .ok_or(AgentError::SessionNotRunning)?;
    let namespace = resolve_session_namespace(
        &state.pool,
        &session,
        &state.config.agent_namespace,
        &state.config,
    )
    .await?;
    let pods: Api<Pod> = Api::namespaced(state.kube.clone(), &namespace);

    let mut attached = pods
        .attach(
            pod_name,
            &AttachParams {
                container: Some("claude".into()),
                stdin: true,
                stdout: false,
                stderr: false,
                ..Default::default()
            },
        )
        .await
        .map_err(|e| AgentError::AttachFailed(e.to_string()))?;

    // Write message to stdin
    let mut stdin = attached
        .stdin()
        .ok_or_else(|| AgentError::AttachFailed("no stdin available".into()))?;
    stdin
        .write_all(format!("{content}\n").as_bytes())
        .await
        .map_err(|e| AgentError::AttachFailed(e.to_string()))?;

    // Store the user message in agent_messages
    sqlx::query!(
        "INSERT INTO agent_messages (session_id, role, content) VALUES ($1, 'user', $2)",
        session_id,
        content,
    )
    .execute(&state.pool)
    .await?;

    Ok(())
}

/// Stop a running session: delete the pod, update status, cleanup identity.
#[tracing::instrument(skip(state), fields(%session_id), err)]
pub async fn stop_session(state: &AppState, session_id: Uuid) -> Result<(), AgentError> {
    let session = fetch_session(&state.pool, session_id).await?;

    match session.execution_mode.as_str() {
        "cli_subprocess" => {
            // CLI subprocess — kill the process and remove from manager
            stop_cli_session(state, session_id).await;
        }
        _ => {
            // Pod session — capture logs and delete pod
            if let Some(ref pod_name) = session.pod_name {
                let namespace = resolve_session_namespace(
                    &state.pool,
                    &session,
                    &state.config.agent_namespace,
                    &state.config,
                )
                .await?;
                let pods: Api<Pod> = Api::namespaced(state.kube.clone(), &namespace);
                capture_session_logs(&pods, pod_name, state, session_id).await;
                let _ = pods.delete(pod_name, &DeleteParams::default()).await;
            }
        }
    }

    // Update session status
    sqlx::query!(
        "UPDATE agent_sessions SET status = 'stopped', finished_at = now() WHERE id = $1",
        session_id,
    )
    .execute(&state.pool)
    .await?;

    // Cleanup agent identity
    if let Some(agent_user_id) = session.agent_user_id {
        identity::cleanup_agent_identity(&state.pool, &state.valkey, agent_user_id).await?;
    }

    // Cleanup Valkey ACL user
    let _ = super::valkey_acl::delete_session_acl(&state.valkey, session_id).await;

    Ok(())
}

// ---------------------------------------------------------------------------
// Background reaper
// ---------------------------------------------------------------------------

/// Run a single reaper iteration (used by E2E tests that don't run the background loop).
#[allow(dead_code)]
pub async fn run_reaper_once(state: &AppState) {
    if let Err(e) = reap_terminated_sessions(state).await {
        tracing::error!(error = %e, "error reaping agent sessions");
    }
}

/// Background task that periodically checks for terminated agent pods and
/// finalizes their sessions.
pub async fn run_reaper(state: AppState, mut shutdown: tokio::sync::watch::Receiver<()>) {
    tracing::info!("agent session reaper started");
    state.task_registry.register("agent_reaper", 60);

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                tracing::info!("agent session reaper shutting down");
                break;
            }
            () = tokio::time::sleep(Duration::from_secs(30)) => {
                match reap_terminated_sessions(&state).await {
                    Ok(()) => state.task_registry.heartbeat("agent_reaper"),
                    Err(e) => {
                        state.task_registry.report_error("agent_reaper", &e.to_string());
                        tracing::error!(error = %e, "error reaping agent sessions");
                    }
                }
            }
        }
    }
}

/// Find running sessions whose pods have terminated and finalize them.
async fn reap_terminated_sessions(state: &AppState) -> Result<(), AgentError> {
    let running = sqlx::query!(
        r#"
        SELECT s.id as "id!", s.pod_name, s.agent_user_id, s.project_id,
               p.namespace_slug as "namespace_slug?"
        FROM agent_sessions s
        LEFT JOIN projects p ON p.id = s.project_id
        WHERE s.status = 'running' AND s.pod_name IS NOT NULL
        "#,
    )
    .fetch_all(&state.pool)
    .await?;

    if running.is_empty() {
        return Ok(());
    }

    for session in running {
        let namespace = session.namespace_slug.as_deref().map_or_else(
            || state.config.agent_namespace.clone(),
            |s| state.config.project_namespace(s, "dev"),
        );
        let pods: Api<Pod> = Api::namespaced(state.kube.clone(), &namespace);
        let Some(ref pod_name) = session.pod_name else {
            continue;
        };

        match pods.get(pod_name).await {
            Ok(pod) => {
                let phase = pod
                    .status
                    .as_ref()
                    .and_then(|s| s.phase.as_deref())
                    .unwrap_or("Unknown");

                let final_status = match phase {
                    "Succeeded" => Some("completed"),
                    "Failed" => Some("failed"),
                    _ => None,
                };

                if let Some(status) = final_status {
                    // Capture logs before cleanup
                    capture_session_logs(&pods, pod_name, state, session.id).await;

                    sqlx::query!(
                        "UPDATE agent_sessions SET status = $2, finished_at = now() WHERE id = $1",
                        session.id,
                        status,
                    )
                    .execute(&state.pool)
                    .await?;

                    // Cleanup pod
                    let _ = pods.delete(pod_name, &DeleteParams::default()).await;

                    // Cleanup agent identity + Valkey ACL
                    if let Some(agent_uid) = session.agent_user_id {
                        let _ =
                            identity::cleanup_agent_identity(&state.pool, &state.valkey, agent_uid)
                                .await;
                    }
                    let _ = super::valkey_acl::delete_session_acl(&state.valkey, session.id).await;

                    // Fire webhook (only if session has a project)
                    if let Some(pid) = session.project_id {
                        fire_agent_webhook(&state.pool, pid, session.id, status).await;
                    }

                    // Notify parent (Manager) session if this is a child
                    notify_parent_of_completion(state, session.id, status).await;

                    tracing::info!(session_id = %session.id, %status, "reaped agent session");
                }
            }
            Err(kube::Error::Api(err)) if err.code == 404 => {
                // Pod disappeared — mark as failed
                sqlx::query!(
                    "UPDATE agent_sessions SET status = 'failed', finished_at = now() WHERE id = $1",
                    session.id,
                )
                .execute(&state.pool)
                .await?;

                if let Some(agent_uid) = session.agent_user_id {
                    let _ = identity::cleanup_agent_identity(&state.pool, &state.valkey, agent_uid)
                        .await;
                }
                let _ = super::valkey_acl::delete_session_acl(&state.valkey, session.id).await;

                if let Some(pid) = session.project_id {
                    fire_agent_webhook(&state.pool, pid, session.id, "failed").await;
                }

                // Notify parent (Manager) session if this is a child
                notify_parent_of_completion(state, session.id, "failed").await;

                tracing::warn!(session_id = %session.id, "agent pod disappeared, marking failed");
            }
            Err(e) => {
                tracing::error!(error = %e, session_id = %session.id, "error checking agent pod");
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve the K8s namespace for a session based on its project's `namespace_slug`.
/// Falls back to `fallback_namespace` for sessions without a project.
async fn resolve_session_namespace(
    pool: &PgPool,
    session: &AgentSession,
    fallback_namespace: &str,
    config: &crate::config::Config,
) -> Result<String, AgentError> {
    if let Some(project_id) = session.project_id {
        let slug = sqlx::query_scalar!(
            "SELECT namespace_slug FROM projects WHERE id = $1 AND is_active = true",
            project_id,
        )
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| AgentError::Other(anyhow::anyhow!("project not found")))?;
        Ok(config.project_namespace(&slug, "dev"))
    } else {
        Ok(fallback_namespace.to_string())
    }
}

/// Look up a project's HTTP clone URL and optional custom agent image.
///
/// Returns an HTTP URL in the format `{platform_api_url}/{owner}/{project}.git`
/// so that agent and pipeline pods can clone via the platform's smart HTTP git
/// server using a scoped API token.
async fn get_project_repo_info(
    pool: &PgPool,
    project_id: Uuid,
    platform_api_url: &str,
) -> Result<(String, Option<String>), AgentError> {
    let project = sqlx::query!(
        r#"SELECT p.name as "name!: String",
                  u.name as "owner_name!: String",
                  p.agent_image
           FROM projects p
           JOIN users u ON u.id = p.owner_id
           WHERE p.id = $1 AND p.is_active = true"#,
        project_id,
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AgentError::Other(anyhow::anyhow!("project not found")))?;

    let repo_clone_url = format!(
        "{}/{}/{}.git",
        platform_api_url.trim_end_matches('/'),
        project.owner_name,
        project.name
    );
    Ok((repo_clone_url, project.agent_image))
}

/// Fetch a session by ID from the database.
pub async fn fetch_session(pool: &PgPool, session_id: Uuid) -> Result<AgentSession, AgentError> {
    let row = sqlx::query!(
        r#"
        SELECT id, project_id, user_id, agent_user_id, prompt, status,
               branch, pod_name, provider, provider_config,
               cost_tokens, created_at, finished_at,
               parent_session_id, spawn_depth, allowed_child_roles,
               execution_mode, uses_pubsub
        FROM agent_sessions WHERE id = $1
        "#,
        session_id,
    )
    .fetch_optional(pool)
    .await?
    .ok_or(AgentError::SessionNotFound)?;

    Ok(AgentSession {
        id: row.id,
        project_id: row.project_id,
        user_id: row.user_id,
        agent_user_id: row.agent_user_id,
        prompt: row.prompt,
        status: row.status,
        branch: row.branch,
        pod_name: row.pod_name,
        provider: row.provider,
        provider_config: row.provider_config,
        cost_tokens: row.cost_tokens,
        created_at: row.created_at,
        finished_at: row.finished_at,
        parent_session_id: row.parent_session_id,
        spawn_depth: row.spawn_depth,
        allowed_child_roles: row.allowed_child_roles,
        execution_mode: row.execution_mode,
        uses_pubsub: row.uses_pubsub,
    })
}

/// Create a global (project-less) CLI subprocess session for app scaffolding.
///
/// Uses `claude -p` with `--json-schema` structured output and `--tools ""`
/// to control tool execution server-side. The session runs as a CLI subprocess,
/// not a K8s pod.
pub async fn create_global_session(
    state: &AppState,
    user_id: Uuid,
    prompt: &str,
    provider_name: &str,
) -> Result<AgentSession, AgentError> {
    let _ = get_provider(provider_name)?;

    // Resolve auth: CLI OAuth token > user API key > global platform secret
    let cli_oauth_token = resolve_cli_oauth_token(state, user_id).await;
    let user_api_key = if cli_oauth_token.is_some() {
        None
    } else {
        match resolve_user_api_key(state, user_id).await {
            Some(key) => Some(key),
            None => resolve_global_api_key(state).await,
        }
    };

    if cli_oauth_token.is_none() && user_api_key.is_none() {
        return Err(AgentError::ConfigurationRequired(
            "No Anthropic API key configured. Set your key in Settings > Provider Keys, or ask an admin to set a global ANTHROPIC_API_KEY secret.".into(),
        ));
    }

    let session_id = Uuid::new_v4();

    // Insert DB row as 'running' with cli_subprocess mode
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, provider, status, execution_mode, uses_pubsub) \
         VALUES ($1, $2, $3, $4, 'running', 'cli_subprocess', true)",
    )
    .bind(session_id)
    .bind(user_id)
    .bind(prompt)
    .bind(provider_name)
    .execute(&state.pool)
    .await?;

    // Save first user message to DB
    sqlx::query("INSERT INTO agent_messages (session_id, role, content) VALUES ($1, 'user', $2)")
        .bind(session_id)
        .bind(prompt)
        .execute(&state.pool)
        .await?;

    // Register in CLI session manager
    let handle = state
        .cli_sessions
        .register(
            session_id,
            user_id,
            super::claude_cli::session::SessionMode::Persistent,
        )
        .await
        .map_err(|e| AgentError::Other(e.into()))?;

    // Start persistence subscriber (writes pub/sub events to agent_messages)
    super::pubsub_bridge::spawn_persistence_subscriber(
        state.pool.clone(),
        state.valkey.clone(),
        session_id,
    )
    .await;

    // Spawn the create-app tool loop (skip when CLI spawn is disabled, e.g. integration tests)
    if state.config.cli_spawn_enabled {
        let state_clone = state.clone();
        let prompt_owned = prompt.to_owned();
        let oauth = cli_oauth_token.clone();
        let api_key = user_api_key.clone();
        tokio::spawn(async move {
            if let Err(e) = super::create_app::run_create_app_loop(
                &state_clone,
                handle,
                prompt_owned,
                oauth,
                api_key,
            )
            .await
            {
                tracing::error!(error = %e, %session_id, "create-app loop failed");
            }
        });
    } else {
        tracing::debug!(%session_id, "CLI spawn disabled, skipping create-app tool loop");
    }

    fetch_session(&state.pool, session_id).await
}

/// Capture agent pod logs to `MinIO` for post-session review.
async fn capture_session_logs(pods: &Api<Pod>, pod_name: &str, state: &AppState, session_id: Uuid) {
    let log_params = LogParams {
        container: Some("claude".into()),
        ..Default::default()
    };

    match pods.logs(pod_name, &log_params).await {
        Ok(logs) => {
            let path = format!("logs/agents/{session_id}/output.log");
            if let Err(e) = state.minio.write(&path, logs.into_bytes()).await {
                tracing::error!(error = %e, %path, "failed to write agent logs to MinIO");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, pod = pod_name, "failed to read agent pod logs");
        }
    }
}

async fn fire_agent_webhook(pool: &PgPool, project_id: Uuid, session_id: Uuid, status: &str) {
    let payload = serde_json::json!({
        "action": status,
        "session_id": session_id,
        "project_id": project_id,
    });
    crate::api::webhooks::fire_webhooks(pool, project_id, "agent", &payload).await;
}

/// Notify the parent session (Manager Agent) when a child session completes or fails.
/// Publishes a `Milestone` event to the parent's events channel so the Manager sees it
/// in its event stream (and the persistence subscriber saves it to `agent_messages`).
async fn notify_parent_of_completion(state: &AppState, child_session_id: Uuid, child_status: &str) {
    // Look up parent_session_id for the completed child
    let Ok(Some(Some(parent_id))) = sqlx::query_scalar!(
        "SELECT parent_session_id FROM agent_sessions WHERE id = $1",
        child_session_id,
    )
    .fetch_optional(&state.pool)
    .await
    else {
        return; // No parent or query failed — nothing to notify
    };

    let event = ProgressEvent {
        kind: ProgressKind::Milestone,
        message: format!(
            "Child agent session {child_session_id} finished with status: {child_status}"
        ),
        metadata: Some(serde_json::json!({
            "event_type": "child_completion",
            "child_session_id": child_session_id,
            "child_status": child_status,
        })),
    };

    if let Err(e) = super::pubsub_bridge::publish_event(&state.valkey, parent_id, &event).await {
        tracing::warn!(
            error = %e,
            %child_session_id,
            %parent_id,
            "failed to notify parent of child completion"
        );
    }
}

/// Resolve project secrets scoped to agent/all for injection into agent pods.
/// Returns an empty vec on error or if no secrets engine is configured.
async fn resolve_agent_secrets(state: &AppState, project_id: Uuid) -> Vec<(String, String)> {
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
        &["agent", "all"],
        None,
    )
    .await
    {
        Ok(secrets) => secrets,
        Err(e) => {
            tracing::warn!(error = %e, %project_id, "failed to resolve agent secrets");
            Vec::new()
        }
    }
}

/// Try to resolve the user's CLI OAuth token from `cli_credentials`.
/// Returns `None` if no credentials are stored or if the secrets engine isn't configured.
async fn resolve_cli_oauth_token(state: &AppState, user_id: Uuid) -> Option<String> {
    let master_key_hex = state.config.master_key.as_deref()?;
    let master_key = crate::secrets::engine::parse_master_key(master_key_hex).ok()?;
    match crate::auth::cli_creds::resolve_cli_auth(&state.pool, &master_key, user_id).await {
        Ok(token) => {
            if token.is_none() {
                tracing::debug!(%user_id, "no CLI credentials stored");
            }
            token
        }
        Err(e) => {
            tracing::error!(error = %e, %user_id, "failed to decrypt CLI credentials");
            None
        }
    }
}

/// Try to resolve the user's Anthropic API key from `user_provider_keys`.
/// Returns `None` if the user hasn't set one or if the secrets engine isn't configured.
pub(crate) async fn resolve_user_api_key(state: &AppState, user_id: Uuid) -> Option<String> {
    let master_key_hex = state.config.master_key.as_deref()?;
    let master_key = crate::secrets::engine::parse_master_key(master_key_hex).ok()?;
    match user_keys::get_user_key(&state.pool, &master_key, user_id, "anthropic").await {
        Ok(key) => key,
        Err(e) => {
            tracing::warn!(error = %e, %user_id, "failed to resolve user API key, falling back to global");
            None
        }
    }
}

/// Try to resolve a global `ANTHROPIC_API_KEY` from the platform secrets engine.
/// Falls back to `None` if no global secret is configured or the secrets engine
/// is unavailable.
pub(crate) async fn resolve_global_api_key(state: &AppState) -> Option<String> {
    let master_key_hex = state.config.master_key.as_deref()?;
    let master_key = crate::secrets::engine::parse_master_key(master_key_hex).ok()?;
    match crate::secrets::engine::resolve_global_secret(
        &state.pool,
        &master_key,
        "ANTHROPIC_API_KEY",
        "agent",
    )
    .await
    {
        Ok(value) => Some(value),
        Err(e) => {
            tracing::debug!(error = %e, "no global ANTHROPIC_API_KEY secret found");
            None
        }
    }
}

/// Send a message to a CLI subprocess session.
///
/// Queues the message in `pending_messages`. If the tool loop is not busy,
/// spawns a new `--resume` round to process it.
async fn send_cli_message(
    state: &AppState,
    session_id: Uuid,
    content: &str,
) -> Result<(), AgentError> {
    let handle = state
        .cli_sessions
        .get(session_id)
        .await
        .ok_or(AgentError::SessionNotRunning)?;

    // Queue the message
    handle
        .pending_messages
        .lock()
        .await
        .push(content.to_owned());

    // Store in agent_messages
    sqlx::query!(
        "INSERT INTO agent_messages (session_id, role, content) VALUES ($1, 'user', $2)",
        session_id,
        content,
    )
    .execute(&state.pool)
    .await?;

    // If no tool loop is running, spawn a new --resume round
    if !handle.is_busy() {
        let state_clone = state.clone();
        let handle_clone = handle.clone();
        let oauth = resolve_cli_oauth_token(state, handle.user_id).await;
        let api_key = if oauth.is_some() {
            None
        } else {
            resolve_user_api_key(state, handle.user_id).await
        };
        tokio::spawn(async move {
            super::create_app::run_pending_messages(&state_clone, handle_clone, oauth, api_key)
                .await;
        });
    }
    // If busy, the tool loop will drain pending_messages after the current round

    Ok(())
}

/// Stop a CLI subprocess session: set cancelled flag and remove from manager.
async fn stop_cli_session(state: &AppState, session_id: Uuid) {
    if let Some(handle) = state.cli_sessions.get(session_id).await {
        handle
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    state.cli_sessions.remove(session_id).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_provider_claude_code_succeeds() {
        let provider = get_provider("claude-code");
        assert!(provider.is_ok());
    }

    #[test]
    fn get_provider_unknown_returns_error() {
        let result = get_provider("unknown-provider");
        assert!(result.is_err());
        let err = result.err().unwrap();
        match err {
            AgentError::InvalidProvider(msg) => {
                assert!(msg.contains("unknown"), "expected 'unknown' in: {msg}");
            }
            other => panic!("expected InvalidProvider, got: {other:?}"),
        }
    }

    #[test]
    fn get_provider_empty_string_returns_error() {
        let result = get_provider("");
        assert!(result.is_err());
    }

    #[test]
    fn get_provider_case_sensitive() {
        // "Claude-Code" should fail — only exact "claude-code" works
        assert!(get_provider("Claude-Code").is_err());
        assert!(get_provider("CLAUDE-CODE").is_err());
    }

    #[test]
    fn get_provider_similar_names_rejected() {
        assert!(get_provider("claude").is_err());
        assert!(get_provider("claude-code-v2").is_err());
        assert!(get_provider("openai").is_err());
    }

    #[test]
    fn get_provider_error_includes_provider_name() {
        match get_provider("my-custom-provider") {
            Err(AgentError::InvalidProvider(msg)) => {
                assert!(
                    msg.contains("my-custom-provider"),
                    "error should include the attempted name: {msg}"
                );
            }
            Err(other) => panic!("expected InvalidProvider, got: {other}"),
            Ok(_) => panic!("expected error for unknown provider"),
        }
    }
}
