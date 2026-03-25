/**
 * Tests for platform-core MCP server
 */

import { describe, it, before, after } from "node:test";
import assert from "node:assert/strict";
import { setup, teardown } from "./helpers.js";

describe("platform-core", () => {
  let api, client, projectId, sessionId;

  before(async () => {
    ({ api, client, projectId, sessionId } = await setup("platform-core.js"));
  });

  after(async () => {
    await teardown(api, client);
  });

  it("list_tools returns all expected tools", async () => {
    const tools = await client.listTools();
    const names = tools.map((t) => t.name);
    assert.ok(names.includes("get_project"), "missing get_project");
    assert.ok(names.includes("list_projects"), "missing list_projects");
    assert.ok(names.includes("spawn_agent"), "missing spawn_agent");
    assert.ok(names.includes("list_children"), "missing list_children");
    assert.ok(names.includes("create_project"), "missing create_project");
    assert.ok(names.includes("update_project"), "missing update_project");
    assert.ok(names.includes("delete_project"), "missing delete_project");
    assert.ok(names.includes("get_session"), "missing get_session");
    assert.ok(names.includes("send_message_to_session"), "missing send_message_to_session");
    assert.ok(names.includes("ask_for_secret"), "missing ask_for_secret");
    assert.ok(names.includes("get_worker_progress"), "missing get_worker_progress");
    assert.ok(names.includes("read_secret"), "missing read_secret");
  });

  it("get_project sends GET /api/projects/:id", async () => {
    api.setResponse(200, { id: projectId, name: "test-proj" });
    await client.callTool("get_project", { project_id: projectId });
    const req = api.lastRequest();
    assert.equal(req.method, "GET");
    assert.ok(req.path.startsWith(`/api/projects/${projectId}`));
  });

  it("list_projects sends GET /api/projects", async () => {
    api.setResponse(200, { items: [], total: 0 });
    await client.callTool("list_projects", { limit: 10 });
    const req = api.lastRequest();
    assert.equal(req.method, "GET");
    assert.ok(req.path.startsWith("/api/projects"));
  });

  it("create_project sends POST /api/projects", async () => {
    api.setResponse(201, { id: "new-id", name: "my-app" });
    await client.callTool("create_project", { name: "my-app", description: "A test app" });
    const req = api.lastRequest();
    assert.equal(req.method, "POST");
    assert.ok(req.path.startsWith("/api/projects"));
    assert.equal(req.body.name, "my-app");
    assert.equal(req.body.description, "A test app");
  });

  it("update_project sends PATCH /api/projects/:id", async () => {
    api.setResponse(200, { id: projectId, display_name: "Updated" });
    await client.callTool("update_project", { project_id: projectId, display_name: "Updated" });
    const req = api.lastRequest();
    assert.equal(req.method, "PATCH");
    assert.ok(req.path.startsWith(`/api/projects/${projectId}`));
    assert.equal(req.body.display_name, "Updated");
  });

  it("delete_project sends DELETE /api/projects/:id", async () => {
    api.setResponse(200, null);
    await client.callTool("delete_project", { project_id: projectId });
    const req = api.lastRequest();
    assert.equal(req.method, "DELETE");
    assert.ok(req.path.startsWith(`/api/projects/${projectId}`));
  });

  it("get_session sends GET /api/projects/:pid/sessions/:sid", async () => {
    const sid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    api.setResponse(200, { id: sid, status: "running" });
    await client.callTool("get_session", { session_id: sid });
    const req = api.lastRequest();
    assert.equal(req.method, "GET");
    assert.ok(req.path.includes(`/sessions/${sid}`));
    assert.ok(req.path.includes(`/api/projects/`));
  });

  it("send_message_to_session sends POST .../sessions/:sid/message", async () => {
    const sid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    api.setResponse(200, { ok: true });
    await client.callTool("send_message_to_session", { session_id: sid, content: "hello" });
    const req = api.lastRequest();
    assert.equal(req.method, "POST");
    assert.ok(req.path.includes(`/sessions/${sid}/message`));
    assert.equal(req.body.content, "hello");
  });

  it("spawn_agent sends POST .../sessions/:sid/spawn", async () => {
    api.setResponse(200, { id: "child-id", status: "pending" });
    await client.callTool("spawn_agent", { prompt: "do something" });
    const req = api.lastRequest();
    assert.equal(req.method, "POST");
    assert.ok(req.path.includes("/spawn"));
    assert.equal(req.body.prompt, "do something");
  });

  it("create_project handles error response", async () => {
    api.setResponse(400, { message: "name already exists" });
    await assert.rejects(
      () => client.callTool("create_project", { name: "dup" }),
      /400/,
    );
  });

  it("ask_for_secret creates request and polls until completed", async () => {
    const requestId = "req-" + Date.now();
    // Queue: POST create → 201, then GET poll (pending), then GET poll (completed)
    api.queueResponses("POST", `/api/projects/${projectId}/secret-requests`,
      { status: 201, body: { id: requestId, status: "pending", name: "MY_TOKEN" } },
    );
    api.queueResponses("GET", `/api/projects/${projectId}/secret-requests/${requestId}`,
      { status: 200, body: { id: requestId, status: "pending", name: "MY_TOKEN" } },
      { status: 200, body: { id: requestId, status: "completed", name: "MY_TOKEN" } },
    );

    const result = await client.callTool("ask_for_secret", {
      name: "MY_TOKEN",
      prompt: "Please provide your token",
    });

    // Verify the POST was made to create the secret request
    const postReqs = api.requestsMatching("POST", `/api/projects/${projectId}/secret-requests`);
    assert.ok(postReqs.length >= 1, "should have created secret request");
    assert.equal(postReqs[0].body.name, "MY_TOKEN");
    assert.equal(postReqs[0].body.description, "Please provide your token");
    assert.equal(postReqs[0].body.session_id, sessionId);

    // Verify the result indicates completion
    const text = result.content[0].text;
    assert.ok(text.includes("completed"), "result should indicate completed");
    assert.ok(text.includes("MY_TOKEN"), "result should include secret name");
  });

  it("ask_for_secret returns error on timeout", async () => {
    const requestId = "req-timeout-" + Date.now();
    // Queue: POST create → 201, then GET poll returns timed_out
    api.queueResponses("POST", `/api/projects/${projectId}/secret-requests`,
      { status: 201, body: { id: requestId, status: "pending", name: "EXPIRED_KEY" } },
    );
    api.queueResponses("GET", `/api/projects/${projectId}/secret-requests/${requestId}`,
      { status: 200, body: { id: requestId, status: "timed_out", name: "EXPIRED_KEY" } },
    );

    // callTool throws on isError results, so expect rejection with timeout message
    await assert.rejects(
      () => client.callTool("ask_for_secret", {
        name: "EXPIRED_KEY",
        prompt: "This will timeout",
      }),
      /timed out/,
    );
  });
});
