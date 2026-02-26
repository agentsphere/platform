/**
 * Platform Core MCP Server
 *
 * Provides project info and general platform queries.
 * Always loaded for every agent role.
 */

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { ListToolsRequestSchema, CallToolRequestSchema } from "@modelcontextprotocol/sdk/types.js";
import { apiGet, apiPost, apiPatch, apiDelete, PROJECT_ID } from "../lib/client.js";

const SESSION_ID = process.env.SESSION_ID || "";

const server = new Server(
  { name: "platform-core", version: "0.1.0" },
  { capabilities: { tools: {} } },
);

const TOOLS = [
  {
    name: "get_project",
    description: "Get project details (name, owner, visibility, default branch, description)",
    inputSchema: {
      type: "object",
      properties: {
        project_id: {
          type: "string",
          description: `Project UUID. Defaults to ${PROJECT_ID || "current project"}.`,
        },
      },
    },
  },
  {
    name: "list_projects",
    description: "List projects the agent has access to",
    inputSchema: {
      type: "object",
      properties: {
        limit: { type: "integer", description: "Max results (default 50, max 100)" },
        offset: { type: "integer", description: "Pagination offset" },
        search: { type: "string", description: "Search by name" },
      },
    },
  },
  {
    name: "spawn_agent",
    description:
      "Spawn a child agent session to work on a sub-task. " +
      "The child inherits your project context and runs in its own pod. " +
      "Requires agent:spawn permission.",
    inputSchema: {
      type: "object",
      properties: {
        prompt: {
          type: "string",
          description: "The task description / prompt for the child agent",
        },
        allowed_child_roles: {
          type: "array",
          items: { type: "string" },
          description: "Roles the child is allowed to spawn (e.g. ['dev', 'ops']). Optional.",
        },
      },
      required: ["prompt"],
    },
  },
  {
    name: "list_children",
    description: "List child agent sessions spawned from the current session",
    inputSchema: {
      type: "object",
      properties: {},
    },
  },
  {
    name: "create_project",
    description: "Create a new project with a bare git repository.",
    inputSchema: {
      type: "object",
      properties: {
        name: { type: "string", description: "Slug-style project name (lowercase, hyphens, 1-255 chars)" },
        display_name: { type: "string", description: "Human-readable display name (optional)" },
        description: { type: "string", description: "Project description (optional)" },
        visibility: { type: "string", description: "Visibility: public, internal, or private (default)" },
      },
      required: ["name"],
    },
  },
  {
    name: "update_project",
    description: "Update an existing project (display name, description, visibility, default branch).",
    inputSchema: {
      type: "object",
      properties: {
        project_id: { type: "string", description: "Project UUID" },
        display_name: { type: "string", description: "New display name" },
        description: { type: "string", description: "New description" },
        visibility: { type: "string", description: "New visibility (public/internal/private)" },
        default_branch: { type: "string", description: "New default branch" },
      },
      required: ["project_id"],
    },
  },
  {
    name: "delete_project",
    description: "Soft-delete a project (sets is_active=false). Requires project:write permission.",
    inputSchema: {
      type: "object",
      properties: {
        project_id: { type: "string", description: "Project UUID to delete" },
      },
      required: ["project_id"],
    },
  },
  {
    name: "get_session",
    description: "Get details of an agent session including messages.",
    inputSchema: {
      type: "object",
      properties: {
        session_id: { type: "string", description: "Session UUID" },
        project_id: { type: "string", description: "Project UUID (defaults to current)" },
      },
      required: ["session_id"],
    },
  },
  {
    name: "send_message_to_session",
    description: "Send a message to an agent session (must be in 'running' status).",
    inputSchema: {
      type: "object",
      properties: {
        session_id: { type: "string", description: "Session UUID" },
        content: { type: "string", description: "Message content" },
        project_id: { type: "string", description: "Project UUID (defaults to current)" },
      },
      required: ["session_id", "content"],
    },
  },
];

server.setRequestHandler(ListToolsRequestSchema, async () => ({ tools: TOOLS }));

server.setRequestHandler(CallToolRequestSchema, async (request) => {
  const { name, arguments: args = {} } = request.params;

  switch (name) {
    case "get_project": {
      const pid = args.project_id || PROJECT_ID;
      const data = await apiGet(`/api/projects/${pid}`);
      return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
    }
    case "list_projects": {
      const data = await apiGet("/api/projects", {
        query: { limit: args.limit, offset: args.offset, search: args.search },
      });
      return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
    }
    case "spawn_agent": {
      if (!SESSION_ID) throw new Error("SESSION_ID not set — cannot spawn child agents");
      if (!PROJECT_ID) throw new Error("PROJECT_ID not set — cannot spawn child agents");
      const payload = { prompt: args.prompt };
      if (args.allowed_child_roles) payload.allowed_child_roles = args.allowed_child_roles;
      const data = await apiPost(
        `/api/projects/${PROJECT_ID}/sessions/${SESSION_ID}/spawn`,
        { body: payload },
      );
      return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
    }
    case "list_children": {
      if (!SESSION_ID) throw new Error("SESSION_ID not set");
      if (!PROJECT_ID) throw new Error("PROJECT_ID not set");
      const data = await apiGet(
        `/api/projects/${PROJECT_ID}/sessions/${SESSION_ID}/children`,
      );
      return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
    }
    case "create_project": {
      const body = { name: args.name };
      if (args.display_name) body.display_name = args.display_name;
      if (args.description) body.description = args.description;
      if (args.visibility) body.visibility = args.visibility;
      const data = await apiPost("/api/projects", { body });
      return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
    }
    case "update_project": {
      const body = {};
      if (args.display_name !== undefined) body.display_name = args.display_name;
      if (args.description !== undefined) body.description = args.description;
      if (args.visibility !== undefined) body.visibility = args.visibility;
      if (args.default_branch !== undefined) body.default_branch = args.default_branch;
      const data = await apiPatch(`/api/projects/${args.project_id}`, { body });
      return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
    }
    case "delete_project": {
      const data = await apiDelete(`/api/projects/${args.project_id}`);
      return { content: [{ type: "text", text: data ? JSON.stringify(data, null, 2) : "Project deleted" }] };
    }
    case "get_session": {
      const p = args.project_id || PROJECT_ID;
      if (!p) throw new Error("PROJECT_ID not set and no project_id provided");
      const data = await apiGet(`/api/projects/${p}/sessions/${args.session_id}`);
      return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
    }
    case "send_message_to_session": {
      const p = args.project_id || PROJECT_ID;
      if (!p) throw new Error("PROJECT_ID not set and no project_id provided");
      const data = await apiPost(
        `/api/projects/${p}/sessions/${args.session_id}/message`,
        { body: { content: args.content } },
      );
      return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
    }
    default:
      throw new Error(`Unknown tool: ${name}`);
  }
});

const transport = new StdioServerTransport();
await server.connect(transport);
