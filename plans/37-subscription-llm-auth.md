# Plan 37: Native Claude CLI Subprocess Wrapper with Subscription Auth

## Implementation Status
- **Branch:** `feat/37-subscription-llm-auth`
- **PR:** #15 (https://github.com/agentsphere/platform/pull/15)
- **Status:** In Review

## Context

The platform currently has two agent paths, both requiring an Anthropic API key:

1. **In-process agents** (`src/agent/anthropic.rs`) — call the Anthropic Messages API directly via `reqwest` with `x-api-key`. Used for the create-app flow. Limited to 2 custom tools (`create_project`, `spawn_coding_agent`).
2. **Pod-based agents** (`src/agent/claude_code/pod.rs`) — spawn K8s pods running `claude --print --output-format stream-json`. Full Claude Code tooling (Read/Write/Edit/Bash/Glob/Grep). Requires `ANTHROPIC_API_KEY` env var.

The user wants to use their **Claude subscription** (Pro/Max, $20/mo+) instead of paying API costs. This is viable for a self-hosted, open-source platform where each user brings their own authenticated CLI.

### Why NOT cc-sdk

The [claude-code-api-rs](https://github.com/ZhangHanDong/claude-code-api-rs) crate was evaluated and rejected:
- Edition 2024 incompatibility (we use 2021)
- `rand 0.8` conflicts with our `rand 0.10`
- `unsafe { std::env::set_var() }` usage
- 3,779 total downloads — insufficient maturity
- The NDJSON protocol is simple enough (~300 LOC) to implement natively

### How Claude CLI Auth Works

After `claude login` (OAuth 2.0 + PKCE flow):
- Tokens stored in `~/.claude/.credentials.json` (Linux/headless) or macOS Keychain
- **No machine-specific bindings** — credentials are fully portable across machines
- Access tokens expire in ~8 hours, auto-refreshed via single-use refresh tokens
- `claude setup-token` generates 1-year tokens for headless/container use
- `CLAUDE_CODE_OAUTH_TOKEN` env var overrides file-based auth

### NDJSON SDK Protocol Summary

The Claude CLI supports a bidirectional subprocess protocol:
- Spawn: `claude --input-format stream-json --output-format stream-json --verbose`
- **stdin**: Send `{"type":"user","message":{"role":"user","content":"..."}}\n`
- **stdout**: Receive NDJSON events: `system` (init), `assistant` (response), `user` (tool results), `result` (completion), `stream_event` (partial tokens)
- Control protocol: `control_request`/`control_response` for permissions, hooks, model switching, interrupts
- Sessions: `--resume <session-id>` to continue conversations, `--session-id <uuid>` to specify ID

---

## Design Principles

1. **Native Rust, zero external SDK deps** — implement the NDJSON subprocess protocol ourselves (~300 LOC transport + ~200 LOC message types)
2. **Auth-agnostic** — support both subscription (mounted credentials) and API key (env var) transparently. The CLI handles auth; we just need the right files/env vars.
3. **Dual execution modes** — run CLI subprocess directly in the platform pod (fast, for lightweight sessions) or in a separate K8s pod (isolated, for heavy coding agents)
4. **Refresh-token safety** — use `CLAUDE_CODE_OAUTH_TOKEN` (or a single long-lived `setup-token`) instead of sharing `.credentials.json` across concurrent processes (avoids the documented refresh-token race condition)
5. **Client binary** — separate Rust binary that connects to the platform via WebSocket, enabling remote terminal-like interaction with agent sessions

---

## PR 1: Claude CLI Subprocess Transport

Core Rust module implementing the NDJSON stdin/stdout protocol for communicating with the Claude CLI.

- [ ] Types & errors defined
- [ ] Migration applied
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### New Module: `src/agent/claude_cli/`

```
src/agent/claude_cli/
├── mod.rs          # Re-exports
├── transport.rs    # SubprocessTransport: spawn, send, recv
├── messages.rs     # NDJSON message types (system, assistant, user, result, stream_event)
├── control.rs      # Control protocol (permissions, hooks, interrupt, model switch)
└── error.rs        # CliError enum
```

### Types: `messages.rs`

```rust
/// Top-level NDJSON message from CLI stdout
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum CliMessage {
    #[serde(rename = "system")]
    System(SystemMessage),
    #[serde(rename = "assistant")]
    Assistant(AssistantMessage),
    #[serde(rename = "user")]
    User(UserMessage),
    #[serde(rename = "result")]
    Result(ResultMessage),
    #[serde(rename = "stream_event")]
    StreamEvent(StreamEventMessage),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemMessage {
    pub subtype: String,           // "init"
    pub session_id: String,
    pub model: Option<String>,
    pub tools: Option<Vec<String>>,
    pub claude_code_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultMessage {
    pub subtype: String,           // "success", "error_max_turns", "error_during_execution"
    pub session_id: String,
    pub is_error: bool,
    pub result: Option<String>,
    pub total_cost_usd: Option<f64>,
    pub duration_ms: Option<u64>,
    pub num_turns: Option<u32>,
    pub usage: Option<UsageInfo>,
}

/// Input message sent to CLI via stdin
#[derive(Debug, Clone, Serialize)]
pub struct CliUserInput {
    #[serde(rename = "type")]
    pub msg_type: &'static str,   // always "user"
    pub message: CliUserContent,
}

#[derive(Debug, Clone, Serialize)]
pub struct CliUserContent {
    pub role: &'static str,       // always "user"
    pub content: serde_json::Value,
}
```

### Transport: `transport.rs`

```rust
pub struct SubprocessTransport {
    child: tokio::process::Child,
    stdin: tokio::sync::Mutex<BufWriter<ChildStdin>>,
    stdout: BufReader<ChildStdout>,
    stderr_task: JoinHandle<()>,
    session_id: Option<String>,
}

/// 16 fields — derive Default for ergonomic builder-style construction.
#[derive(Default)]
pub struct CliSpawnOptions {
    pub cli_path: Option<PathBuf>,            // Override CLI binary path
    pub cwd: Option<PathBuf>,                 // Working directory
    pub model: Option<String>,                // --model
    pub system_prompt: Option<String>,        // --system-prompt
    pub append_system_prompt: Option<String>, // --append-system-prompt
    pub allowed_tools: Option<Vec<String>>,   // --allowedTools
    pub permission_mode: Option<String>,      // --permission-mode
    pub max_turns: Option<u32>,               // --max-turns
    pub resume_session: Option<String>,       // --resume <id>
    pub mcp_config: Option<PathBuf>,          // --mcp-config
    pub include_partial: bool,                // --include-partial-messages
    pub config_dir: Option<PathBuf>,          // CLAUDE_CONFIG_DIR env var
    pub oauth_token: Option<String>,          // CLAUDE_CODE_OAUTH_TOKEN env var
    pub anthropic_api_key: Option<String>,    // ANTHROPIC_API_KEY env var (fallback)
    pub extra_env: Vec<(String, String)>,     // Additional env vars
    pub setting_sources: Option<String>,      // --setting-sources (user,project,local)
    pub agents_json: Option<String>,          // --agents (JSON subagent defs)
}

impl SubprocessTransport {
    /// Spawn the Claude CLI as a subprocess.
    ///
    /// SECURITY: Uses `Command::env_clear()` then adds ONLY whitelisted vars
    /// (PATH, HOME, TMPDIR, CLAUDE_CODE_OAUTH_TOKEN/ANTHROPIC_API_KEY, CLAUDE_CONFIG_DIR,
    /// plus `extra_env`). This prevents leaking DATABASE_URL, PLATFORM_MASTER_KEY, etc.
    /// Each session gets a unique temp working directory under /tmp/platform-cli-sessions/.
    pub async fn spawn(opts: CliSpawnOptions) -> Result<Self, CliError>;

    /// Send a user message to the CLI via stdin.
    pub async fn send_message(&self, content: &str) -> Result<(), CliError>;

    /// Send structured content (multi-part, images) via stdin.
    pub async fn send_structured(&self, content: serde_json::Value) -> Result<(), CliError>;

    /// Read the next NDJSON message from stdout.
    pub async fn recv(&mut self) -> Result<Option<CliMessage>, CliError>;

    // NOTE: No `stream()` method — use `while let Some(msg) = transport.recv().await?`
    // loop pattern, matching the existing `lines.next_line()` pattern in handle_ws().
    // The codebase does not use `async-stream` or `futures::Stream` in agent modules.

    /// Send a control request (interrupt, set_model, etc.).
    pub async fn send_control(&self, request: ControlRequest) -> Result<(), CliError>;

    /// Get the session ID (available after receiving the System init message).
    pub fn session_id(&self) -> Option<&str>;

    /// Kill the subprocess.
    pub async fn kill(&mut self) -> Result<(), CliError>;

    /// Check if the subprocess is still running.
    pub fn is_alive(&self) -> bool;
}
```

### CLI Discovery

```rust
/// Find the `claude` CLI binary. Priority:
/// 1. Explicit path from CliSpawnOptions
/// 2. CLAUDE_CLI_PATH env var
/// 3. PATH lookup via `which`
/// 4. Common npm global install paths
/// 5. /usr/local/bin/claude
fn find_claude_cli(explicit: Option<&Path>) -> Result<PathBuf, CliError>;
```

### Error: `error.rs`

```rust
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("claude CLI not found — install via: npm install -g @anthropic-ai/claude-code")]
    CliNotFound,
    #[error("CLI process exited with code {code}: {stderr}")]
    ProcessExit { code: i32, stderr: String },
    #[error("CLI spawn failed: {0}")]
    SpawnFailed(#[source] std::io::Error),
    #[error("stdin write failed: {0}")]
    StdinWrite(#[source] std::io::Error),
    #[error("stdout read failed: {0}")]
    StdoutRead(#[source] std::io::Error),
    #[error("invalid NDJSON: {0}")]
    InvalidJson(#[source] serde_json::Error),
    #[error("init timeout: CLI did not emit system init within {0}s")]
    InitTimeout(u64),
    #[error("CLI process not running")]
    NotRunning,
    #[error("control protocol error: {0}")]
    ControlError(String),
    #[error("session error: {0}")]
    SessionError(String),
}
```

### Code Changes

| File | Change |
|------|--------|
| `src/agent/claude_cli/mod.rs` | New module: re-exports transport, messages, control, error |
| `src/agent/claude_cli/transport.rs` | `SubprocessTransport` implementation (~200 lines) |
| `src/agent/claude_cli/messages.rs` | NDJSON message type definitions (~150 lines) |
| `src/agent/claude_cli/control.rs` | Control protocol types + request/response handling (~100 lines) |
| `src/agent/claude_cli/error.rs` | `CliError` enum (~30 lines) |
| `src/agent/mod.rs` | Add `pub mod claude_cli;` |
| `src/agent/error.rs` | Add `#[error(transparent)] Cli(#[from] CliError)` variant to `AgentError` + match arm in `From<AgentError> for ApiError` mapping to `Internal` |

### Tests to write FIRST (before implementation) — PR 1

**Unit tests — `src/agent/claude_cli/messages.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_system_init_deserialize` | System init message parses from JSON | Unit |
| `test_system_init_with_optional_fields` | model, tools, claude_code_version nullable | Unit |
| `test_assistant_message_deserialize` | Assistant message with content blocks | Unit |
| `test_user_message_deserialize` | User (tool result) message | Unit |
| `test_result_success_deserialize` | Result success with cost/usage | Unit |
| `test_result_error_deserialize` | Result error variants (max_turns, execution) | Unit |
| `test_stream_event_deserialize` | Partial token stream event | Unit |
| `test_unknown_type_ignored` | Unknown `type` field gracefully skipped | Unit |
| `test_user_input_serialize` | CliUserInput serializes with `type: "user"` | Unit |
| `test_user_input_structured_content` | Structured (multi-part) content serializes | Unit |
| `test_usage_info_deserialize` | UsageInfo with input/output tokens | Unit |
| `test_empty_json_object_rejected` | `{}` fails to deserialize as CliMessage | Unit |

**Unit tests — `src/agent/claude_cli/control.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_interrupt_request_serialize` | Interrupt control request format | Unit |
| `test_set_model_request_serialize` | Model switch control request | Unit |
| `test_permission_response_serialize` | Permission grant/deny response | Unit |
| `test_control_response_deserialize` | Control response from CLI | Unit |

**Unit tests — `src/agent/claude_cli/transport.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_find_cli_explicit_path` | Explicit path preferred over PATH lookup | Unit |
| `test_find_cli_env_var` | CLAUDE_CLI_PATH env var used when set | Unit |
| `test_find_cli_not_found` | CliError::CliNotFound when missing | Unit |
| `test_spawn_options_default` | CliSpawnOptions::default() all None | Unit |
| `test_spawn_builds_correct_args` | --model, --max-turns, --permission-mode flags built correctly | Unit |
| `test_spawn_includes_stream_flags` | --input-format stream-json --output-format stream-json always present | Unit |
| `test_spawn_resume_session_flag` | --resume <id> added when set | Unit |
| `test_spawn_mcp_config_flag` | --mcp-config <path> added when set | Unit |
| `test_spawn_env_clear_whitelist` | Only whitelisted env vars passed (PATH, HOME, TMPDIR + auth) | Unit |
| `test_spawn_oauth_token_env` | CLAUDE_CODE_OAUTH_TOKEN set from opts | Unit |
| `test_spawn_api_key_env_fallback` | ANTHROPIC_API_KEY set when no oauth_token | Unit |
| `test_send_message_writes_ndjson` | send_message writes JSON + newline to stdin | Unit |
| `test_recv_parses_ndjson_line` | recv returns parsed CliMessage | Unit |
| `test_recv_skips_invalid_json` | Invalid JSON lines logged + skipped | Unit |
| `test_recv_returns_none_on_eof` | Returns None when stdout closes | Unit |
| `test_is_alive_after_spawn` | is_alive() true for running process | Unit |
| `test_kill_terminates_process` | kill() sets is_alive() false | Unit |
| `test_session_id_populated_after_init` | session_id() returns Some after System message | Unit |
| `test_session_id_none_before_init` | session_id() returns None initially | Unit |

**Unit tests — `src/agent/claude_cli/error.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_cli_error_display_messages` | All CliError variants have descriptive Display | Unit |
| `test_cli_error_to_agent_error` | CliError converts to AgentError::Cli via From | Unit |
| `test_agent_cli_error_to_api_internal` | AgentError::Cli maps to ApiError::Internal | Unit |

**Existing tests to UPDATE:**

| Test file | Change | Reason |
|---|---|---|
| `src/agent/error.rs` | Add `Cli` variant match arm to From<AgentError> tests | New variant added |

**Branch coverage checklist:**

| Branch/Path | Test that covers it |
|---|---|
| `find_cli: explicit path exists` | `test_find_cli_explicit_path` |
| `find_cli: env var set` | `test_find_cli_env_var` |
| `find_cli: not found` | `test_find_cli_not_found` |
| `spawn: oauth_token present → set env` | `test_spawn_oauth_token_env` |
| `spawn: no oauth → api_key fallback` | `test_spawn_api_key_env_fallback` |
| `spawn: env_clear whitelist` | `test_spawn_env_clear_whitelist` |
| `recv: valid NDJSON` | `test_recv_parses_ndjson_line` |
| `recv: invalid JSON → skip` | `test_recv_skips_invalid_json` |
| `recv: EOF → None` | `test_recv_returns_none_on_eof` |
| `send_message: writes to stdin` | `test_send_message_writes_ndjson` |
| `kill: terminates` | `test_kill_terminates_process` |
| `CliError → AgentError → ApiError` | `test_agent_cli_error_to_api_internal` |

**Tests NOT needed:**
- Real CLI integration tests — deferred to PR 3 (session API) where full lifecycle is tested
- Stderr capture task — implementation detail, covered indirectly by ProcessExit error variant

**Total: 39 unit tests**

---

## PR 2: Auth Credential Management

Store and serve Claude CLI credentials for subscription-based auth.

- [ ] Types & errors defined
- [ ] Migration applied
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### Migration: `20260302020001_cli_auth_credentials`

**Up:**
```sql
-- Store Claude CLI auth credentials per user (encrypted).
-- Uses single encrypted_data BYTEA column matching the existing pattern
-- in user_provider_keys.encrypted_key and secrets.encrypted_value.
-- engine::encrypt() returns nonce(12) || ciphertext || tag as a single blob.
CREATE TABLE cli_credentials (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- 'oauth' (subscription) or 'setup_token' (1-year headless token)
    auth_type TEXT NOT NULL CHECK (auth_type IN ('oauth', 'setup_token')),
    -- Encrypted credential blob: nonce(12) || ciphertext || tag (AES-256-GCM)
    -- For oauth: JSON { access_token, refresh_token, expires_at }
    -- For setup_token: the raw token string
    encrypted_data BYTEA NOT NULL,
    -- When the access token expires (for proactive refresh)
    token_expires_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE(user_id, auth_type)
);

CREATE INDEX idx_cli_credentials_user ON cli_credentials(user_id);
```

**Down:**
```sql
DROP TABLE cli_credentials;
```

### Credential Flow

```
User's machine                    Platform
──────────────                    ────────
claude login (browser)
  ↓
~/.claude/.credentials.json
  ↓
platform-cli upload-creds    →    POST /api/auth/cli-credentials
  (reads .credentials.json)         ↓
                                  encrypt with PLATFORM_MASTER_KEY
                                  store in cli_credentials table
                                    ↓
                              On session spawn:
                                  decrypt credentials
                                  set CLAUDE_CODE_OAUTH_TOKEN env var
                                  (or mount .credentials.json)
```

### Alternative: `setup-token` Flow (Simpler, Recommended)

```
User's machine                    Platform
──────────────                    ────────
claude setup-token
  ↓ (outputs 1-year token)
Copy token                   →    POST /api/auth/cli-credentials
                                  { "auth_type": "setup_token", "token": "..." }
                                    ↓
                                  encrypt + store
                                    ↓
                              On session spawn:
                                  decrypt token
                                  set CLAUDE_CODE_OAUTH_TOKEN=$token
```

The `setup-token` approach avoids the refresh-token race condition entirely since it's a single long-lived token. No concurrent refresh issues.

### Code Changes

| File | Change |
|------|--------|
| `src/auth/cli_creds.rs` | New: `store_credentials()`, `get_credentials()`, `delete_credentials()` with AES-256-GCM encryption via `secrets::engine`. Must check `PLATFORM_MASTER_KEY` exists (return `ApiError::ServiceUnavailable` if missing). |
| `src/auth/mod.rs` | Add `pub mod cli_creds;` |
| `src/api/cli_auth.rs` | New: `POST /api/auth/cli-credentials` (store), `GET /api/auth/cli-credentials` (check existence), `DELETE /api/auth/cli-credentials` (remove). All handlers require `AuthUser`. Store/delete must write to `audit_log` (action: `cli_creds.store`, `cli_creds.delete` — never log the credential value). Apply rate limiting on POST: `check_rate(&valkey, "cli-creds", &user_id, 10, 300)`. |
| `src/api/mod.rs` | Wire new routes: `.merge(cli_auth::router())` |
| `src/agent/service.rs` | In `create_session()`: resolve CLI credentials alongside API key |
| `src/agent/claude_cli/transport.rs` | Accept `oauth_token` in `CliSpawnOptions` (already in struct) |

### API Endpoints

```
POST   /api/auth/cli-credentials   — Store encrypted CLI credentials
GET    /api/auth/cli-credentials   — Check if credentials exist (no secrets returned)
DELETE /api/auth/cli-credentials   — Remove stored credentials
```

### Tests to write FIRST (before implementation) — PR 2

**Unit tests — `src/auth/cli_creds.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_encrypt_decrypt_roundtrip_setup_token` | Setup token survives encrypt→decrypt | Unit |
| `test_encrypt_decrypt_roundtrip_oauth_json` | OAuth JSON blob survives encrypt→decrypt | Unit |
| `test_decrypt_with_wrong_key_fails` | Tampered key produces error | Unit |
| `test_decrypt_with_truncated_data_fails` | Short blob (< 12+16 bytes) returns error | Unit |
| `test_missing_master_key_returns_error` | No PLATFORM_MASTER_KEY → ServiceUnavailable | Unit |
| `test_store_credentials_validates_auth_type` | Only "oauth" and "setup_token" accepted | Unit |

**Integration tests — `tests/cli_auth_integration.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_store_setup_token_credentials` | POST /api/auth/cli-credentials stores encrypted | Integration |
| `test_store_oauth_credentials` | POST with auth_type=oauth stores correctly | Integration |
| `test_get_credentials_returns_existence_only` | GET returns `{exists: true, auth_type}` — no secrets | Integration |
| `test_get_credentials_not_found` | GET when none stored returns `{exists: false}` | Integration |
| `test_delete_credentials` | DELETE removes row, returns 204 | Integration |
| `test_delete_credentials_idempotent` | DELETE when none exists returns 204 | Integration |
| `test_store_credentials_upsert` | Second POST same auth_type replaces | Integration |
| `test_store_credentials_requires_auth` | POST without token returns 401 | Integration |
| `test_get_credentials_requires_auth` | GET without token returns 401 | Integration |
| `test_store_credentials_rate_limited` | 11th POST within 5min returns 429 | Integration |
| `test_store_credentials_audit_logged` | POST writes to audit_log action=cli_creds.store | Integration |
| `test_delete_credentials_audit_logged` | DELETE writes to audit_log | Integration |
| `test_credentials_cascade_on_user_delete` | Deleting user cascades to cli_credentials | Integration |
| `test_user_cannot_read_other_user_creds` | GET only returns own creds | Integration |
| `test_empty_token_rejected` | POST with empty token returns 400 | Integration |

**Branch coverage checklist:**

| Branch/Path | Test that covers it |
|---|---|
| `store: auth_type=setup_token` | `test_store_setup_token_credentials` |
| `store: auth_type=oauth` | `test_store_oauth_credentials` |
| `store: invalid auth_type → 400` | `test_store_credentials_validates_auth_type` |
| `store: empty token → 400` | `test_empty_token_rejected` |
| `store: upsert on conflict` | `test_store_credentials_upsert` |
| `store: rate limit exceeded → 429` | `test_store_credentials_rate_limited` |
| `get: exists → {exists:true}` | `test_get_credentials_returns_existence_only` |
| `get: not found → {exists:false}` | `test_get_credentials_not_found` |
| `delete: exists → 204` | `test_delete_credentials` |
| `delete: not found → 204` | `test_delete_credentials_idempotent` |
| `missing master key → 503` | `test_missing_master_key_returns_error` |
| `wrong key → decrypt error` | `test_decrypt_with_wrong_key_fails` |

**Tests NOT needed:**
- E2E — no K8s involvement; credential storage is DB + crypto
- Token refresh integration — the CLI handles refresh, not the platform

**Total: 6 unit + 15 integration = 21 tests**

---

## PR 3: CLI Session API — Platform-Side Subprocess Mode

Wire the subprocess transport into the session API so sessions can run as CLI subprocesses directly in the platform pod (not K8s agent pods).

- [ ] Types & errors defined
- [ ] Migration applied
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### Migration: `20260302030001_session_execution_mode`

**Up:**
```sql
-- Add execution mode to sessions
ALTER TABLE agent_sessions
    ADD COLUMN execution_mode TEXT NOT NULL DEFAULT 'pod'
    CHECK (execution_mode IN ('pod', 'cli_subprocess', 'inprocess'));

-- Backfill: existing sessions without a pod that are completed/stopped are
-- 'inprocess'. We intentionally do NOT backfill 'pending' sessions without
-- pod_name — those are pod sessions waiting for pod creation (not inprocess).
UPDATE agent_sessions
    SET execution_mode = 'inprocess'
    WHERE pod_name IS NULL
      AND status IN ('completed', 'stopped', 'running')
      AND provider = 'inprocess';
```

**Down:**
```sql
ALTER TABLE agent_sessions DROP COLUMN execution_mode;
```

### Architecture: Session Execution Modes

| Mode | Where it runs | Auth | Tools | Use case |
|------|--------------|------|-------|----------|
| `pod` | K8s pod | API key or subscription | Full Claude Code tools | Heavy coding agents |
| `cli_subprocess` | Platform pod process | Subscription (or API key) | Full Claude Code tools | Quick sessions, /dev prompts |
| `inprocess` | Platform pod (Rust) | API key only | Custom tools only | Create-app flow (existing) |

### Subprocess Session Manager

```rust
/// Manages active CLI subprocess sessions running in the platform pod.
pub struct CliSessionManager {
    /// Active sessions: session_id → (transport, broadcast_tx)
    sessions: Arc<RwLock<HashMap<Uuid, CliSessionHandle>>>,
}

struct CliSessionHandle {
    transport: Arc<Mutex<SubprocessTransport>>,
    tx: broadcast::Sender<ProgressEvent>,
    mode: SessionMode,
    session_id: Uuid,
    cli_session_id: Option<String>,  // Claude CLI's internal session ID
}

/// NOTE: SessionMode is an internal enum for the CliSessionManager only —
/// it does NOT get a DB column. The session API uses the `persistent_session`
/// bool from platform_commands (PR 5). Default is one-shot.
pub enum SessionMode {
    /// One-shot: send prompt, stream result, kill process
    OneShot,
    /// Persistent: keep process alive for multi-turn conversation
    Persistent,
}
```

### AppState Addition

```rust
pub struct AppState {
    // ... existing fields ...
    pub cli_sessions: CliSessionManager,
}
```

### API Endpoint Changes

Existing endpoints get a new `execution_mode` option:

```rust
// POST /api/projects/{id}/sessions
#[derive(Deserialize)]
pub struct CreateSessionRequest {
    pub prompt: String,
    pub provider: Option<String>,
    pub branch: Option<String>,
    pub provider_config: Option<serde_json::Value>,
    // NEW:
    pub execution_mode: Option<String>,  // "pod" (default), "cli_subprocess"
}
```

New endpoints for CLI subprocess sessions:

```
POST /api/sessions/cli          — Create CLI subprocess session (project-optional)
POST /api/sessions/{id}/message — Send follow-up message
POST /api/sessions/{id}/stop    — Stop/kill CLI process
GET  /api/sessions/{id}/ws      — WebSocket stream (reuse existing)
```

### Session Lifecycle: `cli_subprocess`

```
1. POST /api/sessions/cli
   - Resolve auth: CLI credentials → API key → error
   - Spawn SubprocessTransport with auth + options
   - Wait for System init message (session_id)
   - Send initial prompt via stdin
   - Store session in CliSessionManager
   - Insert DB row (execution_mode='cli_subprocess')
   - Return session info

2. Stream via WebSocket (same endpoint as pod sessions)
   - Read CliMessages from stdout
   - Convert to ProgressEvent (reuse existing types)
   - Broadcast to WebSocket subscribers
   - Store in agent_messages table

3. Follow-up messages
   - POST /api/sessions/{id}/message
   - Write to subprocess stdin
   - Continue streaming

4. Completion
   - Result message received → mark session complete
   - One-shot: kill process
   - Persistent: keep alive for next message

5. Stop
   - Send interrupt control request
   - Kill process if no response
   - Mark session stopped
```

### WebSocket Handler Update

```rust
// In handle_ws(): add branch for cli_subprocess sessions
match session.execution_mode.as_str() {
    "pod" => { /* existing pod log streaming */ },
    "inprocess" => { /* existing broadcast channel */ },
    "cli_subprocess" => {
        // Subscribe to CliSessionManager broadcast
        let mut rx = state.cli_sessions.subscribe(session_id)?;
        loop {
            tokio::select! {
                event = rx.recv() => { /* send to ws */ },
                msg = socket.recv() => { /* send_message to CLI stdin */ },
            }
        }
    },
    _ => return Err(ApiError::Internal(...)),
}
```

### Code Changes

| File | Change |
|------|--------|
| `src/agent/claude_cli/session.rs` | New: `CliSessionManager`, session lifecycle, concurrent subprocess limit (`PLATFORM_MAX_CLI_SUBPROCESSES`, default 10) |
| `src/agent/claude_cli/mod.rs` | Export session module |
| `src/store/mod.rs` | Add `cli_sessions: CliSessionManager` to AppState |
| `src/main.rs` | Construct `CliSessionManager::new()` and pass to AppState |
| `src/agent/provider.rs` | Add `execution_mode: String` field to `AgentSession` struct |
| `src/agent/service.rs` | (1) Route `cli_subprocess` mode to CliSessionManager. (2) Change `send_message()` routing from `pod_name.is_none()` to `execution_mode` match. (3) Change `stop_session()` similarly. (4) Update `fetch_session()` to SELECT execution_mode. |
| `src/api/sessions.rs` | (1) Add `execution_mode` to `CreateSessionRequest` and `SessionResponse`. (2) Refactor `handle_ws()` into per-mode helper functions to stay under 100 lines. (3) Update `handle_ws_global()` for cli_subprocess mode. |
| `src/config.rs` | Add `max_cli_subprocesses: usize` field (from `PLATFORM_MAX_CLI_SUBPROCESSES`, default 10) |
| `tests/helpers/mod.rs` | Update `test_state()` to include `CliSessionManager::new(10)` |
| `tests/e2e_helpers/mod.rs` | Update `e2e_state()` similarly |

**SECURITY: Subprocess Isolation**

CLI subprocesses run inside the platform pod. To prevent leaking secrets:
1. `SubprocessTransport::spawn()` uses `Command::env_clear()` + explicit whitelist (PATH, HOME, TMPDIR, auth vars, extra_env)
2. Each session gets a unique temp working dir: `/tmp/platform-cli-sessions/{session_id}/`
3. Temp dirs cleaned up on session stop/reaper
4. Concurrent subprocess limit prevents resource exhaustion (default 10, configurable)

### ProgressEvent Conversion

Reuse existing `ProgressEvent`/`ProgressKind` by converting CLI messages:

```rust
fn cli_message_to_progress(msg: &CliMessage) -> Option<ProgressEvent> {
    match msg {
        CliMessage::Assistant(a) => {
            // Parse content blocks → Thinking, Text, ToolCall events
        },
        CliMessage::User(u) => {
            // Tool results → ToolResult events
        },
        CliMessage::Result(r) => {
            // Completion → Completed event with cost metadata
        },
        CliMessage::StreamEvent(s) => {
            // Partial tokens → Text events
        },
        _ => None,
    }
}
```

### Tests to write FIRST (before implementation) — PR 3

**Unit tests — `src/agent/claude_cli/session.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_cli_session_manager_new` | Manager starts with empty sessions map | Unit |
| `test_session_mode_default_oneshot` | Default SessionMode is OneShot | Unit |
| `test_subscribe_unknown_session_returns_error` | subscribe() with invalid UUID returns error | Unit |
| `test_concurrent_limit_enforced` | Creating 11th session when max=10 returns error | Unit |
| `test_concurrent_limit_respects_config` | Limit uses config.max_cli_subprocesses | Unit |
| `test_remove_session_decrements_count` | Stopping a session frees a slot | Unit |
| `test_cli_message_to_progress_assistant` | Assistant message → Text/Thinking ProgressEvent | Unit |
| `test_cli_message_to_progress_result_success` | Result success → Completed ProgressEvent | Unit |
| `test_cli_message_to_progress_result_error` | Result error → Error ProgressEvent | Unit |
| `test_cli_message_to_progress_tool_call` | Tool use → ToolCall ProgressEvent | Unit |
| `test_cli_message_to_progress_tool_result` | User (tool result) → ToolResult ProgressEvent | Unit |
| `test_cli_message_to_progress_stream_event` | Partial token → Text ProgressEvent | Unit |

**Integration tests — `tests/cli_session_integration.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_create_cli_session_returns_session` | POST /api/sessions/cli creates session with execution_mode=cli_subprocess | Integration |
| `test_create_cli_session_requires_auth` | POST without token returns 401 | Integration |
| `test_create_cli_session_stores_db_row` | DB row has execution_mode=cli_subprocess | Integration |
| `test_send_message_routes_to_cli_stdin` | POST /api/sessions/{id}/message writes to subprocess | Integration |
| `test_stop_cli_session_kills_process` | POST /api/sessions/{id}/stop terminates subprocess | Integration |
| `test_stop_cli_session_marks_stopped` | After stop, status=stopped in DB | Integration |
| `test_execution_mode_in_session_response` | GET /api/sessions/{id} includes execution_mode field | Integration |
| `test_list_sessions_includes_cli_sessions` | GET /api/sessions includes cli_subprocess sessions | Integration |
| `test_send_message_to_pod_session_unchanged` | Existing pod send_message still works | Integration |
| `test_send_message_to_inprocess_unchanged` | Existing inprocess send_message still works | Integration |

**Existing tests to UPDATE:**

| Test file | Change | Reason |
|---|---|---|
| `tests/helpers/mod.rs` | Add `cli_sessions: CliSessionManager::new(10)` to AppState | New field |
| `tests/e2e_helpers/mod.rs` | Same | New field |
| `src/api/sessions.rs` (unit tests) | Update SessionResponse assertions for execution_mode field | New field in response |
| All session integration tests | No change needed — default AppState includes CliSessionManager | Backward compatible |

**Branch coverage checklist:**

| Branch/Path | Test that covers it |
|---|---|
| `create: execution_mode=cli_subprocess` | `test_create_cli_session_returns_session` |
| `create: concurrent limit exceeded → 429/503` | `test_concurrent_limit_enforced` |
| `send_message: execution_mode=pod → pod attach` | `test_send_message_to_pod_session_unchanged` |
| `send_message: execution_mode=inprocess → inprocess` | `test_send_message_to_inprocess_unchanged` |
| `send_message: execution_mode=cli_subprocess → stdin` | `test_send_message_routes_to_cli_stdin` |
| `stop: cli_subprocess → kill process` | `test_stop_cli_session_kills_process` |
| `stop: pod → delete pod (unchanged)` | Existing tests |
| `cli_message → ProgressEvent: each variant` | `test_cli_message_to_progress_*` (6 tests) |

**Tests NOT needed:**
- E2E with real Claude CLI — requires Claude binary + valid auth; covered manually
- WebSocket streaming end-to-end — extremely difficult to unit test subprocess + WS; deferred to manual QA with mock CLI

**Total: 12 unit + 10 integration = 22 tests**

---

## PR 4: Pod Mode with CLI Subscription Auth

Update the K8s pod builder to support subscription auth by mounting credentials or injecting the OAuth token as an env var.

- [ ] Types & errors defined
- [ ] Migration applied
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### Pod Auth Resolution

```rust
// In service.rs create_session():
// Priority for pod auth:
// 1. User CLI credentials (setup_token or oauth) → CLAUDE_CODE_OAUTH_TOKEN env var
// 2. User Anthropic API key → ANTHROPIC_API_KEY env var (existing)
// 3. Global platform secret → ANTHROPIC_API_KEY env var (existing)
// 4. Error: no auth configured
```

### Pod Build Changes

```rust
// In pod.rs build_env_vars():
if let Some(oauth_token) = params.cli_oauth_token {
    // Subscription auth via CLI credentials
    env_vars.push(EnvVar {
        name: "CLAUDE_CODE_OAUTH_TOKEN".into(),
        value: Some(oauth_token),
        ..Default::default()
    });
    // Don't set ANTHROPIC_API_KEY — let CLI use OAuth
} else if let Some(api_key) = params.anthropic_api_key {
    // Existing API key auth (unchanged)
    env_vars.push(EnvVar {
        name: "ANTHROPIC_API_KEY".into(),
        value: Some(api_key.into()),
        ..Default::default()
    });
}
```

### Reserved Env Vars Update

Add `CLAUDE_CODE_OAUTH_TOKEN` and `CLAUDE_CONFIG_DIR` to the reserved list in `pod.rs`.

### Code Changes

| File | Change |
|------|--------|
| `src/agent/claude_code/pod.rs` | Add `cli_oauth_token: Option<&'a str>` to `BuildPodParams` (was `PodBuildParams`), inject as `CLAUDE_CODE_OAUTH_TOKEN` env var, add `CLAUDE_CODE_OAUTH_TOKEN` and `CLAUDE_CONFIG_DIR` to `RESERVED_ENV_VARS` |
| `src/agent/provider.rs` | Add `cli_oauth_token` to `BuildPodParams` struct |
| `src/agent/service.rs` | In `create_session()`: call `resolve_cli_auth()`, pass result to `BuildPodParams` |
| `src/auth/cli_creds.rs` | Add `resolve_cli_auth(pool, user_id, master_key) → Result<Option<String>, AgentError>` |

**NOTE:** Adding `cli_oauth_token` to `BuildPodParams` changes the struct shape. The existing ~34 unit tests in `pod.rs` that construct `BuildPodParams` need a mechanical update to add `cli_oauth_token: None`. This is a one-line change per test.

### Tests to write FIRST (before implementation) — PR 4

**Unit tests — `src/agent/claude_code/pod.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_pod_env_includes_oauth_token` | CLAUDE_CODE_OAUTH_TOKEN set when cli_oauth_token is Some | Unit |
| `test_pod_env_no_api_key_when_oauth_set` | ANTHROPIC_API_KEY absent when oauth_token present | Unit |
| `test_pod_env_fallback_to_api_key` | ANTHROPIC_API_KEY set when no cli_oauth_token | Unit |
| `test_oauth_token_is_reserved` | CLAUDE_CODE_OAUTH_TOKEN in RESERVED_ENV_VARS | Unit |
| `test_config_dir_is_reserved` | CLAUDE_CONFIG_DIR in RESERVED_ENV_VARS | Unit |
| `test_both_oauth_and_api_key_prefers_oauth` | When both set, only CLAUDE_CODE_OAUTH_TOKEN injected | Unit |
| `test_no_auth_configured_returns_error` | Neither oauth nor api_key → ConfigurationRequired | Unit |

**Integration tests — `tests/session_pod_auth_integration.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_create_pod_session_with_cli_creds` | Session creation resolves CLI creds for pod auth | Integration |
| `test_create_pod_session_without_cli_creds` | Falls back to API key when no CLI creds stored | Integration |
| `test_resolve_cli_auth_returns_none_when_no_creds` | resolve_cli_auth with no stored creds returns None | Integration |

**Existing tests to UPDATE:**

| Test file | Change | Reason |
|---|---|---|
| `src/agent/claude_code/pod.rs` (~34 tests) | Add `cli_oauth_token: None` to `BuildPodParams` construction | New struct field |

**Branch coverage checklist:**

| Branch/Path | Test that covers it |
|---|---|
| `build_env: oauth_token present → set env` | `test_pod_env_includes_oauth_token` |
| `build_env: oauth_token → skip API key` | `test_pod_env_no_api_key_when_oauth_set` |
| `build_env: no oauth → API key fallback` | `test_pod_env_fallback_to_api_key` |
| `reserved: CLAUDE_CODE_OAUTH_TOKEN blocked` | `test_oauth_token_is_reserved` |
| `resolve_cli_auth: creds exist → decrypt` | `test_create_pod_session_with_cli_creds` |
| `resolve_cli_auth: no creds → None` | `test_resolve_cli_auth_returns_none_when_no_creds` |

**Tests NOT needed:**
- E2E with real K8s pod + OAuth token — would need valid Claude subscription; too flaky for CI

**Total: 7 unit + 3 integration = 10 tests (+ 34 existing tests need mechanical `cli_oauth_token: None` update)**

---

## PR 5: Platform Commands (Skill Prompts)

Enable `/dev`-style command prompts that inject skill instructions into sessions.

- [ ] Types & errors defined
- [ ] Migration applied
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### Design

Platform commands are **prompt templates** stored as markdown files (same format as `.claude/commands/`). When a user sends `/dev`, the platform:

1. Looks up the command definition (markdown file or DB record)
2. Reads the skill prompt
3. Prepends it to the user's message (or uses `--append-system-prompt`)
4. Sends to the CLI subprocess

### Migration: `20260302050001_platform_commands`

**Up:**
```sql
CREATE TABLE platform_commands (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    -- NULL = global command, set = project-scoped
    project_id UUID REFERENCES projects(id) ON DELETE CASCADE,
    name TEXT NOT NULL,                    -- e.g. "dev", "review", "plan"
    description TEXT NOT NULL DEFAULT '',
    -- The prompt template (markdown). Supports $ARGUMENTS placeholder.
    prompt_template TEXT NOT NULL,
    -- Whether to keep session alive after execution
    persistent_session BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE(project_id, name)
);

-- PostgreSQL UNIQUE treats NULL as distinct (NULL != NULL), so
-- UNIQUE(project_id, name) allows duplicate global commands.
-- This partial unique index enforces uniqueness for global commands.
CREATE UNIQUE INDEX idx_platform_commands_global_name
    ON platform_commands(name) WHERE project_id IS NULL;

-- Seed built-in commands from .claude/commands/
-- (These mirror the platform's own dev workflow commands)
```

**Down:**
```sql
DROP TABLE platform_commands;
```

### Command Resolution

```rust
pub struct ResolvedCommand {
    pub name: String,
    pub prompt: String,           // Template with $ARGUMENTS replaced
    pub persistent: bool,         // Keep session alive after completion
    pub append_system_prompt: Option<String>, // Additional instructions
}

/// Resolve a command like "/dev fix the auth bug"
/// 1. Parse command name and arguments: name="dev", args="fix the auth bug"
/// 2. Look up command: project-scoped → global → built-in
/// 3. Render template: replace $ARGUMENTS with args
pub async fn resolve_command(
    pool: &PgPool,
    project_id: Option<Uuid>,
    input: &str,
) -> Result<ResolvedCommand, ApiError>;
```

### API Changes

The create-session endpoint accepts commands:

```rust
// POST /api/sessions/cli
{
    "prompt": "/dev fix the authentication bug in auth.rs",
    "project_id": "...",
    "execution_mode": "cli_subprocess"
}
// Platform detects "/dev" prefix, resolves command, expands prompt
```

### Built-in Commands (seeded from existing .claude/commands/)

| Command | Session | Description |
|---------|---------|-------------|
| `/dev` | Persistent | Development workflow: investigate → test → implement → verify |
| `/plan` | One-shot | Create implementation plan from codebase investigation |
| `/review` | One-shot | Parallel code review (Rust quality, tests, security) |
| `/plan-review` | One-shot | Validate plan + design TDD test tables |
| `/finalize` | One-shot | Triage findings, fix, verify coverage, branch + commit + PR |

### Code Changes

| File | Change |
|------|--------|
| `src/agent/commands.rs` | New: `resolve_command()`, template rendering, `parse_command_input()`. Input validation: name 1-100 chars, alphanumeric + hyphens only. Template max 100KB. |
| `src/agent/mod.rs` | Add `pub mod commands;` |
| `src/api/commands.rs` | New: CRUD endpoints. `POST /api/commands` requires `Permission::AdminSettings` for global commands or `Permission::ProjectWrite` for project-scoped. `PUT /api/commands/{id}` and `DELETE /api/commands/{id}` similarly. `GET /api/commands` requires auth. All mutations write to `audit_log`. |
| `src/api/sessions.rs` | Parse `/command` prefix in `POST /api/sessions/cli` ONLY (not in existing `POST /api/projects/{id}/sessions` — existing endpoint stays unchanged). |
| `src/api/mod.rs` | Wire command routes: `.merge(commands::router())` |

**IMPORTANT:** Command parsing (`/dev`, `/plan`) only applies to the new `POST /api/sessions/cli` endpoint. The existing `POST /api/projects/{id}/sessions` endpoint does NOT parse commands — prompts pass through verbatim. This prevents breaking existing agent session creation.

### Tests to write FIRST (before implementation) — PR 5

**Unit tests — `src/agent/commands.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_parse_command_input_dev` | `/dev fix bug` → name="dev", args="fix bug" | Unit |
| `test_parse_command_input_no_args` | `/review` → name="review", args="" | Unit |
| `test_parse_command_input_not_a_command` | `fix the bug` → None (not a command) | Unit |
| `test_parse_command_input_slash_in_middle` | `fix /dev bug` → None (slash not at start) | Unit |
| `test_parse_command_input_empty` | `` → None | Unit |
| `test_template_arguments_substitution` | `$ARGUMENTS` replaced with args | Unit |
| `test_template_no_arguments_placeholder` | Template without `$ARGUMENTS` unchanged | Unit |
| `test_template_multiple_arguments_placeholders` | All `$ARGUMENTS` replaced | Unit |
| `test_command_name_validation_valid` | "dev", "plan-review", "my_cmd" accepted | Unit |
| `test_command_name_validation_invalid` | Empty, spaces, special chars rejected | Unit |
| `test_template_size_limit` | Template > 100KB rejected | Unit |

**Integration tests — `tests/commands_integration.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_create_global_command` | POST /api/commands creates global command | Integration |
| `test_create_project_command` | POST with project_id creates scoped command | Integration |
| `test_create_command_requires_admin` | Non-admin creating global command → 403 | Integration |
| `test_project_command_requires_project_write` | Need ProjectWrite for project-scoped | Integration |
| `test_list_commands` | GET /api/commands returns list | Integration |
| `test_resolve_project_overrides_global` | Project-scoped "dev" chosen over global "dev" | Integration |
| `test_resolve_unknown_command_404` | Unknown command name returns 404 | Integration |
| `test_duplicate_global_command_rejected` | Second global command with same name → 409 | Integration |
| `test_duplicate_project_command_rejected` | Same name in same project → 409 | Integration |
| `test_same_name_different_projects_ok` | Same name in different projects allowed | Integration |
| `test_delete_command` | DELETE /api/commands/{id} removes command | Integration |
| `test_update_command` | PUT /api/commands/{id} updates template | Integration |
| `test_command_audit_logged` | Create/update/delete write audit_log | Integration |
| `test_cli_session_with_command_prefix` | POST /api/sessions/cli with "/dev fix bug" resolves command | Integration |

**Existing tests to UPDATE:**

| Test file | Change | Reason |
|---|---|---|
| None | — | New module; existing session endpoints unchanged |

**Branch coverage checklist:**

| Branch/Path | Test that covers it |
|---|---|
| `parse: starts with / → extract name+args` | `test_parse_command_input_dev` |
| `parse: no / prefix → None` | `test_parse_command_input_not_a_command` |
| `parse: empty input → None` | `test_parse_command_input_empty` |
| `resolve: project-scoped found → use it` | `test_resolve_project_overrides_global` |
| `resolve: project-scoped miss → fall back to global` | `test_resolve_project_overrides_global` |
| `resolve: not found → 404` | `test_resolve_unknown_command_404` |
| `template: has $ARGUMENTS → replace` | `test_template_arguments_substitution` |
| `template: no placeholder → unchanged` | `test_template_no_arguments_placeholder` |
| `global unique: duplicate → 409` | `test_duplicate_global_command_rejected` |
| `permission: admin for global` | `test_create_command_requires_admin` |
| `permission: project_write for scoped` | `test_project_command_requires_project_write` |

**Tests NOT needed:**
- E2E — no K8s involvement; command resolution is DB + string processing
- Seeding built-in commands — tested implicitly via integration tests (bootstrap seeds them)

**Total: 11 unit + 14 integration = 25 tests**

---

## PR 6: Client Binary (`platform-cli`)

> **Superseded**: platform-cli deleted in Plan 38. Agent communication uses Valkey pub/sub via agent-runner instead.

Standalone Rust binary that connects to the platform via WebSocket for remote session interaction.

- [ ] Types & errors defined
- [ ] Migration applied
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### Binary: `cli/platform-cli/`

New Cargo workspace member (or standalone binary in the repo):

```
cli/platform-cli/
├── Cargo.toml
├── src/
│   ├── main.rs         # CLI entry point (clap)
│   ├── auth.rs         # Platform authentication (API token)
│   ├── client.rs       # HTTP + WebSocket client
│   ├── session.rs      # Session management commands
│   ├── stream.rs       # WebSocket streaming + terminal output
│   ├── commands.rs     # /dev, /plan etc. command dispatch
│   ├── config.rs       # Config file (~/.platform-cli.toml)
│   └── ui.rs           # Terminal UI (colored output, spinners, progress)
```

**Architecture note:** This is a SEPARATE binary, NOT part of the main platform crate. The project's "single crate" rule applies to the **platform server** — the client CLI is a different build artifact with different dependencies (clap, tokio-tungstenite, colored) and no DB/K8s deps. It lives at `cli/platform-cli/` with its own `Cargo.toml`. It is NOT a Cargo workspace member (no `[workspace]` in root Cargo.toml). It shares zero code with the server — message types are duplicated intentionally to avoid coupling. Build with `just cli-build` (separate cargo invocation).

### CLI Interface

```
platform-cli — Remote Claude agent session manager

USAGE:
    platform-cli [OPTIONS] <COMMAND>

COMMANDS:
    login           Authenticate with the platform
    upload-creds    Upload Claude CLI credentials to the platform
    session         Session management
    send            Send a message to an active session
    watch           Watch a session's output stream
    projects        List projects

SESSION SUBCOMMANDS:
    platform-cli session create [OPTIONS] <PROMPT>
        --project <name|id>     Target project
        --mode <oneshot|persistent>  Session mode (default: oneshot)
        --execution <pod|cli>   Where to run (default: cli)
        --attach                Attach to output stream after creation

    platform-cli session list
        --project <name|id>     Filter by project
        --status <running|completed|all>

    platform-cli session attach <SESSION_ID>
        Interactive WebSocket attachment to a running session

    platform-cli session stop <SESSION_ID>

SHORTHAND:
    platform-cli dev "fix the auth bug"
        ≡ session create --mode persistent "/dev fix the auth bug" --attach

    platform-cli plan "add caching layer"
        ≡ session create --mode oneshot "/plan add caching layer" --attach

    platform-cli review
        ≡ session create --mode oneshot "/review" --attach
```

### Config: `~/.platform-cli.toml`

```toml
[server]
url = "https://platform.example.com"   # Platform URL
token = "plat_..."                      # API token

[defaults]
project = "my-project"                  # Default project
execution_mode = "cli_subprocess"       # Default execution mode
```

### WebSocket Streaming

```rust
pub async fn attach_session(
    config: &Config,
    session_id: Uuid,
    interactive: bool,
) -> Result<(), CliError> {
    let ws_url = format!(
        "{}/api/sessions/{}/ws",
        config.server.url.replace("http", "ws"),
        session_id,
    );

    let (mut ws, _) = connect_async(&ws_url).await?;

    // Spawn input reader (if interactive)
    if interactive {
        let stdin_tx = ws.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(tokio::io::stdin());
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).await?;
                stdin_tx.send(Message::Text(json!({"content": line.trim()}).to_string())).await?;
            }
        });
    }

    // Stream output
    while let Some(msg) = ws.next().await {
        let event: ProgressEvent = serde_json::from_str(&msg?.to_text()?)?;
        render_event(&event);  // Colored terminal output
        if event.kind == ProgressKind::Completed {
            // Desktop notification (optional)
            notify("Session completed", &event.message)?;
            if !interactive { break; }
        }
    }

    Ok(())
}
```

### Terminal Output Rendering

```
$ platform-cli dev "fix the auth module token validation"

🔄 Creating session... (cli_subprocess, persistent)
📎 Project: my-platform | Session: a3f2c1e8

💭 Thinking: Let me analyze the auth module...
📖 Reading src/auth/token.rs
📖 Reading src/auth/middleware.rs
💭 I see the issue — the token validation doesn't check expiry...
✏️  Editing src/auth/token.rs
🔧 Running: cargo test --lib auth
✅ Tests passed (12/12)
📝 The token validation now checks the `exp` claim...

⏳ Session active — type a message or Ctrl+C to detach
> now add a test for expired tokens
💭 Good idea, let me add a test...
```

### Completion Notifications

When a session completes and the client is in background/detached mode:
- **macOS**: `osascript` for native notifications
- **Linux**: `notify-send` via D-Bus
- **All**: Terminal bell (`\x07`)
- **Optional**: Webhook callback URL in config

### Code Changes

| File | Change |
|------|--------|
| `cli/platform-cli/Cargo.toml` | New crate: clap, tokio, tokio-tungstenite, serde, colored, dirs |
| `cli/platform-cli/src/*.rs` | Full client implementation |
| `justfile` | Add `just cli-build` target |

### Build & Distribution

```just
# In justfile
cli-build:
    cargo build --release --manifest-path cli/platform-cli/Cargo.toml

cli-install:
    cargo install --path cli/platform-cli
```

### Tests to write FIRST (before implementation) — PR 6

**Unit tests — `cli/platform-cli/src/`**

| Test | Validates | Layer |
|---|---|---|
| `test_config_parse_toml` | Valid TOML config file parsed | Unit |
| `test_config_parse_missing_server` | Missing [server] section → error | Unit |
| `test_config_default_execution_mode` | Defaults to "cli_subprocess" | Unit |
| `test_shorthand_dev_expands` | `dev "fix bug"` → session create with /dev prefix | Unit |
| `test_shorthand_plan_expands` | `plan "add caching"` → session create with /plan prefix | Unit |
| `test_shorthand_review_expands` | `review` → session create with /review prefix | Unit |
| `test_render_thinking_event` | Thinking ProgressEvent → "Thinking: ..." output | Unit |
| `test_render_tool_call_event` | ToolCall → "Reading/Editing..." output | Unit |
| `test_render_completed_event` | Completed → success message + cost | Unit |
| `test_render_error_event` | Error → red error output | Unit |
| `test_ws_url_from_http` | http://host → ws://host conversion | Unit |
| `test_ws_url_from_https` | https://host → wss://host conversion | Unit |
| `test_session_create_request_shape` | Request body matches expected JSON shape | Unit |
| `test_auth_header_from_config` | API token set in Authorization header | Unit |
| `test_cli_args_parsed_correctly` | clap arg parsing for all subcommands | Unit |
| `test_cli_args_project_flag` | --project flag passes through | Unit |

**Tests NOT needed:**
- Integration tests against running platform — this is a client binary; integration happens at the HTTP/WS boundary which is already tested by PR 3's integration tests
- E2E — the client is a thin shell over HTTP/WS; would require a full platform instance
- Notification tests (platform-specific) — OS notification is best-effort, not testable in CI

**Total: 16 unit tests**

---

## Cross-Cutting Concerns

### Refresh Token Race Condition

The Claude CLI has a [documented race condition](https://github.com/anthropics/claude-code/issues/27933) where concurrent processes compete for single-use refresh tokens. Mitigations:

1. **Prefer `setup-token`** — generates a 1-year token, no refresh needed
2. **Use `CLAUDE_CODE_OAUTH_TOKEN` env var** — bypasses file-based credential storage
3. **Single-writer pattern** — only one CLI process per user should refresh at a time
4. For pod mode: decrypt token at spawn time and inject via env var (no shared file)

### Security

- CLI credentials encrypted at rest via `PLATFORM_MASTER_KEY` (existing AES-256-GCM engine)
- Tokens never logged (audit entries omit credential values)
- `CLAUDE_CODE_OAUTH_TOKEN` added to reserved env vars (can't be overridden by project secrets)
- Client binary authenticates with platform API token (not Claude credentials directly)
- CLI subprocess inherits platform pod's network policy (no unexpected egress)

### Rate Limiting (Subscription)

Claude subscription has usage limits that vary with server demand. The platform should:
- Surface rate-limit errors from CLI output to the user (not retry silently)
- Track per-user session count to prevent runaway usage
- Existing rate limits on session creation (10/5min) still apply

---

## Summary

| PR | What | LOC Estimate | Depends On |
|----|------|-------------|------------|
| 1 | CLI Subprocess Transport | ~500 | — |
| 2 | Auth Credential Management | ~300 + migration | PR 1 |
| 3 | CLI Session API (platform-side) | ~600 + migration | PR 1, 2 |
| 4 | Pod Mode with Subscription Auth | ~100 | PR 2 |
| 5 | Platform Commands | ~400 + migration | PR 3 |
| 6 | Client Binary | ~800 (separate crate) | PR 3, 5 |

Total: ~2,700 LOC across 6 PRs + 3 migrations + 1 new binary crate.

PRs 1-3 form the critical path. PR 4 can be done in parallel with PR 3. PRs 5-6 build on top.

---

## Test Plan Summary

### Coverage target: 100% of touched lines

Every new or modified line of code must be covered by at least one test
(unit, integration, or E2E). The test strategy above maps each code path
to a specific test. `review` and `finalize` will verify this with `just cov-unit`
/ `just cov-total`.

### New test counts by PR

| PR | Unit | Integration | E2E | Total |
|---|---|---|---|---|
| PR 1: CLI Transport | 39 | 0 | 0 | 39 |
| PR 2: Auth Credentials | 6 | 15 | 0 | 21 |
| PR 3: CLI Session API | 12 | 10 | 0 | 22 |
| PR 4: Pod Auth | 7 | 3 | 0 | 10 |
| PR 5: Platform Commands | 11 | 14 | 0 | 25 |
| PR 6: Client Binary | 16 | 0 | 0 | 16 |
| **Total** | **91** | **42** | **0** | **133** |

Plus ~34 existing pod.rs unit tests need mechanical update (add `cli_oauth_token: None`).

### Coverage goals by module

| Module | Current tests | After plan |
|---|---|---|
| `src/agent/claude_cli/` | 0 (new module) | +51 unit |
| `src/auth/cli_creds.rs` | 0 (new file) | +6 unit |
| `src/api/cli_auth.rs` | 0 (new file) | +15 integration |
| `src/api/commands.rs` | 0 (new file) | +14 integration |
| `src/agent/commands.rs` | 0 (new file) | +11 unit |
| `src/agent/claude_code/pod.rs` | 34 unit | +7 unit (pod auth) |
| `cli/platform-cli/` | 0 (new crate) | +16 unit |

---

## Plan Review Findings

**Date:** 2026-03-02
**Status:** APPROVED WITH CONCERNS

### Codebase Reality Check

Issues found and corrected in-place above:

1. **Encryption schema mismatch** — Plan had separate `encrypted_data` + `nonce` columns, but `engine::encrypt()` returns `nonce||ciphertext||tag` as a single blob. Fixed to use single `encrypted_data BYTEA` matching `user_provider_keys.encrypted_key` pattern.

2. **Error routing wrong** — Plan routed `CliError → ApiError` directly in `src/error.rs`. Fixed to route through `AgentError` (`#[error(transparent)] Cli(#[from] CliError)`) matching existing error hierarchy.

3. **Missing files in code changes** — Plan omitted `src/main.rs`, `src/agent/provider.rs` (AgentSession struct), `src/config.rs`, `src/auth/mod.rs` from several PRs. Fixed.

4. **Migration backfill too aggressive** — Original `WHERE pod_name IS NULL` would misclassify pending pod sessions (awaiting pod creation). Fixed with `WHERE pod_name IS NULL AND status IN ('completed', 'stopped', 'running') AND provider = 'inprocess'`.

5. **NULL UNIQUE constraint** — `UNIQUE(project_id, name)` allows duplicate global commands because `NULL != NULL` in PostgreSQL. Added partial unique index: `CREATE UNIQUE INDEX ... ON platform_commands(name) WHERE project_id IS NULL`.

6. **`stream()` method** — Plan used `async-stream` crate and `impl Stream` which don't exist in the codebase. Replaced with `recv()` + loop pattern matching existing `handle_ws()` style.

7. **CliSpawnOptions** — 16 fields without `Default` derive. Added `#[derive(Default)]`.

8. **Routing heuristic** — `send_message()` and `stop_session()` used `pod_name.is_none()` to detect inprocess. Now uses `execution_mode` match which is correct for three modes.

### Remaining Concerns

1. **Subprocess security is the #1 risk.** The CLI subprocess runs inside the platform pod with the same filesystem and (by default) the same environment. The plan now specifies `env_clear()` + whitelist, but implementation must be reviewed carefully. A missed env var leak exposes `DATABASE_URL`, `PLATFORM_MASTER_KEY`, etc.

2. **handle_ws() is already 111 lines** (clippy limit: 100). PR 3 adds a third execution mode branch. The plan now calls for refactoring into per-mode helper functions, but this must actually happen or clippy will fail.

3. **No E2E tests.** All 133 tests are unit + integration. Testing against a real Claude CLI binary would catch protocol mismatches, but requires a valid subscription. Consider adding an optional E2E test behind a feature flag or env var gate in a future PR.

4. **Client binary type duplication.** PR 6 duplicates `ProgressEvent`, `ProgressKind`, and other types in the client crate. This is intentional (no shared code), but changes to server-side types need manual sync to the client. Document this in a `cli/platform-cli/README.md`.

5. **Refresh token race.** The plan recommends `setup-token` over OAuth credentials, but both are supported. If a user uploads OAuth credentials and runs multiple concurrent sessions (pod + subprocess), they'll hit the documented refresh token race. The plan mitigates this with `CLAUDE_CODE_OAUTH_TOKEN` env var (bypasses file storage), but should add a warning in the API response when `auth_type=oauth` and the user has an active session.

### Simplification Opportunities

1. **SessionMode enum may be premature.** One-shot vs persistent can be a simple `bool` on `CliSessionHandle`. The enum adds ceremony without adding type safety (both variants behave the same except for process lifecycle after Result). Consider `keep_alive: bool` instead.

2. **PR 2 + PR 4 could merge.** Auth credential management (PR 2) and pod auth integration (PR 4) are tightly coupled. Merging them into a single "CLI Auth" PR would reduce integration risk and test overhead. Kept separate in the plan for reviewer clarity, but dev may combine.

### Security Notes

- `CLAUDE_CODE_OAUTH_TOKEN` MUST be in `RESERVED_ENV_VARS` (prevents project secrets from overriding it)
- `CLAUDE_CONFIG_DIR` also reserved (prevents pointing CLI at attacker-controlled config)
- Audit log entries for `cli_creds.store` and `cli_creds.delete` must NEVER include the credential value
- Rate limiting on credential storage endpoint prevents brute-force token enumeration
- Subprocess concurrent limit (default 10) prevents resource exhaustion DoS
- Temp working directories under `/tmp/platform-cli-sessions/` must be cleaned on session stop and by the reaper
