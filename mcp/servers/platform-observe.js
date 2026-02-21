/**
 * Platform Observe MCP Server
 *
 * Provides observability tools: log search, trace inspection, metric queries,
 * and alert management.
 * Loaded for: ops, admin roles.
 */

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { apiGet, PROJECT_ID } from "../lib/client.js";

const server = new Server(
  { name: "platform-observe", version: "0.1.0" },
  { capabilities: { tools: {} } },
);

const pid = () => PROJECT_ID;

const TOOLS = [
  {
    name: "search_logs",
    description:
      "Search application logs. Filter by project, session, severity level, service name, or full-text query. Returns structured log entries with timestamps, trace IDs, and attributes.",
    inputSchema: {
      type: "object",
      properties: {
        project_id: { type: "string", description: "Filter by project UUID" },
        session_id: {
          type: "string",
          description: "Filter by agent session UUID",
        },
        trace_id: { type: "string", description: "Filter by trace ID" },
        level: {
          type: "string",
          enum: ["trace", "debug", "info", "warn", "error", "fatal"],
          description: "Minimum log level",
        },
        service: {
          type: "string",
          description: "Filter by service name",
        },
        q: { type: "string", description: "Full-text search query" },
        from: { type: "string", description: "Start time (ISO 8601)" },
        to: { type: "string", description: "End time (ISO 8601)" },
        limit: {
          type: "integer",
          description: "Max results (default 50, max 100)",
        },
        offset: { type: "integer", description: "Pagination offset" },
      },
    },
  },
  {
    name: "get_trace",
    description:
      "Get a distributed trace by ID, including all spans with timing, attributes, and events. Shows the full request flow across services.",
    inputSchema: {
      type: "object",
      properties: {
        trace_id: {
          type: "string",
          description: "Trace ID (32-char hex)",
        },
      },
      required: ["trace_id"],
    },
  },
  {
    name: "list_traces",
    description:
      "List distributed traces. Filter by project, session, service, status, or time range.",
    inputSchema: {
      type: "object",
      properties: {
        project_id: { type: "string", description: "Filter by project UUID" },
        session_id: {
          type: "string",
          description: "Filter by agent session UUID",
        },
        service: {
          type: "string",
          description: "Filter by service name",
        },
        status: {
          type: "string",
          description: "Filter by trace status (ok/error)",
        },
        from: { type: "string", description: "Start time (ISO 8601)" },
        to: { type: "string", description: "End time (ISO 8601)" },
        limit: {
          type: "integer",
          description: "Max results (default 50, max 100)",
        },
        offset: { type: "integer", description: "Pagination offset" },
      },
    },
  },
  {
    name: "query_metrics",
    description:
      "Query time-series metrics. Filter by metric name, labels, project, and time range. Returns data points with timestamps and values.",
    inputSchema: {
      type: "object",
      properties: {
        name: { type: "string", description: "Metric name to query" },
        labels: {
          type: "string",
          description:
            "Label filters as comma-separated key=value pairs (e.g. 'env=production,service=api')",
        },
        project_id: { type: "string", description: "Filter by project UUID" },
        from: { type: "string", description: "Start time (ISO 8601)" },
        to: { type: "string", description: "End time (ISO 8601)" },
      },
    },
  },
  {
    name: "list_metric_names",
    description:
      "List available metric names. Optionally filter by project to see only metrics emitted by that project's services.",
    inputSchema: {
      type: "object",
      properties: {
        project_id: { type: "string", description: "Filter by project UUID" },
      },
    },
  },
  {
    name: "list_alerts",
    description:
      "List alerts. Filter by project, status (firing/resolved/pending), with pagination.",
    inputSchema: {
      type: "object",
      properties: {
        project_id: { type: "string", description: "Filter by project UUID" },
        status: {
          type: "string",
          description: "Filter by alert status (firing/resolved/pending)",
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
    name: "get_alert",
    description:
      "Get alert details by ID, including condition, current value, history, and notification config.",
    inputSchema: {
      type: "object",
      properties: {
        alert_id: { type: "string", description: "Alert UUID" },
      },
      required: ["alert_id"],
    },
  },
];

server.setRequestHandler({ method: "tools/list" }, async () => ({ tools: TOOLS }));

server.setRequestHandler({ method: "tools/call" }, async (request) => {
  const { name, arguments: args = {} } = request.params;
  const p = args.project_id || pid();

  try {
    switch (name) {
      case "search_logs": {
        const data = await apiGet("/api/observe/logs", {
          query: {
            project_id: p,
            session_id: args.session_id,
            trace_id: args.trace_id,
            level: args.level,
            service: args.service,
            q: args.q,
            from: args.from,
            to: args.to,
            limit: args.limit,
            offset: args.offset,
          },
        });
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      case "get_trace": {
        const data = await apiGet(
          `/api/observe/traces/${encodeURIComponent(args.trace_id)}`,
        );
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      case "list_traces": {
        const data = await apiGet("/api/observe/traces", {
          query: {
            project_id: p,
            session_id: args.session_id,
            service: args.service,
            status: args.status,
            from: args.from,
            to: args.to,
            limit: args.limit,
            offset: args.offset,
          },
        });
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      case "query_metrics": {
        const data = await apiGet("/api/observe/metrics", {
          query: {
            name: args.name,
            labels: args.labels,
            project_id: p,
            from: args.from,
            to: args.to,
          },
        });
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      case "list_metric_names": {
        const data = await apiGet("/api/observe/metrics/names", {
          query: { project_id: p },
        });
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      case "list_alerts": {
        const data = await apiGet("/api/observe/alerts", {
          query: {
            project_id: p,
            status: args.status,
            limit: args.limit,
            offset: args.offset,
          },
        });
        return { content: [{ type: "text", text: JSON.stringify(data, null, 2) }] };
      }
      case "get_alert": {
        const data = await apiGet(
          `/api/observe/alerts/${encodeURIComponent(args.alert_id)}`,
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
