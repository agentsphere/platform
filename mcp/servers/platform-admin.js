/**
 * Platform Admin MCP Server
 *
 * Provides user, role, delegation, and token management tools.
 * Loaded for: admin role only.
 */

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { ListToolsRequestSchema, CallToolRequestSchema } from "@modelcontextprotocol/sdk/types.js";
import { apiGet, apiPost, apiPut, apiPatch, apiDelete } from "../lib/client.js";

const server = new Server(
  { name: "platform-admin", version: "0.1.0" },
  { capabilities: { tools: {} } },
);

const TOOLS = [
  // --- Users ---
  {
    name: "list_users",
    description:
      "List all platform users. Returns user ID, name, email, active status, user type, and creation date. Supports pagination.",
    inputSchema: {
      type: "object",
      properties: {
        limit: { type: "number", description: "Max results (default 50, max 100)" },
        offset: { type: "number", description: "Offset for pagination" },
      },
    },
  },
  {
    name: "get_user",
    description:
      "Get detailed information about a specific user including their roles and active delegations.",
    inputSchema: {
      type: "object",
      properties: {
        user_id: { type: "string", description: "User UUID (required)" },
      },
      required: ["user_id"],
    },
  },
  {
    name: "create_user",
    description:
      "Create a new platform user. Returns the created user with ID. The user can then login with the provided credentials.",
    inputSchema: {
      type: "object",
      properties: {
        name: { type: "string", description: "Username (1-255 chars, alphanumeric + -_.)" },
        email: { type: "string", description: "Email address" },
        password: { type: "string", description: "Password (8-1024 chars)" },
        display_name: { type: "string", description: "Optional display name" },
        user_type: {
          type: "string",
          enum: ["human", "agent", "service"],
          description: "User type (default: human)",
        },
      },
      required: ["name", "email", "password"],
    },
  },
  {
    name: "update_user",
    description: "Update user fields (display name, email). Cannot change username.",
    inputSchema: {
      type: "object",
      properties: {
        user_id: { type: "string", description: "User UUID (required)" },
        display_name: { type: "string", description: "New display name" },
        email: { type: "string", description: "New email address" },
      },
      required: ["user_id"],
    },
  },
  {
    name: "deactivate_user",
    description:
      "Deactivate a user. This deletes all their sessions and API tokens, and invalidates their permission cache. The user cannot login until reactivated.",
    inputSchema: {
      type: "object",
      properties: {
        user_id: { type: "string", description: "User UUID to deactivate (required)" },
      },
      required: ["user_id"],
    },
  },
  // --- Roles ---
  {
    name: "list_roles",
    description: "List all roles (system and custom) with their assigned permissions.",
    inputSchema: {
      type: "object",
      properties: {
        limit: { type: "number", description: "Max results" },
        offset: { type: "number", description: "Offset" },
      },
    },
  },
  {
    name: "create_role",
    description:
      "Create a new custom role with specified permissions. System roles (admin, developer, viewer, agent, auditor) cannot be created this way.",
    inputSchema: {
      type: "object",
      properties: {
        name: { type: "string", description: "Role name (1-255 chars)" },
        description: { type: "string", description: "Role description" },
        permissions: {
          type: "array",
          items: { type: "string" },
          description: "List of permission strings (e.g., 'project:read', 'deploy:promote')",
        },
      },
      required: ["name", "permissions"],
    },
  },
  {
    name: "assign_role",
    description:
      "Assign a role to a user, optionally scoped to a specific project. Global roles apply to all projects.",
    inputSchema: {
      type: "object",
      properties: {
        user_id: { type: "string", description: "User UUID (required)" },
        role_id: { type: "string", description: "Role UUID (required)" },
        project_id: {
          type: "string",
          description: "Project UUID (optional, for project-scoped roles)",
        },
      },
      required: ["user_id", "role_id"],
    },
  },
  {
    name: "remove_role",
    description: "Remove a role from a user.",
    inputSchema: {
      type: "object",
      properties: {
        user_id: { type: "string", description: "User UUID (required)" },
        role_id: { type: "string", description: "Role UUID (required)" },
      },
      required: ["user_id", "role_id"],
    },
  },
  // --- Delegations ---
  {
    name: "list_delegations",
    description:
      "List active permission delegations. Shows who delegated what permission to whom, with expiry dates.",
    inputSchema: {
      type: "object",
      properties: {
        user_id: { type: "string", description: "Filter by user (as delegator or delegate)" },
        limit: { type: "number", description: "Max results" },
        offset: { type: "number", description: "Offset" },
      },
    },
  },
  {
    name: "create_delegation",
    description:
      "Delegate a permission to another user. The delegator must hold the permission themselves. Delegations can be time-limited and project-scoped.",
    inputSchema: {
      type: "object",
      properties: {
        delegate_id: { type: "string", description: "User receiving the permission (required)" },
        permission: { type: "string", description: "Permission string (required)" },
        project_id: { type: "string", description: "Scope to project (optional)" },
        expires_at: { type: "string", description: "Expiry time ISO 8601 (optional)" },
        reason: { type: "string", description: "Reason for delegation (optional)" },
      },
      required: ["delegate_id", "permission"],
    },
  },
  {
    name: "revoke_delegation",
    description:
      "Revoke a previously created delegation. The delegated permission is immediately removed.",
    inputSchema: {
      type: "object",
      properties: {
        delegation_id: { type: "string", description: "Delegation UUID to revoke (required)" },
      },
      required: ["delegation_id"],
    },
  },
];

server.setRequestHandler(ListToolsRequestSchema, async () => ({ tools: TOOLS }));

server.setRequestHandler(CallToolRequestSchema, async (request) => {
  const { name, arguments: args = {} } = request.params;

  try {
    switch (name) {
      // --- Users ---
      case "list_users": {
        const data = await apiGet("/api/users/list", {
          query: { limit: args.limit, offset: args.offset },
        });
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      case "get_user": {
        const data = await apiGet(`/api/users/${args.user_id}`);
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      case "create_user": {
        const data = await apiPost("/api/users", {
          body: {
            name: args.name,
            email: args.email,
            password: args.password,
            display_name: args.display_name,
            user_type: args.user_type,
          },
        });
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      case "update_user": {
        const body = {};
        if (args.display_name !== undefined) body.display_name = args.display_name;
        if (args.email !== undefined) body.email = args.email;
        const data = await apiPatch(`/api/users/${args.user_id}`, { body });
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      case "deactivate_user": {
        const data = await apiDelete(`/api/users/${args.user_id}`);
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      // --- Roles ---
      case "list_roles": {
        const data = await apiGet("/api/admin/roles", {
          query: { limit: args.limit, offset: args.offset },
        });
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      case "create_role": {
        const role = await apiPost("/api/admin/roles", {
          body: { name: args.name, description: args.description },
        });
        if (args.permissions && args.permissions.length > 0) {
          await apiPut(`/api/admin/roles/${role.id}/permissions`, {
            body: { permissions: args.permissions },
          });
        }
        return { content: [{ type: "text", text: JSON.stringify(role, null, 2) }] };
      }
      case "assign_role": {
        const data = await apiPost(`/api/admin/users/${args.user_id}/roles`, {
          body: { role_id: args.role_id, project_id: args.project_id },
        });
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      case "remove_role": {
        const data = await apiDelete(
          `/api/admin/users/${args.user_id}/roles/${args.role_id}`,
        );
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      // --- Delegations ---
      case "list_delegations": {
        const data = await apiGet("/api/admin/delegations", {
          query: { user_id: args.user_id, limit: args.limit, offset: args.offset },
        });
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      case "create_delegation": {
        const data = await apiPost("/api/admin/delegations", {
          body: {
            delegate_id: args.delegate_id,
            permission: args.permission,
            project_id: args.project_id,
            expires_at: args.expires_at,
            reason: args.reason,
          },
        });
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      case "revoke_delegation": {
        const data = await apiDelete(`/api/admin/delegations/${args.delegation_id}`);
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      default:
        throw new Error(`Unknown tool: ${name}`);
    }
  } catch (err) {
    return {
      content: [{ type: "text", text: `Error: ${err.message}` }],
      isError: true,
    };
  }
});

const transport = new StdioServerTransport();
await server.connect(transport);
