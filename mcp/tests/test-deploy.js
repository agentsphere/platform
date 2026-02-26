/**
 * Tests for platform-deploy MCP server
 */

import { describe, it, before, after } from "node:test";
import assert from "node:assert/strict";
import { setup, teardown } from "./helpers.js";

describe("platform-deploy", () => {
  let api, client, projectId;

  before(async () => {
    ({ api, client, projectId } = await setup("platform-deploy.js"));
  });

  after(async () => {
    await teardown(api, client);
  });

  it("list_tools returns expected deploy tools", async () => {
    const tools = await client.listTools();
    const names = tools.map((t) => t.name);
    assert.ok(names.includes("list_deployments"), "missing list_deployments");
    assert.ok(names.includes("get_deployment"), "missing get_deployment");
    assert.ok(names.includes("create_deployment"), "missing create_deployment");
    assert.ok(names.includes("update_deployment"), "missing update_deployment");
    assert.ok(names.includes("rollback_deployment"), "missing rollback_deployment");
    assert.ok(names.includes("get_deployment_history"), "missing get_deployment_history");
    assert.ok(names.includes("list_previews"), "missing list_previews");
    assert.ok(names.includes("get_preview"), "missing get_preview");
  });

  it("list_deployments sends GET /api/projects/:id/deployments", async () => {
    api.setResponse(200, { items: [], total: 0 });
    await client.callTool("list_deployments", {});
    const req = api.lastRequest();
    assert.equal(req.method, "GET");
    assert.ok(req.path.includes(`/api/projects/${projectId}/deployments`));
  });

  it("create_deployment sends POST /api/projects/:id/deployments", async () => {
    api.setResponse(201, { environment: "staging", status: "pending" });
    await client.callTool("create_deployment", {
      environment: "staging",
      image: "myapp:v1",
    });
    const req = api.lastRequest();
    assert.equal(req.method, "POST");
    assert.ok(req.path.includes("/deployments"));
  });

  it("rollback_deployment sends POST .../deployments/:env/rollback", async () => {
    api.setResponse(200, {});
    await client.callTool("rollback_deployment", { environment: "staging" });
    const req = api.lastRequest();
    assert.equal(req.method, "POST");
    assert.ok(req.path.includes("/deployments/staging/rollback"));
  });

  it("handles error responses", async () => {
    api.setResponse(500, { message: "server error" });
    await assert.rejects(
      () => client.callTool("list_deployments", {}),
      /500|server error/,
    );
  });
});
