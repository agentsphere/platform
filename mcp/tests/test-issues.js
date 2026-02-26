/**
 * Tests for platform-issues MCP server
 */

import { describe, it, before, after } from "node:test";
import assert from "node:assert/strict";
import { setup, teardown } from "./helpers.js";

describe("platform-issues", () => {
  let api, client, projectId;

  before(async () => {
    ({ api, client, projectId } = await setup("platform-issues.js"));
  });

  after(async () => {
    await teardown(api, client);
  });

  it("list_tools returns all expected tools", async () => {
    const tools = await client.listTools();
    const names = tools.map((t) => t.name);
    assert.ok(names.includes("list_issues"), "missing list_issues");
    assert.ok(names.includes("get_issue"), "missing get_issue");
    assert.ok(names.includes("create_issue"), "missing create_issue");
    assert.ok(names.includes("update_issue"), "missing update_issue");
    assert.ok(names.includes("add_issue_comment"), "missing add_issue_comment");
    assert.ok(names.includes("list_issue_comments"), "missing list_issue_comments");
    assert.ok(names.includes("list_merge_requests"), "missing list_merge_requests");
    assert.ok(names.includes("get_merge_request"), "missing get_merge_request");
    assert.ok(names.includes("create_merge_request"), "missing create_merge_request");
    assert.ok(names.includes("update_merge_request"), "missing update_merge_request");
    assert.ok(names.includes("add_mr_comment"), "missing add_mr_comment");
    assert.ok(names.includes("merge_mr"), "missing merge_mr");
  });

  it("create_issue sends POST /api/projects/:id/issues", async () => {
    api.setResponse(201, { number: 1, title: "Bug" });
    await client.callTool("create_issue", { title: "Bug", body: "It broke" });
    const req = api.lastRequest();
    assert.equal(req.method, "POST");
    assert.ok(req.path.includes(`/api/projects/${projectId}/issues`));
    assert.equal(req.body.title, "Bug");
  });

  it("get_issue sends GET /api/projects/:id/issues/:number", async () => {
    api.setResponse(200, { number: 42, title: "Test" });
    await client.callTool("get_issue", { number: 42 });
    const req = api.lastRequest();
    assert.equal(req.method, "GET");
    assert.ok(req.path.includes(`/issues/42`));
  });

  it("merge_mr sends POST .../merge-requests/:number/merge", async () => {
    api.setResponse(200, { number: 5, status: "merged" });
    await client.callTool("merge_mr", { number: 5 });
    const req = api.lastRequest();
    assert.equal(req.method, "POST");
    assert.ok(req.path.includes(`/merge-requests/5/merge`));
  });

  it("merge_mr handles conflict error", async () => {
    api.setResponse(409, { message: "merge conflict" });
    await assert.rejects(
      () => client.callTool("merge_mr", { number: 5 }),
      /409/,
    );
  });

  it("update_issue sends PATCH /api/projects/:id/issues/:number", async () => {
    api.setResponse(200, { number: 1, status: "closed" });
    await client.callTool("update_issue", { number: 1, status: "closed" });
    const req = api.lastRequest();
    assert.equal(req.method, "PATCH");
    assert.ok(req.path.includes(`/issues/1`));
  });

  it("create_merge_request sends POST .../merge-requests", async () => {
    api.setResponse(201, { number: 1, title: "Feature" });
    await client.callTool("create_merge_request", {
      title: "Feature",
      source_branch: "feat/x",
    });
    const req = api.lastRequest();
    assert.equal(req.method, "POST");
    assert.ok(req.path.includes("/merge-requests"));
    assert.equal(req.body.title, "Feature");
    assert.equal(req.body.source_branch, "feat/x");
  });
});
