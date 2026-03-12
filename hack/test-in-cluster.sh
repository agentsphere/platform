#!/usr/bin/env bash
# test-in-cluster.sh — Run integration/E2E tests against ephemeral Kind services
#
# Creates isolated namespaces (platform-test-{id}-*), deploys PG + Valkey + MinIO
# as NodePort services, deploys a DaemonSet registry proxy, then connects
# directly to the Kind node IP (OrbStack makes it routable from macOS).
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

# ── Defaults ──────────────────────────────────────────────────────────────
TEST_FILTER="*_integration"
TEST_TYPE="integration"   # "integration", "e2e", or "total"
TEST_THREADS=""
FILTER_EXPR=""
KIND_CLUSTER="platform"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

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

# Node IP for direct NodePort access (OrbStack makes Kind node IP-routable)
NODE_IP=""

# ── Detect host address ──────────────────────────────────────────────────
if [[ "$(uname)" == "Darwin" ]]; then
  PLATFORM_HOST="host.docker.internal"
else
  PLATFORM_HOST=$(docker network inspect kind \
    -f '{{range .IPAM.Config}}{{.Gateway}}{{end}}' 2>/dev/null || echo "172.18.0.1")
fi

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
  find "${SEED_DIR}" -name "*.${NS_PREFIX}.seed-cache.*" -delete 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# ── Pre-flight checks ────────────────────────────────────────────────────
echo "==> Pre-flight checks"

if ! command -v kind &>/dev/null; then
  echo "ERROR: 'kind' not found. Install: https://kind.sigs.k8s.io/"
  exit 1
fi

if ! kind get clusters 2>/dev/null | grep -q "^${KIND_CLUSTER}$"; then
  echo "ERROR: Kind cluster '${KIND_CLUSTER}' not found. Run: just cluster-up"
  exit 1
fi

export KUBECONFIG="${HOME}/.kube/kind-${KIND_CLUSTER}"
if [[ ! -f "$KUBECONFIG" ]]; then
  echo "ERROR: Kubeconfig not found at ${KUBECONFIG}"
  exit 1
fi

echo "  Kind cluster: ${KIND_CLUSTER}"
echo "  NS prefix:    ${NS_PREFIX}"
echo "  Test type:    ${TEST_TYPE}"
echo "  Test filter:  ${TEST_FILTER}"

# ── Get node IP (OrbStack makes Kind node directly routable) ────────────
NODE_IP=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "${KIND_CLUSTER}-control-plane")
if [[ -z "$NODE_IP" ]]; then
  echo "ERROR: Could not determine node IP for ${KIND_CLUSTER}-control-plane"
  exit 1
fi
echo "  Node IP:       ${NODE_IP}"

# ── Find free ports (backend + registry only — PG/Valkey/MinIO use NodePort) ──
find_free_port() {
  python3 -c "import socket; s=socket.socket(); s.bind(('',0)); print(s.getsockname()[1]); s.close()"
}

# Find a free port inside the Kind node (for hostPort bindings like registry proxy)
find_free_node_port() {
  docker exec "${KIND_CLUSTER}-control-plane" \
    python3 -c "import socket; s=socket.socket(); s.bind(('',0)); print(s.getsockname()[1]); s.close()"
}

BACKEND_PORT=$(find_free_port)
REGISTRY_NODE_PORT=$(find_free_node_port)

echo ""
echo "==> Local ports"
echo "  Backend:  ${BACKEND_PORT}"
echo "  Registry: ${REGISTRY_NODE_PORT} (node hostPort)"

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
echo "  Postgres: ${NODE_IP}:${PG_PORT}"
echo "  Valkey:   ${NODE_IP}:${VALKEY_PORT}"
echo "  MinIO:    ${NODE_IP}:${MINIO_PORT}"

# Wait for NodePort connectivity (direct to Kind node — no port-forward)
echo -n "  Waiting for NodePort connectivity"
for i in $(seq 1 30); do
  if nc -z "$NODE_IP" "$PG_PORT" 2>/dev/null && \
     nc -z "$NODE_IP" "$VALKEY_PORT" 2>/dev/null && \
     nc -z "$NODE_IP" "$MINIO_PORT" 2>/dev/null; then
    break
  fi
  echo -n "."
  sleep 0.5
done
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

# ── Run tests ─────────────────────────────────────────────────────────────
echo ""
echo "==> Running tests"
echo "────────────────────────────────────────────────────────────────"

# Build env vars
export DATABASE_URL="postgres://platform:dev@${NODE_IP}:${PG_PORT}/platform_dev"
export VALKEY_URL="redis://${NODE_IP}:${VALKEY_PORT}"
export MINIO_ENDPOINT="http://${NODE_IP}:${MINIO_PORT}"
export MINIO_ACCESS_KEY="platform"
export MINIO_SECRET_KEY="devdevdev"
export PLATFORM_MASTER_KEY="0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
export PLATFORM_DEV=true
export RUST_LOG="warn"
export PLATFORM_NS_PREFIX="${NS_PREFIX}"
export PLATFORM_LISTEN_PORT="${BACKEND_PORT}"
export PLATFORM_REGISTRY_URL="${PLATFORM_HOST}:${BACKEND_PORT}"
export PLATFORM_REGISTRY_NODE_URL="localhost:${REGISTRY_NODE_PORT}"
export PLATFORM_API_URL="http://${PLATFORM_HOST}:${BACKEND_PORT}"
export PLATFORM_PIPELINE_NAMESPACE="${PIPELINE_NS}"
export PLATFORM_AGENT_NAMESPACE="${AGENT_NS}"
export PLATFORM_VALKEY_AGENT_HOST="valkey.${SVC_NS}.svc.cluster.local:6379"
export PLATFORM_SEED_IMAGES_PATH="/tmp/platform-e2e/seed-images"
export PLATFORM_AGENT_RUNNER_DIR="${RUNNER_DIR}"
export CLAUDE_CLI_PATH="${PROJECT_DIR}/tests/fixtures/mock-claude-cli.sh"

# Copy mock CLIs to shared mount so they're accessible inside Kind pods
cp "${PROJECT_DIR}/tests/fixtures/mock-claude-cli.sh" "/tmp/platform-e2e/mock-claude-cli.sh"
chmod +x "/tmp/platform-e2e/mock-claude-cli.sh"
cp "${PROJECT_DIR}/tests/fixtures/mock-claude-cli-git.sh" "/tmp/platform-e2e/mock-claude-cli-git.sh"
chmod +x "/tmp/platform-e2e/mock-claude-cli-git.sh"
export PLATFORM_HOST_MOUNT_PATH="/tmp/platform-e2e"
# Override CLAUDE_CLI_PATH for pod-accessible path (hostPath mount)
export CLAUDE_CLI_PATH="/tmp/platform-e2e/mock-claude-cli.sh"

# Always use offline sqlx cache — it contains pre-computed types needed by
# sqlx::query! macros. Under coverage mode (--cfg=coverage), type inference
# breaks without the offline cache.
export SQLX_OFFLINE=true

# Test report file — written after tests complete
REPORT_FILE="${PROJECT_DIR}/test-report.txt"
JUNIT_FILE="${PROJECT_DIR}/target/nextest/ci/junit.xml"

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
  # Combined coverage: unit + integration + E2E in one instrumented build
  # Track failures but continue through all tiers to generate the report
  TIER_FAILURES=0

  echo ""
  echo "==> Running unit tests (coverage, no report)"
  cargo llvm-cov nextest --no-report --lib \
    --ignore-filename-regex "${COV_IGNORE_REGEX}" \
    || TIER_FAILURES=$((TIER_FAILURES + 1))

  echo ""
  echo "==> Running integration tests (coverage, no report)"
  cargo llvm-cov nextest --no-report --test '*_integration' \
    --profile ci --test-threads 32 \
    --ignore-filename-regex "${COV_IGNORE_REGEX}" --no-fail-fast \
    || TIER_FAILURES=$((TIER_FAILURES + 1))

  echo ""
  echo "==> Running E2E tests (coverage, no report)"
  cargo llvm-cov nextest --no-report --test 'e2e_*' \
    --profile ci --run-ignored ignored-only --test-threads 2 \
    --ignore-filename-regex "${COV_IGNORE_REGEX}" --no-fail-fast \
    || TIER_FAILURES=$((TIER_FAILURES + 1))

  echo ""
  echo "==> Generating combined coverage report"
  echo "────────────────────────────────────────────────────────────────"
  cargo llvm-cov report --ignore-filename-regex "${COV_REPORT_IGNORE_REGEX}"

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
    cargo llvm-cov nextest "${COV_ARGS[@]}" "${NEXTEST_ARGS[@]}" || TEST_EXIT=$?
  else
    cargo nextest run "${NEXTEST_ARGS[@]}" || TEST_EXIT=$?
  fi
fi

# ── Generate test report ──────────────────────────────────────────────────
generate_report() {
  local junit="$1" report="$2"
  if [[ ! -f "$junit" ]]; then
    echo "No JUnit XML found at ${junit}" > "$report"
    return
  fi

  # Parse JUnit XML with python3 (available on macOS + Kind node)
  python3 - "$junit" "$report" <<'PYEOF'
import sys, xml.etree.ElementTree as ET

junit_path, report_path = sys.argv[1], sys.argv[2]
tree = ET.parse(junit_path)
root = tree.getroot()

passed, failed, retried, errored = 0, 0, 0, 0
failures = []

for suite in root.iter("testsuite"):
    for case in suite.findall("testcase"):
        name = case.get("name", "?")
        classname = case.get("classname", "")
        failure = case.find("failure")
        error = case.find("error")
        rerun = case.find("flakyFailure") or case.find("rerunFailure")

        if failure is not None:
            failed += 1
            msg = failure.get("message", "")
            stderr = case.find("system-err")
            detail = ""
            if stderr is not None and stderr.text:
                # Extract panic message (most useful part)
                for line in stderr.text.splitlines():
                    if "panicked at" in line or "assertion" in line:
                        detail += line.strip() + "\n"
                if not detail:
                    # Fallback: first 5 lines of stderr
                    detail = "\n".join(stderr.text.strip().splitlines()[:5])
            failures.append((classname, name, msg, detail.strip()))
        elif error is not None:
            errored += 1
            failures.append((classname, name, error.get("message", "error"), ""))
        else:
            passed += 1

total = passed + failed + errored
status = "PASS" if failed == 0 and errored == 0 else "FAIL"

with open(report_path, "w") as f:
    f.write(f"Test Report: {status}\n")
    f.write(f"{'=' * 60}\n")
    f.write(f"Total: {total}  Passed: {passed}  Failed: {failed}  Errors: {errored}\n")
    f.write(f"\n")

    if failures:
        f.write(f"Failed Tests:\n")
        f.write(f"{'-' * 60}\n")
        for classname, name, msg, detail in failures:
            f.write(f"\n  FAIL: {name}\n")
            if classname:
                f.write(f"  File: {classname}\n")
            if msg:
                f.write(f"  Message: {msg}\n")
            if detail:
                f.write(f"  Detail:\n")
                for line in detail.splitlines():
                    f.write(f"    {line}\n")
    else:
        f.write("All tests passed.\n")

print(f"Test report: {report_path} ({status}: {passed}/{total} passed)")
PYEOF
}

generate_report "${JUNIT_FILE}" "${REPORT_FILE}"
exit "${TEST_EXIT:-0}"
