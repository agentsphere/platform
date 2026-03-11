#!/bin/bash
# platform-build-status — Poll the platform pipeline API until the build finishes.
#
# Usage: platform-build-status [timeout_seconds] [poll_interval_seconds]
#
# Required env vars:
#   PLATFORM_API_URL   — e.g. http://platform.platform.svc.cluster.local:8080
#   PLATFORM_API_TOKEN — Bearer token for API auth
#   PROJECT_ID         — UUID of the project
#   BRANCH             — Branch name (without refs/heads/ prefix)
#
# Exit codes:
#   0 — pipeline succeeded
#   1 — pipeline failed or was cancelled
#   2 — timeout waiting for pipeline
#   3 — missing required environment variables
set -euo pipefail

TIMEOUT="${1:-300}"
INTERVAL="${2:-5}"
ELAPSED=0

# Fallback: source .platform/.env if vars are missing (Claude CLI sandbox
# may not inherit all container env vars)
if [ -z "${PROJECT_ID:-}" ] || [ -z "${BRANCH:-}" ]; then
  if [ -f /workspace/.platform/.env ]; then
    set -a
    . /workspace/.platform/.env
    set +a
  fi
fi

# Validate required env vars
for var in PLATFORM_API_URL PLATFORM_API_TOKEN PROJECT_ID BRANCH; do
  if [ -z "${!var:-}" ]; then
    echo "ERROR: $var is not set" >&2
    exit 3
  fi
done

API="${PLATFORM_API_URL}/api/projects/${PROJECT_ID}/pipelines"
AUTH="Authorization: Bearer ${PLATFORM_API_TOKEN}"
GIT_REF="refs/heads/${BRANCH}"

echo "Waiting for pipeline on ${GIT_REF} (timeout: ${TIMEOUT}s)..."

while [ "$ELAPSED" -lt "$TIMEOUT" ]; do
  RESPONSE=$(curl -sf -H "$AUTH" "${API}?git_ref=${GIT_REF}&limit=1" 2>/dev/null || echo '{"items":[]}')

  # Parse status from response — handles both {items:[...]} and [...] formats
  STATUS=$(echo "$RESPONSE" | node -e "
    const d = JSON.parse(require('fs').readFileSync(0, 'utf8'));
    const p = (d.items || d)[0];
    console.log(p ? p.status : 'none');
  " 2>/dev/null || echo "none")

  case "$STATUS" in
    success)
      echo "Pipeline succeeded!"
      exit 0
      ;;
    failure|cancelled)
      echo "Pipeline ${STATUS}."
      # Fetch pipeline ID for detailed info
      PIPELINE_ID=$(echo "$RESPONSE" | node -e "
        const d = JSON.parse(require('fs').readFileSync(0, 'utf8'));
        const p = (d.items || d)[0];
        console.log(p ? p.id : '');
      " 2>/dev/null || echo "")

      if [ -n "$PIPELINE_ID" ]; then
        DETAIL=$(curl -sf -H "$AUTH" "${API}/${PIPELINE_ID}" 2>/dev/null || echo '{}')

        # Print step summary
        echo ""
        echo "Step summary:"
        echo "$DETAIL" | node -e "
          const d = JSON.parse(require('fs').readFileSync(0, 'utf8'));
          (d.steps || []).forEach(s => {
            const exit_info = s.exit_code != null ? ' (exit ' + s.exit_code + ')' : '';
            const dur = s.duration_ms != null ? ' [' + (s.duration_ms / 1000).toFixed(1) + 's]' : '';
            console.log('  ' + s.name + ': ' + s.status + exit_info + dur);
          });
        " 2>/dev/null || true

        # Print logs for failed steps
        FAILED_STEPS=$(echo "$DETAIL" | node -e "
          const d = JSON.parse(require('fs').readFileSync(0, 'utf8'));
          (d.steps || []).filter(s => s.status === 'failure').forEach(s => console.log(s.id + '|' + s.name));
        " 2>/dev/null || echo "")

        echo "$FAILED_STEPS" | while IFS='|' read -r STEP_ID STEP_NAME; do
          if [ -n "$STEP_ID" ]; then
            echo ""
            echo "=== Logs: ${STEP_NAME} ==="
            curl -sf -H "$AUTH" "${API}/${PIPELINE_ID}/steps/${STEP_ID}/logs" 2>/dev/null || echo "(no logs available)"
            echo "=== End logs ==="
          fi
        done
      fi
      exit 1
      ;;
    pending|running)
      ;;
    none)
      # No pipeline found yet — it may not have been created yet
      ;;
  esac

  sleep "$INTERVAL"
  ELAPSED=$((ELAPSED + INTERVAL))
done

echo "Timeout waiting for pipeline after ${TIMEOUT}s"
exit 2
