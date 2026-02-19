use std::time::Duration;

use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use kube::api::{AttachParams, DeleteParams, LogParams, PostParams};
use sqlx::PgPool;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use uuid::Uuid;

use crate::store::AppState;

use super::claude_code::ClaudeCodeProvider;
use super::error::AgentError;
use super::identity;
use super::provider::{AgentProvider, AgentSession, ProviderConfig};

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
#[tracing::instrument(skip(state, prompt, provider_config), fields(%user_id, %project_id), err)]
pub async fn create_session(
    state: &AppState,
    user_id: Uuid,
    project_id: Uuid,
    prompt: &str,
    provider_name: &str,
    branch: Option<&str>,
    provider_config: Option<serde_json::Value>,
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

    // 2. Create ephemeral agent identity with delegated permissions
    let agent_identity = identity::create_agent_identity(
        &state.pool,
        &state.valkey,
        session_id,
        user_id,
        project_id,
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

    // 3. Get repo clone URL for the project
    let project = sqlx::query!(
        "SELECT repo_path FROM projects WHERE id = $1 AND is_active = true",
        project_id,
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| AgentError::Other(anyhow::anyhow!("project not found")))?;

    let repo_path = project
        .repo_path
        .ok_or_else(|| AgentError::Other(anyhow::anyhow!("project has no repo path")))?;
    let repo_clone_url = format!("file://{repo_path}");

    // 4. Build and create the K8s pod
    let namespace = &state.config.agent_namespace;
    let platform_api_url = format!("http://platform.{namespace}.svc.cluster.local:8080");

    let session_for_pod = AgentSession {
        id: session_id,
        project_id,
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
    };

    let pod = provider.build_pod(
        &session_for_pod,
        &config,
        &agent_identity.api_token,
        &platform_api_url,
        &repo_clone_url,
        namespace,
    )?;

    let pod_name = pod
        .metadata
        .name
        .clone()
        .unwrap_or_else(|| format!("agent-{short_id}"));
    let pods: Api<Pod> = Api::namespaced(state.kube.clone(), namespace);
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

    let pod_name = session
        .pod_name
        .as_deref()
        .ok_or(AgentError::SessionNotRunning)?;

    let namespace = &state.config.agent_namespace;
    let pods: Api<Pod> = Api::namespaced(state.kube.clone(), namespace);

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

    // Delete pod if it exists
    if let Some(ref pod_name) = session.pod_name {
        let namespace = &state.config.agent_namespace;
        let pods: Api<Pod> = Api::namespaced(state.kube.clone(), namespace);
        // Capture logs before deleting the pod
        capture_session_logs(&pods, pod_name, state, session_id).await;
        let _ = pods.delete(pod_name, &DeleteParams::default()).await;
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

    let namespace = &state.config.agent_namespace;
    let pods: Api<Pod> = Api::namespaced(state.kube.clone(), namespace);

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
        SELECT id, pod_name, agent_user_id, project_id
        FROM agent_sessions
        WHERE status = 'running' AND pod_name IS NOT NULL
        "#,
    )
    .fetch_all(&state.pool)
    .await?;

    if running.is_empty() {
        return Ok(());
    }

    let namespace = &state.config.agent_namespace;
    let pods: Api<Pod> = Api::namespaced(state.kube.clone(), namespace);

    for session in running {
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

                    // Fire webhook
                    fire_agent_webhook(&state.pool, session.project_id, session.id, status).await;

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

                fire_agent_webhook(&state.pool, session.project_id, session.id, "failed").await;
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

/// Fetch a session by ID from the database.
pub async fn fetch_session(pool: &PgPool, session_id: Uuid) -> Result<AgentSession, AgentError> {
    let row = sqlx::query!(
        r#"
        SELECT id, project_id, user_id, agent_user_id, prompt, status,
               branch, pod_name, provider, provider_config,
               cost_tokens, created_at, finished_at
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
    })
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
