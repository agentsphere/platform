# Manager Agent — Implementation Plan

Replace the narrow create-app flow with a general-purpose Manager Agent that can orchestrate the entire platform through MCP tools, running safely on the platform pod with zero filesystem access.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│  Dashboard UI                                                    │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │  Floating Chat Widget (bottom center)                     │   │
│  │  ┌─ Tab Bar ─────────────────────────────────────────┐   │   │
│  │  │ [Session 1: Deploy v0.2] [Session 2: Fix bugs] [+] │   │   │
│  │  ├───────────────────────────────────────────────────────┤   │
│  │  │  SSE stream ← session events                         │   │
│  │  │  User input → POST /api/manager/{id}/message         │   │
│  │  │  Suggestions: "Deploy to prod" "Check pipelines"     │   │
│  │  └──────────────────────────────────────────────────────┘   │
│  └──────────────────────────────────────────────────────────┘   │
└──────────────┬──────────────────────────────────────────────────┘
               │ HTTP/SSE
┌──────────────▼──────────────────────────────────────────────────┐
│  Platform Backend (Rust)                                         │
│                                                                  │
│  POST /api/manager/sessions      → create manager session        │
│  POST /api/manager/{id}/message  → send to CLI subprocess stdin  │
│  GET  /api/manager/{id}/events   → SSE from Valkey pub/sub       │
│  GET  /api/manager/sessions      → list user's manager sessions  │
│  DELETE /api/manager/{id}        → stop session                  │
│                                                                  │
│  ┌────────────────────────────────────────────────────────────┐  │
│  │  Claude CLI subprocess (in-process, no K8s pod)            │  │
│  │                                                            │  │
│  │  --tools ""                  (no built-in tools)           │  │
│  │  --allowedTools "mcp__*"     (only MCP tools)              │  │
│  │  --permission-mode dontAsk   (auto-deny non-MCP)           │  │
│  │  --mcp-config /tmp/mgr.json  (MCP server definitions)     │  │
│  │  env_clear() + CLAUDE_CODE_OAUTH_TOKEN only                │  │
│  └───────┬───────┬───────┬───────┬───────┬──────────────────┘  │
│          │       │       │       │       │                      │
│  ┌───────▼─┐ ┌──▼────┐ ┌▼─────┐ ┌▼────┐ ┌▼──────┐             │
│  │ core.js │ │admin.js│ │pipe.js││dep.js││obs.js │  MCP servers │
│  └─────────┘ └───────┘ └──────┘ └─────┘ └──────┘              │
│                         │                                        │
│                  Platform REST API (localhost)                    │
└──────────────────────────────────────────────────────────────────┘
```

## Permission Model

### Ground Principles

1. **The manager agent acts as the user** — its API token carries exactly the user's permissions, not more. An admin's manager can do admin things; a viewer's manager can only read.

2. **Actions are classified, not tools** — every MCP tool maps to one action type (READ, CREATE, UPDATE, DELETE, DEPLOY). The permission mode controls which action types auto-execute vs require confirmation.

3. **Modes, not per-tool rules** — users pick a mode that matches their trust level. Modes are easy to understand ("Auto Read" = reads are free, writes ask). No complex per-tool configuration.

4. **Mode is per-session** — different sessions can have different modes. A "fix this bug" session might be Auto Write, while a "production audit" session stays Plan mode. Can be changed mid-conversation.

5. **Token scope = user scope** — the manager's API token carries the user's effective permissions with workspace boundary. No privilege escalation possible regardless of mode.

### Action Classification

Every MCP tool maps to exactly one action type:

```
READ     list_*, get_*, query_*, search_*, staging_status,
         dashboard_stats, get_platform_summary

CREATE   create_project, create_issue, create_comment, create_flag,
         create_command, create_alert_rule, create_user,
         create_delegation, spawn_agent, trigger_pipeline,
         ask_for_secret

UPDATE   update_issue, update_project, update_command, update_flag,
         toggle_flag, send_message_to_session, stop_session,
         cancel_pipeline, assign_role, create_secret, update_secret

DELETE   delete_project, delete_issue, delete_comment, delete_flag,
         delete_command, delete_alert_rule, delete_secret,
         deactivate_user, delete_delegation

DEPLOY   promote_staging, promote_release, rollback_release,
         resume_release
```

### Permission Modes

Five modes, ordered by increasing trust. User selects via dropdown in chat header.

```
┌──────────┬──────────┬──────────┬──────────┬──────────┬──────────┐
│          │ 🔒 Plan  │ 🔓 Guided│ 📖 Auto  │ ✏️ Auto  │ ⚡ Full  │
│ Action   │          │          │ Read     │ Write    │ Auto     │
├──────────┼──────────┼──────────┼──────────┼──────────┼──────────┤
│ READ     │  auto    │  auto    │  auto    │  auto    │  auto    │
├──────────┼──────────┼──────────┼──────────┼──────────┼──────────┤
│ CREATE   │  deny *  │  ask     │  ask     │  auto    │  auto    │
├──────────┼──────────┼──────────┼──────────┼──────────┼──────────┤
│ UPDATE   │  deny *  │  ask     │  ask     │  auto    │  auto    │
├──────────┼──────────┼──────────┼──────────┼──────────┼──────────┤
│ DELETE   │  deny *  │  ask     │  ask     │  ask     │  auto    │
├──────────┼──────────┼──────────┼──────────┼──────────┼──────────┤
│ DEPLOY   │  deny *  │  ask     │  ask     │  ask     │  auto    │
└──────────┴──────────┴──────────┴──────────┴──────────┴──────────┘

auto = executes immediately, no prompt
ask  = shows confirmation dialog (approve once / approve for session / deny)
deny = blocked; manager describes what it WOULD do instead (plan output)

* Plan mode: mutations are denied at the UI layer. The manager receives a
  "tool not available in plan mode" result and instead describes the action
  as a plan step. This is useful for auditing ("what would you do?") and
  for users who want to review a full plan before switching to Guided mode.
```

**Mode descriptions:**

| Mode | Icon | Behavior | Good for |
|------|------|----------|----------|
| **Plan** | 🔒 | Read everything, write nothing. Mutations shown as planned steps. | Auditing, understanding state, getting recommendations |
| **Guided** | 🔓 | Every mutation asks for approval. Most cautious interactive mode. | Learning the platform, high-stakes environments, new users |
| **Auto Read** | 📖 | Reads flow freely, all writes ask. **Default mode.** | Investigation ("find what's wrong, then ask me before fixing") |
| **Auto Write** | ✏️ | Reads + creates + updates flow freely. Deletes and deploys ask. | Active development ("create, build, iterate — but check before prod") |
| **Full Auto** | ⚡ | Everything auto-approved. Warning banner shown. | Experienced users, scripted workflows, batch operations |

**Default:** Auto Read (📖) — safe default that lets the manager investigate freely but asks before making any changes.

### How confirmation works in practice

**Auto Read mode (default):**

```
User: "What's the status of platform-demo?"

Manager: ✓ [auto: list_projects, get_pipeline, staging_status]

         platform-demo:
         • Build #42: success (2 min ago)
         • Staging: v0.2.0 — healthy
         • Production: v0.1.0 — healthy
         • Staging is ahead of production

User: "Deploy the staging version to production"

Manager: I'll promote staging to production for platform-demo.

         ┌─────────────────────────────────────────┐
         │  promote_staging                         │
         │  Staging v0.2.0 → Production             │
         │                                          │
         │  [Approve] [Approve for session] [Deny]  │
         └─────────────────────────────────────────┘

User: [clicks Approve]

Manager: ✓ Promoted platform-demo staging → production
         Canary rollout started: 10% → 50% → 100%
```

**Plan mode:**

```
User: "Fix the broken build and deploy"

Manager: Here's what I would do:

         1. Check build logs for platform-demo pipeline #43
         2. The error is a missing dependency in requirements.txt
         3. Spawn a dev agent to fix requirements.txt
         4. After fix merges, trigger a new build
         5. Once build succeeds, promote staging to production

         Switch to Guided or Auto Write mode to execute this plan.
```

### Confirmation dialog options

When a tool requires confirmation:

- **Approve** — execute this one time
- **Approve for session** — auto-approve this tool name for the rest of this session (only for CREATE/UPDATE, never for DELETE/DEPLOY)
- **Deny** — cancel this action, tell the manager it was denied

### Token creation for manager sessions

When creating a manager session, the token carries the user's own permissions:

```rust
// 1. Resolve user's effective permissions (global — no project filter)
let user_perms = resolver::effective_permissions(pool, valkey, user_id, None).await?;

// 2. Create API token with those permissions as scopes
let scopes: Vec<String> = user_perms.iter().map(|p| p.as_str().to_owned()).collect();

// 3. Token has NO workspace or project boundary — manager is global
//    The user's RBAC permissions are the only constraint.
sqlx::query!(
    "INSERT INTO api_tokens (user_id, name, token_hash, scopes, expires_at)
     VALUES ($1, $2, $3, $4, $5)",
    agent_user_id,
    format!("manager-session-{session_id}"),
    token_hash,
    &scopes,
    Utc::now() + Duration::hours(4),  // 4h lifetime
);
```

**Key difference from agent identity tokens:**
- Agent tokens: project-scoped, role-filtered (role_perms ∩ spawner_perms)
- Manager tokens: **no boundary** (global), user-permissions-as-is (no role intersection — the user IS the authority)

The manager is a global UI element (dashboard, settings, health) — workspace scoping would break cross-workspace project management and platform-level operations. The user's own RBAC permissions are the only limit.

### Confirmation implementation: MCP-level gate

**Why not UI-level interception:**

When Claude CLI runs with `--output-format stream-json` and MCP tools, the tool execution happens *inside* the CLI process: Claude decides to call a tool → CLI invokes MCP server → server executes → result returns to Claude → we see the `tool_use` event in the NDJSON stream *after* execution. The UI cannot pause mid-tool-call — by the time we see the event, the action has already happened.

**The gate must live in the MCP servers.**

**How it works:**

Each MCP server receives the current permission mode via environment variable `MANAGER_MODE` (set when spawning the MCP process as part of the manager session's MCP config). The mode can be updated mid-session by writing to a shared file or Valkey key that the MCP server reads on each tool call.

Every tool call goes through a gate function before executing:

```javascript
// mcp/lib/gate.js — shared by all MCP servers

const ACTION_TYPES = {
  // READ — auto in all modes
  list_projects: 'READ', get_project: 'READ', query_logs: 'READ',
  list_pipelines: 'READ', staging_status: 'READ', /* ... */

  // CREATE
  create_project: 'CREATE', spawn_agent: 'CREATE', trigger_pipeline: 'CREATE',
  create_issue: 'CREATE', create_flag: 'CREATE', /* ... */

  // UPDATE
  update_project: 'UPDATE', toggle_flag: 'UPDATE', stop_session: 'UPDATE',
  cancel_pipeline: 'UPDATE', assign_role: 'UPDATE', /* ... */

  // DELETE
  delete_project: 'DELETE', delete_flag: 'DELETE', deactivate_user: 'DELETE',
  delete_secret: 'DELETE', /* ... */

  // DEPLOY
  promote_staging: 'DEPLOY', promote_release: 'DEPLOY',
  rollback_release: 'DEPLOY', resume_release: 'DEPLOY',
};

const MODE_MATRIX = {
  plan:       { READ: 'auto', CREATE: 'deny',  UPDATE: 'deny',  DELETE: 'deny',  DEPLOY: 'deny'  },
  guided:     { READ: 'auto', CREATE: 'ask',   UPDATE: 'ask',   DELETE: 'ask',   DEPLOY: 'ask'   },
  auto_read:  { READ: 'auto', CREATE: 'ask',   UPDATE: 'ask',   DELETE: 'ask',   DEPLOY: 'ask'   },
  auto_write: { READ: 'auto', CREATE: 'auto',  UPDATE: 'auto',  DELETE: 'ask',   DEPLOY: 'ask'   },
  full_auto:  { READ: 'auto', CREATE: 'auto',  UPDATE: 'auto',  DELETE: 'auto',  DEPLOY: 'auto'  },
};

export function gate(toolName, mode) {
  // Fail closed: unknown tools require confirmation in ALL modes (except full_auto).
  // This prevents a new destructive tool from auto-executing if someone forgets
  // to add it to ACTION_TYPES.
  const actionType = ACTION_TYPES[toolName] || 'UNKNOWN';
  if (actionType === 'UNKNOWN') {
    return mode === 'full_auto' ? 'auto' : 'ask';
  }
  const decision = MODE_MATRIX[mode]?.[actionType] || 'ask';
  return decision; // 'auto' | 'ask' | 'deny'
}
```

**Each MCP server wraps its tool handler:**

```javascript
import { gate, computeActionHash, checkApproval, setPending } from '../lib/gate.js';

server.setRequestHandler(CallToolRequestSchema, async (request) => {
  const { name, arguments: args = {} } = request.params;
  const mode = await readCurrentMode(); // from Valkey

  const decision = gate(name, mode);

  if (decision === 'deny') {
    // Plan mode: tell Claude to describe the action as a plan step
    return {
      content: [{
        type: 'text',
        text: JSON.stringify({
          status: 'denied',
          reason: `Action "${name}" is not available in ${mode} mode. ` +
                  `Do NOT attempt alternative write operations. ` +
                  `Describe this as a numbered plan step instead.`,
          action_type: ACTION_TYPES[name],
        })
      }]
    };
  }

  if (decision === 'ask') {
    const actionHash = computeActionHash(SESSION_ID, name, args);

    // Check if this specific action was approved via Valkey
    const approved = await checkApproval(SESSION_ID, actionHash);
    if (!approved) {
      // Check if this tool name is session-approved (CREATE/UPDATE only)
      const sessionApproved = await isToolSessionApproved(SESSION_ID, name);
      if (!sessionApproved) {
        // Write pending action to Valkey (expires in 5 min)
        const summary = buildSummary(name, args);
        await setPending(SESSION_ID, actionHash, summary);

        return {
          content: [{
            type: 'text',
            text: JSON.stringify({
              status: 'confirmation_required',
              action_hash: actionHash,
              tool: name,
              action_type: ACTION_TYPES[name],
              summary,
              message: 'Ask the user to confirm this action before proceeding.',
            })
          }]
        };
      }
    }
  }

  // 'auto' or approved — execute the actual tool
  // ... existing tool logic ...
});
```

**Confirmation round-trip (LLM-free authorization chain):**

Critical: the LLM must NEVER be in the authorization chain. Claude could hallucinate `confirmed: true` on its first tool call attempt, bypassing the gate entirely. Instead, approvals flow through Valkey with a unique action hash:

```
1. Claude calls: mcp__platform-deploy__promote_staging({ project_id: "..." })

2. MCP gate:
   - Generates action_hash = sha256(session_id + tool_name + JSON(params))
   - Writes to Valkey: SET manager:{session_id}:pending:{action_hash} "{summary}" EX 300
   - Returns: { status: "confirmation_required", action_hash: "abc123",
                summary: "Promote staging → prod" }

3. Claude sees the result and writes to the user:
   "I'd like to promote staging to production. Shall I proceed?"

4. UI renders [Approve] [Approve for session] [Deny] buttons

5. User clicks [Approve]:
   - UI calls POST /api/manager/sessions/{id}/approve_action { action_hash: "abc123" }
   - Backend writes: SET manager:{session_id}:approved:{action_hash} "1" EX 60
   - UI sends "Approved, please proceed." as user message

6. Claude calls the tool again (same params)

7. MCP gate:
   - Computes same action_hash
   - Checks: EXISTS manager:{session_id}:approved:{action_hash} → found
   - Deletes the approved key (single-use)
   - Executes the actual tool
   - Returns: { status: "success", ... }
```

The LLM cannot forge an approval — it flows through the UI → backend → Valkey → MCP server chain. Even if Claude appends `confirmed: true`, the MCP server ignores that parameter and checks Valkey instead.

**Advantages of MCP-level gate:**
- Works correctly with `--output-format stream-json` (no race condition)
- Claude sees denied/confirmation results and can reason about them
- In Plan mode, Claude gets explicit "denied" results and naturally writes plan steps
- The confirmation becomes part of the conversation (auditable in message history)
- Mode changes take effect on next tool call (read from shared state)
- No UI-level tool interception needed (simpler frontend)
- **LLM cannot bypass authorization** — approval state is in Valkey, not in tool params

**How mode changes propagate to running MCP servers:**

When the user changes mode in the UI dropdown:
1. UI calls `POST /api/manager/sessions/{id}/mode` with `{ mode: "auto_write" }`
2. Backend writes mode to Valkey: `SET manager:{session_id}:mode "auto_write"`
3. MCP servers read `readCurrentMode()` on each tool call:
   ```javascript
   function readCurrentMode() {
     // Read from Valkey via platform API, or from a mode file
     // Falls back to 'auto_read' if not set
   }
   ```

This avoids restarting MCP servers on mode change. The mode is a lightweight lookup per tool call.

**How "Approve for session" works:**

When the user approves a tool for the session, the UI calls:
`POST /api/manager/sessions/{id}/approve` with `{ tool: "create_project" }`

Backend stores in Valkey: `SADD manager:{session_id}:approved "create_project"`

MCP `isApproved()` checks: `SISMEMBER manager:{session_id}:approved "create_project"`

Only allowed for CREATE/UPDATE actions. DELETE/DEPLOY always ask (never session-approved).

## Phase 1: Backend — Manager Session

### 1.1 Migration: execution_mode

```sql
-- Add 'manager' to execution_mode CHECK constraint
ALTER TABLE agent_sessions
  DROP CONSTRAINT IF EXISTS agent_sessions_execution_mode_check;
ALTER TABLE agent_sessions
  ADD CONSTRAINT agent_sessions_execution_mode_check
  CHECK (execution_mode IN ('pod', 'cli_subprocess', 'manager'));
```

### 1.2 Config

**File:** `src/config.rs`

```rust
pub mcp_servers_path: String,  // PLATFORM_MCP_SERVERS_PATH, default: "mcp/servers"
```

### 1.3 CliSpawnOptions

**File:** `src/agent/claude_cli/transport.rs`

Add:
```rust
pub mcp_config_path: Option<String>,  // --mcp-config <path>
pub disable_tools: bool,               // --tools ""
```

In `spawn()`:
```rust
if let Some(ref mcp) = options.mcp_config_path {
    cmd.args(["--mcp-config", mcp]);
}
if options.disable_tools {
    cmd.args(["--tools", ""]);
}
```

### 1.4 create_manager_session

**File:** `src/agent/service.rs`

```rust
pub async fn create_manager_session(
    state: &AppState,
    user_id: Uuid,
    prompt: Option<String>,
) -> Result<(Uuid, String), AgentError> {
    let session_id = Uuid::new_v4();

    // 1. Insert session row
    sqlx::query("INSERT INTO agent_sessions (...) VALUES (...)")
        .bind(session_id)
        .bind(user_id)
        .bind(prompt.as_deref().unwrap_or(""))
        .bind("running")
        .bind("manager")  // execution_mode
        .execute(&state.pool).await?;

    // 2. Create manager identity + scoped token
    let identity = create_manager_identity(
        &state.pool, &state.valkey, session_id, user_id
    ).await?;

    // 3. Write MCP config to temp file
    let mcp_config = build_manager_mcp_config(
        &state.config.platform_api_url,
        &identity.api_token,
        &state.config.mcp_servers_path,
    );
    let mcp_path = format!("/tmp/manager-mcp-{session_id}.json");
    tokio::fs::write(&mcp_path, serde_json::to_string_pretty(&mcp_config)?).await?;

    // 4. Resolve LLM provider (same as global sessions)
    let (oauth_token, api_key, extra_env, _) =
        resolve_active_llm_provider(state, user_id, "auto").await?;

    // 5. Spawn CLI with safe MCP-only flags
    let opts = CliSpawnOptions {
        system_prompt: Some(MANAGER_SYSTEM_PROMPT),
        allowed_tools: Some(vec!["mcp__*".to_string()]),
        permission_mode: Some("dontAsk".to_string()),
        disable_tools: true,
        mcp_config_path: Some(mcp_path.clone()),
        oauth_token,
        api_key,
        extra_env,
        // NO --max-turns 1, NO --json-schema (multi-turn, native tool_use)
        ..Default::default()
    };

    // 6. Register CLI session handle + spawn
    let handle = CliSessionHandle::new(session_id);
    state.cli_sessions.write().await.insert(session_id, handle);
    spawn_manager_loop(state.clone(), session_id, prompt, opts);

    Ok((session_id, "running".into()))
}
```

### 1.5 Manager system prompt

**New file:** `src/agent/manager_prompt.rs`

```rust
pub const MANAGER_SYSTEM_PROMPT: &str = r#"
You are the Platform Manager — an AI assistant that helps operate a DevOps platform.

You have access to tools for managing projects, agents, pipelines, deployments,
observability, issues, and platform administration. Use them to help the user.

## Guidelines

- For read operations (listing, querying, checking status), act immediately.
- For write operations (creating, updating, deploying), describe what you'll do
  first and wait for the user to confirm before calling the tool.
- For dangerous operations (delete, rollback, promote to production), always
  explain the impact and ask for explicit confirmation.
- When spawning dev agents, write clear, focused prompts describing the task.
- After completing a task, suggest logical next steps.
- If a request is ambiguous, ask for clarification.
- Summarize status checks concisely — users want quick answers.

## Important: handling denied or confirmation-required tool results

- If a tool returns `status: "confirmation_required"`, ask the user to confirm.
  Do NOT call the tool again until the user explicitly approves.
- If a tool returns `status: "denied"`, the current permission mode does not
  allow this action. Do NOT attempt alternative write operations or retry.
  Instead, immediately describe what you would do as a numbered plan.
  The user can switch to a different mode to execute the plan.

## Available Tool Categories

- **Projects**: create, list, inspect projects
- **Sessions**: spawn dev/ops/review agents, check progress, send messages
- **Pipelines**: trigger builds, check status, read logs
- **Deployments**: manage releases, promote staging, rollback
- **Observability**: query logs/traces/metrics, manage alerts
- **Issues**: create/manage issues and comments
- **Admin**: manage users, roles, permissions (if user has admin access)
"#;
```

### 1.6 API endpoints

**File:** `src/api/sessions.rs` (extend)

```
POST /api/manager/sessions           → create_manager_session
GET  /api/manager/sessions           → list user's manager sessions
POST /api/manager/sessions/{id}/message → send message
GET  /api/manager/sessions/{id}/events  → SSE stream
DELETE /api/manager/sessions/{id}     → stop session
```

List returns all manager sessions for the authenticated user (running + recent completed).

### 1.7 MCP config builder

```rust
fn build_manager_mcp_config(
    api_url: &str,
    api_token: &str,
    servers_path: &str,
) -> serde_json::Value {
    let servers = [
        "platform-core",
        "platform-admin",
        "platform-pipeline",
        "platform-deploy",
        "platform-observe",
        "platform-issues",
    ];
    let mut map = serde_json::Map::new();
    for name in &servers {
        map.insert(name.to_string(), json!({
            "command": "node",
            "args": [format!("{servers_path}/{name}.js")],
            "env": {
                "PLATFORM_API_URL": api_url,
                "PLATFORM_API_TOKEN": api_token,
            }
        }));
    }
    json!({ "mcpServers": map })
}
```

### 1.8 Cleanup

On session stop/reap:
- Delete MCP config temp file
- Delete agent identity (user + token)
- Standard session cleanup

## Phase 2: Frontend — Chat Widget

### 2.1 Multi-session tab bar + mode selector

```
┌─ Manager ──────────────────────────────────────────────────┐
│  ┌──────────────┐ ┌──────────────┐ ┌───┐  Mode: [📖 ▾]    │
│  │ Deploy v0.2  │ │ Check logs ✓ │ │ + │  ┌────────────┐  │
│  └──────────────┘ └──────────────┘ └───┘  │🔒 Plan     │  │
│                                            │🔓 Guided   │  │
│                                            │📖 Auto Read│  │
│                                            │✏️ Auto Write│  │
│                                            │⚡ Full Auto │  │
│                                            └────────────┘  │
├────────────────────────────────────────────────────────────┤
│                                                            │
│  Messages for active tab...                                │
│                                                            │
├── Suggestions ─────────────────────────────────────────────┤
│  [Deploy to prod] [Check pipelines] [New project]          │
├────────────────────────────────────────────────────────────┤
│  ┌────────────────────────────────────────┐ [Send]         │
│  │ Type a message...                      │                │
│  └────────────────────────────────────────┘                │
└────────────────────────────────────────────────────────────┘
```

**Tab behavior:**
- Each tab = one manager session with its own SSE stream + its own mode
- `[+]` creates a new session (POST /api/manager/sessions), defaults to Auto Read mode
- Tabs show first user message as title (truncated to 20 chars)
- Completed sessions show ✓, failed show ✗
- Clicking a tab switches the message view + SSE stream + mode display
- Tab state stored in localStorage: `manager_sessions: [{id, title, status, mode}]`
- On page load, reconnect SSE to running sessions
- Mode dropdown changes the active session's mode immediately (no backend call — mode is a UI-only concept that controls the confirmation gate)
- Full Auto mode shows a yellow warning banner: "All actions auto-approved"

### 2.2 ManagerChat component

**New file:** `ui/src/components/ManagerChat.tsx`

```typescript
interface ManagerSession {
  id: string;
  title: string;
  status: 'running' | 'completed' | 'failed' | 'stopped';
  messages: ChatMessage[];
}

interface ChatMessage {
  id: string;
  role: 'user' | 'assistant' | 'tool_call' | 'tool_result' | 'system';
  content: string;
  timestamp: string;
  toolMeta?: {
    name: string;
    status: 'running' | 'success' | 'error';
    tier?: 0 | 1 | 2;
  };
}

function ManagerChat() {
  const [sessions, setSessions] = useState<ManagerSession[]>([]);
  const [activeIdx, setActiveIdx] = useState(0);
  const [isOpen, setIsOpen] = useState(false);
  const [isMinimized, setIsMinimized] = useState(false);

  // Load sessions from localStorage on mount
  // Connect SSE for running sessions
  // Create new session on [+] click
  // Send message on submit
  // Parse SSE events into ChatMessage[]
}
```

### 2.3 Confirmation dialog (Tier 1/2 gate)

When the SSE stream contains a `tool_use` event for a Tier 1/2 tool:

```typescript
// Intercept tool_use events
if (event.type === 'assistant' && event.message.content[0]?.type === 'tool_use') {
  const toolName = event.message.content[0].name;
  const tier = getToolTier(toolName);

  if (tier >= 1 && !approvedTools.has(toolName)) {
    // Show confirmation dialog
    setPendingTool({ name: toolName, params: event.message.content[0].input });
    return; // Don't add to messages until approved
  }
}
```

The confirmation dialog:
```
┌─────────────────────────────────────────┐
│  Confirm Action                          │
│                                          │
│  promote_staging                         │
│  Promote platform-demo staging → prod    │
│                                          │
│  [Allow Once]  [Allow for Session]  [×]  │
└─────────────────────────────────────────┘
```

- **Allow Once** → sends approval, tool executes, next call asks again
- **Allow for Session** → adds to `approvedTools` set (Tier 1 only, never for Tier 2)
- **[×]** → sends denial message to session

### 2.4 Suggestions

Context-aware, based on dashboard state (already fetched):

```typescript
function getSuggestions(projects, stats): string[] {
  const suggestions = [];
  if (projects.length === 0) suggestions.push("Create a new project");
  if (stats?.failed_builds > 0) suggestions.push("Check failed builds");
  if (stats?.active_sessions > 0) suggestions.push("Check agent progress");
  // Per-project suggestions
  for (const p of projects) {
    if (p.staging_diverged) suggestions.push(`Promote ${p.name} to prod`);
  }
  return suggestions.slice(0, 4);
}
```

### 2.5 Integration with Dashboard

**File:** `ui/src/pages/Dashboard.tsx`

```tsx
import { ManagerChat } from '../components/ManagerChat';

// At the bottom of the dashboard (outside the grid):
<ManagerChat />
```

The chat widget is `position: fixed` so it floats above all pages, not just the dashboard.

**File:** `ui/src/components/Layout.tsx` (or equivalent app wrapper)

Actually place `<ManagerChat />` in the app root so it persists across navigation.

## Phase 3: MCP Server Enhancements

### 3.1 New tools for platform-core.js

```javascript
// Cross-project session listing
{
  name: "list_all_sessions",
  description: "List agent sessions across all projects",
  inputSchema: { status: "string?", limit: "number?" }
}

// Platform summary (calls /api/dashboard/stats)
{
  name: "get_platform_summary",
  description: "Platform status: active sessions, builds, deploys, errors",
  inputSchema: {}
}
```

### 3.2 Backend: admin sessions endpoint

**File:** `src/api/admin.rs`

```
GET /api/admin/sessions?status=running&limit=20
```

Simple query across all `agent_sessions` with optional status filter.

## Phase 4: Testing

### 4.1 Unit tests (in source files)

| Test | File | What it tests |
|------|------|--------------|
| `manager_mcp_config_has_all_servers` | `service.rs` | Config includes 6 MCP servers |
| `manager_mcp_config_injects_token` | `service.rs` | Token embedded in env |
| `manager_mcp_config_correct_paths` | `service.rs` | Server script paths correct |
| `manager_prompt_no_tool_schemas` | `manager_prompt.rs` | Unlike create_app, no hardcoded schemas |
| `manager_prompt_has_guidelines` | `manager_prompt.rs` | Contains confirmation guidelines |

### 4.2 Integration tests

| Test | File | What it tests |
|------|------|--------------|
| `manager_session_create` | `tests/manager_integration.rs` | POST /api/manager/sessions → 201, execution_mode=manager |
| `manager_session_requires_auth` | `tests/manager_integration.rs` | No token → 401 |
| `manager_session_send_message` | `tests/manager_integration.rs` | POST message → 200 |
| `manager_session_stop` | `tests/manager_integration.rs` | DELETE → stopped, cleanup |
| `manager_session_list` | `tests/manager_integration.rs` | GET returns user's sessions only |
| `manager_session_events_sse` | `tests/manager_integration.rs` | SSE headers correct |
| `manager_mcp_config_written` | `tests/manager_integration.rs` | Temp file created with correct content |
| `manager_cleanup_deletes_mcp_config` | `tests/manager_integration.rs` | Temp file removed on stop |
| `manager_token_has_user_permissions` | `tests/manager_integration.rs` | Token scopes match user perms |
| `manager_token_workspace_boundary` | `tests/manager_integration.rs` | Token has workspace_id boundary |
| `admin_list_all_sessions` | `tests/admin_integration.rs` | Cross-project session listing |
| `admin_list_sessions_non_admin_403` | `tests/admin_integration.rs` | Permission check |

### 4.3 Delete old create-app flow

The manager agent fully replaces the custom create-app tool loop. Delete, don't deprecate.

**Files to delete:**

| File | Lines | What it was |
|------|-------|-------------|
| `src/agent/create_app.rs` | 1260 | Custom tool loop, 4 hardcoded tools, structured output parsing |
| `src/agent/create_app_prompt.rs` | 156 | System prompt with inline tool schemas |
| `tests/create_app_integration.rs` | ~550 | Integration tests for custom tool dispatch |
| `tests/cli_create_app_integration.rs` | ~200 | CLI subprocess create-app tests |

**Files to modify (remove create-app references):**

| File | Change |
|------|--------|
| `src/agent/mod.rs` | Remove `pub mod create_app; pub mod create_app_prompt;` |
| `src/api/sessions.rs` | Remove `POST /api/create-app` route + `create_app()` handler (~80 lines) |
| `src/agent/service.rs` | Remove `create_global_session()` (replaced by `create_manager_session()`) |
| `tests/session_integration.rs` | Remove tests that reference create_app flow |
| `tests/contract_integration.rs` | Remove create_app contract tests |
| `tests/session_coverage_integration.rs` | Remove create_app coverage tests |
| `ui/src/pages/Dashboard.tsx` | Change hero "Create" button from `/create-app` to manager chat |
| `CLAUDE.md` | Update agent module description |

**The `/api/create-app` endpoint is replaced by:**
- `POST /api/manager/sessions` — creates a manager session
- Manager agent can create projects via `mcp__platform-core__create_project`
- Manager agent can spawn dev agents via `mcp__platform-core__spawn_agent`

**LLM test files (keep, separate concern):**

| File | Keep? | Reason |
|------|-------|--------|
| `tests/llm_create_app.rs` | Rename to `tests/llm_manager.rs` | Test real CLI + MCP flow |
| `tests/llm_create_app_e2e.rs` | Rename to `tests/llm_manager_e2e.rs` | E2E with real Anthropic API |

### 4.4 Existing tests to update

| File | Change | Reason |
|------|--------|--------|
| `src/agent/claude_cli/transport.rs` | Add spawn test with mcp_config | New field |
| `tests/helpers/mod.rs` | No change | test_state handles CLI mock |
| `tests/e2e_helpers/mod.rs` | Add mcp_servers_path to config | New config field |
| `tests/setup_integration.rs` | Add mcp_servers_path to config | New config field |
| `cli/claude-mock/claude` | Add MCP mode detection | New spawn flags |

### 4.4 Mock CLI update

**File:** `cli/claude-mock/claude`

```bash
# Detect MCP-only mode
if [[ "$DISABLE_TOOLS" == "true" ]] && [[ -n "$MCP_CONFIG" ]]; then
    echo '{"type":"system","subtype":"init","session_id":"'$SESSION_ID'","tools":["mcp__platform-core__list_projects"]}'
    echo '{"type":"assistant","message":{"content":[{"type":"text","text":"I have access to platform tools. How can I help?"}]}}'
    echo '{"type":"result","subtype":"success","session_id":"'$SESSION_ID'","is_error":false,"result":"..."}'
    exit 0
fi
```

## Implementation Order

```
=== Backend ===
Step 1:  Delete old create-app flow                         (1 hour)
         - Delete src/agent/create_app.rs, create_app_prompt.rs
         - Delete tests/create_app_integration.rs, cli_create_app_integration.rs
         - Remove POST /api/create-app route + handler
         - Remove create_global_session() from service.rs
         - Clean up mod.rs, session tests, contract tests
         - Rename LLM test files
         - Update CLAUDE.md
Step 2:  Migration + config (mcp_servers_path)              (30 min)
Step 3:  CliSpawnOptions (mcp_config_path, disable_tools)   (30 min)
Step 4:  build_manager_mcp_config()                         (30 min)
Step 5:  create_manager_session() + cleanup                 (2 hours)
Step 6:  manager_prompt.rs                                  (30 min)
Step 7:  API endpoints (/api/manager/*)                     (2 hours)
         - POST sessions (create, enforce limit, auto-reap)
         - GET sessions (list user's manager sessions)
         - POST sessions/{id}/message (send to CLI stdin)
         - GET sessions/{id}/events (SSE from Valkey pub/sub)
         - DELETE sessions/{id} (stop session + cleanup)
         - POST sessions/{id}/mode { mode } (SET in Valkey)
         - POST sessions/{id}/approve_action { action_hash }
           (SET approved:{hash} in Valkey, single-use, 60s TTL)
         - POST sessions/{id}/approve_tool { tool_name }
           (SADD session-approved set, CREATE/UPDATE only)
Step 8:  Mock CLI update (MCP mode)                          (30 min)
Step 9:  Unit + integration tests                            (2 hours)
--- backend done, test with curl ---

=== MCP Gate ===
Step 10: mcp/lib/gate.js — action classifier + mode matrix   (1 hour)
         - Valkey-based approval checks (action hash)
         - computeActionHash, checkApproval, setPending
         - Unknown tools fail closed (always ask)
Step 11: Wrap all 6 MCP servers with gate() checks            (2 hours)
         - confirmation_required with action_hash for 'ask'
         - denied with plan-mode instructions for 'deny'
         - Valkey approval check (not LLM confirmed param)
Step 12: Mode read from Valkey (via platform API)             (1 hour)
Step 13: Session-approved tool set (Valkey SISMEMBER)         (30 min)
--- MCP gate done ---

=== Frontend ===
Step 14: ManagerChat.tsx (single session, basic messages)     (2 hours)
Step 15: SSE integration + NDJSON parsing                     (1.5 hours)
Step 16: Multi-session tab bar + localStorage persist         (1.5 hours)
Step 17: Mode selector dropdown (5 modes)                     (1 hour)
         - calls POST /api/manager/sessions/{id}/mode
Step 18: Confirmation rendering (Claude asks, user responds)  (1 hour)
         - detect "confirmation_required" in assistant text
         - render [Approve] [Approve for session] [Deny] buttons
         - Approve calls POST .../approve_action { action_hash }
           then sends "Approved, proceed." as user message
         - Approve for session also calls POST .../approve_tool
           (CREATE/UPDATE only)
         - Deny sends "Denied, do not proceed." as user message
Step 19: Plan mode UI (show plan steps, "switch mode" hint)   (30 min)
Step 20: Full Auto warning banner                             (15 min)
Step 21: Suggestions panel (context-aware)                    (1 hour)
Step 22: CSS + responsive + minimize/expand                   (1.5 hours)
Step 23: App root integration (persist across pages)          (30 min)
--- frontend done ---

=== Polish ===
Step 24: MCP server enhancements (list_all_sessions etc.)    (2 hours)
Step 25: Admin sessions endpoint                              (1 hour)
Step 26: E2E test with real CLI                               (2 hours)
Step 27: Polish: error handling, reconnection, loading        (2 hours)
```

## Security Checklist

- [ ] CLI: `--tools ""` (no filesystem/bash)
- [ ] CLI: `--allowedTools "mcp__*"` (only MCP tools)
- [ ] CLI: `--permission-mode dontAsk` (auto-deny non-MCP)
- [ ] CLI: `env_clear()` (no secret leakage)
- [ ] CLI: only `CLAUDE_CODE_OAUTH_TOKEN` for auth
- [ ] Token: scoped to user's own permissions (not elevated)
- [ ] Token: no boundary (global) — RBAC is the only constraint
- [ ] Token: 4h TTL (auto-expires)
- [ ] MCP config: temp file deleted on cleanup
- [ ] MCP gate: DELETE/DEPLOY tools always require confirmation (except Full Auto)
- [ ] MCP gate: Plan mode returns "denied" for all mutations
- [ ] MCP gate: confirmed=true parameter required to bypass gate
- [ ] MCP gate: session-approved set only for CREATE/UPDATE (never DELETE/DEPLOY)
- [ ] Mode stored in Valkey with session TTL (auto-cleanup)
- [ ] Rate limit: max 5 concurrent manager sessions per user
- [ ] Session limit enforced BEFORE token creation (avoid resource waste)
- [ ] On limit hit: auto-reap oldest stopped/failed session to make room
- [ ] Unknown/unmapped tools default to 'ask' (fail closed), not 'auto'
