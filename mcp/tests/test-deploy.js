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
    assert.ok(names.includes("list_targets"), "missing list_targets");
    assert.ok(names.includes("get_target"), "missing get_target");
    assert.ok(names.includes("create_target"), "missing create_target");
    assert.ok(names.includes("list_releases"), "missing list_releases");
    assert.ok(names.includes("get_release"), "missing get_release");
    assert.ok(names.includes("create_release"), "missing create_release");
    assert.ok(names.includes("adjust_traffic"), "missing adjust_traffic");
    assert.ok(names.includes("promote_release"), "missing promote_release");
    assert.ok(names.includes("rollback_release"), "missing rollback_release");
    assert.ok(names.includes("release_history"), "missing release_history");
    assert.ok(names.includes("staging_status"), "missing staging_status");
  });

  it("list_targets sends GET /api/projects/:id/targets", async () => {
    api.setResponse(200, { items: [], total: 0 });
    await client.callTool("list_targets", {});
    const req = api.lastRequest();
    assert.equal(req.method, "GET");
    assert.ok(req.path.includes(`/api/projects/${projectId}/targets`));
  });

  it("create_release sends POST /api/projects/:id/deploy-releases", async () => {
    api.setResponse(201, { id: "rel-1" });
    await client.callTool("create_release", {
      target_id: "tgt-1",
      image_ref: "registry/app:v1",
    });
    const req = api.lastRequest();
    assert.equal(req.method, "POST");
    assert.ok(req.path.includes(`/api/projects/${projectId}/deploy-releases`));
    assert.equal(req.body.target_id, "tgt-1");
    assert.equal(req.body.image_ref, "registry/app:v1");
  });

  it("rollback_release sends POST .../deploy-releases/:id/rollback", async () => {
    api.setResponse(200, {});
    await client.callTool("rollback_release", { release_id: "rel-1" });
    const req = api.lastRequest();
    assert.equal(req.method, "POST");
    assert.ok(req.path.includes("/deploy-releases/rel-1/rollback"));
  });

  it("adjust_traffic sends PATCH .../deploy-releases/:id/traffic", async () => {
    api.setResponse(200, {});
    await client.callTool("adjust_traffic", { release_id: "rel-1", weight: 50 });
    const req = api.lastRequest();
    assert.equal(req.method, "PATCH");
    assert.ok(req.path.includes("/deploy-releases/rel-1/traffic"));
    assert.equal(req.body.weight, 50);
  });

  it("handles error responses", async () => {
    api.setResponse(500, { message: "server error" });
    await assert.rejects(
      () => client.callTool("list_targets", {}),
      /500|server error/,
    );
  });
});
