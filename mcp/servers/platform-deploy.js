/**
 * Platform Deploy MCP Server
 *
 * Provides deployment management tools: targets, releases, traffic splitting,
 * rollback, promotion, and staging status.
 * Loaded for: ops, admin roles.
 */

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { ListToolsRequestSchema, CallToolRequestSchema } from "@modelcontextprotocol/sdk/types.js";
import { apiGet, apiPost, apiPatch, PROJECT_ID } from "../lib/client.js";
import { gateCheck } from '../lib/gate.js';

const server = new Server(
  { name: "platform-deploy", version: "0.2.0" },
  { capabilities: { tools: {} } },
);

const pid = () => PROJECT_ID;

const TOOLS = [
  // --- Deploy Targets ---
  {
    name: "list_targets",
    description:
      "List deploy targets for the current project. A target represents a deployment environment (e.g. production, staging) with its configuration.",
    inputSchema: {
      type: "object",
      properties: {
        project_id: { type: "string", description: "Project UUID (defaults to current project)" },
        limit: { type: "integer", description: "Max results (default 50, max 100)" },
        offset: { type: "integer", description: "Pagination offset" },
      },
    },
  },
  {
    name: "get_target",
    description: "Get details of a specific deploy target by ID.",
    inputSchema: {
      type: "object",
      properties: {
        target_id: { type: "string", description: "Deploy target UUID" },
        project_id: { type: "string", description: "Project UUID (defaults to current project)" },
      },
      required: ["target_id"],
    },
  },
  {
    name: "create_target",
    description:
      "Create a new deploy target. Defines an environment with a deployment strategy (rolling, canary, ab_test).",
    inputSchema: {
      type: "object",
      properties: {
        name: { type: "string", description: "Target name (e.g. 'production')" },
        environment: { type: "string", description: "Environment label: preview, staging, or production (optional, defaults to production)" },
        default_strategy: {
          type: "string",
          description: "Default rollout strategy: rolling, canary, or ab_test (default: rolling)",
        },
        ops_repo_id: { type: "string", description: "Ops repo UUID for manifest rendering (optional)" },
        manifest_path: { type: "string", description: "Path within ops repo to Kustomize overlay (optional)" },
        project_id: { type: "string", description: "Project UUID (defaults to current project)" },
      },
      required: ["name"],
    },
  },
  // --- Releases ---
  {
    name: "list_releases",
    description:
      "List deploy releases for a project. Each release represents a deployment of a specific image to a target, with traffic splitting and health tracking.",
    inputSchema: {
      type: "object",
      properties: {
        project_id: { type: "string", description: "Project UUID (defaults to current project)" },
        limit: { type: "integer", description: "Max results (default 50, max 100)" },
        offset: { type: "integer", description: "Pagination offset" },
      },
    },
  },
  {
    name: "get_release",
    description:
      "Get details of a specific release: image, strategy, phase, traffic weight, health, and rollout config.",
    inputSchema: {
      type: "object",
      properties: {
        release_id: { type: "string", description: "Release UUID" },
        project_id: { type: "string", description: "Project UUID (defaults to current project)" },
      },
      required: ["release_id"],
    },
  },
  {
    name: "create_release",
    description:
      "Create a new release for the project's production deploy target. Starts the rollout of a new image with the specified strategy.",
    inputSchema: {
      type: "object",
      properties: {
        image_ref: { type: "string", description: "Container image reference (e.g. 'registry/app:v1.2.3')" },
        strategy: {
          type: "string",
          description: "Rollout strategy: rolling, canary, or ab_test (defaults to target's default)",
        },
        commit_sha: { type: "string", description: "Git commit SHA for traceability (optional)" },
        values_override: { type: "object", description: "Key-value overrides for rendered manifests (optional)" },
        pipeline_id: { type: "string", description: "Pipeline run UUID that triggered this release (optional)" },
        project_id: { type: "string", description: "Project UUID (defaults to current project)" },
      },
      required: ["image_ref"],
    },
  },
  // --- Release Actions ---
  {
    name: "adjust_traffic",
    description:
      "Adjust traffic weight for a canary/blue-green release. Sets the percentage of traffic routed to the new version.",
    inputSchema: {
      type: "object",
      properties: {
        release_id: { type: "string", description: "Release UUID" },
        weight: { type: "integer", description: "Traffic weight percentage (0-100)" },
        project_id: { type: "string", description: "Project UUID (defaults to current project)" },
      },
      required: ["release_id", "weight"],
    },
  },
  {
    name: "promote_release",
    description:
      "Promote a release to full traffic (100%). Completes a canary or blue-green rollout.",
    inputSchema: {
      type: "object",
      properties: {
        release_id: { type: "string", description: "Release UUID" },
        project_id: { type: "string", description: "Project UUID (defaults to current project)" },
      },
      required: ["release_id"],
    },
  },
  {
    name: "rollback_release",
    description:
      "Rollback a release. Reverts traffic to the previous stable version and marks this release as rolled back.",
    inputSchema: {
      type: "object",
      properties: {
        release_id: { type: "string", description: "Release UUID" },
        project_id: { type: "string", description: "Project UUID (defaults to current project)" },
      },
      required: ["release_id"],
    },
  },
  {
    name: "release_history",
    description:
      "Get the history of actions for a release: traffic adjustments, promotions, rollbacks, phase transitions.",
    inputSchema: {
      type: "object",
      properties: {
        release_id: { type: "string", description: "Release UUID" },
        project_id: { type: "string", description: "Project UUID (defaults to current project)" },
        limit: { type: "integer", description: "Max results (default 50, max 100)" },
        offset: { type: "integer", description: "Pagination offset" },
      },
      required: ["release_id"],
    },
  },
  // --- Staging ---
  {
    name: "staging_status",
    description:
      "Get the current staging deployment status for the project, including pending promotions.",
    inputSchema: {
      type: "object",
      properties: {
        project_id: { type: "string", description: "Project UUID (defaults to current project)" },
      },
    },
  },
];

server.setRequestHandler(ListToolsRequestSchema, async () => ({ tools: TOOLS }));

server.setRequestHandler(CallToolRequestSchema, async (request) => {
  const { name, arguments: args = {} } = request.params;
  const p = args.project_id || pid();

  // Manager gate — checks permission mode, waits for user approval if needed
  const gateResult = await gateCheck(process.env.SESSION_ID || '', name, args);
  if (gateResult) return gateResult;

  try {
    switch (name) {
      // --- Targets ---
      case "list_targets": {
        const data = await apiGet(`/api/projects/${p}/targets`, {
          query: { limit: args.limit, offset: args.offset },
        });
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      case "get_target": {
        const data = await apiGet(`/api/projects/${p}/targets/${args.target_id}`);
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      case "create_target": {
        const body = { name: args.name };
        if (args.environment !== undefined) body.environment = args.environment;
        if (args.default_strategy !== undefined) body.default_strategy = args.default_strategy;
        if (args.ops_repo_id !== undefined) body.ops_repo_id = args.ops_repo_id;
        if (args.manifest_path !== undefined) body.manifest_path = args.manifest_path;
        const data = await apiPost(`/api/projects/${p}/targets`, { body });
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      // --- Releases ---
      case "list_releases": {
        const data = await apiGet(`/api/projects/${p}/deploy-releases`, {
          query: { limit: args.limit, offset: args.offset },
        });
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      case "get_release": {
        const data = await apiGet(`/api/projects/${p}/deploy-releases/${args.release_id}`);
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      case "create_release": {
        const body = { image_ref: args.image_ref };
        if (args.strategy !== undefined) body.strategy = args.strategy;
        if (args.commit_sha !== undefined) body.commit_sha = args.commit_sha;
        if (args.values_override !== undefined) body.values_override = args.values_override;
        if (args.pipeline_id !== undefined) body.pipeline_id = args.pipeline_id;
        const data = await apiPost(`/api/projects/${p}/deploy-releases`, { body });
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      // --- Release Actions ---
      case "adjust_traffic": {
        const data = await apiPatch(`/api/projects/${p}/deploy-releases/${args.release_id}/traffic`, {
          body: { traffic_weight: args.weight },
        });
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      case "promote_release": {
        const data = await apiPost(`/api/projects/${p}/deploy-releases/${args.release_id}/promote`);
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      case "rollback_release": {
        const data = await apiPost(`/api/projects/${p}/deploy-releases/${args.release_id}/rollback`);
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      case "release_history": {
        const data = await apiGet(`/api/projects/${p}/deploy-releases/${args.release_id}/history`, {
          query: { limit: args.limit, offset: args.offset },
        });
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      // --- Staging ---
      case "staging_status": {
        const data = await apiGet(`/api/projects/${p}/staging-status`);
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
