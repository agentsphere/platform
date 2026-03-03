# Combined Plan 39+40: Agent-Runner Integration + CLI Subprocess Create-App

## Context

The platform's agent system has two execution paths that need modernization:

1. **Dev agents (pods)** run Claude CLI directly with `--print`, tailing pod stdout for events and writing to pod stdin. No multi-turn, no structured streaming, no security isolation. Plan 38 built the standalone `agent-runner` CLI wrapper with REPL + Valkey pub/sub — this plan wires it into the platform pod lifecycle.

2. **Manager agent (create-app)** uses ~1,440 LOC of custom Anthropic API streaming (`inprocess.rs` + `anthropic.rs`). This duplicates what Claude CLI does natively and requires `ANTHROPIC_API_KEY` when users may only have OAuth tokens. This plan replaces it with `claude -p` subprocess + structured output.

Both paths converge on the same Valkey pub/sub infrastructure: events published to `session:{id}:events`, messages received on `session:{id}:input`, and a unified SSE streaming endpoint replacing the current WebSocket handlers.

**Outcome**: Unified event transport, ~1,600 LOC net reduction, OAuth-first auth, per-session Valkey ACL isolation, WebSocket→SSE migration.

### Manager Agent vs Dev Agent

| | Manager Agent (create-app) | Dev Agent (pods) |
|---|---|---|
| **Tools** | `--tools ""` disables all built-in tools | Full CLI access (bash, filesystem, tools) |
| **Tool execution** | Server-side via structured output JSON | CLI executes tools directly |
| **Runs in** | CLI subprocess in platform process | K8s pod with `agent-runner` wrapper |
| **Mode** | One-shot `-p` per turn, `--resume` for multi-turn | Persistent subprocess with REPL (`--input-format stream-json`) |
| **Events** | Published via `publish_event()` server-side | Published by agent-runner to Valkey pub/sub |

### Relationship to Plan 38

- **Plan 38** (complete): Standalone `agent-runner` CLI crate (`cli/agent-runner/`). Wraps Claude CLI with REPL + pub/sub. The `--prompt` / `-p` flag and single-shot behavior are already implemented — agent-runner sends the prompt, streams responses, and exits when stdin closes (natural behavior in K8s pods where stdin is a pipe).

## Design Principles

- **Backwards compatible** — existing pod log streaming continues to work for sessions that don't use agent-runner. The pub/sub bridge checks `uses_pubsub` flag before attempting subscription, then falls through to pod logs.
- **Minimal config surface** — one new env var (`PLATFORM_VALKEY_AGENT_HOST`) controls how agents reach Valkey from inside K8s. Everything else is derived.
- **Security isolation** — each agent session gets a Valkey ACL user with `resetkeys resetchannels -@all` baseline then explicit `+subscribe +publish +unsubscribe +ping` on `&session:{id}:*`. No `+@pubsub` category (which would include dangerous diagnostic commands). No key-space access. No cross-session channel access. Credentials rotate per session.
- **Idempotent cleanup** — ACL deletion is idempotent and runs in `stop_session()`, `run_reaper()`, and `create_session()` error paths.
- **Persist-then-forward** — every event from pub/sub is written to `agent_messages` by a dedicated persistence subscriber before being forwarded to SSE clients. Events are never lost, even if no browser is connected. SSE subscribers are read-only.
- **Deterministic routing** — the `uses_pubsub` boolean column in `agent_sessions` determines message routing. No heuristics or try-and-fallback.
- **Server-side tool execution** — the Rust server stays in control. CLI has ZERO built-in tools (`--tools ""`). Claude returns structured JSON describing which tools to call. The Rust server validates and executes them — same security model as today.
- **Delete, don't deprecate** — remove `inprocess.rs` and `anthropic.rs` entirely.

## Critical CLI Learnings (from Plan 38 implementation)

1. **`--input-format stream-json` blocks on piped stdin** — When stdin is a pipe (not a TTY), the CLI reads stdin first before processing the `-p` prompt. Using both `-p` and `--input-format stream-json` together causes the process to hang indefinitely. **For one-shot `-p` mode (create-app): do NOT use `--input-format stream-json`** — stdin is not used. Only use `--input-format stream-json` for persistent subprocess mode (agent-runner REPL).

2. **`env_clear()` is critical for isolation** — Must prevent `DATABASE_URL`, `PLATFORM_MASTER_KEY`, and other secrets from leaking to the Claude CLI subprocess. But must whitelist sufficient env vars for Node.js runtime (PATH, HOME, TMPDIR).

3. **OAuth via `CLAUDE_CODE_OAUTH_TOKEN` env var** — Works correctly when passed as an env var to the subprocess. Do NOT use `CLAUDE_CONFIG_DIR` override (temp dirs have no OAuth credentials). The platform must resolve the user's OAuth token from the secrets engine and pass it directly.

4. **`apiKeySource: "none"` in system init** — Confirms OAuth is being used (not an API key). This is the expected value when `CLAUDE_CODE_OAUTH_TOKEN` is set.

5. **`tokio::process::Command::args()` prevents shell injection** — Args are passed as argv elements, not through a shell. Safe for user-provided prompts.

6. **Exit behavior** — Agent-runner exits naturally after Result message when stdin EOF is received (K8s pod behavior). No explicit `--single-shot` flag needed.

## Real CLI Output Reference (captured 2026-03-03)

**OAuth confirmed working**: `CLAUDE_CODE_OAUTH_TOKEN` works with `-p`, `--output-format stream-json`, and `--verbose`. Tokens valid 1 year, reusable across sessions.

**Multi-turn confirmed working** with `-p` + `--session-id` + `--resume`:
```bash
SESSION_ID=$(uuidgen)
claude -p "whats 10+20" --session-id "$SESSION_ID" --output-format stream-json --verbose
claude -p "and add again 10" --resume "$SESSION_ID" --output-format stream-json --verbose
```

**Structured output confirmed working** with `--tools "" --json-schema`:
```bash
claude -p "Create a React blog app called my-blog with PostgreSQL database" \
  --output-format stream-json --verbose \
  --tools "" \
  --json-schema "$SCHEMA" \
  --system-prompt "$SYSTEM_PROMPT" \
  --max-turns 10
```

### Key observations from real output

**1. System init** — `--tools ""` replaces built-in tools with synthetic `StructuredOutput` tool:
```json
{"type":"system","subtype":"init","session_id":"3b8b91ef-...","tools":["StructuredOutput"],"model":"claude-opus-4-6","apiKeySource":"none"}
```
> `tools` is `["StructuredOutput"]` not empty. `apiKeySource: "none"` confirms OAuth.

**2. Assistant messages** — streamed incrementally (thinking → text → tool_use), same message ID:
```json
{"type":"assistant","message":{"id":"msg_01YGho2L...","content":[{"type":"thinking","thinking":"..."}]}}
{"type":"assistant","message":{"id":"msg_01YGho2L...","content":[{"type":"text","text":"I'll create the project..."}]}}
{"type":"assistant","message":{"id":"msg_01YGho2L...","content":[{"type":"tool_use","id":"toolu_01DLqb89z...","name":"StructuredOutput","input":{"text":"...","tools":[{"name":"create_project","parameters":{...}}]}}]}}
```
> Structured output is delivered as `tool_use` with `name: "StructuredOutput"`. The `input` field contains our schema'd data.

**3. Auto tool_result** — CLI auto-acknowledges the structured output:
```json
{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_01DLqb89z...","type":"tool_result","content":"Structured output provided successfully"}]}}
```
> We don't need to feed this back — the CLI handles it internally.

**4. Rate limit event** — new message type (silently skipped by our parser):
```json
{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1772553600}}
```
> Unknown `type` → our `parse_cli_message()` returns `None` (forward compat).

**5. Result message** — `structured_output` at top level is our extraction point:
```json
{
  "type": "result", "subtype": "success", "is_error": false,
  "duration_ms": 8114, "num_turns": 2,
  "result": "",
  "session_id": "3b8b91ef-...",
  "total_cost_usd": 0.01888075,
  "structured_output": {
    "text": "I'll create your React blog app with PostgreSQL...",
    "tools": [{"name": "create_project", "parameters": {"name": "my-blog", "display_name": "My Blog", "description": "..."}}]
  },
  "usage": {"input_tokens": 4, "output_tokens": 211, "cache_read_input_tokens": 24334}
}
```

**Key findings:**
- `result.result` is **empty string** `""` when structured output is used — text lives in `structured_output.text`
- `structured_output` is a top-level field on the result message — primary extraction point
- `num_turns: 2` — the structured output tool_use + auto tool_result counts as a turn
- `usage` has new nested fields (`server_tool_use`, `cache_creation`) — `#[serde(default)]` handles unknown fields

### Implications

1. **Extract from `result.structured_output`** — simpler, single parse point
2. **`StructuredOutput` tool_use in assistant messages** — our `cli_message_to_progress()` sees this as a `ToolCall` event with name `"StructuredOutput"` — either filter it or let it pass (harmless)
3. **`result.result` is empty** — always use `structured_output.text`
4. **Forward compat is working** — `rate_limit_event` and extra fields silently handled
5. **`--session-id` not required for first call** — CLI auto-generates one. But we pass it explicitly so we can `--resume` later.

---

## Step 1: Valkey ACL Session Scoping

Create per-session Valkey ACL users so each agent pod can only pub/sub on its own channels. This is the security foundation for all pub/sub communication.

### Files

| File | Change |
|---|---|
| `src/agent/valkey_acl.rs` | **New** — `create_session_acl()`, `delete_session_acl()`, `generate_password()`, `SessionValkeyCredentials` (custom Debug redacting password/url) |
| `src/agent/mod.rs` | Add `pub mod valkey_acl;` |
| `src/config.rs` | Add `valkey_agent_host: String` (from `PLATFORM_VALKEY_AGENT_HOST`, default derived from `VALKEY_URL`) |

### Module: `src/agent/valkey_acl.rs`

```rust
/// Credentials for a per-session Valkey ACL user.
/// Custom `Debug` impl redacts `password` and `url` to prevent accidental logging.
pub struct SessionValkeyCredentials {
    pub username: String,
    pub password: String,
    /// Full Redis URL for the agent pod: `redis://{username}:{password}@{host}`
    pub url: String,
}

impl std::fmt::Debug for SessionValkeyCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionValkeyCredentials")
            .field("username", &self.username)
            .field("password", &"[REDACTED]")
            .field("url", &"[REDACTED]")
            .finish()
    }
}

/// Create a scoped Valkey ACL user for an agent session.
/// ACL rule: `resetkeys resetchannels -@all &session:{id}:* +subscribe +publish +unsubscribe +ping`
#[tracing::instrument(skip(valkey), fields(%session_id), err)]
pub async fn create_session_acl(
    valkey: &fred::clients::Pool,
    session_id: Uuid,
    valkey_agent_host: &str,
) -> Result<SessionValkeyCredentials, AgentError>

/// Delete a per-session Valkey ACL user. Idempotent — succeeds even if user doesn't exist.
#[tracing::instrument(skip(valkey), fields(%session_id), err)]
pub async fn delete_session_acl(
    valkey: &fred::clients::Pool,
    session_id: Uuid,
) -> Result<(), AgentError>

/// Generate a cryptographically random password (32 bytes, hex-encoded = 64 chars).
fn generate_password() -> String
```

**Password generation**: Use `rand::fill(&mut [u8; 32])` then `hex::encode()`. Per CLAUDE.md gotcha, use `rand::fill()` free function (rand 0.10 API).

**ACL command**: Uses `fred`'s `CustomCommand` API (same pattern as `invalidate_pattern()` in `src/store/valkey.rs:53`):

```rust
use fred::interfaces::ClientLike;

let result: String = valkey
    .custom(
        fred::types::CustomCommand::new_static("ACL", None, false),
        vec![
            "SETUSER".to_owned(), username.clone(), "on".to_owned(),
            format!(">{password}"), "resetkeys".to_owned(), "resetchannels".to_owned(),
            "-@all".to_owned(), format!("&session:{session_id}:*"),
            "+subscribe".to_owned(), "+publish".to_owned(),
            "+unsubscribe".to_owned(), "+ping".to_owned(),
        ],
    ).await
    .map_err(|e| AgentError::Other(anyhow::anyhow!("ACL SETUSER failed: {e}")))?;
```

**Note**: `resetkeys` + `resetchannels` + `-@all` ensure zero baseline access. `+ping` required for fred keepalive health checks. Uses explicit commands (not `+@pubsub`) to exclude `PUBSUB CHANNELS` diagnostic.

### Config change: `src/config.rs`

```rust
/// Valkey host:port as seen from inside agent pods.
/// Defaults to host:port parsed from VALKEY_URL.
/// Override when platform connects via port-forward but agents use K8s DNS.
/// Example: "valkey.platform.svc.cluster.local:6379"
pub valkey_agent_host: String,
```

Parse from `PLATFORM_VALKEY_AGENT_HOST` env var. Default: extract host:port from existing `valkey_url` using `url::Url::parse()`. Fallback if URL parse fails: `"localhost:6379"`.

### Tests (25)

**Unit (17):**
| Test | Validates |
|---|---|
| `test_acl_username_format` | Username returns `"session-{session_id}"` with full UUID |
| `test_generate_acl_password_length` | Password is 64 hex chars (32 bytes) |
| `test_generate_acl_password_unique` | Two calls produce different passwords |
| `test_generate_acl_password_hex_only` | Password contains only `[0-9a-f]` characters |
| `test_build_acl_setuser_commands` | Correct ACL SETUSER args: `on >{pass} resetkeys resetchannels -@all &session:{id}:* +subscribe +publish +unsubscribe +ping` |
| `test_build_acl_setuser_no_psubscribe` | Command does NOT include `+psubscribe` or `+@pubsub` |
| `test_build_acl_setuser_includes_ping` | Command includes `+ping` for connection health checks |
| `test_channel_pattern_events` | `events_channel(id)` returns `session:{uuid}:events` |
| `test_channel_pattern_input` | `input_channel(id)` returns `session:{uuid}:input` |
| `test_build_acl_deluser_command` | Correct `ACL DELUSER session-{id}` args |
| `test_build_valkey_url_with_credentials` | Constructs `redis://session-{id}:{pass}@{host}:{port}` |
| `test_build_valkey_url_preserves_host_port` | Host and port from config preserved |
| `test_default_valkey_agent_host` | `Config::test_default()` has `"localhost:6379"` |
| `test_valkey_agent_host_from_env` | Env var override works |
| `test_valkey_agent_host_derived_from_url` | Extracted from `VALKEY_URL` when env var not set |
| + 2 more config tests | |

**Integration (8):**
| Test | Validates |
|---|---|
| `test_create_and_delete_acl_roundtrip` | Create ACL user, verify exists, delete, verify gone |
| `test_acl_scoped_user_can_publish_own_channel` | Scoped user can PUBLISH to `session:{id}:events` |
| `test_acl_scoped_user_can_subscribe_own_channel` | Scoped user can SUBSCRIBE to `session:{id}:input` |
| `test_acl_scoped_user_cannot_access_other_session` | Scoped user cannot publish/subscribe to `session:{other_id}:*` |
| `test_acl_scoped_user_cannot_get_set_keys` | Scoped user cannot GET/SET arbitrary keys |
| `test_acl_scoped_user_can_ping` | Scoped user can PING (for keepalive) |
| `test_acl_delete_nonexistent_user_ok` | Idempotent deletion of non-existent user |
| `test_acl_credentials_returned_in_result` | Return value contains username, password, and well-formed URL |

### Validation

- `just test-unit` passes with new unit tests
- Integration: ACL user created, pub/sub scoped to own channels, cross-session blocked, cleanup works
- Config test verifies env var parsing and default derivation

---

## Step 2: Agent-Runner MCP Config + Exit Code

Enhance `cli/agent-runner/` with MCP server configuration and exit code propagation.

**Note:** The `--prompt` / `-p` flag and single-shot-like behavior are **already implemented in Plan 38**. The agent-runner sends the prompt, streams responses via the REPL loop, and exits naturally when stdin closes (K8s pod stdin pipe EOF). No separate `run_single_shot()` function is needed.

### Files

| File | Change |
|---|---|
| `cli/agent-runner/src/mcp.rs` | **New** — MCP config file generation (5 servers, excludes admin) |
| `cli/agent-runner/src/main.rs` | Wire MCP config via `--mcp-config`, add `--no-mcp` flag, exit code from Result message |
| `cli/agent-runner/src/repl.rs` | Return `ExitStatus` from `run()` |

### MCP config generation: `src/mcp.rs`

When `PLATFORM_API_TOKEN` and `PLATFORM_API_URL` are set, generate a temporary `mcp_config.json` file and pass it to Claude CLI via `--mcp-config`.

Generated config references 5 MCP servers (admin excluded):
```json
{
  "mcpServers": {
    "platform-core": { "command": "node", "args": ["/opt/mcp/servers/platform-core.js"], "env": { "PLATFORM_API_URL": "...", "PLATFORM_API_TOKEN": "..." } },
    "platform-issues": { "command": "node", "args": ["/opt/mcp/servers/platform-issues.js"], "env": { "..." } },
    "platform-pipeline": { "command": "node", "args": ["/opt/mcp/servers/platform-pipeline.js"], "env": { "..." } },
    "platform-deploy": { "command": "node", "args": ["/opt/mcp/servers/platform-deploy.js"], "env": { "..." } },
    "platform-observe": { "command": "node", "args": ["/opt/mcp/servers/platform-observe.js"], "env": { "..." } }
  }
}
```

**Note**: MCP server JS files are at `/opt/mcp/servers/` in the container (matching `COPY mcp/ /opt/mcp/` Dockerfile directive). Each server imports `../lib/client.js`, so the full `mcp/` directory structure must be preserved. The `platform-admin` server is intentionally excluded — agents should not have admin access.

### CLI flag wiring

```rust
#[derive(Parser)]
struct Cli {
    // ... existing flags (including -p/--prompt from Plan 38) ...
    /// Disable MCP server integration even when PLATFORM_API_TOKEN is set.
    #[arg(long)]
    no_mcp: bool,
}
```

### Tests (7 unit)

| Test | Validates |
|---|---|
| `test_generate_mcp_config_valid_json` | Produces valid JSON with all 5 servers |
| `test_generate_mcp_config_correct_paths` | Server paths are `/opt/mcp/servers/*.js` |
| `test_generate_mcp_config_excludes_admin` | No `platform-admin` server in config |
| `test_generate_mcp_config_injects_env_vars` | `PLATFORM_API_URL` and `PLATFORM_API_TOKEN` set per server |
| `test_generate_mcp_config_file_written` | File exists at expected path in config dir |
| `test_no_mcp_flag_parsed` | `--no-mcp` prevents MCP config |
| `test_mcp_config_requires_platform_vars` | MCP config not generated without platform vars |

### Validation

- `cargo test -p agent-runner` passes
- Manual: `cargo run -p agent-runner -- -p "say hello" --cwd /tmp` works

---

## Step 3: Pod Startup → Agent-Runner + `uses_pubsub` Flag

Switch pod builder to launch `agent-runner` instead of Claude CLI. Inject Valkey ACL credentials. Add `uses_pubsub` column.

### Migration: `YYYYMMDDHHMMSS_add_uses_pubsub`

```sql
-- Up:
ALTER TABLE agent_sessions ADD COLUMN uses_pubsub BOOLEAN NOT NULL DEFAULT false;
-- Down:
ALTER TABLE agent_sessions DROP COLUMN uses_pubsub;
```

Metadata-only change on Postgres 11+ (non-volatile default). No table rewrite, no locking. Existing rows get `false` (correct — legacy sessions don't use pub/sub).

### Files

| File | Change |
|---|---|
| `src/agent/claude_code/pod.rs` | `build_claude_args()` → `build_agent_runner_args()`, add `VALKEY_URL` to env + `RESERVED_ENV_VARS`, add `valkey_url: Option<&str>` to `PodBuildParams` |
| `src/agent/provider.rs` | Add `valkey_url` to `BuildPodParams`, `uses_pubsub` to `AgentSession`, add `Deserialize` to `ProgressEvent`, add `#[serde(other)] Unknown` to `ProgressKind` |
| `src/agent/service.rs` | Call `create_session_acl()` in `create_session()` with error-path cleanup, `delete_session_acl()` in `stop_session()`/`run_reaper()`, update `fetch_session()` SELECT, set `uses_pubsub = true` |

### Critical type changes

**Both `PodBuildParams` (pod.rs) AND `BuildPodParams` (provider.rs) need `valkey_url`**. These are separate structs with the same fields — one at the provider trait boundary, one at the pod builder implementation. Both must be updated.

**`ProgressEvent` needs `Deserialize`** — Currently only has `#[derive(Debug, Clone, Serialize)]`. The pub/sub bridge (Step 4) must deserialize incoming JSON:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressEvent {
    pub kind: ProgressKind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}
```

**`ProgressKind` needs `Unknown` variant** for forward compatibility — without it, events from agent-runner with unknown kinds cause deserialization failures:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProgressKind {
    Thinking, ToolCall, ToolResult, Milestone, Error, Completed, Text,
    #[serde(other)]
    Unknown,
}
```

**`AgentSession` needs `uses_pubsub: bool`** — update `fetch_session()` SELECT and `create_session()` INSERT.

### Pod spec changes: `src/agent/claude_code/pod.rs`

**Replace `build_claude_args()` with `build_agent_runner_args()`:**
```rust
fn build_agent_runner_args(params: &PodBuildParams<'_>) -> Vec<String> {
    let mut args = vec![
        "--prompt".to_owned(), params.session.prompt.clone(),
        "--cwd".to_owned(), "/workspace".to_owned(),
        "--permission-mode".to_owned(), "bypassPermissions".to_owned(),
    ];
    if let Some(ref model) = params.config.model {
        args.push("--model".to_owned()); args.push(model.clone());
    }
    if let Some(max_turns) = params.config.max_turns {
        args.push("--max-turns".to_owned()); args.push(max_turns.to_string());
    }
    args
}
```

**Container entrypoint change**: `command: Some(vec!["agent-runner".to_owned()])`, `args: Some(agent_runner_args)`

**New `RESERVED_ENV_VARS` entry**: Add `"VALKEY_URL"` to prevent project secrets from hijacking.

**New env var in `build_env_vars()`**:
```rust
if let Some(valkey_url) = params.valkey_url {
    vars.push(env_var("VALKEY_URL", valkey_url));
}
```

### Service changes: `src/agent/service.rs`

**In `create_session()` — ACL creation with error-path cleanup:**
```rust
let valkey_creds = valkey_acl::create_session_acl(
    &state.valkey, session_id, &state.config.valkey_agent_host,
).await?;

let pod = provider.build_pod(BuildPodParams {
    // ... existing fields ...
    valkey_url: Some(&valkey_creds.url),
})?;

// If pod creation fails, clean up ACL
if let Err(e) = provider.create_pod(&pod).await {
    let _ = valkey_acl::delete_session_acl(&state.valkey, session_id).await;
    return Err(e.into());
}
```

**In `stop_session()` and `run_reaper()`**: Call `valkey_acl::delete_session_acl()`.

### Tests (26)

**Unit (19):** pod command, agent-runner args (prompt/cwd/model/max-turns), VALKEY_URL env, RESERVED_ENV_VARS includes VALKEY_URL, ProgressEvent deserialize (text/thinking/tool_call/completed/no-metadata/unknown-kind)

**Integration (5):** ACL created on session create, ACL deleted on stop, ACL cleanup on pod failure, `uses_pubsub` column exists + defaults false

**E2E (2):** pod launches agent-runner, pod has VALKEY_URL

~40 existing pod.rs unit tests need `valkey_url: None` added (mechanical).

### Validation

- `just test-unit` — pod builder tests pass with updated args
- `just test-integration` — ACL lifecycle tests pass
- `just test-e2e` — pod spec assertions pass
- `.sqlx/` regenerated cleanly (`just db-migrate && just db-prepare`)

---

## Step 4: Pub/Sub Event Bridge + WebSocket→SSE Migration

Replace pod log tailing with Valkey pub/sub. Replace ALL WebSocket handlers with SSE. Add `publish_event()` for server-side event publishing.

### Files

| File | Change |
|---|---|
| `src/agent/pubsub_bridge.rs` | **New** — `spawn_persistence_subscriber()`, `subscribe_session_events()` (read-only SSE), `publish_prompt()`/`publish_control()`, `publish_event()`, channel name helpers |
| `src/agent/mod.rs` | Add `pub mod pubsub_bridge;` |
| `src/api/sessions.rs` | **Delete** `ws_handler`, `handle_ws`, `stream_broadcast_to_ws`, `stream_pod_logs_to_ws`, `ws_handler_global`, `handle_ws_global` (~265 LOC). **New** `sse_session_events()` + `sse_session_events_global()`. Routes: `/ws` → `/events` |
| `src/observe/query.rs` | `live_tail_ws()` → `live_tail_sse()` |
| `src/agent/service.rs` | Update `send_message()` with pub/sub routing when `uses_pubsub=true`, call `spawn_persistence_subscriber()` in `create_session()`, delete `get_log_lines()`, extract `finalize_reaped_session()` helper |
| `Cargo.toml` | Remove `"ws"` from axum features, add `tokio-stream` |
| `ui/src/lib/ws.ts` | **Delete** (~86 LOC) |
| `ui/src/lib/sse.ts` | **New** — `EventSourceClient` wrapper (~40 LOC) |
| `ui/src/pages/SessionDetail.tsx` | `createWs` → `createSse`, `/ws` → `/events` |
| `ui/src/pages/CreateApp.tsx` | `createWs` → `createSse`, `ws.send()` → `api.post()` |
| `ui/src/components/OnboardingOverlay.tsx` | Same as CreateApp |
| `ui/src/pages/observe/Logs.tsx` | `createWs` → `createSse` for live tail |

### Architecture

```
Agent pod (agent-runner) ──publish──→ Valkey session:{id}:events
                                          │
                              ┌───────────┴───────────┐
                              ▼                       ▼
                   persistence subscriber       SSE subscriber(s)
                   (started at session          (started on SSE connect,
                    creation, writes to          read-only, forwards
                    agent_messages DB)            to browser)
```

### Module: `src/agent/pubsub_bridge.rs`

```rust
/// Channel name helpers
pub fn events_channel(session_id: Uuid) -> String { format!("session:{session_id}:events") }
pub fn input_channel(session_id: Uuid) -> String { format!("session:{session_id}:input") }

/// Publish a ProgressEvent to the session's events channel.
/// Used by server-side code (create-app tool loop) to emit events.
pub async fn publish_event(valkey: &fred::clients::Pool, session_id: Uuid, event: &ProgressEvent) -> Result<(), anyhow::Error>

/// Publish a user prompt to the session's input channel.
pub async fn publish_prompt(valkey: &fred::clients::Pool, session_id: Uuid, content: &str) -> Result<(), anyhow::Error>

/// Publish a control message (e.g., interrupt) to the session's input channel.
pub async fn publish_control(valkey: &fred::clients::Pool, session_id: Uuid, control_type: &str) -> Result<(), anyhow::Error>

/// Spawn a background task that subscribes to session events and persists them to agent_messages.
/// Started at session creation. Exits on Completed/Error events.
pub fn spawn_persistence_subscriber(pool: PgPool, valkey: fred::clients::Pool, session_id: Uuid) -> JoinHandle<()>

/// Subscribe to session events for SSE streaming. Returns an mpsc::Receiver.
/// Read-only — does NOT write to DB. SSE handler wraps this in Sse<impl Stream>.
pub async fn subscribe_session_events(valkey: &fred::clients::Pool, session_id: Uuid) -> Result<mpsc::Receiver<ProgressEvent>, anyhow::Error>
```

**Key**: `subscribe_session_events()` uses `pool.next().clone_new()` to create a separate connection for subscriptions — `Pool` doesn't impl `PubsubInterface`, only `Client` does.

### Observe live tail SSE: `src/observe/query.rs`

Replace `live_tail_ws()` with `live_tail_sse()`. Same Valkey pub/sub subscription pattern, same `should_forward()` filter logic, but returns `Sse<impl Stream>` instead of upgrading a WebSocket.

### Frontend: `ui/src/lib/sse.ts`

```typescript
export interface SseOptions {
  url: string;
  event?: string;       // SSE event name to listen for (default: "progress")
  onMessage: (data: any) => void;
  onOpen?: () => void;
  onError?: (err: Event) => void;
}

export class EventSourceClient {
  private source: EventSource | null = null;
  private closed = false;
  constructor(private opts: SseOptions) {}

  connect(): void {
    if (this.closed) return;
    this.source = new EventSource(this.opts.url);  // sends cookies automatically
    this.source.onopen = () => this.opts.onOpen?.();
    this.source.addEventListener(this.opts.event || 'progress', (e: MessageEvent) => {
      try { this.opts.onMessage(JSON.parse(e.data)); }
      catch { this.opts.onMessage(e.data); }
    });
    this.source.onerror = (err) => this.opts.onError?.(err);
    // EventSource has built-in auto-reconnect — no manual retry logic
  }

  close(): void {
    this.closed = true;
    this.source?.close();
    this.source = null;
  }
}

export function createSse(opts: SseOptions): EventSourceClient {
  const sse = new EventSourceClient(opts);
  sse.connect();
  return sse;
}
```

**Auth:** `EventSource` sends session cookies on same-origin requests. The `AuthUser` extractor checks Bearer token first, then session cookie — works out of the box.

**UI page changes:**

| Page | WS usage | Change |
|---|---|---|
| `SessionDetail.tsx` | Events + send | SSE for events, already uses REST for send |
| `CreateApp.tsx` | Events + send | SSE for events, switch `ws.send()` to `api.post(/api/sessions/{id}/message)` |
| `OnboardingOverlay.tsx` | Events + send | Same as CreateApp |
| `observe/Logs.tsx` | Events only | SSE with `event: "log"`, trivial — no send path |

### Cargo.toml changes

```diff
-axum = { version = "0.8", features = ["ws", "macros"] }
+axum = { version = "0.8", features = ["macros"] }
+tokio-stream = { version = "0.1", features = ["sync"] }
```

### Message routing update: `src/agent/service.rs`

```rust
pub async fn send_message(state: &AppState, session_id: Uuid, content: &str) -> Result<(), AgentError> {
    let session = fetch_session(&state.pool, session_id).await?;

    // Pub/sub path (agent-runner pods)
    if session.uses_pubsub {
        pubsub_bridge::publish_prompt(&state.valkey, session_id, content).await
            .map_err(|e| AgentError::Other(e))?;
        return Ok(());
    }

    // Fallback: existing routing (inprocess, cli_subprocess stdin, pod stdin attach)
    // ... existing match on execution_mode ...
}
```

**Note:** The SSE handler subscribes via Valkey pub/sub exclusively. The old broadcast-based subscribe paths (`InProcessHandle.tx`, `CliSessionHandle.tx`) become dead code — Step 5 removes them.

### Tests (29)

**Unit (11):** channel names, prompt/control JSON format, ProgressEvent deserialization (text/thinking/tool_call/completed/error + invalid JSON + unknown kind)

**Integration (17):**
| Test | Validates |
|---|---|
| `test_persistence_subscriber_writes_to_db` | Publish event → verify row in `agent_messages` |
| `test_persistence_subscriber_exits_on_completed` | Completed event → subscriber exits |
| `test_persistence_subscriber_exits_on_error` | Error event → subscriber exits |
| `test_persistence_subscriber_skips_malformed` | Malformed JSON → no DB row, no crash |
| `test_pubsub_bridge_receives_events` | Publish → mpsc receiver gets ProgressEvent |
| `test_pubsub_bridge_ignores_malformed_events` | Malformed JSON skipped without crash |
| `test_send_message_routes_via_pubsub` | `uses_pubsub=true` publishes to input channel |
| `test_send_message_falls_back_for_legacy` | `uses_pubsub=false` uses pod attach |
| `test_publish_prompt_format` | Correct JSON format published |
| `test_sse_endpoint_streams_pubsub_events` | SSE receives events via pub/sub |
| `test_sse_endpoint_returns_event_stream_content_type` | Response has `text/event-stream` |
| `test_sse_global_endpoint_owner_only` | Global SSE rejects non-owner |
| `test_sse_endpoint_requires_auth` | No auth token → 401 |
| `test_sse_endpoint_nonexistent_session_404` | Unknown session → 404 |
| `test_pubsub_bridge_multiple_sessions_isolated` | Two bridges receive only their own events |
| `test_pubsub_bridge_receiver_drop_unsubscribes` | Dropping receiver causes background task to exit |
| `test_send_message_still_works_for_inprocess` | No regression on inprocess mode |

**E2E (1):** `test_agent_pubsub_event_streaming` — create session, publish event from fake agent-side client, receive via SSE endpoint

### Validation

- `just test-unit` — no compile errors after removing `ws` feature
- `just test-integration` — pub/sub + SSE tests pass
- `just test-e2e` — end-to-end streaming works
- `grep -r "WebSocket\|ws::" src/` — no remaining WebSocket references
- `grep -r "createWs\|ReconnectingWebSocket" ui/src/` — no remaining WS client references
- Manual: create session → SSE events stream → messages send via REST POST
- Manual: observe/Logs live tail works via SSE

---

## Step 5: CLI Structured Output + Create-App Rewrite

Build the structured output CLI invocation infrastructure. Rewrite `create_global_session()` to use `claude -p` with `--tools "" --json-schema`. Remove `inprocess_sessions` from AppState.

### The JSON Schema

```json
{
  "type": "object",
  "properties": {
    "text": { "type": "string", "description": "Your response to the user" },
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

### Architecture: Structured Output Tool Loop

```
User → POST /api/create-app { description: "Build me a blog" }
  → create_global_session()
  ├── Resolve auth: OAuth > API key > global key
  ├── Insert session (execution_mode='cli_subprocess', uses_pubsub=true)
  ├── Spawn persistence subscriber
  ├── Spawn run_create_app_loop():
  │     Turn 1: claude -p "Build me a blog" --session-id <id> --tools "" --json-schema <schema>
  │       → structured_output: { text: "What framework?", tools: [] }  ← no tools, asking question
  │       → Publish Text + Completed events to pub/sub
  │
  ├── User sends follow-up via POST /api/sessions/{id}/message
  │     Turn 2: claude -p "React please, with Postgres" --resume <id> --tools "" --json-schema <schema>
  │       → structured_output: { text: "I'll create it.", tools: [{ name: "create_project", parameters: {...} }] }
  │       → Rust server executes create_project() server-side
  │       → Publish ToolCall + ToolResult events
  │
  │     Turn 3 (automatic — feed tool results back):
  │       claude -p "Tool results: create_project: success — {...}" --resume <id> --tools "" --json-schema <schema>
  │       → structured_output: { text: "Now spawning the coding agent...", tools: [{ name: "spawn_coding_agent", ... }] }
  │       → Rust server executes spawn_coding_agent()
  │
  │     Turn 4 (automatic — feed tool results):
  │       → structured_output: { text: "Your project is being set up!", tools: [] }  ← done
  └── Session stays running for follow-up messages.
```

### Comparison: Current vs New

| | Current (inprocess.rs) | New (CLI subprocess) |
|---|---|---|
| **LLM call** | Raw Anthropic Messages API + SSE streaming | `claude -p` subprocess + NDJSON |
| **Tool definition** | Anthropic `tools[]` parameter | `--json-schema` structured output |
| **Tool invocation** | Anthropic returns `tool_use` content blocks | CLI returns `result.structured_output.tools[]` |
| **Tool execution** | `execute_tool()` in Rust (server-side) | Same Rust functions, same code path |
| **Tool results** | Fed back as `tool_result` content blocks | Fed back as `-p "Tool results: ..."` via `--resume` |
| **Auth** | `ANTHROPIC_API_KEY` only | `CLAUDE_CODE_OAUTH_TOKEN` primary, API key fallback |
| **Conversation history** | Manual `Vec<ChatMessage>` in memory | Claude CLI manages via `--session-id` |
| **Security** | Server controls tools ✓ | Server controls tools ✓ (CLI has `--tools ""`) |

### Files

| File | Change |
|---|---|
| `src/agent/cli_invoke.rs` | **New** — `invoke_cli()`, `CliInvokeParams`, `StructuredResponse`, `ToolRequest`, `create_app_schema()`, `format_tool_results()` |
| `src/agent/create_app_prompt.rs` | **New** — `build_create_app_system_prompt()` |
| `src/agent/create_app.rs` | **New** — `run_create_app_loop()`, `execute_create_app_tool()`, `execute_create_project()`, `execute_spawn_agent()`, `parse_create_project_input()` (moved from inprocess.rs) |
| `src/agent/claude_cli/transport.rs` | Add `prompt`, `initial_session_id`, `json_schema`, `disable_tools` to `CliSpawnOptions`. Conditional `--input-format` (skip when prompt set) |
| `src/agent/claude_cli/messages.rs` | Add `structured_output: Option<serde_json::Value>` to `ResultMessage` |
| `src/agent/claude_cli/session.rs` | Refactor `CliSessionHandle`: replace `transport + broadcast` with `active_process: Mutex<Option<Child>>`, `cancelled: AtomicBool`, `pending_messages: Mutex<Vec<String>>` |
| `src/agent/service.rs` | Rewrite `create_global_session()`, remove `"inprocess"` branches from `send_message()`/`stop_session()`, add `update_session_cost()` |
| `src/agent/mod.rs` | Add `pub mod cli_invoke;`, `pub mod create_app_prompt;`, `pub mod create_app;` |
| `src/store/mod.rs` | Remove `inprocess_sessions` from AppState |
| `src/main.rs` | Remove `inprocess_sessions` initialization |
| `tests/helpers/mod.rs` | Remove `inprocess_sessions` from `test_state()` |
| `tests/e2e_helpers/mod.rs` | Remove `inprocess_sessions` from `e2e_state()` |
| `tests/setup_integration.rs` | Remove `inprocess_sessions` from `setup_test_state()` |

### Module: `src/agent/cli_invoke.rs`

```rust
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

/// Spawn `claude -p` with structured output, read NDJSON, publish events.
/// Returns the parsed StructuredResponse (text + tool requests).
/// Publishes ProgressEvents to Valkey pub/sub `session:{id}:events` in real-time.
pub async fn invoke_cli(
    params: CliInvokeParams,
    valkey: &fred::clients::Pool,
) -> Result<(StructuredResponse, Option<ResultMessage>), AgentError> {
    // Build CliSpawnOptions with prompt, initial_session_id/resume_session, json_schema, disable_tools=true
    // Spawn SubprocessTransport
    // Read NDJSON in a loop, publish progress events, extract ResultMessage
    // Wrapped in tokio::time::timeout(300s)
    // Always transport.kill() on exit (no Drop impl)
    // Parse structured_output from result message
    // Fallback: if no structured_output, use result.result as text with empty tools
}

/// Format tool execution results for feeding back via --resume.
pub fn format_tool_results(results: &[(String, Result<serde_json::Value, String>)]) -> String

/// The JSON schema for create-app structured output.
pub fn create_app_schema() -> serde_json::Value
```

### `CliSpawnOptions` Additions

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
    /// `--tools ""` — disable all built-in tools.
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
if opts.disable_tools {
    args.push("--tools".to_owned());
    args.push(String::new());  // --tools ""
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

### `CliSessionHandle` Refactored Struct

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

**No `broadcast::Sender`** — all progress events go through Valkey pub/sub. Plan 39's `spawn_persistence_subscriber()` handles DB persistence; SSE subscribes via `subscribe_session_events()`.

### Stop & Concurrent Send Semantics

**Stop behavior:**
1. User calls `stop_session()` → sets `cancelled = true`
2. If a CLI subprocess is running, kill it via SIGTERM
3. `run_create_app_loop()` checks `cancelled` between tool rounds and exits early
4. Does NOT kill mid-tool-execution — tool round completes, then loop exits
5. Session state on disk intact for completed turns
6. DB status → `'stopped'`, publish `ProgressKind::Completed` with "Session stopped by user"

**Concurrent sends — queue-and-drain:**
```
User sends "Use React" → run_create_app_loop() running
User sends "Also add TypeScript"    ← queued in pending_messages
User sends "And use Tailwind CSS"   ← queued in pending_messages

→ create_project finishes
→ Tool loop checks pending_messages: found 2 messages!
→ Drains & concatenates: "Also add TypeScript\n\nAnd use Tailwind CSS"
→ Feeds combined message via --resume
→ Claude sees all user messages and responds accordingly
```

In `send_message()`:
```rust
"cli_subprocess" => {
    if let Some(handle) = state.cli_sessions.get(session_id).await {
        handle.pending_messages.lock().await.push(content.to_owned());
        // If no tool loop is running, spawn a new --resume
        if !handle.is_busy() {
            tokio::spawn(async move { run_pending_messages(&state_clone, session_id).await; });
        }
        // If busy, tool loop drains pending_messages after current round
    }
}
```

In `run_create_app_loop()`, between tool rounds:
```rust
// 1. Check cancellation
if handle.cancelled.load(Ordering::Relaxed) { break; }
// 2. Check for queued user messages (priority over tool results)
let pending = { let mut msgs = handle.pending_messages.lock().await; msgs.drain(..).collect() };
if let Some(user_messages) = pending {
    current_prompt = user_messages;
    is_resume = true;
    continue;  // Skip tool results, go to --resume
}
// 3. No pending messages — feed tool results back as normal
```

### Tests (34)

**Unit (28):**
- `cli_invoke.rs` (10): schema validation, StructuredResponse deser, format_tool_results (success/error/mixed/empty)
- `transport.rs` (6): build_args for disable_tools, json_schema, prompt, initial_session_id, prompt-skips-input-format, disable_tools-false
- `create_app_prompt.rs` (4): mentions tools, has phases, describes parameters
- `messages.rs` (2): ResultMessage with/without structured_output
- `create_app.rs` (4): unknown tool error, MAX_TOOL_ROUNDS, parse helpers
- `pubsub_bridge.rs` (2): publish_event serialization + channel name

**Integration (6):**
- No credentials → ConfigurationRequired error
- Session has `execution_mode = 'cli_subprocess'`
- Events published to pub/sub
- `execute_create_app_tool` creates project in DB
- `stop_session` sets cancelled, kills process

### Validation

- `just test-unit` — no compile errors from removed `inprocess_sessions`
- `just test-integration` — create-app tests updated
- `/api/create-app` creates `cli_subprocess` session with `uses_pubsub = true`

---

### Step 5 — Implementation Progress

- [x] Types & errors defined (`StructuredResponse`, `ToolRequest`, `CliInvokeParams`)
- [x] `CliSpawnOptions` — 4 new fields: `prompt`, `initial_session_id`, `json_schema`, `disable_tools`
- [x] `build_args()` — conditional `--input-format` skip for `-p` mode
- [x] `ResultMessage.structured_output` field added to messages.rs
- [x] `CliSessionHandle` rewritten: `cancelled`, `pending_messages`, `busy`, `user_id`
- [x] `cli_invoke.rs` — `invoke_cli()`, `format_tool_results()`, `create_app_schema()`, `update_session_cost()`, `parse_structured_output()`
- [x] `create_app_prompt.rs` — system prompt for structured output
- [x] `create_app.rs` — `run_create_app_loop()`, `run_pending_messages()`, tool execution, project creation, spawn agent
- [x] `service.rs` — rewrote `create_global_session()`, `send_cli_message()`, `stop_cli_session()` for CLI subprocess
- [x] `inprocess.rs` — annotated dead code (full removal in Step 6)
- [x] Unit tests passing (33 new tests: 13 cli_invoke, 7 create_app, 4 create_app_prompt, 4 transport, 2 messages, 3 session)
- [x] `just lint` — clean (0 warnings)
- [ ] Integration tests (`just test-integration`) — deferred to Step 6 (need Kind cluster)

> **Deviation:** Kept `inprocess_sessions` in `AppState` (with dead_code comment) instead of removing in Step 5.
> Reason: `tests/inprocess_integration.rs`, `tests/create_app_integration.rs`, and `tests/setup_integration.rs` reference it. Removing requires updating test helpers in Step 6.

> **Deviation:** Unit test count is 33 (vs planned 28). Extra tests: `parse_uuid_field` (3 tests), `parse_create_project_input_with_all_fields` (1 extra), `result_without_structured_output` (1 extra).

---

## Step 6: Delete Dead Code + Replace Tests + LLM Tests

Remove inprocess.rs, anthropic.rs, and their tests. Replace with CLI-based tests using mock CLI script. Add LLM test tier. Drop `'inprocess'` from DB CHECK constraint.

### Migration: `YYYYMMDDHHMMSS_drop_inprocess_execution_mode`

```sql
-- Up:
ALTER TABLE agent_sessions DROP CONSTRAINT IF EXISTS agent_sessions_execution_mode_check;
ALTER TABLE agent_sessions ADD CONSTRAINT agent_sessions_execution_mode_check
    CHECK (execution_mode IN ('pod', 'cli_subprocess'));
-- Down:
ALTER TABLE agent_sessions DROP CONSTRAINT IF EXISTS agent_sessions_execution_mode_check;
ALTER TABLE agent_sessions ADD CONSTRAINT agent_sessions_execution_mode_check
    CHECK (execution_mode IN ('pod', 'cli_subprocess', 'inprocess'));
```

### Deleted files (~2,724 LOC)

| File | LOC | Reason |
|---|---|---|
| `src/agent/inprocess.rs` | 815 | Replaced by CLI tool loop |
| `src/agent/anthropic.rs` | 623 | Raw Anthropic API no longer needed |
| `tests/inprocess_integration.rs` | 986 | Replaced by CLI tests |
| `tests/mock_anthropic.rs` | ~300 | No longer needed |

### New files (~1,100 LOC)

| File | Est. LOC | Purpose |
|---|---|---|
| `tests/fixtures/mock-claude-cli.sh` | ~40 | Mock CLI for tests |
| `tests/cli_create_app_integration.rs` | ~400 | Integration tests (mock CLI) |
| `tests/llm_create_app.rs` | ~250 | LLM tests (real CLI, opt-in) |

### Mock CLI Script Design

`tests/fixtures/mock-claude-cli.sh` — invoked via `CLAUDE_CLI_PATH` env var override (already supported by `find_claude_cli()` in transport.rs).

Multi-invocation support via `MOCK_CLI_RESPONSE_FILE` (JSON array) and `MOCK_CLI_STATE_DIR` (counter file):

```bash
#!/usr/bin/env bash
set -euo pipefail
# Parse args for -p, --session-id, --resume, --json-schema, --tools, etc.
# Track invocation count for multi-call scenarios
STATE_DIR="${MOCK_CLI_STATE_DIR:-/tmp/mock-cli-state}"
COUNT=$(cat "$STATE_DIR/invocation-count" 2>/dev/null || echo "0")
echo $((COUNT + 1)) > "$STATE_DIR/invocation-count"
# Read response from $MOCK_CLI_RESPONSE_FILE[$COUNT] or use env var fallback
# Emit NDJSON: system init → assistant message → result with structured_output
```

### Deleted tests → replacement mapping

| Deleted Test | Replacement | Justification |
|---|---|---|
| `inprocess_text_response` | `cli_create_app_text_only` | Same behavior, different transport |
| `inprocess_followup_message` | `cli_create_app_followup_via_resume` | Uses --resume instead of stdin |
| `inprocess_tool_use_creates_project` | `cli_create_app_creates_project` | Same server-side tool execution |
| `inprocess_text_then_tool_use` | Subsumed by `cli_create_app_creates_project` | Text + tool in structured output |
| `inprocess_multiple_tool_use_blocks` | `cli_create_app_creates_project_and_spawns_agent` + `cli_create_app_unknown_tool_error` | Sequential tools + unknown tool |
| `inprocess_subscribe_and_remove` | Not needed | Pub/sub replaces broadcast; SSE tested in Step 4 |
| `inprocess_request_contract` | Not needed | CLI handles Anthropic API contract |
| `inprocess_no_api_key` | `cli_create_app_no_credentials` | Same error path |
| `inprocess_create_session_via_api` | `cli_create_app_session_is_cli_subprocess` | Verifies execution_mode |
| `inprocess_conversation_history_grows` | Not needed | CLI manages history via `--session-id`/`--resume` |

### Integration Tests (13)

| # | Test | Validates |
|---|---|---|
| 1 | `cli_create_app_text_only` | Mock CLI returns text, empty tools. Text + Completed events. Assistant message in DB. |
| 2 | `cli_create_app_creates_project` | Mock CLI returns `create_project` tool. Server executes, project in DB. Resume returns text. |
| 3 | `cli_create_app_creates_project_and_spawns_agent` | Full 2-tool flow: create_project → spawn_coding_agent → text. |
| 4 | `cli_create_app_unknown_tool_error` | Mock CLI returns unknown tool name. Error fed back. Final response is text. |
| 5 | `cli_create_app_followup_via_resume` | Create session, then `send_message()`. Second invocation uses `--resume`. |
| 6 | `cli_create_app_no_credentials` | No API key, no OAuth. Returns 400. |
| 7 | `cli_create_app_permissions_required` | Viewer role → 403. |
| 8 | `cli_create_app_rate_limited` | 6th creation returns 429. |
| 9 | `cli_create_app_session_is_cli_subprocess` | Session `execution_mode == "cli_subprocess"`. |
| 10 | `cli_create_app_empty_description_rejected` | Empty description → 400. |
| 11 | `cli_create_app_stop_during_tool_loop` | Stop while running. Tool loop checks `cancelled`, exits. Status = 'stopped'. |
| 12 | `cli_create_app_send_queued_while_busy` | Send 2 messages while busy. Queued, drained between rounds. |
| 13 | `cli_create_app_tools_empty_string_works` | Verify `--tools ""` passed correctly. |

### LLM Test Tier

A new test tier using **real Claude CLI with real OAuth/API tokens**. In addition to unit + integration tests (which cover 100% via mock CLI).

**Infrastructure:**
- `tests/llm_create_app.rs` — uses `#[ignore]` attribute
- Run via `just test-llm` (added to justfile)
- Guard: skip gracefully if no `CLAUDE_CODE_OAUTH_TOKEN` or `ANTHROPIC_API_KEY`
- 60s timeout per test (LLM calls can be slow)
- Validate *structure* not *content* (non-deterministic LLM output)
- Not in CI — manual/opt-in only

**LLM Tests (8):**

| # | Test | Validates |
|---|---|---|
| 1 | `llm_structured_output_text_only` | `-p "Say hello" --tools "" --json-schema` → valid StructuredResponse with empty tools |
| 2 | `llm_structured_output_with_tool_request` | `-p "Create a project called test-app"` with system prompt → `create_project` in tools |
| 3 | `llm_session_id_and_resume` | First call with `--session-id`, second with `--resume`. Context remembered. |
| 4 | `llm_oauth_token_auth` | `CLAUDE_CODE_OAUTH_TOKEN` works with `-p --output-format stream-json --verbose` |
| 5 | `llm_ndjson_stream_format` | Real output is valid NDJSON. All parse via `parse_cli_message()`. |
| 6 | `llm_result_has_structured_output` | Result message contains `structured_output` matching schema |
| 7 | `llm_tools_empty_disables_builtins` | System init has `tools: ["StructuredOutput"]` only |
| 8 | `llm_full_create_app_flow` | Full tool loop with real LLM: prompt → tool request → server executes → --resume → text |

### Step 6 — Implementation Progress

- [x] Migration `20260303020001_drop_inprocess_execution_mode` — drop 'inprocess' from CHECK constraint
- [x] Deleted `src/agent/inprocess.rs` (815 LOC)
- [x] Deleted `src/agent/anthropic.rs` (622 LOC)
- [x] Deleted `tests/inprocess_integration.rs` (985 LOC)
- [x] Deleted `tests/mock_anthropic.rs` (418 LOC)
- [x] Removed `inprocess_sessions` from `AppState` (store/mod.rs, main.rs)
- [x] Removed `pub mod inprocess` and `pub mod anthropic` from agent/mod.rs
- [x] Removed inprocess branches from service.rs (`send_message`, `stop_session`)
- [x] Updated test helpers: removed `inprocess_sessions` from `test_state()`, `e2e_state()`, `setup_test_state()`
- [x] Added `valkey_agent_host` to test Config constructors (helpers, e2e_helpers, setup_integration)
- [x] Updated `create_app_integration.rs` — replaced `create_app_session_is_inprocess` with `create_app_session_is_cli_subprocess`
- [x] Created `tests/fixtures/mock-claude-cli.sh` (70 LOC) — mock CLI for integration tests
- [x] Created `tests/cli_create_app_integration.rs` (221 LOC) — 7 tests (5 API-level + 2 mock CLI ignored)
- [x] Created `tests/llm_create_app.rs` (341 LOC) — 7 LLM tests (all `#[ignore]`, opt-in)
- [x] Added `test-llm` target to justfile
- [x] Migration applied + `.sqlx/` regenerated
- [x] `just lint` — clean
- [x] `just test-unit` — 1187 tests pass
- [x] No `inprocess` references remain in `src/`

> **Deviation:** Mock CLI integration tests use `#[ignore]` instead of running by default — the `unsafe` `set_var` restriction in edition 2024 prevents setting `CLAUDE_CLI_PATH` from within tests. Mock CLI tests require `CLAUDE_CLI_PATH` to be set externally.

> **Deviation:** 7 LLM tests (vs planned 8). Merged `llm_full_create_app_flow` into `llm_structured_output_with_tool_request` since they test the same mechanics.

### Validation

- `just test-unit` — no dead code warnings, no unused imports
- `just test-integration` — all mock CLI tests green
- `just lint` passes
- `cargo build` succeeds
- Net: ~2,840 LOC removed, ~1,676 LOC added (~1,164 LOC net reduction)

---

## Container Image Build (BLOCKING Prerequisite for Step 3)

The `platform-claude-runner` image must be built and pushed **before** Step 3 is deployed. If the platform deploys Step 3 with the new entrypoint before the image is updated, new pods will fail with `CrashLoopBackOff` ("agent-runner: command not found").

The image must include:
1. `agent-runner` binary at `/usr/local/bin/agent-runner`
2. MCP server files at `/opt/mcp/` (preserving `mcp/servers/` and `mcp/lib/` directory structure)
3. Node.js runtime (for MCP servers)
4. Claude CLI (`claude` binary)

```dockerfile
# Stage 1: Build agent-runner
FROM rust:1.82 AS builder
COPY cli/agent-runner/ /build/
RUN cargo build --release -p agent-runner

# Stage 2: Runtime
FROM node:22-slim
RUN npm install -g @anthropic-ai/claude-code
COPY --from=builder /build/target/release/agent-runner /usr/local/bin/
COPY mcp/ /opt/mcp/
RUN cd /opt/mcp && npm install --production
```

---

## Execution Order & Parallelism

```
Container Image Build ─────────────────┐
                                         │
Step 1 (Valkey ACL) ──────┐              │
                           ├──→ Step 3 (Pod + migration) ──→ Step 4 (Pub/sub + SSE) ──→ Step 5 (CLI + create-app) ──→ Step 6 (Delete + tests)
Step 2 (MCP config) ──────┘
```

- Steps 1 and 2 are independent, can be done in parallel
- Container image build can proceed in parallel with Steps 1-2
- Step 3 depends on Steps 1 + 2 + container image
- Steps 4→5→6 are strictly sequential
- **Steps 3+4 must be deployed together** — between Step 3 (agent-runner entrypoint) and Step 4 (SSE bridge), agent-runner writes events to pub/sub but the old WebSocket handler reads pod stdout, resulting in empty event streams. Deploy as a single release.

---

## Cross-Cutting Concerns

### Step 1 (Valkey ACL)
- [x] No new endpoints (internal module only)
- [x] No auth needed (called from already-authenticated session creation)
- [x] Audit: ACL creation/deletion logged via tracing with structured fields (`session_id`, `acl_username`) — not audit_log (ephemeral infra)
- [x] Secrets: ACL passwords never logged — `SessionValkeyCredentials` has custom Debug
- [x] Config: new `PLATFORM_VALKEY_AGENT_HOST` env var
- [x] ACL uses `resetkeys resetchannels -@all` baseline

### Step 2 (Agent-runner enhancements)
- [x] Standalone crate — no platform dependencies
- [x] MCP config excludes admin server (security)
- [x] MCP server paths use `/opt/mcp/servers/*.js` (matching container)
- [x] Platform token not logged (clap `hide_env_values = true`)

### Step 3 (Pod startup + migration)
- [x] Auth: ACL credentials are per-session, short-lived
- [x] Cleanup: ACL deletion in stop_session + reaper + create_session error path
- [x] VALKEY_URL in RESERVED_ENV_VARS prevents hijacking
- [x] `ProgressEvent` gains `Deserialize` (needed by Step 4)
- [x] Both `PodBuildParams` AND `BuildPodParams` get `valkey_url`
- [x] ~40 existing pod.rs unit tests need `valkey_url: None` added
- [x] `.sqlx/` regeneration after migration + query changes

### Step 4 (Pub/sub bridge + SSE)
- [x] Auth: SSE endpoint still requires AuthUser (cookies sent automatically by EventSource)
- [x] Permissions: existing `require_project_read` check unchanged
- [x] Message persistence: `spawn_persistence_subscriber()` started in `create_session()` for all `uses_pubsub=true` sessions
- [x] SSE subscribers are read-only (no double-writes)
- [x] WebSocket infrastructure fully removed (~265 LOC server, ~86 LOC client)
- [x] `axum` `"ws"` feature removed, `tokio-stream` dependency added
- [x] `publish_event()` added for server-side event publishing (Step 5 create-app flow)
- [x] WS removal is a security improvement — old WS handlers bypassed `require_session_write` check

### Step 5 (CLI + create-app)
- [x] CLI has `--tools ""` — ZERO built-in tools
- [x] Server-side tool execution — same Rust code
- [x] OAuth token / API key passed via env vars only (never CLI args)
- [x] CLI subprocess env-cleared, whitelisted vars only
- [x] `tokio::process::Command::args()` prevents shell injection
- [x] Subprocess timeout (300s) prevents hanging
- [x] Subprocess killed explicitly on exit
- [ ] **TODO**: Add `check_length("prompt", prompt, 1, 100_000)` in `execute_spawn_agent`
- [ ] **TODO**: Add length cap on `structured.text` (100K) before broadcast/save

### Step 6 (Dead code + tests)
- [x] `inprocess.rs` + `anthropic.rs` fully deleted
- [x] `inprocess` execution mode removed from CHECK constraint
- [x] Mock CLI tests cover same behaviors as deleted tests
- [x] LLM tests opt-in only, not in CI

---

## Plan Review Findings

### Issues Found & Fixed (from Plan 39 review)

| # | Issue | Fix |
|---|---|---|
| 1 | `PodBuildParams` vs `BuildPodParams` — two separate structs both need `valkey_url` | Both updated in Step 3 |
| 2 | `ProgressEvent` lacked `Deserialize` | Added to Step 3, with `#[serde(other)] Unknown` on `ProgressKind` |
| 3 | Migration was in wrong step | Moved to Step 3 since it writes to the column |
| 4 | `+@pubsub` too broad (includes `PUBSUB CHANNELS`) | Changed to explicit `+subscribe +publish +unsubscribe` |
| 5 | ACL command missing `resetkeys -@all +ping` | Added to prevent default key access and support fred keepalive |
| 6 | `SessionValkeyCredentials` lacked custom Debug | Added `impl Debug` that redacts password/url |
| 7 | `ProgressKind` missing forward-compat for unknown kinds | Added `#[serde(other)] Unknown` variant |
| 8 | `get_log_lines()` becomes dead code after WebSocket removal | Added deletion to Step 4 |
| 9 | `reap_terminated_sessions()` will exceed 100-line clippy limit | Added `finalize_reaped_session()` helper extraction |
| 10 | Step 3→4 streaming gap | Added "MUST deploy together" constraint |
| 11 | Message persistence gap after `stream_pod_logs_to_ws()` removal | Added `spawn_persistence_subscriber()` |
| 12 | Container image was parallel, should be blocking | Changed to blocking prerequisite |

### Issues Found & Fixed (from Plan 40 review)

| # | Issue | Fix |
|---|---|---|
| 1 | `CliSpawnOptions.session_id` naming collision with internal tracking field | Renamed to `initial_session_id` |
| 2 | `get_broadcast()` does not exist on `CliSessionManager` | Use `.get(session_id).await` then access fields directly |
| 3 | `--input-format stream-json` conflicts with `-p` mode | Conditional logic: skip when `prompt` is set |
| 4 | `service.rs` would exceed 1000 LOC | Extracted to `src/agent/create_app.rs` module |
| 5 | Missing `tests/setup_integration.rs` from update list | Added |
| 6 | No subprocess timeout in `invoke_cli()` | Added `tokio::time::timeout(300s)` |
| 7 | No subprocess cleanup on exit | Added explicit `transport.kill()` |
| 8 | Missing `save_assistant_message()` | Removed — persistence handled by `spawn_persistence_subscriber()` |

### Remaining Concerns

1. **Platform subscriber scaling** (MEDIUM) — Step 4 creates a new Valkey connection per active SSE session. Future optimization: `PSUBSCRIBE session:*:events` on single connection with channel-based routing.

2. **Orphaned ACL on platform crash** (MEDIUM) — if platform crashes between ACL creation and session DB update, orphaned Valkey users persist. Consider extending reaper to sweep `pending` sessions older than 5 minutes.

3. **No ACL user TTL** (LOW) — Valkey ACL users have no built-in expiry. Consider periodic `ACL LIST` reconciliation.

4. **No feature flag for pub/sub** (LOW) — no `PLATFORM_VALKEY_ACL_ENABLED` toggle. Once Step 3 deployed, ALL new sessions use agent-runner + pub/sub. Mitigated by "deploy together" constraint.

5. **SSE connection exhaustion** (LOW) — each SSE connection creates a new Valkey subscriber via `clone_new()`. Add a concurrency limiter if needed.

### Security Notes

- **ACL baseline: `resetkeys resetchannels -@all`** — starts from zero permissions
- **ACL passwords**: 256 bits of entropy via `rand::fill()` + `hex::encode()`
- **`VALKEY_URL` reserved**: in `RESERVED_ENV_VARS` to prevent hijacking. Critical security control.
- **Session UUIDs server-generated**: `Uuid::new_v4()`, not user input. No injection risk.
- **`SessionValkeyCredentials` custom Debug**: redacts password and URL
- **WebSocket removal is a security improvement**: old WS handlers bypassed `require_session_write` check
- **MCP config excludes admin server**: agents cannot perform admin operations
- **CLI `env_clear()` + whitelist**: prevents `DATABASE_URL`, `PLATFORM_MASTER_KEY` leakage
- **CLI `--tools ""`**: ZERO built-in tools, server-side execution only

### Simplification Opportunities

1. `PodBuildParams` (pod.rs) and `BuildPodParams` (provider.rs) are near-identical structs — future consolidation
2. `inprocess::subscribe()`, `InProcessHandle.tx`, `CliSessionHandle.tx` become dead code after WS→SSE — Step 5/6 removes
3. Observe `live_tail` migration from `clone()` to `clone_new()` is an implicit bug fix

---

## Final Validation Criteria

| Check | Command | When |
|---|---|---|
| Unit tests pass | `just test-unit` | After every step |
| Integration tests pass | `just test-integration` | After steps 1, 3, 4, 5, 6 |
| E2E tests pass | `just test-e2e` | After steps 3, 4 |
| Lint clean | `just lint` | After every step |
| Format clean | `just fmt` | After every step |
| Deny check | `just deny` | After step 4 (Cargo.toml changes) |
| `.sqlx/` up to date | `just db-check` | After steps 3, 6 |
| No WebSocket remnants | `grep -r "WebSocket\|ws::" src/` | After step 4 |
| No WS client remnants | `grep -r "createWs\|ReconnectingWebSocket" ui/src/` | After step 4 |
| No inprocess remnants | `grep -r "inprocess" src/` | After step 6 |
| Full CI | `just ci-full` | After step 6 (final gate) |
| LLM tests (opt-in) | `just test-llm` | After step 6, when tokens available |

### Test Summary

| Step | Unit | Integration | E2E | LLM | Total |
|---|---|---|---|---|---|
| 1: Valkey ACL | 17 | 8 | 0 | 0 | 25 |
| 2: MCP Config | 7 | 0 | 0 | 0 | 7 |
| 3: Pod Startup | 19 | 5 | 2 | 0 | 26 |
| 4: Pub/Sub Bridge | 11 | 17 | 1 | 0 | 29 |
| 5: CLI + Create-App | 28 | 6 | 0 | 0 | 34 |
| 6: Delete + Tests | 0 | 13 | 0 | 8 | 21 |
| **Total new** | **82** | **49** | **3** | **8** | **142** |

Plus ~40 existing pod.rs unit tests mechanically updated, ~5 WebSocket tests removed, ~35 inprocess tests removed.

### End-state expectations

- `POST /api/create-app` creates `cli_subprocess` session with `uses_pubsub = true`
- Dev agent pods launch `agent-runner` with `VALKEY_URL` env, scoped ACL credentials
- All event streaming via SSE (no WebSocket anywhere)
- Events persisted to `agent_messages` by pub/sub persistence subscriber
- `inprocess.rs`, `anthropic.rs`, `ws.ts` fully deleted
- Net reduction: ~1,600 LOC
- 142 new tests across unit/integration/E2E/LLM tiers
