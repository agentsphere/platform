/**
 * Platform Issues & Merge Requests MCP Server
 *
 * Provides issue tracking and merge request management tools.
 * Loaded for: dev, admin, ui roles.
 */

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { apiGet, apiPost, apiPatch, PROJECT_ID } from "../lib/client.js";

const server = new Server(
  { name: "platform-issues", version: "0.1.0" },
  { capabilities: { tools: {} } },
);

const pid = () => PROJECT_ID;

const TOOLS = [
  // --- Issues ---
  {
    name: "list_issues",
    description: "List issues for the current project. Filter by status (open/closed), labels, or assignee.",
    inputSchema: {
      type: "object",
      properties: {
        status: { type: "string", description: "Filter by status (open/closed)" },
        limit: { type: "integer", description: "Max results (default 50)" },
        offset: { type: "integer", description: "Pagination offset" },
        project_id: { type: "string", description: "Project UUID (defaults to current)" },
      },
    },
  },
  {
    name: "get_issue",
    description: "Get issue details by its project-scoped number (not UUID).",
    inputSchema: {
      type: "object",
      properties: {
        number: { type: "integer", description: "Issue number within the project" },
        project_id: { type: "string", description: "Project UUID (defaults to current)" },
      },
      required: ["number"],
    },
  },
  {
    name: "create_issue",
    description: "Create a new issue in the project.",
    inputSchema: {
      type: "object",
      properties: {
        title: { type: "string", description: "Issue title (1-500 chars)" },
        body: { type: "string", description: "Issue body/description (markdown)" },
        labels: {
          type: "array",
          items: { type: "string" },
          description: "Labels to apply (max 50)",
        },
        project_id: { type: "string", description: "Project UUID (defaults to current)" },
      },
      required: ["title"],
    },
  },
  {
    name: "update_issue",
    description: "Update an issue (title, body, status, labels).",
    inputSchema: {
      type: "object",
      properties: {
        number: { type: "integer", description: "Issue number" },
        title: { type: "string", description: "New title" },
        body: { type: "string", description: "New body" },
        status: { type: "string", description: "New status (open/closed)" },
        labels: {
          type: "array",
          items: { type: "string" },
          description: "New labels (replaces existing)",
        },
        project_id: { type: "string", description: "Project UUID (defaults to current)" },
      },
      required: ["number"],
    },
  },
  {
    name: "add_issue_comment",
    description: "Add a comment to an issue.",
    inputSchema: {
      type: "object",
      properties: {
        number: { type: "integer", description: "Issue number" },
        body: { type: "string", description: "Comment body (markdown)" },
        project_id: { type: "string", description: "Project UUID (defaults to current)" },
      },
      required: ["number", "body"],
    },
  },
  {
    name: "list_issue_comments",
    description: "List comments on an issue.",
    inputSchema: {
      type: "object",
      properties: {
        number: { type: "integer", description: "Issue number" },
        limit: { type: "integer", description: "Max results" },
        offset: { type: "integer", description: "Pagination offset" },
        project_id: { type: "string", description: "Project UUID (defaults to current)" },
      },
      required: ["number"],
    },
  },
  // --- Merge Requests ---
  {
    name: "list_merge_requests",
    description: "List merge requests for the current project.",
    inputSchema: {
      type: "object",
      properties: {
        status: { type: "string", description: "Filter by status (open/closed/merged)" },
        limit: { type: "integer", description: "Max results" },
        offset: { type: "integer", description: "Pagination offset" },
        project_id: { type: "string", description: "Project UUID (defaults to current)" },
      },
    },
  },
  {
    name: "get_merge_request",
    description: "Get merge request details by its project-scoped number.",
    inputSchema: {
      type: "object",
      properties: {
        number: { type: "integer", description: "MR number within the project" },
        project_id: { type: "string", description: "Project UUID (defaults to current)" },
      },
      required: ["number"],
    },
  },
  {
    name: "create_merge_request",
    description: "Create a new merge request.",
    inputSchema: {
      type: "object",
      properties: {
        title: { type: "string", description: "MR title (1-500 chars)" },
        body: { type: "string", description: "MR description (markdown)" },
        source_branch: { type: "string", description: "Source branch name" },
        target_branch: { type: "string", description: "Target branch (defaults to project default)" },
        labels: {
          type: "array",
          items: { type: "string" },
          description: "Labels to apply",
        },
        project_id: { type: "string", description: "Project UUID (defaults to current)" },
      },
      required: ["title", "source_branch"],
    },
  },
  {
    name: "update_merge_request",
    description: "Update a merge request (title, body, labels).",
    inputSchema: {
      type: "object",
      properties: {
        number: { type: "integer", description: "MR number" },
        title: { type: "string", description: "New title" },
        body: { type: "string", description: "New body" },
        labels: {
          type: "array",
          items: { type: "string" },
          description: "New labels",
        },
        project_id: { type: "string", description: "Project UUID (defaults to current)" },
      },
      required: ["number"],
    },
  },
  {
    name: "add_mr_comment",
    description: "Add a comment to a merge request.",
    inputSchema: {
      type: "object",
      properties: {
        number: { type: "integer", description: "MR number" },
        body: { type: "string", description: "Comment body (markdown)" },
        project_id: { type: "string", description: "Project UUID (defaults to current)" },
      },
      required: ["number", "body"],
    },
  },
];

server.setRequestHandler({ method: "tools/list" }, async () => ({ tools: TOOLS }));

server.setRequestHandler({ method: "tools/call" }, async (request) => {
  const { name, arguments: args = {} } = request.params;
  const p = args.project_id || pid();

  switch (name) {
    // --- Issues ---
    case "list_issues": {
      const data = await apiGet(`/api/projects/${p}/issues`, {
        query: { status: args.status, limit: args.limit, offset: args.offset },
      });
      return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
    }
    case "get_issue": {
      const data = await apiGet(`/api/projects/${p}/issues/${args.number}`);
      return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
    }
    case "create_issue": {
      const data = await apiPost(`/api/projects/${p}/issues`, {
        body: { title: args.title, body: args.body, labels: args.labels },
      });
      return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
    }
    case "update_issue": {
      const body = {};
      if (args.title !== undefined) body.title = args.title;
      if (args.body !== undefined) body.body = args.body;
      if (args.status !== undefined) body.status = args.status;
      if (args.labels !== undefined) body.labels = args.labels;
      const data = await apiPatch(`/api/projects/${p}/issues/${args.number}`, { body });
      return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
    }
    case "add_issue_comment": {
      const data = await apiPost(`/api/projects/${p}/issues/${args.number}/comments`, {
        body: { body: args.body },
      });
      return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
    }
    case "list_issue_comments": {
      const data = await apiGet(`/api/projects/${p}/issues/${args.number}/comments`, {
        query: { limit: args.limit, offset: args.offset },
      });
      return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
    }
    // --- Merge Requests ---
    case "list_merge_requests": {
      const data = await apiGet(`/api/projects/${p}/merge-requests`, {
        query: { status: args.status, limit: args.limit, offset: args.offset },
      });
      return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
    }
    case "get_merge_request": {
      const data = await apiGet(`/api/projects/${p}/merge-requests/${args.number}`);
      return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
    }
    case "create_merge_request": {
      const data = await apiPost(`/api/projects/${p}/merge-requests`, {
        body: {
          title: args.title,
          body: args.body,
          source_branch: args.source_branch,
          target_branch: args.target_branch,
          labels: args.labels,
        },
      });
      return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
    }
    case "update_merge_request": {
      const body = {};
      if (args.title !== undefined) body.title = args.title;
      if (args.body !== undefined) body.body = args.body;
      if (args.labels !== undefined) body.labels = args.labels;
      const data = await apiPatch(`/api/projects/${p}/merge-requests/${args.number}`, { body });
      return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
    }
    case "add_mr_comment": {
      const data = await apiPost(`/api/projects/${p}/merge-requests/${args.number}/comments`, {
        body: { body: args.body },
      });
      return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
    }
    default:
      throw new Error(`Unknown tool: ${name}`);
  }
});

const transport = new StdioServerTransport();
await server.connect(transport);
