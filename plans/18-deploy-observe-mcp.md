# Plan 18 — Deploy & Observe MCP Servers

## Overview

Create the `platform-deploy` and `platform-observe` MCP servers so that agents in the `ops` and `admin` roles can interact with deployments and observability data. Currently, agents have no structured access to deployment management (create, rollback, history) or observability (logs, traces, metrics, alerts). The backend APIs already exist — this plan creates the MCP server wrappers and extends the agent identity system to delegate deploy/observe permissions.

**This corresponds to Agent DX Phase C from Plan 14.**

---

## Motivation

- **Ops agents are blind**: An `ops`-role agent currently gets `core` + `pipeline` MCP tools but has no way to check deployment status, read logs, query traces, or trigger rollbacks
- **Admin agents are incomplete**: The `admin` role should have access to all platform capabilities, but deploy and observe tools are missing
- **APIs already exist**: `src/api/deployments.rs` has full CRUD + history + ops repos; `src/observe/query.rs` has log search, trace listing, metric queries, and alert management. Only the MCP wrappers are needed
- **Permission gap**: Agent identity creation only delegates `ProjectRead` + `ProjectWrite`. Deploy-capable agents need `DeployRead` + `DeployPromote` delegated

---

## Prerequisites

| Requirement | Status |
|---|---|
| MCP infrastructure (lib/client.js, package.json) | Complete (Phase A) |
| platform-core.js, platform-pipeline.js, platform-issues.js | Complete (Phase A) |
| Deployment API (`src/api/deployments.rs`) | Complete |
| Observability query API (`src/observe/query.rs`) | Complete |
| Role-based entrypoint (`docker/entrypoint.sh`) | Complete |

---

## Architecture

### MCP Server Composition (Updated Role Matrix)

| Agent Role | Core | Pipeline | Issues | Deploy | Observe | Admin |
|:----------:|:----:|:--------:|:------:|:------:|:-------:|:-----:|
| **dev**    |  x   |    x     |   x    |        |         |       |
| **ops**    |  x   |    x     |        |   x    |    x    |       |
| **admin**  |  x   |    x     |   x    |   x    |    x    |   x   |
| **ui**     |  x   |          |   x    |        |         |       |

The entrypoint already generates MCP config based on `AGENT_ROLE`. The deploy and observe servers are referenced in the `ops` and `admin` case branches but the files don't exist yet.

### Permission Delegation Extension

Currently `create_agent_identity()` in `src/agent/identity.rs` delegates exactly two permissions:
- `ProjectRead`
- `ProjectWrite`

Both are unconditionally attempted (silently skipped if delegator lacks them).

After this plan, the delegation set becomes configurable:
- **Base set** (always): `ProjectRead`, `ProjectWrite`
- **Deploy set** (opt-in): `DeployRead`, `DeployPromote`
- **Observe set** (opt-in): `ObserveRead` (if such permission exists, else `ProjectRead` suffices for observe endpoints)

---

## Detailed Implementation

### Step C1: `mcp/servers/platform-deploy.js` (~180 lines)

**New file.** Wraps the deployment management API.

#### Tools

| # | Tool Name | Method | API Endpoint | Parameters |
|---|-----------|--------|-------------|------------|
| 1 | `list_deployments` | GET | `/api/projects/{project_id}/deployments` | `project_id?`, `limit?`, `offset?` |
| 2 | `get_deployment` | GET | `/api/projects/{project_id}/deployments/{environment}` | `project_id?`, `environment` (required) |
| 3 | `create_deployment` | POST | `/api/projects/{project_id}/deployments` | `environment` (required), `image_ref` (required), `ops_repo_id?`, `manifest_path?`, `values_override?` |
| 4 | `update_deployment` | PATCH | `/api/projects/{project_id}/deployments/{environment}` | `environment` (required), `image_ref?`, `desired_status?`, `values_override?` |
| 5 | `rollback_deployment` | POST | `/api/projects/{project_id}/deployments/{environment}/rollback` | `environment` (required) |
| 6 | `get_deployment_history` | GET | `/api/projects/{project_id}/deployments/{environment}/history` | `environment` (required), `limit?`, `offset?` |
| 7 | `list_previews` | GET | `/api/projects/{project_id}/previews` | `project_id?`, `limit?`, `offset?` |
| 8 | `get_preview` | GET | `/api/projects/{project_id}/previews/{slug}` | `slug` (required) |

#### Tool Descriptions (for Claude's tool selection)

```javascript
{
  name: "list_deployments",
  description: "List all deployments for the current project. Shows environment, status, image, and deploy timestamp.",
  inputSchema: {
    type: "object",
    properties: {
      project_id: { type: "string", description: "Project UUID (defaults to current project)" },
      limit: { type: "number", description: "Max results (default 50, max 100)" },
      offset: { type: "number", description: "Offset for pagination" }
    }
  }
}
```

Each tool follows the same pattern as existing MCP servers:
1. Extract parameters from input
2. Call `apiGet`/`apiPost`/`apiPatch` from `lib/client.js`
3. Return `{ content: [{ type: "text", text: JSON.stringify(result, null, 2) }] }`

#### Error Handling

```javascript
async function handleTool(name, args) {
  try {
    // ... tool logic ...
  } catch (err) {
    return {
      content: [{ type: "text", text: `Error: ${err.message}` }],
      isError: true,
    };
  }
}
```

#### Server Setup Pattern (matching existing servers)

```javascript
import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { apiGet, apiPost, apiPatch, PROJECT_ID } from "../lib/client.js";

const server = new Server(
  { name: "platform-deploy", version: "1.0.0" },
  { capabilities: { tools: {} } }
);

server.setRequestHandler(ListToolsRequestSchema, async () => ({
  tools: [ /* ... tool definitions ... */ ]
}));

server.setRequestHandler(CallToolRequestSchema, async (request) => {
  return handleTool(request.params.name, request.params.arguments ?? {});
});

const transport = new StdioServerTransport();
await server.connect(transport);
```

---

### Step C2: `mcp/servers/platform-observe.js` (~150 lines)

**New file.** Wraps the observability query API.

#### Tools

| # | Tool Name | Method | API Endpoint | Parameters |
|---|-----------|--------|-------------|------------|
| 1 | `search_logs` | GET | `/api/observe/logs` | `project_id?`, `session_id?`, `trace_id?`, `level?`, `service?`, `q?` (full-text), `from?`, `to?`, `limit?`, `offset?` |
| 2 | `get_trace` | GET | `/api/observe/traces/{trace_id}` | `trace_id` (required) |
| 3 | `list_traces` | GET | `/api/observe/traces` | `project_id?`, `session_id?`, `service?`, `status?`, `from?`, `to?`, `limit?`, `offset?` |
| 4 | `query_metrics` | GET | `/api/observe/metrics` | `name?`, `labels?`, `project_id?`, `from?`, `to?` |
| 5 | `list_metric_names` | GET | `/api/observe/metrics/names` | `project_id?` |
| 6 | `list_alerts` | GET | `/api/observe/alerts` | `project_id?`, `status?`, `limit?`, `offset?` |
| 7 | `get_alert` | GET | `/api/observe/alerts/{id}` | `alert_id` (required) |

#### Tool Descriptions

```javascript
{
  name: "search_logs",
  description: "Search application logs. Filter by project, session, severity level, service name, or full-text query. Returns structured log entries with timestamps, trace IDs, and attributes.",
  inputSchema: {
    type: "object",
    properties: {
      project_id: { type: "string", description: "Filter by project UUID" },
      session_id: { type: "string", description: "Filter by agent session UUID" },
      level: { type: "string", enum: ["trace", "debug", "info", "warn", "error", "fatal"], description: "Minimum log level" },
      q: { type: "string", description: "Full-text search query" },
      from: { type: "string", description: "Start time (ISO 8601)" },
      to: { type: "string", description: "End time (ISO 8601)" },
      limit: { type: "number", description: "Max results (default 50, max 100)" }
    }
  }
}
```

```javascript
{
  name: "get_trace",
  description: "Get a distributed trace by ID, including all spans with timing, attributes, and events. Shows the full request flow across services.",
  inputSchema: {
    type: "object",
    properties: {
      trace_id: { type: "string", description: "Trace ID (32-char hex)" }
    },
    required: ["trace_id"]
  }
}
```

#### Log Search Result Formatting

The observe server should format results for readability:

```javascript
async function handleSearchLogs(args) {
  const params = {};
  if (args.project_id) params.project_id = args.project_id;
  if (args.session_id) params.session_id = args.session_id;
  if (args.level) params.level = args.level;
  if (args.q) params.q = args.q;
  if (args.from) params.from = args.from;
  if (args.to) params.to = args.to;
  params.limit = args.limit || 50;
  params.offset = args.offset || 0;

  const result = await apiGet("/api/observe/logs", { query: params });
  return {
    content: [{
      type: "text",
      text: JSON.stringify(result, null, 2)
    }]
  };
}
```

---

### Step C3: Extend Agent Identity — Permission Delegation

**Modify: `src/agent/identity.rs`**

Current signature:
```rust
pub async fn create_agent_identity(
    pool: &PgPool,
    valkey: &fred::clients::Pool,
    session_id: Uuid,
    delegator_id: Uuid,
    project_id: Uuid,
) -> Result<AgentIdentity, anyhow::Error>
```

New signature:
```rust
pub async fn create_agent_identity(
    pool: &PgPool,
    valkey: &fred::clients::Pool,
    session_id: Uuid,
    delegator_id: Uuid,
    project_id: Uuid,
    extra_permissions: &[Permission],
) -> Result<AgentIdentity, anyhow::Error>
```

#### Implementation Changes

```rust
// Base permissions (always delegated)
let base_permissions = [Permission::ProjectRead, Permission::ProjectWrite];

// Combine base + extra
let all_permissions: Vec<_> = base_permissions
    .iter()
    .chain(extra_permissions.iter())
    .collect();

// Delegate each permission (silently skip if delegator lacks it)
for permission in &all_permissions {
    let delegator_has = resolver::has_permission(
        pool, valkey, delegator_id, Some(project_id), **permission
    ).await?;

    if delegator_has {
        delegation::create_delegation(pool, valkey, CreateDelegationParams {
            delegator_id,
            delegate_id: agent_user_id,
            permission: *permission,
            project_id: Some(project_id),
            expires_at: Some(expires_at),
            reason: Some(format!("agent session {}", session_id)),
        }).await?;
    }
}
```

---

### Step C4: Session Creation API — Delegation Flags

**Modify: `src/api/sessions.rs`**

#### Extend `CreateSessionRequest`

```rust
#[derive(Debug, serde::Deserialize)]
pub struct CreateSessionRequest {
    pub prompt: String,
    pub provider: Option<String>,
    pub branch: Option<String>,
    pub config: Option<ProviderConfig>,
    #[serde(default)]
    pub delegate_deploy: bool,   // NEW
    #[serde(default)]
    pub delegate_observe: bool,  // NEW
}
```

#### Build Extra Permissions

```rust
let mut extra_permissions = Vec::new();
if body.delegate_deploy {
    extra_permissions.push(Permission::DeployRead);
    extra_permissions.push(Permission::DeployPromote);
}
if body.delegate_observe {
    // Observe endpoints use ProjectRead — no extra permission needed
    // But if ObserveRead exists as a separate permission, add it here
}
```

**Modify: `src/agent/service.rs`** — Pass `extra_permissions` to `create_agent_identity()`:

```rust
let identity = create_agent_identity(
    &state.pool,
    &state.valkey,
    session.id,
    user_id,
    project_id,
    &extra_permissions,
).await?;
```

---

### Step C5: Verify Entrypoint Configuration

**File: `docker/entrypoint.sh`** — Already configured correctly.

The existing entrypoint has:
```bash
ops)
    MCP_JSON+=',"platform-pipeline":...'
    MCP_JSON+=',"platform-deploy":...'
    MCP_JSON+=',"platform-observe":...'
    ;;
admin)
    for server in platform-pipeline platform-issues platform-deploy platform-observe platform-admin; do
      ...
    done
    ;;
```

This references `platform-deploy.js` and `platform-observe.js` — which will now exist after steps C1 and C2.

---

### Step C6: Unit Tests

**Add to `src/agent/identity.rs` tests:**

```rust
#[cfg(test)]
mod tests {
    #[test]
    fn extra_permissions_combined_with_base() {
        // Verify that extra_permissions are appended to base set
    }

    #[test]
    fn empty_extra_permissions_uses_base_only() {
        // Verify backward compatibility — empty extras = base permissions only
    }
}
```

**MCP server tests** (manual or via Node.js test runner):

```bash
# Test each server standalone (requires running platform API)
PROJECT_ID=<uuid> PLATFORM_API_TOKEN=<token> PLATFORM_API_URL=http://localhost:8080 \
  node mcp/servers/platform-deploy.js
# Verify tool list returned on ListTools request

PROJECT_ID=<uuid> PLATFORM_API_TOKEN=<token> PLATFORM_API_URL=http://localhost:8080 \
  node mcp/servers/platform-observe.js
# Verify tool list returned on ListTools request
```

---

## Files Changed

| File | Action | Description |
|------|--------|-------------|
| `mcp/servers/platform-deploy.js` | **New** | Deploy MCP server (8 tools, ~180 lines) |
| `mcp/servers/platform-observe.js` | **New** | Observe MCP server (7 tools, ~150 lines) |
| `src/agent/identity.rs` | **Modify** | Add `extra_permissions` parameter to `create_agent_identity()` |
| `src/agent/service.rs` | **Modify** | Pass extra permissions from session request |
| `src/api/sessions.rs` | **Modify** | Add `delegate_deploy` and `delegate_observe` fields to `CreateSessionRequest` |

No new migrations. No new Rust dependencies.

---

## Verification

### Automated
1. `just ci` passes (identity tests, service tests)
2. `just lint` — no clippy warnings
3. MCP servers load without errors: `node mcp/servers/platform-deploy.js` (exits cleanly when stdin closes)

### Manual Testing
1. Create agent session with `delegate_deploy: true`:
   ```bash
   curl -X POST /api/projects/{id}/sessions \
     -H "Authorization: Bearer $TOKEN" \
     -d '{"prompt":"check deployment status","config":{"role":"ops"},"delegate_deploy":true}'
   ```
2. Verify agent pod has `AGENT_ROLE=ops` and MCP config includes `platform-deploy` and `platform-observe`
3. Verify agent can call `list_deployments` tool
4. Verify agent **without** `delegate_deploy` gets 403 when calling deploy API

### Integration Test (after Plan 17)
```rust
#[sqlx::test(migrations = "migrations")]
async fn session_with_deploy_delegation(pool: PgPool) {
    // Create session with delegate_deploy: true
    // Verify delegations table has DeployRead + DeployPromote entries
    // Verify agent token can access deployment endpoints
}
```

---

## Risks and Mitigations

| Risk | Impact | Mitigation |
|------|--------|-----------|
| Deploy permission doesn't exist yet | Delegation silently skipped | Verify Permission enum includes `DeployRead`/`DeployPromote` variants |
| Observe endpoints require different permission | 403 on log/trace queries | Observe endpoints currently use `ProjectRead` — no extra delegation needed |
| MCP SDK version mismatch | Server won't start | Pin version in `mcp/package.json` (currently `^1.12.1`) |
| Node.js not in custom agent images | MCP servers won't load | MCP servers run in the agent container (always has Node) — custom images need Node or MCP must be sidecar |

---

## Estimated Scope

| Metric | Value |
|--------|-------|
| New files | 2 (JS) |
| Modified files | 3 (Rust) |
| New migrations | 0 |
| Estimated LOC | ~600 (330 JS + 270 Rust changes) |
| New MCP tools | 15 (8 deploy + 7 observe) |
