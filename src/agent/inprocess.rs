use std::sync::Arc;

use tokio::sync::{RwLock, broadcast};
use uuid::Uuid;

use super::anthropic::{self, ChatMessage, TurnResult};
use super::error::AgentError;
use super::provider::{ProgressEvent, ProgressKind};
use crate::audit::{AuditEntry, write_audit};
use crate::store::AppState;

const BROADCAST_CAPACITY: usize = 256;
const MAX_TOOL_ROUNDS: usize = 5;

const CREATE_APP_SYSTEM_PROMPT: &str = r#"You are an app-creation assistant for the Platform developer tool. Your job is to help users go from an idea to a running project in two phases.

== PHASE 1: CLARIFY ==
Ask 1-2 concise clarifying questions about the tech stack, framework, database, and deployment needs. When the user confirms the plan, move to Phase 2. Do NOT call any tools during this phase.

== PHASE 2: EXECUTE ==
Once the user confirms, execute these steps using your tools:

1. Call `create_project` with a slug-style name (lowercase, hyphens, e.g. "my-blog-api").
2. Call `create_ops_repo` with the project_id from step 1.
3. Call `spawn_coding_agent` with the project_id and a detailed prompt summarizing every requirement the user confirmed (language, framework, database, features, structure).

After all tools succeed, reply with a short confirmation and tell the user their project is being set up.

Keep all responses concise. Never ask more than two questions at a time."#;

/// Handle for an in-process agent session.
/// Holds the broadcast channel, conversation history, and user context.
#[derive(Clone)]
pub struct InProcessHandle {
    pub tx: broadcast::Sender<ProgressEvent>,
    pub messages: Arc<RwLock<Vec<ChatMessage>>>,
    pub api_key: String,
    pub model: Option<String>,
    pub user_id: Uuid,
}

impl InProcessHandle {
    pub fn new(api_key: String, model: Option<String>, user_id: Uuid) -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            tx,
            messages: Arc::new(RwLock::new(Vec::new())),
            api_key,
            model,
            user_id,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ProgressEvent> {
        self.tx.subscribe()
    }
}

// ---------------------------------------------------------------------------
// Tool definitions
// ---------------------------------------------------------------------------

/// Returns the tool definitions for the create-app agent.
pub fn create_app_tools() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "name": "create_project",
            "description": "Create a new project with a bare git repository. Returns the project ID and name.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Slug-style project name (lowercase, hyphens allowed, e.g. 'my-blog-api')"
                    },
                    "display_name": {
                        "type": "string",
                        "description": "Human-readable project name (optional)"
                    },
                    "description": {
                        "type": "string",
                        "description": "Short project description (optional)"
                    }
                },
                "required": ["name"]
            }
        }),
        serde_json::json!({
            "name": "create_ops_repo",
            "description": "Create an ops repo (deployment manifests) for a project. Returns the ops repo ID.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "project_id": {
                        "type": "string",
                        "description": "UUID of the project to create the ops repo for"
                    },
                    "name": {
                        "type": "string",
                        "description": "Ops repo name (optional, defaults to '<project-name>-ops')"
                    }
                },
                "required": ["project_id"]
            }
        }),
        serde_json::json!({
            "name": "spawn_coding_agent",
            "description": "Spawn a coding agent session to scaffold and write code for the project.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "project_id": {
                        "type": "string",
                        "description": "UUID of the project"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Detailed coding prompt with all requirements the user confirmed"
                    }
                },
                "required": ["project_id", "prompt"]
            }
        }),
    ]
}

// ---------------------------------------------------------------------------
// Session lifecycle
// ---------------------------------------------------------------------------

/// Create an in-process session: resolve API key, store handle, set status to running,
/// and spawn the first conversation turn as a background task.
pub async fn create_inprocess_session(
    state: &AppState,
    user_id: Uuid,
    description: &str,
    provider_name: &str,
) -> Result<Uuid, AgentError> {
    let _ = super::service::get_provider(provider_name)?;

    // Resolve user API key
    let api_key = resolve_user_api_key(state, user_id).await.ok_or_else(|| {
        AgentError::Other(anyhow::anyhow!(
            "No Anthropic API key configured. Set your key in Settings > Provider Keys."
        ))
    })?;

    let session_id = Uuid::new_v4();

    // Insert DB row as 'running'
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, provider, status) VALUES ($1, $2, $3, $4, 'running')",
    )
    .bind(session_id)
    .bind(user_id)
    .bind(description)
    .bind(provider_name)
    .execute(&state.pool)
    .await?;

    // Create handle
    let handle = InProcessHandle::new(api_key, None, user_id);

    // Store in AppState
    {
        let mut sessions = state.inprocess_sessions.write().unwrap();
        sessions.insert(session_id, handle.clone());
    }

    // Save first user message to DB
    sqlx::query("INSERT INTO agent_messages (session_id, role, content) VALUES ($1, 'user', $2)")
        .bind(session_id)
        .bind(description)
        .execute(&state.pool)
        .await?;

    // Add first user message to conversation history
    {
        let mut msgs = handle.messages.write().await;
        msgs.push(ChatMessage::user(description));
    }

    // Spawn first turn
    let state_clone = state.clone();
    let handle_clone = handle.clone();
    tokio::spawn(async move {
        if let Err(e) = run_turn(&state_clone, session_id, &handle_clone).await {
            tracing::error!(error = %e, %session_id, "first turn failed");
        }
    });

    Ok(session_id)
}

/// Send a follow-up user message to an in-process session.
pub async fn send_inprocess_message(
    state: &AppState,
    session_id: Uuid,
    content: &str,
) -> Result<(), AgentError> {
    let handle = {
        let sessions = state.inprocess_sessions.read().unwrap();
        sessions.get(&session_id).cloned()
    }
    .ok_or(AgentError::SessionNotRunning)?;

    // Save user message to DB
    sqlx::query("INSERT INTO agent_messages (session_id, role, content) VALUES ($1, 'user', $2)")
        .bind(session_id)
        .bind(content)
        .execute(&state.pool)
        .await?;

    // Add to conversation history
    {
        let mut msgs = handle.messages.write().await;
        msgs.push(ChatMessage::user(content));
    }

    // Spawn turn
    let state_clone = state.clone();
    let handle_clone = handle;
    tokio::spawn(async move {
        if let Err(e) = run_turn(&state_clone, session_id, &handle_clone).await {
            tracing::error!(error = %e, %session_id, "turn failed");
        }
    });

    Ok(())
}

// ---------------------------------------------------------------------------
// Turn execution (tool-aware loop)
// ---------------------------------------------------------------------------

/// Run a conversation turn with tool support.
/// Loops up to `MAX_TOOL_ROUNDS` times when the model returns `tool_use` blocks.
async fn run_turn(
    state: &AppState,
    session_id: Uuid,
    handle: &InProcessHandle,
) -> Result<(), anyhow::Error> {
    let tools = create_app_tools();

    for _round in 0..MAX_TOOL_ROUNDS {
        let messages = handle.messages.read().await.clone();

        let result = anthropic::stream_turn_with_tools(
            &handle.api_key,
            handle.model.as_deref(),
            &messages,
            CREATE_APP_SYSTEM_PROMPT,
            &tools,
            &handle.tx,
        )
        .await?;

        match result {
            TurnResult::Text(text) => {
                save_assistant_text(state, session_id, handle, &text).await?;
                break;
            }
            TurnResult::ToolUse {
                text,
                tool_calls,
                assistant_blocks,
            } => {
                // Save any text emitted before tools
                if !text.is_empty() {
                    save_assistant_text_only_db(state, session_id, &text).await?;
                }

                // Append assistant blocks (text + tool_use) to history
                {
                    let mut msgs = handle.messages.write().await;
                    msgs.push(ChatMessage::assistant_blocks(assistant_blocks));
                }

                // Execute each tool and collect results
                let mut tool_results = Vec::new();
                for tc in &tool_calls {
                    let result = execute_tool(state, session_id, handle, tc).await;
                    let (content, is_error) = match &result {
                        Ok(v) => (v.to_string(), false),
                        Err(e) => (e.clone(), true),
                    };

                    // Emit ToolResult progress event
                    let _ = handle.tx.send(ProgressEvent {
                        kind: ProgressKind::ToolResult,
                        message: if is_error {
                            format!("{}: error — {content}", tc.name)
                        } else {
                            format!("{}: done", tc.name)
                        },
                        metadata: Some(serde_json::json!({
                            "tool_use_id": tc.id,
                            "tool_name": tc.name,
                            "is_error": is_error,
                            "result": content,
                        })),
                    });

                    let mut block = serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": tc.id,
                        "content": content,
                    });
                    if is_error {
                        block["is_error"] = serde_json::json!(true);
                    }
                    tool_results.push(block);
                }

                // Append tool results to history
                {
                    let mut msgs = handle.messages.write().await;
                    msgs.push(ChatMessage::tool_results(tool_results));
                }
                // Continue loop for next turn
            }
        }
    }

    // Signal completion
    let _ = handle.tx.send(ProgressEvent {
        kind: ProgressKind::Completed,
        message: "Turn completed".into(),
        metadata: None,
    });

    Ok(())
}

// ---------------------------------------------------------------------------
// Tool execution
// ---------------------------------------------------------------------------

/// Execute a single tool call, dispatching by name.
async fn execute_tool(
    state: &AppState,
    session_id: Uuid,
    handle: &InProcessHandle,
    tc: &anthropic::ToolCall,
) -> Result<serde_json::Value, String> {
    match tc.name.as_str() {
        "create_project" => execute_create_project(state, session_id, handle, &tc.input)
            .await
            .map_err(|e| e.to_string()),
        "create_ops_repo" => execute_create_ops_repo(state, handle, &tc.input)
            .await
            .map_err(|e| e.to_string()),
        "spawn_coding_agent" => execute_spawn_agent(state, handle, &tc.input)
            .await
            .map_err(|e| e.to_string()),
        other => Err(format!("unknown tool: {other}")),
    }
}

/// Create a project with a bare git repo.
async fn execute_create_project(
    state: &AppState,
    session_id: Uuid,
    handle: &InProcessHandle,
    input: &serde_json::Value,
) -> Result<serde_json::Value, anyhow::Error> {
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

    // Look up owner username
    let owner_name: String = sqlx::query_scalar("SELECT username FROM users WHERE id = $1")
        .bind(handle.user_id)
        .fetch_one(&state.pool)
        .await?;

    // Init bare repo
    let repo_path =
        crate::git::repo::init_bare_repo(&state.config.git_repos_path, &owner_name, name, "main")
            .await?;

    let project_id = Uuid::new_v4();
    let repo_path_str = repo_path.to_string_lossy().to_string();

    sqlx::query(
        "INSERT INTO projects (id, name, display_name, description, owner_id, visibility, repo_path) \
         VALUES ($1, $2, $3, $4, $5, 'private', $6)",
    )
    .bind(project_id)
    .bind(name)
    .bind(&display_name)
    .bind(&description)
    .bind(handle.user_id)
    .bind(&repo_path_str)
    .execute(&state.pool)
    .await?;

    // Link session to project
    sqlx::query("UPDATE agent_sessions SET project_id = $2 WHERE id = $1")
        .bind(session_id)
        .bind(project_id)
        .execute(&state.pool)
        .await?;

    // Audit
    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: handle.user_id,
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
        "repo_path": repo_path_str,
    }))
}

/// Create an ops repo for a project.
async fn execute_create_ops_repo(
    state: &AppState,
    handle: &InProcessHandle,
    input: &serde_json::Value,
) -> Result<serde_json::Value, anyhow::Error> {
    let project_id = parse_uuid_field(input, "project_id")?;

    // Get project name for default ops repo name
    let project_name: String =
        sqlx::query_scalar("SELECT name FROM projects WHERE id = $1 AND is_active = true")
            .bind(project_id)
            .fetch_optional(&state.pool)
            .await?
            .ok_or_else(|| anyhow::anyhow!("project not found"))?;

    let ops_name = input
        .get("name")
        .and_then(|v| v.as_str())
        .map_or_else(|| format!("{project_name}-ops"), String::from);

    let repo_path =
        crate::deployer::ops_repo::init_ops_repo(&state.config.ops_repos_path, &ops_name, "main")
            .await
            .map_err(|e| anyhow::anyhow!("ops repo init failed: {e}"))?;

    let ops_repo_id = Uuid::new_v4();
    let repo_path_str = repo_path.to_string_lossy().to_string();

    sqlx::query(
        "INSERT INTO ops_repos (id, project_id, name, repo_path, default_branch) \
         VALUES ($1, $2, $3, $4, 'main')",
    )
    .bind(ops_repo_id)
    .bind(project_id)
    .bind(&ops_name)
    .bind(&repo_path_str)
    .execute(&state.pool)
    .await?;

    // Audit
    let owner_name: String = sqlx::query_scalar("SELECT username FROM users WHERE id = $1")
        .bind(handle.user_id)
        .fetch_one(&state.pool)
        .await?;

    write_audit(
        &state.pool,
        &AuditEntry {
            actor_id: handle.user_id,
            actor_name: &owner_name,
            action: "ops_repo.create",
            resource: "ops_repo",
            resource_id: Some(ops_repo_id),
            project_id: Some(project_id),
            detail: Some(serde_json::json!({"name": ops_name, "source": "create-app-agent"})),
            ip_addr: None,
        },
    )
    .await;

    Ok(serde_json::json!({
        "ops_repo_id": ops_repo_id.to_string(),
        "name": ops_name,
    }))
}

/// Spawn a K8s coding agent session for the project.
async fn execute_spawn_agent(
    state: &AppState,
    handle: &InProcessHandle,
    input: &serde_json::Value,
) -> Result<serde_json::Value, anyhow::Error> {
    let project_id = parse_uuid_field(input, "project_id")?;
    let prompt = input
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required field: prompt"))?;

    let session = super::service::create_session(
        state,
        handle.user_id,
        project_id,
        prompt,
        "claude-code",
        None,
        None,
        &[],
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

/// Parse a UUID from a JSON object field.
fn parse_uuid_field(input: &serde_json::Value, field: &str) -> Result<Uuid, anyhow::Error> {
    let s = input
        .get(field)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required field: {field}"))?;
    Uuid::parse_str(s).map_err(|e| anyhow::anyhow!("invalid UUID for {field}: {e}"))
}

/// Save assistant text to DB and conversation history.
async fn save_assistant_text(
    state: &AppState,
    session_id: Uuid,
    handle: &InProcessHandle,
    text: &str,
) -> Result<(), anyhow::Error> {
    if text.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO agent_messages (session_id, role, content) VALUES ($1, 'assistant', $2)",
    )
    .bind(session_id)
    .bind(text)
    .execute(&state.pool)
    .await?;

    let mut msgs = handle.messages.write().await;
    msgs.push(ChatMessage::assistant_text(text));
    Ok(())
}

/// Save assistant text to DB only (no history update — used when `tool_use` follows).
async fn save_assistant_text_only_db(
    state: &AppState,
    session_id: Uuid,
    text: &str,
) -> Result<(), anyhow::Error> {
    sqlx::query(
        "INSERT INTO agent_messages (session_id, role, content) VALUES ($1, 'assistant', $2)",
    )
    .bind(session_id)
    .bind(text)
    .execute(&state.pool)
    .await?;
    Ok(())
}

/// Get a broadcast receiver for an in-process session's events.
pub fn subscribe(state: &AppState, session_id: Uuid) -> Option<broadcast::Receiver<ProgressEvent>> {
    let sessions = state.inprocess_sessions.read().unwrap();
    sessions.get(&session_id).map(InProcessHandle::subscribe)
}

/// Remove an in-process session handle (called on stop/cleanup).
pub fn remove_session(state: &AppState, session_id: Uuid) {
    let mut sessions = state.inprocess_sessions.write().unwrap();
    sessions.remove(&session_id);
}

/// Try to resolve the user's Anthropic API key from `user_provider_keys`.
async fn resolve_user_api_key(state: &AppState, user_id: Uuid) -> Option<String> {
    let master_key_hex = state.config.master_key.as_deref()?;
    let master_key = crate::secrets::engine::parse_master_key(master_key_hex).ok()?;
    match crate::secrets::user_keys::get_user_key(&state.pool, &master_key, user_id, "anthropic")
        .await
    {
        Ok(key) => key,
        Err(e) => {
            tracing::warn!(error = %e, %user_id, "failed to resolve user API key");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_new_creates_empty_messages() {
        let handle = InProcessHandle::new("test-key".into(), None, Uuid::nil());
        let msgs = handle.messages.try_read().unwrap();
        assert!(msgs.is_empty());
    }

    #[test]
    fn handle_new_stores_api_key_and_model() {
        let handle =
            InProcessHandle::new("sk-ant-123".into(), Some("claude-3".into()), Uuid::nil());
        assert_eq!(handle.api_key, "sk-ant-123");
        assert_eq!(handle.model, Some("claude-3".into()));
    }

    #[test]
    fn handle_new_no_model() {
        let handle = InProcessHandle::new("key".into(), None, Uuid::nil());
        assert!(handle.model.is_none());
    }

    #[test]
    fn handle_subscribe_returns_receiver() {
        let handle = InProcessHandle::new("key".into(), None, Uuid::nil());
        let _rx = handle.subscribe();
    }

    #[test]
    fn handle_clone_shares_messages() {
        let handle = InProcessHandle::new("key".into(), None, Uuid::nil());
        let clone = handle.clone();
        assert!(Arc::ptr_eq(&handle.messages, &clone.messages));
    }

    #[test]
    fn broadcast_capacity_is_256() {
        assert_eq!(BROADCAST_CAPACITY, 256);
    }

    #[test]
    fn system_prompt_has_two_phases() {
        assert!(CREATE_APP_SYSTEM_PROMPT.contains("PHASE 1"));
        assert!(CREATE_APP_SYSTEM_PROMPT.contains("PHASE 2"));
    }

    #[test]
    fn system_prompt_mentions_tools() {
        assert!(CREATE_APP_SYSTEM_PROMPT.contains("create_project"));
        assert!(CREATE_APP_SYSTEM_PROMPT.contains("spawn_coding_agent"));
    }

    #[test]
    fn create_app_tools_returns_three_tools() {
        let tools = create_app_tools();
        assert_eq!(tools.len(), 3);
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
            .collect();
        assert!(names.contains(&"create_project"));
        assert!(names.contains(&"create_ops_repo"));
        assert!(names.contains(&"spawn_coding_agent"));
    }

    #[test]
    fn create_app_tools_have_required_fields() {
        for tool in create_app_tools() {
            assert!(tool.get("name").is_some(), "tool missing 'name'");
            assert!(
                tool.get("description").is_some(),
                "tool missing 'description'"
            );
            assert!(
                tool.get("input_schema").is_some(),
                "tool missing 'input_schema'"
            );
        }
    }

    #[test]
    fn create_app_tools_input_schemas_valid() {
        for tool in create_app_tools() {
            let schema = tool.get("input_schema").unwrap();
            assert_eq!(schema.get("type").and_then(|t| t.as_str()), Some("object"));
            assert!(schema.get("properties").is_some());
        }
    }

    #[test]
    fn handle_stores_user_id() {
        let uid = Uuid::new_v4();
        let handle = InProcessHandle::new("key".into(), None, uid);
        assert_eq!(handle.user_id, uid);
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
