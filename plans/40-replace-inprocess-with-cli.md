# Plan 40: Replace In-Process Agent with Claude CLI Subprocess

## Context

The platform currently has **three agent execution modes**:
1. **Pod-based (`"pod"`)** — spawns a K8s pod with Claude CLI for project-scoped agents (dev agents)
2. **In-process (`"inprocess"`)** — calls the Anthropic Messages API directly from the platform process, used for the "create-app" flow (manager/orchestrator agent)
3. **CLI subprocess (`"cli_subprocess"`)** — spawns a local Claude CLI subprocess, managed by `CliSessionManager` (currently unused in production)

The in-process mode (`src/agent/inprocess.rs` + `src/agent/anthropic.rs`) is ~1,440 LOC of custom Anthropic API streaming, SSE parsing, tool-loop orchestration, and conversation history management. It duplicates what the Claude CLI already does natively, and requires an `ANTHROPIC_API_KEY` when users may only have an OAuth subscription token.

### Manager Agent vs Dev Agent

This plan implements the **manager agent** (create-app flow):
- **No bash, no filesystem access** — `--tools ""` disables all built-in CLI tools
- **Server-side tool execution** — Claude returns structured JSON describing tools to call; the Rust server validates and executes them
- **Orchestrates dev agents** — the `spawn_coding_agent` tool creates separate pod-based sessions that run the `agent-runner` CLI wrapper (Plan 38)
- **Runs as CLI subprocess in the platform process** — NOT inside a K8s pod
- **Progress events published to Valkey pub/sub** — unified event transport for all agent types

The **dev agents** (Plan 38/39) are different:
- Full CLI access (bash, filesystem, tools)
- Run inside K8s pods with `agent-runner` wrapper
- Persistent subprocess with REPL mode (`--input-format stream-json`)
- Also publish events to Valkey pub/sub

**OAuth confirmed working**: `CLAUDE_CODE_OAUTH_TOKEN` works with `-p`, `--output-format stream-json`, and `--verbose`. Tokens valid 1 year, reusable across sessions.

**Multi-turn confirmed working** with `-p` + `--session-id` + `--resume`:
```bash
SESSION_ID=$(uuidgen)
claude -p "whats 10+20" --session-id "$SESSION_ID" --output-format stream-json --verbose
claude -p "and add again 10" --resume "$SESSION_ID" --output-format stream-json --verbose
```

**Structured output confirmed working** with `--tools "" --json-schema` (real output captured below):
```bash
claude -p "Create a React blog app called my-blog with PostgreSQL database" \
  --output-format stream-json --verbose \
  --tools "" \
  --json-schema "$SCHEMA" \
  --system-prompt "$SYSTEM_PROMPT" \
  --max-turns 10
```

This disables ALL built-in CLI tools and forces Claude to return structured JSON matching the schema. The result message contains a `structured_output` field with the parsed response.

### Real CLI Output (captured 2026-03-03)

The following NDJSON messages were captured from a real invocation. Key observations annotated inline.

**1. System init** — `--tools ""` replaces built-in tools with synthetic `StructuredOutput` tool:
```json
{
  "type": "system",
  "subtype": "init",
  "session_id": "3b8b91ef-d38e-4537-b1db-b7cf7ab10a1c",
  "tools": ["StructuredOutput"],
  "model": "claude-opus-4-6",
  "apiKeySource": "none",
  "claude_code_version": "2.1.63",
  "mcp_servers": [...],
  "permissionMode": "default",
  "slash_commands": [...],
  "agents": [...],
  "skills": [...],
  "uuid": "af8cd0cb-f913-4b36-9e66-a0b13be49a80"
}
```
> **Key**: `tools` is `["StructuredOutput"]` not empty. The CLI replaces all built-in tools with a synthetic tool for JSON schema output. `apiKeySource: "none"` confirms OAuth is used (not an API key).

**2. Assistant messages** — streamed incrementally (thinking → text → tool_use), same message ID:
```json
{"type":"assistant","message":{"id":"msg_01YGho2LZusuRza4ET1gYURe",
  "content":[{"type":"thinking","thinking":"The user wants to create a React blog app..."}]}}

{"type":"assistant","message":{"id":"msg_01YGho2LZusuRza4ET1gYURe",
  "content":[{"type":"text","text":"I'll create the project and then spawn a coding agent..."}]}}

{"type":"assistant","message":{"id":"msg_01YGho2LZusuRza4ET1gYURe",
  "content":[{"type":"tool_use","id":"toolu_01DLqb89z1LY66vTuDWQxnxA",
    "name":"StructuredOutput",
    "input":{
      "text":"I'll create your React blog app with PostgreSQL...",
      "tools":[{"name":"create_project","parameters":{"name":"my-blog","display_name":"My Blog","description":"A React blog application with PostgreSQL database"}}]
    }}]}}
```
> **Key**: Structured output is delivered as a `tool_use` content block with `name: "StructuredOutput"`. The `input` field contains our schema'd data (`text` + `tools[]`). Multiple assistant messages share the same `id` — each is an incremental content block.

**3. Auto tool_result** — CLI auto-acknowledges the structured output:
```json
{"type":"user","message":{"role":"user","content":[
  {"tool_use_id":"toolu_01DLqb89z1LY66vTuDWQxnxA",
   "type":"tool_result",
   "content":"Structured output provided successfully"}
]}}
```
> **Key**: We don't need to feed this back — the CLI handles it internally.

**4. Rate limit event** — new message type (silently skipped by our parser):
```json
{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1772553600,...}}
```
> **Key**: Unknown `type` → our `parse_cli_message()` returns `None` (forward compat). No code changes needed.

**5. Result message** — `structured_output` at top level is our extraction point:
```json
{
  "type": "result",
  "subtype": "success",
  "is_error": false,
  "duration_ms": 8114,
  "duration_api_ms": 8096,
  "num_turns": 2,
  "result": "",
  "session_id": "3b8b91ef-d38e-4537-b1db-b7cf7ab10a1c",
  "total_cost_usd": 0.01888075,
  "structured_output": {
    "text": "I'll create your React blog app with PostgreSQL...",
    "tools": [
      {"name": "create_project", "parameters": {"name": "my-blog", "display_name": "My Blog", "description": "A React blog application with PostgreSQL database"}}
    ]
  },
  "usage": {
    "input_tokens": 4,
    "output_tokens": 211,
    "cache_read_input_tokens": 24334,
    "cache_creation_input_tokens": 227,
    "server_tool_use": {"web_search_requests": 0, "web_fetch_requests": 0},
    "cache_creation": {"ephemeral_1h_input_tokens": 227, "ephemeral_5m_input_tokens": 0}
  },
  "modelUsage": {"claude-opus-4-6": {"inputTokens": 4, "outputTokens": 211, "costUSD": 0.01888075, ...}},
  "permission_denials": [],
  "fast_mode_state": "off"
}
```
> **Key findings:**
> - `result.result` is **empty string** `""` when structured output is used — text lives in `structured_output.text`
> - `structured_output` is a top-level field on the result message — this is our primary extraction point
> - `num_turns: 2` — the structured output tool_use + auto tool_result counts as a turn
> - `usage` has new nested fields (`server_tool_use`, `cache_creation` sub-object, `modelUsage`) — our `UsageInfo` struct uses `#[serde(default)]` so unknown fields are silently ignored
> - `duration_api_ms` is a new field alongside `duration_ms`

### Implications for the plan

1. **Extract from `result.structured_output`** (not from assistant `tool_use` blocks) — simpler, single parse point
2. **`StructuredOutput` tool_use in assistant messages** — our `cli_message_to_progress()` will see this as a `ToolCall` event with name `"StructuredOutput"`. We should either filter it out or let it pass as a progress event (harmless).
3. **`result.result` is empty** — don't use it for text; always use `structured_output.text`
4. **Forward compat is working** — `rate_limit_event` and extra fields on `usage`/`result` are silently handled
5. **`--session-id` not required for first call** — CLI auto-generates one. But we pass it explicitly so we can `--resume` later with a known UUID.

### Critical CLI Learnings (from Plan 38 implementation)

These insights were discovered while debugging the `agent-runner` CLI wrapper and are essential for correct implementation:

1. **`--input-format stream-json` blocks on piped stdin** — When stdin is a pipe (not a TTY), the CLI reads stdin first before processing the `-p` prompt. This means using both `-p` and `--input-format stream-json` together causes the process to hang indefinitely. **For one-shot `-p` mode (this plan): do NOT use `--input-format stream-json`** — stdin is not used. Only use `--input-format stream-json` for persistent subprocess mode (Plan 38's agent-runner).

2. **`env_clear()` is critical for isolation** — Must prevent `DATABASE_URL`, `PLATFORM_MASTER_KEY`, and other secrets from leaking to the Claude CLI subprocess. But must whitelist sufficient env vars for Node.js runtime (PATH, HOME, TMPDIR).

3. **OAuth via `CLAUDE_CODE_OAUTH_TOKEN` env var** — Works correctly when passed as an env var to the subprocess. Do NOT use `CLAUDE_CONFIG_DIR` override (temp dirs have no OAuth credentials). The platform must resolve the user's OAuth token from the secrets engine and pass it directly.

4. **`apiKeySource: "none"` in system init** — Confirms OAuth is being used (not an API key). This is the expected value when `CLAUDE_CODE_OAUTH_TOKEN` is set.

5. **`tokio::process::Command::args()` prevents shell injection** — Args are passed as argv elements, not through a shell. Safe for user-provided prompts.

### Create-app spawns a separate dev agent

The `spawn_coding_agent` tool in `inprocess.rs:546` calls `service::create_session()` which creates a **separate K8s pod-based session** (execution_mode = "pod"). The two sessions are completely independent.

## Design Principles

- **Server-side tool execution**: The Rust server stays in control. CLI has ZERO built-in tools (`--tools ""`). Claude returns structured JSON describing which tools to call. The Rust server validates and executes them — same security model as today.
- **Structured output**: `--json-schema` forces Claude to return `{text, tools[]}` — the Rust server parses tool calls from the JSON, executes them, and feeds results back via `--resume`.
- **`-p` + `--session-id` + `--resume`**: Each message is a separate CLI process. Claude CLI manages conversation history natively. No persistent subprocess.
- **OAuth-first auth**: `CLAUDE_CODE_OAUTH_TOKEN` primary; `ANTHROPIC_API_KEY` fallback.
- **Delete, don't deprecate**: Remove `inprocess.rs` and `anthropic.rs` entirely.

---

## Architecture: Structured Output Tool Loop

### The JSON Schema

```json
{
  "type": "object",
  "properties": {
    "text": {
      "type": "string",
      "description": "Your response to the user"
    },
    "tools": {
      "type": "array",
      "description": "List of tools to execute. Empty array if no tools needed.",
      "items": {
        "type": "object",
        "properties": {
          "name": { "type": "string", "enum": ["create_project", "spawn_coding_agent"] },
          "parameters": { "type": "object" }
        },
        "required": ["name", "parameters"]
      }
    }
  },
  "required": ["text", "tools"]
}
```

### How the Tool Loop Works

```
User → POST /api/create-app { description: "Build me a blog" }
         │
         ▼
  create_global_session()
  ├── Generate session_id (UUID)
  ├── Resolve auth: OAuth > API key > global key
  ├── Spawn turn 1:
  │     claude -p "Build me a blog" \
  │       --session-id <session_id> \
  │       --system-prompt <create-app-instructions> \
  │       --tools "" \
  │       --json-schema <schema> \
  │       --output-format stream-json --verbose
  │         │
  │         ▼
  │     CLI returns structured_output:
  │     {
  │       "text": "What framework do you want? React or Vue?",
  │       "tools": []     ← no tools, just asking a question
  │     }
  │         │
  │         ▼
  │     Publish text as ProgressEvent::Text to pub/sub `session:{id}:events`
  │     Save assistant message to DB
  │     Turn complete (no tools to execute)
  │
  ▼ (user sends follow-up via POST /api/sessions/{id}/message)
  send_message("React please, with Postgres")
  ├── Spawn turn 2:
  │     claude -p "React please, with Postgres" \
  │       --resume <session_id> \
  │       --tools "" \
  │       --json-schema <schema> \
  │       --output-format stream-json --verbose
  │         │
  │         ▼
  │     CLI returns structured_output:
  │     {
  │       "text": "I'll create the project now.",
  │       "tools": [
  │         {"name": "create_project", "parameters": {"name": "react-blog"}}
  │       ]
  │     }
  │         │
  │         ▼
  │     Publish text + ToolCall events to pub/sub `session:{id}:events`
  │     Rust server executes create_project() server-side:
  │       - git::repo::init_bare_repo()
  │       - INSERT INTO projects
  │       - setup_project_infrastructure()
  │       - Link session to project
  │     Publish ToolResult event to pub/sub
  │
  ├── Spawn turn 3 (automatic — feed tool results back):
  │     claude -p "Tool results:\ncreate_project: {\"project_id\":\"...\",\"name\":\"react-blog\"}" \
  │       --resume <session_id> \
  │       --tools "" \
  │       --json-schema <schema> \
  │       --output-format stream-json --verbose
  │         │
  │         ▼
  │     CLI returns structured_output:
  │     {
  │       "text": "Project created! Now spawning the coding agent...",
  │       "tools": [
  │         {"name": "spawn_coding_agent", "parameters": {"project_id": "...", "prompt": "..."}}
  │       ]
  │     }
  │         │
  │         ▼
  │     Rust server executes spawn_coding_agent() server-side:
  │       - service::create_session() → K8s pod (SEPARATE session)
  │     Publish ToolResult event to pub/sub
  │
  ├── Spawn turn 4 (automatic — feed tool results):
  │     claude -p "Tool results:\nspawn_coding_agent: {\"session_id\":\"...\",\"status\":\"running\"}" \
  │       --resume <session_id> \
  │       --tools "" \
  │       --json-schema <schema> \
  │       --output-format stream-json --verbose
  │         │
  │         ▼
  │     {
  │       "text": "Your project is being set up! A coding agent is writing your code...",
  │       "tools": []     ← no more tools, turn complete
  │     }
  │
  └── Done. Session stays running for follow-up messages.
```

### Key insight: Same tool execution as today, different LLM interface

| | Current (inprocess.rs) | New (CLI subprocess) |
|---|---|---|
| **LLM call** | Raw Anthropic Messages API + SSE streaming | `claude -p` subprocess + NDJSON |
| **Tool definition** | Anthropic `tools[]` parameter | `--json-schema` structured output (CLI uses synthetic `StructuredOutput` tool internally) |
| **Tool invocation** | Anthropic returns `tool_use` content blocks | CLI returns `result.structured_output.tools[]` (also visible as `StructuredOutput` tool_use in assistant messages) |
| **Tool execution** | `execute_tool()` in Rust (server-side) | Same Rust functions, same code path |
| **Tool results** | Fed back as `tool_result` content blocks | Fed back as `-p "Tool results: ..."` via `--resume` |
| **Auth** | `ANTHROPIC_API_KEY` only | `CLAUDE_CODE_OAUTH_TOKEN` primary, API key fallback |
| **Conversation history** | Manual `Vec<ChatMessage>` in memory | Claude CLI manages via `--session-id` |
| **Security** | Server controls tools ✓ | Server controls tools ✓ (CLI has `--tools ""`) |

---

## PR 1: Types + CLI Invoke + Wire Create-App

*Combined PR (originally PRs 1+2).* Build the structured output infrastructure, then wire it into the create-app flow. Removes `inprocess_sessions` from AppState.

- [ ] Types & errors defined
- [ ] Migration applied
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### Migration

None needed.

### Code Changes

| File | Change |
|---|---|
| `src/agent/cli_invoke.rs` | **New** — One-shot CLI invocation with structured output parsing. Contains: `CliInvokeParams`, `StructuredResponse`, `ToolRequest`, `invoke_cli()`, `create_app_schema()`, tool result formatting. |
| `src/agent/create_app_prompt.rs` | **New** — System prompt for create-app sessions. Describes the two available tools (create_project, spawn_coding_agent) with parameter schemas so Claude knows what to put in the `tools[]` array. |
| `src/agent/claude_cli/transport.rs` | **Update** — Add `prompt: Option<String>`, `initial_session_id: Option<String>` (for `--session-id`), `json_schema: Option<String>`, `disable_tools: bool` to `CliSpawnOptions`. Update `build_args()`: skip `--input-format stream-json` when `prompt` is set (not needed in `-p` mode); emit `-p`, `--session-id`, `--tools ""`, `--json-schema`. |
| `src/agent/mod.rs` | Add `pub mod cli_invoke;` and `pub mod create_app_prompt;` |

### `cli_invoke.rs` Design

```rust
use uuid::Uuid;

use super::claude_cli::messages::ResultMessage;
use super::claude_cli::transport::{CliSpawnOptions, SubprocessTransport};
use super::claude_cli::session::cli_message_to_progress;
use super::error::AgentError;
use super::provider::{ProgressEvent, ProgressKind};
use super::pubsub_bridge;

/// Structured output from a CLI invocation with --json-schema.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct StructuredResponse {
    pub text: String,
    pub tools: Vec<ToolRequest>,
}

/// A tool call requested by the LLM via structured output.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ToolRequest {
    pub name: String,
    pub parameters: serde_json::Value,
}

/// Parameters for a one-shot CLI invocation.
pub struct CliInvokeParams {
    pub session_id: Uuid,
    pub prompt: String,
    pub is_resume: bool,
    pub system_prompt: Option<String>,
    pub oauth_token: Option<String>,
    pub anthropic_api_key: Option<String>,
    pub max_turns: Option<u32>,
}

/// The JSON schema for create-app structured output.
pub fn create_app_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "text": {
                "type": "string",
                "description": "Your response to the user"
            },
            "tools": {
                "type": "array",
                "description": "Tools to execute. Empty array if no tools needed.",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "enum": ["create_project", "spawn_coding_agent"]
                        },
                        "parameters": { "type": "object" }
                    },
                    "required": ["name", "parameters"]
                }
            }
        },
        "required": ["text", "tools"]
    })
}

/// Spawn `claude -p` with structured output, read NDJSON, publish events.
///
/// Returns the parsed StructuredResponse (text + tool requests).
/// Publishes ProgressEvents to Valkey pub/sub `session:{id}:events` in real-time.
pub async fn invoke_cli(
    params: CliInvokeParams,
    valkey: &fred::clients::Pool,
) -> Result<(StructuredResponse, Option<ResultMessage>), AgentError> {
    let schema = create_app_schema();
    let schema_str = serde_json::to_string(&schema)
        .map_err(|e| AgentError::Other(e.into()))?;

    let opts = CliSpawnOptions {
        oauth_token: params.oauth_token,
        anthropic_api_key: params.anthropic_api_key,
        system_prompt: params.system_prompt,
        prompt: Some(params.prompt),
        // Use `initial_session_id` (not `session_id`) to avoid collision with
        // SubprocessTransport's internal `session_id` tracking field.
        initial_session_id: if params.is_resume { None } else { Some(params.session_id.to_string()) },
        resume_session: if params.is_resume { Some(params.session_id.to_string()) } else { None },
        json_schema: Some(schema_str),
        disable_tools: true,  // --tools ""
        ..Default::default()
    };

    let transport = SubprocessTransport::spawn(opts)
        .map_err(|e| AgentError::Other(e.into()))?;

    // Read all NDJSON messages, broadcasting progress events.
    // Wrap in a timeout to prevent hanging on a stuck CLI process.
    let mut result_msg = None;
    let read_result = tokio::time::timeout(
        std::time::Duration::from_secs(300), // 5 min timeout per invocation
        async {
            loop {
                match transport.recv().await {
                    Ok(Some(msg)) => {
                        if let Some(event) = cli_message_to_progress(&msg) {
                            let _ = pubsub_bridge::publish_event(valkey, params.session_id, &event).await;
                        }
                        if let super::claude_cli::messages::CliMessage::Result(r) = msg {
                            result_msg = Some(r);
                            break;
                        }
                    }
                    Ok(None) => break,  // EOF
                    Err(e) => {
                        tracing::error!(error = %e, "CLI read error");
                        break;
                    }
                }
            }
        }
    ).await;

    // Always kill subprocess on exit (no Drop impl on SubprocessTransport)
    let _ = transport.kill().await;

    if read_result.is_err() {
        return Err(AgentError::Other(anyhow::anyhow!("CLI subprocess timed out after 300s")));
    }

    // Parse structured output from result message
    let structured = result_msg.as_ref()
        .and_then(|r| r.structured_output.as_ref())
        .map(|v| serde_json::from_value::<StructuredResponse>(v.clone()))
        .transpose()
        .map_err(|e| AgentError::Other(anyhow::anyhow!("failed to parse structured output: {e}")))?
        .unwrap_or_else(|| StructuredResponse {
            text: result_msg.as_ref()
                .and_then(|r| r.result.clone())
                .unwrap_or_default(),
            tools: vec![],
        });

    Ok((structured, result_msg))
}

/// Format tool execution results for feeding back via --resume.
pub fn format_tool_results(results: &[(String, Result<serde_json::Value, String>)]) -> String {
    let mut output = String::from("Tool execution results:\n");
    for (name, result) in results {
        match result {
            Ok(value) => output.push_str(&format!("- {name}: success — {value}\n")),
            Err(err) => output.push_str(&format!("- {name}: error — {err}\n")),
        }
    }
    output
}
```

### `CliSpawnOptions` Additions

Existing fields already handled: `system_prompt`, `resume_session`, `oauth_token`, `anthropic_api_key`, `output_format`, `verbose`. New fields to add:

```rust
pub struct CliSpawnOptions {
    // ... existing fields (system_prompt, resume_session, etc.) ...

    /// `-p <text>` — one-shot prompt mode. When set, `--input-format stream-json`
    /// is omitted from args (stdin is not used in `-p` mode).
    pub prompt: Option<String>,
    /// `--session-id <id>` — set CLI session ID (first invocation).
    /// Named `initial_session_id` to avoid confusion with SubprocessTransport's
    /// internal `session_id` tracking field.
    pub initial_session_id: Option<String>,
    /// `--json-schema <json>` — force structured output.
    pub json_schema: Option<String>,
    /// `--tools ""` — disable all built-in tools. Real output shows CLI replaces
    /// built-in tools with synthetic `StructuredOutput` tool.
    pub disable_tools: bool,
}
```

Update `build_args()`:
```rust
// When using -p mode, skip --input-format stream-json (stdin not used)
if opts.prompt.is_none() {
    args.push("--input-format".to_owned());
    args.push("stream-json".to_owned());
}
// ... existing --output-format stream-json --verbose ...

if opts.disable_tools {
    args.push("--tools".to_owned());
    args.push(String::new());  // --tools "" → CLI replaces built-in tools with synthetic "StructuredOutput" tool
}
if let Some(ref schema) = opts.json_schema {
    args.push("--json-schema".to_owned());
    args.push(schema.clone());
}
if let Some(ref prompt) = opts.prompt {
    args.push("-p".to_owned());
    args.push(prompt.clone());
}
if let Some(ref sid) = opts.initial_session_id {
    args.push("--session-id".to_owned());
    args.push(sid.clone());
}
```

### `ResultMessage` Update

The existing `ResultMessage` in `src/agent/claude_cli/messages.rs` needs a `structured_output` field. Real output also contains additional fields (`duration_api_ms`, `modelUsage`, `permission_denials`, `fast_mode_state`, `uuid`) which are silently ignored by serde `#[serde(default)]` — no need to model them unless we want them later.

```rust
pub struct ResultMessage {
    // ... existing fields ...
    /// Parsed structured output when --json-schema is used.
    /// Contains the schema'd response (e.g., `{text, tools[]}`).
    /// This is the PRIMARY extraction point — `result.result` is empty string
    /// when structured output is active.
    #[serde(default)]
    pub structured_output: Option<serde_json::Value>,
}
```

**Note**: `SystemMessage` real output also includes new fields (`mcp_servers`, `permissionMode`, `slash_commands`, `apiKeySource`, `agents`, `skills`, `plugins`, `uuid`, `fast_mode_state`). These are silently ignored by serde — no struct changes needed.

### `create_app_prompt.rs` Design

```rust
/// System prompt for create-app CLI sessions.
///
/// Describes the available tools so Claude knows what to put in the
/// structured output `tools[]` array. Claude cannot execute these tools
/// directly — the platform server executes them and feeds results back.
pub fn build_create_app_system_prompt() -> &'static str {
    r#"You are an app-creation assistant for the Platform developer tool.
Your job is to help users go from an idea to a fully deployed application.

== PHASE 1: CLARIFY ==
Ask 1-2 concise clarifying questions about the tech stack, framework, database,
and deployment needs. When the user confirms, move to Phase 2.
Return an empty tools array during this phase.

== PHASE 2: EXECUTE ==
Use the tools array in your structured output to request tool execution.
The platform server will execute the tools and send you the results.

Available tools:

1. create_project
   Creates a new project with git repo, K8s namespaces, and ops repo.
   Parameters:
   - name (string, required): slug-style name (lowercase, hyphens, e.g. "my-blog-api")
   - display_name (string, optional): human-readable name
   - description (string, optional): short description
   Example: {"name": "create_project", "parameters": {"name": "my-blog-api"}}

2. spawn_coding_agent
   Spawns a coding agent to write application code in the project.
   Parameters:
   - project_id (string, required): UUID from create_project result
   - prompt (string, required): detailed coding instructions
   The prompt MUST instruct the agent to create:
   - Application source code with GET /healthz on port 8080
   - Multi-stage Dockerfile exposing port 8080
   - `.platform.yaml` with kaniko build step
   - `deploy/production.yaml` with K8s Deployment + Service
   - OpenTelemetry instrumentation
   - Commit and push to main branch
   Example: {"name": "spawn_coding_agent", "parameters": {"project_id": "...", "prompt": "..."}}

== RULES ==
- Call tools IN ORDER: create_project first, then spawn_coding_agent
- Wait for tool results before requesting the next tool
- Keep responses concise. Never ask more than two questions at a time.
- After all tools succeed, summarize what was set up for the user."#
}
```

### TDD Test Strategy — PR 1

#### Tests to write FIRST (before implementation)

**Unit tests — `src/agent/cli_invoke.rs`**

| # | Test | Validates | Layer |
|---|---|---|---|
| 1 | `create_app_schema_is_valid_json_object` | `create_app_schema()` returns JSON with `"type": "object"` | Unit |
| 2 | `create_app_schema_has_text_and_tools_required` | Schema properties contain `text` (string) and `tools` (array), both in `required` | Unit |
| 3 | `create_app_schema_tools_enum_matches_available_tools` | Tools items name enum = `["create_project", "spawn_coding_agent"]` | Unit |
| 4 | `structured_response_deserialize_text_only` | `StructuredResponse` from `{"text":"hello","tools":[]}` | Unit |
| 5 | `structured_response_deserialize_with_tool_requests` | `StructuredResponse` with populated tools array | Unit |
| 6 | `structured_response_tool_request_fields` | `ToolRequest` name and parameters correctly populated | Unit |
| 7 | `format_tool_results_success` | `format_tool_results()` with `Ok` values produces readable output | Unit |
| 8 | `format_tool_results_error` | `format_tool_results()` with `Err` values includes error text | Unit |
| 9 | `format_tool_results_mixed` | Mixed Ok/Err entries | Unit |
| 10 | `format_tool_results_empty` | Empty vec returns header only | Unit |

**Unit tests — `src/agent/claude_cli/transport.rs`**

| # | Test | Validates | Layer |
|---|---|---|---|
| 11 | `build_args_with_disable_tools` | `disable_tools: true` → `["--tools", ""]` in args | Unit |
| 12 | `build_args_with_json_schema` | `json_schema: Some(...)` → `["--json-schema", "<json>"]` | Unit |
| 13 | `build_args_with_prompt` | `prompt: Some("hello")` → `["-p", "hello"]` | Unit |
| 14 | `build_args_with_initial_session_id` | `initial_session_id: Some("abc")` → `["--session-id", "abc"]` | Unit |
| 15 | `build_args_prompt_skips_input_format` | When `prompt` is set, `--input-format` is NOT in args | Unit |
| 16 | `build_args_disable_tools_false_no_flag` | `disable_tools: false` does not emit `--tools` | Unit |

**Unit tests — `src/agent/create_app_prompt.rs`**

| # | Test | Validates | Layer |
|---|---|---|---|
| 17 | `system_prompt_mentions_create_project` | Prompt contains `"create_project"` | Unit |
| 18 | `system_prompt_mentions_spawn_coding_agent` | Prompt contains `"spawn_coding_agent"` | Unit |
| 19 | `system_prompt_has_clarify_and_execute_phases` | Prompt contains `"CLARIFY"` and `"EXECUTE"` | Unit |
| 20 | `system_prompt_describes_tool_parameters` | Prompt includes parameter descriptions for both tools | Unit |

**Unit tests — `src/agent/claude_cli/messages.rs`**

| # | Test | Validates | Layer |
|---|---|---|---|
| 21 | `result_message_with_structured_output` | `ResultMessage` with `structured_output` field deserializes correctly | Unit |
| 22 | `result_message_without_structured_output` | `ResultMessage` without field → `structured_output: None` | Unit |

**Total: 22 unit tests**

#### Existing tests to UPDATE

| Test File | Change | Reason |
|---|---|---|
| `src/agent/claude_cli/transport.rs` — `spawn_options_default` | Add assertions for new fields: `prompt: None`, `initial_session_id: None`, `json_schema: None`, `disable_tools: false` | New fields added to struct |

#### Branch coverage checklist

| Branch/Path | Test covering it |
|---|---|
| `create_app_schema()` — returns valid schema | #1, #2, #3 |
| `StructuredResponse` deser — empty tools | #4 |
| `StructuredResponse` deser — populated tools | #5, #6 |
| `format_tool_results()` — Ok results | #7 |
| `format_tool_results()` — Err results | #8 |
| `format_tool_results()` — mixed | #9 |
| `format_tool_results()` — empty | #10 |
| `build_args()` — `disable_tools: true` | #11 |
| `build_args()` — `disable_tools: false` | #16 |
| `build_args()` — `json_schema: Some` | #12 |
| `build_args()` — `prompt: Some` | #13 |
| `build_args()` — `initial_session_id: Some` | #14 |
| `build_args()` — prompt skips `--input-format` | #15 |
| `ResultMessage.structured_output` present | #21 |
| `ResultMessage.structured_output` absent | #22 |

#### Tests NOT needed (with justification)

| What | Why |
|---|---|
| `invoke_cli()` full subprocess test | Deferred to PR 2 (requires mock CLI script). Individual components tested here. |
| `CliSpawnOptions` serialization round-trip | Not serialized; consumed by `build_args()` which is tested. |
| `CliInvokeParams` constructor | Plain struct with no logic. |

#### Coverage: 100% of touched lines

| Code path | Covered by | Tier |
|---|---|---|
| `cli_invoke.rs` — `create_app_schema()` | #1, #2, #3 | Unit |
| `cli_invoke.rs` — `StructuredResponse` deser | #4, #5, #6 | Unit |
| `cli_invoke.rs` — `format_tool_results()` all branches | #7–#10 | Unit |
| `cli_invoke.rs` — `invoke_cli()` | Deferred to PR 2 | — |
| `create_app_prompt.rs` — `build_create_app_system_prompt()` | #17–#20 | Unit |
| `transport.rs` — `build_args()` new branches | #11–#16 | Unit |
| `messages.rs` — `ResultMessage.structured_output` | #21, #22 | Unit |

### Verification
- `just test-unit` passes
- `build_args()` produces correct CLI invocation for `-p` mode
- Schema matches expected format

### PR 1 (continued): Wire Create-App to CLI Tool Loop

Replace `create_inprocess_session` with the CLI structured output tool loop. The Rust server spawns `claude -p`, parses the structured output, executes tools server-side, and feeds results back via `--resume`. Removes `inprocess_sessions` from AppState.

### Code Changes

| File | Change |
|---|---|
| `src/agent/create_app.rs` | **New** — Extract tool loop and tool execution into a dedicated module (keeps `service.rs` under 1000 LOC). Contains: `run_create_app_loop()`, `execute_create_app_tool()`, `execute_create_project()`, `execute_spawn_agent()`, `parse_create_project_input()`. Moved from `inprocess.rs` with `InProcessHandle` dependency removed. |
| `src/agent/service.rs` | **Rewrite** `create_global_session()`: resolve auth → insert DB row (with explicit `execution_mode = 'cli_subprocess'`, `uses_pubsub = true`) → register `CliSessionHandle` → spawn `run_create_app_loop()` background task. Events published to Valkey pub/sub (not broadcast channel). |
| `src/agent/service.rs` | **Update** `send_message()` `"cli_subprocess"` branch: acquire per-session Mutex, spawn `invoke_cli` with `--resume` and user's message. |
| `src/agent/service.rs` | **Remove** `"inprocess"` branches from `send_message()` and `stop_session()`. |
| `src/agent/service.rs` | **Add** `update_session_cost()` helper function (pure DB update, ~10 LOC). Note: `save_assistant_message()` is NOT needed — Plan 39's `spawn_persistence_subscriber()` handles all event-to-DB persistence centrally via the pub/sub bridge. |
| `src/agent/mod.rs` | Add `pub mod create_app;` |
| `src/agent/pubsub_bridge.rs` | **Update** — Add `publish_event()` helper (publishes `ProgressEvent` JSON to `session:{id}:events`). This extends the pub/sub bridge from Plan 39 with a server-side publish function. Events published here are persisted to `agent_messages` by Plan 39's `spawn_persistence_subscriber()`. |
| `src/api/sessions.rs` | **No WS changes needed** — Plan 39 PR 4 already replaces all WebSocket handlers with SSE endpoints. Create-app sessions publish events to pub/sub → Plan 39's `spawn_persistence_subscriber()` writes to `agent_messages` → SSE endpoint streams to browser. |
| `src/agent/claude_cli/session.rs` | **Refactor** `CliSessionHandle`: replace `transport: Arc<Mutex<SubprocessTransport>>` with `active_process: Mutex<Option<Child>>` + `cancelled: AtomicBool`. The handle no longer owns a persistent transport — each `-p` invocation creates and destroys its own `SubprocessTransport`. No `register_broadcast_only()` needed (pub/sub replaces broadcast channels; SSE replaces WebSocket on the browser side). Update `send_cli_message()` and `stop_cli_session()` accordingly. |
| `src/store/mod.rs` | **Remove** `inprocess_sessions` field and `InProcessHandle` import from `AppState`. |
| `src/main.rs` | **Remove** `inprocess_sessions` from AppState initialization. |
| `tests/helpers/mod.rs` | **Remove** `inprocess_sessions` from `test_state()`. |
| `tests/e2e_helpers/mod.rs` | **Remove** `inprocess_sessions` from `e2e_state()`. |
| `tests/setup_integration.rs` | **Remove** `inprocess_sessions` from `setup_test_state()`. |

### `create_global_session()` Rewrite

```rust
pub async fn create_global_session(
    state: &AppState,
    user_id: Uuid,
    prompt: &str,
    provider_name: &str,
) -> Result<AgentSession, AgentError> {
    let _ = get_provider(provider_name)?;

    // 1. Resolve auth
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
            "No Claude credentials configured. Upload CLI credentials via Settings, \
             set an Anthropic API key, or ask an admin to configure a global key.".into(),
        ));
    }

    // 2. Create session row
    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agent_sessions (id, user_id, prompt, provider, status, execution_mode, uses_pubsub) \
         VALUES ($1, $2, $3, $4, 'running', 'cli_subprocess', true)",
    )
    .bind(session_id).bind(user_id).bind(prompt).bind(provider_name)
    .execute(&state.pool).await?;

    // 3. Save first user message
    sqlx::query("INSERT INTO agent_messages (session_id, role, content) VALUES ($1, 'user', $2)")
        .bind(session_id).bind(prompt)
        .execute(&state.pool).await?;

    // 4. Start persistence subscriber — writes all pub/sub events to agent_messages
    //    (Plan 39 architecture: persist-then-forward)
    pubsub_bridge::spawn_persistence_subscriber(
        state.pool.clone(), &state.valkey, session_id,
    );

    // 5. Spawn tool loop as background task
    let state_clone = state.clone();
    let prompt_owned = prompt.to_owned();
    tokio::spawn(async move {
        run_create_app_loop(
            &state_clone, session_id, user_id,
            &prompt_owned, cli_oauth_token, user_api_key,
        ).await;
    });

    fetch_session(&state.pool, session_id).await
}
```

### `run_create_app_loop()` — The Tool Loop

**Note:** This function lives in `src/agent/create_app.rs` (extracted from service.rs to keep module sizes manageable).

```rust
const MAX_TOOL_ROUNDS: usize = 10;

/// Run the create-app tool loop:
/// invoke_cli → parse tools → execute server-side → feed results → repeat.
///
/// Lives in `src/agent/create_app.rs`.
async fn run_create_app_loop(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    initial_prompt: &str,
    oauth_token: Option<String>,
    api_key: Option<String>,
) {
    let system_prompt = create_app_prompt::build_create_app_system_prompt().to_owned();
    let mut current_prompt = initial_prompt.to_owned();
    let mut is_resume = false;

    for round in 0..MAX_TOOL_ROUNDS {
        // Invoke CLI
        let params = CliInvokeParams {
            session_id,
            prompt: current_prompt.clone(),
            is_resume,
            system_prompt: if is_resume { None } else { Some(system_prompt.clone()) },
            oauth_token: oauth_token.clone(),
            anthropic_api_key: api_key.clone(),
            max_turns: Some(1),  // one turn per invocation
        };

        let (structured, result_msg) = match cli_invoke::invoke_cli(params, &state.valkey).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, %session_id, round, "CLI turn failed");
                let _ = pubsub_bridge::publish_event(&state.valkey, session_id, &ProgressEvent {
                    kind: ProgressKind::Error,
                    message: e.to_string(),
                    metadata: None,
                }).await;
                break;
            }
        };

        // Publish text to pub/sub — persistence handled by spawn_persistence_subscriber() (Plan 39)
        if !structured.text.is_empty() {
            let _ = pubsub_bridge::publish_event(&state.valkey, session_id, &ProgressEvent {
                kind: ProgressKind::Text,
                message: structured.text.clone(),
                metadata: None,
            }).await;
            // No save_assistant_message() — the persistence subscriber writes to
            // agent_messages for every event published to session:{id}:events
        }

        // Save cost metadata
        if let Some(ref r) = result_msg {
            update_session_cost(&state.pool, session_id, r).await;
        }

        // If no tools requested, turn is complete
        if structured.tools.is_empty() {
            let _ = pubsub_bridge::publish_event(&state.valkey, session_id, &ProgressEvent {
                kind: ProgressKind::Completed,
                message: "Turn completed".into(),
                metadata: None,
            }).await;
            break;
        }

        // Execute tools server-side
        let mut tool_results = Vec::new();
        for tool_req in &structured.tools {
            let _ = pubsub_bridge::publish_event(&state.valkey, session_id, &ProgressEvent {
                kind: ProgressKind::ToolCall,
                message: tool_req.name.clone(),
                metadata: Some(serde_json::json!({"parameters": tool_req.parameters})),
            }).await;

            let result = execute_create_app_tool(
                state, session_id, user_id, &tool_req.name, &tool_req.parameters,
            ).await;

            let _ = pubsub_bridge::publish_event(&state.valkey, session_id, &ProgressEvent {
                kind: ProgressKind::ToolResult,
                message: match &result {
                    Ok(_) => format!("{}: done", tool_req.name),
                    Err(e) => format!("{}: error — {e}", tool_req.name),
                },
                metadata: None,
            }).await;

            tool_results.push((tool_req.name.clone(), result));
        }

        // Format results and feed back via --resume
        current_prompt = cli_invoke::format_tool_results(&tool_results);
        is_resume = true;
    }
}
```

### `execute_create_app_tool()` — Server-Side Tool Dispatch

Extracted from `inprocess.rs::execute_tool()` into `src/agent/create_app.rs`. Same logic, same security. Also moves `parse_create_project_input()` and `parse_uuid_field()` helpers (so their unit tests survive the inprocess.rs deletion):

```rust
/// Lives in `src/agent/create_app.rs`.
async fn execute_create_app_tool(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    name: &str,
    params: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    match name {
        "create_project" => {
            execute_create_project(state, session_id, user_id, params)
                .await.map_err(|e| e.to_string())
        }
        "spawn_coding_agent" => {
            execute_spawn_agent(state, user_id, params)
                .await.map_err(|e| e.to_string())
        }
        other => Err(format!("unknown tool: {other}")),
    }
}
```

The `execute_create_project` and `execute_spawn_agent` functions are moved from `inprocess.rs` with minimal changes:
- Remove `InProcessHandle` dependency, use `user_id` directly
- **Add** `validation::check_length("prompt", prompt, 1, 100_000)?` in `execute_spawn_agent` (missing in original)
- **Add** length check on `structured.text` (max 100K) before broadcasting/saving

### TDD Test Strategy — PR 1 (continued)

#### Tests to write FIRST (before implementation)

**Unit tests — `src/agent/create_app.rs`**

| # | Test | Validates | Layer |
|---|---|---|---|
| 1 | `execute_create_app_tool_unknown_returns_error` | `execute_create_app_tool("bad_tool", ...)` → `Err("unknown tool: bad_tool")` | Unit |
| 2 | `max_tool_rounds_is_10` | `MAX_TOOL_ROUNDS == 10` | Unit |
| 3 | `parse_create_project_input_valid` | `parse_create_project_input()` extracts name/display_name/description | Unit |
| 4 | `parse_create_project_input_missing_name` | Returns error for missing `name` field | Unit |
| 5 | `parse_uuid_field_valid` | `parse_uuid_field()` extracts valid UUID | Unit |
| 6 | `parse_uuid_field_invalid` | Returns error for invalid UUID string | Unit |

**Unit tests — `src/agent/pubsub_bridge.rs`**

| # | Test | Validates | Layer |
|---|---|---|---|
| 7 | `publish_event_serializes_progress_event` | `publish_event()` produces correct JSON for `ProgressEvent` | Unit |
| 8 | `publish_event_uses_correct_channel` | Channel name is `session:{id}:events` | Unit |

**Unit tests — `src/agent/claude_cli/session.rs`**

| # | Test | Validates | Layer |
|---|---|---|---|
| 9 | `session_handle_active_process_none_initially` | New handle has `active_process: None` | Unit |
| 10 | `session_handle_cancelled_false_initially` | New handle has `cancelled: false` | Unit |

**Integration tests — `tests/create_app_integration.rs` (updates)**

| # | Test | Validates | Layer |
|---|---|---|---|
| 11 | `create_global_session_no_credentials_returns_error` | Without OAuth or API key → `ConfigurationRequired` error | Integration |
| 12 | `create_global_session_inserts_cli_subprocess_row` | After `POST /api/create-app`, session has `execution_mode = 'cli_subprocess'` | Integration |
| 13 | `create_global_session_publishes_events_to_pubsub` | After creation, events published to `session:{id}:events` are receivable via Valkey subscribe | Integration |
| 14 | `execute_create_app_tool_create_project` | `execute_create_app_tool("create_project", params)` inserts project in DB | Integration |
| 15 | `stop_session_cli_subprocess_sets_cancelled` | `stop_session()` → `cancelled` flag set, session removed from manager, DB status = 'stopped' | Integration |
| 16 | `stop_session_kills_active_process` | With a running subprocess, `stop_session()` kills it (process exits) | Integration |

**Total: 6 unit + 6 integration = 12 tests**

#### Existing tests to UPDATE

| Test File | Change | Reason |
|---|---|---|
| `tests/helpers/mod.rs` — `test_state()` | Remove `inprocess_sessions` field | Removed from AppState |
| `tests/e2e_helpers/mod.rs` — `e2e_state()` | Remove `inprocess_sessions` field | Removed from AppState |
| `tests/setup_integration.rs` — `setup_test_state()` | Remove `inprocess_sessions` field | Removed from AppState |
| `tests/create_app_integration.rs` — `create_app_session_is_inprocess` | Rename to `create_app_session_is_cli_subprocess`, assert `execution_mode == "cli_subprocess"` | Execution mode changed |
| `tests/create_app_integration.rs` — `create_app_without_api_key_fails` | Update error message text if wording changes ("CLI credentials") | Error message may change |
| `src/agent/claude_cli/session.rs` — `remove_session_decrements_count` | Update to handle no broadcast channel (pub/sub replaces it) | Architecture change |
| `src/agent/claude_cli/session.rs` — `concurrent_limit_enforced` | Update handle construction (no transport, no broadcast) | Architecture change |

#### Branch coverage checklist

| Branch/Path | Test covering it |
|---|---|
| `create_global_session()` — OAuth token present | Existing `create_app_session` test |
| `create_global_session()` — no credentials | #11 |
| `create_global_session()` — API key fallback | Existing `create_app_session` (via `set_user_api_key`) |
| `create_global_session()` — insert with 'cli_subprocess' | #12 |
| `create_global_session()` — events via pub/sub | #13 |
| `execute_create_app_tool()` — "create_project" | #14 |
| `execute_create_app_tool()` — "spawn_coding_agent" | Deferred to PR 2 (needs mock K8s) |
| `execute_create_app_tool()` — unknown tool | #1 |
| `publish_event()` — serializes ProgressEvent | #7 |
| `publish_event()` — correct channel name | #8 |
| `stop_session()` — cli_subprocess | #15 |
| `stop_session()` — inprocess branch removed | Compile-time (branch deleted) |
| `send_message()` — inprocess branch removed | Compile-time (branch deleted) |
| `parse_create_project_input()` — valid | #3 |
| `parse_create_project_input()` — missing name | #4 |
| `parse_uuid_field()` — valid/invalid | #5, #6 |

#### Tests NOT needed (with justification)

| What | Why |
|---|---|
| Full `run_create_app_loop()` with mock CLI | Deferred to PR 2 (needs mock script). Wiring verified by unit + integration tests above. |
| SSE endpoint integration test | SSE subscribes to pub/sub bridge (Plan 39 PR 4). Pub/sub event flow tested via #13. Full SSE test in Plan 39. |
| `update_session_cost()` | Pure DB update wrapper. Implicitly tested via PR 2 integration tests. `save_assistant_message()` removed — persistence handled by Plan 39's `spawn_persistence_subscriber()`. |

#### Coverage: 100% of touched lines

| Code path | Covered by | Tier |
|---|---|---|
| `create_app.rs` — `execute_create_app_tool()` 3 match arms | #1, #14, PR 2 | Unit/Integration |
| `create_app.rs` — `parse_create_project_input()` | #3, #4 | Unit |
| `create_app.rs` — `parse_uuid_field()` | #5, #6 | Unit |
| `create_app.rs` — `run_create_app_loop()` | Deferred to PR 2 | — |
| `service.rs` — `create_global_session()` rewrite | #11, #12, #13, existing create_app tests | Integration |
| `service.rs` — `send_message()` removed inprocess | Compile-time | — |
| `service.rs` — `stop_session()` cli_subprocess | #15 | Integration |
| `pubsub_bridge.rs` — `publish_event()` | #7, #8 | Unit |
| `session.rs` — `CliSessionHandle` refactored (no transport) | #9, #10, existing tests updated | Unit |
| `store/mod.rs` — removed field | Compile-time | — |
| `sessions.rs` — SSE endpoints (from Plan 39 PR 4) | No changes needed in Plan 40 | — |

### Verification
- `just test-unit` passes (no compile errors from removed `inprocess_sessions`)
- `just test-integration` passes (create-app tests updated)
- `/api/create-app` creates `cli_subprocess` session
- SSE streaming subscribes to Valkey pub/sub (WebSocket removed by Plan 39 PR 4)

---

## PR 2: Delete Dead Code + Replace Tests + LLM Tests

Remove inprocess.rs, anthropic.rs, and their tests. Replace with CLI-based tests. Add LLM test tier. Clean up `'inprocess'` from DB CHECK constraint.

- [ ] Types & errors defined
- [ ] Migration applied
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### Migration: `YYYYMMDDHHMMSS_drop_inprocess_execution_mode`

**Up:**
```sql
-- Remove 'inprocess' from execution_mode CHECK constraint.
-- No data cleanup needed — DB is wiped between deployments.
ALTER TABLE agent_sessions DROP CONSTRAINT IF EXISTS agent_sessions_execution_mode_check;
ALTER TABLE agent_sessions ADD CONSTRAINT agent_sessions_execution_mode_check
    CHECK (execution_mode IN ('pod', 'cli_subprocess'));
```

**Down:**
```sql
ALTER TABLE agent_sessions DROP CONSTRAINT IF EXISTS agent_sessions_execution_mode_check;
ALTER TABLE agent_sessions ADD CONSTRAINT agent_sessions_execution_mode_check
    CHECK (execution_mode IN ('pod', 'cli_subprocess', 'inprocess'));
```

### Code Changes — Deletion

| File | Change |
|---|---|
| `src/agent/inprocess.rs` | **Delete** (815 LOC) |
| `src/agent/anthropic.rs` | **Delete** (623 LOC) |
| `src/agent/mod.rs` | Remove `pub mod anthropic;` and `pub mod inprocess;` |
| `tests/inprocess_integration.rs` | **Delete** (986 LOC) |
| `tests/mock_anthropic.rs` | **Delete** (~300 LOC) |

### Code Changes — Test Replacement

| File | Change |
|---|---|
| `tests/fixtures/mock-claude-cli.sh` | **New** — Mock CLI that reads `-p`, `--session-id`/`--resume`, `--json-schema`. Emits NDJSON system init + structured output result. Multi-invocation support via `MOCK_CLI_RESPONSE_FILE` (JSON array) and `MOCK_CLI_STATE_DIR` (counter file). Invoked via `CLAUDE_CLI_PATH` env var override. |
| `tests/cli_create_app_integration.rs` | **New** — Integration tests exercising the full CLI tool loop with mock CLI. Each test creates a temp dir with response files. |
| `tests/llm_create_app.rs` | **New** — LLM tests using real Claude CLI with real OAuth/API tokens. `#[ignore]` attribute, run via `just test-llm`. Validates structured output, session resume, NDJSON parsing, and full create-app flow against real Claude. |

### Mock CLI Script

Supports multi-invocation scenarios (tool loop) via a response file with an array of responses and a state counter.

```bash
#!/usr/bin/env bash
# Mock Claude CLI for integration tests.
# Reads responses from $MOCK_CLI_RESPONSE_FILE (JSON array).
# Tracks invocation count via $MOCK_CLI_STATE_DIR/invocation-count.
set -euo pipefail

PROMPT="" SESSION_ID="" IS_RESUME=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    -p) PROMPT="$2"; shift 2;;
    --session-id) SESSION_ID="$2"; shift 2;;
    --resume) SESSION_ID="$2"; IS_RESUME=true; shift 2;;
    --json-schema|--system-prompt|--tools|--output-format) shift 2;;
    --verbose) shift;;
    *) shift;;
  esac
done

# Track invocation count for multi-call scenarios
STATE_DIR="${MOCK_CLI_STATE_DIR:-/tmp/mock-cli-state}"
mkdir -p "$STATE_DIR"
COUNT_FILE="$STATE_DIR/invocation-count"
COUNT=$(cat "$COUNT_FILE" 2>/dev/null || echo "0")
echo $((COUNT + 1)) > "$COUNT_FILE"

# Read response from file or use env var fallback
if [ -n "${MOCK_CLI_RESPONSE_FILE:-}" ] && [ -f "$MOCK_CLI_RESPONSE_FILE" ]; then
  RESPONSE=$(jq -r ".[$COUNT] // empty" "$MOCK_CLI_RESPONSE_FILE")
  TEXT=$(echo "$RESPONSE" | jq -r '.text // "ok"')
  TOOLS=$(echo "$RESPONSE" | jq -c '.tools // []')
else
  TEXT="${MOCK_TEXT:-Received: $PROMPT}"
  TOOLS="${MOCK_TOOLS:-[]}"
fi

STRUCTURED="{\"text\":$(echo "$TEXT" | jq -Rs .),\"tools\":$TOOLS}"

echo "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"$SESSION_ID\",\"model\":\"mock\"}"
echo "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":$(echo "$TEXT" | jq -Rs .)}]}}"
echo "{\"type\":\"result\",\"subtype\":\"success\",\"session_id\":\"$SESSION_ID\",\"is_error\":false,\"result\":$(echo "$TEXT" | jq -Rs .),\"total_cost_usd\":0.01,\"duration_ms\":100,\"num_turns\":1,\"structured_output\":$STRUCTURED}"
```

### TDD Test Strategy — PR 2

#### Mock CLI Script Design

`tests/fixtures/mock-claude-cli.sh` — invoked via `CLAUDE_CLI_PATH` env var override (already supported by `find_claude_cli()` in transport.rs).

Multi-invocation support: the script reads `MOCK_CLI_RESPONSE_FILE` env var pointing to a JSON file containing an array of responses. Uses a counter file (`$MOCK_CLI_STATE_DIR/invocation-count`) to track which response to emit per invocation. Each test creates its own temp dir.

```bash
#!/usr/bin/env bash
set -euo pipefail
# Parse args for -p, --session-id, --resume
# Read response from $MOCK_CLI_RESPONSE_FILE[invocation_count]
# Emit NDJSON: system init → assistant message → result with structured_output
```

#### Tests to write FIRST (before implementation)

**Integration tests — `tests/cli_create_app_integration.rs`**

| # | Test | Validates | Layer |
|---|---|---|---|
| 1 | `cli_create_app_text_only` | Mock CLI returns text, empty tools. Broadcasts Text + Completed events. Assistant message saved to DB. | Integration |
| 2 | `cli_create_app_creates_project` | Mock CLI returns `create_project` tool. Server executes it, project appears in DB. Second invocation (--resume with results) returns text only. | Integration |
| 3 | `cli_create_app_creates_project_and_spawns_agent` | Full 2-tool flow: create_project → spawn_coding_agent → text. Both project and agent session in DB. | Integration |
| 4 | `cli_create_app_unknown_tool_error` | Mock CLI returns unknown tool name. Error fed back. Final response is text. | Integration |
| 5 | `cli_create_app_followup_via_resume` | Create session, then `send_message()`. Second invocation uses `--resume`. Verify mock receives `--resume` flag. | Integration |
| 6 | `cli_create_app_no_credentials` | No API key, no OAuth. `POST /api/create-app` returns 400. | Integration |
| 7 | `cli_create_app_permissions_required` | Viewer role → 403. | Integration |
| 8 | `cli_create_app_rate_limited` | 6th creation returns 429. | Integration |
| 9 | `cli_create_app_session_is_cli_subprocess` | Session `execution_mode == "cli_subprocess"`. | Integration |
| 10 | `cli_create_app_empty_description_rejected` | Empty description → 400. | Integration |
| 11 | `cli_create_app_stop_during_tool_loop` | Stop while mock CLI is "running" (slow mock). Tool loop checks `cancelled`, exits. Session status = 'stopped'. | Integration |
| 12 | `cli_create_app_send_queued_while_busy` | Send 2 messages while tool loop running. Messages queued in `pending_messages`, drained between rounds, fed as combined `--resume`. | Integration |
| 13 | `cli_create_app_tools_empty_string_works` | Verify `--tools ""` actually disables tools (mock CLI logs args, test checks `--tools` followed by empty arg). | Integration |

**Total: 13 integration tests**

#### Tests DELETED (with replacement mapping)

| Deleted Test | Replacement | Justification |
|---|---|---|
| `inprocess_text_response` | `cli_create_app_text_only` (#1) | Same behavior, different transport |
| `inprocess_followup_message` | `cli_create_app_followup_via_resume` (#5) | Uses --resume instead of stdin |
| `inprocess_tool_use_creates_project` | `cli_create_app_creates_project` (#2) | Same server-side tool execution |
| `inprocess_text_then_tool_use` | Subsumed by #2 | Text + tool in structured output |
| `inprocess_multiple_tool_use_blocks` | #3 + #4 | Sequential tools + unknown tool |
| `inprocess_api_error_propagates` | Mock CLI error result | CLI handles API errors internally |
| `inprocess_thinking_delta` | **Not needed** | CLI manages thinking blocks internally; not visible in structured output |
| `inprocess_subscribe_and_remove` | Not needed | Pub/sub replaces in-process broadcast; SSE + pub/sub bridge tested in Plan 39 |
| `inprocess_request_contract` | **Not needed** | We don't call Anthropic API directly; CLI handles contract |
| `inprocess_no_api_key` | `cli_create_app_no_credentials` (#6) | Same error path |
| `inprocess_create_session_via_api` | `cli_create_app_session_is_cli_subprocess` (#9) | Verifies execution_mode |
| `inprocess_create_session_with_mock` | `cli_create_app_text_only` (#1) | Full lifecycle test |
| `inprocess_send_message_triggers_turn` | `cli_create_app_followup_via_resume` (#5) | Same flow |
| `inprocess_send_message_nonexistent` | Already covered by session_integration.rs | Not create-app specific |
| `inprocess_conversation_history_grows` | **Not needed** | CLI manages history via `--session-id`/`--resume` |

#### Branch coverage checklist

| Branch/Path | Test covering it |
|---|---|
| `run_create_app_loop()` — CLI returns text only (no tools) | #1 |
| `run_create_app_loop()` — CLI returns tools, server executes, feeds back | #2, #3 |
| `run_create_app_loop()` — unknown tool error fed back | #4 |
| `run_create_app_loop()` — max rounds limit | PR 2 constant assertion |
| `invoke_cli()` — spawn transport + read NDJSON | #1–#5 (all use real subprocess) |
| `invoke_cli()` — parse structured_output from result | #1–#4 |
| `invoke_cli()` — timeout (300s) | Not tested (requires slow mock — acceptable gap) |
| `invoke_cli()` — kill subprocess on exit | Implicit in all tests (process cleanup) |
| `execute_create_app_tool()` — create_project | #2, #3 |
| `execute_create_app_tool()` — spawn_coding_agent | #3 (error path if K8s not set up) |
| `execute_create_app_tool()` — unknown tool | #4 |
| `run_create_app_loop()` — cancelled flag exits early | #11 |
| `run_create_app_loop()` — drains pending_messages between rounds | #12 |
| `send_message()` — queues to pending_messages | #12 |
| `build_args()` — `--tools ""` empty string arg passed correctly | #13 |

#### Tests NOT needed (with justification)

| What | Why |
|---|---|
| Thinking block events | CLI manages thinking internally. Not in structured output. |
| Conversation history accumulation | CLI manages via `--session-id`/`--resume`. |
| SSE chunk parsing | CLI parses SSE internally. We only read NDJSON. |
| Request contract validation | CLI handles Anthropic API contract. |
| OAuth token priority test | Auth resolution same as pod sessions; tested in session_integration.rs. |

#### Coverage: 100% of touched lines

| Code path | Covered by | Tier |
|---|---|---|
| `cli_invoke.rs` — `invoke_cli()` spawn + read loop | #1–#5 | Integration |
| `cli_invoke.rs` — `invoke_cli()` parse structured_output | #1–#4 | Integration |
| `cli_invoke.rs` — `invoke_cli()` timeout path | Documented exception (requires 5-min test) | — |
| `create_app.rs` — `run_create_app_loop()` full flow | #1–#4 | Integration |
| `create_app.rs` — `execute_create_project()` | #2, #3 | Integration |
| `create_app.rs` — `execute_spawn_agent()` error path | #3 | Integration |
| `create_app.rs` — `format_tool_results()` in loop | #2–#4 | Integration |

### Verification
- `just test-unit` passes (no dead code warnings)
- `just test-integration` passes (all 10 new + updated tests green)
- `just lint` passes (no unused imports)
- `cargo build` succeeds
- Net deletion: ~2,724 LOC removed, ~590 LOC added

---

## Cross-Cutting Concerns

### Security
- [x] CLI has `--tools ""` — ZERO built-in tools, no Bash/file access
- [x] Server-side tool execution — same Rust code as today
- [x] Tool requests validated by name (enum in schema, also checked server-side)
- [x] OAuth token / API key passed via env vars only (never CLI args)
- [x] System prompt contains no secrets (no bearer tokens, no API keys)
- [x] CLI subprocess env-cleared, whitelisted vars only
- [x] `tokio::process::Command::args()` prevents shell injection (argv, not shell)
- [x] Subprocess timeout (300s) prevents hanging
- [x] Subprocess killed explicitly on exit (no `Drop` impl needed)
- [ ] **NEW**: Add `check_length("prompt", prompt, 1, 100_000)` in `execute_spawn_agent`
- [ ] **NEW**: Add length cap on `structured.text` (100K) before broadcast/save

### Auth & Permissions
- [x] `create_app` handler still uses `AuthUser` extractor
- [x] Permissions unchanged (project:write + agent:run)
- [x] Rate limiting unchanged (5 per 10 min)
- [x] Audit logging unchanged

### Observability
- [x] `tracing::instrument` on `create_global_session`, `run_create_app_loop`, `execute_create_app_tool`
- [x] ProgressEvents broadcast for all phases (text, tool call, tool result, completed, error)
- [x] No `.unwrap()` in production code
- [x] Sensitive data never logged

### Infrastructure updates (PR 2)
- [ ] Add `test-llm` to `justfile`
- [ ] Add LLM test tier to `CLAUDE.md` testing table
- [ ] Add `CLAUDE_CODE_OAUTH_TOKEN` / `ANTHROPIC_API_KEY` to `CLAUDE.md` env var table
- [ ] Update `docs/testing.md` with LLM test tier description

---

## LLM Test Tier

A new test tier using **real Claude CLI with real OAuth/API tokens** to verify actual LLM interactions end-to-end. These are in addition to unit + integration tests (which cover 100% via mock CLI).

### Infrastructure

**Test file:** `tests/llm_create_app.rs` — uses `#[ignore]` so they're excluded from normal test runs.

**Just command:**
```
just test-llm    # runs LLM tests (requires CLAUDE_CODE_OAUTH_TOKEN or ANTHROPIC_API_KEY)
```

**Justfile entry:**
```
test-llm:
    bash {{test_script}} --filter 'llm_*' --run-ignored
```

**Guard pattern** — tests skip gracefully if no token is available:
```rust
fn require_llm_token() -> (Option<String>, Option<String>) {
    let oauth = std::env::var("CLAUDE_CODE_OAUTH_TOKEN").ok();
    let api_key = std::env::var("ANTHROPIC_API_KEY").ok();
    if oauth.is_none() && api_key.is_none() {
        eprintln!("SKIP: no CLAUDE_CODE_OAUTH_TOKEN or ANTHROPIC_API_KEY set");
        return (None, None);  // test body checks and returns early
    }
    (oauth, api_key)
}
```

**Real CLI path** — LLM tests use the actual `claude` binary (NOT `CLAUDE_CLI_PATH` mock). The test verifies the real CLI is available:
```rust
fn require_claude_cli() -> PathBuf {
    which::which("claude").expect("claude CLI not found in PATH — install with: npm i -g @anthropic-ai/claude-code")
}
```

### LLM Tests

| # | Test | Validates |
|---|---|---|
| 1 | `llm_structured_output_text_only` | `-p "Say hello" --tools "" --json-schema <schema>` returns valid `StructuredResponse` with text and empty tools |
| 2 | `llm_structured_output_with_tool_request` | `-p "Create a project called test-app" --tools "" --json-schema <schema>` with create-app system prompt returns `create_project` in tools array |
| 3 | `llm_session_id_and_resume` | First call with `--session-id`, second with `--resume`. Claude remembers context from first call. |
| 4 | `llm_oauth_token_auth` | `CLAUDE_CODE_OAUTH_TOKEN` works with `-p --output-format stream-json --verbose` |
| 5 | `llm_ndjson_stream_format` | Real CLI output is valid NDJSON: system init → assistant → result. All parse correctly via `parse_cli_message()`. |
| 6 | `llm_result_has_structured_output` | Result message contains `structured_output` field matching the schema when `--json-schema` is used |
| 7 | `llm_tools_empty_disables_builtins` | `--tools ""` results in system init message with `tools: ["StructuredOutput"]` only (no Read, Write, Bash, etc.) |
| 8 | `llm_full_create_app_flow` | Full tool loop with real LLM: system prompt → Claude requests `create_project` → server executes → feeds result via `--resume` → Claude requests `spawn_coding_agent` or responds with text. Validates the real structured output round-trip. |

### Key design principles

- **No side effects on real infrastructure** — LLM tests that exercise `create_project` use a test DB (via `sqlx::test`) but do NOT create real K8s namespaces. Mock the K8s client or skip `setup_project_infrastructure()`.
- **Cost-aware** — each test invokes real Claude API calls. Keep prompts minimal. Use `--max-turns 1` where possible.
- **Timeout** — LLM calls can be slow. Use 60s timeout per test (`#[tokio::test(flavor = "multi_thread")]` with `tokio::time::timeout`).
- **Determinism** — LLM output is non-deterministic. Tests validate *structure* (valid JSON, correct schema shape, tools array present) not *content* (specific text).
- **Not in CI** — LLM tests are manual/opt-in. Not part of `just ci` or `just ci-full`. Run via `just test-llm` when tokens are available.

### CLAUDE.md updates

Add to the testing table:
```
| LLM      | 8     | ~60s   | OAuth/API key | `just test-llm`       | Manual, real CLI verification |
```

Add env var:
```
| `CLAUDE_CODE_OAUTH_TOKEN` | — | OAuth token for CLI auth (LLM tests + create-app) |
| `ANTHROPIC_API_KEY`       | — | Anthropic API key fallback |
```

---

## Dependency Graph

```
Plan 39 PR 4 (pub/sub bridge) ─┐
                                 ├──→ Plan 40 PR 1 (types + cli_invoke + wire create-app) → PR 2 (delete dead code + tests)
Plan 39 PR 1 (Valkey ACL) ─────┘
```

**Cross-plan dependency:** Plan 40 depends on Plan 39's pub/sub bridge (`pubsub_bridge.rs`) for event publishing, persistence, and SSE streaming. Specifically:
- `publish_event()` in `pubsub_bridge.rs` (added by this plan, extends Plan 39)
- `spawn_persistence_subscriber()` in `pubsub_bridge.rs` (from Plan 39 PR 4) — handles all event-to-DB persistence for BOTH pod agents and create-app sessions. Plan 40 does NOT call `save_assistant_message()` — events published via `publish_event()` are persisted by the subscriber.
- `subscribe_session_events()` in `pubsub_bridge.rs` (from Plan 39 PR 4) — read-only SSE forwarding
- SSE endpoint's pub/sub subscription path (from Plan 39 PR 4)

Plan 40 PR 1 adds all new code alongside the existing inprocess path (both work).
Plan 40 PR 2 removes the old code and replaces tests.

---

## Files Summary

### Deleted

| File | LOC | Reason |
|---|---|---|
| `src/agent/inprocess.rs` | 815 | Replaced by CLI tool loop |
| `src/agent/anthropic.rs` | 623 | Raw Anthropic API no longer needed |
| `tests/inprocess_integration.rs` | 986 | Replaced by CLI tests |
| `tests/mock_anthropic.rs` | ~300 | No longer needed |
| **Total** | **~2,724** | |

### Added

| File | Est. LOC | Purpose |
|---|---|---|
| `src/agent/cli_invoke.rs` | ~150 | Structured output CLI invocation |
| `src/agent/create_app_prompt.rs` | ~60 | System prompt for create-app |
| `src/agent/create_app.rs` | ~250 | Tool loop + tool execution (moved from inprocess.rs) |
| `tests/fixtures/mock-claude-cli.sh` | ~40 | Mock CLI for tests |
| `tests/cli_create_app_integration.rs` | ~400 | Integration tests (mock CLI) |
| `tests/llm_create_app.rs` | ~250 | LLM tests (real CLI, opt-in) |
| **Total** | **~1,100** | |

### Modified

| File | Change |
|---|---|
| `src/agent/mod.rs` | Remove `anthropic`, `inprocess`; add `cli_invoke`, `create_app_prompt`, `create_app` |
| `src/agent/service.rs` | Rewrite `create_global_session`, add helpers, remove inprocess branches |
| `src/agent/claude_cli/transport.rs` | Add `prompt`, `initial_session_id`, `json_schema`, `disable_tools` to `CliSpawnOptions`; conditional `--input-format` |
| `src/agent/claude_cli/messages.rs` | Add `structured_output` to `ResultMessage` |
| `src/agent/claude_cli/session.rs` | Remove transport + broadcast from handle, per-session invoke Mutex |
| `src/agent/pubsub_bridge.rs` | Add `publish_event()` for server-side event publishing (persistence handled by Plan 39's `spawn_persistence_subscriber()`) |
| `src/api/sessions.rs` | No changes needed — WebSocket→SSE conversion done in Plan 39 PR 4 |
| `src/store/mod.rs` | Remove `inprocess_sessions` from AppState |
| `src/main.rs` | Remove `inprocess_sessions` initialization |
| `tests/helpers/mod.rs` | Remove `inprocess_sessions` |
| `tests/e2e_helpers/mod.rs` | Remove `inprocess_sessions` |
| `tests/setup_integration.rs` | Remove `inprocess_sessions` |
| `tests/create_app_integration.rs` | Update for `cli_subprocess`, rename test |

**Net reduction: ~1,624 LOC** (2,724 deleted − 1,100 added)

---

## Test Plan Summary

### Coverage target: 100% of touched lines

Every new or modified line of code must be covered by at least one test
(unit, integration, or E2E). The test strategy above maps each code path
to a specific test. `review` and `finalize` will verify with `just cov-unit`
/ `just cov-total`.

### New test counts by PR

| PR | Unit | Integration | LLM | Total |
|---|---|---|---|---|
| PR 1 | 28 | 6 | 0 | 34 |
| PR 2 | 0 | 13 | 8 | 21 |
| **Total new** | **28** | **19** | **8** | **55** |
| **Tests deleted** | ~20 | 15 | 0 | ~35 |
| **Net** | **+8** | **+4** | **+8** | **+20** |

Note: LLM tests are opt-in (require real OAuth/API token, not in CI). Unit + integration tests cover 100% of touched lines via mock CLI.

### Coverage goals by module

| Module | Current tests | After plan |
|---|---|---|
| `src/agent/cli_invoke.rs` | 0 | +10 unit |
| `src/agent/create_app_prompt.rs` | 0 | +4 unit |
| `src/agent/create_app.rs` | 0 | +8 unit + 13 integration (via mock CLI) |
| `src/agent/claude_cli/transport.rs` | 27 unit | +6 unit (build_args new branches) |
| `src/agent/claude_cli/messages.rs` | 13 unit | +2 unit (structured_output) |
| `src/agent/claude_cli/session.rs` | 5 unit | +2 unit (active_process, cancelled) |
| `src/agent/pubsub_bridge.rs` | Plan 39 (incl. `spawn_persistence_subscriber`) | +2 unit (publish_event) |
| `tests/llm_create_app.rs` | 0 | +8 LLM (real CLI, opt-in) |
| `src/agent/inprocess.rs` | 16 unit | **DELETED** |
| `src/agent/anthropic.rs` | 15 unit | **DELETED** |

---

## Plan Review Findings

**Date:** 2026-03-03, Updated 2026-03-03 (pub/sub + CLI learnings)
**Status:** APPROVED WITH CONCERNS

### Update: Pub/Sub Replaces Broadcast Channels + SSE Replaces WebSocket (2026-03-03)

Based on deep debugging of the Claude CLI during Plan 38 implementation and alignment with Plan 39's pub/sub architecture:

1. **All `broadcast::Sender<ProgressEvent>` replaced with Valkey pub/sub** — Progress events now published to `session:{id}:events` via `pubsub_bridge::publish_event()`. This unifies the event transport: both create-app (Plan 40) and dev agent (Plan 38/39) sessions use the same pub/sub channels. Plan 39's `spawn_persistence_subscriber()` handles DB persistence for ALL session types — Plan 40 does NOT call `save_assistant_message()` (removed).

2. **`register_broadcast_only()` removed** — No longer needed. `CliSessionHandle` only tracks `active_process`, `cancelled`, and `pending_messages`.

3. **WebSocket replaced with SSE (Server-Sent Events)** — Plan 39 PR 4 deletes all WebSocket infrastructure (`ws_handler`, `handle_ws`, `stream_broadcast_to_ws`, `stream_pod_logs_to_ws`, `ws_handler_global`, `handle_ws_global`, `ReconnectingWebSocket` in `ws.ts`) and replaces with SSE endpoints. The SSE handler subscribes to Valkey pub/sub via `pubsub_bridge::subscribe_session_events()`. Client→server messages use existing REST POST endpoints (`POST .../message`). This removes the `axum` `"ws"` feature dependency and ~430 LOC of WebSocket infrastructure, replacing it with ~190 LOC of SSE code.

4. **CLI learnings documented** — Added "Critical CLI Learnings" section with discoveries about `--input-format stream-json` + piped stdin blocking, `env_clear()` requirements, and OAuth token handling.

### Codebase Reality Check — Issues Fixed In-Place

1. **`CliSpawnOptions.session_id` naming collision** — The plan originally added `session_id: Option<String>` to `CliSpawnOptions`, but `SubprocessTransport` already has an internal `session_id: Mutex<Option<String>>` tracking field. **Fixed:** Renamed to `initial_session_id` throughout the plan.

2. **`get_broadcast()` does not exist** on `CliSessionManager` — The plan called `state.cli_sessions.get_broadcast(session_id)` but the method doesn't exist. **Fixed:** Use `state.cli_sessions.get(session_id).await` to get `Arc<CliSessionHandle>`, then access `.tx` directly.

3. **`--input-format stream-json` conflicts with `-p` mode** — The current `build_args()` always emits `--input-format stream-json`, which is redundant/potentially conflicting in `-p` mode (stdin not used). **Fixed:** Plan now specifies conditional logic to skip `--input-format` when `prompt` is set.

4. **`service.rs` would exceed 1000 LOC** — Moving tool execution from `inprocess.rs` to `service.rs` (already ~800 LOC) would breach the 1000-line threshold. **Fixed:** Extracted to new `src/agent/create_app.rs` module.

5. **Missing `tests/setup_integration.rs`** from update list — This file also has `inprocess_sessions` in its AppState construction. **Fixed:** Added to PR 1 file change table.

6. **No subprocess timeout** in `invoke_cli()` — Could hang forever if CLI process stalls. **Fixed:** Added `tokio::time::timeout(300s)` wrapper around the read loop.

7. **No subprocess cleanup on exit** — `SubprocessTransport` has no `Drop` impl. **Fixed:** Added explicit `transport.kill()` after the read loop in `invoke_cli()`.

8. **Missing `save_assistant_message()` and `update_session_cost()`** — The plan called these but they don't exist anywhere. **Fixed:** `save_assistant_message()` removed entirely — Plan 39's `spawn_persistence_subscriber()` handles all event-to-DB persistence centrally. Only `update_session_cost()` added as a new helper (cost metadata comes from ResultMessage, not pub/sub events).

### Remaining Concerns

1. ~~**`--tools ""` empty string argument**~~ — **RESOLVED.** Real CLI output (2026-03-03) confirms `--tools ""` works correctly. System init shows `"tools": ["StructuredOutput"]` — the CLI replaces all built-in tools with a synthetic `StructuredOutput` tool for JSON schema mode. No fallback needed.

### Design Decision: Stop & Concurrent Send Semantics

**Key insight**: With `-p` mode, each invocation is a **separate process**. Claude CLI saves session state (conversation history) to disk after each completed turn. Between tool rounds, no process is running — just Rust code executing tools.

**Key constraint**: Multiple rapid sends while Claude is working must not corrupt session state or leave partially created resources. Killing mid-tool-execution (e.g., mid-`create_project`) could leave orphaned git repos or DB rows.

#### Architecture: Message Queue + Cancellation

```rust
pub struct CliSessionHandle {
    pub mode: SessionMode,
    pub session_id: Uuid,
    pub cli_session_id: Mutex<Option<String>>,
    /// Currently running subprocess (if any). Used for stop/kill.
    pub active_process: Mutex<Option<tokio::process::Child>>,
    /// Cancellation flag — checked between tool rounds.
    pub cancelled: AtomicBool,
    /// Queued user messages — drained between tool rounds or after tool loop finishes.
    pub pending_messages: Mutex<Vec<String>>,
}
```

**Note:** No `broadcast::Sender` — all progress events go through Valkey pub/sub (`session:{id}:events`). Plan 39's `spawn_persistence_subscriber()` writes them to `agent_messages`; the SSE endpoint subscribes via `subscribe_session_events()` (read-only, Plan 39 PR 4). Client→server messages use REST POST endpoints.

#### Stop behavior

1. User calls `stop_session()` → sets `cancelled = true`
2. If a CLI subprocess is currently running (during `invoke_cli()`), kill it via SIGTERM
3. The `run_create_app_loop()` checks `cancelled` between tool rounds and exits early
4. **Does NOT kill mid-tool-execution** — tool round completes, then loop exits
5. Session state on disk is intact for all completed turns
6. DB status → `'stopped'`, publish `ProgressKind::Completed` to pub/sub with "Session stopped by user"

```rust
async fn stop_cli_session(state: &AppState, session_id: Uuid) {
    if let Some(handle) = state.cli_sessions.get(session_id).await {
        handle.cancelled.store(true, Ordering::Relaxed);
        // Kill currently running CLI subprocess (not tool execution)
        if let Some(ref mut child) = *handle.active_process.lock().await {
            let _ = child.kill().await;
        }
    }
    state.cli_sessions.remove(session_id).await;
}
```

#### User sends messages while tool loop is running

**Design: queue-and-drain.** Messages are queued and processed after the current tool round completes. This is safer than interrupt-and-restart because:
- No risk of killing mid-tool-execution (partial project creation, etc.)
- Multiple rapid sends are naturally batched
- Claude sees the full conversation history via `--resume`

**Flow:**

```
User sends "Use React"
  → run_create_app_loop() running, Claude responds with create_project tool
  → Server executing create_project...

User sends "Also add TypeScript"    ← queued in pending_messages
User sends "And use Tailwind CSS"   ← queued in pending_messages

  → create_project finishes
  → Tool loop checks pending_messages: found 2 messages!
  → Drains & concatenates: "Also add TypeScript\n\nAnd use Tailwind CSS"
  → Feeds combined message via --resume (instead of tool results)
  → Claude sees all user messages and responds accordingly
```

**Implementation:**

In `send_message()`:
```rust
"cli_subprocess" => {
    // Queue the message — tool loop or a new invocation will pick it up
    if let Some(handle) = state.cli_sessions.get(session_id).await {
        handle.pending_messages.lock().await.push(content.to_owned());

        // If no tool loop is running (session idle), spawn a new --resume
        // The tool loop sets a "busy" flag; if not busy, we handle it directly.
        if !handle.is_busy() {
            let state_clone = state.clone();
            tokio::spawn(async move {
                run_pending_messages(&state_clone, session_id).await;
            });
        }
        // If busy, the tool loop will drain pending_messages after
        // the current round completes.
    }
    // Save to DB
    sqlx::query("INSERT INTO agent_messages (session_id, role, content) VALUES ($1, 'user', $2)")
        .bind(session_id).bind(content)
        .execute(&state.pool).await?;
}
```

In `run_create_app_loop()`, between tool rounds:
```rust
// 1. Check cancellation
if handle.cancelled.load(Ordering::Relaxed) {
    let _ = pubsub_bridge::publish_event(&state.valkey, session_id, &ProgressEvent {
        kind: ProgressKind::Completed,
        message: "Session stopped by user".into(),
        metadata: None,
    }).await;
    break;
}

// 2. Check for queued user messages (takes priority over tool results)
let pending = {
    let mut msgs = handle.pending_messages.lock().await;
    if !msgs.is_empty() {
        Some(msgs.drain(..).collect::<Vec<_>>().join("\n\n"))
    } else {
        None
    }
};

if let Some(user_messages) = pending {
    // User sent messages while we were working — feed them via --resume
    // instead of feeding tool results. Claude will see user's input.
    current_prompt = user_messages;
    is_resume = true;
    continue;  // Skip tool result feeding, go straight to --resume
}

// 3. No pending messages — feed tool results back as normal
if structured.tools.is_empty() {
    let _ = pubsub_bridge::publish_event(&state.valkey, session_id, &ProgressEvent {
        kind: ProgressKind::Completed,
        message: "Turn completed".into(),
        metadata: None,
    }).await;
    break;
}
// ... execute tools, format results, continue loop
```

This handles all scenarios:
- **Single send while idle**: spawns `--resume` directly
- **Single send while busy**: queued, picked up after current round
- **Multiple sends while busy**: all queued, concatenated, fed as one message
- **Stop while busy**: kills CLI process, loop exits between rounds

### Simplification — Applied

1. **PRs 1+2 combined** into a single PR 1. See updated dependency graph.

2. **`CliSessionHandle` no longer holds a persistent transport or broadcast channel** (see revised struct above). Each `-p` invocation creates its own `SubprocessTransport` within `invoke_cli()`. The handle tracks `active_process` (for kill), `pending_messages` (queue), and `cancelled` (stop flag). All progress events are published to Valkey pub/sub `session:{id}:events` → persisted to `agent_messages` by Plan 39's persistence subscriber → streamed to browser via read-only SSE subscriber. No in-process broadcast channels or manual DB writes needed.

### Security Notes — All Good

- `env_clear()` + whitelist confirmed correct in `build_env()`
- OAuth/API key passed via env vars only (never CLI args) ✓
- All handler-level auth/permission/rate-limit checks preserved (untouched in `create_app` handler) ✓
- Server-side tool validation: `check_name()` on project name, `parse_uuid_field()` on project_id ✓
- **Add** `check_length("prompt", prompt, 1, 100_000)` in `execute_spawn_agent` (missing in current code too)
- **Add** length cap on `structured.text` (100K) before broadcasting/saving (LLM output defense)
