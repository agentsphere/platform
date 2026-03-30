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
// 1. Resolve user's effective permissions (all projects + workspace)
let user_perms = resolver::effective_permissions(pool, valkey, user_id, None).await?;

// 2. Create API token with those permissions as scopes
let scopes: Vec<String> = user_perms.iter().map(|p| p.as_str().to_owned()).collect();

// 3. Token has workspace boundary but NO project boundary
//    — manager needs cross-project access within the workspace
sqlx::query!(
    "INSERT INTO api_tokens (user_id, name, token_hash, scopes, scope_workspace_id, expires_at)
     VALUES ($1, $2, $3, $4, $5, $6)",
    agent_user_id,
    format!("manager-session-{session_id}"),
    token_hash,
    &scopes,
    workspace_id,  // workspace boundary
    Utc::now() + Duration::hours(4),  // 4h lifetime
);
```

**Key difference from agent identity tokens:**
- Agent tokens: project-scoped, role-filtered (role_perms ∩ spawner_perms)
- Manager tokens: workspace-scoped, user-permissions-as-is (no role intersection — the user IS the authority)

### Confirmation implementation

**UI-level gate (MVP approach):**

The UI intercepts `tool_use` events from the SSE stream. Based on the current mode and the tool's action type, it either:
- **auto**: lets the tool execute (no UI interruption)
- **ask**: pauses the stream, shows approval dialog, sends approval/denial message to session
- **deny**: sends a "tool denied in plan mode" message back to the session

The mode + action classification lives entirely in the frontend. MCP servers always execute — the gate is in the UI before the user's approval message reaches the CLI stdin. This keeps MCP servers simple and the confirmation logic in one place.

```typescript
// Frontend: action type classification
const ACTION_TYPE: Record<string, 'READ' | 'CREATE' | 'UPDATE' | 'DELETE' | 'DEPLOY'> = {
  'list_projects': 'READ',
  'get_project': 'READ',
  'query_logs': 'READ',
  // ...
  'create_project': 'CREATE',
  'spawn_agent': 'CREATE',
  // ...
  'update_project': 'UPDATE',
  'toggle_flag': 'UPDATE',
  // ...
  'delete_project': 'DELETE',
  // ...
  'promote_staging': 'DEPLOY',
  'rollback_release': 'DEPLOY',
};

// Mode matrix: what needs confirmation?
const MODE_MATRIX: Record<Mode, Record<ActionType, 'auto' | 'ask' | 'deny'>> = {
  plan:       { READ: 'auto', CREATE: 'deny',  UPDATE: 'deny',  DELETE: 'deny',  DEPLOY: 'deny'  },
  guided:     { READ: 'auto', CREATE: 'ask',   UPDATE: 'ask',   DELETE: 'ask',   DEPLOY: 'ask'   },
  auto_read:  { READ: 'auto', CREATE: 'ask',   UPDATE: 'ask',   DELETE: 'ask',   DEPLOY: 'ask'   },
  auto_write: { READ: 'auto', CREATE: 'auto',  UPDATE: 'auto',  DELETE: 'ask',   DEPLOY: 'ask'   },
  full_auto:  { READ: 'auto', CREATE: 'auto',  UPDATE: 'auto',  DELETE: 'auto',  DEPLOY: 'auto'  },
};
```

**Future: MCP-level gate (Option A):**

When we want the confirmation to be part of the conversation context (so Claude can reason about denied actions, adjust its plan, etc.), move the gate into the MCP servers. Each tool checks a `confirmation_mode` header and returns `{ status: "confirmation_required", ... }` instead of executing. Claude then asks the user explicitly and re-calls with `{ confirmed: true }`.

This is more work but gives better conversation quality. Implement after MVP is stable.

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

### 4.3 Existing tests to update

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
Step 1:  Migration + config (mcp_servers_path)            (30 min)
Step 2:  CliSpawnOptions (mcp_config_path, disable_tools)  (30 min)
Step 3:  build_manager_mcp_config()                        (30 min)
Step 4:  create_manager_session() + cleanup                (2 hours)
Step 5:  manager_prompt.rs                                 (30 min)
Step 6:  API endpoints (/api/manager/*)                    (1.5 hours)
Step 7:  Mock CLI update (MCP mode)                        (30 min)
Step 8:  Unit + integration tests                          (2 hours)
--- backend done, test with curl ---

=== Frontend ===
Step 9:  ManagerChat.tsx (single session, basic messages)  (2 hours)
Step 10: SSE integration + NDJSON parsing                  (1.5 hours)
Step 11: Multi-session tab bar + localStorage persist      (1.5 hours)
Step 12: Mode selector dropdown (5 modes)                  (1 hour)
Step 13: Action classifier (tool → READ/CREATE/UPDATE/     (1 hour)
         DELETE/DEPLOY) + mode matrix
Step 14: Confirmation dialog (approve/approve-session/deny)(1.5 hours)
Step 15: Plan mode (deny mutations, show planned steps)    (1 hour)
Step 16: Full Auto warning banner                          (15 min)
Step 17: Suggestions panel (context-aware)                 (1 hour)
Step 18: CSS + responsive + minimize/expand                (1.5 hours)
Step 19: App root integration (persist across pages)       (30 min)
--- frontend done ---

=== MCP + Polish ===
Step 20: MCP server enhancements (list_all_sessions etc.)  (2 hours)
Step 21: Admin sessions endpoint                           (1 hour)
Step 22: E2E test with real CLI                            (2 hours)
Step 23: Polish: error handling, reconnection, loading     (2 hours)
```

## Security Checklist

- [ ] CLI: `--tools ""` (no filesystem/bash)
- [ ] CLI: `--allowedTools "mcp__*"` (only MCP tools)
- [ ] CLI: `--permission-mode dontAsk` (auto-deny non-MCP)
- [ ] CLI: `env_clear()` (no secret leakage)
- [ ] CLI: only `CLAUDE_CODE_OAUTH_TOKEN` for auth
- [ ] Token: scoped to user's permissions (not elevated)
- [ ] Token: workspace boundary (no cross-workspace)
- [ ] Token: 4h TTL (auto-expires)
- [ ] MCP config: temp file deleted on cleanup
- [ ] UI: Tier 2 tools always require confirmation
- [ ] UI: Tier 1 tools require first-time confirmation
- [ ] Rate limit: max 5 concurrent manager sessions per user
