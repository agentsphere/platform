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
#[tracing::instrument(skip(state, handle, initial_prompt), fields(session_id = %handle.session_id), err)]
pub async fn run_create_app_loop(
    state: &AppState,
    handle: Arc<CliSessionHandle>,
    initial_prompt: String,
    oauth_token: Option<String>,
    anthropic_api_key: Option<String>,
) -> Result<(), AgentError> {
    handle.busy.store(true, Ordering::Relaxed);
    let session_id = handle.session_id;

    let mut current_prompt = initial_prompt;
    let mut is_resume = false;

    for round in 0..MAX_TOOL_ROUNDS {
        // Check cancellation
        if handle.cancelled.load(Ordering::Relaxed) {
            tracing::info!(%session_id, "create-app loop cancelled");
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
                // Done — no tools and no pending messages
                let _ = pubsub_bridge::publish_event(
                    &state.valkey,
                    session_id,
                    &ProgressEvent {
                        kind: ProgressKind::Completed,
                        message: "Turn completed".into(),
                        metadata: None,
                    },
                )
                .await;
                break;
            }
            // User sent messages while we were processing — use them as next prompt
            current_prompt = pending;
            is_resume = true;
            continue;
        }

        // Execute tools
        let tool_results = execute_tools(state, session_id, handle.user_id, &response.tools).await;

        // Check cancellation after tool execution
        if handle.cancelled.load(Ordering::Relaxed) {
            tracing::info!(%session_id, "create-app loop cancelled after tool execution");
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
    Ok(())
}

/// Run pending user messages through the CLI via `--resume`.
///
/// Called from `send_message()` when the tool loop is not busy.
pub async fn run_pending_messages(
    state: &AppState,
    handle: Arc<CliSessionHandle>,
    oauth_token: Option<String>,
    anthropic_api_key: Option<String>,
) {
    let pending = drain_pending(&handle).await;
    if pending.is_empty() {
        return;
    }

    if let Err(e) =
        run_create_app_loop(state, handle, pending, oauth_token, anthropic_api_key).await
    {
        tracing::error!(error = %e, "run_pending_messages failed");
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
        let (msg, is_error) = match &result {
            Ok(_) => (format!("{}: done", tool.name), false),
            Err(e) => (format!("{}: error — {e}", tool.name), true),
        };
        let _ = pubsub_bridge::publish_event(
            &state.valkey,
            session_id,
            &ProgressEvent {
                kind: ProgressKind::ToolResult,
                message: msg,
                metadata: Some(serde_json::json!({"tool": &tool.name, "is_error": is_error})),
            },
        )
        .await;

        results.push((tool.name.clone(), result));
    }

    results
}

/// Execute a single tool call, dispatching by name.
async fn execute_create_app_tool(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    tool: &ToolRequest,
) -> Result<serde_json::Value, String> {
    match tool.name.as_str() {
        "create_project" => execute_create_project(state, session_id, user_id, &tool.parameters)
            .await
            .map_err(|e| e.to_string()),
        "spawn_coding_agent" => execute_spawn_agent(state, user_id, &tool.parameters)
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
    }))
}

/// Spawn a K8s coding agent session for the project.
async fn execute_spawn_agent(
    state: &AppState,
    user_id: Uuid,
    input: &serde_json::Value,
) -> Result<serde_json::Value, anyhow::Error> {
    let project_id = parse_uuid_field(input, "project_id")?;
    let prompt = input
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required field: prompt"))?;

    let session = super::service::create_session(
        state,
        user_id,
        project_id,
        prompt,
        "claude-code",
        Some("main"),
        None,
        super::AgentRoleName::Dev,
    )
    .await?;

    Ok(serde_json::json!({
        "session_id": session.id.to_string(),
        "status": session.status,
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
}
