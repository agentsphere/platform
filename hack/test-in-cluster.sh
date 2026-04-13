#!/usr/bin/env bash
# test-in-cluster.sh — Run integration/E2E tests against ephemeral cluster services
#
# Creates isolated namespaces (platform-test-{id}-*), deploys PG + Valkey + MinIO
# as NodePort services, deploys a DaemonSet registry proxy, then connects
# directly to the cluster node IP (Kind makes it routable from macOS).
# Runs tests natively with cargo nextest.
#
# Usage:
#   bash hack/test-in-cluster.sh                          # integration tests
#   bash hack/test-in-cluster.sh --filter '*_integration' # specific filter
#   bash hack/test-in-cluster.sh --type e2e               # E2E tests
#   bash hack/test-in-cluster.sh --type total              # all tiers with coverage
#   bash hack/test-in-cluster.sh --threads 4              # custom parallelism
#   bash hack/test-in-cluster.sh --coverage               # with coverage instrumentation
#   bash hack/test-in-cluster.sh --coverage --lcov out.lcov  # coverage → LCOV file

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_DIR"

# ── Defaults ──────────────────────────────────────────────────────────────
TEST_FILTER="*_integration"
TEST_TYPE="integration"   # "integration", "e2e", or "total"
TEST_THREADS=""
FILTER_EXPR=""


# Coverage options
COVERAGE_MODE=false
LCOV_PATH=""
COV_NO_REPORT=false
COV_CLEAN=false

# ── Parse arguments ───────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
  case "$1" in
    --filter)       TEST_FILTER="$2"; shift 2 ;;
    --type)         TEST_TYPE="$2"; shift 2 ;;
    --threads)      TEST_THREADS="$2"; shift 2 ;;
    --coverage)     COVERAGE_MODE=true; shift ;;
    --lcov)         LCOV_PATH="$2"; shift 2 ;;
    --cov-no-report) COV_NO_REPORT=true; COVERAGE_MODE=true; shift ;;
    --cov-clean)    COV_CLEAN=true; shift ;;
    --expr)         FILTER_EXPR="$2"; shift 2 ;;
    *)              echo "Unknown arg: $1"; exit 1 ;;
  esac
done

# --type total implies coverage mode
if [[ "$TEST_TYPE" == "total" ]]; then
  COVERAGE_MODE=true
  COV_CLEAN=true
fi

# For E2E, override default filter (not for total — tiers are run separately)
if [[ "$TEST_TYPE" == "e2e" && "$TEST_FILTER" == "*_integration" ]]; then
  TEST_FILTER="e2e_*"
fi

# ── Namespace ID ──────────────────────────────────────────────────────────
RUN_ID=$(openssl rand -hex 4)
NS_PREFIX="platform-test-${RUN_ID}"
SVC_NS="${NS_PREFIX}-services"
PIPELINE_NS="${NS_PREFIX}-pipelines"
AGENT_NS="${NS_PREFIX}-agents"

# ── Cleanup on exit ───────────────────────────────────────────────────────
cleanup() {
  echo ""
  echo "==> Cleaning up"

  # Delete all namespaces with our prefix
  echo "  Deleting namespaces: ${NS_PREFIX}-*"
  kubectl get namespaces -o name | grep "^namespace/${NS_PREFIX}" | \
    xargs -r kubectl delete --wait=false 2>/dev/null || true
  kubectl delete clusterrolebinding "${NS_PREFIX}-runner" 2>/dev/null || true

  # DaemonSet cleanup happens automatically with namespace deletion

  # Clean up seed cache/lock files scoped to this run
  local wt
  wt="$(bash "${SCRIPT_DIR}/detect-worktree.sh" 2>/dev/null || echo main)"
  find "/tmp/platform-e2e/${wt}/seed-images" -name "*.${NS_PREFIX}.seed-cache.*" -delete 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# ── Pre-flight checks / cluster detection ───────────────────────────────
echo "==> Pre-flight checks"
source "${SCRIPT_DIR}/cluster-info.sh"

echo "  Cluster:   ${NODE_CONTAINER}"
echo "  NS prefix: ${NS_PREFIX}"
echo "  Test type: ${TEST_TYPE}"
echo "  Test filter: ${TEST_FILTER}"
echo "  Node IP:   ${NODE_IP}"

# ── Find free ports (backend + registry only — PG/Valkey/MinIO use NodePort) ──
find_free_port() {
  python3 -c "import socket; s=socket.socket(); s.bind(('',0)); print(s.getsockname()[1]); s.close()"
}

BACKEND_PORT=$(find_free_port)
REGISTRY_NODE_PORT=$(find_free_node_port)
GATEWAY_HTTP_NODE_PORT=$(find_free_node_port)
GATEWAY_TLS_NODE_PORT=$(find_free_node_port)

echo ""
echo "==> Local ports"
echo "  Backend:      ${BACKEND_PORT}"
echo "  Registry:     ${REGISTRY_NODE_PORT} (node hostPort)"
echo "  Gateway HTTP: ${GATEWAY_HTTP_NODE_PORT} (node hostPort)"
echo "  Gateway TLS:  ${GATEWAY_TLS_NODE_PORT} (node hostPort)"

# ── Build seed images + agent-runner binaries (cached, worktree-scoped) ───
bash "${SCRIPT_DIR}/build-agent-images.sh"
WORKTREE="$(bash "${SCRIPT_DIR}/detect-worktree.sh")"
RUNNER_DIR="/tmp/platform-e2e/${WORKTREE}/agent-runner"

# ── Deploy services + registry proxy using shared script ──────────────────
REGISTRY_BACKEND_HOST="${PLATFORM_HOST}" \
  REGISTRY_BACKEND_PORT="${BACKEND_PORT}" \
  REGISTRY_NODE_PORT="${REGISTRY_NODE_PORT}" \
  bash "${SCRIPT_DIR}/deploy-services.sh" "${SVC_NS}"

# ── Discover NodePorts (K8s auto-assigned) ──────────────────────────────
echo ""
echo "==> Discovering NodePorts"
PG_PORT=$(kubectl get svc -n "${SVC_NS}" postgres -o jsonpath='{.spec.ports[0].nodePort}')
VALKEY_PORT=$(kubectl get svc -n "${SVC_NS}" valkey -o jsonpath='{.spec.ports[0].nodePort}')
MINIO_PORT=$(kubectl get svc -n "${SVC_NS}" minio -o jsonpath='{.spec.ports[0].nodePort}')
PREVIEW_PROXY_PORT=$(kubectl get svc -n "${SVC_NS}" preview-proxy -o jsonpath='{.spec.ports[0].nodePort}')
echo "  Postgres:      ${NODE_IP}:${PG_PORT}"
echo "  Valkey:        ${NODE_IP}:${VALKEY_PORT}"
echo "  MinIO:         ${NODE_IP}:${MINIO_PORT}"
echo "  Preview proxy: ${NODE_IP}:${PREVIEW_PROXY_PORT}"

# Wait for NodePort connectivity (direct to cluster node — no port-forward)
echo -n "  Waiting for NodePort connectivity"
for i in $(seq 1 60); do
  if nc -z "$NODE_IP" "$PG_PORT" 2>/dev/null && \
     nc -z "$NODE_IP" "$VALKEY_PORT" 2>/dev/null && \
     nc -z "$NODE_IP" "$MINIO_PORT" 2>/dev/null; then
    break
  fi
  echo -n "."
  sleep 0.5
done
if ! nc -z "$NODE_IP" "$PG_PORT" 2>/dev/null; then
  echo ""
  echo "ERROR: Could not connect to services after 15s"
  exit 1
fi
echo " ready"

# ── RBAC for all test types ───────────────────────────────────────────────
echo ""
echo "==> Setting up RBAC"

# Create pipeline/agent namespaces
kubectl create namespace "${PIPELINE_NS}" --dry-run=client -o yaml | kubectl apply -f -
kubectl create namespace "${AGENT_NS}" --dry-run=client -o yaml | kubectl apply -f -

# Apply the ClusterRole (idempotent)
kubectl apply -f "${SCRIPT_DIR}/test-manifests/rbac.yaml"

# Create ServiceAccount in services namespace
kubectl create serviceaccount test-runner -n "${SVC_NS}" 2>/dev/null || true

# Bind ClusterRole to the ServiceAccount
kubectl create clusterrolebinding "${NS_PREFIX}-runner" \
  --clusterrole=test-runner \
  --serviceaccount="${SVC_NS}:test-runner" 2>/dev/null || true

# Gateway is auto-deployed by the platform binary (PLATFORM_GATEWAY_AUTO_DEPLOY=true)
# No manual EnvoyProxy/Gateway CRD creation needed

# ── Run tests ─────────────────────────────────────────────────────────────
echo ""
echo "==> Running tests"
echo "────────────────────────────────────────────────────────────────"

# Build env vars — DATABASE_URL is set after the connectivity check above
# so sqlx::query!() macros can validate queries against the real DB at compile time.
export DATABASE_URL="postgres://platform:dev@${NODE_IP}:${PG_PORT}/platform_dev?sslmode=require"

# Run migrations so the DB schema is up-to-date for sqlx compile-time validation
echo "  Running migrations..."
sqlx migrate run --source "${PROJECT_DIR}/migrations" 2>/dev/null || {
  # If sqlx-cli isn't installed, fall back to offline mode
  echo "  sqlx-cli not found, using offline cache"
  export SQLX_OFFLINE=true
}
export VALKEY_URL="redis://:dev@${NODE_IP}:${VALKEY_PORT}"
export MINIO_ENDPOINT="https://${NODE_IP}:${MINIO_PORT}"
export MINIO_ACCESS_KEY="platform"
export MINIO_SECRET_KEY="devdevdev"
export MINIO_INSECURE=true  # S55: accept self-signed TLS cert in dev/test
export REGISTRY_PROXY_BLOBS=true  # Stream blobs through platform (MinIO NodePort unreachable from pods)
export PLATFORM_MASTER_KEY="0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
export PLATFORM_MESH_ENABLED=true
export PLATFORM_DEV=true
export RUST_LOG="${RUST_LOG:-platform=debug}"
export PLATFORM_NS_PREFIX="${NS_PREFIX}"
export PLATFORM_NAMESPACE="${SVC_NS}"
export PLATFORM_LISTEN_PORT="${BACKEND_PORT}"
export PLATFORM_REGISTRY_URL="${PLATFORM_HOST}:${BACKEND_PORT}"
export PLATFORM_REGISTRY_NODE_URL="localhost:${REGISTRY_NODE_PORT}"
export PLATFORM_API_URL="http://${PLATFORM_HOST}:${BACKEND_PORT}"
export PLATFORM_PIPELINE_NAMESPACE="${PIPELINE_NS}"
export PLATFORM_AGENT_NAMESPACE="${AGENT_NS}"
export PLATFORM_GATEWAY_NAMESPACE="${PLATFORM_GATEWAY_NAMESPACE:-${SVC_NS}}"
export PLATFORM_GATEWAY_AUTO_DEPLOY=true
export PLATFORM_GATEWAY_HTTP_NODE_PORT="${GATEWAY_HTTP_NODE_PORT}"
export PLATFORM_GATEWAY_TLS_NODE_PORT="${GATEWAY_TLS_NODE_PORT}"
export PLATFORM_GATEWAY_WATCH_NAMESPACES="${SVC_NS},${PIPELINE_NS},${AGENT_NS}"
export PLATFORM_VALKEY_AGENT_HOST="valkey.${SVC_NS}.svc.cluster.local:6379"
export PLATFORM_PREVIEW_PROXY_URL="http://${NODE_IP}:${PREVIEW_PROXY_PORT}"
export PLATFORM_SEED_IMAGES_PATH="/tmp/platform-e2e/${WORKTREE}/seed-images"
export PLATFORM_AGENT_RUNNER_DIR="${RUNNER_DIR}"
export PLATFORM_PROXY_PATH="/tmp/platform-e2e/${WORKTREE}/proxy"
export PLATFORM_MCP_SERVERS_TARBALL="/tmp/platform-e2e/${WORKTREE}/mcp-servers.tar.gz"
export CLAUDE_CLI_PATH="${PROJECT_DIR}/tests/fixtures/claude-mock/claude"

# Copy mock CLIs to shared mount so they're accessible inside cluster pods
cp "${PROJECT_DIR}/tests/fixtures/claude-mock/claude" "/tmp/platform-e2e/mock-claude-cli.sh"
chmod +x "/tmp/platform-e2e/mock-claude-cli.sh"
cp "${PROJECT_DIR}/tests/fixtures/mock-claude-cli-git.sh" "/tmp/platform-e2e/mock-claude-cli-git.sh"
chmod +x "/tmp/platform-e2e/mock-claude-cli-git.sh"
export PLATFORM_HOST_MOUNT_PATH="/tmp/platform-e2e"
# Override CLAUDE_CLI_PATH for pod-accessible path (hostPath mount)
export CLAUDE_CLI_PATH="/tmp/platform-e2e/mock-claude-cli.sh"

# Test output files — grouped by RUN_ID prefix for easy discovery
REPORT_FILE="${PROJECT_DIR}/test-${RUN_ID}-report.txt"
OUTPUT_FILE="${PROJECT_DIR}/test-${RUN_ID}-output.txt"
LOG_FILE="${PROJECT_DIR}/test-${RUN_ID}-logs.jsonl"
JUNIT_FILE="${PROJECT_DIR}/target/nextest/ci/junit.xml"

# Structured JSON logs with threadName per line (consumed by helpers::init_test_tracing)
export TEST_LOG_FILE="${LOG_FILE}"

# ── Coverage: clean previous data ────────────────────────────────────────
if $COV_CLEAN; then
  echo "==> Cleaning previous coverage data"
  cargo llvm-cov clean --workspace
fi

# ── Filename regex for coverage exclusions ────────────────────────────────
COV_IGNORE_REGEX='(proto\.rs|ui\.rs)'
COV_REPORT_IGNORE_REGEX='(proto\.rs|ui\.rs|main\.rs)'

# ── Run tests ─────────────────────────────────────────────────────────────
if [[ "$TEST_TYPE" == "total" ]]; then
  # Combined coverage: unit + integration (no E2E — use cov-all for that)
  # Track failures but continue through all tiers to generate the report
  TIER_FAILURES=0

  echo ""
  echo "==> Running unit tests (coverage, no report)"
  cargo llvm-cov nextest --no-report --lib \
    --ignore-filename-regex "${COV_IGNORE_REGEX}" \
    2>&1 | tee -a "${OUTPUT_FILE}" \
    || TIER_FAILURES=$((TIER_FAILURES + 1))

  echo ""
  echo "==> Running integration tests (coverage, no report)"
  cargo llvm-cov nextest --no-report --test '*_integration' \
    --profile ci --test-threads 32 \
    --ignore-filename-regex "${COV_IGNORE_REGEX}" --no-fail-fast \
    2>&1 | tee -a "${OUTPUT_FILE}" \
    || TIER_FAILURES=$((TIER_FAILURES + 1))

  COV_REPORT_FILE="${PROJECT_DIR}/test-${RUN_ID}-coverage.txt"

  echo ""
  echo "==> Generating combined coverage report"
  echo "────────────────────────────────────────────────────────────────"
  cargo llvm-cov report --ignore-filename-regex "${COV_REPORT_IGNORE_REGEX}" \
    2>&1 | tee "${COV_REPORT_FILE}"

  if [[ -n "$LCOV_PATH" ]]; then
    echo ""
    echo "==> Generating combined LCOV → ${LCOV_PATH}"
    cargo llvm-cov report --lcov --output-path "${LCOV_PATH}" \
      --ignore-filename-regex "${COV_REPORT_IGNORE_REGEX}"
  fi

  if [[ $TIER_FAILURES -gt 0 ]]; then
    echo ""
    echo "WARNING: ${TIER_FAILURES} test tier(s) had failures (see above)"
    TEST_EXIT=1
  else
    TEST_EXIT=0
  fi
else
  # Single tier run
  NEXTEST_ARGS=(--test "${TEST_FILTER}")

  if [[ -n "$FILTER_EXPR" ]]; then
    NEXTEST_ARGS+=(-E "${FILTER_EXPR}")
  fi

  if [[ -n "$TEST_THREADS" ]]; then
    NEXTEST_ARGS+=(--test-threads "${TEST_THREADS}")
  elif [[ "$TEST_TYPE" == "e2e" ]]; then
    NEXTEST_ARGS+=(--test-threads 2)
  elif [[ "$TEST_TYPE" == "integration" ]]; then
    # Each #[sqlx::test] creates a fresh DB + pool (~10 connections).
    # Postgres max_connections=300 → safe up to ~30 concurrent tests.
    NEXTEST_ARGS+=(--test-threads 32)
  fi

  if [[ "$TEST_TYPE" == "e2e" ]]; then
    NEXTEST_ARGS+=(--run-ignored ignored-only)
  fi

  # Use CI profile for retries on transient failures (e.g. DB connection resets)
  NEXTEST_ARGS+=(--profile ci --no-fail-fast)

  TEST_EXIT=0
  if $COVERAGE_MODE; then
    COV_ARGS=(--ignore-filename-regex "${COV_IGNORE_REGEX}")
    if $COV_NO_REPORT; then
      COV_ARGS+=(--no-report)
    elif [[ -n "$LCOV_PATH" ]]; then
      COV_ARGS+=(--lcov --output-path "${LCOV_PATH}")
    fi
    cargo llvm-cov nextest "${COV_ARGS[@]}" "${NEXTEST_ARGS[@]}" 2>&1 | tee "${OUTPUT_FILE}" || TEST_EXIT=$?
  else
    cargo nextest run "${NEXTEST_ARGS[@]}" 2>&1 | tee "${OUTPUT_FILE}" || TEST_EXIT=$?
  fi
fi

# ── Generate test report ──────────────────────────────────────────────────
bash "${SCRIPT_DIR}/generate-test-report.sh" "${JUNIT_FILE}" "${REPORT_FILE}" || true

echo ""
echo "==> Test outputs (RUN_ID=${RUN_ID}):"
echo "    Report: ${REPORT_FILE}"
[[ -f "${OUTPUT_FILE}" ]] && echo "    Output: ${OUTPUT_FILE}"
[[ -f "${LOG_FILE}" ]] && echo "    Logs:   ${LOG_FILE}"
[[ -n "${COV_REPORT_FILE:-}" && -f "${COV_REPORT_FILE}" ]] && echo "    Coverage: ${COV_REPORT_FILE}"
exit "${TEST_EXIT:-0}"
