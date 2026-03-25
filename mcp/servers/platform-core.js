/**
 * Platform Core MCP Server
 *
 * Provides project info and general platform queries.
 * Always loaded for every agent role.
 */

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { ListToolsRequestSchema, CallToolRequestSchema } from "@modelcontextprotocol/sdk/types.js";
import { appendFileSync } from "node:fs";
import { apiGet, apiPost, apiPatch, apiDelete, PROJECT_ID } from "../lib/client.js";

const SESSION_ID = process.env.SESSION_ID || "";
const ENV_DEV_PATH = "/workspace/.env.dev";

/** Append a secret to .env.dev so the agent can source it for app dev. */
function appendToEnvDev(name, value) {
  try {
    const escaped = value.replace(/'/g, "'\\''");
    appendFileSync(ENV_DEV_PATH, `${name}='${escaped}'\n`);
  } catch {
    // Non-fatal — workspace may not be writable
  }
}

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
  {
    name: "ask_for_secret",
    description:
      "Request a secret from the user (e.g. API key, password). " +
      "Creates a pending secret request that appears in the UI. " +
      "The user enters the value in a modal, then this tool returns the secret value. " +
      "Polls until the user responds or the request times out (5 min).",
    inputSchema: {
      type: "object",
      properties: {
        name: {
          type: "string",
          description: "Secret name (e.g. GITHUB_TOKEN, API_KEY)",
        },
        prompt: {
          type: "string",
          description: "Human-readable prompt explaining what the secret is for",
        },
        environments: {
          type: "array",
          items: { type: "string" },
          description: "Environments to store the secret for (e.g. ['production', 'staging']). Optional.",
        },
        project_id: {
          type: "string",
          description: "Project UUID (defaults to current project)",
        },
      },
      required: ["name", "prompt"],
    },
  },
  {
    name: "get_worker_progress",
    description:
      "Get the latest progress/task list from a worker agent session. " +
      "Returns the structured progress markdown that the worker maintains at .platform/progress.md.",
    inputSchema: {
      type: "object",
      properties: {
        session_id: {
          type: "string",
          description: "Worker session UUID",
        },
        project_id: {
          type: "string",
          description: "Project UUID (defaults to current project)",
        },
      },
      required: ["session_id"],
    },
  },
  {
    name: "read_secret",
    description:
      "Read a stored secret value by name. Returns the decrypted value. " +
      "Use this to retrieve secrets that were previously stored (e.g. via ask_for_secret or the UI).",
    inputSchema: {
      type: "object",
      properties: {
        name: {
          type: "string",
          description: "Secret name (e.g. STRIPE_API_KEY)",
        },
        scope: {
          type: "string",
          description: "Secret scope filter (default: 'agent'). One of: agent, pipeline, deploy, all.",
        },
        project_id: {
          type: "string",
          description: "Project UUID (defaults to current project)",
        },
      },
      required: ["name"],
    },
  },
];

server.setRequestHandler(ListToolsRequestSchema, async () => ({ tools: TOOLS }));

server.setRequestHandler(CallToolRequestSchema, async (request) => {
  const { name, arguments: args = {} } = request.params;

  try {
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
    case "ask_for_secret": {
      if (!SESSION_ID) throw new Error("SESSION_ID not set — cannot request secrets");
      const p = args.project_id || PROJECT_ID;
      if (!p) throw new Error("PROJECT_ID not set and no project_id provided");

      // Create the secret request
      const payload = {
        name: args.name,
        description: args.prompt || "",
        environments: args.environments || [],
        session_id: SESSION_ID,
      };
      const created = await apiPost(`/api/projects/${p}/secret-requests`, { body: payload });
      const requestId = created.id;

      // Poll until completed or timed out (max 5 min, 2s interval)
      const maxAttempts = 150; // 5 min / 2s
      for (let attempt = 0; attempt < maxAttempts; attempt++) {
        await new Promise((r) => setTimeout(r, 2000));
        const status = await apiGet(`/api/projects/${p}/secret-requests/${requestId}`);
        if (status.status === "completed") {
          // Read the decrypted secret value
          try {
            const secret = await apiGet(`/api/projects/${p}/secrets/${encodeURIComponent(args.name)}`, {
              query: { scope: "agent" },
            });
            appendToEnvDev(secret.name, secret.value);
            return {
              content: [{
                type: "text",
                text: JSON.stringify({
                  status: "completed",
                  name: secret.name,
                  value: secret.value,
                }, null, 2),
              }],
            };
          } catch (readErr) {
            // Fallback: secret was stored but we couldn't read it back
            return {
              content: [{
                type: "text",
                text: JSON.stringify({
                  status: "completed",
                  name: status.name,
                  note: "Secret stored but could not be read back: " + readErr.message,
                }, null, 2),
              }],
            };
          }
        }
        if (status.status === "timed_out") {
          return {
            content: [{ type: "text", text: `Secret request timed out. The user did not provide the secret "${args.name}" within 5 minutes.` }],
            isError: true,
          };
        }
      }

      return {
        content: [{ type: "text", text: `Secret request polling timed out for "${args.name}".` }],
        isError: true,
      };
    }
    case "get_worker_progress": {
      const p = args.project_id || PROJECT_ID;
      if (!p) throw new Error("PROJECT_ID not set and no project_id provided");
      try {
        const data = await apiGet(`/api/projects/${p}/sessions/${args.session_id}/progress`);
        return { content: [{ type: "text", text: data.content || "No progress updates yet." }] };
      } catch (err) {
        if (err.message && err.message.includes("404")) {
          return { content: [{ type: "text", text: "No progress updates yet for this session." }] };
        }
        throw err;
      }
    }
    case "read_secret": {
      const p = args.project_id || PROJECT_ID;
      if (!p) throw new Error("PROJECT_ID not set and no project_id provided");
      const scope = args.scope || "agent";
      const secret = await apiGet(`/api/projects/${p}/secrets/${encodeURIComponent(args.name)}`, {
        query: { scope },
      });
      return {
        content: [{ type: "text", text: JSON.stringify(secret, null, 2) }],
      };
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
