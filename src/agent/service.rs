use std::time::Duration;

use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use kube::api::{AttachParams, DeleteParams, LogParams, PostParams};
use sqlx::PgPool;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use uuid::Uuid;

use crate::secrets::user_keys;
use crate::store::AppState;

use super::AgentRoleName;
use super::claude_code::ClaudeCodeProvider;
use super::error::AgentError;
use super::identity;
use super::provider::{AgentProvider, AgentSession, BuildPodParams, ProviderConfig};

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
#[allow(clippy::too_many_arguments)]
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

    sqlx::query!(
        r#"
        INSERT INTO agent_sessions (id, project_id, user_id, prompt, provider, provider_config, branch, status)
        VALUES ($1, $2, $3, $4, $5, $6, $7, 'pending')
        "#,
        session_id,
        project_id,
        user_id,
        prompt,
        provider_name,
        provider_config.as_ref(),
        branch_name,
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
    let namespace = format!("{}-dev", project_info.namespace_slug);

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

    // 4. Look up user's provider key (if set)
    let user_api_key = resolve_user_api_key(state, user_id).await;

    // 5. Build and create the K8s pod
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
    };

    let pod = provider.build_pod(BuildPodParams {
        session: &session_for_pod,
        config: &config,
        agent_api_token: &agent_identity.api_token,
        platform_api_url,
        repo_clone_url: &repo_clone_url,
        namespace: &namespace,
        project_agent_image: project_agent_image.as_deref(),
        anthropic_api_key: user_api_key.as_deref(),
    })?;

    let pod_name = pod
        .metadata
        .name
        .clone()
        .unwrap_or_else(|| format!("agent-{short_id}"));
    let pods: Api<Pod> = Api::namespaced(state.kube.clone(), &namespace);
    pods.create(&PostParams::default(), &pod)
        .await
        .map_err(|e| AgentError::PodCreationFailed(e.to_string()))?;

    // 5. Update session to running with pod_name
    sqlx::query!(
        "UPDATE agent_sessions SET status = 'running', pod_name = $2 WHERE id = $1",
        session_id,
        pod_name,
    )
    .execute(&state.pool)
    .await?;

    // 6. Return the complete session
    fetch_session(&state.pool, session_id).await
}

/// Send a message to a running agent session by attaching to the pod's stdin.
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

    // In-process sessions (no pod) use the inprocess module
    if session.pod_name.is_none() {
        return super::inprocess::send_inprocess_message(state, session_id, content).await;
    }

    let pod_name = session.pod_name.as_deref().unwrap();
    let namespace =
        resolve_session_namespace(&state.pool, &session, &state.config.agent_namespace).await?;
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

    if let Some(ref pod_name) = session.pod_name {
        // K8s pod session — capture logs and delete pod
        let namespace =
            resolve_session_namespace(&state.pool, &session, &state.config.agent_namespace).await?;
        let pods: Api<Pod> = Api::namespaced(state.kube.clone(), &namespace);
        capture_session_logs(&pods, pod_name, state, session_id).await;
        let _ = pods.delete(pod_name, &DeleteParams::default()).await;
    } else {
        // In-process session — remove handle from memory
        super::inprocess::remove_session(state, session_id);
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

    Ok(())
}

/// Get a log stream from the agent pod for WebSocket streaming.
/// Returns a `Lines` reader over the pod's stdout for line-by-line reading.
#[tracing::instrument(skip(state), fields(%session_id), err)]
pub async fn get_log_lines(
    state: &AppState,
    session_id: Uuid,
) -> Result<tokio::io::Lines<tokio::io::BufReader<impl tokio::io::AsyncRead>>, AgentError> {
    let session = fetch_session(&state.pool, session_id).await?;

    let pod_name = session
        .pod_name
        .as_deref()
        .ok_or(AgentError::SessionNotRunning)?;

    let namespace =
        resolve_session_namespace(&state.pool, &session, &state.config.agent_namespace).await?;
    let pods: Api<Pod> = Api::namespaced(state.kube.clone(), &namespace);

    let log_stream = pods
        .log_stream(
            pod_name,
            &LogParams {
                container: Some("claude".into()),
                follow: true,
                ..Default::default()
            },
        )
        .await?;

    // kube v3 log_stream returns impl futures_util::AsyncBufRead.
    // Convert to tokio AsyncRead via the compat layer, then wrap for lines().
    use tokio_util::compat::FuturesAsyncReadCompatExt;
    let compat_reader = log_stream.compat();
    Ok(tokio::io::BufReader::new(compat_reader).lines())
}

// ---------------------------------------------------------------------------
// Background reaper
// ---------------------------------------------------------------------------

/// Background task that periodically checks for terminated agent pods and
/// finalizes their sessions.
pub async fn run_reaper(state: AppState, mut shutdown: tokio::sync::watch::Receiver<()>) {
    tracing::info!("agent session reaper started");

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                tracing::info!("agent session reaper shutting down");
                break;
            }
            () = tokio::time::sleep(Duration::from_secs(30)) => {
                if let Err(e) = reap_terminated_sessions(&state).await {
                    tracing::error!(error = %e, "error reaping agent sessions");
                }
            }
        }
    }
}

/// Find running sessions whose pods have terminated and finalize them.
async fn reap_terminated_sessions(state: &AppState) -> Result<(), AgentError> {
    let running = sqlx::query!(
        r#"
        SELECT s.id, s.pod_name, s.agent_user_id, s.project_id,
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
            |s| format!("{s}-dev"),
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

                    // Cleanup agent identity
                    if let Some(agent_uid) = session.agent_user_id {
                        let _ =
                            identity::cleanup_agent_identity(&state.pool, &state.valkey, agent_uid)
                                .await;
                    }

                    // Fire webhook (only if session has a project)
                    if let Some(pid) = session.project_id {
                        fire_agent_webhook(&state.pool, pid, session.id, status).await;
                    }

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

                if let Some(pid) = session.project_id {
                    fire_agent_webhook(&state.pool, pid, session.id, "failed").await;
                }
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
) -> Result<String, AgentError> {
    if let Some(project_id) = session.project_id {
        let slug = sqlx::query_scalar!(
            "SELECT namespace_slug FROM projects WHERE id = $1 AND is_active = true",
            project_id,
        )
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| AgentError::Other(anyhow::anyhow!("project not found")))?;
        Ok(format!("{slug}-dev"))
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
               parent_session_id, spawn_depth, allowed_child_roles
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
    })
}

/// Create a global (project-less) in-process agent session for app scaffolding.
/// The session runs in-process (no K8s pod) using the Anthropic Messages API.
pub async fn create_global_session(
    state: &AppState,
    user_id: Uuid,
    prompt: &str,
    provider_name: &str,
) -> Result<AgentSession, AgentError> {
    let session_id =
        super::inprocess::create_inprocess_session(state, user_id, prompt, provider_name, None)
            .await?;

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
