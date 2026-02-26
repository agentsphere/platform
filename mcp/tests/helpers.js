/**
 * MCP Test Helpers
 *
 * Provides MockApiServer (captures HTTP requests) and McpTestClient
 * (spawns an MCP server as a child process and communicates via stdio).
 */

import { createServer } from "node:http";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { randomUUID } from "node:crypto";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const __dirname = dirname(fileURLToPath(import.meta.url));
const SERVERS_DIR = join(__dirname, "..", "servers");

// ---------------------------------------------------------------------------
// MockApiServer — captures requests for assertion
// ---------------------------------------------------------------------------

export class MockApiServer {
  constructor() {
    this.requests = [];
    this.nextResponse = { status: 200, body: {} };
    this.server = createServer((req, res) => {
      let body = "";
      req.on("data", (chunk) => (body += chunk));
      req.on("end", () => {
        this.requests.push({
          method: req.method,
          path: req.url,
          headers: req.headers,
          body: body ? JSON.parse(body) : null,
        });
        res.writeHead(this.nextResponse.status, { "Content-Type": "application/json" });
        res.end(JSON.stringify(this.nextResponse.body));
      });
    });
  }

  async start() {
    this.server.listen(0, "127.0.0.1");
    await once(this.server, "listening");
    const addr = this.server.address();
    this.url = `http://127.0.0.1:${addr.port}`;
    return this.url;
  }

  setResponse(status, body) {
    this.nextResponse = { status, body };
  }

  lastRequest() {
    return this.requests[this.requests.length - 1];
  }

  reset() {
    this.requests = [];
    this.nextResponse = { status: 200, body: {} };
  }

  async close() {
    this.server.close();
    await once(this.server, "close");
  }
}

// ---------------------------------------------------------------------------
// McpTestClient — communicates with an MCP server via stdio JSON-RPC
// ---------------------------------------------------------------------------

export class McpTestClient {
  constructor(serverFile, env = {}) {
    this.serverFile = join(SERVERS_DIR, serverFile);
    this.env = env;
    this.proc = null;
    this.msgId = 0;
    this.pending = new Map();
    this.buffer = "";
  }

  async start() {
    this.proc = spawn("node", [this.serverFile], {
      stdio: ["pipe", "pipe", "pipe"],
      env: { ...process.env, ...this.env },
    });

    this.proc.stdout.on("data", (data) => {
      this.buffer += data.toString();
      let newlineIdx;
      while ((newlineIdx = this.buffer.indexOf("\n")) !== -1) {
        const line = this.buffer.slice(0, newlineIdx).trim();
        this.buffer = this.buffer.slice(newlineIdx + 1);
        if (!line) continue;
        try {
          const msg = JSON.parse(line);
          if (msg.id !== undefined && this.pending.has(msg.id)) {
            const { resolve } = this.pending.get(msg.id);
            this.pending.delete(msg.id);
            resolve(msg);
          }
        } catch {
          // ignore non-JSON lines (stderr leaking, etc.)
        }
      }
    });

    // Initialize the MCP connection
    const initResult = await this.send("initialize", {
      protocolVersion: "2024-11-05",
      capabilities: {},
      clientInfo: { name: "test-client", version: "0.0.1" },
    });

    // Send initialized notification
    this.notify("notifications/initialized", {});

    return initResult;
  }

  send(method, params = {}) {
    const id = ++this.msgId;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`Timeout waiting for response to ${method} (id=${id})`));
      }, 5000);

      this.pending.set(id, {
        resolve: (msg) => {
          clearTimeout(timer);
          resolve(msg);
        },
      });

      const msg = JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n";
      this.proc.stdin.write(msg);
    });
  }

  notify(method, params = {}) {
    const msg = JSON.stringify({ jsonrpc: "2.0", method, params }) + "\n";
    this.proc.stdin.write(msg);
  }

  async listTools() {
    const resp = await this.send("tools/list", {});
    return resp.result?.tools || [];
  }

  async callTool(name, args = {}) {
    const resp = await this.send("tools/call", { name, arguments: args });
    if (resp.error) {
      throw new Error(`MCP error: ${resp.error.message || JSON.stringify(resp.error)}`);
    }
    // The MCP SDK wraps tool handler errors as isError content
    if (resp.result?.isError) {
      const text = resp.result.content?.[0]?.text || "unknown error";
      throw new Error(`Tool error: ${text}`);
    }
    return resp.result;
  }

  async close() {
    if (this.proc) {
      this.proc.stdin.end();
      this.proc.kill("SIGTERM");
      // Give it a moment to clean up
      await new Promise((r) => setTimeout(r, 100));
      if (!this.proc.killed) this.proc.kill("SIGKILL");
    }
  }
}

// ---------------------------------------------------------------------------
// Test setup helper
// ---------------------------------------------------------------------------

/**
 * Create a MockApiServer and McpTestClient pair for testing.
 * @param {string} serverFile - MCP server filename (e.g. "platform-core.js")
 * @param {object} extraEnv - Additional env vars
 * @returns {{ api: MockApiServer, client: McpTestClient }}
 */
export async function setup(serverFile, extraEnv = {}) {
  const api = new MockApiServer();
  const url = await api.start();

  const projectId = randomUUID();
  const sessionId = randomUUID();

  const client = new McpTestClient(serverFile, {
    PLATFORM_API_URL: url,
    PLATFORM_API_TOKEN: "test-token-123",
    PROJECT_ID: projectId,
    SESSION_ID: sessionId,
    ...extraEnv,
  });

  await client.start();
  return { api, client, projectId, sessionId };
}

/**
 * Teardown helper — close both client and api server.
 */
export async function teardown(api, client) {
  await client.close();
  await api.close();
}
