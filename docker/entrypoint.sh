#!/bin/bash
set -euo pipefail
cd /workspace

# ---------------------------------------------------------------------------
# Configure git credentials using PLATFORM_API_TOKEN for push auth
# ---------------------------------------------------------------------------
if [ -n "${PLATFORM_API_TOKEN:-}" ]; then
  printf '#!/bin/sh\necho "$PLATFORM_API_TOKEN"\n' > /tmp/git-askpass.sh
  chmod +x /tmp/git-askpass.sh
  export GIT_ASKPASS=/tmp/git-askpass.sh
fi

# ---------------------------------------------------------------------------
# Generate MCP config based on agent role
# ---------------------------------------------------------------------------
ROLE="${AGENT_ROLE:-dev}"
# Prefer workspace-downloaded MCP (init container), fallback to baked-in
if [ -d /workspace/.platform/mcp/servers ]; then
  MCP_DIR="/workspace/.platform/mcp/servers"
else
  MCP_DIR="/usr/local/lib/mcp/servers"
fi

# Helper: build a single MCP server entry with platform env vars injected
mcp_server() {
  local name="$1"
  cat <<SEOF
"$name":{"command":"node","args":["$MCP_DIR/$name.js"],"env":{"PLATFORM_API_URL":"${PLATFORM_API_URL:-}","PLATFORM_API_TOKEN":"${PLATFORM_API_TOKEN:-}","SESSION_ID":"${SESSION_ID:-}","PROJECT_ID":"${PROJECT_ID:-}"}}
SEOF
}

# Core server is always included
MCP_JSON='{"mcpServers":{'
MCP_JSON+="$(mcp_server platform-core)"

case "$ROLE" in
  dev)
    MCP_JSON+=",$(mcp_server platform-pipeline)"
    MCP_JSON+=",$(mcp_server platform-issues)"
    ;;
  ops)
    MCP_JSON+=",$(mcp_server platform-pipeline)"
    MCP_JSON+=",$(mcp_server platform-deploy)"
    MCP_JSON+=",$(mcp_server platform-observe)"
    ;;
  admin)
    for s in platform-pipeline platform-issues platform-deploy platform-observe platform-admin; do
      MCP_JSON+=",$(mcp_server "$s")"
    done
    ;;
  ui)
    MCP_JSON+=",$(mcp_server platform-issues)"
    ;;
  test)
    MCP_JSON+=",$(mcp_server platform-pipeline)"
    MCP_JSON+=",$(mcp_server platform-issues)"
    MCP_JSON+=",$(mcp_server platform-observe)"
    ;;
  review)
    MCP_JSON+=",$(mcp_server platform-issues)"
    ;;
  manager|create-app)
    for s in platform-pipeline platform-issues platform-deploy platform-observe platform-admin; do
      MCP_JSON+=",$(mcp_server "$s")"
    done
    ;;
esac

# Add browser MCP server when browser sidecar is enabled
if [ "${BROWSER_ENABLED:-}" = "true" ]; then
    MCP_JSON+=",$(mcp_server platform-browser)"
fi

MCP_JSON+='}}'
echo "$MCP_JSON" > /tmp/mcp-config.json

# ---------------------------------------------------------------------------
# Write env vars to a discoverable file for tools that run inside claude's
# sandbox (Claude CLI's Bash tool may not inherit all container env vars)
# ---------------------------------------------------------------------------
mkdir -p /workspace/.platform
cat > /workspace/.platform/.env <<ENVEOF
PROJECT_ID=${PROJECT_ID:-}
BRANCH=${BRANCH:-}
SESSION_ID=${SESSION_ID:-}
PLATFORM_API_URL=${PLATFORM_API_URL:-}
PLATFORM_API_TOKEN=${PLATFORM_API_TOKEN:-}
ENVEOF
chmod 600 /workspace/.platform/.env

# Ensure platform secrets dir is never committed
grep -qxF '.platform/' /workspace/.gitignore 2>/dev/null || echo '.platform/' >> /workspace/.gitignore

# ---------------------------------------------------------------------------
# Run Claude Code with MCP config, streaming JSON output
# ---------------------------------------------------------------------------
claude --print --output-format stream-json --verbose --dangerously-skip-permissions \
  --mcp-config /tmp/mcp-config.json "$@"
EXIT_CODE=$?

# ---------------------------------------------------------------------------
# After claude exits, push whatever it did
# ---------------------------------------------------------------------------
if [ -n "$(git status --porcelain)" ]; then
  git add -A -- ':!.platform/'
  git commit -m "agent session ${SESSION_ID:-unknown}"
  git push origin "${BRANCH:-agent/${SESSION_ID:-unknown}}"
fi

# ---------------------------------------------------------------------------
# Wait for pipeline build to finish (if a pipeline config exists)
# ---------------------------------------------------------------------------
if [ -f /workspace/.platform.yaml ] && [ "$EXIT_CODE" -eq 0 ]; then
  echo "Verifying pipeline build..."
  platform-build-status 300 10 || echo "WARNING: Pipeline build did not succeed (exit $?)"
fi

exit $EXIT_CODE
