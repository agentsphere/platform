# Plan 39: Agent-Runner Platform Integration

## Context

Plan 38 designed the standalone `agent-runner` CLI crate (`cli/agent-runner/`) that wraps the Claude Code CLI, isolates config, handles auth, and connects back to the platform via Valkey pub/sub. This plan implements the **future work items** from Plan 38: the platform-side infrastructure needed to fully integrate agent-runner into the pod lifecycle.

Currently, agent pods run Claude CLI directly with `--print` (one-shot mode). The main container in `src/agent/claude_code/pod.rs:339-369` passes Claude CLI args directly, tails pod stdout for progress events, and writes to pod stdin for messages. This architecture has limitations:
- **No multi-turn persistence** — each pod runs one prompt and exits
- **No structured event streaming** — progress events are parsed from raw log lines
- **No cross-node communication** — pod log tailing requires direct K8s API access
- **No security isolation** — agents share the platform's Valkey connection

With agent-runner + Valkey pub/sub, agent pods get:
- Multi-turn conversations via pub/sub input channel
- Structured event streaming via pub/sub events channel
- Per-session Valkey ACL isolation (agents can only access their own channels)
- MCP server integration (Claude can call platform APIs for issues, pipelines, etc.)
- Decoupled architecture — platform subscribes to events instead of tailing logs

**Prerequisite**: Plan 38 core implementation (the `cli/agent-runner/` crate with REPL, pub/sub client, transport, render modules) must be complete before this plan.

## Relationship to Plans 38 and 40

- **Plan 38** (complete): Standalone `agent-runner` CLI wrapper crate (`cli/agent-runner/`). Wraps Claude CLI with REPL + pub/sub. The `--prompt` / `-p` flag and single-shot behavior are **already implemented** — agent-runner sends the prompt, streams responses, and exits when stdin closes (natural behavior in K8s pods where stdin is a pipe).
- **Plan 40** (in progress): Replaces in-process Anthropic API with Claude CLI subprocess for the "create-app" manager agent. Uses structured output (`--tools "" --json-schema`). Publishes events to the SAME pub/sub channels (`session:{id}:events`). Depends on this plan's pub/sub bridge.
- **This plan (39)**: Platform-side integration infrastructure — Valkey ACL scoping, pod builder updates, pub/sub event bridge. Required by BOTH Plan 38 (dev agents in pods) and Plan 40 (manager agent event publishing).

### Critical CLI Learnings (from Plan 38 debugging)

1. **`--input-format stream-json` blocks on piped stdin** — In pod mode, the agent-runner uses `--input-format stream-json` and sends prompts via stdin JSON. Do NOT combine with `--print` or `-p` (they conflict with piped stdin reads).
2. **`env_clear()` is required in pod mode** — Prevents leaking `DATABASE_URL`, `PLATFORM_MASTER_KEY` to the Claude CLI subprocess. Must whitelist PATH, HOME, TMPDIR for Node.js runtime.
3. **OAuth via `CLAUDE_CODE_OAUTH_TOKEN`** — Platform resolves from secrets engine and passes as env var. Do NOT use `CLAUDE_CONFIG_DIR` with temp dirs (loses OAuth credentials).
4. **Exit behavior** — Agent-runner exits naturally after Result message when stdin EOF is received (K8s pod behavior). No explicit `--single-shot` flag needed.

## Design Principles

- **Backwards compatible** — existing pod log streaming continues to work for sessions that don't use agent-runner. The pub/sub bridge checks `uses_pubsub` flag before attempting subscription, then falls through to pod logs.
- **Minimal config surface** — one new env var (`PLATFORM_VALKEY_AGENT_HOST`) controls how agents reach Valkey from inside K8s. Everything else is derived.
- **Security isolation** — each agent session gets a Valkey ACL user with `resetkeys resetchannels -@all` baseline then explicit `+subscribe +publish +unsubscribe +ping` on `&session:{id}:*`. No `+@pubsub` category (which would include dangerous diagnostic commands). No key-space access. No cross-session channel access. Credentials rotate per session.
- **Idempotent cleanup** — ACL deletion is idempotent and runs in `stop_session()`, `run_reaper()`, and `create_session()` error paths.
- **Persist-then-forward** — every event from pub/sub is written to `agent_messages` by a dedicated persistence subscriber before being forwarded to SSE clients. Events are never lost, even if no browser is connected. SSE subscribers are read-only.
- **Deterministic routing** — the `uses_pubsub` boolean column in `agent_sessions` determines message routing. No heuristics or try-and-fallback.

---

## PR 1: Valkey ACL Session Scoping

Create per-session Valkey ACL users so each agent pod can only pub/sub on its own session channels. This is the security foundation for all pub/sub communication.

- [ ] Types & errors defined
- [ ] Migration applied
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### No migration needed

ACL users are ephemeral Valkey state, not persisted in Postgres.

### Code Changes

| File | Change |
|---|---|
| `src/agent/valkey_acl.rs` | **New** — ACL user lifecycle: create, delete, password generation |
| `src/agent/mod.rs` | Add `pub mod valkey_acl;` |
| `src/config.rs` | Add `valkey_agent_host: String` field (default derived from `valkey_url`) |

### Module: `src/agent/valkey_acl.rs`

```rust
use uuid::Uuid;

/// Credentials for a per-session Valkey ACL user.
///
/// Custom `Debug` impl redacts `password` and `url` (which contains the password)
/// to prevent accidental logging via `#[tracing::instrument]` or `dbg!()`.
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
///
/// The user can ONLY subscribe/publish on channels matching
/// `session:{session_id}:*`. Uses explicit commands (not `+@pubsub` category)
/// to exclude diagnostic commands like `PUBSUB CHANNELS`.
///
/// ACL rule: `resetkeys resetchannels -@all &session:{id}:* +subscribe +publish +unsubscribe +ping`
#[tracing::instrument(skip(valkey), fields(%session_id), err)]
pub async fn create_session_acl(
    valkey: &fred::clients::Pool,
    session_id: Uuid,
    valkey_agent_host: &str,
) -> Result<SessionValkeyCredentials, AgentError>

/// Delete a per-session Valkey ACL user. Idempotent — succeeds even if user doesn't exist.
///
/// Uses `ACL DELUSER session-{id}`
#[tracing::instrument(skip(valkey), fields(%session_id), err)]
pub async fn delete_session_acl(
    valkey: &fred::clients::Pool,
    session_id: Uuid,
) -> Result<(), AgentError>

/// Generate a cryptographically random password (32 bytes, hex-encoded = 64 chars).
fn generate_password() -> String
```

**Password generation**: Use `rand::fill(&mut [u8; 32])` then `hex::encode()`. The `hex` crate is already a dependency (used in `src/auth/token.rs`, `src/api/webhooks.rs`, etc.). Per CLAUDE.md gotcha, use `rand::fill()` free function (rand 0.10 API).

**ACL command**: Uses `fred`'s `CustomCommand` API (same pattern as `invalidate_pattern()` in `src/store/valkey.rs:53`). Uses explicit commands instead of `+@pubsub` category to prevent access to `PUBSUB CHANNELS` diagnostic command:

```rust
use fred::interfaces::ClientLike;

let result: String = valkey
    .custom(
        fred::types::CustomCommand::new_static("ACL", None, false),
        vec![
            "SETUSER".to_owned(),
            username.clone(),
            "on".to_owned(),
            format!(">{password}"),
            "resetkeys".to_owned(),
            "resetchannels".to_owned(),
            "-@all".to_owned(),
            format!("&session:{session_id}:*"),
            "+subscribe".to_owned(),
            "+publish".to_owned(),
            "+unsubscribe".to_owned(),
            "+ping".to_owned(),
        ],
    )
    .await
    .map_err(|e| AgentError::Other(anyhow::anyhow!("ACL SETUSER failed: {e}")))?;
```

**Note**: All args are owned `String` values, matching the existing pattern in `src/store/valkey.rs:52-54`. The `resetkeys` + `resetchannels` + `-@all` directives ensure zero baseline access (no keys, no channels, no commands) before granting only the explicit commands needed. The `+ping` is required for fred's connection health checks — without it, keepalive pings are rejected by Valkey.

### Error handling

Use `AgentError::Other(anyhow::anyhow!("..."))` for ACL failures. This is consistent with how the codebase handles non-user-facing infrastructure errors — they all map to `ApiError::Internal` regardless.

### Config change: `src/config.rs`

Add to `Config` struct:

```rust
/// Valkey host:port as seen from inside agent pods.
/// Defaults to host:port parsed from VALKEY_URL.
/// Override when platform connects via port-forward but agents use K8s DNS.
/// Example: "valkey.platform.svc.cluster.local:6379"
pub valkey_agent_host: String,
```

Parse from `PLATFORM_VALKEY_AGENT_HOST` env var. Default: extract host:port from existing `valkey_url` using `url::Url::parse()` (`url` crate v2 is a direct dependency in `Cargo.toml`). Fallback if URL parse fails: `"localhost:6379"`. Also add to `Config::test_default()`.

### Test Strategy — PR 1

#### Tests to write FIRST

**Unit tests — `src/agent/valkey_acl.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_acl_username_format` | Username returns `"session-{session_id}"` with full UUID | Unit |
| `test_generate_acl_password_length` | Password is 64 hex chars (32 bytes) | Unit |
| `test_generate_acl_password_unique` | Two calls produce different passwords | Unit |
| `test_generate_acl_password_hex_only` | Password contains only `[0-9a-f]` characters | Unit |
| `test_build_acl_setuser_commands` | Correct ACL SETUSER args: `on >{pass} resetkeys resetchannels -@all &session:{id}:* +subscribe +publish +unsubscribe +ping` | Unit |
| `test_build_acl_setuser_no_psubscribe` | Command does NOT include `+psubscribe` or `+@pubsub` | Unit |
| `test_build_acl_setuser_includes_ping` | Command includes `+ping` for connection health checks | Unit |
| `test_channel_pattern_events` | `events_channel(id)` returns `session:{uuid}:events` | Unit |
| `test_channel_pattern_input` | `input_channel(id)` returns `session:{uuid}:input` | Unit |
| `test_build_acl_deluser_command` | Correct `ACL DELUSER session-{id}` args | Unit |
| `test_build_valkey_url_with_credentials` | Constructs `redis://session-{id}:{pass}@{host}:{port}` | Unit |
| `test_build_valkey_url_preserves_host_port` | Host and port from config preserved | Unit |

**Unit tests — `src/config.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_default_valkey_agent_host` | `Config::test_default()` has `"localhost:6379"` | Unit |
| `test_valkey_agent_host_from_env` | Env var override works | Unit |
| `test_valkey_agent_host_derived_from_url` | Extracted from `VALKEY_URL` when env var not set | Unit |

**Integration tests — `tests/valkey_acl_integration.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_create_and_delete_acl_roundtrip` | Create ACL user, verify exists, delete, verify gone | Integration |
| `test_acl_scoped_user_can_publish_own_channel` | Scoped user can PUBLISH to `session:{id}:events` | Integration |
| `test_acl_scoped_user_can_subscribe_own_channel` | Scoped user can SUBSCRIBE to `session:{id}:input` | Integration |
| `test_acl_scoped_user_cannot_access_other_session` | Scoped user cannot publish/subscribe to `session:{other_id}:*` | Integration |
| `test_acl_scoped_user_cannot_get_set_keys` | Scoped user cannot GET/SET arbitrary keys (resetkeys effective) | Integration |
| `test_acl_scoped_user_can_ping` | Scoped user can PING (for connection keepalive) | Integration |
| `test_acl_delete_nonexistent_user_ok` | Idempotent deletion of non-existent user | Integration |
| `test_acl_credentials_returned_in_result` | Return value contains username, password, and well-formed URL | Integration |

#### Existing tests to UPDATE

| Test file | Change | Reason |
|---|---|---|
| `src/config.rs` tests | Add `valkey_agent_host` to `test_default()` assertions | New config field |

#### Branch coverage checklist

| Branch/Path | Test that covers it |
|---|---|
| Username formatting | `test_acl_username_format` |
| Password generation happy path | `test_generate_acl_password_length` |
| Password uniqueness | `test_generate_acl_password_unique` |
| Password hex-only chars | `test_generate_acl_password_hex_only` |
| ACL SETUSER correct permissions | `test_build_acl_setuser_commands` |
| ACL SETUSER no dangerous perms | `test_build_acl_setuser_no_psubscribe` |
| ACL SETUSER includes +ping | `test_build_acl_setuser_includes_ping` |
| ACL SETUSER includes -@all + resetkeys | `test_build_acl_setuser_commands` |
| Channel name events | `test_channel_pattern_events` |
| Channel name input | `test_channel_pattern_input` |
| ACL DELUSER format | `test_build_acl_deluser_command` |
| URL construction | `test_build_valkey_url_with_credentials` |
| Publish isolation | `test_acl_scoped_user_can_publish_own_channel` |
| Subscribe isolation | `test_acl_scoped_user_can_subscribe_own_channel` |
| Cross-session blocked | `test_acl_scoped_user_cannot_access_other_session` |
| Non-pubsub blocked | `test_acl_scoped_user_cannot_get_set_keys` |
| Idempotent delete | `test_acl_delete_nonexistent_user_ok` |

#### Tests NOT needed

| Area | Justification |
|---|---|
| Valkey ACL persistence across restart | ACL users are ephemeral; persistence is Valkey config concern |
| Concurrent ACL creation | UUID-based usernames never collide; Valkey handles atomically |

**Total: 17 unit + 8 integration = 25 tests**

### Verification
- `just test-unit` passes with new unit tests
- Integration test creates ACL user, verifies pub/sub isolation, cleans up
- Config test verifies env var parsing and default derivation

---

## PR 2: Agent-Runner MCP Config + Exit Code

Enhance the `cli/agent-runner/` crate with MCP server configuration for platform API integration and proper exit code reporting.

**Note:** The `--prompt` / `-p` flag and single-shot-like behavior are **already implemented in Plan 38**. The agent-runner sends the prompt, streams responses via the REPL loop, and exits naturally when stdin closes (K8s pod stdin pipe EOF). No separate `run_single_shot()` function is needed.

- [ ] Types & errors defined
- [ ] Migration applied
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### No migration needed

### Code Changes

| File | Change |
|---|---|
| `cli/agent-runner/src/mcp.rs` | **New** — MCP config file generation |
| `cli/agent-runner/src/main.rs` | Wire MCP config into `CliSpawnOptions`, add exit code based on Result message |
| `cli/agent-runner/src/repl.rs` | Return `ExitStatus` from `run()` for exit code propagation |

### Pod exit behavior (already working)

In K8s pods, stdin is a pipe that gets EOF when the container starts. The existing REPL loop handles this:
1. Sends initial prompt via `transport.send_message()`
2. Streams all responses until `Result` message
3. Outer loop reads stdin → `None` (EOF) → breaks
4. Agent-runner exits

This is functionally equivalent to single-shot mode without needing a separate code path. The main addition needed is propagating the exit code (0 for success, 1 for error result) from the `Result` message.

### MCP config generation: `src/mcp.rs`

When `PLATFORM_API_TOKEN` and `PLATFORM_API_URL` are set, generate a temporary `mcp_config.json` file and pass it to Claude CLI via `--mcp-config`.

The MCP config enables Claude to call platform APIs (issues, pipelines, deployments, observability) with the agent's scoped token.

```rust
// cli/agent-runner/src/mcp.rs

use std::path::Path;
use serde_json::json;

/// Generate an MCP configuration file for platform server integration.
/// Returns the path to the generated file (inside the config temp dir).
pub fn generate_mcp_config(
    config_dir: &Path,
    platform_url: &str,
    platform_token: &str,
) -> anyhow::Result<std::path::PathBuf>
```

The generated `mcp_config.json` references 5 MCP servers (admin excluded):

```json
{
  "mcpServers": {
    "platform-core": {
      "command": "node",
      "args": ["/opt/mcp/servers/platform-core.js"],
      "env": {
        "PLATFORM_API_URL": "{platform_url}",
        "PLATFORM_API_TOKEN": "{platform_token}"
      }
    },
    "platform-issues": {
      "command": "node",
      "args": ["/opt/mcp/servers/platform-issues.js"],
      "env": { "..." }
    },
    "platform-pipeline": {
      "command": "node",
      "args": ["/opt/mcp/servers/platform-pipeline.js"],
      "env": { "..." }
    },
    "platform-deploy": {
      "command": "node",
      "args": ["/opt/mcp/servers/platform-deploy.js"],
      "env": { "..." }
    },
    "platform-observe": {
      "command": "node",
      "args": ["/opt/mcp/servers/platform-observe.js"],
      "env": { "..." }
    }
  }
}
```

**Note**: MCP server JS files are at `/opt/mcp/servers/` in the container (matching the `COPY mcp/ /opt/mcp/` Dockerfile directive). Each server imports `../lib/client.js`, so the full `mcp/` directory structure must be preserved. The `platform-admin` server is intentionally excluded — agents should not have admin access.

### CLI flag wiring in `main.rs`

```rust
#[derive(Parser)]
struct Cli {
    // ... existing flags (including -p/--prompt from Plan 38) ...

    /// Disable MCP server integration even when PLATFORM_API_TOKEN is set.
    #[arg(long)]
    no_mcp: bool,
}
```

Flow update in `main()`:
1. If `--platform-token` and `--platform-url` are set and `--no-mcp` is not:
   - Generate MCP config file in temp dir
   - Set `opts.mcp_config = Some(mcp_config_path)`
2. Call `repl::run(transport, pubsub, initial_prompt)` (existing REPL — already handles single-shot via stdin EOF in pod mode)

### Test Strategy — PR 2

Tests run via `cargo test -p agent-runner`. Standalone crate, independent of platform test infra.

#### Tests to write FIRST

**Unit tests — `cli/agent-runner/src/mcp.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_generate_mcp_config_valid_json` | Produces valid JSON with all 5 servers | Unit |
| `test_generate_mcp_config_correct_paths` | Server paths are `/opt/mcp/servers/*.js` | Unit |
| `test_generate_mcp_config_excludes_admin` | No `platform-admin` server in config | Unit |
| `test_generate_mcp_config_injects_env_vars` | `PLATFORM_API_URL` and `PLATFORM_API_TOKEN` set per server | Unit |
| `test_generate_mcp_config_file_written` | File exists at expected path in config dir | Unit |

**Unit tests — `cli/agent-runner/src/main.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_no_mcp_flag_parsed` | `--no-mcp` prevents MCP config | Unit |
| `test_mcp_config_requires_platform_vars` | MCP config not generated without platform vars | Unit |

**Note:** `--prompt` flag tests already exist from Plan 38 implementation. Single-shot exit behavior tested implicitly — agent-runner's REPL exits on stdin EOF (K8s pod natural behavior).

**Total: 7 unit tests (new for this PR)**

### Verification
- `cargo test -p agent-runner` passes
- Manual test: `cargo run -p agent-runner -- -p "say hello" --cwd /tmp`
- Manual test: verify MCP config file contents

---

## PR 3: Pod Startup → Agent-Runner + `uses_pubsub` Flag

Modify the platform's pod builder to launch `agent-runner` instead of Claude CLI directly. Inject Valkey ACL credentials into the pod environment. Wire ACL lifecycle into session create/stop/reaper. Add `uses_pubsub` column to `agent_sessions` for deterministic message routing.

- [ ] Types & errors defined
- [ ] Migration applied
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### Migration: `20260303010001_add_uses_pubsub`

**Up:**
```sql
ALTER TABLE agent_sessions ADD COLUMN uses_pubsub BOOLEAN NOT NULL DEFAULT false;
```

**Down:**
```sql
ALTER TABLE agent_sessions DROP COLUMN uses_pubsub;
```

This is a metadata-only change on Postgres 11+ (non-volatile default). No table rewrite, no locking. Existing rows get `false` (correct — legacy sessions don't use pub/sub).

**Rationale for placing in PR 3 (not PR 4)**: PR 3 modifies `create_session()` to create Valkey ACL users and must set `uses_pubsub = true` at that point. The flag must exist before PR 3 can write to it. PR 4 then reads the flag for message routing.

### Code Changes

| File | Change |
|---|---|
| `src/agent/claude_code/pod.rs` | Rename `build_claude_args()` → `build_agent_runner_args()`, update `build_main_container()` command |
| `src/agent/claude_code/pod.rs` | Add `VALKEY_URL` to `build_env_vars()` and `RESERVED_ENV_VARS` |
| `src/agent/claude_code/pod.rs` | Add `valkey_url: Option<&'a str>` to `PodBuildParams` |
| `src/agent/provider.rs` | Add `valkey_url: Option<&'a str>` to `BuildPodParams`, add `uses_pubsub: bool` to `AgentSession`, add `Deserialize` to `ProgressEvent` |
| `src/agent/service.rs` | Call `valkey_acl::create_session_acl()` in `create_session()` with error-path cleanup |
| `src/agent/service.rs` | Call `valkey_acl::delete_session_acl()` in `stop_session()` and `run_reaper()` |
| `src/agent/service.rs` | Update `fetch_session()` SELECT to include `uses_pubsub` |
| `src/agent/service.rs` | Set `uses_pubsub = true` in session INSERT/UPDATE |
| `tests/e2e_agent.rs` | Update pod spec assertions |

### Critical type changes

**Both `PodBuildParams` (pod.rs) AND `BuildPodParams` (provider.rs) need `valkey_url`**. These are separate structs with the same fields — one at the provider trait boundary, one at the pod builder implementation. Both must be updated.

**`ProgressEvent` needs `Deserialize`** added to its derive macro. Currently `src/agent/provider.rs:116` only has `#[derive(Debug, Clone, Serialize)]`. The pub/sub bridge (PR 4) must deserialize incoming JSON into `ProgressEvent`. Add `Deserialize`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressEvent {
    pub kind: ProgressKind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}
```

**`ProgressKind` needs `Unknown` variant** for forward compatibility. Without it, events from agent-runner with kinds not yet in the platform's enum (e.g., `SecretRequest` from UI types, or future kinds) cause deserialization failures and silent event loss. Add `#[serde(other)]`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProgressKind {
    Thinking,
    ToolCall,
    ToolResult,
    Milestone,
    Error,
    Completed,
    Text,
    #[serde(other)]
    Unknown,
}
```

This ensures any unknown event kind deserializes to `ProgressKind::Unknown` instead of returning an error. The SSE bridge forwards all events including `Unknown` — the UI TypeScript can handle unknown kinds gracefully.

**`AgentSession` needs `uses_pubsub: bool`**. The struct at `src/agent/provider.rs:52-72` is manually mapped from SQL in `fetch_session()`. Add the field and update:
- `fetch_session()` SELECT column list (`src/agent/service.rs:540`)
- `session_for_pod` construction in `create_session()` (`src/agent/service.rs:151-169`)

### Pod spec changes: `src/agent/claude_code/pod.rs`

**New `RESERVED_ENV_VARS` entry:**
```rust
const RESERVED_ENV_VARS: &[&str] = &[
    // ... existing ...
    "VALKEY_URL",
];
```

**New env var in `build_env_vars()`:**
```rust
if let Some(valkey_url) = params.valkey_url {
    vars.push(env_var("VALKEY_URL", valkey_url));
}
```

**Replace `build_claude_args()` with `build_agent_runner_args()`:**

```rust
fn build_agent_runner_args(params: &PodBuildParams<'_>) -> Vec<String> {
    let mut args = vec![
        "--prompt".to_owned(),
        params.session.prompt.clone(),
        "--cwd".to_owned(),
        "/workspace".to_owned(),
        "--permission-mode".to_owned(),
        "bypassPermissions".to_owned(),
    ];
    if let Some(ref model) = params.config.model {
        args.push("--model".to_owned());
        args.push(model.clone());
    }
    if let Some(max_turns) = params.config.max_turns {
        args.push("--max-turns".to_owned());
        args.push(max_turns.to_string());
    }
    args
}
```

**Container entrypoint change** in `build_main_container()`:
- `command: Some(vec!["agent-runner".to_owned()])` — explicit entrypoint
- `args: Some(agent_runner_args)` — agent-runner flags

### Service changes: `src/agent/service.rs`

**In `create_session()` — ACL creation with error-path cleanup:**

```rust
// Create scoped Valkey ACL user for pub/sub isolation
let valkey_creds = valkey_acl::create_session_acl(
    &state.valkey,
    session_id,
    &state.config.valkey_agent_host,
).await?;

// Build and create pod...
let pod = provider.build_pod(BuildPodParams {
    // ... existing fields ...
    valkey_url: Some(&valkey_creds.url),
})?;

match pods.create(&PostParams::default(), &pod).await {
    Ok(_) => { /* continue */ },
    Err(e) => {
        // Cleanup ACL on pod creation failure
        let _ = valkey_acl::delete_session_acl(&state.valkey, session_id).await;
        return Err(AgentError::PodCreationFailed(e.to_string()));
    }
}

// Update session: set uses_pubsub = true
sqlx::query!("UPDATE agent_sessions SET status = 'running', pod_name = $2, uses_pubsub = true WHERE id = $1",
    session_id, &pod_name).execute(&state.pool).await?;
```

**In `stop_session()` and `run_reaper()`** — ACL cleanup (idempotent):

```rust
if let Err(e) = valkey_acl::delete_session_acl(&state.valkey, session_id).await {
    tracing::warn!(error = %e, %session_id, "failed to delete Valkey ACL user");
}
```

### `.sqlx/` regeneration

After migration + query changes: `just db-migrate && just db-prepare`. Commit `.sqlx/` changes.

### Test Strategy — PR 3

#### Tests to write FIRST

**Unit tests — `src/agent/claude_code/pod.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_pod_command_launches_agent_runner` | Main container command is `["agent-runner"]` | Unit |
| `test_pod_agent_runner_args_include_prompt` | `--prompt` flag with session prompt | Unit |
| `test_pod_agent_runner_args_include_cwd` | `--cwd /workspace` | Unit |
| `test_pod_agent_runner_args_include_permission_mode` | `--permission-mode bypassPermissions` | Unit |
| `test_pod_agent_runner_args_include_model` | `--model` when config has model | Unit |
| `test_pod_agent_runner_args_include_max_turns` | `--max-turns` when config has max_turns | Unit |
| `test_pod_env_includes_valkey_url` | `VALKEY_URL` env var present when provided | Unit |
| `test_pod_env_no_valkey_url_when_none` | `VALKEY_URL` absent when not provided | Unit |
| `test_valkey_url_is_reserved_env_var` | `VALKEY_URL` in RESERVED_ENV_VARS list | Unit |
| `test_pod_env_session_id_still_set` | `SESSION_ID` env var still correct | Unit |
| `test_pod_agent_runner_args_no_model_when_none` | No `--model` in args when config.model is None | Unit |
| `test_pod_agent_runner_args_no_max_turns_when_none` | No `--max-turns` in args when config.max_turns is None | Unit |
| `test_pod_env_project_id_still_set` | `PROJECT_ID` env var still correct | Unit |

**Unit tests — `src/agent/provider.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_progress_event_deserialize_text` | `ProgressEvent` with kind=text round-trips via serde | Unit |
| `test_progress_event_deserialize_thinking` | `ProgressEvent` with kind=thinking round-trips | Unit |
| `test_progress_event_deserialize_tool_call` | `ProgressEvent` with kind=tool_call round-trips | Unit |
| `test_progress_event_deserialize_completed` | `ProgressEvent` with kind=completed + metadata round-trips | Unit |
| `test_progress_event_deserialize_no_metadata` | `ProgressEvent` without metadata field deserializes correctly | Unit |
| `test_progress_kind_unknown_variant` | Unknown kind string deserializes to `ProgressKind::Unknown` | Unit |

**Integration tests — `tests/session_integration.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_create_session_creates_valkey_acl` | ACL user exists after `create_session()` | Integration |
| `test_stop_session_deletes_valkey_acl` | ACL user deleted after `stop_session()` | Integration |
| `test_create_session_cleans_acl_on_pod_failure` | ACL deleted when pod creation fails | Integration |
| `test_session_uses_pubsub_column_exists` | Inserted session row has `uses_pubsub` column accessible | Integration |
| `test_session_uses_pubsub_default_false` | Sessions inserted without explicit flag default to `false` | Integration |

**E2E tests — `tests/e2e_agent.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_agent_pod_launches_agent_runner` | Pod container command is `agent-runner`, not `claude` | E2E |
| `test_agent_pod_has_valkey_url_env` | Pod environment includes `VALKEY_URL` | E2E |

#### Existing tests to UPDATE

| Test file | Change | Reason |
|---|---|---|
| `src/agent/claude_code/pod.rs` (~40 tests) | Add `valkey_url: None` to all `PodBuildParams` constructions | New field on struct |
| `src/agent/provider.rs` | `ProgressEvent` gains `Deserialize` — no test breakage | Additive derive |
| `tests/e2e_agent.rs` | Update pod spec assertions for agent-runner command/args | Entrypoint changed |

**Critical**: Every existing pod.rs unit test constructs `PodBuildParams` directly. All ~40 tests need `valkey_url: None` added. This is mechanical but high-volume.

#### Branch coverage checklist

| Branch/Path | Test that covers it |
|---|---|
| Agent-runner command in container | `test_pod_command_launches_agent_runner` |
| `--prompt` arg | `test_pod_agent_runner_args_include_prompt` |
| `--model` when set | `test_pod_agent_runner_args_include_model` |
| `--max-turns` when set | `test_pod_agent_runner_args_include_max_turns` |
| VALKEY_URL present | `test_pod_env_includes_valkey_url` |
| VALKEY_URL absent | `test_pod_env_no_valkey_url_when_none` |
| VALKEY_URL reserved | `test_valkey_url_is_reserved_env_var` |
| `--model` absent when None | `test_pod_agent_runner_args_no_model_when_none` |
| `--max-turns` absent when None | `test_pod_agent_runner_args_no_max_turns_when_none` |
| ProgressEvent deserializes (text, thinking, tool_call, completed) | `test_progress_event_deserialize_*` |
| ProgressKind Unknown fallback | `test_progress_kind_unknown_variant` |
| `uses_pubsub` column defaults false | `test_session_uses_pubsub_default_false` |
| ACL created on session create | `test_create_session_creates_valkey_acl` |
| ACL deleted on session stop | `test_stop_session_deletes_valkey_acl` |
| ACL cleaned on pod failure | `test_create_session_cleans_acl_on_pod_failure` |

**Total: 19 unit + 5 integration + 2 E2E + ~40 existing test updates = 26 new tests**

### Verification
- `just test-unit` passes with updated pod builder tests
- `just test-integration` passes with ACL lifecycle tests
- `just test-e2e` passes with updated pod spec expectations
- `just db-prepare` regenerates `.sqlx/` cleanly
- Manual: `kubectl describe pod agent-{id}` shows agent-runner entrypoint + VALKEY_URL env

---

## PR 4: Pub/Sub Event Bridge to SSE

Replace pod log tailing with Valkey pub/sub streaming for agent-runner sessions. The platform subscribes to `session:{id}:events` and bridges events to **SSE (Server-Sent Events)** endpoints. Outgoing messages (user → agent) are published to `session:{id}:input` via existing REST POST endpoints instead of writing to pod stdin.

**Key design change (2026-03-03):** WebSocket replaced with SSE. SSE is unidirectional (server→client), simpler than WebSocket, and has built-in browser reconnection via `EventSource`. The client→server direction (sending messages) already has REST endpoints (`POST /api/projects/{id}/sessions/{session_id}/message` and `POST /api/sessions/{session_id}/message`). This removes the `axum` `"ws"` feature dependency and ~430 LOC of WebSocket infrastructure.

- [ ] Types & errors defined
- [ ] Migration applied
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### No migration needed

The `uses_pubsub` column was added in PR 3.

### Code Changes

| File | Change |
|---|---|
| `src/agent/pubsub_bridge.rs` | **New** — `spawn_persistence_subscriber()` (persist events to `agent_messages`), `subscribe_session_events()` (read-only SSE forwarding), `publish_prompt()` / `publish_control()` (input channel) |
| `src/agent/mod.rs` | Add `pub mod pubsub_bridge;` |
| `src/agent/pubsub_bridge.rs` | **Update** — Add `publish_event()` for server-side event publishing (used by Plan 40 create-app flow) |
| `src/agent/service.rs` | **Update** `create_session()` — after successful pod creation for `uses_pubsub = true` sessions, call `pubsub_bridge::spawn_persistence_subscriber()` to start background event persistence |
| `src/api/sessions.rs` | **Delete** `ws_handler()`, `handle_ws()`, `stream_broadcast_to_ws()`, `stream_pod_logs_to_ws()`, `ws_handler_global()`, `handle_ws_global()` (~265 LOC) |
| `src/api/sessions.rs` | **New** `sse_session_events()` — project-scoped SSE endpoint (`GET .../sessions/{session_id}/events`) |
| `src/api/sessions.rs` | **New** `sse_session_events_global()` — global SSE endpoint (`GET /api/sessions/{session_id}/events`) |
| `src/api/sessions.rs` | **New** `subscribe_session_events()` — Valkey pub/sub → mpsc bridge (same pattern as `src/store/eventbus.rs`) |
| `src/api/sessions.rs` | **Update** routes: replace `/ws` with `/events` |
| `src/api/sessions.rs` | **Remove** `axum::extract::ws` and `WebSocketUpgrade` imports |
| `src/observe/query.rs` | **Rewrite** `live_tail_ws()` → `live_tail_sse()`: SSE endpoint for observe log tail |
| `Cargo.toml` | **Remove** `"ws"` from axum features, **add** `tokio-stream` dependency |
| `ui/src/lib/ws.ts` | **Delete entirely** (~86 LOC) |
| `ui/src/lib/sse.ts` | **New** `EventSourceClient` wrapper (~40 LOC) with built-in reconnection via `EventSource` |
| `ui/src/pages/SessionDetail.tsx` | **Update** `createWs` → `createSse`, URL `/ws` → `/events` |
| `ui/src/pages/CreateApp.tsx` | **Update** `createWs` → `createSse`, URL `/ws` → `/events`, `ws.send()` → `api.post()` |
| `ui/src/components/OnboardingOverlay.tsx` | **Update** same as CreateApp |
| `ui/src/pages/observe/Logs.tsx` | **Update** `createWs` → `createSse` for live tail |
| `src/agent/service.rs` | Update `send_message()` to route via pub/sub when `uses_pubsub = true` |
| `src/agent/service.rs` | **Delete** `get_log_lines()` function (~30 LOC) — zero callers after WebSocket removal |
| `src/agent/service.rs` | Extract `finalize_reaped_session()` helper from `reap_terminated_sessions()` to stay under 100-line clippy limit after ACL cleanup additions |

### Module: `src/agent/pubsub_bridge.rs`

```rust
use crate::agent::provider::{ProgressEvent, ProgressKind};
use fred::interfaces::PubsubInterface;
use sqlx::PgPool;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Spawn a background persistence subscriber that writes every pub/sub event
/// to `agent_messages`. Started once per `uses_pubsub = true` session at creation
/// time. Exits on Completed/Error event or channel closure.
#[tracing::instrument(skip(pool, valkey), fields(%session_id))]
pub fn spawn_persistence_subscriber(
    pool: PgPool,
    valkey: &fred::clients::Pool,
    session_id: Uuid,
) -> tokio::task::JoinHandle<()>

/// Subscribe to a session's event channel and return a receiver of ProgressEvents.
/// Read-only — does NOT persist (persistence handled by spawn_persistence_subscriber).
///
/// Spawns a background task that:
/// 1. Creates a dedicated Valkey subscriber client (`pool.next().clone_new()`)
/// 2. Subscribes to `session:{session_id}:events`
/// 3. Parses JSON messages into ProgressEvent (requires Deserialize — added in PR 3)
/// 4. Forwards via mpsc channel
///
/// **Cleanup**: When the mpsc receiver drops (SSE client disconnect → axum drops Sse response
/// → drops ReceiverStream → drops mpsc::Receiver), the background task detects `tx.send()`
/// failure, unsubscribes from the channel, and exits. No manual cleanup needed.
#[tracing::instrument(skip(valkey), fields(%session_id), err)]
pub async fn subscribe_session_events(
    valkey: &fred::clients::Pool,
    session_id: Uuid,
) -> anyhow::Result<mpsc::Receiver<ProgressEvent>>

/// Publish a prompt message to a session's input channel.
///
/// Message format: `{"type": "prompt", "content": "..."}`
#[tracing::instrument(skip(valkey, content), fields(%session_id), err)]
pub async fn publish_prompt(
    valkey: &fred::clients::Pool,
    session_id: Uuid,
    content: &str,
) -> anyhow::Result<()>

/// Publish a control message to a session's input channel.
///
/// Message format: `{"type": "control", "control": {"type": "interrupt"}}`
#[tracing::instrument(skip(valkey), fields(%session_id), err)]
pub async fn publish_control(
    valkey: &fred::clients::Pool,
    session_id: Uuid,
    control: &serde_json::Value,
) -> anyhow::Result<()>

fn events_channel(session_id: Uuid) -> String {
    format!("session:{session_id}:events")
}

fn input_channel(session_id: Uuid) -> String {
    format!("session:{session_id}:input")
}
```

The subscriber background task must handle cleanup:
```rust
tokio::spawn(async move {
    loop {
        match message_rx.recv().await {
            Ok(message) => {
                let payload: String = match message.value.convert() {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                match serde_json::from_str::<ProgressEvent>(&payload) {
                    Ok(event) => {
                        if tx.send(event).await.is_err() {
                            // Receiver dropped (SSE client disconnected) — clean up
                            let _ = subscriber.unsubscribe(channel).await;
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "invalid event JSON on pub/sub, skipping");
                    }
                }
            }
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
});
```

### SSE endpoint design: `src/api/sessions.rs`

**Delete all WebSocket handlers** (`ws_handler`, `handle_ws`, `stream_broadcast_to_ws`, `stream_pod_logs_to_ws`, `ws_handler_global`, `handle_ws_global`) and replace with SSE endpoints. All session types (create-app, agent-runner pods, future modes) publish events to `session:{id}:events` via Valkey pub/sub. The SSE endpoint subscribes to pub/sub and streams events to the browser.

**Routes:**

| Old (WebSocket) | New (SSE) |
|---|---|
| `GET .../sessions/{session_id}/ws` (WS upgrade) | `GET .../sessions/{session_id}/events` (SSE) |
| `GET /api/sessions/{session_id}/ws` (WS upgrade) | `GET /api/sessions/{session_id}/events` (SSE) |
| `GET /api/observe/logs/tail` (WS upgrade) | `GET /api/observe/logs/tail` (SSE, same URL) |

Client→server messages: already covered by existing REST endpoints:
- `POST /api/projects/{id}/sessions/{session_id}/message`
- `POST /api/sessions/{session_id}/message`

**Project-scoped SSE handler:**

```rust
use axum::response::sse::{Event, KeepAlive, Sse};
use tokio_stream::wrappers::ReceiverStream;
use futures_util::stream::StreamExt;
use std::convert::Infallible;

async fn sse_session_events(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, session_id)): Path<(Uuid, Uuid)>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    require_project_read(&state, &auth, id).await?;

    let session = service::fetch_session(&state.pool, session_id)
        .await
        .map_err(ApiError::from)?;
    if session.project_id != Some(id) {
        return Err(ApiError::NotFound("session".into()));
    }

    let rx = subscribe_session_events(&state.valkey, session_id).await
        .map_err(ApiError::Internal)?;

    let stream = ReceiverStream::new(rx).map(|event| {
        Ok(Event::default()
            .event("progress")
            .data(serde_json::to_string(&event).unwrap_or_default()))
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
```

**Global SSE handler** — same pattern, owner-only auth check.

**Persist-then-forward subscriber** — subscribes to Valkey pub/sub, writes each event to `agent_messages` for durable persistence, then forwards to SSE. This replaces the persistence that `stream_pod_logs_to_ws()` did inline — no event is lost even if the SSE client disconnects:

```rust
async fn subscribe_session_events(
    pool: &PgPool,
    valkey: &fred::clients::Pool,
    session_id: Uuid,
) -> anyhow::Result<mpsc::Receiver<ProgressEvent>> {
    let (tx, rx) = mpsc::channel(256);
    let subscriber = valkey.next().clone_new();
    subscriber.init().await?;
    let channel = format!("session:{session_id}:events");
    subscriber.subscribe(&channel).await?;
    let mut message_rx = subscriber.message_rx();
    let pool = pool.clone();

    tokio::spawn(async move {
        loop {
            match message_rx.recv().await {
                Ok(msg) => {
                    let Ok(text): Result<String, _> = msg.value.convert() else { continue };
                    let Ok(event): Result<ProgressEvent, _> = serde_json::from_str(&text) else {
                        tracing::warn!("invalid event JSON on pub/sub, skipping");
                        continue;
                    };

                    // 1. Persist to DB FIRST — durable record regardless of SSE state
                    let _ = sqlx::query(
                        "INSERT INTO agent_messages (session_id, role, content, metadata) VALUES ($1, 'assistant', $2, $3)"
                    )
                    .bind(session_id)
                    .bind(&event.message)
                    .bind(&event.metadata)
                    .execute(&pool)
                    .await;

                    // 2. Forward to SSE channel — if client disconnected, clean up
                    if tx.send(event).await.is_err() {
                        let _ = subscriber.unsubscribe(&channel).await;
                        break;
                    }
                }
                Err(_) => {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
    });

    Ok(rx)
}
```

**Persist-then-forward guarantees:**
- Every event is written to `agent_messages` before being forwarded to the SSE stream
- If the SSE client disconnects, persistence continues until the background task detects `tx.send()` failure — at most one extra event persisted after disconnect (acceptable)
- If the DB INSERT fails (transient), the event is still forwarded to SSE (best-effort persistence, no data loss for live viewers)
- On SSE reconnect, the frontend can fetch missed events from `GET /api/.../sessions/{id}` which reads from `agent_messages`

**Lifecycle:** SSE disconnect → axum drops `Sse` response → drops `ReceiverStream` → drops `mpsc::Receiver` → background task's `tx.send()` fails → task unsubscribes and exits. No manual cleanup needed.

**What if no SSE client connects?** Events are still persisted. The `subscribe_session_events()` subscriber is started when the SSE endpoint is hit. For sessions where no one watches live, the bridge is never created, and events flow through pub/sub but are NOT persisted. To handle this, `create_session()` must also start a background persistence subscriber for every `uses_pubsub = true` session (not just when SSE connects). This **session persistence subscriber** runs for the session's lifetime and only persists — it has no SSE channel. See "Session persistence subscriber" below.

**Session persistence subscriber** — started in `create_session()` for every `uses_pubsub = true` session:

```rust
/// Spawns a background task that subscribes to a session's pub/sub events
/// and persists them to agent_messages. Runs independently of SSE connections.
/// Exits when the session is stopped (channel closed) or pod terminates.
#[tracing::instrument(skip(pool, valkey), fields(%session_id))]
pub fn spawn_persistence_subscriber(
    pool: PgPool,
    valkey: &fred::clients::Pool,
    session_id: Uuid,
) -> tokio::task::JoinHandle<()> {
    let subscriber = valkey.next().clone_new();
    tokio::spawn(async move {
        if let Err(e) = subscriber.init().await {
            tracing::error!(error = %e, "persistence subscriber init failed");
            return;
        }
        let channel = format!("session:{session_id}:events");
        if let Err(e) = subscriber.subscribe(&channel).await {
            tracing::error!(error = %e, "persistence subscriber subscribe failed");
            return;
        }
        let mut message_rx = subscriber.message_rx();

        loop {
            match message_rx.recv().await {
                Ok(msg) => {
                    let Ok(text): Result<String, _> = msg.value.convert() else { continue };
                    let Ok(event): Result<ProgressEvent, _> = serde_json::from_str(&text) else {
                        continue;
                    };
                    let _ = sqlx::query(
                        "INSERT INTO agent_messages (session_id, role, content, metadata) VALUES ($1, 'assistant', $2, $3)"
                    )
                    .bind(session_id)
                    .bind(&event.message)
                    .bind(&event.metadata)
                    .execute(&pool)
                    .await;

                    // Exit on Completed/Error — session is done
                    if matches!(event.kind, ProgressKind::Completed | ProgressKind::Error) {
                        let _ = subscriber.unsubscribe(&channel).await;
                        break;
                    }
                }
                Err(_) => {
                    // Connection lost — retry after delay
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
        tracing::info!(%session_id, "persistence subscriber exited");
    })
}
```

**SSE subscriber is then read-only** — when a client connects via SSE, it subscribes to the same pub/sub channel but does NOT persist (the persistence subscriber already handles that). This means `subscribe_session_events()` simplifies back to the non-persisting version for SSE forwarding only:

```rust
/// Subscribe to a session's event channel for SSE streaming (read-only, no persistence).
/// Persistence is handled by spawn_persistence_subscriber() started at session creation.
pub async fn subscribe_session_events(
    valkey: &fred::clients::Pool,
    session_id: Uuid,
) -> anyhow::Result<mpsc::Receiver<ProgressEvent>> {
    let (tx, rx) = mpsc::channel(256);
    let subscriber = valkey.next().clone_new();
    subscriber.init().await?;
    let channel = format!("session:{session_id}:events");
    subscriber.subscribe(&channel).await?;
    let mut message_rx = subscriber.message_rx();

    tokio::spawn(async move {
        loop {
            match message_rx.recv().await {
                Ok(msg) => {
                    let Ok(text): Result<String, _> = msg.value.convert() else { continue };
                    let Ok(event): Result<ProgressEvent, _> = serde_json::from_str(&text) else {
                        tracing::warn!("invalid event JSON on pub/sub, skipping");
                        continue;
                    };
                    if tx.send(event).await.is_err() {
                        let _ = subscriber.unsubscribe(&channel).await;
                        break;
                    }
                }
                Err(_) => {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
    });

    Ok(rx)
}
```

**Architecture summary:**
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

**Note:** The SSE handler does NOT use `inprocess::subscribe()` or `cli_sessions.subscribe()` — it subscribes via Valkey pub/sub exclusively. The old broadcast-based subscribe paths (`InProcessHandle.tx`, `CliSessionHandle.tx`) become dead code in this PR; Plan 40 removes them when it refactors `CliSessionHandle` and deletes `inprocess.rs`.

### Observe live tail SSE: `src/observe/query.rs`

Replace `live_tail_ws()` with `live_tail_sse()`. Same Valkey pub/sub subscription pattern, same `should_forward()` filter logic, but returns `Sse<impl Stream>` instead of upgrading a WebSocket:

```rust
async fn live_tail_sse(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(params): Query<LiveTailParams>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let project_id = params.project_id
        .ok_or_else(|| ApiError::BadRequest("project_id required for live tail".into()))?;
    require_observe_read(&state, &auth, Some(project_id)).await?;

    let (tx, rx) = mpsc::channel(256);
    let subscriber = state.valkey.next().clone_new();
    subscriber.init().await.map_err(|e| ApiError::Internal(e.into()))?;
    let channel = format!("logs:{project_id}");
    subscriber.subscribe(&channel).await.map_err(|e| ApiError::Internal(e.into()))?;
    let mut message_rx = subscriber.message_rx();

    tokio::spawn(async move {
        loop {
            match message_rx.recv().await {
                Ok(msg) => {
                    let Ok(text): Result<String, _> = msg.value.convert() else { continue };
                    if !should_forward(&text, &params) { continue; }
                    if tx.send(text).await.is_err() {
                        let _ = subscriber.unsubscribe(&channel).await;
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let stream = ReceiverStream::new(rx).map(|text| {
        Ok(Event::default().event("log").data(text))
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
```

### Frontend: `ui/src/lib/sse.ts` (replaces `ws.ts`)

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

**UI page changes (4 files):** Each page replaces `createWs` with `createSse`, changes URL from `/ws` to `/events`, and sends messages via `api.post()` instead of `ws.send()`:

| Page | WS usage | Change |
|---|---|---|
| `SessionDetail.tsx` | Events + send | SSE for events, already uses REST for send |
| `CreateApp.tsx` | Events + send | SSE for events, switch `ws.send()` to `api.post(/api/sessions/{id}/message)` |
| `OnboardingOverlay.tsx` | Events + send | Same as CreateApp |
| `observe/Logs.tsx` | Events only (read-only) | SSE with `event: "log"`, trivial — no send path |

### Cargo.toml changes

```diff
-axum = { version = "0.8", features = ["ws", "macros"] }
+axum = { version = "0.8", features = ["macros"] }

+tokio-stream = { version = "0.1", features = ["sync"] }
```

### Message routing update: `src/agent/service.rs`

Update `send_message()` to use `uses_pubsub` flag for deterministic routing. This PR only adds the pod pub/sub path. Plan 40 later extends this block with a `cli_subprocess` branch that queues messages in `CliSessionHandle.pending_messages`:

```rust
pub async fn send_message(state: &AppState, session_id: Uuid, content: &str) -> Result<(), AgentError> {
    let session = fetch_session(&state.pool, session_id).await?;

    // Pub/sub path (agent-runner pods)
    // Note: Plan 40 will extend this block with a cli_subprocess branch
    // that queues messages in CliSessionHandle.pending_messages.
    if session.uses_pubsub {
        pubsub_bridge::publish_prompt(&state.valkey, session_id, content).await
            .map_err(|e| AgentError::Other(e))?;
        return Ok(());
    }

    // Fallback: existing routing (inprocess, cli_subprocess stdin, pod stdin attach)
    // ... existing match on execution_mode ...
}
```

### Test Strategy — PR 4

#### Tests to write FIRST

**Unit tests — `src/agent/pubsub_bridge.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_events_channel_name` | Format `session:{id}:events` | Unit |
| `test_input_channel_name` | Format `session:{id}:input` | Unit |
| `test_build_prompt_message_json` | `{"type":"prompt","content":"..."}` format | Unit |
| `test_build_control_interrupt_json` | `{"type":"control","control":{"type":"interrupt"}}` format | Unit |
| `test_progress_event_deserialize_text` | Text event from JSON | Unit |
| `test_progress_event_deserialize_thinking` | Thinking event from JSON | Unit |
| `test_progress_event_deserialize_tool_call` | ToolCall event with metadata | Unit |
| `test_progress_event_deserialize_completed` | Completed event with metadata | Unit |
| `test_progress_event_deserialize_error` | Error event from JSON | Unit |
| `test_progress_event_invalid_json_returns_error` | Invalid JSON → Err | Unit |
| `test_progress_event_unknown_kind` | Unknown kind field → `ProgressKind::Unknown` (serde other) | Unit |

**Integration tests — `tests/session_integration.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_persistence_subscriber_writes_to_db` | Publish event to pub/sub → verify row in `agent_messages` with correct session_id, role, content, metadata | Integration |
| `test_persistence_subscriber_exits_on_completed` | Publish Completed event → subscriber exits (JoinHandle resolves) | Integration |
| `test_persistence_subscriber_exits_on_error` | Publish Error event → subscriber exits | Integration |
| `test_persistence_subscriber_skips_malformed` | Malformed JSON on pub/sub → no DB row, no crash, subscriber continues | Integration |
| `test_pubsub_bridge_receives_events` | Publish to events channel, verify mpsc receiver gets ProgressEvent (SSE read-only path) | Integration |
| `test_pubsub_bridge_ignores_malformed_events` | Malformed JSON skipped without crash | Integration |
| `test_send_message_routes_via_pubsub` | `send_message()` with `uses_pubsub=true` publishes to input channel | Integration |
| `test_send_message_falls_back_for_legacy` | `send_message()` with `uses_pubsub=false` uses pod attach | Integration |
| `test_publish_prompt_format` | Published message has correct JSON format | Integration |
| `test_sse_endpoint_streams_pubsub_events` | SSE endpoint receives events via pub/sub path | Integration |
| `test_sse_endpoint_returns_event_stream_content_type` | SSE response has `text/event-stream` content type | Integration |
| `test_sse_global_endpoint_owner_only` | Global SSE endpoint rejects non-owner | Integration |
| `test_sse_endpoint_requires_auth` | SSE endpoint without auth token returns 401 | Integration |
| `test_sse_endpoint_nonexistent_session_404` | SSE endpoint for unknown session returns 404 | Integration |
| `test_pubsub_bridge_multiple_sessions_isolated` | Two bridges for different sessions only receive their own events | Integration |
| `test_pubsub_bridge_receiver_drop_unsubscribes` | Dropping mpsc::Receiver causes background task to exit | Integration |
| `test_send_message_still_works_for_inprocess` | `send_message()` on `inprocess` mode still routes correctly (no regression) | Integration |

**E2E tests — `tests/e2e_agent.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_agent_pubsub_event_streaming` | Create session, publish event from fake agent-side client, receive via SSE endpoint | E2E |

#### Existing tests to UPDATE

| Test file | Change | Reason |
|---|---|---|
| `tests/session_integration.rs` | Tests with `uses_pubsub=false` sessions work unchanged | Default is false |
| `tests/session_integration.rs` | WS-based streaming tests replaced by SSE + pub/sub tests | WS handlers deleted in this PR; `inprocess::subscribe()` paths are dead code (Plan 40 deletes them) |
| `tests/e2e_agent.rs` | Optional: add pub/sub SSE streaming E2E test | New streaming path |

#### Branch coverage checklist

| Branch/Path | Test that covers it |
|---|---|
| Events channel format | `test_events_channel_name` |
| Input channel format | `test_input_channel_name` |
| Prompt message JSON | `test_build_prompt_message_json` |
| Control message JSON | `test_build_control_interrupt_json` |
| Each ProgressKind deserialization | 5 `test_progress_event_deserialize_*` tests |
| Invalid JSON handling | `test_progress_event_invalid_json_returns_error` |
| Unknown kind handling | `test_progress_event_unknown_kind` |
| Persistence subscriber writes to DB | `test_persistence_subscriber_writes_to_db` |
| Persistence subscriber exits on Completed | `test_persistence_subscriber_exits_on_completed` |
| Persistence subscriber exits on Error | `test_persistence_subscriber_exits_on_error` |
| Persistence subscriber skips malformed | `test_persistence_subscriber_skips_malformed` |
| SSE bridge receives events (read-only) | `test_pubsub_bridge_receives_events` |
| SSE bridge skips malformed | `test_pubsub_bridge_ignores_malformed_events` |
| `send_message` pub/sub path | `test_send_message_routes_via_pubsub` |
| `send_message` legacy fallback | `test_send_message_falls_back_for_legacy` |
| SSE → pub/sub events | `test_sse_endpoint_streams_pubsub_events` |
| SSE content type | `test_sse_endpoint_returns_event_stream_content_type` |
| SSE global auth | `test_sse_global_endpoint_owner_only` |
| SSE auth required | `test_sse_endpoint_requires_auth` |
| SSE 404 for missing session | `test_sse_endpoint_nonexistent_session_404` |
| Multi-session isolation | `test_pubsub_bridge_multiple_sessions_isolated` |
| Receiver drop triggers cleanup | `test_pubsub_bridge_receiver_drop_unsubscribes` |
| Inprocess mode no regression | `test_send_message_still_works_for_inprocess` |
| `get_log_lines()` dead code removed | Compile success after deletion |

#### Tests NOT needed

| Area | Justification |
|---|---|
| Full E2E with real agent-runner + Claude CLI | Requires actual Claude CLI binary; beyond platform integration scope |
| SSE reconnection | Client concern — `EventSource` has built-in auto-reconnect |
| Concurrent SSE connections for same session | Sessions are 1:1 with pods by design |
| Large message handling (>1MB) | Agent-runner side enforces `MAX_INPUT_MESSAGE_SIZE`; platform accepts whatever Valkey delivers |

**Total: 11 unit + 17 integration + 1 E2E = 29 tests**

### Verification
- `just test-unit` passes — no compile errors after removing `ws` feature
- `just test-integration` passes with Valkey pub/sub + SSE endpoint tests
- `just test-e2e` passes with end-to-end pub/sub streaming
- `grep -r "WebSocket\|ws::" src/` — no remaining WebSocket references
- `grep -r "createWs\|ReconnectingWebSocket" ui/src/` — no remaining WS client references
- Manual: create session → SSE events stream via EventSource → messages send via REST POST
- Manual: observe/Logs live tail works via SSE

---

## Cross-Cutting Concerns Checklist

### PR 1 (Valkey ACL)
- [x] No new endpoints (internal module only)
- [x] No auth needed (called from already-authenticated session creation)
- [x] Audit: ACL creation/deletion logged via tracing with structured fields (`session_id`, `acl_username`) — not audit_log (ephemeral infra, consistent with session lifecycle not being in audit_log)
- [x] Secrets: ACL passwords never logged — `SessionValkeyCredentials` has custom Debug that redacts password/url
- [x] Config: new `PLATFORM_VALKEY_AGENT_HOST` env var documented
- [x] No AppState changes
- [x] ACL uses `resetkeys resetchannels -@all` baseline + explicit `+subscribe +publish +unsubscribe +ping` (no `+@pubsub`, no key access)

### PR 2 (Agent-runner enhancements)
- [x] Standalone crate — no platform dependencies
- [x] MCP config excludes admin server (security)
- [x] MCP server paths use `/opt/mcp/servers/*.js` (matching container directory structure)
- [x] Platform token not logged (clap `hide_env_values = true` already set)
- [x] No DB/migration changes

### PR 3 (Pod startup + migration)
- [x] Auth: ACL credentials are per-session, short-lived
- [x] Cleanup: ACL deletion in stop_session + reaper + create_session error path
- [x] VALKEY_URL in RESERVED_ENV_VARS prevents project secrets from hijacking
- [x] `ProgressEvent` gains `Deserialize` (needed by PR 4)
- [x] `AgentSession` gains `uses_pubsub: bool` field
- [x] `fetch_session()` SELECT updated with new column
- [x] Both `PodBuildParams` AND `BuildPodParams` get `valkey_url: Option<&'a str>`
- [x] ~40 existing pod.rs unit tests need `valkey_url: None` added
- [x] `.sqlx/` regeneration after migration + query changes

### PR 4 (Pub/sub bridge + SSE)
- [x] Auth: SSE endpoint still requires AuthUser (unchanged — session cookies sent automatically by `EventSource`)
- [x] Permissions: existing `require_project_read` check unchanged
- [x] Pub/sub subscription per SSE connection — follows `eventbus.rs` pattern
- [x] Message persistence: `spawn_persistence_subscriber()` is started in `create_session()` for every `uses_pubsub = true` session. It subscribes to `session:{id}:events` and writes each event to `agent_messages` — same INSERT as the old `stream_pod_logs_to_ws()` did inline. SSE subscribers are read-only (no double-writes). For Plan 40 create-app sessions, `publish_event()` goes through pub/sub → persistence subscriber handles DB writes (Plan 40's `save_assistant_message()` is removed).
- [x] Subscriber cleanup: background task unsubscribes on mpsc sender drop (SSE disconnect chain)
- [x] WebSocket infrastructure fully removed: `ws_handler`, `handle_ws`, `stream_broadcast_to_ws`, `stream_pod_logs_to_ws`, `ws_handler_global`, `handle_ws_global` (~265 LOC deleted)
- [x] Observe live tail converted from WebSocket to SSE
- [x] Frontend: `ws.ts` deleted, `sse.ts` created, 4 UI pages updated
- [x] `axum` `"ws"` feature removed, `tokio-stream` dependency added
- [x] No new AppState fields (uses existing `state.valkey`)
- [x] SSE handler uses pub/sub exclusively — old broadcast subscribe paths (`InProcessHandle.tx`, `CliSessionHandle.tx`) become dead code, cleaned up by Plan 40
- [x] `publish_event()` added for server-side event publishing (Plan 40 create-app flow)

---

## Container Image Build (BLOCKING Prerequisite for PR 3)

The agent pod image (`platform-claude-runner`) must be built and pushed **before** PR 3 is deployed. If the platform deploys PR 3 with the new entrypoint before the image is updated, new pods will fail with `CrashLoopBackOff` ("agent-runner: command not found").

The image must include:
1. `agent-runner` binary at `/usr/local/bin/agent-runner`
2. MCP server files at `/opt/mcp/` (preserving `mcp/servers/` and `mcp/lib/` directory structure for relative imports)
3. Node.js runtime (for MCP servers)
4. Claude CLI (`claude` binary)

```dockerfile
# Stage 1: Build agent-runner
FROM rust:1.82 AS builder
COPY cli/agent-runner/ /build/
RUN cargo build --release -p agent-runner

# Stage 2: Runtime
FROM node:22-slim
# Install Claude CLI
RUN npm install -g @anthropic-ai/claude-code
# Copy agent-runner
COPY --from=builder /build/target/release/agent-runner /usr/local/bin/
# Copy MCP servers (preserve directory structure for ../lib/client.js imports)
COPY mcp/ /opt/mcp/
RUN cd /opt/mcp && npm install --production
```

---

## Dependency Graph

```
Container Image Build ─────────────────────┐
                                            │
PR 1 (Valkey ACL) ──────┐                  │
                         ├──→ PR 3 (Pod + migration) ──→ PR 4 (Pub/sub bridge) ──→ Plan 40 (create-app CLI)
PR 2 (MCP config) ──────┘
```

- PR 1 and PR 2 are independent and can be developed in parallel
- Container image build can proceed in parallel with PRs 1-2
- PR 3 depends on PR 1 (needs ACL module), PR 2 (needs MCP config), and the container image
- PR 4 depends on PR 3 (needs `uses_pubsub` flag and pub/sub-enabled pods)
- **PR 3 and PR 4 MUST be deployed together** — between PR 3 (agent-runner entrypoint) and PR 4 (SSE bridge), agent-runner writes events to pub/sub but the old WebSocket handler reads pod stdout, resulting in empty event streams. Deploy as a single release.
- **Plan 40 depends on PR 4** — the create-app flow publishes events via `publish_event()` and the SSE endpoint subscribes via `subscribe_session_events()`, both from `pubsub_bridge.rs`

---

## Test Plan Summary

### Coverage target: 100% of touched lines

Every new or modified line of code must be covered by at least one test (unit, integration, or E2E). The test strategy above maps each code path to a specific test. `review` and `finalize` will verify with `just cov-unit` / `just cov-total`.

### New test counts by PR

| PR | Unit | Integration | E2E | Total |
|---|---|---|---|---|
| PR 1: Valkey ACL | 17 | 8 | 0 | 25 |
| PR 2: Agent-Runner MCP | 7 | 0 | 0 | 7 |
| PR 3: Pod Startup | 19 | 5 | 2 | 26 |
| PR 4: Pub/Sub Bridge | 11 | 17 | 1 | 29 |
| **Total** | **54** | **30** | **3** | **87 new** |

Plus ~50 existing pod.rs unit tests mechanically updated with `valkey_url: None`, and ~5 existing WebSocket tests removed.

### Coverage goals by module

| Module | Current tests | After plan |
|---|---|---|
| `src/agent/valkey_acl.rs` | 0 (new) | +17 unit + 8 integration |
| `src/agent/pubsub_bridge.rs` | 0 (new) | +11 unit + 17 integration |
| `src/agent/claude_code/pod.rs` | ~50 unit | +13 unit, ~50 updated |
| `src/agent/provider.rs` | 14 unit | +6 unit (Deserialize/Unknown) |
| `src/agent/service.rs` | 7 unit | +5 integration + 2 E2E |
| `src/config.rs` | existing | +3 unit |
| `cli/agent-runner/src/mcp.rs` | 0 (new) | +5 unit |
| `cli/agent-runner/src/main.rs` | existing | +2 unit |

---

## Plan Review Findings

**Date:** 2026-03-03
**Reviewed:** 2026-03-03 (5-agent parallel review: schema, security, architecture, tests, integration)
**Status:** APPROVED WITH CONCERNS

### Previous Review: Overlap Analysis + Pub/Sub Unification (2026-03-03)

Based on Plan 38 implementation completion and Plan 40 improvements:

1. **PR 2 `--prompt` flag already implemented** — Plan 38's agent-runner has `-p`/`--prompt` flag, REPL with `first_turn` handling, and natural stdin-EOF exit behavior. Removed redundant `run_single_shot()` function. PR 2 now focuses on MCP config generation only.

2. **WebSocket replaced with SSE + pub/sub** — This plan's PR 4 replaces all WebSocket infrastructure with SSE endpoints. The SSE handler subscribes to Valkey pub/sub via `pubsub_bridge::subscribe_session_events()`. The old broadcast subscribe paths (`inprocess::subscribe()`, `cli_sessions.subscribe()`) become dead code — Plan 40 removes them when it refactors `CliSessionHandle` and deletes `inprocess.rs`. Client→server messages use existing REST POST endpoints.

3. **`publish_event()` added to PR 4** — Server-side event publishing function for Plan 40's create-app flow. The pub/sub bridge module now handles both directions: subscribe (PR 4 original) and publish (new, for Plan 40).

4. **`send_message()` routing clarified** — This plan (PR 4) adds the pod pub/sub path only, gated on `uses_pubsub` flag. Plan 40 later extends the block with `cli_subprocess` routing via `pending_messages` queue.

### Codebase Reality Check (corrected in-place above)

| # | Issue | Fix |
|---|---|---|
| 1 | `PodBuildParams` vs `BuildPodParams` — two separate structs both need `valkey_url` | Both updated in PR 3 code changes table |
| 2 | `ProgressEvent` lacked `Deserialize` | Added to PR 3, with `#[serde(other)] Unknown` on `ProgressKind` |
| 3 | Migration was in wrong PR (PR 4 → PR 3) | Moved to PR 3 since it writes to the column |
| 4 | `+@pubsub` too broad (includes `PUBSUB CHANNELS`) | Changed to explicit `+subscribe +publish +unsubscribe` |
| 5 | ACL command missing `resetkeys -@all +ping` | Added to prevent default key access and support fred keepalive |
| 6 | `SessionValkeyCredentials` lacked custom Debug | Added `impl Debug` that redacts password/url |
| 7 | `ProgressKind` missing forward-compat for unknown kinds | Added `#[serde(other)] Unknown` variant |
| 8 | `get_log_lines()` becomes dead code after WebSocket removal | Added deletion to PR 4 code changes |
| 9 | `reap_terminated_sessions()` will exceed 100-line clippy limit | Added `finalize_reaped_session()` helper extraction |
| 10 | PR 3 → PR 4 streaming gap (agent-runner writes pub/sub, old WS reads stdout) | Added "MUST deploy together" constraint |
| 11 | Message persistence gap after `stream_pod_logs_to_ws()` removal | Added `spawn_persistence_subscriber()` — server-side persist-then-forward architecture |
| 12 | Container image was parallel, should be blocking | Changed to blocking prerequisite |

### Remaining Concerns

1. **Platform subscriber scaling** (MEDIUM) — PR 4 creates a new Valkey connection per active SSE session. With many concurrent sessions, this could exhaust connection limits. Future optimization: use `PSUBSCRIBE session:*:events` on a single connection and route by channel name. Not needed for initial implementation but worth tracking.

2. **Orphaned ACL on platform crash** (MEDIUM) — if the platform crashes between ACL creation and session DB update, orphaned Valkey users persist. The error-path cleanup in `create_session()` covers pod creation failures, but not platform crashes. The reaper only queries `WHERE status = 'running' AND pod_name IS NOT NULL`, so a `pending` session with no pod would be missed. Consider extending the reaper to sweep `pending` sessions older than 5 minutes and delete their ACL users.

3. **No ACL user TTL** (LOW) — Valkey ACL users have no built-in expiry. If Valkey is configured with `aclfile` persistence, orphaned users survive restarts. Document whether the deployment uses ACL persistence. Consider periodic `ACL LIST` reconciliation against active sessions.

4. **Message persistence** (RESOLVED) — `spawn_persistence_subscriber()` runs for every `uses_pubsub = true` session, writing events to `agent_messages` independently of SSE connections. SSE subscribers are read-only. Plan 40's `save_assistant_message()` is removed (persistence handled centrally).

5. **No feature flag for pub/sub** (LOW) — there is no `PLATFORM_VALKEY_ACL_ENABLED` toggle. Once PR 3 is deployed, ALL new sessions use agent-runner + pub/sub. Rollback of PR 4 while PR 3 is deployed would leave streaming broken. Mitigated by "deploy together" constraint.

6. **SSE connection exhaustion** (LOW) — each SSE connection creates a new Valkey subscriber via `clone_new()`. Add a concurrency limiter (similar to `WEBHOOK_SEMAPHORE`) if connection limits become an issue in production.

7. **`Config::load()` needs slight refactoring** (LOW) — `valkey_agent_host` default derives from `valkey_url`, requiring the URL to be computed before the struct literal. Extract a `let valkey_url = ...` binding before the `Config { ... }` construction.

### Simplification Opportunities

1. The two nearly-identical structs `PodBuildParams` (pod.rs) and `BuildPodParams` (provider.rs) are tech debt. Future consolidation would reduce the field-addition burden.
2. `inprocess::subscribe()`, `InProcessHandle.tx`, and `CliSessionHandle.tx` become dead code after this plan's WS→SSE conversion. Plan 40 removes them (deletes `inprocess.rs`, refactors `CliSessionHandle`).
3. The observe `live_tail` migration from `clone()` to `clone_new()` is an implicit bug fix (shared connection for subscriptions) — call it out in the PR description.

### Security Notes

- **ACL baseline: `resetkeys resetchannels -@all`** — starts from zero permissions, then adds only `+subscribe +publish +unsubscribe +ping`. No key-space access. No diagnostic commands. No pattern subscribe.
- **ACL passwords**: 256 bits of entropy via `rand::fill()` + `hex::encode()` — matches existing token generation in `src/auth/token.rs`.
- **`VALKEY_URL` reserved**: in `RESERVED_ENV_VARS` to prevent project secrets from hijacking Valkey credentials. Critical security control.
- **`VALKEY_URL` plaintext in pod spec**: same pattern as existing `ANTHROPIC_API_KEY` and `GIT_AUTH_TOKEN`. Acceptable for short-lived, narrowly-scoped credentials.
- **Session UUIDs server-generated**: `Uuid::new_v4()`, not user input. No injection risk in `ACL SETUSER` command construction.
- **`SessionValkeyCredentials` custom Debug**: redacts password and URL. Prevents accidental logging via `#[tracing::instrument]`.
- **WebSocket removal is a security improvement**: the old WS handlers accepted `SendMessageRequest` via WS text frames, bypassing the `require_session_write` check. After PR 4, messages can only be sent via REST POST endpoints which have proper authorization.
- **MCP config excludes admin server**: agents cannot perform admin operations.
