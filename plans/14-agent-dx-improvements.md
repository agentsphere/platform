# Agent DX Improvements — Implementation Plan

## Context

The platform has a working agent orchestration system (Phase 07) where Claude Code runs in K8s pods with ephemeral RBAC identities. Agents can clone repos, write code, commit, and push — which auto-triggers pipelines and deployments. However:

1. Agents have **no structured access to platform APIs** (pipelines, deployments, issues, MRs)
2. The **container image is hardcoded** (node-only, no Go/Rust/Python)
3. Agents **can't bootstrap deployment infrastructure** (no `DeployPromote` permission)
4. There are **no preview environments** for branch work

Future agent roles (dev, ops, admin, UI) will need **different tool sets**. The MCP architecture must support **role-based MCP server composition** — each agent role gets a different combination of MCP servers.

---

## MCP Architecture: Multi-Server, Role-Based

### Design: One MCP Server per Domain

Instead of one monolithic MCP server, split into **domain-specific servers** that can be composed per agent role:

```
mcp/
├── servers/
│   ├── platform-core.js      # Project info, git browse, search
│   ├── platform-pipeline.js   # Pipeline CRUD, logs, triggers, artifacts
│   ├── platform-issues.js     # Issues + MRs + comments + reviews
│   ├── platform-deploy.js     # Deployments, rollbacks, previews, ops repos
│   ├── platform-observe.js    # Logs, traces, metrics, alerts
│   └── platform-admin.js      # User mgmt, roles, delegations, platform config
├── lib/
│   └── client.js              # Shared HTTP client (auth, error handling, base URL)
└── package.json               # @modelcontextprotocol/sdk dependency
```

### Role → MCP Server Mapping

| Agent Role | Core | Pipeline | Issues | Deploy | Observe | Admin |
|------------|:----:|:--------:|:------:|:------:|:-------:|:-----:|
| **dev**    |  x   |    x     |   x    |        |         |       |
| **ops**    |  x   |    x     |        |   x    |    x    |       |
| **admin**  |  x   |    x     |   x    |   x    |    x    |   x   |
| **ui**     |  x   |          |   x    |        |         |       |

### How It Works

The entrypoint generates `.mcp.json` dynamically based on an `AGENT_ROLE` env var:

```json
{
  "mcpServers": {
    "platform-core": {
      "command": "node",
      "args": ["/usr/local/lib/mcp/servers/platform-core.js"]
    },
    "platform-pipeline": {
      "command": "node",
      "args": ["/usr/local/lib/mcp/servers/platform-pipeline.js"]
    },
    "platform-issues": {
      "command": "node",
      "args": ["/usr/local/lib/mcp/servers/platform-issues.js"]
    }
  }
}
```

Claude Code supports multiple MCP servers in one config — each server is a separate stdio process, and Claude sees all their tools merged together.

### Why Node.js (not Rust)

| Factor | Node.js | Rust |
|--------|---------|------|
| Already in container | Yes (`node:22-slim`) | Would add ~50MB binary |
| MCP SDK maturity | Official, stable | Official but newer (`rmcp`) |
| Dev iteration speed | Edit JS → restart | Edit → compile → restart |
| Performance need | No (thin API wrappers) | Overkill for HTTP proxying |
| Agent container size | +20MB for SDK | +50MB for binary |
| Startup time | ~500ms per server | ~50ms per server |

**Decision: Node.js for MCP servers.** These are thin REST API wrappers — no CPU-intensive work. The container already has Node. If performance ever becomes a concern (unlikely for stdio MCP), Rust servers can replace individual Node servers without changing the config format.

### Shared Client Library

**`mcp/lib/client.js`** (~50 lines) — all servers import this:

```javascript
// Reads PLATFORM_API_TOKEN, PLATFORM_API_URL, PROJECT_ID from env
// Provides: apiGet(path), apiPost(path, body), apiPatch(path, body), apiDelete(path)
// Handles: Bearer auth, error parsing, JSON response extraction
// Resolves {project_id} placeholders from PROJECT_ID env var
```

---

## Phase A: MCP Server Infrastructure + Core/Pipeline/Issues Servers

### A1. Create shared client library

**New: `mcp/lib/client.js`** (~50 lines)
- Reads `PLATFORM_API_URL`, `PLATFORM_API_TOKEN`, `PROJECT_ID` from env
- Exports `apiGet(path)`, `apiPost(path, body)`, `apiPatch(path, body)`, `apiDelete(path)`
- All methods inject Bearer token, parse JSON responses, throw on HTTP errors

### A2. Create `platform-core` MCP server

**New: `mcp/servers/platform-core.js`** (~80 lines)

Tools:
| Tool | Method | Endpoint |
|------|--------|----------|
| `get_project` | GET | `/api/projects/{id}` |
| `list_projects` | GET | `/api/projects` |

### A3. Create `platform-pipeline` MCP server

**New: `mcp/servers/platform-pipeline.js`** (~150 lines)

Tools:
| Tool | Method | Endpoint |
|------|--------|----------|
| `list_pipelines` | GET | `/api/projects/{id}/pipelines` |
| `get_pipeline` | GET | `/api/projects/{id}/pipelines/{pid}` |
| `get_step_logs` | GET | `/api/projects/{id}/pipelines/{pid}/steps/{sid}/logs` |
| `trigger_pipeline` | POST | `/api/projects/{id}/pipelines` |
| `cancel_pipeline` | POST | `/api/projects/{id}/pipelines/{pid}/cancel` |
| `list_artifacts` | GET | `/api/projects/{id}/pipelines/{pid}/artifacts` |

### A4. Create `platform-issues` MCP server

**New: `mcp/servers/platform-issues.js`** (~180 lines)

Tools:
| Tool | Method | Endpoint |
|------|--------|----------|
| `list_issues` | GET | `/api/projects/{id}/issues` |
| `get_issue` | GET | `/api/projects/{id}/issues/{number}` |
| `create_issue` | POST | `/api/projects/{id}/issues` |
| `update_issue` | PATCH | `/api/projects/{id}/issues/{number}` |
| `add_issue_comment` | POST | `/api/projects/{id}/issues/{number}/comments` |
| `list_merge_requests` | GET | `/api/projects/{id}/merge-requests` |
| `create_merge_request` | POST | `/api/projects/{id}/merge-requests` |
| `get_merge_request` | GET | `/api/projects/{id}/merge-requests/{number}` |

### A5. Create `package.json`

**New: `mcp/package.json`**
```json
{
  "name": "platform-mcp-servers",
  "private": true,
  "type": "module",
  "dependencies": {
    "@modelcontextprotocol/sdk": "^1.x"
  }
}
```

### A6. Update Dockerfile

**Modify: `docker/Dockerfile.claude-runner`**
```dockerfile
COPY --chown=agent:agent mcp/ /usr/local/lib/mcp/
RUN cd /usr/local/lib/mcp && npm install --production
```

### A7. Update entrypoint for role-based MCP config generation

**Modify: `docker/entrypoint.sh`**

```bash
#!/bin/bash
set -euo pipefail
cd /workspace

# Generate MCP config based on agent role
ROLE="${AGENT_ROLE:-dev}"
MCP_DIR="/usr/local/lib/mcp/servers"

# Start with core (always included)
MCP_JSON='{"mcpServers":{"platform-core":{"command":"node","args":["'$MCP_DIR'/platform-core.js"]}'

# Role-based server inclusion
case "$ROLE" in
  dev)
    MCP_JSON+=',"platform-pipeline":{"command":"node","args":["'$MCP_DIR'/platform-pipeline.js"]}'
    MCP_JSON+=',"platform-issues":{"command":"node","args":["'$MCP_DIR'/platform-issues.js"]}'
    ;;
  ops)
    MCP_JSON+=',"platform-pipeline":{"command":"node","args":["'$MCP_DIR'/platform-pipeline.js"]}'
    MCP_JSON+=',"platform-deploy":{"command":"node","args":["'$MCP_DIR'/platform-deploy.js"]}'
    MCP_JSON+=',"platform-observe":{"command":"node","args":["'$MCP_DIR'/platform-observe.js"]}'
    ;;
  admin)
    for server in platform-pipeline platform-issues platform-deploy platform-observe platform-admin; do
      MCP_JSON+=',"'$server'":{"command":"node","args":["'$MCP_DIR'/'$server'.js"]}'
    done
    ;;
  ui)
    MCP_JSON+=',"platform-issues":{"command":"node","args":["'$MCP_DIR'/platform-issues.js"]}'
    ;;
esac

MCP_JSON+='}}'
echo "$MCP_JSON" > /tmp/mcp-config.json

# Run claude with MCP config
claude --output-format stream-json --mcp-config /tmp/mcp-config.json "$@"
EXIT_CODE=$?

# After claude exits, push whatever it did
if [ -n "$(git status --porcelain)" ]; then
  git add -A
  git commit -m "agent session ${SESSION_ID:-unknown}"
  git push origin "${BRANCH:-main}"
fi

exit $EXIT_CODE
```

### A8. Pass PROJECT_ID and AGENT_ROLE to pod

**Modify: `src/agent/claude_code/pod.rs`**
- `build_env_vars()`: add `env_var("PROJECT_ID", &session.project_id.to_string())`
- `build_env_vars()`: add `env_var("AGENT_ROLE", &resolve_role(params))` (default: `"dev"`)

**Modify: `src/agent/provider.rs`** — add to `ProviderConfig`:
```rust
#[serde(default)]
pub role: Option<String>,  // "dev", "ops", "admin", "ui"
```

### Files touched
- New: `mcp/package.json`, `mcp/lib/client.js`, `mcp/servers/platform-core.js`, `mcp/servers/platform-pipeline.js`, `mcp/servers/platform-issues.js`
- Modify: `docker/Dockerfile.claude-runner`, `docker/entrypoint.sh`, `src/agent/claude_code/pod.rs`, `src/agent/provider.rs`
- No migrations

---

## Phase B: Configurable Agent Container Images

Allows projects to specify runtime environments (Go, Rust, Python) instead of the hardcoded `node:22-slim` image.

### B1. Migration: add `agent_image` to projects

**New: `migrations/20260221010001_agent_image_config.up.sql`**
```sql
ALTER TABLE projects ADD COLUMN agent_image TEXT;
```

### B2. Extend ProviderConfig

**Modify: `src/agent/provider.rs`** — add to `ProviderConfig`:
```rust
pub image: Option<String>,
pub setup_commands: Option<Vec<String>>,
```

### B3. Image validation

**Modify: `src/validation.rs`** — add `check_container_image()`: length 1-500, reject shell metacharacters (`;`, `&`, `|`, `$`, backtick, quotes, `\`, newline)

### B4. Pod builder changes

**Modify: `src/agent/claude_code/pod.rs`**
- Add `project_agent_image: Option<&'a str>` to `PodBuildParams`
- `build_main_container()` resolves image: session override > project default > `"platform-claude-runner:latest"`
- If `setup_commands` provided, add a second init container `"setup"` (runs after `git-clone`, before `claude`) using the resolved image, executing commands joined with `&&`

### B5. Thread project image through service layer

**Modify: `src/agent/service.rs`** — in `create_session()`, fetch `agent_image` from project row, pass to `PodBuildParams`

### B6. API validation

**Modify: `src/api/sessions.rs`** — validate `config.image` and `config.setup_commands` (max 20 cmds, each 1-2000 chars)

**Modify: `src/api/projects.rs`** — allow setting `agent_image` on project update (PATCH)

### Files touched
- New: migration `20260221010001_agent_image_config.{up,down}.sql`
- Modify: `src/agent/provider.rs`, `src/agent/claude_code/pod.rs`, `src/agent/service.rs`, `src/api/sessions.rs`, `src/api/projects.rs`, `src/validation.rs`

---

## Phase C: Ops Repo Bootstrapping + Deploy MCP Server

Lets agents create and manage deployments. Adds the deploy and observe MCP servers.

### C1. Extend permission delegation

**Modify: `src/agent/identity.rs`** — `create_agent_identity()` accepts `extra_permissions: &[Permission]` parameter. Default set stays `[ProjectRead, ProjectWrite]`, extras appended. Silently skips permissions the delegator doesn't hold.

### C2. Session creation API gets delegation flags

**Modify: `src/api/sessions.rs`** — add `delegate_deploy: Option<bool>` and `delegate_observe: Option<bool>` to `CreateSessionRequest`. Build extra permissions list: if `delegate_deploy`, add `DeployRead` + `DeployPromote`. Thread through `service.rs` to `identity.rs`.

**Modify: `src/agent/service.rs`** — pass extra permissions to `create_agent_identity()`

### C3. Add POST deployment endpoint

**Modify: `src/api/deployments.rs`** — add `create_deployment()` handler:
- `POST /api/projects/{id}/deployments`
- Body: `{ environment, image_ref, ops_repo_id?, manifest_path?, values_override? }`
- Requires `DeployPromote` permission
- Validates environment is one of `preview`/`staging`/`production`
- Inserts with `desired_status='active'`, `current_status='pending'`
- `UNIQUE(project_id, environment)` constraint returns 409 on conflict

### C4. Create `platform-deploy` MCP server

**New: `mcp/servers/platform-deploy.js`** (~150 lines)

Tools:
| Tool | Method | Endpoint |
|------|--------|----------|
| `list_deployments` | GET | `/api/projects/{id}/deployments` |
| `get_deployment` | GET | `/api/projects/{id}/deployments/{env}` |
| `create_deployment` | POST | `/api/projects/{id}/deployments` |
| `update_deployment` | PATCH | `/api/projects/{id}/deployments/{env}` |
| `rollback_deploy` | POST | `/api/projects/{id}/deployments/{env}/rollback` |
| `get_deploy_history` | GET | `/api/projects/{id}/deployments/{env}/history` |
| `list_previews` | GET | `/api/projects/{id}/previews` |
| `get_preview` | GET | `/api/projects/{id}/previews/{slug}` |

### C5. Create `platform-observe` MCP server

**New: `mcp/servers/platform-observe.js`** (~120 lines)

Tools:
| Tool | Method | Endpoint |
|------|--------|----------|
| `search_logs` | GET | `/api/observe/logs` |
| `get_trace` | GET | `/api/observe/traces/{trace_id}` |
| `query_metrics` | GET | `/api/observe/metrics` |
| `list_alerts` | GET | `/api/observe/alerts` |

### Files touched
- New: `mcp/servers/platform-deploy.js`, `mcp/servers/platform-observe.js`
- Modify: `src/agent/identity.rs`, `src/agent/service.rs`, `src/api/sessions.rs`, `src/api/deployments.rs`
- No new migrations (reuses existing deployments table)

---

## Phase D: Preview Environments per Branch

Auto-creates short-lived preview deployments when pipelines succeed on non-main branches.

### D1. Migration: preview_deployments table

**New: `migrations/20260221010002_preview_deployments.up.sql`**
```sql
CREATE TABLE preview_deployments (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id      UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    branch          TEXT NOT NULL,
    branch_slug     TEXT NOT NULL,
    image_ref       TEXT NOT NULL,
    desired_status  TEXT NOT NULL DEFAULT 'active' CHECK (desired_status IN ('active', 'stopped')),
    current_status  TEXT NOT NULL DEFAULT 'pending' CHECK (current_status IN ('pending', 'syncing', 'healthy', 'degraded', 'failed')),
    ttl_hours       INT NOT NULL DEFAULT 24,
    expires_at      TIMESTAMPTZ NOT NULL DEFAULT now() + INTERVAL '24 hours',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (project_id, branch_slug)
);
```

### D2. Extend pipeline executor

**Modify: `src/pipeline/executor.rs`** — in `detect_and_write_deployment()`:
- Extract branch from `pipeline.git_ref` (strip `refs/heads/`)
- If `main`/`master`: existing production deployment behavior (unchanged)
- If other branch: upsert into `preview_deployments` with `branch_slug = slugify(branch)`, `expires_at = now + 24h`

Add `slugify_branch()` helper: replace `/`, `.`, `_` with `-`, lowercase, truncate to 63 chars.

### D3. Preview reconciler

**New: `src/deployer/preview.rs`** (~150 lines)

Background task (every 15s):
- `reconcile_previews()`: find pending previews, generate Deployment+Service manifest, apply to K8s
- `cleanup_expired()`: find where `expires_at < now()`, scale to 0, delete row

### D4. Cleanup on MR merge

**Modify: `src/api/merge_requests.rs`** — after successful merge, set `desired_status='stopped'` on matching preview_deployment

### D5. Preview API endpoints

**Modify: `src/api/deployments.rs`** — add:
- `GET /api/projects/{id}/previews` — list previews (requires `ProjectRead`)
- `GET /api/projects/{id}/previews/{branch_slug}` — get preview (requires `ProjectRead`)
- `DELETE /api/projects/{id}/previews/{branch_slug}` — stop preview (requires `ProjectWrite`)

### D6. Wire into main.rs

**Modify: `src/main.rs`** — spawn `deployer::preview::run()` background task

### Files touched
- New: migration `20260221010002_preview_deployments.{up,down}.sql`, `src/deployer/preview.rs`
- Modify: `src/pipeline/executor.rs`, `src/deployer/mod.rs`, `src/api/deployments.rs`, `src/api/merge_requests.rs`, `src/main.rs`

---

## Phase E: Admin MCP Server (Future)

**New: `mcp/servers/platform-admin.js`** (~150 lines)

Tools for user management, role assignment, delegation management, platform config. Only loaded for `admin` role agents. Deferred until admin agent role is defined.

---

## Dependency Order

```
Phase A (MCP infra + core/pipeline/issues servers)  ←  no dependencies
Phase B (Custom images) ← no dependencies, parallel with A
Phase C (Deploy bootstrap + deploy/observe servers)  ← depends on A (shared client lib)
Phase D (Preview environments) ← depends on C (deploy endpoint), new migration
Phase E (Admin server) ← depends on A, deferred
```

---

## Verification

### Phase A
- `just ci` passes (pod.rs tests verify `PROJECT_ID`, `AGENT_ROLE` env vars, `--mcp-config` in args)
- Build docker image: `docker build -f docker/Dockerfile.claude-runner -t platform-claude-runner:latest docker/`
- Test each MCP server standalone: `PROJECT_ID=xxx PLATFORM_API_TOKEN=xxx node mcp/servers/platform-core.js`
- Manual test: create agent session with `role: "dev"`, verify Claude gets core + pipeline + issues tools
- Manual test: create session with `role: "ops"`, verify Claude gets core + pipeline + deploy + observe tools

### Phase B
- `just ci` passes (image resolution, validation tests)
- `just db-migrate && just db-prepare` after migration
- Test: `POST /api/projects/{id}/sessions` with `{"config": {"image": "golang:1.23"}}`, verify pod spec

### Phase C
- `just ci` passes (identity delegation, deployment API tests)
- Test: create session with `delegate_deploy: true`, agent can call deploy MCP tools
- Test: session without `delegate_deploy` cannot access deploy tools (403 from API)

### Phase D
- `just db-migrate && just db-prepare` after migration
- `just ci` passes (executor branching, slugify, preview reconciler tests)
- Test: push to feature branch → pipeline succeeds → preview created → reconciler deploys
- Test: merge MR → preview stopped → cleaned up after TTL
