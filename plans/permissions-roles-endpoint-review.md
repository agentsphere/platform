# Platform Permissions, Roles & Endpoint Security Review

**Date**: 2026-03-03
**Scope**: Full audit of all permissions, roles, endpoint protection, and authorization patterns

---

## Executive Summary

**138+ endpoints** across 21 API modules + Git/Registry/OTLP routes. All are protected except 5 intentionally unauthenticated entry points (login, setup, passkey-login, healthz, static UI). The RBAC system uses **21 permissions**, **10 roles**, project-scoped + global assignments, delegations, workspace-derived access, and API token scope intersection.

---

## 1. Permission Definitions (21 total)

| Permission | Scope | Description |
|---|---|---|
| `project:read` | Project | Read project data, issues, MRs, pipelines |
| `project:write` | Project | Create/update issues, MRs, trigger pipelines |
| `project:delete` | Project | Delete projects |
| `agent:run` | Project | Start agent sessions |
| `agent:spawn` | Project | Spawn child agent sessions |
| `deploy:read` | Project | View deployments |
| `deploy:promote` | Project | Promote/rollback deployments |
| `observe:read` | Global/Project | Read logs, metrics, traces |
| `observe:write` | Global/Project | Write observability data (OTLP ingest) |
| `alert:manage` | Global | Create/manage alert rules |
| `secret:read` | Project | Read secret metadata (not values) |
| `secret:write` | Project | Create/update/delete secrets |
| `admin:users` | Global | Manage users, roles, service accounts |
| `admin:roles` | Global | Manage role definitions |
| `admin:config` | Global | Manage platform configuration |
| `admin:delegate` | Global | Create/revoke delegations |
| `workspace:read` | Workspace | Read workspace data |
| `workspace:write` | Workspace | Create/update workspaces |
| `workspace:admin` | Workspace | Manage workspace members/settings |
| `registry:pull` | Project | Pull images from registry |
| `registry:push` | Project | Push images to registry |

---

## 2. Role Definitions (10 roles)

### System Roles (immutable)

| Role | Permissions | Count |
|---|---|---|
| **admin** | ALL 21 permissions | 21 |
| **developer** | project:read/write, agent:run/spawn, deploy:read, observe:read, secret:read, workspace:read/write, registry:pull/push | 11 |
| **ops** | deploy:read/promote, observe:read/write, alert:manage, secret:read, registry:pull | 7 |
| **viewer** | project:read, observe:read, deploy:read, registry:pull | 4 |
| **agent** | (none — legacy fallback) | 0 |

### Agent Roles (customizable by admins)

| Role | Permissions | Count |
|---|---|---|
| **agent-dev** | project:read/write, secret:read, registry:pull/push | 5 |
| **agent-ops** | project:read, deploy:read/promote, observe:read/write, alert:manage, secret:read, registry:pull | 8 |
| **agent-test** | project:read, observe:read, registry:pull | 3 |
| **agent-review** | project:read, observe:read | 2 |
| **agent-manager** | project:read/write, agent:run/spawn, deploy:read, observe:read, workspace:read | 7 |

---

## 3. Permission Resolution Chain

```
effective_permissions(user_id, project_id) =
  UNION(
    global_role_permissions,          -- user_roles WHERE project_id IS NULL
    project_role_permissions,         -- user_roles WHERE project_id = $pid
    global_delegations,               -- active, not revoked, not expired
    project_delegations,              -- active, not revoked, not expired
    workspace_implicit_permissions    -- owner/admin → read+write, member → read
  )
  ∩ token_scopes                     -- API tokens intersect with role perms
```

- **Cached** in Valkey: `perms:{user_id}:{project_id}`, TTL 300s (configurable via `PLATFORM_PERMISSION_CACHE_TTL`)
- **Invalidated** on every role/delegation change via `invalidate_permissions()`

### Workspace-Derived Permissions

If a project belongs to a workspace:
- Workspace **owner/admin** → implicit `ProjectRead` + `ProjectWrite`
- Workspace **member** → implicit `ProjectRead`

### Token Scope Intersection

- `token_scopes = None` (session auth) → unrestricted by scopes
- `token_scopes = Some([])` or `Some(["*"])` → unrestricted token
- `token_scopes = Some(["project:read", ...])` → permission must be in BOTH roles AND scopes

---

## 4. Authentication Architecture

### AuthUser Extractor (`src/auth/middleware.rs`)

```rust
pub struct AuthUser {
    pub user_id: Uuid,
    pub user_name: String,
    pub user_type: UserType,
    pub ip_addr: Option<String>,
    pub token_scopes: Option<Vec<String>>,
    pub scope_workspace_id: Option<Uuid>,   // hard boundary from token
    pub scope_project_id: Option<Uuid>,     // hard boundary from token
}
```

**Extraction order:**
1. `Authorization: Bearer {token}` → `api_tokens` table (extracts scopes)
2. Session cookie → `auth_sessions` table (`token_scopes = None`)
3. Both fail → 401 Unauthorized

**Hard scope enforcement:**
- `check_project_scope(project_id)` → 404 if token scoped to different project
- `check_workspace_scope(workspace_id)` → 404 if token scoped to different workspace

---

## 5. Complete Endpoint Protection Matrix

### Unauthenticated Endpoints (7)

| Method | Path | Protection |
|---|---|---|
| POST | `/api/auth/login` | Rate limit: 10/5min, timing-safe verify |
| GET | `/api/setup/status` | Returns only `needs_setup: bool` |
| POST | `/api/setup` | Rate limit: 3/5min, single-use token required |
| POST | `/api/auth/passkey/login/begin` | WebAuthn challenge (no secrets) |
| POST | `/api/auth/passkey/login/complete` | WebAuthn credential verification |
| GET | `/healthz` | K8s probe, returns "ok" |
| GET | `/*` (fallback) | Static UI assets only |

---

### Auth & Users (`src/api/users.rs`) — 11 endpoints

| Method | Path | Auth | Permission |
|---|---|---|---|
| POST | `/api/auth/logout` | AuthUser | — |
| GET | `/api/auth/me` | AuthUser | — |
| POST | `/api/users` | AuthUser | `admin:users` |
| GET | `/api/users/list` | AuthUser | `admin:users` |
| GET | `/api/users/{id}` | AuthUser | self OR `admin:users` |
| PATCH | `/api/users/{id}` | AuthUser | self OR `admin:users` |
| DELETE | `/api/users/{id}` | AuthUser | self OR `admin:users` |
| POST | `/api/tokens` | AuthUser | — (own tokens) |
| GET | `/api/tokens` | AuthUser | — (own tokens) |
| DELETE | `/api/tokens/{id}` | AuthUser | owner OR `admin:users` |

### Admin & RBAC (`src/api/admin.rs`) — 12 endpoints

| Method | Path | Auth | Permission |
|---|---|---|---|
| GET | `/api/admin/roles` | AuthUser | `admin:users` |
| POST | `/api/admin/roles` | AuthUser | `admin:users` |
| GET | `/api/admin/roles/{id}/permissions` | AuthUser | `admin:users` |
| PUT | `/api/admin/roles/{id}/permissions` | AuthUser | `admin:users` |
| POST | `/api/admin/users/{id}/roles` | AuthUser | `admin:users` |
| DELETE | `/api/admin/users/{id}/roles/{role_id}` | AuthUser | `admin:users` |
| GET | `/api/admin/delegations` | AuthUser | `admin:delegate` |
| POST | `/api/admin/delegations` | AuthUser | `admin:delegate` |
| DELETE | `/api/admin/delegations/{id}` | AuthUser | `admin:delegate` |
| POST | `/api/admin/service-accounts` | AuthUser | `admin:users` |
| GET | `/api/admin/service-accounts` | AuthUser | `admin:users` |
| DELETE | `/api/admin/service-accounts/{id}` | AuthUser | `admin:users` |

### Projects (`src/api/projects.rs`) — 5 endpoints

| Method | Path | Auth | Permission |
|---|---|---|---|
| GET | `/api/projects` | AuthUser | visibility filter (public/internal/owned/RBAC) |
| POST | `/api/projects` | AuthUser | `project:write` (global) |
| GET | `/api/projects/{id}` | AuthUser | `project:read` or visibility |
| PATCH | `/api/projects/{id}` | AuthUser | `project:write` |
| DELETE | `/api/projects/{id}` | AuthUser | `admin:users` (global admin only) |

### Issues (`src/api/issues.rs`) — 7 endpoints

| Method | Path | Auth | Permission |
|---|---|---|---|
| GET | `/api/projects/{id}/issues` | AuthUser | `project:read` |
| POST | `/api/projects/{id}/issues` | AuthUser | `project:write` |
| GET | `/api/projects/{id}/issues/{n}` | AuthUser | `project:read` |
| PATCH | `/api/projects/{id}/issues/{n}` | AuthUser | `project:write` |
| GET | `/api/projects/{id}/issues/{n}/comments` | AuthUser | `project:read` |
| POST | `/api/projects/{id}/issues/{n}/comments` | AuthUser | `project:write` |
| PATCH | `/api/projects/{id}/issues/{n}/comments/{cid}` | AuthUser | `project:write` |

### Merge Requests (`src/api/merge_requests.rs`) — 10 endpoints

| Method | Path | Auth | Permission |
|---|---|---|---|
| GET | `/api/projects/{id}/merge-requests` | AuthUser | `project:read` |
| POST | `/api/projects/{id}/merge-requests` | AuthUser | `project:write` |
| GET | `/api/projects/{id}/merge-requests/{n}` | AuthUser | `project:read` |
| PATCH | `/api/projects/{id}/merge-requests/{n}` | AuthUser | `project:write` |
| POST | `/api/projects/{id}/merge-requests/{n}/merge` | AuthUser | `project:write` |
| GET | `/api/projects/{id}/merge-requests/{n}/reviews` | AuthUser | `project:read` |
| POST | `/api/projects/{id}/merge-requests/{n}/reviews` | AuthUser | `project:write` |
| GET | `/api/projects/{id}/merge-requests/{n}/comments` | AuthUser | `project:read` |
| POST | `/api/projects/{id}/merge-requests/{n}/comments` | AuthUser | `project:write` |
| PATCH | `/api/projects/{id}/merge-requests/{n}/comments/{cid}` | AuthUser | `project:write` |

### Webhooks (`src/api/webhooks.rs`) — 6 endpoints

| Method | Path | Auth | Permission |
|---|---|---|---|
| GET | `/api/projects/{id}/webhooks` | AuthUser | `project:read` |
| POST | `/api/projects/{id}/webhooks` | AuthUser | `project:write` + SSRF validation |
| GET | `/api/projects/{id}/webhooks/{wid}` | AuthUser | `project:read` |
| PATCH | `/api/projects/{id}/webhooks/{wid}` | AuthUser | `project:write` |
| DELETE | `/api/projects/{id}/webhooks/{wid}` | AuthUser | `project:write` |
| POST | `/api/projects/{id}/webhooks/{wid}/test` | AuthUser | `project:write` |

### Pipelines (`src/api/pipelines.rs`) — 7 endpoints

| Method | Path | Auth | Permission |
|---|---|---|---|
| GET | `/api/projects/{id}/pipelines` | AuthUser | `project:read` |
| POST | `/api/projects/{id}/pipelines` | AuthUser | `project:write` |
| GET | `/api/projects/{id}/pipelines/{pid}` | AuthUser | `project:read` |
| POST | `/api/projects/{id}/pipelines/{pid}/cancel` | AuthUser | `project:write` |
| GET | `.../steps/{sid}/logs` | AuthUser | `project:read` |
| GET | `.../artifacts` | AuthUser | `project:read` |
| GET | `.../artifacts/{aid}/download` | AuthUser | `project:read` |

### Deployments (`src/api/deployments.rs`) — 13 endpoints

| Method | Path | Auth | Permission |
|---|---|---|---|
| GET | `/api/projects/{id}/deployments` | AuthUser | `project:read` |
| GET | `/api/projects/{id}/deployments/{env}` | AuthUser | `project:read` |
| PATCH | `/api/projects/{id}/deployments/{env}` | AuthUser | `deploy:promote` |
| POST | `/api/projects/{id}/deployments/{env}/rollback` | AuthUser | `deploy:promote` |
| GET | `/api/projects/{id}/deployments/{env}/history` | AuthUser | `project:read` |
| GET | `/api/projects/{id}/previews` | AuthUser | `project:read` |
| GET | `/api/projects/{id}/previews/{slug}` | AuthUser | `project:read` |
| DELETE | `/api/projects/{id}/previews/{slug}` | AuthUser | `project:write` |
| GET | `/api/admin/ops-repos` | AuthUser | `admin:users` |
| POST | `/api/admin/ops-repos` | AuthUser | `admin:users` |
| GET | `/api/admin/ops-repos/{rid}` | AuthUser | `admin:users` |
| PATCH | `/api/admin/ops-repos/{rid}` | AuthUser | `admin:users` |
| DELETE | `/api/admin/ops-repos/{rid}` | AuthUser | `admin:users` |

### Sessions (`src/api/sessions.rs`) — 12 endpoints

| Method | Path | Auth | Permission |
|---|---|---|---|
| GET | `/api/projects/{id}/sessions` | AuthUser | `project:read` |
| POST | `/api/projects/{id}/sessions` | AuthUser | `agent:run` |
| GET | `/api/projects/{id}/sessions/{sid}` | AuthUser | `project:read` |
| POST | `.../message` | AuthUser | owner OR `project:write` |
| POST | `.../stop` | AuthUser | owner OR `project:write` |
| POST | `.../spawn` | AuthUser | `agent:run` |
| GET | `.../children` | AuthUser | `project:read` |
| GET | `.../ws` | AuthUser | `project:read` (WebSocket) |
| POST | `/api/create-app` | AuthUser | — |
| PATCH | `/api/sessions/{sid}` | AuthUser | owner OR `project:write` |
| POST | `/api/sessions/{sid}/message` | AuthUser | owner OR `project:write` |
| GET | `/api/sessions/{sid}/ws` | AuthUser | owner OR `project:write` |

### Secrets (`src/api/secrets.rs`) — 13 endpoints

| Method | Path | Auth | Permission |
|---|---|---|---|
| GET | `/api/projects/{id}/secrets` | AuthUser | `secret:read` |
| POST | `/api/projects/{id}/secrets` | AuthUser | `secret:write` |
| DELETE | `/api/projects/{id}/secrets/{name}` | AuthUser | `secret:write` |
| GET | `/api/projects/{id}/secret-requests` | AuthUser | `secret:read` |
| POST | `/api/projects/{id}/secret-requests` | AuthUser | `agent:run` |
| GET | `/api/projects/{id}/secret-requests/{rid}` | AuthUser | `secret:read` or owner |
| POST | `/api/projects/{id}/secret-requests/{rid}` | AuthUser | `secret:write` |
| GET | `/api/workspaces/{id}/secrets` | AuthUser | workspace member |
| POST | `/api/workspaces/{id}/secrets` | AuthUser | workspace admin |
| DELETE | `/api/workspaces/{id}/secrets/{name}` | AuthUser | workspace admin |
| GET | `/api/admin/secrets` | AuthUser | `admin:users` |
| POST | `/api/admin/secrets` | AuthUser | `admin:users` |
| DELETE | `/api/admin/secrets/{name}` | AuthUser | `admin:users` |

### Workspaces (`src/api/workspaces.rs`) — 9 endpoints

| Method | Path | Auth | Permission |
|---|---|---|---|
| GET | `/api/workspaces` | AuthUser | — (own memberships) |
| POST | `/api/workspaces` | AuthUser | — (becomes owner) |
| GET | `/api/workspaces/{id}` | AuthUser | workspace member |
| PATCH | `/api/workspaces/{id}` | AuthUser | workspace admin |
| DELETE | `/api/workspaces/{id}` | AuthUser | workspace admin |
| GET | `/api/workspaces/{id}/members` | AuthUser | workspace member |
| POST | `/api/workspaces/{id}/members` | AuthUser | workspace admin |
| DELETE | `/api/workspaces/{id}/members/{uid}` | AuthUser | workspace admin |
| GET | `/api/workspaces/{id}/projects` | AuthUser | workspace member |

### Other API Modules

| Module | File | Endpoints | Auth | Permissions |
|---|---|---|---|---|
| Passkeys | `passkeys.rs` | 7 (2 unauth for login) | AuthUser (5) | — (own passkeys) |
| SSH Keys | `ssh_keys.rs` | 4 | AuthUser | — (own) + `admin:users` (admin views) |
| GPG Keys | `gpg_keys.rs` | 5 | AuthUser | — (own) + `admin:users` (admin views) |
| Provider Keys | `user_keys.rs` | 4 | AuthUser | — (own keys) |
| Notifications | `notifications.rs` | 3 | AuthUser | — (own notifications) |
| Dashboard | `dashboard.rs` | 3 | AuthUser | `admin:users` (stats/audit), — (onboarding) |
| CLI Auth | `cli_auth.rs` | 3 | AuthUser | — (own credentials) |
| Commands | `commands.rs` | 6 | AuthUser | `admin:config` (global) / `project:write` (project) |

---

### Non-API Routes

| Category | Route Pattern | Auth Method | Permission |
|---|---|---|---|
| **Git Smart HTTP** (read) | `/{o}/{r}/info/refs`, `git-upload-pack` | HTTP Basic | Public repos: none; Private: `project:read` |
| **Git Smart HTTP** (write) | `git-receive-pack` | HTTP Basic | `project:write` (always required) |
| **Git LFS** | `info/lfs/objects/batch` | HTTP Basic | `project:read` (download), `project:write` (upload) |
| **OCI Registry** | `/v2/**` | Bearer/Basic | `registry:pull` (read), `registry:push` (write) |
| **OTLP Ingest** | `/v1/traces,logs,metrics` | AuthUser | `observe:write` per project, rate-limited 1000/min |
| **Observe Query** | `/api/observe/**` | AuthUser | `observe:read`, optional project scoping |
| **Observe Alerts** | `/api/observe/alerts/**` | AuthUser | `observe:read` (read), `alert:manage` (write) |

---

## 6. Security Defense Layers

| Layer | Implementation | Status |
|---|---|---|
| **Authentication** | Bearer token + session cookie + HTTP Basic (Git) | All endpoints covered |
| **Authorization** | 21 discrete permissions, RBAC resolver with caching | All mutations checked |
| **Token Scoping** | API tokens intersected with role permissions | Enforced at resolution |
| **Project Scope** | Hard boundary from scoped tokens (`check_project_scope`) | 404 on mismatch |
| **Workspace Scope** | Hard boundary from scoped tokens (`check_workspace_scope`) | 404 on mismatch |
| **Existence Hiding** | 404 (not 403) for private resources | Consistent pattern |
| **Rate Limiting** | Login (10/5min), Setup (3/5min), OTLP (1000/min), CLI (10/5min) | Applied to entry points |
| **Timing-Safe Auth** | Dummy hash for non-existent users | Prevents enumeration |
| **SSRF Protection** | Webhook URL validation (private IPs, metadata, non-HTTP) | Blocks outbound abuse |
| **Input Validation** | Field length limits, format checks via `validation.rs` | All handlers |
| **Audit Logging** | All mutations → `audit_log` with actor, resource, IP | Comprehensive |
| **Encryption at Rest** | Secrets use AES-256-GCM with `PLATFORM_MASTER_KEY` | Never logged |
| **Session Security** | HttpOnly, SameSite=Strict, optional Secure flag | Cookie hardened |
| **Cache Invalidation** | Permission cache cleared on role/delegation changes | Prevents stale grants |

---

## 7. Authorization Patterns Used

### Pattern 1: Inline Admin Check

```rust
async fn require_admin(state: &AppState, auth: &AuthUser) -> Result<(), ApiError> {
    let allowed = resolver::has_permission_scoped(
        &state.pool, &state.valkey, auth.user_id, None,
        Permission::AdminUsers, auth.token_scopes.as_deref(),
    ).await.map_err(ApiError::Internal)?;
    if !allowed { return Err(ApiError::Forbidden); }
    Ok(())
}
```

Used by: `admin.rs`, `secrets.rs` (global), `dashboard.rs`, `ssh_keys.rs`, `gpg_keys.rs`

### Pattern 2: Project-Scoped Read/Write Helpers

```rust
pub async fn require_project_read(state: &AppState, auth: &AuthUser, project_id: Uuid) -> Result<(), ApiError> {
    auth.check_project_scope(project_id)?;        // hard scope from token
    // check visibility (public/internal) → allow
    // check ownership → allow
    // check RBAC project:read → allow or 404
}

pub async fn require_project_write(state: &AppState, auth: &AuthUser, project_id: Uuid) -> Result<(), ApiError> {
    auth.check_project_scope(project_id)?;
    // check RBAC project:write → allow or Forbidden
}
```

Used by: `issues.rs`, `merge_requests.rs`, `pipelines.rs`, `deployments.rs`, `sessions.rs`, `webhooks.rs`

### Pattern 3: Resource Owner OR Permission

```rust
async fn require_session_write(state: &AppState, auth: &AuthUser, project_id: Uuid, session_user_id: Uuid) -> Result<(), ApiError> {
    auth.check_project_scope(project_id)?;
    if auth.user_id == session_user_id { return Ok(()); }  // owner bypass
    // check project:write RBAC
}
```

Used by: `sessions.rs` (message, stop, spawn), `secrets.rs` (secret-requests)

### Pattern 4: Workspace Membership Check

```rust
// workspace member → read access
// workspace admin → write access
let membership = service::get_membership(&state.pool, workspace_id, auth.user_id).await?;
if membership.is_none() { return Err(ApiError::NotFound(...)); }
if membership.role != "admin" { return Err(ApiError::Forbidden); }
```

Used by: `workspaces.rs`, `secrets.rs` (workspace secrets)

---

## 8. Delegation System

### Structure

```sql
CREATE TABLE delegations (
    id            UUID PRIMARY KEY,
    delegator_id  UUID NOT NULL REFERENCES users(id),
    delegate_id   UUID NOT NULL REFERENCES users(id),
    permission_id UUID NOT NULL REFERENCES permissions(id),
    project_id    UUID REFERENCES projects(id),  -- NULL = global
    expires_at    TIMESTAMPTZ,                   -- NULL = never
    reason        TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at    TIMESTAMPTZ                    -- soft-delete
);
```

### Rules

- Delegator must already hold the permission being delegated
- Self-delegation prevented
- Time-bound with optional expiry
- Soft-revoke (sets `revoked_at`, not hard delete)
- Cache invalidated for delegate on create/revoke

---

## 9. Findings & Gaps

### No unprotected endpoints found

Every mutating endpoint requires authentication + appropriate permission checks. Read endpoints on private resources return 404.

### Minor gaps worth noting

| Gap | Risk | Recommendation |
|---|---|---|
| No rate limit on password change (`PATCH /api/users/{id}`) | Low — requires valid session | Add 5/hour limit |
| No rate limit on token creation (`POST /api/tokens`) | Low — requires valid session | Add 20/hour limit |
| No rate limit on SSH key add (`POST /api/ssh-keys`) | Low — requires admin perm | Add 10/hour limit |
| Force-push rejection (Plan 03) not implemented | Medium — no branch protection API | Implement branch protection rules |
| Legacy `agent` role has 0 permissions | None — unused fallback | Remove or document |
| `admin:roles` permission defined but `admin.rs` uses `admin:users` | None — `admin:roles` unused | Consider consolidating |

### Strengths

- **Consistent patterns**: Every module uses the same `require_project_read/write` helpers
- **Token scope intersection**: API tokens can never escalate beyond their declared scopes
- **Workspace-derived access**: Implicit permissions simplify team collaboration
- **Delegation system**: Time-bound permission grants with audit trail
- **All WebSocket endpoints authenticated**: WS upgrades go through same AuthUser extraction
- **Existence hiding**: Private resources return 404 (not 403) consistently
- **SSRF protection**: Webhook URLs validated against private IPs, metadata endpoints
- **Timing-safe auth**: Prevents user enumeration via password verification timing

---

## 10. Source Files Reference

| File | Purpose |
|---|---|
| `src/rbac/types.rs` | Permission + Role enums |
| `src/rbac/resolver.rs` | Permission resolution + caching |
| `src/rbac/delegation.rs` | Delegation CRUD |
| `src/rbac/middleware.rs` | `require_permission` route layer |
| `src/auth/middleware.rs` | `AuthUser` extractor |
| `src/auth/password.rs` | Password hashing + timing-safe verify |
| `src/auth/rate_limit.rs` | Rate limiting via Valkey |
| `src/api/helpers.rs` | `require_project_read/write` helpers |
| `src/api/admin.rs` | `require_admin/require_delegate` helpers |
| `src/store/bootstrap.rs` | Role + permission seeding |
| `src/validation.rs` | Input validation utilities |
