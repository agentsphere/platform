# platform/Justfile

export DATABASE_URL := env("DATABASE_URL", "postgres://platform:dev@localhost:5432/platform_dev")
export VALKEY_URL := env("VALKEY_URL", "redis://localhost:6379")

# Detect in-cluster environment (KUBERNETES_SERVICE_HOST is set automatically in pods)
# Routes test commands to test-in-pod.sh (DNS) vs test-in-cluster.sh (port-forward)
in_cluster := env("KUBERNETES_SERVICE_HOST", "")
test_script := if in_cluster != "" { "hack/test-in-pod.sh" } else { "hack/test-in-cluster.sh" }

default:
    @just --list

# -- Cluster --------------------------------------------------------
cluster-up:
    @if [ -n "{{in_cluster}}" ]; then echo "Already in-cluster, skipping"; exit 0; fi
    bash hack/kind-up.sh

cluster-down:
    @if [ -n "{{in_cluster}}" ]; then echo "Already in-cluster, skipping"; exit 0; fi
    bash hack/kind-down.sh

# -- Dev ------------------------------------------------------------
watch:
    bacon

run:
    cargo run

ui:
    cd ui && npm run build

# -- Types (ts-rs) --------------------------------------------------
types:
    SQLX_OFFLINE=true cargo test --lib -- export_bindings
    cd ui && npx tsc --noEmit --skipLibCheck 2>&1 | grep -v "path.*IntrinsicAttributes" || true
    @echo "Types generated in ui/src/lib/generated/"

# -- Quality --------------------------------------------------------
fmt:
    cargo fmt

lint:
    cargo clippy --all-features -- -D warnings

deny:
    cargo deny check

check: fmt lint deny

# -- Test -----------------------------------------------------------
test:
    cargo nextest run

test-unit:
    cargo nextest run --lib

test-doc:
    cargo test --doc

# Ephemeral services (Kind cluster locally, K8s DNS in-cluster)
test-integration:
    bash {{test_script}} --filter '*_integration'

test-e2e:
    bash {{test_script}} --type e2e

test-llm:
    cargo nextest run --test llm_create_app --run-ignored ignored-only

test-mcp:
    cd mcp && npm test

test-ui:
    @echo "Requires running server: just run"
    cd ui && npx playwright test

# Start platform server, run Playwright tests, stop server
test-ui-headless port="8090":
    #!/usr/bin/env bash
    set -uo pipefail
    echo "Starting platform server on port {{port}}..."
    PLATFORM_DEV=true PLATFORM_LISTEN="0.0.0.0:{{port}}" cargo run &
    SERVER_PID=$!
    for i in $(seq 1 30); do
      curl -sf "http://localhost:{{port}}/healthz" >/dev/null 2>&1 && break
      sleep 2
    done
    curl -sf "http://localhost:{{port}}/healthz" >/dev/null 2>&1 || { echo "Server failed to start"; kill "$SERVER_PID" 2>/dev/null; exit 1; }
    cd ui && PLATFORM_URL="http://localhost:{{port}}" npx playwright test; STATUS=$?
    kill "$SERVER_PID" 2>/dev/null || true
    exit "$STATUS"

# Cleanup stale test namespaces
test-cleanup:
    @echo "Deleting stale test-* namespaces..."
    @kubectl get namespaces -o name | grep '^namespace/test-' | xargs -r kubectl delete --wait=false

# -- Coverage -------------------------------------------------------
cov-unit:
    cargo llvm-cov nextest --lib --lcov --output-path coverage-unit.lcov \
        --ignore-filename-regex '(proto\.rs|ui\.rs)'

cov-integration:
    bash {{test_script}} --filter '*_integration' --coverage --lcov coverage-integration.lcov

cov-e2e:
    bash {{test_script}} --type e2e --coverage --lcov coverage-e2e.lcov

# Combined: unit + integration + E2E
cov-total:
    @echo "=== Combined coverage: unit + integration + E2E ==="
    bash {{test_script}} --type total

# Diff coverage: only lines changed vs a branch
cov-diff branch="main":
    bash {{test_script}} --type total --lcov coverage-total.lcov
    diff-cover coverage-total.lcov --compare-branch={{branch}} --show-uncovered

# Diff coverage strict: fail if changed lines < 100% covered
cov-diff-check branch="main":
    bash {{test_script}} --type total --lcov coverage-total.lcov
    diff-cover coverage-total.lcov --compare-branch={{branch}} --show-uncovered --fail-under=100

cov-html:
    cargo llvm-cov nextest --lib --html --output-dir coverage-html \
        --ignore-filename-regex '(proto\.rs|ui\.rs)'
    @echo "Open coverage-html/index.html"

cov-summary:
    @echo "=== Unit ==="
    @cargo llvm-cov nextest --lib --ignore-filename-regex '(proto\.rs|ui\.rs)' 2>&1 | tail -3

# -- Database -------------------------------------------------------
db-add name:
    cargo sqlx migrate add -r {{ name }}

db-migrate:
    cargo sqlx migrate run

db-revert:
    cargo sqlx migrate revert

db-prepare:
    cargo sqlx prepare

db-check:
    cargo sqlx prepare --check

# -- Build ----------------------------------------------------------
build:
    just ui
    SQLX_OFFLINE=true cargo build --release

cli-build:
    cargo build --release --manifest-path cli/platform-cli/Cargo.toml

cli-install:
    cargo install --path cli/platform-cli

docker tag="platform:dev":
    docker build -f docker/Dockerfile -t {{ tag }} .

agent-image:
    docker build -f docker/Dockerfile.claude-runner -t localhost:8080/platform-runner/platform-claude-runner:latest .
    docker push localhost:8080/platform-runner/platform-claude-runner:latest

registry-login:
    @echo "Login to the platform's built-in registry (admin/admin in dev mode):"
    @echo "  docker login localhost:8080"

# -- Deploy to kind -------------------------------------------------
deploy-local tag="platform:dev":
    just docker {{ tag }}
    kind load docker-image {{ tag }} --name platform
    kubectl apply -k deploy/dev
    kubectl rollout status deployment/platform -n platform --timeout=60s

# -- Full CI locally ------------------------------------------------
ci: fmt lint deny test-unit test-mcp test-integration build
    @echo "All checks passed"

ci-full: fmt lint deny test-unit test-mcp test-integration test-e2e build
    @echo "All checks passed (including E2E tests)"
