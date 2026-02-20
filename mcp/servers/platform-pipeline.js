/**
 * Platform Pipeline MCP Server
 *
 * Provides CI/CD pipeline management tools.
 * Loaded for: dev, ops, admin roles.
 */

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { apiGet, apiPost, PROJECT_ID } from "../lib/client.js";

const server = new Server(
  { name: "platform-pipeline", version: "0.1.0" },
  { capabilities: { tools: {} } },
);

const pid = () => PROJECT_ID;

const TOOLS = [
  {
    name: "list_pipelines",
    description:
      "List pipelines for the current project. Filter by status (pending/running/success/failure/cancelled), trigger type (push/api/mr), or git ref.",
    inputSchema: {
      type: "object",
      properties: {
        status: { type: "string", description: "Filter by status" },
        trigger: { type: "string", description: "Filter by trigger type" },
        git_ref: { type: "string", description: "Filter by git ref" },
        limit: { type: "integer", description: "Max results (default 50)" },
        offset: { type: "integer", description: "Pagination offset" },
        project_id: { type: "string", description: "Project UUID (defaults to current)" },
      },
    },
  },
  {
    name: "get_pipeline",
    description:
      "Get pipeline details including all steps with their status, exit codes, and durations.",
    inputSchema: {
      type: "object",
      properties: {
        pipeline_id: { type: "string", description: "Pipeline UUID" },
        project_id: { type: "string", description: "Project UUID (defaults to current)" },
      },
      required: ["pipeline_id"],
    },
  },
  {
    name: "get_step_logs",
    description:
      "Get logs for a specific pipeline step. Returns a presigned URL for completed steps or streaming logs for running ones.",
    inputSchema: {
      type: "object",
      properties: {
        pipeline_id: { type: "string", description: "Pipeline UUID" },
        step_id: { type: "string", description: "Step UUID" },
        project_id: { type: "string", description: "Project UUID (defaults to current)" },
      },
      required: ["pipeline_id", "step_id"],
    },
  },
  {
    name: "trigger_pipeline",
    description: "Manually trigger a pipeline on the specified git ref (branch or tag).",
    inputSchema: {
      type: "object",
      properties: {
        git_ref: { type: "string", description: "Git ref to build (e.g. 'main', 'feature/x')" },
        project_id: { type: "string", description: "Project UUID (defaults to current)" },
      },
      required: ["git_ref"],
    },
  },
  {
    name: "cancel_pipeline",
    description: "Cancel a running pipeline. Kills running pods and marks as cancelled.",
    inputSchema: {
      type: "object",
      properties: {
        pipeline_id: { type: "string", description: "Pipeline UUID to cancel" },
        project_id: { type: "string", description: "Project UUID (defaults to current)" },
      },
      required: ["pipeline_id"],
    },
  },
  {
    name: "list_artifacts",
    description: "List build artifacts for a completed pipeline.",
    inputSchema: {
      type: "object",
      properties: {
        pipeline_id: { type: "string", description: "Pipeline UUID" },
        project_id: { type: "string", description: "Project UUID (defaults to current)" },
      },
      required: ["pipeline_id"],
    },
  },
  {
    name: "download_artifact",
    description: "Get a presigned download URL for a build artifact.",
    inputSchema: {
      type: "object",
      properties: {
        pipeline_id: { type: "string", description: "Pipeline UUID" },
        artifact_id: { type: "string", description: "Artifact UUID" },
        project_id: { type: "string", description: "Project UUID (defaults to current)" },
      },
      required: ["pipeline_id", "artifact_id"],
    },
  },
];

server.setRequestHandler({ method: "tools/list" }, async () => ({ tools: TOOLS }));

server.setRequestHandler({ method: "tools/call" }, async (request) => {
  const { name, arguments: args = {} } = request.params;
  const p = args.project_id || pid();

  switch (name) {
    case "list_pipelines": {
      const data = await apiGet(`/api/projects/${p}/pipelines`, {
        query: {
          status: args.status,
          trigger: args.trigger,
          git_ref: args.git_ref,
          limit: args.limit,
          offset: args.offset,
        },
      });
      return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
    }
    case "get_pipeline": {
      const data = await apiGet(`/api/projects/${p}/pipelines/${args.pipeline_id}`);
      return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
    }
    case "get_step_logs": {
      const data = await apiGet(
        `/api/projects/${p}/pipelines/${args.pipeline_id}/steps/${args.step_id}/logs`,
      );
      return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
    }
    case "trigger_pipeline": {
      const data = await apiPost(`/api/projects/${p}/pipelines`, {
        body: { git_ref: args.git_ref },
      });
      return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
    }
    case "cancel_pipeline": {
      const data = await apiPost(`/api/projects/${p}/pipelines/${args.pipeline_id}/cancel`);
      return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
    }
    case "list_artifacts": {
      const data = await apiGet(`/api/projects/${p}/pipelines/${args.pipeline_id}/artifacts`);
      return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
    }
    case "download_artifact": {
      const data = await apiGet(
        `/api/projects/${p}/pipelines/${args.pipeline_id}/artifacts/${args.artifact_id}/download`,
      );
      return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
    }
    default:
      throw new Error(`Unknown tool: ${name}`);
  }
});

const transport = new StdioServerTransport();
await server.connect(transport);
