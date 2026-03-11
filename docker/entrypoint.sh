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
MCP_DIR="/usr/local/lib/mcp/servers"

# Core server is always included
MCP_JSON='{"mcpServers":{"platform-core":{"command":"node","args":["'"$MCP_DIR"'/platform-core.js"]}'

case "$ROLE" in
  dev)
    MCP_JSON+=',"platform-pipeline":{"command":"node","args":["'"$MCP_DIR"'/platform-pipeline.js"]}'
    MCP_JSON+=',"platform-issues":{"command":"node","args":["'"$MCP_DIR"'/platform-issues.js"]}'
    ;;
  ops)
    MCP_JSON+=',"platform-pipeline":{"command":"node","args":["'"$MCP_DIR"'/platform-pipeline.js"]}'
    MCP_JSON+=',"platform-deploy":{"command":"node","args":["'"$MCP_DIR"'/platform-deploy.js"]}'
    MCP_JSON+=',"platform-observe":{"command":"node","args":["'"$MCP_DIR"'/platform-observe.js"]}'
    ;;
  admin)
    for s in platform-pipeline platform-issues platform-deploy platform-observe platform-admin; do
      MCP_JSON+=',"'"$s"'":{"command":"node","args":["'"$MCP_DIR"'/'"$s"'.js"]}'
    done
    ;;
  ui)
    MCP_JSON+=',"platform-issues":{"command":"node","args":["'"$MCP_DIR"'/platform-issues.js"]}'
    ;;
  test)
    MCP_JSON+=',"platform-pipeline":{"command":"node","args":["'"$MCP_DIR"'/platform-pipeline.js"]}'
    MCP_JSON+=',"platform-issues":{"command":"node","args":["'"$MCP_DIR"'/platform-issues.js"]}'
    MCP_JSON+=',"platform-observe":{"command":"node","args":["'"$MCP_DIR"'/platform-observe.js"]}'
    ;;
  review)
    MCP_JSON+=',"platform-issues":{"command":"node","args":["'"$MCP_DIR"'/platform-issues.js"]}'
    ;;
  manager|create-app)
    for s in platform-pipeline platform-issues platform-deploy platform-observe platform-admin; do
      MCP_JSON+=',"'"$s"'":{"command":"node","args":["'"$MCP_DIR"'/'"$s"'.js"]}'
    done
    ;;
esac

# Add browser MCP server when browser sidecar is enabled
if [ "${BROWSER_ENABLED:-}" = "true" ]; then
    MCP_JSON+=',"platform-browser":{"command":"node","args":["'"$MCP_DIR"'/platform-browser.js"]}'
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

# ---------------------------------------------------------------------------
# Run Claude Code with MCP config, streaming JSON output
# ---------------------------------------------------------------------------
# MCP servers are generated above but currently disabled due to a Claude CLI
# compatibility issue where --mcp-config causes the process to hang
# indefinitely during MCP server startup. The coding agent works fine with
# built-in tools (Bash, Edit, Read, Write).  Re-enable when the issue is
# resolved by uncommenting --mcp-config below.
claude --print --output-format stream-json --verbose --dangerously-skip-permissions "$@"
EXIT_CODE=$?

# ---------------------------------------------------------------------------
# After claude exits, push whatever it did
# ---------------------------------------------------------------------------
if [ -n "$(git status --porcelain)" ]; then
  git add -A
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
