# Plan 31 (Superseded) → Plan 32: Permission Model Redesign

**This plan has been superseded.** The original Plan 31 (per-user agents + MCP fixes) has been replaced with a comprehensive permission model redesign.

See the full plan below.

---

# Plan 32: Permission Model Redesign — Secure Bootstrap, Agent Isolation, Scoped Roles

## Context

The permission/agent system has grown organically across 25 plans and is getting hard to reason about. Key problems:

1. **Insecure bootstrap**: `admin/admin@localhost` with password "admin" is created automatically — no setup flow
2. **No hard agent isolation**: Agents get permissions via delegation, but nothing prevents an agent token from being used to access resources outside its intended scope
3. **Plan 31 proposed 4 permanent agent users per human** — wasteful and complex. Ephemeral is fine; it just needs proper scoping
4. **Delegation-as-agent-permission is overcomplex**: Creating delegation rows per agent session, then revoking them on cleanup, is a lot of ceremony for what's really "give this ephemeral session a predefined capability set"
5. **`api_tokens.project_id` already exists** but is completely unused in the auth middleware — a missed opportunity for hard scope enforcement

This plan replaces Plan 31 with a cleaner architecture. Plan 31's bug fixes (SQL `username`→`name`, missing MCP tools, MCP tests) are split into a separate prerequisite PR.

### Core Design Principles

- **Role = capabilities, Scope = boundaries**: An agent role defines WHAT it can do; the scope defines WHERE
- **Unified role system**: Agent roles are regular DB roles, same `role_permissions` mechanism as human roles
- **Least privilege, always scoped**: Every agent token is hard-bounded to a workspace/project
- **No permanent agent users**: Ephemeral sessions with scoped tokens. No `owner_id`, no `ensure_user_agents()`
- **Human chain of custody**: All agent actions trace back to a human principal (`agent_sessions.user_id`)
- **Workspace = isolation boundary**: Every project belongs to a workspace. Workspace membership gates access

---

## PR 1: Plan 31 Bug Fixes + Missing MCP Tools + MCP Tests (prerequisite)

_Kept from original Plan 31 PR 1, unchanged. Unblocks agent flow immediately._

### Changes

| File | Change |
|---|---|
| `src/agent/inprocess.rs` | Fix `SELECT username` → `SELECT name` (lines 385, 482) |
| `mcp/servers/platform-core.js` | Add `create_project`, `update_project`, `delete_project`, `get_session`, `send_message_to_session` tools |
| `mcp/servers/platform-issues.js` | Add `merge_mr` tool |
| `docker/entrypoint.sh` | Rename `create-app` → `manager` role, add `test` case, add `platform-observe` to manager |
| `src/agent/provider.rs` | `VALID_ROLES` validation + `create-app` alias → `manager` |
| `src/agent/inprocess.rs` | Update system prompt to reference "manager" |
| `mcp/tests/helpers.js` | New: MockApiServer + McpTestClient test harness |
| `mcp/tests/test-*.js` | New: 6 test files (core, admin, issues, pipeline, deploy, observe) |
| `Justfile` | Add `test-mcp`, update `ci` |
| `CLAUDE.md` | Document new tools + roles |

### TDD Test Plan — PR 1

#### Tests to write FIRST (before implementation)

**Unit tests — `src/agent/provider.rs` (add to existing `#[cfg(test)] mod tests`)**

| Test | Validates | Layer |
|---|---|---|
| `valid_roles_contains_manager` | `VALID_ROLES` includes "manager" | Unit |
| `valid_roles_contains_create_app_alias` | `VALID_ROLES` includes "create-app" for backward compat | Unit |
| `valid_roles_contains_dev_ops_test_review` | All 5 agent role strings accepted | Unit |
| `valid_roles_rejects_unknown` | "unknown-role" not in `VALID_ROLES` | Unit |
| `create_app_alias_resolves_to_manager` | Role alias resolution maps "create-app" → "manager" | Unit |

**Unit tests — `src/agent/inprocess.rs` (add to existing `#[cfg(test)] mod tests`)**

| Test | Validates | Layer |
|---|---|---|
| `system_prompt_references_manager_role` | CREATE_APP_SYSTEM_PROMPT contains "manager" not "create-app" | Unit |

**MCP tests — `mcp/tests/test-core.js` (new)**

| Test | Validates | Layer |
|---|---|---|
| `list_tools_returns_all_core_tools` | Tool listing includes new tools | MCP |
| `create_project_sends_correct_request` | create_project tool sends POST /api/projects | MCP |
| `update_project_sends_correct_request` | update_project sends PUT /api/projects/:id | MCP |
| `delete_project_sends_correct_request` | delete_project sends DELETE /api/projects/:id | MCP |
| `get_session_sends_correct_request` | get_session sends GET /api/sessions/:id | MCP |
| `send_message_to_session_sends_correct_request` | sends POST /api/sessions/:id/messages | MCP |
| `create_project_handles_error_response` | 400/500 responses propagated correctly | MCP |

**MCP tests — `mcp/tests/test-issues.js` (new)**

| Test | Validates | Layer |
|---|---|---|
| `merge_mr_sends_correct_request` | merge_mr sends POST /api/projects/:id/merge-requests/:num/merge | MCP |
| `merge_mr_handles_conflict` | 409 (merge conflict) propagated correctly | MCP |

**MCP tests — `mcp/tests/test-admin.js`, `test-pipeline.js`, `test-deploy.js`, `test-observe.js` (new)**

| Test (per server) | Validates | Layer |
|---|---|---|
| `list_tools_returns_expected_tools` | Each server exposes correct tool set | MCP |
| `tool_sends_correct_http_method_and_path` | Each tool maps to correct API endpoint | MCP |
| `tool_handles_error_responses` | Error propagation works | MCP |

Total: ~5 unit + ~30 MCP tests

#### Existing tests to UPDATE

| Test file | Change | Reason |
|---|---|---|
| `tests/session_integration.rs::create_session_invalid_provider` | No change needed | Provider length check unchanged |
| `tests/create_app_integration.rs::*` | Verify `SELECT name` fix resolves any prior failures | SQL column rename |

#### Tests NOT needed

- No new integration tests required — PR 1 is bug fixes + MCP tooling only
- Existing 15 `inprocess.rs` unit tests already cover system prompt structure

### Verification
- `just ci` passes
- `just test-mcp` passes (NEW)
- `AGENT_ROLE=manager` generates correct MCP config
- `AGENT_ROLE=create-app` still works (backward compat)

---

## PR 2: Mandatory Workspaces

Make `workspace_id` NOT NULL on projects. This is the foundation for workspace-as-isolation-boundary.

### Migration: `{next}_mandatory_workspaces`

**Up:**
```sql
-- 1. Create personal workspace for each user who owns projects without a workspace
INSERT INTO workspaces (id, name, display_name, description, owner_id)
SELECT gen_random_uuid(),
       u.name || '-personal',
       u.display_name || '''s workspace',
       'Auto-created personal workspace',
       u.id
FROM users u
WHERE u.user_type = 'human'
  AND u.is_active = true
  AND NOT EXISTS (
    SELECT 1 FROM workspaces w WHERE w.owner_id = u.id AND w.is_active = true
  )
  AND EXISTS (
    SELECT 1 FROM projects p WHERE p.owner_id = u.id AND p.workspace_id IS NULL AND p.is_active = true
  );

-- 2. Add workspace owners as members
INSERT INTO workspace_members (id, workspace_id, user_id, role)
SELECT gen_random_uuid(), w.id, w.owner_id, 'owner'
FROM workspaces w
WHERE NOT EXISTS (
    SELECT 1 FROM workspace_members wm WHERE wm.workspace_id = w.id AND wm.user_id = w.owner_id
);

-- 3. Assign orphan projects to their owner's personal workspace
UPDATE projects p
SET workspace_id = (
    SELECT w.id FROM workspaces w
    WHERE w.owner_id = p.owner_id AND w.is_active = true
    ORDER BY w.created_at LIMIT 1
)
WHERE p.workspace_id IS NULL AND p.is_active = true;

-- 4. Make workspace_id NOT NULL
ALTER TABLE projects ALTER COLUMN workspace_id SET NOT NULL;
```

**Down:**
```sql
ALTER TABLE projects ALTER COLUMN workspace_id DROP NOT NULL;
```

### Code Changes

| File | Change |
|---|---|
| `src/api/projects.rs` | `CreateProjectRequest.workspace_id` becomes required (not `Option`). If not provided, use user's default workspace |
| `src/api/projects.rs` | `create_project` handler: auto-assign to user's default workspace if `workspace_id` is None |
| `src/workspace/service.rs` | Add `get_or_create_default_workspace(pool, user_id, username)` — idempotent |
| `src/store/bootstrap.rs` | After creating admin user, create admin's personal workspace + add as owner member |
| `src/api/admin.rs` | After `create_user`, call `get_or_create_default_workspace` |

### TDD Test Plan — PR 2

#### Tests to write FIRST (before implementation)

**Unit tests — `src/workspace/service.rs` (new `#[cfg(test)] mod tests` block)**

_Note: `get_or_create_default_workspace` is async + DB, so pure unit tests are limited. The core logic is "if workspace exists, return it; else create it." Test the naming convention:_

| Test | Validates | Layer |
|---|---|---|
| `default_workspace_name_format` | Helper fn produces `"{username}-personal"` name | Unit |
| `default_workspace_display_name_format` | Produces `"{display_name}'s workspace"` | Unit |

**Integration tests — `tests/workspace_integration.rs` (add to existing file)**

| Test | Validates | Layer |
|---|---|---|
| `get_or_create_default_workspace_creates_new` | First call creates workspace + owner membership | Integration |
| `get_or_create_default_workspace_idempotent` | Second call returns same workspace ID | Integration |
| `get_or_create_default_workspace_adds_owner_member` | Created workspace has owner as member with "owner" role | Integration |

**Integration tests — `tests/project_integration.rs` (add to existing file)**

| Test | Validates | Layer |
|---|---|---|
| `create_project_without_workspace_id_uses_default` | Omitting workspace_id auto-assigns to user's default workspace | Integration |
| `create_project_with_explicit_workspace_id` | Providing workspace_id uses that workspace | Integration |
| `create_project_workspace_id_not_null_in_response` | Response always includes non-null workspace_id | Integration |

**Integration tests — `tests/admin_integration.rs` (add to existing file)**

| Test | Validates | Layer |
|---|---|---|
| `admin_create_user_creates_default_workspace` | Creating a user auto-creates their personal workspace | Integration |
| `admin_create_user_default_workspace_has_owner_member` | User is owner member of their personal workspace | Integration |

**Integration tests — `tests/project_integration.rs` (regression)**

| Test | Validates | Layer |
|---|---|---|
| `all_projects_have_workspace_id` | After migration, no project has NULL workspace_id (query check) | Integration |

Total: ~2 unit + ~8 integration tests

#### Existing tests to UPDATE

| Test file | Change | Reason |
|---|---|---|
| `tests/project_integration.rs::create_project` | May need to supply workspace_id or verify auto-assignment | workspace_id now NOT NULL |
| `tests/project_integration.rs::create_project_with_visibility` | Same — verify workspace_id in response | workspace_id now NOT NULL |
| `tests/session_integration.rs::*` (all tests that create projects) | Projects created via helper must have workspace_id | migration makes it NOT NULL |
| `tests/helpers/mod.rs` (test helpers) | `create_test_project()` helper must provide workspace_id or rely on auto-assignment | All test projects need a workspace |
| `tests/e2e_helpers/mod.rs` | Same — E2E project creation helpers | workspace_id NOT NULL |
| `tests/agent_spawn_integration.rs::*` | Project creation in test setup needs workspace_id | Cascading from helpers change |
| `tests/create_app_integration.rs::*` | Same | Cascading from helpers change |
| `tests/issue_mr_integration.rs::*` | Same | Cascading from helpers change |
| `tests/pipeline_integration.rs::*` | Same | Cascading from helpers change |
| `tests/deployment_integration.rs::*` | Same | Cascading from helpers change |
| `tests/secrets_integration.rs::*` | Same | Cascading from helpers change |
| `tests/webhook_integration.rs::*` | Same | Cascading from helpers change |
| `tests/git_smart_http_integration.rs::*` | Same | Cascading from helpers change |
| `tests/e2e_*.rs` (all E2E test files) | Same — E2E setup creates projects | Cascading from helpers change |

**Strategy for cascading updates**: Update `create_test_project()` in `tests/helpers/mod.rs` and `tests/e2e_helpers/mod.rs` to auto-create a default workspace first. This single change fixes all downstream tests. No individual test file changes needed beyond the helper.

#### Tests NOT needed

- No E2E tests — workspace assignment is API-level logic, fully testable at integration tier
- No unit tests for migration SQL — validated by `just db-migrate` + integration tests

### Verification
- `just db-migrate` applies cleanly
- All existing projects get assigned to a workspace
- Creating a project without `workspace_id` auto-assigns to default workspace
- `just test-unit` and `just test-integration` pass

---

## PR 3: Secure Bootstrap (Setup Token Flow)

Replace hardcoded `admin/admin` with a one-time setup token.

### Migration: `{next}_setup_tokens`

```sql
CREATE TABLE setup_tokens (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    token_hash TEXT NOT NULL,
    used_at    TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL
);
```

### Code Changes

| File | Change |
|---|---|
| `src/store/bootstrap.rs` | **Rewrite**: Split into two paths: (a) always seed permissions + roles, (b) if no users exist AND `PLATFORM_DEV=true` → create admin/admin as today (dev convenience), (c) if no users exist AND production → generate setup token, print to stdout, store hashed in `setup_tokens`, optionally store as K8s secret `platform-setup-token` in the platform namespace |
| `src/api/setup.rs` | **New file**: `POST /api/setup` endpoint — accepts `{token, name, email, password}`, validates token hash, creates first admin user with `admin` role, creates personal workspace, consumes token (sets `used_at`). Optional: `passkey_registration` field to register a passkey during setup |
| `src/api/mod.rs` | Mount `/api/setup` route (no auth required, but only works when 0 users exist) |
| `ui/src/pages/Setup.tsx` | **New file**: Setup page with form: token input, name, email, password, confirm password. Shown when navigating to `/setup` or auto-redirected from login when no users exist |
| `ui/src/app.tsx` | Add `/setup` route |
| `src/main.rs` | If setup token was generated, log the token value at WARN level with instructions |

### Security Considerations
- Setup token has 1-hour TTL
- Token is SHA-256 hashed before storage (same as API tokens)
- `POST /api/setup` rate-limited (3 attempts per 5 minutes)
- Endpoint returns 404 when users already exist (no information leak)
- If token expires unused, next restart generates a new one
- `PLATFORM_DEV=true` bypasses setup flow entirely (keeps current admin/admin for dev)

### Helm Integration
- Helm chart `NOTES.txt` reads the K8s secret and prints: `"Open https://{{ .Values.ingress.host }}/setup and enter your setup token: $(kubectl get secret platform-setup-token -o jsonpath='{.data.token}' | base64 -d)"`

### TDD Test Plan — PR 3

#### Tests to write FIRST (before implementation)

**Unit tests — `src/store/bootstrap.rs` (new `#[cfg(test)] mod tests` block)**

| Test | Validates | Layer |
|---|---|---|
| `setup_token_has_one_hour_ttl` | Generated token expiry = now + 1h | Unit |
| `setup_token_is_sha256_hashed` | Stored hash != raw token, matches SHA-256 of raw | Unit |
| `setup_token_format_is_printable` | Raw token is hex-encoded, 64 chars (32 bytes) | Unit |

**Unit tests — `src/api/setup.rs` (new `#[cfg(test)] mod tests` block)**

| Test | Validates | Layer |
|---|---|---|
| `setup_request_validation_empty_name` | Empty name rejected (400) | Unit |
| `setup_request_validation_empty_email` | Empty email rejected (400) | Unit |
| `setup_request_validation_short_password` | Password < 8 chars rejected (400) | Unit |
| `setup_request_validation_email_format` | Invalid email format rejected (400) | Unit |
| `setup_request_validation_name_too_long` | Name > 255 chars rejected (400) | Unit |
| `setup_request_validation_password_too_long` | Password > 1024 chars rejected (400) | Unit |

**Integration tests — `tests/setup_integration.rs` (new file)**

| Test | Validates | Layer |
|---|---|---|
| `setup_with_valid_token_creates_admin` | POST /api/setup with correct token → 200, admin user exists | Integration |
| `setup_creates_personal_workspace` | Setup creates admin's personal workspace + owner membership | Integration |
| `setup_assigns_admin_role` | Created user has admin role | Integration |
| `setup_consumes_token` | Token's `used_at` is set after use | Integration |
| `setup_with_wrong_token_returns_401` | Wrong token → 401 | Integration |
| `setup_with_expired_token_returns_401` | Expired token → 401 | Integration |
| `setup_with_used_token_returns_401` | Already-consumed token → 401 | Integration |
| `setup_when_users_exist_returns_404` | POST /api/setup when admin already exists → 404 | Integration |
| `setup_rate_limited` | >3 attempts in 5 min → 429 | Integration |
| `setup_get_status_no_users` | GET /api/setup/status returns `{needs_setup: true}` when no users | Integration |
| `setup_get_status_has_users` | GET /api/setup/status returns `{needs_setup: false}` when users exist | Integration |

**Integration tests — `tests/auth_integration.rs` (add to existing)**

| Test | Validates | Layer |
|---|---|---|
| `dev_mode_creates_admin_automatically` | With PLATFORM_DEV=true, admin/admin exists at startup (existing behavior preserved) | Integration |

**Bootstrap integration tests — new section in `tests/admin_integration.rs`**

| Test | Validates | Layer |
|---|---|---|
| `bootstrap_always_seeds_permissions` | Permissions table populated regardless of dev mode | Integration |
| `bootstrap_always_seeds_roles` | Roles table populated regardless of dev mode | Integration |
| `bootstrap_dev_mode_creates_admin` | PLATFORM_DEV=true creates admin user | Integration |

Total: ~9 unit + ~14 integration tests

#### Existing tests to UPDATE

| Test file | Change | Reason |
|---|---|---|
| `tests/helpers/mod.rs` | All integration tests rely on the admin user being bootstrapped. With `PLATFORM_DEV=true` (which test infra should set), bootstrap unchanged. **No changes needed.** | Dev mode preserves existing behavior |
| `tests/auth_integration.rs::login_valid_credentials` | Verify still works — PLATFORM_DEV ensures admin/admin exists | Bootstrap rewrite |
| All integration tests | No changes — `PLATFORM_DEV=true` in test env preserves current bootstrap | Bootstrap rewrite only affects prod path |

**Critical invariant**: Integration test infrastructure MUST set `PLATFORM_DEV=true` (or equivalent). Verify `tests/helpers/mod.rs` does this. If not, add it.

#### Tests to REMOVE

None — all existing tests remain valid under dev-mode bootstrap.

#### Tests NOT needed

- No E2E tests for setup flow — it's a one-time-use endpoint, fully testable at integration tier
- No Playwright tests for `Setup.tsx` — UI form is trivial (submit token + credentials), tested manually during development
- No tests for K8s secret storage — optional feature, tested via `just deploy-local`

### Verification
- Fresh DB + `PLATFORM_DEV=false` → token printed to stdout, no admin user created
- `POST /api/setup` with correct token → admin created, token consumed
- `POST /api/setup` with wrong/expired token → 401
- `POST /api/setup` when users exist → 404
- `PLATFORM_DEV=true` → old behavior (admin/admin created, no setup token)

---

## PR 4: Unified Agent Roles + Hard Scope Boundaries

The core of the permission redesign. Agent roles become regular DB roles (same system as human roles), permissions mapped via `role_permissions`. Delegation replaced by role assignment + scope enforcement.

### 4.1 Agent Roles as DB Roles (Unified System)

Agent roles are seeded in the `roles` table alongside human roles. They use the same `role_permissions` mapping. Admins can customize their permissions via the existing admin API.

**Migration: `{next}_agent_roles`** — seed new roles + wire permissions:

```sql
-- Seed agent roles (is_system = false → admins can customize permissions)
INSERT INTO roles (id, name, description, is_system) VALUES
  (gen_random_uuid(), 'agent-dev',     'Agent: developer — code within a project',           false),
  (gen_random_uuid(), 'agent-ops',     'Agent: operations — deploy and observe a project',   false),
  (gen_random_uuid(), 'agent-test',    'Agent: tester — read-only project + observability',  false),
  (gen_random_uuid(), 'agent-review',  'Agent: reviewer — read-only project access',         false),
  (gen_random_uuid(), 'agent-manager', 'Agent: manager — create projects, spawn agents',     false)
ON CONFLICT (name) DO NOTHING;

-- Wire permissions (same pattern as bootstrap.rs)
-- agent-dev: project:read, project:write, secret:read, registry:pull, registry:push
-- agent-ops: project:read, deploy:read, deploy:promote, observe:read, observe:write, alert:manage, secret:read, registry:pull
-- agent-test: project:read, observe:read, registry:pull
-- agent-review: project:read, observe:read
-- agent-manager: project:read, project:write, agent:run, agent:spawn, deploy:read, observe:read, workspace:read
INSERT INTO role_permissions (role_id, permission_id)
SELECT r.id, p.id FROM roles r, permissions p
WHERE (r.name, p.name) IN (
  ('agent-dev', 'project:read'), ('agent-dev', 'project:write'), ('agent-dev', 'secret:read'),
  ('agent-dev', 'registry:pull'), ('agent-dev', 'registry:push'),
  ('agent-ops', 'project:read'), ('agent-ops', 'deploy:read'), ('agent-ops', 'deploy:promote'),
  ('agent-ops', 'observe:read'), ('agent-ops', 'observe:write'), ('agent-ops', 'alert:manage'),
  ('agent-ops', 'secret:read'), ('agent-ops', 'registry:pull'),
  ('agent-test', 'project:read'), ('agent-test', 'observe:read'), ('agent-test', 'registry:pull'),
  ('agent-review', 'project:read'), ('agent-review', 'observe:read'),
  ('agent-manager', 'project:read'), ('agent-manager', 'project:write'),
  ('agent-manager', 'agent:run'), ('agent-manager', 'agent:spawn'),
  ('agent-manager', 'deploy:read'), ('agent-manager', 'observe:read'),
  ('agent-manager', 'workspace:read')
) ON CONFLICT DO NOTHING;
```

**Scope type** for each agent role (enforced in code, not DB):

| Role | Scope | Description |
|---|---|---|
| `agent-dev` | Project | Code within a single project |
| `agent-ops` | Project | Deploy and observe a single project |
| `agent-test` | Project | Read-only test + observe for a project |
| `agent-review` | Project | Read-only code review |
| `agent-manager` | Workspace | Create projects, spawn agents across workspace |

**Resolution formula (same as human roles, plus two constraints):**
```
1. RBAC resolver computes: role_permissions for the assigned agent role
2. Intersected with: spawner's effective permissions (can't exceed what human has)
3. Stored as: token.scopes (pre-computed snapshot)
4. Enforced by: token.scope_workspace_id + token.project_id (hard boundary)
```

**Why `is_system = false`**: Admins can adjust agent role permissions via `PUT /api/admin/roles/{id}/permissions`. E.g., an org might want `agent-dev` to also have `deploy:read`. The seeded permissions are sensible defaults.

**Helper enum** `AgentRoleName` in `src/agent/mod.rs` for parsing + scope-type lookup:

```rust
pub enum AgentRoleName { Dev, Ops, Test, Review, Manager }

impl AgentRoleName {
    pub fn db_role_name(&self) -> &'static str { /* "agent-dev", "agent-ops", ... */ }
    pub fn is_workspace_scoped(&self) -> bool { matches!(self, Self::Manager) }
    pub fn from_str(s: &str) -> Option<Self> { /* "dev"|"agent-dev" → Dev, etc. */ }
}
```

### 4.2 Migration: Add `scope_workspace_id` to `api_tokens`

```sql
ALTER TABLE api_tokens
    ADD COLUMN scope_workspace_id UUID REFERENCES workspaces(id);

-- project_id already exists and is already a FK to projects
-- Add index for scope lookups
CREATE INDEX idx_api_tokens_scope_workspace ON api_tokens(scope_workspace_id)
    WHERE scope_workspace_id IS NOT NULL;
CREATE INDEX idx_api_tokens_scope_project ON api_tokens(project_id)
    WHERE project_id IS NOT NULL;
```

### 4.3 Extend AuthUser with Scope

**`src/auth/middleware.rs`**:

```rust
pub struct AuthUser {
    pub user_id: Uuid,
    pub user_name: String,
    pub user_type: UserType,
    pub ip_addr: Option<String>,
    pub token_scopes: Option<Vec<String>>,
    // NEW: hard scope boundaries from token
    pub scope_workspace_id: Option<Uuid>,
    pub scope_project_id: Option<Uuid>,
}

impl AuthUser {
    /// Verify this request is allowed to access the given project.
    /// Returns 404 for scope violations (don't leak existence).
    pub fn check_project_scope(&self, project_id: Uuid) -> Result<(), ApiError> {
        if let Some(scope_pid) = self.scope_project_id {
            if scope_pid != project_id {
                return Err(ApiError::NotFound("project".into()));
            }
        }
        Ok(())
    }

    /// Verify this request is allowed to access resources in the given workspace.
    pub fn check_workspace_scope(&self, workspace_id: Uuid) -> Result<(), ApiError> {
        if let Some(scope_wid) = self.scope_workspace_id {
            if scope_wid != workspace_id {
                return Err(ApiError::NotFound("workspace".into()));
            }
        }
        Ok(())
    }
}
```

Update `TokenAuthLookup` struct and `lookup_api_token` SQL to SELECT `t.project_id` and `t.scope_workspace_id`.

### 4.4 Scope Enforcement in Helpers

**`src/api/helpers.rs`** — update `require_project_read` and `require_project_write`:

```rust
pub async fn require_project_read(
    state: &AppState,
    auth: &AuthUser,
    project_id: Uuid,
) -> Result<(), ApiError> {
    // Hard scope check FIRST — before any DB query
    auth.check_project_scope(project_id)?;

    // If workspace-scoped, verify project belongs to that workspace
    if let Some(scope_wid) = auth.scope_workspace_id {
        let in_workspace = sqlx::query_scalar!(
            "SELECT EXISTS(SELECT 1 FROM projects WHERE id = $1 AND workspace_id = $2 AND is_active = true) as \"exists!: bool\"",
            project_id, scope_wid,
        ).fetch_one(&state.pool).await?;
        if !in_workspace {
            return Err(ApiError::NotFound("project".into()));
        }
    }

    // ... existing visibility + RBAC checks unchanged ...
}
```

Same pattern for `require_project_write` and all other project-accessing helpers.

### 4.5 Rewrite Agent Identity

**`src/agent/identity.rs`**:

Replace delegation-based approach with role assignment + scoped token:

```rust
pub async fn create_agent_identity(
    pool: &PgPool,
    valkey: &fred::clients::Pool,
    session_id: Uuid,
    spawner_id: Uuid,         // human user
    project_id: Uuid,
    workspace_id: Uuid,       // from project lookup
    agent_role: AgentRoleName,
) -> Result<AgentIdentity, AgentError> {
    // 1. Create ephemeral agent user (same as today — for audit trail)
    let agent_user_id = Uuid::new_v4();
    let short_id = &session_id.to_string()[..8];
    let agent_name = format!("agent-{short_id}");
    // ... insert into users ...

    // 2. Assign the REQUESTED agent role (e.g. "agent-dev") — NOT the empty "agent" role
    let role_id = sqlx::query_scalar!(
        "SELECT id FROM roles WHERE name = $1",
        agent_role.db_role_name(),
    ).fetch_one(pool).await?;

    sqlx::query!(
        "INSERT INTO user_roles (id, user_id, role_id, project_id) VALUES ($1, $2, $3, $4)",
        Uuid::new_v4(), agent_user_id, role_id,
        if agent_role.is_workspace_scoped() { None } else { Some(project_id) },
    ).execute(pool).await?;

    // 3. Compute effective permissions: role's DB permissions ∩ spawner's permissions
    //    This uses the standard RBAC resolver — same code path as human permission checks
    let role_perms = resolver::role_permissions(pool, role_id).await?;
    let spawner_perms = resolver::effective_permissions(pool, valkey, spawner_id, Some(project_id)).await?;
    let effective: Vec<String> = role_perms.iter()
        .filter(|p| spawner_perms.contains(p))
        .map(|p| p.as_str().to_owned())
        .collect();

    // 4. Create SCOPED API token
    let (raw_token, token_hash) = token::generate_api_token();
    let (scope_ws, scope_proj) = if agent_role.is_workspace_scoped() {
        (Some(workspace_id), None)          // manager: workspace boundary
    } else {
        (Some(workspace_id), Some(project_id))  // dev/ops/test: project boundary
    };

    sqlx::query!(
        "INSERT INTO api_tokens (user_id, name, token_hash, scopes, project_id, scope_workspace_id, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
        agent_user_id,
        format!("agent-session-{session_id}"),
        token_hash,
        &effective,        // pre-computed: role_perms ∩ spawner_perms
        scope_proj,        // hard project boundary
        scope_ws,          // hard workspace boundary
        Utc::now() + Duration::hours(24),
    ).execute(pool).await?;

    // NO delegation rows — role assignment + token scopes + scope boundaries handle everything

    Ok(AgentIdentity { user_id: agent_user_id, api_token: raw_token })
}
```

**`src/rbac/resolver.rs`** — add `role_permissions()` helper:

```rust
/// Get permissions for a specific role by ID (from role_permissions join table).
pub async fn role_permissions(pool: &PgPool, role_id: Uuid) -> Result<HashSet<Permission>, anyhow::Error> {
    let rows = sqlx::query_scalar!(
        "SELECT p.name FROM permissions p JOIN role_permissions rp ON rp.permission_id = p.id WHERE rp.role_id = $1",
        role_id,
    ).fetch_all(pool).await?;
    Ok(rows.iter().filter_map(|n| n.parse().ok()).collect())
}
```

**`src/agent/identity.rs`** — simplify `cleanup_agent_identity`:

```rust
pub async fn cleanup_agent_identity(pool, valkey, agent_user_id) {
    // Delete API tokens
    // Delete auth sessions
    // Deactivate user
    // Invalidate permission cache
    // NO delegation revocation needed — just remove the user_roles + tokens
}
```

### 4.6 Update Session API

**`src/api/sessions.rs`**:

- `CreateSessionRequest`: Replace `delegate_deploy`/`delegate_observe`/`delegate_admin` booleans with a single `role: Option<String>` field (defaults to `"dev"`)
- `create_session` handler: Parse role string → `AgentRoleName` enum, look up project's `workspace_id`, pass to `create_agent_identity`
- Remove delegation flags from all session creation paths

### 4.7 Agent Spawning Scope Rules

**`src/api/sessions.rs`** — `spawn_child` handler:

- Child scope MUST be ≤ parent scope:
  - Workspace-scoped parent (manager) → can spawn project-scoped children within that workspace
  - Project-scoped parent → can only spawn children for the SAME project
- Child effective = `child_role_perms ∩ parent_effective ∩ child_scope`
- Validate: child's target project must be in parent's workspace scope
- `user_id` always inherited from root session (human principal)

### TDD Test Plan — PR 4

This is the most complex PR. Test plan organized by component, in implementation order.

#### Phase A: AgentRoleName enum (write tests first, then implement)

**Unit tests — `src/agent/mod.rs` (new `#[cfg(test)] mod tests` block)**

| Test | Validates | Layer |
|---|---|---|
| `agent_role_name_from_str_dev` | `"dev"` → `Some(Dev)` | Unit |
| `agent_role_name_from_str_agent_dev` | `"agent-dev"` → `Some(Dev)` | Unit |
| `agent_role_name_from_str_ops` | `"ops"` → `Some(Ops)` | Unit |
| `agent_role_name_from_str_agent_ops` | `"agent-ops"` → `Some(Ops)` | Unit |
| `agent_role_name_from_str_test` | `"test"` → `Some(Test)` | Unit |
| `agent_role_name_from_str_review` | `"review"` → `Some(Review)` | Unit |
| `agent_role_name_from_str_manager` | `"manager"` → `Some(Manager)` | Unit |
| `agent_role_name_from_str_agent_manager` | `"agent-manager"` → `Some(Manager)` | Unit |
| `agent_role_name_from_str_unknown` | `"unknown"` → `None` | Unit |
| `agent_role_name_from_str_empty` | `""` → `None` | Unit |
| `agent_role_name_db_role_name_dev` | `Dev.db_role_name()` → `"agent-dev"` | Unit |
| `agent_role_name_db_role_name_ops` | `Ops.db_role_name()` → `"agent-ops"` | Unit |
| `agent_role_name_db_role_name_test` | `Test.db_role_name()` → `"agent-test"` | Unit |
| `agent_role_name_db_role_name_review` | `Review.db_role_name()` → `"agent-review"` | Unit |
| `agent_role_name_db_role_name_manager` | `Manager.db_role_name()` → `"agent-manager"` | Unit |
| `agent_role_is_workspace_scoped_manager` | `Manager.is_workspace_scoped()` → `true` | Unit |
| `agent_role_is_workspace_scoped_dev` | `Dev.is_workspace_scoped()` → `false` | Unit |
| `agent_role_is_workspace_scoped_ops` | `Ops.is_workspace_scoped()` → `false` | Unit |
| `agent_role_is_workspace_scoped_test` | `Test.is_workspace_scoped()` → `false` | Unit |
| `agent_role_is_workspace_scoped_review` | `Review.is_workspace_scoped()` → `false` | Unit |

Total: 20 unit tests

#### Phase B: AuthUser scope checks (write tests first, then implement)

**Unit tests — `src/auth/middleware.rs` (add to existing `#[cfg(test)] mod tests`)**

| Test | Validates | Layer |
|---|---|---|
| `check_project_scope_none_allows_any` | `scope_project_id=None` → Ok for any project_id | Unit |
| `check_project_scope_matching_allows` | `scope_project_id=Some(X)` + project_id=X → Ok | Unit |
| `check_project_scope_mismatch_returns_404` | `scope_project_id=Some(X)` + project_id=Y → NotFound | Unit |
| `check_workspace_scope_none_allows_any` | `scope_workspace_id=None` → Ok for any workspace_id | Unit |
| `check_workspace_scope_matching_allows` | `scope_workspace_id=Some(X)` + workspace_id=X → Ok | Unit |
| `check_workspace_scope_mismatch_returns_404` | `scope_workspace_id=Some(X)` + workspace_id=Y → NotFound | Unit |
| `test_constructor_has_none_scopes` | `AuthUser::test_human()` has scope_workspace_id=None, scope_project_id=None | Unit |

_Add test constructor:_

| Test | Validates | Layer |
|---|---|---|
| `test_with_project_scope_constructor` | `AuthUser::test_with_project_scope(user_id, project_id)` sets scope_project_id | Unit |
| `test_with_workspace_scope_constructor` | `AuthUser::test_with_workspace_scope(user_id, ws_id)` sets scope_workspace_id | Unit |

Total: 9 unit tests

#### Phase C: Token lookup returns scope fields (write tests first, then implement)

**Integration tests — `tests/auth_integration.rs` (add to existing)**

| Test | Validates | Layer |
|---|---|---|
| `scoped_api_token_returns_scope_project_id` | Token with project_id → AuthUser.scope_project_id set | Integration |
| `scoped_api_token_returns_scope_workspace_id` | Token with scope_workspace_id → AuthUser.scope_workspace_id set | Integration |
| `unscoped_api_token_returns_none_scopes` | Regular token → scope fields are None | Integration |
| `session_auth_has_no_scope_fields` | Session cookie auth → scope fields are None | Integration |

Total: 4 integration tests

#### Phase D: role_permissions() resolver helper (write tests first, then implement)

**Integration tests — `tests/rbac_integration.rs` (add to existing)**

| Test | Validates | Layer |
|---|---|---|
| `role_permissions_returns_correct_set` | `role_permissions(admin_role_id)` returns all admin permissions | Integration |
| `role_permissions_empty_for_no_perms_role` | Role with no permissions → empty HashSet | Integration |
| `role_permissions_nonexistent_role` | Nonexistent role_id → empty set (no error) | Integration |
| `agent_dev_role_has_correct_permissions` | `agent-dev` role has project:read, project:write, secret:read, registry:pull, registry:push | Integration |
| `agent_ops_role_has_correct_permissions` | `agent-ops` role has 7 permissions including deploy:promote | Integration |
| `agent_test_role_has_correct_permissions` | `agent-test` role has project:read, observe:read, registry:pull | Integration |
| `agent_review_role_has_correct_permissions` | `agent-review` role has project:read, observe:read | Integration |
| `agent_manager_role_has_correct_permissions` | `agent-manager` role has 7 permissions including agent:run, agent:spawn | Integration |

Total: 8 integration tests

#### Phase E: Scope enforcement in helpers (write tests first, then implement)

**Integration tests — `tests/project_integration.rs` (add to existing)**

| Test | Validates | Layer |
|---|---|---|
| `project_scoped_token_can_access_own_project` | Token scoped to project X → GET /api/projects/X works | Integration |
| `project_scoped_token_cannot_access_other_project` | Token scoped to project X → GET /api/projects/Y → 404 | Integration |
| `workspace_scoped_token_can_access_workspace_project` | Token scoped to workspace W → project in W → works | Integration |
| `workspace_scoped_token_cannot_access_other_workspace_project` | Token scoped to workspace W → project in V → 404 | Integration |
| `unscoped_token_can_access_any_project` | Regular token with permissions → any project works | Integration |

**Integration tests — `tests/issue_mr_integration.rs` (add to existing)**

| Test | Validates | Layer |
|---|---|---|
| `project_scoped_token_can_access_issues` | Scoped token → issues under own project → works | Integration |
| `project_scoped_token_blocked_cross_project_issues` | Scoped token → issues under other project → 404 | Integration |

**Integration tests — `tests/secrets_integration.rs` (add to existing)**

| Test | Validates | Layer |
|---|---|---|
| `project_scoped_token_can_read_own_secrets` | Scoped token → secrets for own project → works | Integration |
| `project_scoped_token_blocked_cross_project_secrets` | Scoped token → secrets for other project → 404 | Integration |

Total: 9 integration tests

#### Phase F: Rewritten agent identity (write tests first, then implement)

**Integration tests — `tests/session_integration.rs` (add to existing)**

| Test | Validates | Layer |
|---|---|---|
| `create_session_with_role_dev` | `role: "dev"` → session created, agent has project-scoped token | Integration |
| `create_session_with_role_ops` | `role: "ops"` → agent has ops permissions (deploy:read, etc.) | Integration |
| `create_session_with_role_test` | `role: "test"` → agent has read-only permissions | Integration |
| `create_session_with_role_review` | `role: "review"` → agent has minimal read permissions | Integration |
| `create_session_role_defaults_to_dev` | No `role` field → defaults to "dev" | Integration |
| `create_session_with_invalid_role` | `role: "hacker"` → 400 | Integration |
| `create_session_role_manager_requires_workspace` | `role: "manager"` → workspace-scoped token created | Integration |

**Integration tests — `tests/agent_spawn_integration.rs` (add to existing)**

| Test | Validates | Layer |
|---|---|---|
| `agent_token_scoped_to_project` | Agent identity token has `project_id` set in DB | Integration |
| `agent_token_scoped_to_workspace` | Agent identity token has `scope_workspace_id` set in DB | Integration |
| `agent_dev_cannot_access_other_project` | Agent-dev token used on wrong project → 404 | Integration |
| `agent_manager_can_access_workspace_projects` | Agent-manager can access any project in its workspace | Integration |
| `agent_manager_cannot_access_other_workspace` | Agent-manager blocked from other workspace's projects → 404 | Integration |
| `agent_permissions_intersected_with_spawner` | If spawner lacks deploy:promote, ops agent doesn't get it | Integration |
| `agent_cleanup_removes_user_roles` | Cleanup removes user_roles row (not delegations) | Integration |
| `agent_cleanup_removes_tokens` | Cleanup deletes API tokens | Integration |
| `agent_cleanup_deactivates_user` | Cleanup sets user is_active=false | Integration |
| `agent_no_delegation_rows_created` | New identity creates 0 delegation rows | Integration |

Total: 17 integration tests

#### Phase G: Session API role field + spawn scope validation (write tests first, then implement)

**Unit tests — `src/api/sessions.rs` (add to existing `#[cfg(test)] mod tests`)**

| Test | Validates | Layer |
|---|---|---|
| `validate_role_field_valid_values` | "dev", "ops", "test", "review", "manager" all accepted | Unit |
| `validate_role_field_invalid` | "hacker" rejected | Unit |
| `validate_role_field_none_defaults_dev` | None → "dev" | Unit |

**Integration tests — `tests/agent_spawn_integration.rs` (add to existing)**

| Test | Validates | Layer |
|---|---|---|
| `spawn_child_inherits_project_scope` | Project-scoped parent → child scoped to same project | Integration |
| `spawn_child_cannot_escalate_to_different_project` | Project-scoped parent → child for different project → 400/403 | Integration |
| `spawn_child_manager_can_spawn_for_workspace_projects` | Workspace-scoped parent → child for any project in workspace | Integration |
| `spawn_child_manager_blocked_outside_workspace` | Workspace-scoped parent → child for project outside workspace → 404 | Integration |
| `spawn_child_inherits_user_id` | Child session's user_id = root human principal, not parent agent | Integration |
| `spawn_child_role_field_accepted` | spawn_child with `role: "test"` → child has test role | Integration |
| `spawn_child_effective_perms_intersected` | child_effective = child_role ∩ parent_effective | Integration |

Total: 3 unit + 7 integration tests

#### Phase H: E2E tests (write after integration tests pass, validates full stack)

**E2E tests — `tests/e2e_agent.rs` (add to existing)**

| Test | Validates | Layer |
|---|---|---|
| `agent_role_dev_pod_has_scoped_token` | Dev agent pod env has token; token rejects cross-project API calls | E2E |
| `agent_role_ops_has_deploy_permissions` | Ops agent can access deploy endpoints for its project | E2E |
| `agent_scope_isolation_cross_project` | Agent for project A makes API call to project B → 404 | E2E |
| `agent_scope_isolation_cross_workspace` | Agent for workspace W makes API call to workspace V project → 404 | E2E |
| `agent_manager_creates_project_in_workspace` | Manager agent creates project — it lands in manager's workspace | E2E |
| `agent_spawn_child_scope_inherited` | Manager spawns dev child → child token scoped to project | E2E |

Total: 6 E2E tests

#### Summary: All existing tests to UPDATE for PR 4

| Test file | Change | Reason |
|---|---|---|
| `tests/session_integration.rs::create_session_*` (all create tests) | Remove `delegate_deploy`/`delegate_observe`/`delegate_admin` from request bodies; add `role` field | API field change |
| `tests/session_integration.rs::spawn_child_session` | Add `role` field to spawn request | API field change |
| `tests/agent_spawn_integration.rs::spawn_*` | Same — `role` field replaces delegation bools | API field change |
| `tests/create_app_integration.rs::create_app_session` | Verify inprocess session still works (no role field needed for create-app) | Regression |
| `tests/auth_integration.rs::scoped_api_token_authenticates` | Verify scope fields populated correctly | AuthUser struct change |
| `tests/e2e_agent.rs::agent_identity_created` | Verify agent has user_role row (not delegation), verify token has project_id + scope_workspace_id | Identity rewrite |
| `tests/e2e_agent.rs::agent_cleanup_on_completion` | Verify cleanup deletes user_roles (not delegations) | Identity cleanup change |
| `tests/e2e_agent.rs::agent_pod_spec_correct` | Verify pod env unchanged (token still passed as env var) | Regression |
| `tests/helpers/mod.rs` | Update `create_session()` helper to use `role` field instead of delegation bools | API field change |
| `tests/e2e_helpers/mod.rs` | Same — update E2E session creation helpers | API field change |

#### Tests to REMOVE

| Test file | Test name | Reason |
|---|---|---|
| `tests/agent_spawn_integration.rs` (if exists) | Any test validating delegation row creation for agents | Delegations no longer created for agents |
| `tests/e2e_agent.rs` (if exists) | Any assertion checking `delegations` table for agent users | Same — delegation replaced by role assignment |

#### Tests NOT needed (coverage already achieved by above)

- No unit tests for `create_agent_identity` internals — it's all DB operations, fully covered by integration tests
- No unit tests for `cleanup_agent_identity` — same reasoning, all DB ops
- No separate `tests/scope_integration.rs` — scope tests are distributed across project/session/spawn integration files where the scope enforcement actually applies
- No Playwright tests — scope enforcement is API-level, no UI component involved

### Files Changed

| File | Change |
|---|---|
| `migrations/{next}_agent_roles.up.sql` | Seed 5 agent roles + wire `role_permissions` |
| `migrations/{next}_scope_workspace.up.sql` | Add `scope_workspace_id` to `api_tokens`, add indexes |
| `src/agent/mod.rs` | New `AgentRoleName` enum (parsing + scope-type lookup, NOT permission definitions) |
| `src/auth/middleware.rs` | Extend `AuthUser` with scope fields, update `TokenAuthLookup` + SQL query, add `check_project_scope()` / `check_workspace_scope()` |
| `src/api/helpers.rs` | Add scope checks to `require_project_read`, `require_project_write` |
| `src/rbac/resolver.rs` | Add `role_permissions(pool, role_id)` helper to query a role's permission set |
| `src/agent/identity.rs` | Rewrite: assign agent DB role, compute `role_perms ∩ spawner_perms`, create scoped token, no delegation |
| `src/agent/service.rs` | Pass `AgentRoleName` + `workspace_id` to `create_agent_identity` |
| `src/api/sessions.rs` | Replace delegation bools with `role` field, update `create_session`, update `spawn_child` scope validation |
| `src/store/bootstrap.rs` | Also seed agent roles (for fresh installs where migration hasn't run yet) |

### Verification
- Agent for project X cannot access project Y (returns 404)
- Agent for workspace W cannot access projects in workspace V (returns 404)
- `dev` agent gets `project:read + project:write` but NOT `deploy:promote`
- `ops` agent gets `deploy:promote` only if the spawning human has it
- `manager` agent can create projects within its workspace but not outside
- Parent agent can spawn child agents only within its own scope
- `just test-unit` + `just test-integration` pass
- New integration tests for scope enforcement

---

## Test Plan Summary

### New test counts by PR

| PR | Unit | Integration | E2E | MCP | Total new |
|---|---|---|---|---|---|
| PR 1: Bug fixes + MCP | 6 | 0 | 0 | ~30 | ~36 |
| PR 2: Mandatory workspaces | 2 | 8 | 0 | 0 | 10 |
| PR 3: Secure bootstrap | 9 | 14 | 0 | 0 | 23 |
| PR 4: Agent roles + scope | 32 | 45 | 6 | 0 | 83 |
| **Total** | **49** | **67** | **6** | **~30** | **~152** |

### Tests updated (cascading changes)

| PR | Files touched | Reason |
|---|---|---|
| PR 2 | `tests/helpers/mod.rs`, `tests/e2e_helpers/mod.rs` | workspace_id NOT NULL — update project creation helpers |
| PR 4 | `tests/helpers/mod.rs`, `tests/e2e_helpers/mod.rs`, 5+ integration test files, 2+ E2E test files | `role` replaces delegation bools in session creation |

### Tests removed

| PR | What | Reason |
|---|---|---|
| PR 4 | Any assertions on delegation rows for agent users | Delegation no longer used for agent permissions |

### Coverage goals by module

| Module | Current coverage source | After plan 32 |
|---|---|---|
| `src/agent/mod.rs` | None (module declarations only) | 20 unit tests for `AgentRoleName` |
| `src/agent/identity.rs` | Integration only (41+ tests) | Integration + 10 new targeted integration tests |
| `src/agent/service.rs` | Integration only (41+ tests) | No new tests needed — service calls unchanged |
| `src/agent/provider.rs` | 8 unit + 6 integration | +5 unit tests for role validation |
| `src/agent/inprocess.rs` | 15 unit + 9 integration | +1 unit test for system prompt |
| `src/auth/middleware.rs` | 25 unit + 13 integration | +9 unit + 4 integration for scope checks |
| `src/api/helpers.rs` | 0 unit + 50+ integration | +9 integration for scope enforcement |
| `src/api/sessions.rs` | 10 unit + 41+ integration | +3 unit + 7 integration for role field + spawn scope |
| `src/api/setup.rs` | NEW file | 6 unit + 11 integration |
| `src/api/projects.rs` | 16 integration | +5 integration for scope enforcement |
| `src/rbac/resolver.rs` | 11 unit + 15 integration | +8 integration for role_permissions() |
| `src/store/bootstrap.rs` | Implicit only | +3 unit + 3 integration |
| `src/workspace/service.rs` | 15+ integration | +2 unit + 3 integration |
| MCP servers (6 files) | 0 | ~30 MCP tests |

### Branch coverage checklist — every new code branch has a test

**`AuthUser.check_project_scope()`**: 3 branches → 3 unit tests (None, match, mismatch)
**`AuthUser.check_workspace_scope()`**: 3 branches → 3 unit tests (None, match, mismatch)
**`AgentRoleName::from_str()`**: 10 variants + empty + unknown → 10 unit tests
**`AgentRoleName::db_role_name()`**: 5 variants → 5 unit tests
**`AgentRoleName::is_workspace_scoped()`**: 5 variants → 5 unit tests
**`require_project_read` scope path**: 4 branches (no scope, project match, project mismatch, workspace check) → 5 integration tests
**`require_project_write` scope path**: Same → covered by project scope integration tests
**`create_agent_identity` new logic**: role lookup, role assignment, perm intersection, scoped token → 10 integration tests
**`cleanup_agent_identity` simplified**: delete tokens, delete sessions, deactivate, invalidate cache → 3 integration tests
**`POST /api/setup`**: valid token, wrong token, expired, used, users-exist, rate-limit → 6+ integration tests
**`bootstrap.rs` dev vs prod path**: dev creates admin, prod generates token → 3 integration tests
**`spawn_child` scope validation**: same project, different project, workspace-scoped, cross-workspace → 7 integration tests
**Session `role` field**: valid values, invalid, default → 3 unit + 7 integration tests

---

## What This Replaces from Original Plan 31

| Original Plan 31 Item | Disposition |
|---|---|
| SQL bug fix | Kept in PR 1 |
| Missing MCP tools | Kept in PR 1 |
| Role rename (create-app → manager) | Kept in PR 1 |
| MCP tests | Kept in PR 1 |
| `owner_id` column on users | **Dropped** — no permanent agent users |
| `ensure_user_agents()` (4 agents/user) | **Dropped** — ephemeral only |
| `target_session_id` on messages | Deferred — not needed for the permission redesign |
| `notify_parent` MCP tool | Deferred — can be added independently |
| Per-user agent identity reuse | **Dropped** — ephemeral sessions are fine with unified roles |

---

## Summary of Concepts

**Before (current):**
- Agent gets empty `agent` role → individual permissions delegated from human → delegation rows in DB → cleanup revokes them
- No scope boundary on tokens → agent could theoretically access any project
- `api_tokens.project_id` unused
- Bootstrap creates insecure default admin
- Agent permission system is separate from human role system

**After (this plan):**
- **Unified role system**: Agent roles (`agent-dev`, `agent-ops`, etc.) are regular DB roles alongside human roles (`admin`, `developer`, `ops`, `viewer`)
- **Same permission mechanism**: Both use `role_permissions` table. Admins customize via same API
- **Pre-computed intersection**: `role_perms ∩ spawner_perms` → stored as `token.scopes`
- **Hard scope boundaries**: `token.scope_workspace_id` + `token.project_id` → middleware enforces
- **Workspace = isolation**: Every project belongs to a workspace. Scope enforcement prevents cross-workspace access
- **Secure bootstrap**: One-time setup token → first admin created via browser

**Permission resolution (unified for humans and agents):**
```
Humans:   RBAC resolver (role_permissions + workspace implicit perms) → full access
Agents:   role_permissions(agent-role) ∩ spawner_perms → token.scopes
          + token.scope_workspace_id / project_id → hard boundary
          + middleware enforces scope on every request
```

**What stays the same:**
- Delegation system → still available for human-to-human delegation
- RBAC resolver → unchanged, works for both humans and agents
- Ephemeral agent users → still created per session for clean audit trail
- `has_permission_scoped` → already intersects token scopes with permissions

---

## Review Findings (2026-02-25)

_Automated parallel review by 5 agents: Schema & Migration, Security & Authorization, Rust Architecture, Test Strategy, API & Integration Impact._

### Executive Summary

**Plan 32 is architecturally sound but not yet implementation-ready.** The core design — unified role system, workspace-as-isolation-boundary, pre-computed scoped tokens — is the right approach. However, the review uncovered **scope enforcement gaps in 8+ code paths** that would leave the new permission boundaries leaky, **3 incorrect MCP endpoint references**, and a **migration that will fail if agent-owned projects exist**. These must be addressed before implementation begins.

Biggest risk: **PR 4's scope enforcement only covers `require_project_read`/`require_project_write` in `src/api/helpers.rs`**, but at least 8 other code paths (secrets, Git, registry, observe, sessions, projects) do direct `has_permission()` calls that bypass scope checks entirely.

### Codebase Reality Check

> **Plan assumes:** MCP `update_project` uses `PUT /api/projects/:id`
> **Reality:** The endpoint is `PATCH /api/projects/{id}` (`src/api/projects.rs:77`). The MCP client library has `apiPatch`, not `apiPut`.

> **Plan assumes:** MCP `get_session` uses `GET /api/sessions/:id`
> **Reality:** No global GET route exists for sessions. Only `GET /api/projects/{id}/sessions/{session_id}` (`src/api/sessions.rs:122`).

> **Plan assumes:** MCP `send_message_to_session` uses `POST /api/sessions/:id/messages` (plural)
> **Reality:** The route is `/message` (singular) — `POST /api/sessions/{session_id}/message` (`src/api/sessions.rs:150-152`).

> **Plan assumes:** `create_test_project()` helper exists in test files
> **Reality:** The helper is named `create_project()` in both `tests/helpers/mod.rs:209` and `tests/e2e_helpers/mod.rs:268`.

> **Plan assumes:** `AgentRoleName::from_str()` is a regular method
> **Reality:** This shadows the standard `FromStr` trait. Every other parseable type in the codebase (`Permission` in `src/rbac/types.rs:65-93`, `UserType`) implements `FromStr`. Clippy will flag `should_implement_trait`.

> **Plan assumes:** `api_tokens.project_id` just needs scope checking added
> **Reality:** The `lookup_api_token` SQL (`src/auth/middleware.rs:191-205`) doesn't SELECT `t.project_id` at all. The `TokenAuthLookup` struct has no `project_id` field. Both must be added.

> **Plan assumes:** Scope checks in `require_project_read`/`require_project_write` cover all project access
> **Reality:** At least 8 code paths bypass these helpers with direct `has_permission()` calls (see Critical Blockers below).

### Critical Blockers

#### Security

**S1. Scope enforcement gaps — 8+ bypassed code paths (PR 4)**
The plan only adds scope checks to `require_project_read` and `require_project_write` in `src/api/helpers.rs`. These code paths do their own direct permission checks and will **not** get scope enforcement:

| Code path | File | Why it's missed |
|---|---|---|
| Secrets CRUD | `src/api/secrets.rs` (`require_secret_read/write`) | Own permission helpers |
| Session management | `src/api/sessions.rs` (`require_agent_run`, `require_session_write`) | Direct `has_permission` calls |
| Git push/pull | `src/git/smart_http.rs:431` | Direct `has_permission` call |
| Git browser | `src/git/browser.rs:172` | Direct `has_permission` call |
| LFS operations | `src/git/lfs.rs:96` | Direct `has_permission` call |
| Registry auth | `src/registry/mod.rs:76` | Direct `has_permission` call |
| Observe queries | `src/observe/query.rs:228` | Has its **own copy** of `require_project_read` |
| Project CRUD | `src/api/projects.rs` (create/update/delete) | Inline permission checks |
| List endpoints | `src/api/projects.rs:209` | Returns all visible projects, no scope filter |

**Fix:** Audit every `has_permission()` call site. Consider middleware-level scope enforcement or a unified `check_and_require_permission()` helper. At minimum, add scope checks to all 8+ paths.

**S2. Setup token timing-safety (PR 3)**
No `subtle` crate in dependencies. Standard `==` on SHA-256 hashes short-circuits on first mismatch.
**Fix:** Use `subtle::ConstantTimeEq` or `ring::constant_time::verify_slices_are_equal`.

**S3. Setup endpoint race condition (PR 3)**
Two concurrent requests could both pass the "0 users exist" check and create two admin users.
**Fix:** Atomic token consumption: `UPDATE setup_tokens SET used_at = now() WHERE token_hash = $1 AND used_at IS NULL RETURNING id`.

**S4. `token_scopes` ignored in permission checks (PR 4)**
Existing `require_project_read`/`require_project_write` call `has_permission()` which ignores `token_scopes`. The scoped variant `has_permission_scoped()` exists but is only used in admin/users/rbac middleware. An agent token with `scopes: ["project:read"]` would pass `require_project_write()` if the DB role grants `project:write`.
**Fix:** Switch ALL permission checks to `has_permission_scoped()` to enforce `token_scopes`.

#### Data

**D1. PR 2 migration fails for agent-owned projects**
Migration creates personal workspaces only for `user_type = 'human'`. Agent-owned projects (`user_type = 'agent'`) won't get a workspace. `ALTER TABLE ... SET NOT NULL` will fail.
**Fix:** Add catch-all: `UPDATE projects SET workspace_id = (SELECT id FROM workspaces ORDER BY created_at LIMIT 1) WHERE workspace_id IS NULL AND is_active = true`.

**D2. PR 2 workspace name collision**
`"{username}-personal"` could collide if names are similar. Migration INSERT has no `ON CONFLICT` handling.
**Fix:** Add `ON CONFLICT (name) DO NOTHING` or use UUID suffix.

#### Architecture

**A1. `from_str()` shadows `FromStr` trait**
Every other parseable type implements `FromStr`. Clippy will flag `should_implement_trait`.
**Fix:** `impl FromStr for AgentRoleName` with `type Err`.

**A2. `create_agent_identity` caller cascade not documented**
Plan doesn't show `service.rs::create_session` signature rewrite (already at 8 params with `#[allow(clippy::too_many_arguments)]`).
**Fix:** Document full call chain: `api/sessions.rs` → `agent/service.rs` → `agent/identity.rs`.

**A3. `cleanup_agent_identity` omits `user_roles` deletion**
Pseudocode shows only: delete tokens, delete sessions, deactivate user, invalidate cache. Missing: `DELETE FROM user_roles WHERE user_id = $1`.
**Fix:** Add to pseudocode and implementation.

**A4. All 6 AuthUser construction sites must be updated**
Adding scope fields breaks: (1) API token path, (2) session token path, (3) session cookie path, (4-6) three `#[cfg(test)]` constructors. Plan only mentions some of these.
**Fix:** List all 6 explicitly.

### Missing Test Coverage

| Missing test | Why it matters |
|---|---|
| Expired agent token returns 401 | 24h TTL means sessions can outlive tokens |
| Admin user with scoped token | Should scope checks apply to admins? Plan is silent |
| `list_projects` with scoped token returns only in-scope projects | List endpoints bypass `require_project_read` |
| Concurrent `get_or_create_default_workspace` | Race condition on workspace creation |
| Partial cleanup failure (DB error mid-cleanup) | Cleanup is not transactional |
| Stale `token.scopes` after role permission change | Admin modifies agent-dev role, existing tokens retain old permissions |

**Missing infrastructure:**
- PR 3 needs `setup_test_state(pool)` helper that skips `bootstrap::run()` (normal `test_state()` creates admin user, making setup tests impossible)
- MCP tests: `mcp/package.json` has no test runner, no `devDependencies`, no `scripts.test`. Must specify framework (vitest/node:test) and add dependency.
- Plan references `create_test_project()` — should be `create_project()`

### Simplification Opportunities

**1. PR 2 cascading test updates are overstated.**
Plan lists ~15 test files needing updates. If the API auto-assigns a default workspace when `workspace_id` is omitted, **zero existing test files need changes**. The `create_project()` helper sends JSON without `workspace_id` — the server handles the default. Remove the "Cascading from helpers change" entries.

**2. Merge scope DB query into existing `require_project_read` query.**
Plan adds a separate `SELECT EXISTS(...)` for workspace membership. The existing helper already queries `projects`. Combine:
```sql
SELECT visibility, owner_id, workspace_id FROM projects WHERE id = $1 AND is_active = true
```
Then check `workspace_id == scope_wid` in Rust. Eliminates extra round-trip.

**3. Consider middleware-level scope enforcement.**
Rather than adding `auth.check_project_scope()` to every handler, extract `project_id` from URL path in a middleware layer. Makes it impossible for handlers to forget scope checks.

**4. PR 4 could split into 2 sub-PRs.**
- PR 4a: AgentRoleName enum + AuthUser scope fields + role_permissions resolver + scope enforcement
- PR 4b: Agent identity rewrite + session API changes + spawn scope + E2E tests

### Suggested Changes by PR

#### PR 1

| # | Change |
|---|---|
| 1 | MCP `update_project`: PUT → PATCH (`apiPatch`) |
| 2 | MCP `get_session`: add `GET /api/sessions/{id}` route or use project-scoped path |
| 3 | MCP `send_message_to_session`: `/messages` → `/message` (singular) |
| 4 | `docker/entrypoint.sh`: use `create-app\|manager)` combined case for rolling upgrades |

#### PR 2

| # | Change |
|---|---|
| 5 | Migration: add catch-all for agent-owned projects with NULL workspace |
| 6 | Keep `CreateProjectRequest.workspace_id` as `Option<Uuid>`, auto-assign server-side (plan text says "becomes required" which contradicts auto-assignment) |
| 7 | Remove cascading test update section — auto-assignment means no test changes needed |
| 8 | Add `just types` to verification checklist for ts-rs regeneration |

#### PR 3

| # | Change |
|---|---|
| 9 | Add `subtle` crate for constant-time token comparison |
| 10 | Atomic token consumption via `UPDATE ... WHERE used_at IS NULL RETURNING id` |
| 11 | Uniform error responses: return 401 for all failure cases (not 404 for "users exist") |
| 12 | Rate limit key: global `rate:setup:global`, not per-IP |
| 13 | Create `setup_test_state(pool)` helper that skips bootstrap |
| 14 | UI: Login page calls `GET /api/setup/status`, redirects to `/setup` if `needs_setup: true` |
| 15 | Specify MCP test framework in `mcp/package.json` |

#### PR 4

| # | Change |
|---|---|
| 16 | Implement `FromStr` trait instead of inherent `from_str()` |
| 17 | Add scope checks to ALL 8+ bypassed code paths (S1 table above) |
| 18 | Switch all `has_permission()` to `has_permission_scoped()` for token_scopes enforcement |
| 19 | Add scope filtering to list endpoints (`list_projects`, `list_issues`, etc.) |
| 20 | Add `DELETE FROM user_roles` to cleanup pseudocode |
| 21 | Show full `service.rs::create_session` signature rewrite |
| 22 | Add `#[tracing::instrument]` to `role_permissions()` |
| 23 | Update all 6 AuthUser construction sites explicitly |
| 24 | Update MCP `spawn_agent` tool in `platform-core.js` to pass `role` parameter |
| 25 | Document stale token scopes: admin role changes won't affect existing tokens (24h TTL) |
| 26 | Add backward compat mapping for delegation booleans during transition |
| 27 | Add `.sqlx/` regeneration (`just db-prepare`) to each PR's verification checklist |

### Additional Security Notes

- **Stale pre-computed `token.scopes`**: If admin modifies `agent-dev` role permissions, existing tokens retain old set until 24h expiry. Document explicitly; consider invalidating affected tokens on role change.
- **Parent token expiry doesn't cascade to children**: Child agent retains permissions after parent expires. Consider `child_expires_at = min(parent_expires_at, now + 24h)`.
- **`observe/query.rs` has separate `require_project_read` copy** (`line 228`) that won't get scope checks. Refactor to use shared helper.
- **Notification endpoints** (`src/api/notifications.rs`) have no project scope filtering — agent could read notifications for unrelated projects.
- **`is_system = false` on agent roles** lets admins add `admin:delegate` to agent-dev — privilege escalation path. Add validation blocking admin-tier permissions on agent roles, or set `is_system = true`.
