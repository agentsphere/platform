/**
 * Tests for platform-admin MCP server
 */

import { describe, it, before, after } from "node:test";
import assert from "node:assert/strict";
import { setup, teardown } from "./helpers.js";

describe("platform-admin", () => {
  let api, client;

  before(async () => {
    ({ api, client } = await setup("platform-admin.js"));
  });

  after(async () => {
    await teardown(api, client);
  });

  it("list_tools returns expected admin tools", async () => {
    const tools = await client.listTools();
    const names = tools.map((t) => t.name);
    assert.ok(names.includes("list_users"), "missing list_users");
    assert.ok(names.includes("get_user"), "missing get_user");
    assert.ok(names.includes("create_user"), "missing create_user");
    assert.ok(names.includes("update_user"), "missing update_user");
    assert.ok(names.includes("deactivate_user"), "missing deactivate_user");
    assert.ok(names.includes("list_roles"), "missing list_roles");
    assert.ok(names.includes("create_role"), "missing create_role");
    assert.ok(names.includes("assign_role"), "missing assign_role");
    assert.ok(names.includes("remove_role"), "missing remove_role");
    assert.ok(names.includes("list_permissions"), "missing list_permissions");
    assert.ok(names.includes("list_delegations"), "missing list_delegations");
    assert.ok(names.includes("create_delegation"), "missing create_delegation");
    assert.ok(names.includes("revoke_delegation"), "missing revoke_delegation");
    assert.ok(names.includes("create_token_for_user"), "missing create_token_for_user");
  });

  it("list_users sends GET /api/admin/users", async () => {
    api.setResponse(200, { items: [], total: 0 });
    await client.callTool("list_users", {});
    const req = api.lastRequest();
    assert.equal(req.method, "GET");
    assert.ok(req.path.startsWith("/api/admin/users"));
  });

  it("create_user sends POST /api/admin/users", async () => {
    api.setResponse(201, { id: "user-id", name: "alice" });
    await client.callTool("create_user", { name: "alice", email: "a@b.com", password: "secret123" });
    const req = api.lastRequest();
    assert.equal(req.method, "POST");
    assert.ok(req.path.startsWith("/api/admin/users"));
    assert.equal(req.body.name, "alice");
  });

  it("deactivate_user sends POST /api/admin/users/:id/deactivate", async () => {
    const uid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    api.setResponse(200, {});
    await client.callTool("deactivate_user", { user_id: uid });
    const req = api.lastRequest();
    assert.equal(req.method, "POST");
    assert.ok(req.path.includes(`/users/${uid}/deactivate`));
  });

  it("handles error responses", async () => {
    api.setResponse(500, { message: "internal error" });
    await assert.rejects(
      () => client.callTool("list_users", {}),
      /500|internal error/,
    );
  });
});
