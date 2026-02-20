#!/bin/bash
set -euo pipefail
cd /workspace

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
esac

MCP_JSON+='}}'
echo "$MCP_JSON" > /tmp/mcp-config.json

# ---------------------------------------------------------------------------
# Run Claude Code with MCP config, streaming JSON output
# ---------------------------------------------------------------------------
claude --output-format stream-json --mcp-config /tmp/mcp-config.json "$@"
EXIT_CODE=$?

# ---------------------------------------------------------------------------
# After claude exits, push whatever it did
# ---------------------------------------------------------------------------
if [ -n "$(git status --porcelain)" ]; then
  git add -A
  git commit -m "agent session ${SESSION_ID:-unknown}"
  git push origin "${BRANCH:-agent/${SESSION_ID:-unknown}}"
fi

exit $EXIT_CODE
