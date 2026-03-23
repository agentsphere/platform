#!/usr/bin/env bash
# cli-test-pubsub.sh — Run agent-runner CLI pub/sub tests against ephemeral Valkey
#
# Deploys Valkey into an ephemeral cluster namespace, port-forwards it,
# and runs the ignored pub/sub integration tests in cli/agent-runner.
#
# Usage:
#   bash hack/cli-test-pubsub.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# ── Cluster detection ───────────────────────────────────────────────────
source "${SCRIPT_DIR}/cluster-info.sh"

# ── Namespace ID ──────────────────────────────────────────────────────────
RUN_ID=$(openssl rand -hex 4)
NS="platform-cli-test-${RUN_ID}"

PF_PID=""

# ── Cleanup on exit ───────────────────────────────────────────────────────
cleanup() {
  echo ""
  echo "==> Cleaning up"
  if [[ -n "$PF_PID" ]]; then
    kill "$PF_PID" 2>/dev/null || true
    wait "$PF_PID" 2>/dev/null || true
  fi
  echo "  Deleting namespace: ${NS}"
  kubectl delete namespace "${NS}" --wait=false 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# ── Pre-flight checks ────────────────────────────────────────────────────
echo "==> Pre-flight checks"

# ── Find free local port ─────────────────────────────────────────────────
find_free_port() {
  python3 -c "import socket; s=socket.socket(); s.bind(('',0)); print(s.getsockname()[1]); s.close()"
}

VALKEY_PORT=$(find_free_port)

# ── Deploy Valkey ─────────────────────────────────────────────────────────
echo ""
echo "==> Deploying Valkey into namespace: ${NS}"
kubectl create namespace "${NS}" --dry-run=client -o yaml | kubectl apply -f -
kubectl apply -n "${NS}" -f "${SCRIPT_DIR}/test-manifests/valkey.yaml"
kubectl wait -n "${NS}" --for=condition=Ready pod/valkey --timeout=30s
echo "  Valkey ready"

# ── Port-forward ──────────────────────────────────────────────────────────
echo ""
echo "==> Port-forwarding Valkey → 127.0.0.1:${VALKEY_PORT}"
kubectl port-forward -n "${NS}" pod/valkey "${VALKEY_PORT}:6379" &>/dev/null &
PF_PID=$!

# Wait for port-forward to be ready
echo -n "  Waiting for port-forward"
for i in $(seq 1 30); do
  if nc -z 127.0.0.1 "$VALKEY_PORT" 2>/dev/null; then
    break
  fi
  echo -n "."
  sleep 0.5
done
echo " ready"

# Verify port-forward process is still alive
if ! kill -0 "$PF_PID" 2>/dev/null; then
  echo "ERROR: port-forward process died unexpectedly"
  exit 1
fi

# ── Run CLI pub/sub tests ────────────────────────────────────────────────
echo ""
echo "==> Running agent-runner pub/sub tests (VALKEY_URL=redis://127.0.0.1:${VALKEY_PORT})"
echo ""

cd "${PROJECT_DIR}/cli/agent-runner"
VALKEY_URL="redis://127.0.0.1:${VALKEY_PORT}" \
  cargo test --bin agent-runner -- --ignored

echo ""
echo "==> All pub/sub tests passed"
