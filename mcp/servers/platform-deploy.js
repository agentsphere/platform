/**
 * Platform Deploy MCP Server
 *
 * Provides deployment management tools: list, get, create, update,
 * rollback deployments, view history, and manage preview environments.
 * Loaded for: ops, admin roles.
 */

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { apiGet, apiPost, apiPatch, PROJECT_ID } from "../lib/client.js";

const server = new Server(
  { name: "platform-deploy", version: "0.1.0" },
  { capabilities: { tools: {} } },
);

const pid = () => PROJECT_ID;

const TOOLS = [
  {
    name: "list_deployments",
    description:
      "List all deployments for the current project. Shows environment, status, image, and deploy timestamp.",
    inputSchema: {
      type: "object",
      properties: {
        project_id: {
          type: "string",
          description: "Project UUID (defaults to current project)",
        },
        limit: {
          type: "integer",
          description: "Max results (default 50, max 100)",
        },
        offset: { type: "integer", description: "Pagination offset" },
      },
    },
  },
  {
    name: "get_deployment",
    description:
      "Get deployment details for a specific environment, including current image, status, replicas, and last deploy time.",
    inputSchema: {
      type: "object",
      properties: {
        environment: {
          type: "string",
          description: "Environment name (e.g. 'production', 'staging')",
        },
        project_id: {
          type: "string",
          description: "Project UUID (defaults to current project)",
        },
      },
      required: ["environment"],
    },
  },
  {
    name: "create_deployment",
    description:
      "Create a new deployment to an environment. Requires the target environment and container image reference.",
    inputSchema: {
      type: "object",
      properties: {
        environment: {
          type: "string",
          description: "Target environment (e.g. 'staging', 'production')",
        },
        image_ref: {
          type: "string",
          description:
            "Container image reference (e.g. 'registry.example.com/app:v1.2.3')",
        },
        ops_repo_id: {
          type: "string",
          description: "UUID of the ops repository for GitOps manifests",
        },
        manifest_path: {
          type: "string",
          description:
            "Path to the manifest file within the ops repo (e.g. 'k8s/production/deployment.yaml')",
        },
        values_override: {
          type: "object",
          description: "Key-value overrides for deployment values/config",
        },
        project_id: {
          type: "string",
          description: "Project UUID (defaults to current project)",
        },
      },
      required: ["environment", "image_ref"],
    },
  },
  {
    name: "update_deployment",
    description:
      "Update an existing deployment. Can change image, desired status, or value overrides.",
    inputSchema: {
      type: "object",
      properties: {
        environment: {
          type: "string",
          description: "Environment name to update",
        },
        image_ref: { type: "string", description: "New container image reference" },
        desired_status: {
          type: "string",
          description: "Desired status (e.g. 'running', 'paused')",
        },
        values_override: {
          type: "object",
          description: "Updated key-value overrides",
        },
        project_id: {
          type: "string",
          description: "Project UUID (defaults to current project)",
        },
      },
      required: ["environment"],
    },
  },
  {
    name: "rollback_deployment",
    description:
      "Rollback a deployment to the previous version. Reverts to the last known-good image and config.",
    inputSchema: {
      type: "object",
      properties: {
        environment: {
          type: "string",
          description: "Environment name to rollback",
        },
        project_id: {
          type: "string",
          description: "Project UUID (defaults to current project)",
        },
      },
      required: ["environment"],
    },
  },
  {
    name: "get_deployment_history",
    description:
      "Get deployment history for an environment. Shows past deployments with image, status, timestamp, and actor.",
    inputSchema: {
      type: "object",
      properties: {
        environment: {
          type: "string",
          description: "Environment name",
        },
        limit: {
          type: "integer",
          description: "Max results (default 50, max 100)",
        },
        offset: { type: "integer", description: "Pagination offset" },
        project_id: {
          type: "string",
          description: "Project UUID (defaults to current project)",
        },
      },
      required: ["environment"],
    },
  },
  {
    name: "list_previews",
    description:
      "List preview environments for the current project. Preview environments are ephemeral deployments for branches/MRs.",
    inputSchema: {
      type: "object",
      properties: {
        project_id: {
          type: "string",
          description: "Project UUID (defaults to current project)",
        },
        limit: {
          type: "integer",
          description: "Max results (default 50, max 100)",
        },
        offset: { type: "integer", description: "Pagination offset" },
      },
    },
  },
  {
    name: "get_preview",
    description:
      "Get details of a specific preview environment by its slug (usually derived from branch name).",
    inputSchema: {
      type: "object",
      properties: {
        slug: {
          type: "string",
          description: "Preview environment slug",
        },
        project_id: {
          type: "string",
          description: "Project UUID (defaults to current project)",
        },
      },
      required: ["slug"],
    },
  },
];

server.setRequestHandler({ method: "tools/list" }, async () => ({ tools: TOOLS }));

server.setRequestHandler({ method: "tools/call" }, async (request) => {
  const { name, arguments: args = {} } = request.params;
  const p = args.project_id || pid();

  try {
    switch (name) {
      case "list_deployments": {
        const data = await apiGet(`/api/projects/${p}/deployments`, {
          query: { limit: args.limit, offset: args.offset },
        });
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      case "get_deployment": {
        const data = await apiGet(
          `/api/projects/${p}/deployments/${encodeURIComponent(args.environment)}`,
        );
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      case "create_deployment": {
        const body = {
          environment: args.environment,
          image_ref: args.image_ref,
        };
        if (args.ops_repo_id) body.ops_repo_id = args.ops_repo_id;
        if (args.manifest_path) body.manifest_path = args.manifest_path;
        if (args.values_override) body.values_override = args.values_override;
        const data = await apiPost(`/api/projects/${p}/deployments`, { body });
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      case "update_deployment": {
        const body = {};
        if (args.image_ref !== undefined) body.image_ref = args.image_ref;
        if (args.desired_status !== undefined) body.desired_status = args.desired_status;
        if (args.values_override !== undefined) body.values_override = args.values_override;
        const data = await apiPatch(
          `/api/projects/${p}/deployments/${encodeURIComponent(args.environment)}`,
          { body },
        );
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      case "rollback_deployment": {
        const data = await apiPost(
          `/api/projects/${p}/deployments/${encodeURIComponent(args.environment)}/rollback`,
        );
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      case "get_deployment_history": {
        const data = await apiGet(
          `/api/projects/${p}/deployments/${encodeURIComponent(args.environment)}/history`,
          { query: { limit: args.limit, offset: args.offset } },
        );
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      case "list_previews": {
        const data = await apiGet(`/api/projects/${p}/previews`, {
          query: { limit: args.limit, offset: args.offset },
        });
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      case "get_preview": {
        const data = await apiGet(
          `/api/projects/${p}/previews/${encodeURIComponent(args.slug)}`,
        );
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
