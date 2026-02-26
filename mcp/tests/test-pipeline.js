/**
 * Tests for platform-pipeline MCP server
 */

import { describe, it, before, after } from "node:test";
import assert from "node:assert/strict";
import { setup, teardown } from "./helpers.js";

describe("platform-pipeline", () => {
  let api, client, projectId;

  before(async () => {
    ({ api, client, projectId } = await setup("platform-pipeline.js"));
  });

  after(async () => {
    await teardown(api, client);
  });

  it("list_tools returns expected pipeline tools", async () => {
    const tools = await client.listTools();
    const names = tools.map((t) => t.name);
    assert.ok(names.includes("list_pipelines"), "missing list_pipelines");
    assert.ok(names.includes("get_pipeline"), "missing get_pipeline");
    assert.ok(names.includes("get_step_logs"), "missing get_step_logs");
    assert.ok(names.includes("trigger_pipeline"), "missing trigger_pipeline");
    assert.ok(names.includes("cancel_pipeline"), "missing cancel_pipeline");
    assert.ok(names.includes("list_artifacts"), "missing list_artifacts");
    assert.ok(names.includes("download_artifact"), "missing download_artifact");
  });

  it("list_pipelines sends GET /api/projects/:id/pipelines", async () => {
    api.setResponse(200, { items: [], total: 0 });
    await client.callTool("list_pipelines", {});
    const req = api.lastRequest();
    assert.equal(req.method, "GET");
    assert.ok(req.path.includes(`/api/projects/${projectId}/pipelines`));
  });

  it("trigger_pipeline sends POST /api/projects/:id/pipelines", async () => {
    api.setResponse(201, { id: "pipe-id", status: "pending" });
    await client.callTool("trigger_pipeline", { branch: "main" });
    const req = api.lastRequest();
    assert.equal(req.method, "POST");
    assert.ok(req.path.includes("/pipelines"));
  });

  it("cancel_pipeline sends POST .../pipelines/:id/cancel", async () => {
    const pipeId = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    api.setResponse(200, {});
    await client.callTool("cancel_pipeline", { pipeline_id: pipeId });
    const req = api.lastRequest();
    assert.equal(req.method, "POST");
    assert.ok(req.path.includes(`/pipelines/${pipeId}/cancel`));
  });

  it("handles error responses", async () => {
    api.setResponse(404, { message: "not found" });
    await assert.rejects(
      () => client.callTool("get_pipeline", { pipeline_id: "bogus" }),
      /404/,
    );
  });
});
