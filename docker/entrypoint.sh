#!/bin/bash
set -euo pipefail
cd /workspace

# Run claude interactively, streaming JSON output
claude --output-format stream-json "$@"
EXIT_CODE=$?

# After claude exits, push whatever it did
if [ -n "$(git status --porcelain)" ]; then
  git add -A
  git commit -m "agent session ${SESSION_ID:-unknown}"
  git push origin "agent/${SESSION_ID:-unknown}"
fi

exit $EXIT_CODE
