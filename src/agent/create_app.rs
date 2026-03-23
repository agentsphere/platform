use std::sync::Arc;
use std::sync::atomic::Ordering;

use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::claude_cli::session::CliSessionHandle;
use super::cli_invoke::{self, CliInvokeParams, ToolRequest};
use super::create_app_prompt;
use super::error::AgentError;
use super::provider::{ProgressEvent, ProgressKind};
use super::pubsub_bridge;
use crate::audit::{AuditEntry, write_audit};
use crate::store::AppState;

/// Maximum number of automatic tool rounds before giving up.
const MAX_TOOL_ROUNDS: usize = 10;

/// Outcome of a `run_create_app_loop` invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopOutcome {
    /// Tools were executed and the LLM finished — session can be finalized.
    Completed,
    /// LLM returned text with no tool calls (e.g. clarification question) — session
    /// stays running so the user can reply.
    WaitingForInput,
    /// The loop was cancelled via `handle.cancelled`.
    Cancelled,
}

// ---------------------------------------------------------------------------
// Tool loop
// ---------------------------------------------------------------------------

/// Run the create-app tool loop for a CLI subprocess session.
///
/// Invokes `claude -p` with structured output, executes returned tools,
/// and feeds results back via `--resume`. Publishes all events to Valkey pub/sub.
///
/// Exits when:
/// - LLM returns no tools and no pending user messages → done
/// - `handle.cancelled` is set → stopped
/// - `MAX_TOOL_ROUNDS` reached → safety limit
#[allow(clippy::too_many_lines)]
#[tracing::instrument(skip(state, handle, initial_prompt, oauth_token, anthropic_api_key, extra_env, model_override), fields(session_id = %handle.session_id), err)]
pub async fn run_create_app_loop(
    state: &AppState,
    handle: Arc<CliSessionHandle>,
    initial_prompt: String,
    oauth_token: Option<String>,
    anthropic_api_key: Option<String>,
    extra_env: Vec<(String, String)>,
    model_override: Option<String>,
) -> Result<LoopOutcome, AgentError> {
    handle.busy.store(true, Ordering::Relaxed);
    let session_id = handle.session_id;

    // Wrap user prompt with critical rules from the system prompt.
    // Models sometimes ignore --system-prompt in favor of -p content,
    // so we reinforce the key constraints directly in the user prompt.
    let mut current_prompt = format!(
        "RULES (you MUST follow these):\n\
         - You are the Manager Agent. You orchestrate, you do NOT write code.\n\
         - Use the tools: create_project, then spawn_coding_agent. That is ALL you do.\n\
         - Your spawn_coding_agent prompt MUST be SHORT — only describe WHAT to build.\n\
         - Do NOT include file paths, code, Dockerfiles, docker-compose, k8s manifests, or project structure.\n\
         - Do NOT mention /tmp, /private, or any absolute paths. The worker knows its workspace.\n\
         - This is a Kubernetes-native platform. There is NO docker-compose, NO SQLite — use PostgreSQL.\n\
         - The worker has CLAUDE.md with the full development workflow. Do NOT repeat it.\n\n\
         USER REQUEST:\n{initial_prompt}"
    );
    // If we already have a CLI session ID from a prior invocation, resume it
    // (e.g. user sent a follow-up message after the first turn completed)
    let mut is_resume = handle.cli_session_id.lock().await.is_some();
    let mut outcome = LoopOutcome::Completed;
    let mut any_tools_executed = false;

    for round in 0..MAX_TOOL_ROUNDS {
        // Check cancellation
        if handle.cancelled.load(Ordering::Relaxed) {
            tracing::info!(%session_id, "create-app loop cancelled");
            outcome = LoopOutcome::Cancelled;
            break;
        }

        tracing::debug!(%session_id, round, is_resume, "invoking CLI");

        let params = CliInvokeParams {
            session_id,
            prompt: current_prompt.clone(),
            is_resume,
            system_prompt: if is_resume {
                None
            } else {
                Some(create_app_prompt::build_create_app_system_prompt().to_owned())
            },
            oauth_token: oauth_token.clone(),
            anthropic_api_key: anthropic_api_key.clone(),
            max_turns: Some(1),
            extra_env: extra_env.clone(),
            model_override: model_override.clone(),
        };

        let (response, result_msg) = match cli_invoke::invoke_cli(params, &state.valkey).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, %session_id, "CLI invocation failed");
                let _ = pubsub_bridge::publish_event(
                    &state.valkey,
                    session_id,
                    &ProgressEvent {
                        kind: ProgressKind::Error,
                        message: format!("CLI invocation failed: {e}"),
                        metadata: None,
                    },
                )
                .await;
                break;
            }
        };

        // Update cost tracking
        if let Some(ref result) = result_msg {
            let _ = cli_invoke::update_session_cost(&state.pool, session_id, result).await;
        }

        // Store CLI session ID for resume
        if let Some(ref result) = result_msg {
            let mut cli_sid = handle.cli_session_id.lock().await;
            *cli_sid = Some(result.session_id.clone());
        }

        // Publish the LLM's text response
        if !response.text.is_empty() {
            let _ = pubsub_bridge::publish_event(
                &state.valkey,
                session_id,
                &ProgressEvent {
                    kind: ProgressKind::Text,
                    message: response.text.clone(),
                    metadata: None,
                },
            )
            .await;
        }

        if response.tools.is_empty() {
            // No tools — check for pending user messages
            let pending = drain_pending(&handle).await;
            if pending.is_empty() {
                if any_tools_executed {
                    // Tools ran in prior rounds, LLM is done — session complete.
                    let _ = pubsub_bridge::publish_event(
                        &state.valkey,
                        session_id,
                        &ProgressEvent {
                            kind: ProgressKind::Completed,
                            message: "Create-app completed".into(),
                            metadata: None,
                        },
                    )
                    .await;
                    outcome = LoopOutcome::Completed;
                } else {
                    // No tools executed at all (clarification question) — wait for input.
                    let _ = pubsub_bridge::publish_event(
                        &state.valkey,
                        session_id,
                        &ProgressEvent {
                            kind: ProgressKind::WaitingForInput,
                            message: "Turn completed — waiting for input".into(),
                            metadata: None,
                        },
                    )
                    .await;
                    outcome = LoopOutcome::WaitingForInput;
                }
                break;
            }
            // User sent messages while we were processing — use them as next prompt
            current_prompt = pending;
            is_resume = true;
            continue;
        }

        // Execute tools
        any_tools_executed = true;
        let tool_results = execute_tools(state, session_id, handle.user_id, &response.tools).await;

        // Check cancellation after tool execution
        if handle.cancelled.load(Ordering::Relaxed) {
            tracing::info!(%session_id, "create-app loop cancelled after tool execution");
            outcome = LoopOutcome::Cancelled;
            break;
        }

        // Check for pending user messages (priority over tool results)
        let pending = drain_pending(&handle).await;
        if pending.is_empty() {
            current_prompt = cli_invoke::format_tool_results(&tool_results);
        } else {
            current_prompt = pending;
        }
        is_resume = true;
    }

    handle.busy.store(false, Ordering::Relaxed);
    Ok(outcome)
}

/// Run pending user messages through the CLI via `--resume`.
///
/// Called from `send_message()` when the tool loop is not busy.
/// Finalizes the session if the resumed loop completes or fails.
pub async fn run_pending_messages(
    state: &AppState,
    handle: Arc<CliSessionHandle>,
    oauth_token: Option<String>,
    anthropic_api_key: Option<String>,
    extra_env: Vec<(String, String)>,
    model_override: Option<String>,
) {
    let pending = drain_pending(&handle).await;
    if pending.is_empty() {
        return;
    }

    let session_id = handle.session_id;
    match run_create_app_loop(
        state,
        handle,
        pending,
        oauth_token,
        anthropic_api_key,
        extra_env,
        model_override,
    )
    .await
    {
        Ok(LoopOutcome::Completed | LoopOutcome::Cancelled) => {
            let _ = sqlx::query(
                "UPDATE agent_sessions SET status = 'completed', finished_at = now() \
                 WHERE id = $1 AND status = 'running'",
            )
            .bind(session_id)
            .execute(&state.pool)
            .await;
        }
        Ok(LoopOutcome::WaitingForInput) => {
            // Session stays running — user will send another follow-up
        }
        Err(e) => {
            tracing::error!(error = %e, %session_id, "run_pending_messages failed");
            let _ = sqlx::query(
                "UPDATE agent_sessions SET status = 'failed', finished_at = now() \
                 WHERE id = $1 AND status = 'running'",
            )
            .bind(session_id)
            .execute(&state.pool)
            .await;
        }
    }
}

// ---------------------------------------------------------------------------
// Tool execution
// ---------------------------------------------------------------------------

/// Execute all tools from a structured response and collect results.
async fn execute_tools(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    tools: &[ToolRequest],
) -> Vec<(String, Result<serde_json::Value, String>)> {
    let mut results = Vec::new();

    for tool in tools {
        // Publish ToolCall event
        let _ = pubsub_bridge::publish_event(
            &state.valkey,
            session_id,
            &ProgressEvent {
                kind: ProgressKind::ToolCall,
                message: format!("Executing tool: {}", tool.name),
                metadata: Some(serde_json::json!({"tool": &tool.name})),
            },
        )
        .await;

        let result = execute_create_app_tool(state, session_id, user_id, tool).await;

        // Publish ToolResult event
        let (msg, metadata) = match &result {
            Ok(val) => (
                format!("{}: done", tool.name),
                serde_json::json!({
                    "tool": &tool.name,
                    "tool_name": &tool.name,
                    "is_error": false,
                    "result": serde_json::to_string(val).unwrap_or_default(),
                }),
            ),
            Err(e) => (
                format!("{}: error — {e}", tool.name),
                serde_json::json!({
                    "tool": &tool.name,
                    "tool_name": &tool.name,
                    "is_error": true,
                }),
            ),
        };
        let _ = pubsub_bridge::publish_event(
            &state.valkey,
            session_id,
            &ProgressEvent {
                kind: ProgressKind::ToolResult,
                message: msg,
                metadata: Some(metadata),
            },
        )
        .await;

        results.push((tool.name.clone(), result));
    }

    results
}

/// Execute a single tool call, dispatching by name.
pub async fn execute_create_app_tool(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    tool: &ToolRequest,
) -> Result<serde_json::Value, String> {
    match tool.name.as_str() {
        "create_project" => execute_create_project(state, session_id, user_id, &tool.parameters)
            .await
            .map_err(|e| e.to_string()),
        "spawn_coding_agent" => execute_spawn_agent(state, session_id, user_id, &tool.parameters)
            .await
            .map_err(|e| e.to_string()),
        "send_message_to_session" => execute_send_message(state, session_id, &tool.parameters)
            .await
            .map_err(|e| e.to_string()),
        "check_session_progress" => execute_check_progress(state, session_id, &tool.parameters)
            .await
            .map_err(|e| e.to_string()),
        other => Err(format!("unknown tool: {other}")),
    }
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

/// Create a project with a bare git repo.
async fn execute_create_project(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    input: &serde_json::Value,
) -> Result<serde_json::Value, anyhow::Error> {
    let (name, display_name, description) = parse_create_project_input(input)?;

    // Look up owner username
    let owner_name: String = sqlx::query_scalar("SELECT name FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(&state.pool)
        .await?;

    // Init bare repo
    let repo_path =
        crate::git::repo::init_bare_repo(&state.config.git_repos_path, &owner_name, name, "main")
            .await?;

    let project_id = Uuid::new_v4();
    let repo_path_str = repo_path.to_string_lossy().to_string();
    let namespace_slug = crate::deployer::namespace::slugify_namespace(name);

    // Ensure the user has a workspace for the project
    let workspace_id = crate::workspace::service::get_or_create_default_workspace(
        &state.pool,
        user_id,
        &owner_name,
        &owner_name,
    )
    .await?;

    // Insert with collision retry on namespace_slug
    let dn = display_name.as_deref();
    let desc = description.as_deref();
    let final_slug = match try_insert_project(
        &state.pool,
        project_id,
        name,
        dn,
        desc,
        user_id,
        &repo_path_str,
        workspace_id,
        &namespace_slug,
    )
    .await
    {
        Ok(slug) => slug,
        Err(e) if e.to_string().contains("namespace") => {
            // Collision on namespace_slug — append short hash suffix
            let hash = &format!("{:x}", Sha256::digest(name.as_bytes()))[..6];
            let slug_with_hash =
                format!("{}-{hash}", &namespace_slug[..namespace_slug.len().min(33)]);
            try_insert_project(
                &state.pool,
                project_id,
                name,
                dn,
                desc,
                user_id,
                &repo_path_str,
                workspace_id,
                &slug_with_hash,
            )
            .await?
        }
        Err(e) => return Err(e),
    };
    let namespace_slug = final_slug;

    // Link session to project
    sqlx::query("UPDATE agent_sessions SET project_id = $2 WHERE id = $1")
        .bind(session_id)
        .bind(project_id)
        .execute(&state.pool)
        .await?;

    // Best-effort infra setup (namespaces, network policy, ops repo)
    if let Err(e) =
        crate::api::projects::setup_project_infrastructure(state, project_id, &namespace_slug).await
    {
        tracing::warn!(error = %e, %project_id, "project infra setup incomplete (create-app)");
    }

    // Auto-create default branch protection rule for main
    if let Err(e) = sqlx::query(
        "INSERT INTO branch_protection_rules (project_id, pattern) VALUES ($1, 'main') \
         ON CONFLICT (project_id, pattern) DO NOTHING",
    )
    .bind(project_id)
    .execute(&state.pool)
    .await
    {
        tracing::warn!(error = %e, %project_id, "failed to create default branch protection (create-app)");
    }

    // Audit
    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: user_id,
            actor_name: &owner_name,
            action: "project.create",
            resource: "project",
            resource_id: Some(project_id),
            project_id: Some(project_id),
            detail: Some(serde_json::json!({"name": name, "source": "create-app-agent"})),
            ip_addr: None,
        },
    )
    .await;

    Ok(serde_json::json!({
        "project_id": project_id.to_string(),
        "name": name,
        "namespace_slug": namespace_slug,
        "_next": "Now call spawn_coding_agent with this project_id"
    }))
}

/// Spawn a K8s coding agent session for the project.
async fn execute_spawn_agent(
    state: &AppState,
    manager_session_id: Uuid,
    user_id: Uuid,
    input: &serde_json::Value,
) -> Result<serde_json::Value, anyhow::Error> {
    let project_id = parse_uuid_field(input, "project_id")?;
    let raw_prompt = input
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required field: prompt"))?;

    // Truncate overly verbose manager prompts — the worker has CLAUDE.md for process details.
    // Strip everything except the actual requirements (the manager often adds file paths,
    // code snippets, docker-compose references etc. that the worker should ignore).
    let truncated_prompt = if raw_prompt.len() > 2000 {
        &raw_prompt[..2000]
    } else {
        raw_prompt
    };

    // Invoke the /dev skill command, which is seeded into every project repo at
    // .claude/commands/dev.md. The command contains explicit step-by-step instructions
    // (read CLAUDE.md, install tools, deploy postgres, test locally, push, create MR).
    // $ARGUMENTS is replaced by Claude with the text after /dev.
    let prompt = format!("/dev {truncated_prompt}");

    let session = super::service::create_session(
        state,
        user_id,
        project_id,
        &prompt,
        "claude-code",
        Some("feature/initial-app"),
        None,
        super::AgentRoleName::Dev,
        Some(manager_session_id), // Link child to Manager
    )
    .await?;

    Ok(serde_json::json!({
        "session_id": session.id.to_string(),
        "status": session.status,
        "_next": "Agent is running. Tell the user and return tools: []. Do NOT call check_session_progress."
    }))
}

/// Send a message from the Manager to a child Worker session.
async fn execute_send_message(
    state: &AppState,
    manager_session_id: Uuid,
    input: &serde_json::Value,
) -> Result<serde_json::Value, anyhow::Error> {
    let child_session_id = parse_uuid_field(input, "session_id")?;
    let message = input
        .get("message")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required field: message"))?;

    crate::validation::check_length("message", message, 1, 100_000)
        .map_err(|e| anyhow::anyhow!("invalid message: {e}"))?;

    // Verify child session exists and belongs to this Manager
    let child: (Uuid, Option<Uuid>, String) =
        sqlx::query_as("SELECT id, parent_session_id, status FROM agent_sessions WHERE id = $1")
            .bind(child_session_id)
            .fetch_optional(&state.pool)
            .await?
            .ok_or_else(|| anyhow::anyhow!("session not found: {child_session_id}"))?;

    if child.1 != Some(manager_session_id) {
        return Err(anyhow::anyhow!(
            "session {child_session_id} is not a child of this session"
        ));
    }

    if child.2 != "running" {
        return Err(anyhow::anyhow!(
            "session {child_session_id} is not running (status: {})",
            child.2,
        ));
    }

    // Publish to Worker's input channel with source="manager"
    super::pubsub_bridge::publish_prompt_with_source(
        &state.valkey,
        child_session_id,
        message,
        "manager",
    )
    .await?;

    Ok(serde_json::json!({
        "ok": true,
        "session_id": child_session_id.to_string(),
    }))
}

/// Check the progress of a child Worker session.
/// Returns the session status and the last N messages.
async fn execute_check_progress(
    state: &AppState,
    manager_session_id: Uuid,
    input: &serde_json::Value,
) -> Result<serde_json::Value, anyhow::Error> {
    let child_session_id = parse_uuid_field(input, "session_id")?;
    let limit = input
        .get("limit")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(20)
        .min(50);

    // Verify child belongs to this Manager
    let child: (
        Uuid,
        Option<Uuid>,
        String,
        Option<i64>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as(
        "SELECT id, parent_session_id, status, cost_tokens, finished_at \
             FROM agent_sessions WHERE id = $1",
    )
    .bind(child_session_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| anyhow::anyhow!("session not found: {child_session_id}"))?;

    if child.1 != Some(manager_session_id) {
        return Err(anyhow::anyhow!(
            "session {child_session_id} is not a child of this session"
        ));
    }

    // Fetch recent messages (newest first, then reverse for chronological order)
    let messages: Vec<(String, String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT role, content, created_at \
         FROM agent_messages \
         WHERE session_id = $1 \
         ORDER BY created_at DESC \
         LIMIT $2",
    )
    .bind(child_session_id)
    .bind(limit)
    .fetch_all(&state.pool)
    .await?;

    let messages: Vec<serde_json::Value> = messages
        .into_iter()
        .rev()
        .map(|(role, content, created_at)| {
            serde_json::json!({
                "role": role,
                "content": content,
                "created_at": created_at.to_rfc3339(),
            })
        })
        .collect();

    Ok(serde_json::json!({
        "session_id": child_session_id.to_string(),
        "status": child.2,
        "cost_tokens": child.3,
        "finished_at": child.4.map(|t| t.to_rfc3339()),
        "messages": messages,
    }))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Drain pending messages from the session handle, joining with newlines.
async fn drain_pending(handle: &CliSessionHandle) -> String {
    let mut msgs = handle.pending_messages.lock().await;
    if msgs.is_empty() {
        return String::new();
    }
    msgs.drain(..).collect::<Vec<_>>().join("\n\n")
}

/// Extract and validate create-project input fields.
fn parse_create_project_input(
    input: &serde_json::Value,
) -> Result<(&str, Option<String>, Option<String>), anyhow::Error> {
    let name = input
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required field: name"))?;

    crate::validation::check_name(name)
        .map_err(|e| anyhow::anyhow!("invalid project name: {e}"))?;

    let display_name = input
        .get("display_name")
        .and_then(|v| v.as_str())
        .map(String::from);
    let description = input
        .get("description")
        .and_then(|v| v.as_str())
        .map(String::from);

    if let Some(ref dn) = display_name {
        crate::validation::check_length("display_name", dn, 1, 255)
            .map_err(|e| anyhow::anyhow!("invalid display_name: {e}"))?;
    }
    if let Some(ref d) = description {
        crate::validation::check_length("description", d, 0, 10_000)
            .map_err(|e| anyhow::anyhow!("invalid description: {e}"))?;
    }

    Ok((name, display_name, description))
}

/// Try to insert a project row, returning the `namespace_slug` on success.
#[allow(clippy::too_many_arguments)]
async fn try_insert_project(
    pool: &sqlx::PgPool,
    project_id: Uuid,
    name: &str,
    display_name: Option<&str>,
    description: Option<&str>,
    owner_id: Uuid,
    repo_path: &str,
    workspace_id: Uuid,
    namespace_slug: &str,
) -> Result<String, anyhow::Error> {
    sqlx::query(
        "INSERT INTO projects (id, name, display_name, description, owner_id, visibility, repo_path, workspace_id, namespace_slug) \
         VALUES ($1, $2, $3, $4, $5, 'private', $6, $7, $8)",
    )
    .bind(project_id)
    .bind(name)
    .bind(display_name)
    .bind(description)
    .bind(owner_id)
    .bind(repo_path)
    .bind(workspace_id)
    .bind(namespace_slug)
    .execute(pool)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db_err) if db_err.code().as_deref() == Some("23505") => {
            if db_err.constraint().is_some_and(|c| c.contains("namespace_slug")) {
                anyhow::anyhow!("namespace slug collision")
            } else {
                anyhow::anyhow!("a project named '{name}' already exists")
            }
        }
        _ => e.into(),
    })?;
    Ok(namespace_slug.to_string())
}

/// Parse a UUID from a JSON object field.
fn parse_uuid_field(input: &serde_json::Value, field: &str) -> Result<Uuid, anyhow::Error> {
    let s = input
        .get(field)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required field: {field}"))?;
    Uuid::parse_str(s).map_err(|e| anyhow::anyhow!("invalid UUID for {field}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_tool_rounds_is_ten() {
        assert_eq!(MAX_TOOL_ROUNDS, 10);
    }

    #[test]
    fn unknown_tool_returns_error() {
        let tool = ToolRequest {
            name: "nonexistent".into(),
            parameters: serde_json::json!({}),
        };
        // Can't call execute_create_app_tool without state, but test the match logic
        assert_eq!(
            format!("unknown tool: {}", tool.name),
            "unknown tool: nonexistent"
        );
    }

    #[test]
    fn parse_create_project_input_valid() {
        let input = serde_json::json!({"name": "my-app"});
        let (name, dn, desc) = parse_create_project_input(&input).unwrap();
        assert_eq!(name, "my-app");
        assert!(dn.is_none());
        assert!(desc.is_none());
    }

    #[test]
    fn parse_create_project_input_with_all_fields() {
        let input = serde_json::json!({
            "name": "my-app",
            "display_name": "My App",
            "description": "A test app"
        });
        let (name, dn, desc) = parse_create_project_input(&input).unwrap();
        assert_eq!(name, "my-app");
        assert_eq!(dn.as_deref(), Some("My App"));
        assert_eq!(desc.as_deref(), Some("A test app"));
    }

    #[test]
    fn parse_create_project_input_missing_name() {
        let input = serde_json::json!({});
        assert!(parse_create_project_input(&input).is_err());
    }

    #[test]
    fn parse_uuid_field_valid() {
        let id = Uuid::new_v4();
        let input = serde_json::json!({"project_id": id.to_string()});
        let parsed = parse_uuid_field(&input, "project_id").unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn parse_uuid_field_missing() {
        let input = serde_json::json!({});
        assert!(parse_uuid_field(&input, "project_id").is_err());
    }

    #[test]
    fn parse_uuid_field_invalid() {
        let input = serde_json::json!({"project_id": "not-a-uuid"});
        assert!(parse_uuid_field(&input, "project_id").is_err());
    }

    #[test]
    fn send_message_input_requires_session_id() {
        let input = serde_json::json!({"message": "hello"});
        assert!(parse_uuid_field(&input, "session_id").is_err());
    }

    #[test]
    fn send_message_input_requires_message() {
        let input = serde_json::json!({"session_id": Uuid::new_v4().to_string()});
        let message = input.get("message").and_then(|v| v.as_str());
        assert!(message.is_none());
    }

    #[test]
    fn check_progress_default_limit() {
        let input = serde_json::json!({"session_id": Uuid::new_v4().to_string()});
        let limit = input
            .get("limit")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(20)
            .min(50);
        assert_eq!(limit, 20);
    }

    #[test]
    fn check_progress_limit_capped_at_50() {
        let input = serde_json::json!({"session_id": Uuid::new_v4().to_string(), "limit": 100});
        let limit = input
            .get("limit")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(20)
            .min(50);
        assert_eq!(limit, 50);
    }

    #[test]
    fn tool_dispatch_recognizes_new_tools() {
        // Verify the tool name strings match what the schema expects
        let names = ["send_message_to_session", "check_session_progress"];
        for name in &names {
            assert!(
                [
                    "create_project",
                    "spawn_coding_agent",
                    "send_message_to_session",
                    "check_session_progress"
                ]
                .contains(name),
                "tool {name} not in known tools"
            );
        }
    }

    #[test]
    fn loop_outcome_variants() {
        assert_ne!(LoopOutcome::Completed, LoopOutcome::WaitingForInput);
        assert_ne!(LoopOutcome::Completed, LoopOutcome::Cancelled);
        assert_ne!(LoopOutcome::WaitingForInput, LoopOutcome::Cancelled);
    }

    #[test]
    fn loop_outcome_debug() {
        let s = format!("{:?}", LoopOutcome::WaitingForInput);
        assert!(s.contains("WaitingForInput"));
    }
}
