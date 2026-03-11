#!/usr/bin/env bash
# Mock Claude CLI variant that creates a file, commits, and pushes before
# emitting NDJSON. Unlike the standard mock, this does NOT use python3
# (unavailable in node:22-slim). Used by e2e_agent_git_clone_push test.
set -euo pipefail

SESSION_ID="mock-session-id"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --session-id) SESSION_ID="$2"; shift 2 ;;
        --resume) SESSION_ID="$2"; shift 2 ;;
        -p) shift 2 ;;
        *) shift ;;
    esac
done

cd /workspace
echo "agent-pushed-content" > agent-test-file.txt
# Redirect all git output to stderr — stdout must stay clean for NDJSON
git add -A >&2
git commit -m "agent push test" >&2
git push origin main >&2 2>&1 || git push origin HEAD:refs/heads/main >&2 2>&1

# Emit NDJSON
echo '{"type":"system","subtype":"init","session_id":"'"$SESSION_ID"'","tools":[],"model":"mock","claude_code_version":"1.0.0-mock"}'
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"Pushed test file"}]},"session_id":"'"$SESSION_ID"'"}'
echo '{"type":"result","subtype":"success","session_id":"'"$SESSION_ID"'","is_error":false,"result":"Pushed test file","usage":{"input_tokens":100,"output_tokens":50}}'
