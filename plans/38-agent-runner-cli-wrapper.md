# Plan: `agent-runner` — Standalone Claude CLI Wrapper

## Context

The platform spawns AI dev agents in K8s pods where the environment is unknown (agents can modify their own Dockerfile). The platform needs a self-contained binary that wraps the Claude Code CLI, isolates config, handles auth, and connects back to the platform via Valkey pub/sub. This binary runs both locally on macOS (for testing/REPL-only) and inside K8s pods (pub/sub + optional REPL).

**Architecture: pub/sub IS the transport.** Valkey pub/sub replaces the previous WebSocket/pod-attach approach for BE↔pod communication entirely. The flow: Platform BE spawns agent pod → agent-runner starts inside pod → publishes init event to `session:{id}:events` → Platform BE subscribes and publishes the prompt to `session:{id}:input` → agent-runner subscribes, passes to Claude CLI → streams progress events back via pub/sub. No WebSocket between BE and pod. WebSocket only remains as the external client boundary (browser/CLI → Platform BE), where the BE bridges pub/sub events to connected WebSocket clients.

The previous `cli/platform-cli/` (remote session client via WebSocket) is deleted — it was based on a misunderstanding of the architecture. The agent-runner replaces it.

The existing `src/agent/claude_cli/` module has proven transport/message/control types that we'll adapt. The wrapper must NOT depend on the main platform crate.

**OAuth confirmed working**: `CLAUDE_CODE_OAUTH_TOKEN` works with `-p`, `--output-format stream-json`, and `--verbose`. Token valid 1 year, reusable across sessions.

## Crate Structure

```
cli/agent-runner/
├── Cargo.toml
└── src/
    ├── main.rs        # clap CLI, auth resolution, spawn + dispatch
    ├── messages.rs    # NDJSON protocol types (from src/agent/claude_cli/messages.rs)
    ├── control.rs     # Control request types (from src/agent/claude_cli/control.rs)
    ├── transport.rs   # Subprocess spawn + NDJSON I/O (from src/agent/claude_cli/transport.rs)
    ├── render.rs      # Terminal rendering with colors
    ├── repl.rs        # Interactive REPL + pub/sub bridge loop
    ├── pubsub.rs      # Valkey pub/sub client (subscribe input, publish events)
    └── error.rs       # Standalone error types
```

## Dependencies (Cargo.toml)

```toml
[package]
name = "agent-runner"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "agent-runner"
path = "src/main.rs"

[dependencies]
clap = { version = "4", features = ["derive"] }
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
colored = "3"
anyhow = "1"
thiserror = "2"
tempfile = "3"
fred = { version = "10", features = ["subscriber-client"] }

[lints.rust]
unsafe_code = "forbid"
```

## Pub/Sub Design

### Channel naming (dual channel per session)

- **Input**: `session:{session_id}:input` — platform publishes prompts, wrapper subscribes
- **Events**: `session:{session_id}:events` — wrapper publishes progress events, platform subscribes

### Message format

**Input messages** (platform → wrapper, published to `session:{id}:input`):
```json
{"type": "prompt", "content": "fix the login bug"}
{"type": "control", "control": {"type": "interrupt"}}
```

**Event messages** (wrapper → platform, published to `session:{id}:events`):
```json
{"kind": "milestone", "message": "Session started (model: opus)", "metadata": {"session_id": "...", "claude_code_version": "1.0"}}
{"kind": "text", "message": "I'll fix the login bug..."}
{"kind": "thinking", "message": "Let me analyze..."}
{"kind": "tool_call", "message": "Read", "metadata": {"tool": "Read", "input": "..."}}
{"kind": "tool_result", "message": "Tool results: t1", "metadata": {"tool_use_id": "..."}}
{"kind": "completed", "message": "Done", "metadata": {"total_cost_usd": 0.05, "num_turns": 3, "duration_ms": 5000}}
{"kind": "error", "message": "Rate limit exceeded"}
```

Event `kind` values match the platform's `ProgressKind` enum exactly (`text`, `thinking`, `tool_call`, `tool_result`, `milestone`, `completed`, `error`). System init maps to `milestone` (same as `cli_message_to_progress()` in `src/agent/claude_cli/session.rs`). The platform BE can deserialize these directly as `ProgressEvent` when bridging to WebSocket.

### Per-session Valkey ACL scoping (required before production pub/sub)

The platform creates a temporary Valkey ACL user per agent session for security isolation:

```bash
# Platform BE creates scoped user before launching pod:
ACL SETUSER session-{session_id} on >{random_password} +subscribe +publish +unsubscribe &session:{session_id}:*
```

Uses explicit commands (`+subscribe +publish +unsubscribe`) instead of `+@pubsub` to prevent `PSUBSCRIBE` with glob patterns. Valkey 7.2+ `&` prefix restricts pub/sub channels, ensuring agents can ONLY publish/subscribe to their own session channels.

The scoped credentials are passed to the agent pod via env vars in the Valkey URL:

```
VALKEY_URL=redis://session-{session_id}:{password}@valkey:6379
```

The `fred` crate's `Config::from_url()` parses the username:password automatically.

**Cleanup**: Platform BE deletes the ACL user when the session ends:
```bash
ACL DELUSER session-{session_id}
```

**SECURITY NOTE**: Without Valkey ACL, any agent pod with the global Valkey URL can subscribe to any session's channels and read/inject messages. The initial implementation ships without ACL — pub/sub is safe for **local development only**. ACL must be implemented before enabling pub/sub in production K8s pods. See Future Work.

### Auto-detect mode

- If `VALKEY_URL` and `SESSION_ID` env vars are set → enable pub/sub
- Pub/sub and interactive REPL run **simultaneously** (useful for debugging in-pod)
- Without Valkey → pure REPL mode (local development)

### Configuration (env vars injected by platform startup script)

| Env var | Purpose |
|---|---|
| `VALKEY_URL` | Valkey connection URL with ACL credentials (e.g. `redis://session-abc:pass@valkey:6379`) |
| `SESSION_ID` | Session UUID for channel naming |
| `CLAUDE_CODE_OAUTH_TOKEN` | Subscription auth (or `ANTHROPIC_API_KEY`) |
| `PLATFORM_API_TOKEN` | Scoped platform auth token (for MCP servers that call platform APIs) |
| `PLATFORM_API_URL` | Platform API base URL (for MCP servers) |

## Implementation Steps

### Step 0: Delete `cli/platform-cli/`

The `platform-cli` remote session client was a misunderstanding — the platform doesn't need a separate CLI client for managing remote sessions via WebSocket. The agent-runner replaces it entirely with a different architecture (Valkey pub/sub as the transport, running inside the pod, not as an external client).

**Delete:**
- `cli/platform-cli/` — entire directory (Cargo.toml, Cargo.lock, src/main.rs, src/config.rs, src/client.rs, src/commands.rs, src/stream.rs, target/)

**Clean up references in:**
- `plans/37-subscription-llm-auth.md` — PR 6 section describes the platform-cli binary. Add a note at the top of that section: `> **Superseded**: platform-cli deleted in Plan 38. Agent communication uses Valkey pub/sub via agent-runner instead.`

**Git:** `git rm -r cli/platform-cli/` (tracked files only; `target/` is gitignored).

### Step 1: Scaffold crate + error types

Create `cli/agent-runner/Cargo.toml` and `src/error.rs`.

**error.rs** — standalone version of `src/agent/claude_cli/error.rs`:
- Remove `use crate::agent::error::AgentError` and `From<CliError> for AgentError`
- Remove tests that reference `AgentError`/`ApiError`
- Add `PubSubError(String)` variant for Valkey errors
- Keep all existing variants and display messages

### Step 2: Message + control types

**messages.rs** — verbatim copy of `src/agent/claude_cli/messages.rs` (326 lines):
- All types unchanged, all 14 tests included (not 12 — includes `parse_cli_message_*` tests)

**control.rs** — verbatim copy of `src/agent/claude_cli/control.rs` (124 lines):
- All types unchanged, all 5 tests included
- NOTE: `ControlRequest` derives `Serialize` only (NOT `Deserialize`) because `msg_type: &'static str` cannot be deserialized. This is fine — we only serialize `ControlRequest` to send to the CLI. For pub/sub input deserialization, we deserialize `ControlPayload` directly (see Step 4).

### Step 3: Transport layer

**transport.rs** — adapted from `src/agent/claude_cli/transport.rs` (743 lines):

Changes:
- `use super::control::ControlRequest` → `use crate::control::ControlRequest` (and similar for error, messages)
- 3 tracing call sites to convert:
  - Line 106: `tracing::debug!(target: "claude_cli_stderr", ...)` → `eprintln!("[stderr] {}", line)`
  - Line 173: `tracing::warn!(line = ..., error = ..., "skipping invalid NDJSON...")` → `eprintln!("[warn] skipping invalid NDJSON: {}", line.trim())`
  - Line 217: `tracing::warn!(error = %e, "stderr capture task panicked")` → `eprintln!("[warn] stderr capture task panicked: {}", e)`
- All functionality and all 23 tests kept (17 sync + 6 async, not 14 as previously estimated)

### Step 4: Pub/sub client

**pubsub.rs** — new module:

```rust
use crate::control::{ControlPayload, ControlRequest};
use crate::messages::CliMessage;

pub struct PubSubClient {
    client: fred::clients::Client,  // single client for PUBLISH
    session_id: String,
}

impl PubSubClient {
    pub async fn connect(url: &str, session_id: &str) -> anyhow::Result<Self>
    pub fn input_channel(&self) -> String  // "session:{id}:input"
    pub fn events_channel(&self) -> String // "session:{id}:events"
    pub async fn publish_event(&self, event: &PubSubEvent) -> anyhow::Result<()>
    pub async fn subscribe_input(&self) -> anyhow::Result<tokio::sync::mpsc::Receiver<PubSubInput>>
}
```

Uses `fred::clients::Client` (not `Pool`) — a single agent process needs only one publish connection. The subscriber uses `client.clone_new()` for a dedicated connection (same pattern as `src/store/eventbus.rs`).

**PubSubKind** — typed enum matching platform's `ProgressKind`:
```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PubSubKind {
    Text,
    Thinking,
    ToolCall,
    ToolResult,
    Milestone,
    Error,
    Completed,
}
```

**PubSubEvent** — maps from CliMessage to publishable event:
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PubSubEvent {
    pub kind: PubSubKind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}
```

**PubSubInput** — incoming commands from platform:
```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum PubSubInput {
    #[serde(rename = "prompt")]
    Prompt { content: String },
    #[serde(rename = "control")]
    Control { control: ControlPayload },  // ControlPayload has Deserialize; ControlRequest does not
}
```

NOTE: `ControlRequest` cannot derive `Deserialize` (its `msg_type: &'static str` field blocks it). Instead, we deserialize only the `ControlPayload` (which does have `Deserialize`), then reconstruct the full `ControlRequest` via factory methods when sending to the CLI:
```rust
match input {
    PubSubInput::Prompt { content } => transport.send_message(&content).await?,
    PubSubInput::Control { control } => {
        let req = ControlRequest { msg_type: "control", control };
        transport.send_control(req).await?;
    }
}
```

**CliMessage → PubSubEvent conversion** — port the logic from `src/agent/claude_cli/session.rs` (`cli_message_to_progress()` and helpers, ~115 lines). This handles:
- `System` → `PubSubKind::Milestone` with session_id + version metadata
- `Assistant` content blocks → iterate to find thinking/text/tool_use
- `User` content blocks → tool_result extraction
- `Result` → `Completed` or `Error` with cost/turns/duration metadata

Add a `pub fn cli_message_to_event(msg: &CliMessage) -> Option<PubSubEvent>` function.

`subscribe_input()` spawns a background task that:
1. Creates a dedicated subscriber client (`client.clone_new()`)
2. Subscribes to `session:{id}:input`
3. Parses incoming JSON messages into `PubSubInput` (max 1 MB per message — reject larger)
4. Forwards via `tokio::sync::mpsc::channel(32)`
5. On parse error: log warning to stderr, skip message (don't crash)

### Step 5: Terminal rendering

**render.rs** — renders CliMessage variants with colors (pattern from `cli/platform-cli/src/stream.rs`):
- `System` → stderr, dimmed: "Session started (model: X)"
- Assistant `thinking` → stderr, dimmed
- Assistant `text` → **stdout** (allows piping)
- Assistant `tool_use` → stderr, cyan with tool name
- User `tool_result` → stderr, blue (content truncated at 200 chars)
- `Result` success → stderr, green with cost/turns/duration
- `Result` error → stderr, red
- `notify_desktop()` — macOS/Linux notifications (terminal bell on all platforms)

### Step 6: REPL + pub/sub bridge

**repl.rs** — the main event loop, merging stdin + pub/sub + CLI output:

```rust
pub async fn run(
    transport: SubprocessTransport,
    pubsub: Option<PubSubClient>,
) -> anyhow::Result<()>
```

Architecture:
1. Register SIGTERM handler (`tokio::signal::unix::signal(SignalKind::terminate())`) for K8s graceful shutdown
2. Wait for `System` init message (30s timeout), render it
3. If pub/sub: publish system event, start input subscriber
4. Spawn stdin reader → `tokio::sync::mpsc::channel::<String>(32)`
5. Main loop:
   - Print `> ` prompt (only if stdin is a TTY)
   - `tokio::select!` on input sources:
     - **stdin** → send to CLI via `transport.send_message()`
     - **pub/sub input** → dispatch prompt or control to CLI
     - **SIGTERM** → send interrupt to CLI, publish error event, break
     - _(these feed the CLI, then we stream responses)_
   - Response streaming loop with `tokio::select!`:
     - `transport.recv()` → render + publish event; break on `Result`
     - `signal::ctrl_c()` → send interrupt, continue
   - Back to prompt after `Result`
6. On shutdown: publish "completed" event if pub/sub connected, kill CLI subprocess

**Testability**: Extract `wait_for_init()` and `dispatch_input()` as `pub(crate)` helpers for unit testing:
```rust
pub(crate) async fn wait_for_init(transport: &SubprocessTransport, timeout_secs: u64) -> Result<SystemMessage, CliError>
pub(crate) async fn dispatch_input(transport: &SubprocessTransport, input: PubSubInput) -> Result<(), CliError>
```

### Step 7: Entry point

**main.rs** — clap CLI:

**Credential flags are env-var-only** — CLI args appear in `ps aux` and leak to all users on the host. Credentials are accepted exclusively via env vars:

| Flag | Env-only | Purpose |
|---|---|---|
| — | `ANTHROPIC_API_KEY` | API key auth |
| — | `CLAUDE_CODE_OAUTH_TOKEN` | Subscription auth (priority over API key) |
| — | `VALKEY_URL` | Valkey connection URL with ACL credentials |
| — | `SESSION_ID` | Session UUID for pub/sub channel naming |
| — | `PLATFORM_API_TOKEN` | Scoped platform auth token (for future MCP servers) |
| — | `PLATFORM_API_URL` | Platform API base URL (for future MCP servers) |
| `--model` | — | Model selection |
| `--system-prompt` | — | System prompt |
| `--max-turns` | — | Max turns |
| `--permission-mode` | — | e.g. `bypassPermissions` |
| `--allowed-tools` | — | Comma-separated tool names |
| `--cwd` | — | Working directory for Claude |
| `--cli-path` | `CLAUDE_CLI_PATH` | Path to `claude` binary |
| `--extra-env` | — | Additional KEY=VALUE (repeatable) |

**RESERVED_ENV_VARS blocklist for `--extra-env`** — prevent overriding security-critical vars (mirrors `src/agent/claude_code/pod.rs` pattern):
```rust
const RESERVED_ENV_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_CONFIG_DIR",
    "PATH", "HOME", "TMPDIR", "VALKEY_URL", "SESSION_ID",
    "PLATFORM_API_TOKEN", "PLATFORM_API_URL",
];
```
Reject any `--extra-env` key that matches a reserved var.

Flow:
1. Parse CLI args
2. Read auth from env: `CLAUDE_CODE_OAUTH_TOKEN` (priority) or `ANTHROPIC_API_KEY`
3. Validate auth (bail if neither is set)
4. Read `VALKEY_URL` and `SESSION_ID` from env; if `VALKEY_URL` set, validate `SESSION_ID` is also set
5. Validate `--extra-env` keys against `RESERVED_ENV_VARS` blocklist
6. Create isolated temp config dir via `tempfile::TempDir`
7. Connect pub/sub if configured: `PubSubClient::connect()`
8. Build `CliSpawnOptions`, spawn `SubprocessTransport`
9. `repl::run(transport, pubsub).await`
10. TempDir cleaned up on exit

## Config Isolation

- `tempfile::TempDir` creates unique temp dir per run
- Passed as `CLAUDE_CONFIG_DIR` to subprocess
- `env_clear()` in transport ensures no `~/.claude` is read
- Auto-cleanup on drop

## Auth Handling

Credentials are env-var-only (never CLI flags — CLI args are visible in `ps aux`).
Priority: `CLAUDE_CODE_OAUTH_TOKEN` → `ANTHROPIC_API_KEY`. Bail if neither is set.

## Files to Copy/Adapt

| Source | Target | Changes |
|---|---|---|
| `src/agent/claude_cli/messages.rs` | `cli/agent-runner/src/messages.rs` | Verbatim (14 tests) |
| `src/agent/claude_cli/control.rs` | `cli/agent-runner/src/control.rs` | Verbatim (5 tests) |
| `src/agent/claude_cli/transport.rs` | `cli/agent-runner/src/transport.rs` | `super::` → `crate::`, 3× `tracing::` → `eprintln!` (23 tests) |
| `src/agent/claude_cli/error.rs` | `cli/agent-runner/src/error.rs` | Remove AgentError/ApiError, add PubSubError |
| `src/agent/claude_cli/session.rs` | Logic for `cli/agent-runner/src/pubsub.rs` | Port `cli_message_to_progress()` + helpers (~115 lines) as `cli_message_to_event()` |
| _(deleted `cli/platform-cli/src/stream.rs`)_ | Pattern for `cli/agent-runner/src/render.rs` | Rewrite render.rs from scratch using colored crate; platform-cli is deleted in Step 0 |
| — (new) | `cli/agent-runner/src/pubsub.rs` | Valkey pub/sub client + PubSubEvent/PubSubInput types + conversion |
| — (new) | `cli/agent-runner/src/repl.rs` | REPL + pub/sub bridge |
| — (new) | `cli/agent-runner/src/main.rs` | Entry point |

**Code duplication note**: This copies ~1,200 lines from `src/agent/claude_cli/`. Add `// Forked from src/agent/claude_cli/{module}.rs — keep in sync manually` comment at the top of each copied file. Future optimization: extract a shared `claude-cli-protocol` library crate when the workspace is set up.

## Testing

Run via `cargo test --manifest-path cli/agent-runner/Cargo.toml` (not `just test-unit` — this is a standalone crate, not part of the main platform workspace).

### Tests — error.rs

**Copied tests:**

| Test | Status |
|---|---|
| `cli_error_display_messages` | Copied verbatim — tests Display impl of all CliError variants |
| `cli_error_to_agent_error` | **REMOVED** — `AgentError` does not exist in standalone crate |
| `agent_cli_error_to_api_internal` | **REMOVED** — `ApiError` does not exist in standalone crate |

**New tests:**

| Test | Validates | Tier |
|---|---|---|
| `pubsub_error_display` | `CliError::PubSubError("msg")` formats correctly via Display | Unit |
| `cli_error_variants_are_send_sync` | All variants implement `Send + Sync` (required for `anyhow::Error`) | Unit |

**Total: 3 tests (1 kept + 2 new)**

### Tests — messages.rs

All 14 tests copied verbatim — no platform deps. Tests: `system_init_deserialize`, `system_init_with_optional_fields_null`, `assistant_message_deserialize`, `user_message_deserialize`, `result_success_deserialize`, `result_error_deserialize`, `unknown_type_returns_none`, `user_input_serialize_text`, `user_input_structured_content`, `usage_info_deserialize`, `empty_json_object_rejected`, `parse_cli_message_empty_line`, `parse_cli_message_invalid_json`, `parse_cli_message_valid_system`.

**Total: 14 tests**

### Tests — control.rs

All 5 tests copied verbatim: `interrupt_request_serialize`, `set_model_request_serialize`, `permission_response_serialize_granted`, `permission_response_serialize_denied`, `control_response_deserialize`.

**Total: 5 tests**

### Tests — transport.rs

All 23 tests copied (17 sync + 6 async tokio tests using `spawn_cat_transport()` mock). No production code changes affect test behavior — only `tracing::` → `eprintln!` in 3 call sites, and `super::` → `crate::` imports.

**Total: 23 tests**

### Tests — pubsub.rs (NEW)

| Test | Validates | Tier |
|---|---|---|
| `pubsub_event_serialize_milestone` | System init maps to `PubSubKind::Milestone` with metadata | Unit |
| `pubsub_event_serialize_text` | Text event serializes correctly | Unit |
| `pubsub_event_serialize_thinking` | Thinking event serializes | Unit |
| `pubsub_event_serialize_tool_call` | ToolCall event with metadata | Unit |
| `pubsub_event_serialize_tool_result` | ToolResult event with metadata | Unit |
| `pubsub_event_serialize_completed` | Completed event with cost/turns/duration metadata | Unit |
| `pubsub_event_serialize_error` | Error event serializes | Unit |
| `pubsub_event_serialize_no_metadata` | `None` metadata omitted via `skip_serializing_if` | Unit |
| `pubsub_input_deserialize_prompt` | `{"type":"prompt","content":"fix bug"}` → `PubSubInput::Prompt { content }` | Unit |
| `pubsub_input_deserialize_control_interrupt` | `{"type":"control","control":{"type":"interrupt"}}` → correct variant | Unit |
| `pubsub_input_deserialize_control_set_model` | `{"type":"control","control":{"type":"set_model","model":"opus"}}` | Unit |
| `pubsub_input_deserialize_control_permission` | Permission response variant | Unit |
| `pubsub_input_deserialize_unknown_type` | `{"type":"unknown"}` returns deser error | Unit |
| `pubsub_input_deserialize_invalid_json` | `"not json"` returns error | Unit |
| `pubsub_input_deserialize_missing_content` | `{"type":"prompt"}` without `content` → error | Unit |
| `input_channel_name` | Constructs `"session:{id}:input"` correctly | Unit |
| `events_channel_name` | Constructs `"session:{id}:events"` correctly | Unit |
| `pubsub_event_round_trip` | Serialize then deserialize, verify fields match | Unit |
| `cli_message_to_event_system` | `CliMessage::System` → `PubSubKind::Milestone` | Unit |
| `cli_message_to_event_text` | Assistant with text block → `PubSubKind::Text` | Unit |
| `cli_message_to_event_thinking` | Assistant with thinking block → `PubSubKind::Thinking` | Unit |
| `cli_message_to_event_tool_call` | Assistant with tool_use block → `PubSubKind::ToolCall` | Unit |
| `cli_message_to_event_tool_result` | User with tool_result block → `PubSubKind::ToolResult` | Unit |
| `cli_message_to_event_result_success` | Result success → `PubSubKind::Completed` | Unit |
| `cli_message_to_event_result_error` | Result error → `PubSubKind::Error` | Unit |
| `cli_message_to_event_empty_content` | Assistant with empty content → `None` | Unit |

**Branch coverage:**

| Branch/Path | Covered by test |
|---|---|
| Each `PubSubKind` variant serialization | `pubsub_event_serialize_*` (7 tests) |
| `PubSubInput::Prompt` variant | `pubsub_input_deserialize_prompt` |
| `PubSubInput::Control` with each `ControlPayload` variant | `pubsub_input_deserialize_control_*` (3 tests) |
| Invalid/unknown input messages | `pubsub_input_deserialize_unknown_type`, `_invalid_json`, `_missing_content` |
| Channel name construction | `input_channel_name`, `events_channel_name` |
| Each `CliMessage` → `PubSubEvent` conversion path | `cli_message_to_event_*` (8 tests) |

**Tests that require real Valkey (env-gated with `#[ignore]`):**

| Test | Validates |
|---|---|
| `pubsub_connect_and_publish` | `PubSubClient::connect()` + `publish_event()` round-trip |
| `pubsub_subscribe_and_receive` | Full subscribe → publish → receive cycle |

**Total: 26 unit tests + 2 ignored integration tests = 28 tests**

### Tests — render.rs (NEW)

| Test | Validates | Tier |
|---|---|---|
| `render_system_message` | System init renders without panic | Unit |
| `render_assistant_text` | Text content block renders | Unit |
| `render_assistant_thinking` | Thinking content block renders | Unit |
| `render_assistant_tool_use` | tool_use content block renders | Unit |
| `render_assistant_multiple_blocks` | Multiple text blocks concatenated | Unit |
| `render_assistant_mixed_content` | Text + tool_use both render | Unit |
| `render_assistant_empty_content` | Empty content array doesn't panic | Unit |
| `render_user_tool_result_short` | Short tool result not truncated | Unit |
| `render_user_tool_result_exact_200` | Exactly 200 chars not truncated | Unit |
| `render_user_tool_result_201_truncated` | 201 chars truncated to 200 + "..." | Unit |
| `render_result_success` | Success result renders | Unit |
| `render_result_success_with_metadata` | Cost, turns, duration rendered | Unit |
| `render_result_success_no_metadata` | No cost/turns/duration renders cleanly | Unit |
| `render_result_error` | Error result renders | Unit |
| `render_result_error_no_message` | Error without result text uses fallback | Unit |

These are smoke tests (verify no panic), matching the platform-cli pattern. Output assertions would require refactoring render to accept `Write` trait objects — deferred.

**Total: 15 tests**

### Tests — repl.rs (NEW)

| Test | Validates | Tier |
|---|---|---|
| `init_timeout_triggers` | No System message within timeout → `CliError::InitTimeout` | Unit |
| `init_succeeds_with_system_message` | System message received → returns `SystemMessage` | Unit |
| `dispatch_prompt_input` | `PubSubInput::Prompt` calls `transport.send_message()` | Unit |
| `dispatch_control_interrupt` | `PubSubInput::Control { Interrupt }` calls `transport.send_control()` | Unit |
| `loop_exits_on_result_message` | `CliMessage::Result` breaks the response loop | Unit |
| `loop_exits_on_eof` | `transport.recv()` returns `None` → loop breaks | Unit |

Uses the `spawn_cat_transport()` pattern from transport.rs tests.

**Tests that CANNOT be unit tested:**

| Area | Reason |
|---|---|
| Full REPL loop with stdin | `tokio::io::stdin()` not mockable; tested via manual `cargo run` |
| Ctrl+C/SIGTERM handling | Process-global signals conflict with test harness |
| TTY detection for prompt | Depends on actual terminal state |

**Total: 6 tests**

### Tests — main.rs (NEW)

Uses clap's `try_parse_from()` for arg parsing tests.

| Test | Validates | Tier |
|---|---|---|
| `auth_from_oauth_env` | `CLAUDE_CODE_OAUTH_TOKEN` env → auth resolved | Unit |
| `auth_from_api_key_env` | `ANTHROPIC_API_KEY` env → auth resolved | Unit |
| `no_auth_fails` | No auth env vars → validation error | Unit |
| `oauth_takes_precedence` | Both set → oauth used, not api key | Unit |
| `valkey_without_session_id_fails` | `VALKEY_URL` set but no `SESSION_ID` → error | Unit |
| `session_id_without_valkey_ok` | `SESSION_ID` without `VALKEY_URL` → pub/sub disabled (not error) | Unit |
| `valkey_with_session_id` | Both set → pub/sub enabled | Unit |
| `parse_model_flag` | `--model opus` parsed | Unit |
| `parse_system_prompt` | `--system-prompt "..."` parsed | Unit |
| `parse_max_turns` | `--max-turns 10` parsed | Unit |
| `parse_allowed_tools` | `--allowed-tools "Read,Write"` parsed | Unit |
| `parse_permission_mode` | `--permission-mode bypassPermissions` parsed | Unit |
| `parse_cwd` | `--cwd /tmp` parsed | Unit |
| `parse_extra_env_single` | `--extra-env KEY=VALUE` → `("KEY", "VALUE")` | Unit |
| `parse_extra_env_multiple` | `--extra-env A=1 --extra-env B=2` → two pairs | Unit |
| `parse_extra_env_invalid_no_equals` | `--extra-env NOEQUALS` → validation error | Unit |
| `extra_env_reserved_var_rejected` | `--extra-env ANTHROPIC_API_KEY=x` → rejected by blocklist | Unit |
| `extra_env_reserved_path_rejected` | `--extra-env PATH=/x` → rejected | Unit |

**Total: 18 tests**

### Test Plan Summary

| Module | Copied | New | Total |
|---|---|---|---|
| error.rs | 1 | 2 | 3 |
| messages.rs | 14 | 0 | 14 |
| control.rs | 5 | 0 | 5 |
| transport.rs | 23 | 0 | 23 |
| pubsub.rs | 0 | 28 | 28 |
| render.rs | 0 | 15 | 15 |
| repl.rs | 0 | 6 | 6 |
| main.rs | 0 | 18 | 18 |
| **Total** | **43** | **69** | **112** |

All tests are unit tier (no DB, no Kind cluster). 2 pub/sub integration tests are `#[ignore]` (require real Valkey).

### Manual verification

```bash
# Build (standalone, not workspace)
cargo build --manifest-path cli/agent-runner/Cargo.toml

# Run tests
cargo test --manifest-path cli/agent-runner/Cargo.toml

# REPL only (local dev)
CLAUDE_CODE_OAUTH_TOKEN=... cargo run --manifest-path cli/agent-runner/Cargo.toml -- --cwd /tmp

# REPL + pub/sub (simulating pod, local Valkey)
CLAUDE_CODE_OAUTH_TOKEN=... VALKEY_URL=redis://localhost:6379 SESSION_ID=test-123 \
  cargo run --manifest-path cli/agent-runner/Cargo.toml -- --cwd /tmp
# Then from another terminal: redis-cli PUBLISH session:test-123:input '{"type":"prompt","content":"say hello"}'
```

## Deployment (sketch for follow-up PR)

The agent-runner binary runs inside K8s pods, not on the host. Deployment path:

1. **Cross-compilation**: Build for `x86_64-unknown-linux-musl` (static binary) alongside the platform Docker build
2. **Multi-stage Dockerfile**: Copy agent-runner binary into the agent base image, or make it downloadable from the platform's OCI registry
3. **Pod startup**: Platform BE overwrites the pod command to: (a) install Claude CLI via npm, (b) launch `agent-runner` with appropriate env vars
4. **NetworkPolicy**: `build_network_policy()` in `src/deployer/namespace.rs` currently blocks cluster-internal IPs from agent pods. Must add egress rule for Valkey (port 6379) in the platform namespace before pub/sub works in production.

## Future Work (not this PR)

- `--prompt "do X"` single-shot mode (non-interactive, exit after one result)
- MCP server integration: wrapper passes `PLATFORM_API_TOKEN` + `PLATFORM_API_URL` to Claude CLI via `--mcp-config`, enabling Claude to call platform APIs (issues, pipelines, deployments) with scoped auth
- **Valkey ACL (REQUIRED before production pub/sub)**:
  - Platform BE: create per-session Valkey ACL user (`ACL SETUSER session-{id} on >{pass} +subscribe +publish +unsubscribe &session:{id}:*`)
  - Platform BE: inject `VALKEY_URL` with scoped credentials into pod env
  - Platform BE: cleanup ACL user on session end (`ACL DELUSER session-{id}`)
- Platform BE: new execution mode (e.g. `"agent_runner"`) — `send_message()` in `src/agent/service.rs` needs a branch to publish to `session:{id}:input` instead of K8s pod attach
- Platform BE: subscribing to `session:{id}:events` and bridging to WebSocket for UI
- Platform BE: reconcile pub/sub lifecycle events ("completed"/"error") with the existing pod reaper (30s poll cycle in `service.rs`)
- NetworkPolicy update in `src/deployer/namespace.rs` to allow agent pod egress to Valkey
- Shared `claude-cli-protocol` library crate to eliminate code duplication between `src/agent/claude_cli/` and `cli/agent-runner/`
- Valkey reconnection strategy: fred supports automatic reconnect — configure it; handle degraded mode (events dropped) vs terminate on permanent disconnect

## Implementation Progress

**Date:** 2026-03-03
**Status:** COMPLETE — all steps implemented, all tests passing

### Steps completed

- [x] Step 0: Delete `cli/platform-cli/` — `git rm -rf`, reference in plan 37 updated
- [x] Step 1: Scaffold crate + error types — Cargo.toml, error.rs (3 tests)
- [x] Step 2: Message types — messages.rs verbatim copy (14 tests)
- [x] Step 3: Control + transport — control.rs verbatim (5 tests), transport.rs adapted (23 tests)
- [x] Step 4: Pub/sub client — pubsub.rs (26 unit + 2 ignored = 28 tests)
- [x] Step 5: Terminal rendering — render.rs (15 tests)
- [x] Step 6: REPL + pub/sub bridge — repl.rs (6 tests)
- [x] Step 7: Entry point — main.rs (18 tests)

### Deviations from plan

1. **Rust 2021 `let` chains**: `transport.rs` uses nested `if let` instead of `if let ... &&` (plan didn't specify edition constraint, but 2024 features aren't available in 2021 edition)
2. **fred subscribe API**: Plan showed `subscriber.subscribe::<(), _>(&channel)` but fred v10 takes 1 generic arg, not 2. Fixed to `subscriber.subscribe(&channel)`
3. **`#[allow(dead_code)]` on module declarations**: control.rs, error.rs, transport.rs have copied types not all used by the standalone binary. Added `#[allow(dead_code)]` annotations to module declarations in main.rs (matching the plan's "forked from" approach)
4. **`dispatch_input` in pubsub.rs, not repl.rs**: The plan's Step 6 showed `dispatch_input` as `pub(crate)` in repl.rs, but it was placed in pubsub.rs since it's more cohesive with the pub/sub module

### Verification

- `cargo fmt --check` — clean
- `cargo clippy -- -D warnings` — clean
- `cargo test` — 110 passed, 0 failed, 2 ignored
- Main platform `just test-unit` — 1154 passed (deleting platform-cli had no effect)

## Plan Review Findings

**Date:** 2026-03-03
**Status:** APPROVED WITH CONCERNS

### Codebase Reality Check

Issues found and corrected in-place above:

1. **`ControlRequest` lacks `Deserialize`** (CRITICAL) — `msg_type: &'static str` cannot be deserialized from JSON. The plan originally had `PubSubInput::Control(ControlRequest)` which would fail at compile time. Fixed: `PubSubInput::Control { control: ControlPayload }` — deserialize only the payload (which has `Deserialize`), reconstruct `ControlRequest` via factory method when sending to CLI.

2. **Credential CLI flags in `ps aux`** (CRITICAL) — `--api-key`, `--oauth-token`, `--valkey-url`, `--platform-token` were proposed as CLI flags. CLI args are visible to all users via `ps aux` / `/proc/<pid>/cmdline`. Fixed: credentials are now env-var-only. `hide_env_values` in clap only affects `--help` output, not process table visibility.

3. **Test count underestimates** — Plan said "~35 tests, 12 message, 14 transport". Actual: 14 messages, 23 transport, 43 total copied. With 69 new tests, total is 112 (not ~35). Fixed in test tables above.

4. **Event kind mismatch** — Plan used `kind: String` with `"system"` kind that doesn't exist in platform's `ProgressKind` enum. The platform maps `System` → `Milestone`. Fixed: added `PubSubKind` enum matching `ProgressKind` exactly, using `Milestone` for system init.

5. **Missing conversion logic** — Plan's "Files to Copy/Adapt" table omitted `session.rs`'s `cli_message_to_progress()` (~115 lines) needed for `CliMessage → PubSubEvent` conversion. Fixed: added to copy table.

6. **fred Pool overkill** — A single agent process needs one publish + one subscribe connection, not a pool. Fixed: use `fred::clients::Client` + `clone_new()` pattern (matching `src/store/eventbus.rs`).

7. **No SIGTERM handling** — K8s sends SIGTERM before SIGKILL (30s grace). Without handler, CLI subprocess could be orphaned. Fixed: added SIGTERM handler to REPL architecture.

8. **No `--extra-env` security** — Could override `PATH`, auth tokens, etc. Fixed: added `RESERVED_ENV_VARS` blocklist mirroring `src/agent/claude_code/pod.rs`.

### Remaining Concerns

1. **Valkey ACL not in initial implementation** — Without ACL, any agent pod with the global Valkey URL can read/write all session channels AND other Valkey keys (permission caches, rate limits). Pub/sub is safe for local dev only. Production deployment MUST implement ACL first. This is correctly listed in Future Work but the severity should be emphasized.

2. **NetworkPolicy blocks Valkey** — `build_network_policy()` in `src/deployer/namespace.rs` denies cluster-internal IPs from agent pods. Valkey runs on a cluster-internal IP. Agent pods cannot reach Valkey without a NetworkPolicy update. Not a blocker for this PR (local dev works), but the follow-up PR MUST address this.

3. **No Valkey reconnection strategy** — If Valkey connection drops mid-session, the agent continues running but the platform loses visibility. Fred supports auto-reconnect but it must be explicitly configured. The REPL should handle the pub/sub input channel closing gracefully (continue in degraded mode, don't crash).

4. **No pub/sub message size limits** — A malicious/buggy platform could publish enormous prompts. Added 1 MB limit in the plan but verify fred exposes this configuration.

5. **VALKEY_URL credential logging** — Fred's `Config::from_url()` error messages may include the URL (with embedded password). When converting tracing to eprintln, ensure Valkey connection errors are inspected — if they include the URL, redact before logging.

### Simplification Opportunities

1. **Mutexes on stdin/stdout** — `SubprocessTransport` uses `Mutex<BufWriter/BufReader>` for thread safety. In the single-task REPL, these are unnecessary overhead. However, removing them diverges from the platform copy, making future shared-crate extraction harder. Accept as-is.

2. **Keep `tracing` instead of `eprintln!`** — Adding `tracing-subscriber` (~2 deps) would provide structured JSON logging to stderr, valuable for pod log collection. But this increases deps for a CLI tool. Either approach is acceptable; `eprintln!` is fine for v1.

### Security Notes

- `env_clear()` + whitelist in transport.rs is a strong security model — verified no leaking of `DATABASE_URL`, `PLATFORM_MASTER_KEY`, etc.
- `tempfile::TempDir` creates directories with `0o700` permissions — secure against other users.
- Session IDs are UUIDs — no channel injection risk in `session:{id}:*` naming.
- ACL proposal uses explicit commands (`+subscribe +publish +unsubscribe`) instead of `+@pubsub` — prevents pattern subscription bypass.
