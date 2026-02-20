# Plan 22 — Admin MCP Server

## Overview

Create the `platform-admin` MCP server, completing the MCP server suite. This server enables `admin`-role agents to manage users, roles, delegations, and platform configuration through structured tool calls. It is the final MCP server in the role-based composition matrix.

**This corresponds to Agent DX Phase E from Plan 14.**

---

## Motivation

- **Admin agents are incomplete**: The `admin` role currently gets core + pipeline + issues + deploy + observe MCP tools but cannot manage users, roles, or delegations
- **Automation of admin tasks**: Agent-driven user onboarding, role assignment, and delegation management reduces operational overhead
- **Completes the MCP suite**: After this, all 6 planned MCP servers are implemented:
  1. `platform-core.js` (complete)
  2. `platform-pipeline.js` (complete)
  3. `platform-issues.js` (complete)
  4. `platform-deploy.js` (Plan 18)
  5. `platform-observe.js` (Plan 18)
  6. `platform-admin.js` (this plan)

---

## Prerequisites

| Requirement | Status |
|---|---|
| MCP infrastructure (lib/client.js) | Complete |
| Admin API (`src/api/admin.rs`) | Complete — user CRUD, roles, delegations, permissions |
| Role-based entrypoint | Complete — `admin` case already references `platform-admin.js` |
| Admin permission delegation to agents | Requires `AdminUsers` + `AdminRoles` in extra_permissions (Plan 18) |

---

## Architecture

### Role Access

The admin MCP server is **only** loaded for agents with `AGENT_ROLE=admin`. The entrypoint already configures this:

```bash
admin)
    for server in platform-pipeline platform-issues platform-deploy platform-observe platform-admin; do
      MCP_JSON+=',"'$server'":{"command":"node","args":["'$MCP_DIR'/'$server'.js"]}'
    done
    ;;
```

### Permission Requirements

Admin tools require `AdminUsers`, `AdminRoles`, and/or `AdminConfig` permissions. These must be delegated to the agent identity when `delegate_admin: true` is set on session creation.

**Modify: `src/api/sessions.rs`** — add `delegate_admin` flag:

```rust
pub struct CreateSessionRequest {
    pub prompt: String,
    pub provider: Option<String>,
    pub branch: Option<String>,
    pub config: Option<ProviderConfig>,
    pub delegate_deploy: bool,
    pub delegate_observe: bool,
    pub delegate_admin: bool,  // NEW
}
```

When `delegate_admin` is true, add to extra permissions:
- `Permission::AdminUsers`
- `Permission::AdminRoles`
- `Permission::AdminConfig`

---

## Detailed Implementation

### Step E1: `mcp/servers/platform-admin.js` (~200 lines)

**New file.** Wraps the admin management API.

#### Tools

| # | Tool Name | Method | API Endpoint | Required Permission |
|---|-----------|--------|-------------|---------------------|
| 1 | `list_users` | GET | `/api/admin/users` | AdminUsers |
| 2 | `get_user` | GET | `/api/admin/users/{id}` | AdminUsers |
| 3 | `create_user` | POST | `/api/admin/users` | AdminUsers |
| 4 | `update_user` | PATCH | `/api/admin/users/{id}` | AdminUsers |
| 5 | `deactivate_user` | POST | `/api/admin/users/{id}/deactivate` | AdminUsers |
| 6 | `list_roles` | GET | `/api/admin/roles` | AdminRoles |
| 7 | `create_role` | POST | `/api/admin/roles` | AdminRoles |
| 8 | `assign_role` | POST | `/api/admin/users/{id}/roles` | AdminRoles |
| 9 | `remove_role` | DELETE | `/api/admin/users/{id}/roles/{role_id}` | AdminRoles |
| 10 | `list_permissions` | GET | `/api/admin/permissions` | AdminRoles |
| 11 | `list_delegations` | GET | `/api/admin/delegations` | AdminRoles |
| 12 | `create_delegation` | POST | `/api/admin/delegations` | AdminRoles |
| 13 | `revoke_delegation` | DELETE | `/api/admin/delegations/{id}` | AdminRoles |
| 14 | `create_token_for_user` | POST | `/api/admin/users/{id}/tokens` | AdminUsers |

#### Tool Definitions

```javascript
const tools = [
  {
    name: "list_users",
    description: "List all platform users. Returns user ID, name, email, active status, user type, and creation date. Supports pagination.",
    inputSchema: {
      type: "object",
      properties: {
        limit: { type: "number", description: "Max results (default 50, max 100)" },
        offset: { type: "number", description: "Offset for pagination" },
        search: { type: "string", description: "Search by name or email" }
      }
    }
  },
  {
    name: "get_user",
    description: "Get detailed information about a specific user including their roles and active delegations.",
    inputSchema: {
      type: "object",
      properties: {
        user_id: { type: "string", description: "User UUID (required)" }
      },
      required: ["user_id"]
    }
  },
  {
    name: "create_user",
    description: "Create a new platform user. Returns the created user with ID. The user can then login with the provided credentials.",
    inputSchema: {
      type: "object",
      properties: {
        name: { type: "string", description: "Username (1-255 chars, alphanumeric + -_.)" },
        email: { type: "string", description: "Email address" },
        password: { type: "string", description: "Password (8-1024 chars)" },
        display_name: { type: "string", description: "Optional display name" },
        user_type: { type: "string", enum: ["human", "agent", "service"], description: "User type (default: human)" }
      },
      required: ["name", "email", "password"]
    }
  },
  {
    name: "update_user",
    description: "Update user fields (display name, email). Cannot change username.",
    inputSchema: {
      type: "object",
      properties: {
        user_id: { type: "string", description: "User UUID (required)" },
        display_name: { type: "string", description: "New display name" },
        email: { type: "string", description: "New email address" }
      },
      required: ["user_id"]
    }
  },
  {
    name: "deactivate_user",
    description: "Deactivate a user. This deletes all their sessions and API tokens, and invalidates their permission cache. The user cannot login until reactivated.",
    inputSchema: {
      type: "object",
      properties: {
        user_id: { type: "string", description: "User UUID to deactivate (required)" }
      },
      required: ["user_id"]
    }
  },
  {
    name: "list_roles",
    description: "List all roles (system and custom) with their assigned permissions.",
    inputSchema: {
      type: "object",
      properties: {
        limit: { type: "number", description: "Max results" },
        offset: { type: "number", description: "Offset" }
      }
    }
  },
  {
    name: "create_role",
    description: "Create a new custom role with specified permissions. System roles (admin, developer, viewer, agent, auditor) cannot be created this way.",
    inputSchema: {
      type: "object",
      properties: {
        name: { type: "string", description: "Role name (1-255 chars)" },
        description: { type: "string", description: "Role description" },
        permissions: {
          type: "array",
          items: { type: "string" },
          description: "List of permission strings (e.g., 'project:read', 'deploy:promote')"
        }
      },
      required: ["name", "permissions"]
    }
  },
  {
    name: "assign_role",
    description: "Assign a role to a user, optionally scoped to a specific project. Global roles apply to all projects.",
    inputSchema: {
      type: "object",
      properties: {
        user_id: { type: "string", description: "User UUID (required)" },
        role_id: { type: "string", description: "Role UUID (required)" },
        project_id: { type: "string", description: "Project UUID (optional, for project-scoped roles)" }
      },
      required: ["user_id", "role_id"]
    }
  },
  {
    name: "remove_role",
    description: "Remove a role assignment from a user.",
    inputSchema: {
      type: "object",
      properties: {
        user_id: { type: "string", description: "User UUID (required)" },
        role_assignment_id: { type: "string", description: "Role assignment UUID (required)" }
      },
      required: ["user_id", "role_assignment_id"]
    }
  },
  {
    name: "list_permissions",
    description: "List all available permissions in the system. Useful for understanding what permissions can be assigned to roles.",
    inputSchema: { type: "object", properties: {} }
  },
  {
    name: "list_delegations",
    description: "List active permission delegations. Shows who delegated what permission to whom, with expiry dates.",
    inputSchema: {
      type: "object",
      properties: {
        user_id: { type: "string", description: "Filter by user (as delegator or delegate)" },
        limit: { type: "number" },
        offset: { type: "number" }
      }
    }
  },
  {
    name: "create_delegation",
    description: "Delegate a permission to another user. The delegator must hold the permission themselves. Delegations can be time-limited and project-scoped.",
    inputSchema: {
      type: "object",
      properties: {
        delegate_id: { type: "string", description: "User receiving the permission (required)" },
        permission: { type: "string", description: "Permission string (required)" },
        project_id: { type: "string", description: "Scope to project (optional)" },
        expires_at: { type: "string", description: "Expiry time ISO 8601 (optional)" },
        reason: { type: "string", description: "Reason for delegation (optional)" }
      },
      required: ["delegate_id", "permission"]
    }
  },
  {
    name: "revoke_delegation",
    description: "Revoke a previously created delegation. The delegated permission is immediately removed.",
    inputSchema: {
      type: "object",
      properties: {
        delegation_id: { type: "string", description: "Delegation UUID to revoke (required)" }
      },
      required: ["delegation_id"]
    }
  },
  {
    name: "create_token_for_user",
    description: "Create an API token for a specific user. The raw token is shown once — it cannot be retrieved later. Useful for creating service account tokens.",
    inputSchema: {
      type: "object",
      properties: {
        user_id: { type: "string", description: "User UUID (required)" },
        name: { type: "string", description: "Token name (required)" },
        scopes: {
          type: "array",
          items: { type: "string" },
          description: "Token scopes (e.g., ['project:read', 'project:write'])"
        },
        expires_days: { type: "number", description: "Token lifetime in days (1-365, default 90)" }
      },
      required: ["user_id", "name"]
    }
  }
];
```

#### Handler Implementation

```javascript
async function handleTool(name, args) {
  try {
    switch (name) {
      case "list_users":
        return ok(await apiGet("/api/admin/users", {
          query: { limit: args.limit, offset: args.offset, search: args.search }
        }));

      case "get_user":
        return ok(await apiGet(`/api/admin/users/${args.user_id}`));

      case "create_user":
        return ok(await apiPost("/api/admin/users", {
          body: {
            name: args.name,
            email: args.email,
            password: args.password,
            display_name: args.display_name,
            user_type: args.user_type,
          }
        }));

      case "update_user":
        return ok(await apiPatch(`/api/admin/users/${args.user_id}`, {
          body: { display_name: args.display_name, email: args.email }
        }));

      case "deactivate_user":
        return ok(await apiPost(`/api/admin/users/${args.user_id}/deactivate`));

      case "list_roles":
        return ok(await apiGet("/api/admin/roles", {
          query: { limit: args.limit, offset: args.offset }
        }));

      case "create_role":
        return ok(await apiPost("/api/admin/roles", {
          body: { name: args.name, description: args.description, permissions: args.permissions }
        }));

      case "assign_role":
        return ok(await apiPost(`/api/admin/users/${args.user_id}/roles`, {
          body: { role_id: args.role_id, project_id: args.project_id }
        }));

      case "remove_role":
        return ok(await apiDelete(`/api/admin/users/${args.user_id}/roles/${args.role_assignment_id}`));

      case "list_permissions":
        return ok(await apiGet("/api/admin/permissions"));

      case "list_delegations":
        return ok(await apiGet("/api/admin/delegations", {
          query: { user_id: args.user_id, limit: args.limit, offset: args.offset }
        }));

      case "create_delegation":
        return ok(await apiPost("/api/admin/delegations", {
          body: {
            delegate_id: args.delegate_id,
            permission: args.permission,
            project_id: args.project_id,
            expires_at: args.expires_at,
            reason: args.reason,
          }
        }));

      case "revoke_delegation":
        return ok(await apiDelete(`/api/admin/delegations/${args.delegation_id}`));

      case "create_token_for_user":
        return ok(await apiPost(`/api/admin/users/${args.user_id}/tokens`, {
          body: { name: args.name, scopes: args.scopes, expires_days: args.expires_days }
        }));

      default:
        return { content: [{ type: "text", text: `Unknown tool: ${name}` }], isError: true };
    }
  } catch (err) {
    return { content: [{ type: "text", text: `Error: ${err.message}` }], isError: true };
  }
}

function ok(result) {
  return { content: [{ type: "text", text: JSON.stringify(result, null, 2) }] };
}
```

---

### Step E2: Extend Session Creation for Admin Delegation

**Modify: `src/api/sessions.rs`** — add `delegate_admin` handling:

```rust
// In extra_permissions construction:
if body.delegate_admin {
    extra_permissions.push(Permission::AdminUsers);
    extra_permissions.push(Permission::AdminRoles);
    extra_permissions.push(Permission::AdminConfig);
}
```

**Security consideration**: Only users who already hold admin permissions can delegate them. The `create_agent_identity()` function silently skips permissions the delegator lacks. So a non-admin user setting `delegate_admin: true` has no effect.

---

### Step E3: Validate Admin Role

**Modify: `src/api/sessions.rs`** — warn if role is `admin` but `delegate_admin` is false:

```rust
// After parsing config:
if let Some(ref config) = body.config {
    if config.role.as_deref() == Some("admin") && !body.delegate_admin {
        tracing::warn!(
            user_id = %auth.user_id,
            "agent session requested admin role without delegate_admin flag — admin MCP tools will get 403"
        );
    }
}
```

This is a soft warning, not an error — the agent will still start, but admin tools will fail with 403 because the agent lacks admin permissions.

---

## Entrypoint Configuration

The entrypoint (`docker/entrypoint.sh`) already loads `platform-admin.js` for the admin role. No changes needed.

```bash
admin)
    for server in platform-pipeline platform-issues platform-deploy platform-observe platform-admin; do
      MCP_JSON+=',"'$server'":{"command":"node","args":["'$MCP_DIR'/'$server'.js"]}'
    done
    ;;
```

---

## Files Changed

| File | Action | Description |
|------|--------|-------------|
| `mcp/servers/platform-admin.js` | **New** | Admin MCP server (14 tools, ~200 lines) |
| `src/api/sessions.rs` | **Modify** | Add `delegate_admin` flag, admin delegation logic |

No new migrations. No new Rust dependencies.

---

## Verification

### Automated
1. `just ci` passes
2. MCP server loads: `node mcp/servers/platform-admin.js` (verify tool list via stdio)

### Manual Testing

1. **Admin agent session**:
   ```bash
   curl -X POST /api/projects/{id}/sessions \
     -H "Authorization: Bearer $ADMIN_TOKEN" \
     -d '{"prompt":"list all users","config":{"role":"admin"},"delegate_admin":true}'
   ```
2. Verify agent can call `list_users`, `create_user`, `assign_role` tools
3. Verify non-admin user with `delegate_admin: true` → admin tools get 403

### Tool Coverage Matrix

| Tool | API Endpoint | Permission | Tested |
|------|-------------|------------|--------|
| list_users | GET /api/admin/users | AdminUsers | Manual |
| get_user | GET /api/admin/users/{id} | AdminUsers | Manual |
| create_user | POST /api/admin/users | AdminUsers | Manual |
| update_user | PATCH /api/admin/users/{id} | AdminUsers | Manual |
| deactivate_user | POST /api/admin/users/{id}/deactivate | AdminUsers | Manual |
| list_roles | GET /api/admin/roles | AdminRoles | Manual |
| create_role | POST /api/admin/roles | AdminRoles | Manual |
| assign_role | POST /api/admin/users/{id}/roles | AdminRoles | Manual |
| remove_role | DELETE /api/admin/users/{id}/roles/{rid} | AdminRoles | Manual |
| list_permissions | GET /api/admin/permissions | AdminRoles | Manual |
| list_delegations | GET /api/admin/delegations | AdminRoles | Manual |
| create_delegation | POST /api/admin/delegations | AdminRoles | Manual |
| revoke_delegation | DELETE /api/admin/delegations/{id} | AdminRoles | Manual |
| create_token_for_user | POST /api/admin/users/{id}/tokens | AdminUsers | Manual |

---

## Risks and Mitigations

| Risk | Impact | Mitigation |
|------|--------|-----------|
| Admin agent creates rogue users | Unauthorized accounts | Agent identity expires in 24h; all actions audit-logged |
| Agent deactivates admin user | Lockout | Audit log captures actor; can recover via DB |
| Permission escalation via delegation | Agent grants itself more perms | Delegation enforces delegator-holds check |
| Agent token with admin perms persists | Security gap | Tokens auto-expire (24h); cleaned up on session end |

---

## Security Notes

- **Audit trail**: All admin actions go through the platform API and are audit-logged with the agent's user ID
- **Time-limited**: Agent admin permissions expire with the session (24h max via delegation expiry)
- **Least privilege**: The delegation system ensures agents can only delegate permissions the original user holds
- **Token scope**: Agent API tokens have `agent:session` scope; they cannot create tokens with broader scope
- **No persistent elevation**: When the session ends, `cleanup_agent_identity()` revokes all delegations and deletes the agent user

---

## Estimated Scope

| Metric | Value |
|--------|-------|
| New files | 1 (JS) |
| Modified files | 1 (Rust) |
| New migrations | 0 |
| Estimated LOC | ~250 (200 JS + 50 Rust) |
| New MCP tools | 14 |
| Total MCP tools (all servers) | 36 (2 core + 7 pipeline + 11 issues + 8 deploy + 7 observe + 14 admin) |
