#!/usr/bin/env bash
# test-in-cluster.sh — Run integration/E2E tests against dev cluster services
#
# Reuses the long-lived dev namespace (platform-dev-*) for PG, Valkey, MinIO
# (sourced from .env.dev). Creates only ephemeral pipeline/agent namespaces
# per test run. Runs tests natively with cargo nextest.
#
# Usage:
#   bash hack/test-in-cluster.sh                          # API handler tests (default)
#   bash hack/test-in-cluster.sh --type api               # API handler tests
#   bash hack/test-in-cluster.sh --type integration       # library integration tests
#   bash hack/test-in-cluster.sh --type contract          # contract tests
#   bash hack/test-in-cluster.sh --type e2e               # E2E tests
#   bash hack/test-in-cluster.sh --type total             # all tiers with coverage
#   bash hack/test-in-cluster.sh --filter '*_api'         # specific filter
#   bash hack/test-in-cluster.sh --threads 4              # custom parallelism
#   bash hack/test-in-cluster.sh --coverage               # with coverage instrumentation
#   bash hack/test-in-cluster.sh --coverage --lcov out.lcov  # coverage → LCOV file

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_DIR"

# ── Defaults ──────────────────────────────────────────────────────────────
TEST_FILTER=""
TEST_TYPE="api"   # "integration", "api", "contract", "e2e", or "total"
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

# Set default TEST_FILTER per type (unless --filter was explicitly given)
if [[ -z "$TEST_FILTER" ]]; then
  case "$TEST_TYPE" in
    integration) TEST_FILTER="*_integration" ;;
    api)         TEST_FILTER="*_api" ;;
    contract)    TEST_FILTER="*_contract" ;;
    e2e)         TEST_FILTER="e2e_*" ;;
    total)       TEST_FILTER="" ;;  # handled separately below
    *)           echo "Unknown test type: $TEST_TYPE"; exit 1 ;;
  esac
fi

# ── Namespace ID ──────────────────────────────────────────────────────────
RUN_ID=$(openssl rand -hex 4)
NS_PREFIX="platform-test-${RUN_ID}"
PIPELINE_NS="${NS_PREFIX}-pipelines"
AGENT_NS="${NS_PREFIX}-agents"

# ── Source dev environment (reuse long-lived PG/Valkey/MinIO) ────────────
if [[ ! -f "${PROJECT_DIR}/.env.dev" ]]; then
  echo "ERROR: .env.dev not found. Run 'just dev-up' first."
  exit 1
fi
# shellcheck disable=SC1091
source "${PROJECT_DIR}/.env.dev"

# Extract the dev namespace from .env.dev (e.g. platform-dev-main)
DEV_NS="${PLATFORM_NAMESPACE:-platform-dev-main}"

# ── Cleanup on exit ───────────────────────────────────────────────────────
cleanup() {
  echo ""
  echo "==> Cleaning up"

  # Delete only the ephemeral pipeline/agent namespaces (NOT the dev services)
  echo "  Deleting namespaces: ${NS_PREFIX}-*"
  kubectl get namespaces -o name | grep "^namespace/${NS_PREFIX}" | \
    xargs -r kubectl delete --wait=false 2>/dev/null || true
  kubectl delete clusterrolebinding "${NS_PREFIX}-runner" 2>/dev/null || true

  # Delete the per-run registry proxy DaemonSet
  kubectl delete daemonset "registry-proxy-${RUN_ID}" -n "${DEV_NS}" 2>/dev/null || true

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
echo "  Dev NS:    ${DEV_NS}"
echo "  NS prefix: ${NS_PREFIX}"
echo "  Test type: ${TEST_TYPE}"
echo "  Test filter: ${TEST_FILTER}"
echo "  Node IP:   ${NODE_IP}"

# ── Find free ports (backend + registry + gateway — services reuse dev NS) ──
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

# ── Reuse dev namespace services (PG/Valkey/MinIO already running) ────────
# No deploy-services.sh call — .env.dev already has connection strings.
# Just verify connectivity to the dev services.
echo ""
echo "==> Verifying dev namespace services (${DEV_NS})"
echo "  DATABASE_URL: ${DATABASE_URL}"
echo "  VALKEY_URL:   ${VALKEY_URL}"
echo "  MINIO:        ${MINIO_ENDPOINT}"

# Extract host:port from DATABASE_URL for connectivity check
DB_HOST_PORT=$(echo "$DATABASE_URL" | sed -E 's|.*@([^/]+)/.*|\1|')
DB_HOST=$(echo "$DB_HOST_PORT" | cut -d: -f1)
DB_PORT=$(echo "$DB_HOST_PORT" | cut -d: -f2)
VALKEY_HOST_PORT=$(echo "$VALKEY_URL" | sed -E 's|.*@([^/]*).*|\1|')
VK_HOST=$(echo "$VALKEY_HOST_PORT" | cut -d: -f1)
VK_PORT=$(echo "$VALKEY_HOST_PORT" | cut -d: -f2)
MINIO_HOST_PORT=$(echo "$MINIO_ENDPOINT" | sed -E 's|https?://||')
MN_HOST=$(echo "$MINIO_HOST_PORT" | cut -d: -f1)
MN_PORT=$(echo "$MINIO_HOST_PORT" | cut -d: -f2)

echo -n "  Checking connectivity"
for i in $(seq 1 30); do
  if nc -z "$DB_HOST" "$DB_PORT" 2>/dev/null && \
     nc -z "$VK_HOST" "$VK_PORT" 2>/dev/null && \
     nc -z "$MN_HOST" "$MN_PORT" 2>/dev/null; then
    break
  fi
  echo -n "."
  sleep 0.5
done
if ! nc -z "$DB_HOST" "$DB_PORT" 2>/dev/null; then
  echo ""
  echo "ERROR: Could not connect to dev services. Run 'just dev-up' first."
  exit 1
fi
echo " ready"

# Deploy a per-run registry proxy DaemonSet (separate from the dev one).
# Each test run has its own backend port, so it needs its own proxy.
REGISTRY_PROXY_NAME="registry-proxy-${RUN_ID}"
echo "==> Deploying registry proxy DaemonSet: ${REGISTRY_PROXY_NAME} (port ${REGISTRY_NODE_PORT} → ${PLATFORM_HOST}:${BACKEND_PORT})"
cat <<DAEMONSET | kubectl apply -n "${DEV_NS}" -f -
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: ${REGISTRY_PROXY_NAME}
  labels:
    app: ${REGISTRY_PROXY_NAME}
spec:
  selector:
    matchLabels:
      app: ${REGISTRY_PROXY_NAME}
  template:
    metadata:
      labels:
        app: ${REGISTRY_PROXY_NAME}
    spec:
      tolerations:
        - operator: Exists
      containers:
        - name: socat
          image: alpine/socat:1.8.0.1
          args:
            - "TCP-LISTEN:${REGISTRY_NODE_PORT},fork,reuseaddr"
            - "TCP:${PLATFORM_HOST}:${BACKEND_PORT}"
          ports:
            - containerPort: ${REGISTRY_NODE_PORT}
              hostPort: ${REGISTRY_NODE_PORT}
              protocol: TCP
          resources:
            requests:
              cpu: 10m
              memory: 16Mi
            limits:
              memory: 32Mi
DAEMONSET
kubectl rollout status -n "${DEV_NS}" daemonset/"${REGISTRY_PROXY_NAME}" --timeout=30s 2>/dev/null || true

# Discover preview-proxy port from dev namespace
PREVIEW_PROXY_PORT=$(kubectl get svc -n "${DEV_NS}" preview-proxy -o jsonpath='{.spec.ports[0].nodePort}' 2>/dev/null || echo "")

# ── RBAC for all test types ───────────────────────────────────────────────
echo ""
echo "==> Setting up RBAC"

# Create pipeline/agent namespaces
kubectl create namespace "${PIPELINE_NS}" --dry-run=client -o yaml | kubectl apply -f -
kubectl create namespace "${AGENT_NS}" --dry-run=client -o yaml | kubectl apply -f -

# Apply the ClusterRole (idempotent)
kubectl apply -f "${SCRIPT_DIR}/test-manifests/rbac.yaml"

# Create ServiceAccount in dev namespace (reuse existing if present)
kubectl create serviceaccount test-runner -n "${DEV_NS}" 2>/dev/null || true

# Bind ClusterRole to the ServiceAccount
kubectl create clusterrolebinding "${NS_PREFIX}-runner" \
  --clusterrole=test-runner \
  --serviceaccount="${DEV_NS}:test-runner" 2>/dev/null || true

# Gateway is auto-deployed by the platform binary (PLATFORM_GATEWAY_AUTO_DEPLOY=true)
# No manual EnvoyProxy/Gateway CRD creation needed

# ── Run tests ─────────────────────────────────────────────────────────────
echo ""
echo "==> Running tests"
echo "────────────────────────────────────────────────────────────────"

# Build env vars — DATABASE_URL, VALKEY_URL, MINIO_* already set from .env.dev.
# Only override test-run-specific vars.

# Run migrations so the DB schema is up-to-date for sqlx compile-time validation
echo "  Running migrations..."
sqlx migrate run --source "${PROJECT_DIR}/migrations" 2>/dev/null || {
  # If sqlx-cli isn't installed, fall back to offline mode
  echo "  sqlx-cli not found, using offline cache"
  export SQLX_OFFLINE=true
}

# .env.dev already exports: DATABASE_URL, VALKEY_URL, MINIO_ENDPOINT,
# MINIO_ACCESS_KEY, MINIO_SECRET_KEY, MINIO_INSECURE, PLATFORM_MASTER_KEY,
# PLATFORM_DEV, PLATFORM_MESH_ENABLED
export REGISTRY_PROXY_BLOBS=true  # Stream blobs through platform (MinIO NodePort unreachable from pods)
export RUST_LOG="${RUST_LOG:-platform=debug}"

# Override namespace-related vars for this test run
export PLATFORM_NS_PREFIX="${NS_PREFIX}"
export PLATFORM_NAMESPACE="${DEV_NS}"
export PLATFORM_LISTEN_PORT="${BACKEND_PORT}"
export PLATFORM_REGISTRY_URL="${PLATFORM_HOST}:${BACKEND_PORT}"
export PLATFORM_REGISTRY_NODE_URL="localhost:${REGISTRY_NODE_PORT}"
export PLATFORM_API_URL="http://${PLATFORM_HOST}:${BACKEND_PORT}"
export PLATFORM_PIPELINE_NAMESPACE="${PIPELINE_NS}"
export PLATFORM_AGENT_NAMESPACE="${AGENT_NS}"
export PLATFORM_GATEWAY_NAMESPACE="${DEV_NS}"
export PLATFORM_GATEWAY_AUTO_DEPLOY=true
export PLATFORM_GATEWAY_HTTP_NODE_PORT="${GATEWAY_HTTP_NODE_PORT}"
export PLATFORM_GATEWAY_TLS_NODE_PORT="${GATEWAY_TLS_NODE_PORT}"
export PLATFORM_GATEWAY_WATCH_NAMESPACES="${DEV_NS},${PIPELINE_NS},${AGENT_NS}"
export PLATFORM_VALKEY_AGENT_HOST="valkey.${DEV_NS}.svc.cluster.local:6379"
if [[ -n "${PREVIEW_PROXY_PORT}" ]]; then
  export PLATFORM_PREVIEW_PROXY_URL="http://${NODE_IP}:${PREVIEW_PROXY_PORT}"
fi
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
JUNIT_FILE="${PROJECT_DIR}/target/nextest/integration/junit.xml"

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
  # Combined coverage: unit + api + contract + integration
  # Track failures but continue through all tiers to generate the report
  TIER_FAILURES=0

  echo ""
  echo "==> Running unit tests (coverage, no report)"
  cargo llvm-cov nextest --no-report --lib \
    --ignore-filename-regex "${COV_IGNORE_REGEX}" \
    2>&1 | tee -a "${OUTPUT_FILE}" \
    || TIER_FAILURES=$((TIER_FAILURES + 1))

  echo ""
  echo "==> Running API tests (coverage, no report)"
  cargo llvm-cov nextest --no-report --test '*_api' \
    --profile api \
    --ignore-filename-regex "${COV_IGNORE_REGEX}" --no-fail-fast \
    2>&1 | tee -a "${OUTPUT_FILE}" \
    || TIER_FAILURES=$((TIER_FAILURES + 1))

  echo ""
  echo "==> Running contract tests (coverage, no report)"
  cargo llvm-cov nextest --no-report --test '*_contract' \
    --profile contract \
    --ignore-filename-regex "${COV_IGNORE_REGEX}" --no-fail-fast \
    2>&1 | tee -a "${OUTPUT_FILE}" \
    || TIER_FAILURES=$((TIER_FAILURES + 1))

  echo ""
  echo "==> Running integration tests (coverage, no report)"
  cargo llvm-cov nextest --no-report --test '*_integration' \
    --profile integration \
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

  # Thread counts baked into nextest profiles (integration=32, api=32, e2e=2, k8s=4).
  # Only override if --threads explicitly passed on CLI.
  if [[ -n "$TEST_THREADS" ]]; then
    NEXTEST_ARGS+=(--test-threads "${TEST_THREADS}")
  fi

  if [[ "$TEST_TYPE" == "e2e" ]]; then
    NEXTEST_ARGS+=(--run-ignored ignored-only)
  fi

  # Use the matching nextest profile for each test type
  case "$TEST_TYPE" in
    integration) NEXTEST_ARGS+=(--profile integration) ;;
    api)         NEXTEST_ARGS+=(--profile api) ;;
    contract)    NEXTEST_ARGS+=(--profile contract) ;;
    e2e)         NEXTEST_ARGS+=(--profile e2e --no-fail-fast) ;;
    *)           NEXTEST_ARGS+=(--profile default --no-fail-fast) ;;
  esac

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
