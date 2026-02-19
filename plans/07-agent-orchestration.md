# 07 — Agent Orchestration

## Prerequisite
- 01-foundation complete (store, AppState, kube client)
- 02-identity-auth complete (AuthUser, RequirePermission, delegation system)

## Blocks
- Nothing — self-contained module

## Can Parallelize With
- 03-git-server, 04-project-mgmt, 05-build-engine, 06-deployer, 08-observability, 09-secrets-notify

## Reference
- `mgr/` Go codebase provides the design patterns and logic to port

---

## Scope

Agent session lifecycle: create sessions, spawn agent pods in K8s, bridge WebSocket for live streaming, manage ephemeral agent identities with delegated RBAC permissions. Port from the existing `mgr/` Go prototype. Primary provider: Claude Code.

---

## Deliverables

### 1. `src/agent/mod.rs` — Module Root
Re-exports service, identity, provider. Contains `AgentProvider` trait definition.

### 2. `src/agent/provider.rs` — AgentProvider Trait

```rust
// Uses native async fn in trait (Rust 2024 edition) — no #[async_trait] crate needed
pub trait AgentProvider: Send + Sync {
    /// Build the K8s pod spec for this agent type
    fn pod_spec(&self, session: &AgentSession, config: &ProviderConfig) -> Result<PodSpec>;

    /// Parse streaming output into structured progress events
    fn parse_progress(&self, line: &str) -> Option<ProgressEvent>;

    /// Provider name (e.g., "claude-code")
    fn name(&self) -> &str;
}

pub struct ProgressEvent {
    pub kind: ProgressKind,
    pub message: String,
    pub metadata: Option<serde_json::Value>,
}

pub enum ProgressKind {
    Thinking,
    ToolCall,
    ToolResult,
    Milestone,
    Error,
    Completed,
}
```

### 3. `src/agent/identity.rs` — Ephemeral Agent Identity

Create and manage the agent's identity within the RBAC system:

- `pub async fn create_agent_identity(pool, session: &AgentSession, delegator_id: Uuid) -> Result<AgentIdentity>`
  1. Create a user row with `name = "agent-{session_id}"`, `is_active = true`
  2. Assign the `agent` system role
  3. Delegate permissions from the requesting user to the agent user
     - Which permissions to delegate: determined by the requesting user's session config
     - Default: `project:read`, `project:write`, `agent:run` on the specific project
     - Ops agents get additional: `deploy:read`, `observe:read`
  4. Return `AgentIdentity { user_id, api_token }` — agent uses this token for platform API calls

- `pub async fn cleanup_agent_identity(pool, agent_user_id: Uuid) -> Result<()>`
  - Revoke all delegations
  - Deactivate the agent user
  - Called when session finishes

### 4. `src/agent/service.rs` — Session Lifecycle

Core session management:

- `pub async fn create_session(state, user_id, project_id, prompt, provider, config) -> Result<AgentSession>`
  1. Insert `agent_sessions` row (status: `pending`)
  2. Create agent identity with delegated permissions
  3. Build pod spec from provider
  4. Create pod via kube-rs
  5. Update session status to `running`, store `pod_name`
  6. Return session (WebSocket clients can now connect)

- `pub async fn stream_session(state, session_id) -> Result<impl Stream<Item = AgentMessage>>`
  - Attach to pod stdout/stderr via kube-rs `AttachedProcess`
  - Parse streaming output through provider's `parse_progress()`
  - Store messages in `agent_messages` table
  - Yield structured `AgentMessage` events for WebSocket clients

- `pub async fn send_message(state, session_id, content: &str) -> Result<()>`
  - Write to pod stdin via kube-rs attached process
  - Store in `agent_messages` (role: `user`)

- `pub async fn stop_session(state, session_id) -> Result<()>`
  - Delete the pod
  - Update session status to `stopped`
  - Cleanup agent identity
  - Record final cost/tokens

- Background reaper task:
  - Periodically check for sessions where pod has terminated
  - Update status to `completed` or `failed` based on exit code
  - Cleanup agent identity

### 5. `src/agent/claude_code/mod.rs` — Claude Code Provider

Claude Code-specific implementation:

### 6. `src/agent/claude_code/adapter.rs` — Provider Implementation

```rust
pub struct ClaudeCodeProvider;

impl AgentProvider for ClaudeCodeProvider {
    fn pod_spec(&self, session: &AgentSession, config: &ProviderConfig) -> Result<PodSpec> {
        // Build pod spec for Claude Code agent
    }

    fn parse_progress(&self, line: &str) -> Option<ProgressEvent> {
        // Parse Claude Code's stream-json output format
    }

    fn name(&self) -> &str { "claude-code" }
}
```

### 7. `src/agent/claude_code/pod.rs` — Pod Spec Builder

Build the K8s pod spec for a Claude Code agent session:

- Base image: `docker/Dockerfile.claude-runner` (has git, Claude Code CLI, tools)
- Environment:
  - `ANTHROPIC_API_KEY` from platform secrets
  - `PLATFORM_API_URL` — platform's API URL for the agent to call back
  - `PLATFORM_API_TOKEN` — the agent's delegated API token
  - `PROJECT_CLONE_URL` — git clone URL for the project
  - `BRANCH` — working branch
  - `PROMPT` — the initial prompt
- Volume mounts:
  - EmptyDir for `/workspace` (agent's working directory)
- Init container: clone the project repo into `/workspace`
- Resource limits: configurable CPU/memory (default 2 CPU, 4Gi memory)
- Labels: `platform.io/session={id}`, `platform.io/project={id}`
- Restart policy: Never

### 8. `src/agent/claude_code/progress.rs` — Progress Parser

Parse Claude Code's streaming JSON output into structured events:

- Detect: `thinking`, `tool_use`, `tool_result`, `text` blocks
- Extract: tool name, file paths modified, commands run
- Milestone detection: "created file X", "ran tests", "committed changes"
- Cost tracking: extract token usage from stream metadata

### 9. `src/api/sessions.rs` — Agent Session API

- `POST /api/projects/:id/sessions` — create agent session
  - Required: `prompt`
  - Optional: `provider` (default: claude-code), `config` (provider-specific), `branch`
  - Requires: `agent:run` on the project
  - Returns session ID

- `GET /api/projects/:id/sessions` — list sessions
  - Filter by: status, user_id
  - Requires: `project:read`

- `GET /api/projects/:id/sessions/:session_id` — get session detail
  - Includes: status, pod_name, cost, messages (paginated)
  - Requires: `project:read`

- `GET /api/projects/:id/sessions/:session_id/ws` — WebSocket connection
  - Streams `AgentMessage` events in real-time
  - Allows sending messages (user → agent)
  - Requires: `project:read`

- `POST /api/projects/:id/sessions/:session_id/message` — send message to agent
  - Required: `content`
  - Requires: `project:write`

- `POST /api/projects/:id/sessions/:session_id/stop` — stop session
  - Requires: `project:write` or session owner

---

## Pod Lifecycle

```
User creates session → session row (pending)
  → create agent identity (user + delegated perms + API token)
  → build pod spec (image, env, volumes)
  → create K8s pod → session status = running
  → attach to pod stdout → stream to WebSocket clients
  → agent works: reads code, makes changes, pushes commits
  → agent uses PLATFORM_API_TOKEN to call platform APIs (RBAC-scoped)
  → pod exits → session status = completed/failed
  → cleanup agent identity (revoke delegations, deactivate user)
```

---

## Testing

- Unit: progress parser (various Claude Code output formats), pod spec building
- Integration:
  - Create session → pod created in kind cluster → status becomes running
  - Stream session → receive progress events via WebSocket
  - Send message → agent receives input
  - Stop session → pod deleted, identity cleaned up
  - Agent identity: verify delegated permissions work, verify cleanup on session end
  - Cost tracking: token counts stored on session

## Done When

1. Agent sessions create/start/stream/stop lifecycle works
2. Claude Code pods spawn in K8s with correct environment
3. Agent identity created with delegated permissions, cleaned up on session end
4. WebSocket streaming delivers real-time progress events
5. Messages can be sent to running agents
6. Pod reaper handles terminated sessions

## Estimated LOC
~800 Rust
