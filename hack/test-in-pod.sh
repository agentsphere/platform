#!/usr/bin/env bash
# test-in-pod.sh — Run integration/E2E tests from INSIDE a K8s cluster pod.
#
# This is the in-cluster variant of test-in-cluster.sh. Instead of port-
# forwarding, it connects to ephemeral services via K8s DNS.
#
# Usage:
#   bash hack/test-in-pod.sh                          # integration tests
#   bash hack/test-in-pod.sh --filter '*_integration' # specific filter
#   bash hack/test-in-pod.sh --type e2e               # E2E tests
#   bash hack/test-in-pod.sh --type total             # all tiers with coverage
#   bash hack/test-in-pod.sh --threads 4              # custom parallelism
#   bash hack/test-in-pod.sh --coverage               # with coverage instrumentation
#   bash hack/test-in-pod.sh --coverage --lcov out.lcov  # coverage → LCOV file

set -euo pipefail

# ── Defaults ──────────────────────────────────────────────────────────────
TEST_FILTER="*_integration"
TEST_TYPE="integration"   # "integration", "e2e", or "total"
TEST_THREADS=""
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
RUN_ID="$(date +%s)-$(od -An -tx1 -N4 /dev/urandom | tr -d ' \n')"
NS="test-${RUN_ID}"
PIPELINE_NS="${NS}-pipelines"
AGENT_NS="${NS}-agents"

# ── Cleanup on exit ───────────────────────────────────────────────────────
cleanup() {
  echo ""
  echo "==> Cleaning up"

  echo "  Deleting namespace: ${NS}"
  kubectl delete namespace "${NS}" --wait=false 2>/dev/null || true
  if [[ "$TEST_TYPE" == "e2e" || "$TEST_TYPE" == "total" ]]; then
    kubectl delete namespace "${PIPELINE_NS}" --wait=false 2>/dev/null || true
    kubectl delete namespace "${AGENT_NS}" --wait=false 2>/dev/null || true
    kubectl delete clusterrolebinding "test-runner-${RUN_ID}" 2>/dev/null || true
  fi
}
trap cleanup EXIT INT TERM

# ── Pre-flight checks ────────────────────────────────────────────────────
echo "==> Pre-flight checks"

if ! kubectl cluster-info &>/dev/null; then
  echo "ERROR: Cannot reach K8s API. Are you running inside a pod with a ServiceAccount?"
  exit 1
fi

echo "  K8s API:      reachable"
echo "  Namespace:    ${NS}"
echo "  Test type:    ${TEST_TYPE}"
echo "  Test filter:  ${TEST_FILTER}"

# ── Create namespace and deploy services ──────────────────────────────────
echo ""
echo "==> Creating namespace: ${NS}"
kubectl create namespace "${NS}"

echo "==> Deploying services"
kubectl apply -n "${NS}" -f "${SCRIPT_DIR}/test-manifests/postgres.yaml"
kubectl apply -n "${NS}" -f "${SCRIPT_DIR}/test-manifests/valkey.yaml"
kubectl apply -n "${NS}" -f "${SCRIPT_DIR}/test-manifests/minio.yaml"

echo "==> Waiting for services to be ready"
kubectl wait -n "${NS}" --for=condition=Ready pod/postgres --timeout=60s
kubectl wait -n "${NS}" --for=condition=Ready pod/valkey --timeout=30s
kubectl wait -n "${NS}" --for=condition=Ready pod/minio --timeout=30s
echo "  All pods ready"

# ── Post-deploy setup ────────────────────────────────────────────────────
echo "==> Post-deploy setup"

# Wait for Postgres to accept connections (pod Ready doesn't guarantee it)
until kubectl exec -n "${NS}" postgres -- pg_isready -U platform -q 2>/dev/null; do
  sleep 0.5
done

# Grant CREATEDB (required by #[sqlx::test] macro)
kubectl exec -n "${NS}" postgres -- \
  psql -U platform -d platform_dev -c "SELECT 1;" -q

# Create MinIO bucket
kubectl exec -n "${NS}" minio -- mkdir -p /data/platform-e2e

echo "  Postgres ready, MinIO bucket created"

# ── E2E-specific setup ───────────────────────────────────────────────────
if [[ "$TEST_TYPE" == "e2e" || "$TEST_TYPE" == "total" ]]; then
  echo ""
  echo "==> E2E setup: creating pipeline/agent namespaces + RBAC"

  kubectl create namespace "${PIPELINE_NS}"
  kubectl create namespace "${AGENT_NS}"

  # Apply the ClusterRole (idempotent)
  kubectl apply -f "${SCRIPT_DIR}/test-manifests/rbac.yaml"

  # Create ServiceAccount in test namespace
  kubectl create serviceaccount test-runner -n "${NS}" 2>/dev/null || true

  # Bind ClusterRole to the ServiceAccount
  kubectl create clusterrolebinding "test-runner-${RUN_ID}" \
    --clusterrole=test-runner \
    --serviceaccount="${NS}:test-runner"
fi

# ── Run tests ─────────────────────────────────────────────────────────────
echo ""
echo "==> Running tests"
echo "────────────────────────────────────────────────────────────────"

# Build env vars — use K8s DNS instead of localhost + port-forward
export DATABASE_URL="postgres://platform:dev@postgres.${NS}.svc.cluster.local:5432/platform_dev"
export VALKEY_URL="redis://valkey.${NS}.svc.cluster.local:6379"
export MINIO_ENDPOINT="http://minio.${NS}.svc.cluster.local:9000"
export MINIO_ACCESS_KEY="platform"
export MINIO_SECRET_KEY="devdevdev"
export PLATFORM_MASTER_KEY="0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
export PLATFORM_DEV=true
export RUST_LOG="warn"

# Always use offline sqlx cache — it contains pre-computed types needed by
# sqlx::query! macros. Under coverage mode (--cfg=coverage), type inference
# breaks without the offline cache.
export SQLX_OFFLINE=true

if [[ "$TEST_TYPE" == "e2e" || "$TEST_TYPE" == "total" ]]; then
  export PLATFORM_PIPELINE_NAMESPACE="${PIPELINE_NS}"
  export PLATFORM_AGENT_NAMESPACE="${AGENT_NS}"
fi

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
    --ignore-filename-regex "${COV_IGNORE_REGEX}" --no-fail-fast \
    || TIER_FAILURES=$((TIER_FAILURES + 1))

  echo ""
  echo "==> Running E2E tests (coverage, no report)"
  cargo llvm-cov nextest --no-report --test 'e2e_*' \
    --run-ignored ignored-only --test-threads 2 \
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
    exit 1
  fi
else
  # Single tier run
  NEXTEST_ARGS=(--test "${TEST_FILTER}")

  if [[ -n "$TEST_THREADS" ]]; then
    NEXTEST_ARGS+=(--test-threads "${TEST_THREADS}")
  elif [[ "$TEST_TYPE" == "e2e" ]]; then
    NEXTEST_ARGS+=(--test-threads 2)
  fi

  if [[ "$TEST_TYPE" == "e2e" ]]; then
    NEXTEST_ARGS+=(--run-ignored ignored-only)
  fi

  if $COVERAGE_MODE; then
    COV_ARGS=(--ignore-filename-regex "${COV_IGNORE_REGEX}")
    if $COV_NO_REPORT; then
      COV_ARGS+=(--no-report)
    elif [[ -n "$LCOV_PATH" ]]; then
      COV_ARGS+=(--lcov --output-path "${LCOV_PATH}")
    fi
    cargo llvm-cov nextest "${COV_ARGS[@]}" "${NEXTEST_ARGS[@]}"
  else
    cargo nextest run "${NEXTEST_ARGS[@]}"
  fi
fi
