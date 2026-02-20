/**
 * Platform Core MCP Server
 *
 * Provides project info and general platform queries.
 * Always loaded for every agent role.
 */

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { apiGet, PROJECT_ID } from "../lib/client.js";

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
];

server.setRequestHandler({ method: "tools/list" }, async () => ({ tools: TOOLS }));

server.setRequestHandler({ method: "tools/call" }, async (request) => {
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
    default:
      throw new Error(`Unknown tool: ${name}`);
  }
});

const transport = new StdioServerTransport();
await server.connect(transport);
