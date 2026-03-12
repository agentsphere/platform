#!/usr/bin/env bash
# run-llm-e2e.sh — Run the LLM E2E create-app test against ephemeral Kind services.
#
# Unlike test-in-cluster.sh, this does NOT set CLAUDE_CLI_PATH to the mock —
# the manager subprocess needs the real Claude CLI.
#
# Usage:
#   bash hack/run-llm-e2e.sh

set -eo pipefail

KIND_CLUSTER="platform"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

export KUBECONFIG="${HOME}/.kube/kind-${KIND_CLUSTER}"

RUN_ID=$(openssl rand -hex 4)
NS_PREFIX="platform-test-${RUN_ID}"
SVC_NS="${NS_PREFIX}-services"
PIPELINE_NS="${NS_PREFIX}-pipelines"
AGENT_NS="${NS_PREFIX}-agents"

PLATFORM_HOST="host.docker.internal"

cleanup() {
  echo ""
  echo "==> Cleaning up"
  kubectl get namespaces -o name | grep "^namespace/${NS_PREFIX}" | \
    xargs -r kubectl delete --wait=false 2>/dev/null || true
  kubectl delete clusterrolebinding "${NS_PREFIX}-runner" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

NODE_IP=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "${KIND_CLUSTER}-control-plane")

find_free_port() {
  python3 -c "import socket; s=socket.socket(); s.bind(('',0)); print(s.getsockname()[1]); s.close()"
}
find_free_node_port() {
  docker exec "${KIND_CLUSTER}-control-plane" \
    python3 -c "import socket; s=socket.socket(); s.bind(('',0)); print(s.getsockname()[1]); s.close()"
}

BACKEND_PORT=$(find_free_port)
REGISTRY_NODE_PORT=$(find_free_node_port)
echo "Node: ${NODE_IP}  Backend: ${BACKEND_PORT}  Registry: ${REGISTRY_NODE_PORT}"

# Build seed images + agent-runner binaries (cached, worktree-scoped)
bash "${SCRIPT_DIR}/build-agent-images.sh"
WORKTREE="$(bash "${SCRIPT_DIR}/detect-worktree.sh")"
RUNNER_DIR="/tmp/platform-e2e/${WORKTREE}/agent-runner"

# Deploy services
echo "==> Deploying services"
REGISTRY_BACKEND_HOST="${PLATFORM_HOST}" \
  REGISTRY_BACKEND_PORT="${BACKEND_PORT}" \
  REGISTRY_NODE_PORT="${REGISTRY_NODE_PORT}" \
  bash "${SCRIPT_DIR}/deploy-services.sh" "${SVC_NS}"

# Discover NodePorts
PG_PORT=$(kubectl get svc -n "${SVC_NS}" postgres -o jsonpath='{.spec.ports[0].nodePort}')
VALKEY_PORT=$(kubectl get svc -n "${SVC_NS}" valkey -o jsonpath='{.spec.ports[0].nodePort}')
MINIO_PORT=$(kubectl get svc -n "${SVC_NS}" minio -o jsonpath='{.spec.ports[0].nodePort}')
echo "PG: ${NODE_IP}:${PG_PORT}  Valkey: ${NODE_IP}:${VALKEY_PORT}  MinIO: ${NODE_IP}:${MINIO_PORT}"

echo -n "Waiting for connectivity"
for i in $(seq 1 30); do
  if nc -z "$NODE_IP" "$PG_PORT" 2>/dev/null && \
     nc -z "$NODE_IP" "$VALKEY_PORT" 2>/dev/null && \
     nc -z "$NODE_IP" "$MINIO_PORT" 2>/dev/null; then break; fi
  echo -n "."; sleep 0.5
done
echo " ready"

# RBAC
kubectl create namespace "${PIPELINE_NS}" --dry-run=client -o yaml | kubectl apply -f -
kubectl create namespace "${AGENT_NS}" --dry-run=client -o yaml | kubectl apply -f -
kubectl apply -f "${SCRIPT_DIR}/test-manifests/rbac.yaml"
kubectl create serviceaccount test-runner -n "${SVC_NS}" 2>/dev/null || true
kubectl create clusterrolebinding "${NS_PREFIX}-runner" \
  --clusterrole=test-runner \
  --serviceaccount="${SVC_NS}:test-runner" 2>/dev/null || true

# Copy mock CLIs for worker pods (pod init containers use these)
mkdir -p /tmp/platform-e2e
cp "${PROJECT_DIR}/tests/fixtures/mock-claude-cli.sh" "/tmp/platform-e2e/mock-claude-cli.sh"
chmod +x "/tmp/platform-e2e/mock-claude-cli.sh"
cp "${PROJECT_DIR}/tests/fixtures/mock-claude-cli-git.sh" "/tmp/platform-e2e/mock-claude-cli-git.sh"
chmod +x "/tmp/platform-e2e/mock-claude-cli-git.sh"

# Env vars for the test binary
export DATABASE_URL="postgres://platform:dev@${NODE_IP}:${PG_PORT}/platform_dev"
export VALKEY_URL="redis://${NODE_IP}:${VALKEY_PORT}"
export MINIO_ENDPOINT="http://${NODE_IP}:${MINIO_PORT}"
export MINIO_ACCESS_KEY="platform"
export MINIO_SECRET_KEY="devdevdev"
export PLATFORM_MASTER_KEY="0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
export PLATFORM_DEV=true
export RUST_LOG="info"
export PLATFORM_NS_PREFIX="${NS_PREFIX}"
export PLATFORM_LISTEN_PORT="${BACKEND_PORT}"
export PLATFORM_REGISTRY_URL="${PLATFORM_HOST}:${BACKEND_PORT}"
export PLATFORM_REGISTRY_NODE_URL="localhost:${REGISTRY_NODE_PORT}"
export PLATFORM_API_URL="http://${PLATFORM_HOST}:${BACKEND_PORT}"
export PLATFORM_PIPELINE_NAMESPACE="${PIPELINE_NS}"
export PLATFORM_AGENT_NAMESPACE="${AGENT_NS}"
export PLATFORM_VALKEY_AGENT_HOST="valkey.${SVC_NS}.svc.cluster.local:6379"
export PLATFORM_SEED_IMAGES_PATH="/tmp/platform-e2e/seed-images"
export PLATFORM_AGENT_RUNNER_DIR="/tmp/platform-e2e/agent-runner"
export PLATFORM_HOST_MOUNT_PATH="/tmp/platform-e2e"
export SQLX_OFFLINE=true

# CRITICAL: Unset CLAUDE_CLI_PATH so the real claude CLI is used by the manager subprocess.
# The manager spawns `claude -p` as a local subprocess (not in a pod).
unset CLAUDE_CLI_PATH

# Read OAuth token from .env.test
CLAUDE_OAUTH_TOKEN=$(grep '^CLAUDE_OAUTH_TOKEN=' "${PROJECT_DIR}/.env.test" | cut -d= -f2- || true)
if [[ -z "${CLAUDE_OAUTH_TOKEN:-}" ]]; then
  echo "ERROR: CLAUDE_OAUTH_TOKEN not found in .env.test"
  exit 1
fi
export CLAUDE_OAUTH_TOKEN
echo "CLAUDE_OAUTH_TOKEN: set (${#CLAUDE_OAUTH_TOKEN} chars)"

echo ""
echo "==> Running LLM E2E test"
echo "════════════════════════════════════════════════════════════════"

cd "${PROJECT_DIR}"
cargo nextest run \
  --test llm_create_app_e2e \
  -E 'test(llm_create_app_full_flow)' \
  --run-ignored ignored-only \
  --test-threads 1 \
  --no-fail-fast \
  --success-output immediate \
  --failure-output immediate \
  2>&1

echo ""
echo "==> Done"
