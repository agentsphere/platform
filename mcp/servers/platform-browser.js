/**
 * Platform Browser MCP Server
 *
 * Provides headless browser tools via Playwright, connecting to a Chromium
 * sidecar container over CDP (Chrome DevTools Protocol).
 *
 * Loaded when: BROWSER_ENABLED=true (roles: ui, test).
 *
 * Environment:
 *   BROWSER_CDP_URL          - CDP endpoint (default: http://localhost:9222)
 *   BROWSER_ALLOWED_ORIGINS  - JSON array of allowed navigation origins
 */

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { ListToolsRequestSchema, CallToolRequestSchema } from "@modelcontextprotocol/sdk/types.js";
import { chromium } from "playwright-core";

const CDP_URL = process.env.BROWSER_CDP_URL || "http://localhost:9222";
const ALLOWED_ORIGINS = JSON.parse(process.env.BROWSER_ALLOWED_ORIGINS || "[]");

// ---------------------------------------------------------------------------
// URL validation
// ---------------------------------------------------------------------------

function isAllowedOrigin(url) {
  try {
    const parsed = new URL(url);
    const origin = parsed.origin;
    return ALLOWED_ORIGINS.some((o) => {
      try {
        return new URL(o).origin === origin;
      } catch {
        return false;
      }
    });
  } catch {
    return false;
  }
}

// ---------------------------------------------------------------------------
// Lazy browser/page connection
// ---------------------------------------------------------------------------

let browser = null;
let page = null;

async function getPage() {
  if (!browser) {
    browser = await chromium.connectOverCDP(CDP_URL);
  }
  if (!page) {
    const contexts = browser.contexts();
    const ctx = contexts[0] || (await browser.newContext());
    const pages = ctx.pages();
    page = pages[0] || (await ctx.newPage());
  }
  return page;
}

// ---------------------------------------------------------------------------
// MCP server
// ---------------------------------------------------------------------------

const server = new Server(
  { name: "platform-browser", version: "0.1.0" },
  { capabilities: { tools: {} } },
);

const TOOLS = [
  {
    name: "browser_navigate",
    description:
      "Navigate the browser to a URL. Only allowed origins can be navigated to. " +
      `Allowed: ${ALLOWED_ORIGINS.join(", ") || "(none)"}`,
    inputSchema: {
      type: "object",
      properties: {
        url: { type: "string", description: "URL to navigate to" },
      },
      required: ["url"],
    },
  },
  {
    name: "browser_screenshot",
    description:
      "Take a screenshot of the current page. Returns a base64-encoded PNG.",
    inputSchema: { type: "object", properties: {} },
  },
  {
    name: "browser_click",
    description: "Click an element identified by CSS selector.",
    inputSchema: {
      type: "object",
      properties: {
        selector: { type: "string", description: "CSS selector of element to click" },
      },
      required: ["selector"],
    },
  },
  {
    name: "browser_type",
    description: "Type text into an element identified by CSS selector.",
    inputSchema: {
      type: "object",
      properties: {
        selector: { type: "string", description: "CSS selector of element to type into" },
        text: { type: "string", description: "Text to type" },
      },
      required: ["selector", "text"],
    },
  },
  {
    name: "browser_get_text",
    description:
      "Get the text content of the page or a specific element. " +
      "If no selector is provided, returns the full page text content.",
    inputSchema: {
      type: "object",
      properties: {
        selector: {
          type: "string",
          description: "CSS selector (optional — defaults to full page body)",
        },
      },
    },
  },
  {
    name: "browser_evaluate",
    description:
      "Evaluate JavaScript in the page context and return the result. " +
      "The script should be a JS expression or IIFE.",
    inputSchema: {
      type: "object",
      properties: {
        script: { type: "string", description: "JavaScript code to evaluate" },
      },
      required: ["script"],
    },
  },
  {
    name: "browser_wait",
    description: "Wait for an element matching the CSS selector to appear in the DOM.",
    inputSchema: {
      type: "object",
      properties: {
        selector: { type: "string", description: "CSS selector to wait for" },
        timeout: {
          type: "integer",
          description: "Max wait time in milliseconds (default 5000)",
        },
      },
      required: ["selector"],
    },
  },
  {
    name: "browser_scroll",
    description: "Scroll the page up or down by a given number of pixels.",
    inputSchema: {
      type: "object",
      properties: {
        direction: {
          type: "string",
          enum: ["up", "down"],
          description: "Scroll direction",
        },
        amount: {
          type: "integer",
          description: "Pixels to scroll (default 500)",
        },
      },
      required: ["direction"],
    },
  },
  {
    name: "browser_get_url",
    description: "Get the current page URL.",
    inputSchema: { type: "object", properties: {} },
  },
];

server.setRequestHandler(ListToolsRequestSchema, async () => ({ tools: TOOLS }));

server.setRequestHandler(CallToolRequestSchema, async (request) => {
  const { name, arguments: args = {} } = request.params;

  switch (name) {
    case "browser_navigate": {
      if (!isAllowedOrigin(args.url)) {
        throw new Error(
          `Navigation blocked: "${args.url}" is not in allowed origins. ` +
            `Allowed: ${ALLOWED_ORIGINS.join(", ")}`,
        );
      }
      const p = await getPage();
      await p.goto(args.url, { waitUntil: "domcontentloaded", timeout: 30000 });
      return {
        content: [{ type: "text", text: `Navigated to ${p.url()}` }],
      };
    }

    case "browser_screenshot": {
      const p = await getPage();
      const buf = await p.screenshot({ type: "png", fullPage: false });
      const b64 = buf.toString("base64");
      return {
        content: [{ type: "image", data: b64, mimeType: "image/png" }],
      };
    }

    case "browser_click": {
      const p = await getPage();
      await p.click(args.selector, { timeout: 5000 });
      return {
        content: [{ type: "text", text: `Clicked "${args.selector}"` }],
      };
    }

    case "browser_type": {
      const p = await getPage();
      await p.fill(args.selector, args.text, { timeout: 5000 });
      return {
        content: [
          {
            type: "text",
            text: `Typed ${args.text.length} chars into "${args.selector}"`,
          },
        ],
      };
    }

    case "browser_get_text": {
      const p = await getPage();
      const selector = args.selector || "body";
      const text = await p.textContent(selector, { timeout: 5000 });
      return {
        content: [{ type: "text", text: text || "(empty)" }],
      };
    }

    case "browser_evaluate": {
      const p = await getPage();
      const result = await p.evaluate(args.script);
      return {
        content: [
          {
            type: "text",
            text:
              typeof result === "string" ? result : JSON.stringify(result, null, 2),
          },
        ],
      };
    }

    case "browser_wait": {
      const p = await getPage();
      const timeout = args.timeout || 5000;
      await p.waitForSelector(args.selector, { timeout });
      return {
        content: [
          { type: "text", text: `Element "${args.selector}" appeared` },
        ],
      };
    }

    case "browser_scroll": {
      const p = await getPage();
      const amount = args.amount || 500;
      const delta = args.direction === "up" ? -amount : amount;
      await p.evaluate((dy) => window.scrollBy(0, dy), delta);
      return {
        content: [
          { type: "text", text: `Scrolled ${args.direction} ${amount}px` },
        ],
      };
    }

    case "browser_get_url": {
      const p = await getPage();
      return {
        content: [{ type: "text", text: p.url() }],
      };
    }

    default:
      throw new Error(`Unknown tool: ${name}`);
  }
});

const transport = new StdioServerTransport();
await server.connect(transport);
