use std::sync::Arc;

use tokio::sync::{RwLock, broadcast};
use uuid::Uuid;

use sha2::{Digest, Sha256};

use super::anthropic::{self, ChatMessage, TurnResult};
use super::error::AgentError;
use super::provider::{ProgressEvent, ProgressKind};
use crate::audit::{AuditEntry, write_audit};
use crate::store::AppState;

const BROADCAST_CAPACITY: usize = 256;
const MAX_TOOL_ROUNDS: usize = 10;

const CREATE_APP_SYSTEM_PROMPT: &str = r#"You are an app-creation assistant for the Platform developer tool. Your job is to help users go from an idea to a fully deployed, monitored application in two phases.

== PHASE 1: CLARIFY ==
Ask 1-2 concise clarifying questions about the tech stack, framework, database, and deployment needs. When the user confirms the plan, move to Phase 2. Do NOT call any tools during this phase.

== PHASE 2: EXECUTE ==
Once the user confirms, execute these steps IN ORDER using your tools:

1. Call `create_project` with a slug-style name (lowercase, hyphens, e.g. "my-blog-api"). This automatically creates the K8s namespaces, network policy, and ops repo.
2. Call `spawn_coding_agent` with the project_id and a DETAILED prompt. The prompt MUST instruct the coding agent to:
   - Create the application source code with a GET /healthz endpoint returning 200 on port 8080
   - Create a multi-stage Dockerfile that builds and runs the app, EXPOSEing port 8080
   - Create a `.platform.yaml` file at the repo root with a kaniko build step:
     ```yaml
     steps:
       - name: build
         image: gcr.io/kaniko-project/executor:latest
         commands:
           - /kaniko/executor --context=dir:///workspace --dockerfile=/workspace/Dockerfile --destination=$REGISTRY/$PROJECT:$COMMIT_SHA --cache=true
     ```
     (The env vars $REGISTRY, $PROJECT, $COMMIT_SHA are injected by the pipeline executor)
   - Create a `deploy/production.yaml` file with plain K8s manifests (Deployment + Service) using minijinja template variables: `{{ project_name }}` for resource names, `{{ image_ref }}` for the container image, `{{ values.replicas | default(1) }}` for replica count
   - Add OpenTelemetry SDK instrumentation that reads OTEL_EXPORTER_OTLP_ENDPOINT and OTEL_SERVICE_NAME env vars to send traces, logs, and metrics
   - Commit ALL files and push to the `main` branch (not a feature branch)

After all tools succeed, tell the user:
"Your project is being set up! Here's what happens next:
1. A coding agent is writing your application code, Dockerfile, pipeline config, and deploy manifests.
2. When it pushes to main, the CI pipeline will automatically build a container image.
3. The deploy manifests will be synced to the ops repo and applied to your production namespace.
4. Once running, telemetry (traces, logs, metrics) will appear in the Observe dashboard.
You can track progress in the Sessions and Pipelines pages."

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
    /// Override the Anthropic API URL (for testing with mock servers).
    pub api_url: Option<String>,
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
            api_url: None,
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

/// Run a conversation turn for an existing in-process session.
///
/// Wraps the private `run_turn` so integration tests can exercise the full
/// `run_turn` → `execute_tool` → save pipeline without reimplementing it.
/// Only called from `tests/inprocess_integration.rs`.
#[allow(dead_code)]
pub async fn run_turn_for_session(state: &AppState, session_id: Uuid) -> Result<(), anyhow::Error> {
    let handle = {
        state
            .inprocess_sessions
            .read()
            .unwrap()
            .get(&session_id)
            .cloned()
    }
    .ok_or_else(|| anyhow::anyhow!("session not found"))?;
    run_turn(state, session_id, &handle).await
}

/// Create an in-process session: resolve API key, store handle, set status to running,
/// and spawn the first conversation turn as a background task.
///
/// `api_url` overrides the Anthropic API endpoint (used by tests with a mock server).
pub async fn create_inprocess_session(
    state: &AppState,
    user_id: Uuid,
    description: &str,
    provider_name: &str,
    api_url: Option<&str>,
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
    let mut handle = InProcessHandle::new(api_key, None, user_id);
    handle.api_url = api_url.map(String::from);

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
            handle.api_url.as_deref(),
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
        "spawn_coding_agent" => execute_spawn_agent(state, handle, &tc.input)
            .await
            .map_err(|e| e.to_string()),
        other => Err(format!("unknown tool: {other}")),
    }
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

/// Create a project with a bare git repo.
async fn execute_create_project(
    state: &AppState,
    session_id: Uuid,
    handle: &InProcessHandle,
    input: &serde_json::Value,
) -> Result<serde_json::Value, anyhow::Error> {
    let (name, display_name, description) = parse_create_project_input(input)?;

    // Look up owner username
    let owner_name: String = sqlx::query_scalar("SELECT name FROM users WHERE id = $1")
        .bind(handle.user_id)
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
        handle.user_id,
        &owner_name,
        &owner_name,
    )
    .await?;

    // Insert with collision retry on namespace_slug (R3)
    let dn = display_name.as_deref();
    let desc = description.as_deref();
    let final_slug = match try_insert_inprocess(
        &state.pool,
        project_id,
        name,
        dn,
        desc,
        handle.user_id,
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
            try_insert_inprocess(
                &state.pool,
                project_id,
                name,
                dn,
                desc,
                handle.user_id,
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
        tracing::warn!(error = %e, %project_id, "project infra setup incomplete (in-process)");
    }

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
        "namespace_slug": namespace_slug,
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

/// Try to insert a project row, returning the `namespace_slug` on success.
/// Maps constraint violations to descriptive errors.
#[allow(clippy::too_many_arguments)]
async fn try_insert_inprocess(
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
        // Removed tools should NOT be in the prompt
        assert!(!CREATE_APP_SYSTEM_PROMPT.contains("create_ops_repo"));
        assert!(!CREATE_APP_SYSTEM_PROMPT.contains("seed_ops_repo"));
        assert!(!CREATE_APP_SYSTEM_PROMPT.contains("create_deployment"));
    }

    #[test]
    fn create_app_tools_returns_two_tools() {
        let tools = create_app_tools();
        assert_eq!(tools.len(), 2);
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
            .collect();
        assert!(names.contains(&"create_project"));
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

    #[test]
    fn system_prompt_mentions_lifecycle_flow() {
        assert!(CREATE_APP_SYSTEM_PROMPT.contains("Dockerfile"));
        assert!(CREATE_APP_SYSTEM_PROMPT.contains(".platform.yaml"));
        assert!(CREATE_APP_SYSTEM_PROMPT.contains("healthz"));
        assert!(CREATE_APP_SYSTEM_PROMPT.contains("OTEL_EXPORTER_OTLP_ENDPOINT"));
        assert!(CREATE_APP_SYSTEM_PROMPT.contains("deploy/production.yaml"));
    }
}
