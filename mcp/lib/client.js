/**
 * Shared HTTP client for platform MCP servers.
 *
 * Reads configuration from environment:
 *   PLATFORM_API_URL   - Base URL of the platform API (e.g. http://platform:8080)
 *   PLATFORM_API_TOKEN - Bearer token for authentication
 *   PROJECT_ID         - Default project UUID (injected by agent pod)
 */

const API_URL = process.env.PLATFORM_API_URL || "http://localhost:8080";
const API_TOKEN = process.env.PLATFORM_API_TOKEN || "";
const PROJECT_ID = process.env.PROJECT_ID || "";

/**
 * Resolve {project_id} placeholders and prepend base URL.
 */
function buildUrl(path, params = {}) {
  let resolved = path.replace("{project_id}", params.project_id || PROJECT_ID);
  for (const [key, value] of Object.entries(params)) {
    resolved = resolved.replace(`{${key}}`, encodeURIComponent(value));
  }
  return `${API_URL}${resolved}`;
}

/**
 * Make an authenticated request to the platform API.
 * Returns parsed JSON body. Throws on non-2xx status.
 */
async function request(method, path, { params = {}, body, query } = {}) {
  const url = new URL(buildUrl(path, params));
  if (query) {
    for (const [key, value] of Object.entries(query)) {
      if (value !== undefined && value !== null && value !== "") {
        url.searchParams.set(key, String(value));
      }
    }
  }

  const options = {
    method,
    headers: {
      Authorization: `Bearer ${API_TOKEN}`,
      "Content-Type": "application/json",
    },
  };
  if (body !== undefined) {
    options.body = JSON.stringify(body);
  }

  const res = await fetch(url.toString(), options);
  const text = await res.text();

  if (!res.ok) {
    let detail = text;
    try {
      const json = JSON.parse(text);
      detail = json.message || json.error || text;
    } catch {
      // use raw text
    }
    throw new Error(`API ${method} ${path} returned ${res.status}: ${detail}`);
  }

  if (!text) return null;
  return JSON.parse(text);
}

export function apiGet(path, opts) {
  return request("GET", path, opts);
}

export function apiPost(path, opts) {
  return request("POST", path, opts);
}

export function apiPatch(path, opts) {
  return request("PATCH", path, opts);
}

export function apiDelete(path, opts) {
  return request("DELETE", path, opts);
}

export { PROJECT_ID };
