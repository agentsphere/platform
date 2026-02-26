/**
 * Tests for platform-observe MCP server
 */

import { describe, it, before, after } from "node:test";
import assert from "node:assert/strict";
import { setup, teardown } from "./helpers.js";

describe("platform-observe", () => {
  let api, client;

  before(async () => {
    ({ api, client } = await setup("platform-observe.js"));
  });

  after(async () => {
    await teardown(api, client);
  });

  it("list_tools returns expected observe tools", async () => {
    const tools = await client.listTools();
    const names = tools.map((t) => t.name);
    assert.ok(names.includes("search_logs"), "missing search_logs");
    assert.ok(names.includes("get_trace"), "missing get_trace");
    assert.ok(names.includes("list_traces"), "missing list_traces");
    assert.ok(names.includes("query_metrics"), "missing query_metrics");
    assert.ok(names.includes("list_metric_names"), "missing list_metric_names");
    assert.ok(names.includes("list_alerts"), "missing list_alerts");
    assert.ok(names.includes("get_alert"), "missing get_alert");
  });

  it("search_logs sends GET /api/observe/logs", async () => {
    api.setResponse(200, { items: [], total: 0 });
    await client.callTool("search_logs", { query: "error" });
    const req = api.lastRequest();
    assert.equal(req.method, "GET");
    assert.ok(req.path.startsWith("/api/observe/logs"));
  });

  it("list_traces sends GET /api/observe/traces", async () => {
    api.setResponse(200, { items: [], total: 0 });
    await client.callTool("list_traces", {});
    const req = api.lastRequest();
    assert.equal(req.method, "GET");
    assert.ok(req.path.startsWith("/api/observe/traces"));
  });

  it("query_metrics sends GET /api/observe/metrics", async () => {
    api.setResponse(200, { items: [] });
    await client.callTool("query_metrics", { name: "cpu_usage" });
    const req = api.lastRequest();
    assert.equal(req.method, "GET");
    assert.ok(req.path.startsWith("/api/observe/metrics"));
  });

  it("handles error responses", async () => {
    api.setResponse(500, { message: "internal error" });
    await assert.rejects(
      () => client.callTool("search_logs", {}),
      /500|internal error/,
    );
  });
});
