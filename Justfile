# platform/Justfile

set dotenv-filename := ".env.dev"
set dotenv-load := true

export DATABASE_URL := env("DATABASE_URL", "postgres://platform:dev@localhost:5432/platform_dev")
export VALKEY_URL := env("VALKEY_URL", "redis://localhost:6379")
export KUBECONFIG := env("KUBECONFIG", env("HOME", "/tmp") / ".kube/kind-platform")

# Detect worktree name for path isolation (avoids cross-worktree binary overwrites)
worktree := `bash hack/detect-worktree.sh`
agent_runner_dir := "/tmp/platform-e2e/" + worktree + "/agent-runner"

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
dev-env:
    bash hack/dev-env.sh

dev-env-stop:
    #!/usr/bin/env bash
    set -euo pipefail
    export KUBECONFIG="${HOME}/.kube/kind-platform"
    WORKTREE="$(bash hack/detect-worktree.sh)"
    NS="platform-dev-${WORKTREE}"
    echo "Deleting namespace: ${NS}"
    kubectl delete namespace "${NS}" --wait=false 2>/dev/null || true
    rm -f .env.dev
    # Clean up legacy PID files from old port-forward approach
    if [ -f /tmp/platform-dev-pf.pids ]; then
      while read -r pid; do kill "$pid" 2>/dev/null || true; done < /tmp/platform-dev-pf.pids
      rm -f /tmp/platform-dev-pf.pids
    fi
    echo "Dev environment stopped (${WORKTREE})"

dev-env-stop-all:
    #!/usr/bin/env bash
    set -euo pipefail
    export KUBECONFIG="${HOME}/.kube/kind-platform"
    echo "Deleting all platform-dev-* namespaces..."
    kubectl get namespaces -o name | grep '^namespace/platform-dev-' | xargs -r kubectl delete --wait=false 2>/dev/null || true
    rm -f .env.dev
    echo "All dev environments stopped"

dev:
    bash hack/dev-env.sh

watch:
    bacon

run:
    @if [ ! -f .env.dev ]; then echo "ERROR: .env.dev not found. Run: just dev-env"; exit 1; fi
    @mkdir -p /tmp/platform-repos /tmp/platform-ops-repos /tmp/platform-e2e/seed-images
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
    cargo test --manifest-path cli/agent-runner/Cargo.toml --bin agent-runner

test-doc:
    cargo test --doc

# Ephemeral services (Kind cluster locally, K8s DNS in-cluster)
test-integration filter="":
    bash {{test_script}} --filter '*_integration' {{ if filter != "" { "--expr 'test(" + filter + ")'" } else { "" } }}

# Run a specific integration test binary (avoids enumerating all binaries)
test-integration-bin bin filter="":
    bash {{test_script}} --filter '{{bin}}' {{ if filter != "" { "--expr 'test(" + filter + ")'" } else { "" } }}

test-e2e filter="":
    bash {{test_script}} --type e2e {{ if filter != "" { "--expr 'test(" + filter + ")'" } else { "" } }}

test-llm:
    cargo nextest run --test llm_create_app --test llm_create_app_e2e --run-ignored ignored-only

test-e2e-llm:
    bash hack/run-llm-e2e.sh

test-mcp:
    cd mcp && npm test

test-ui:
    @echo "Requires running server: just run"
    @kubectl exec -n platform pod/valkey -- valkey-cli DEL rate:login:admin >/dev/null 2>&1 || true
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
    kubectl exec -n platform pod/valkey -- valkey-cli DEL rate:login:admin >/dev/null 2>&1 || true
    cd ui && PLATFORM_URL="http://localhost:{{port}}" npx playwright test; STATUS=$?
    kill "$SERVER_PID" 2>/dev/null || true
    exit "$STATUS"

# Cleanup stale test namespaces
test-cleanup:
    @echo "Deleting stale platform-test-* namespaces..."
    @kubectl get namespaces -o name | grep '^namespace/platform-test-' | xargs -r kubectl delete --wait=false

# All tests except LLM (unit + integration + e2e)
test-all: test-unit test-integration test-e2e

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

# -- Agent Runner CLI -----------------------------------------------
# Build seed images + cross-compiled agent-runner (cached, worktree-scoped)
build-agent-images:
    bash hack/build-agent-images.sh

cli-build:
    cargo build --release --manifest-path cli/agent-runner/Cargo.toml

# Cross-compile agent-runner for linux/amd64 and linux/arm64 (uses Docker)
# Default dir is worktree-scoped to avoid overwrites between parallel worktrees.
cli-cross dir=agent_runner_dir:
    mkdir -p {{ dir }}
    docker run --rm \
      -v "$(pwd)/cli/agent-runner:/src" \
      -v "{{ dir }}:/out" \
      rust:1.88-slim-bookworm sh -c '\
        apt-get update && apt-get install -y --no-install-recommends \
          gcc-aarch64-linux-gnu libc6-dev-arm64-cross \
          gcc-x86-64-linux-gnu libc6-dev-amd64-cross && \
        rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu && \
        cd /src && \
        CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
          cargo build --release --target aarch64-unknown-linux-gnu && \
        CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
          cargo build --release --target x86_64-unknown-linux-gnu && \
        cp target/aarch64-unknown-linux-gnu/release/agent-runner /out/arm64 && \
        cp target/x86_64-unknown-linux-gnu/release/agent-runner /out/amd64'

cli-install:
    cargo install --path cli/agent-runner

cli-test:
    cargo test --manifest-path cli/agent-runner/Cargo.toml --bin agent-runner

cli-lint:
    cargo clippy --manifest-path cli/agent-runner/Cargo.toml --all-features -- -D warnings

cli-fmt:
    cargo fmt --manifest-path cli/agent-runner/Cargo.toml

cli-test-pubsub:
    bash hack/test-cli-incluster.sh

cli-test-llm:
    cargo test --manifest-path cli/agent-runner/Cargo.toml --bin agent-runner llm_ -- --ignored

cli-cov:
    cargo llvm-cov --manifest-path cli/agent-runner/Cargo.toml --bin agent-runner

docker tag="platform:dev":
    docker build -f docker/Dockerfile -t {{ tag }} .

agent-image registry_url="${PLATFORM_REGISTRY_URL:-localhost:8080}":
    docker build -f docker/Dockerfile.platform-runner -t {{registry_url}}/platform-runner:latest .
    docker push {{registry_url}}/platform-runner:latest

agent-image-bare registry_url="${PLATFORM_REGISTRY_URL:-localhost:8080}":
    docker build -f docker/Dockerfile.platform-runner-bare -t {{registry_url}}/platform-runner:latest .
    docker push {{registry_url}}/platform-runner:latest

agent-images registry_url="${PLATFORM_REGISTRY_URL:-localhost:8080}":
    just agent-image {{registry_url}}
    just agent-image-bare {{registry_url}}

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
ci: fmt lint deny test-unit cli-lint cli-test test-mcp test-integration build
    @echo "All checks passed"

ci-full: fmt lint deny test-unit cli-lint cli-test test-mcp test-integration test-e2e build
    @echo "All checks passed (including E2E tests)"
