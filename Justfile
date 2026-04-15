# platform/Justfile

set dotenv-filename := ".env.dev"
set dotenv-load := true

mod cli
mod ui
mod mcp

export DATABASE_URL := env("DATABASE_URL", "postgres://platform:dev@localhost:5432/platform_dev")
export VALKEY_URL := env("VALKEY_URL", "redis://localhost:6379")
export KUBECONFIG := env("KUBECONFIG", env("HOME", "/tmp") / ".kube/platform")

# Detect worktree name for path isolation (avoids cross-worktree binary overwrites)
worktree := `bash hack/detect-worktree.sh`

# Detect in-cluster environment (KUBERNETES_SERVICE_HOST is set automatically in pods)
# Routes test commands to test-in-pod.sh (DNS) vs test-in-cluster.sh (port-forward)
in_cluster := env("KUBERNETES_SERVICE_HOST", "")
test_script := if in_cluster != "" { "hack/test-in-pod.sh" } else { "hack/test-in-cluster.sh" }

default:
    @just --list

# -- Cluster --------------------------------------------------------
[group('cluster')]
cluster-up:
    @if [ -n "{{in_cluster}}" ]; then echo "Already in-cluster, skipping"; exit 0; fi
    bash hack/cluster-up.sh

[group('cluster')]
cluster-down:
    @if [ -n "{{in_cluster}}" ]; then echo "Already in-cluster, skipping"; exit 0; fi
    bash hack/cluster-down.sh

# -- Dev ------------------------------------------------------------

# Deploy worktree-isolated services + generate .env.dev
[group('dev')]
dev-up:
    bash hack/dev-up.sh

# Tear down this worktree's dev namespace
[group('dev')]
dev-down:
    #!/usr/bin/env bash
    set -euo pipefail
    export KUBECONFIG="${HOME}/.kube/platform"
    WORKTREE="$(bash hack/detect-worktree.sh)"
    NS_PREFIX="platform-dev-${WORKTREE}"

    echo "Looking for namespaces starting with: ${NS_PREFIX}..."

    # Grab all namespaces, format as 'namespace/name', and filter by prefix
    # '|| true' prevents grep from failing the script if no matches are found
    MATCHING_NS=$(kubectl get namespaces -o name | grep "^namespace/${NS_PREFIX}" || true)

    if [[ -z "$MATCHING_NS" ]]; then
        echo "No matching namespaces found."
    else
        for ns in $MATCHING_NS; do
            echo "Deleting ${ns}..."
            kubectl delete "$ns" --wait=false 2>/dev/null || true
        done
    fi
    rm -f .env.dev
    # Clean up seed cache (MinIO is ephemeral — stale cache causes blob NotFound)
    rm -f /tmp/platform-e2e/"${WORKTREE}"/seed-images/.*.seed-cache.json
    rm -rf /tmp/platform-e2e/"${WORKTREE}"/repos
    rm -rf /tmp/platform-e2e/"${WORKTREE}"/ops-repos
    # Clean up legacy PID files from old port-forward approach
    if [ -f /tmp/platform-dev-pf.pids ]; then
      while read -r pid; do kill "$pid" 2>/dev/null || true; done < /tmp/platform-dev-pf.pids
      rm -f /tmp/platform-dev-pf.pids
    fi
    echo "Dev environment stopped (${WORKTREE})"

# Tear down ALL worktree dev namespaces
[group('dev')]
dev-down-all:
    #!/usr/bin/env bash
    set -euo pipefail
    export KUBECONFIG="${HOME}/.kube/platform"
    echo "Deleting all platform-dev-* namespaces..."
    kubectl get namespaces -o name | grep '^namespace/platform-dev-' | xargs -r kubectl delete --wait=false 2>/dev/null || true
    rm -f .env.dev
    echo "All dev environments stopped"

# Run server in dev mode (uses .env.dev from dev-up), logs to server.log
[group('dev')]
dev:
    @if [ ! -f .env.dev ]; then echo "ERROR: .env.dev not found. Run: just dev-up"; exit 1; fi
    @grep -E '^PLATFORM_(GIT_REPOS|OPS_REPOS|SEED_IMAGES)_PATH=' .env.dev | cut -d= -f2 | xargs mkdir -p
    cargo run -p platform-next 2>&1 | tee server.log

# Run server with custom env file
[group('dev')]
run env=".env":
    #!/usr/bin/env bash
    set -euo pipefail
    if [ ! -f "{{env}}" ]; then echo "ERROR: {{env}} not found."; exit 1; fi
    set -a; source "{{env}}"; set +a
    exec cargo run -p platform-next

[group('dev')]
watch:
    bacon

# -- Types (ts-rs) --------------------------------------------------
types:
    TS_RS_EXPORT_DIR=ui/src/lib/generated SQLX_OFFLINE=true cargo test -p platform-next export_bindings
    TS_RS_EXPORT_DIR=ui/src/lib/generated SQLX_OFFLINE=true cargo test -p platform-types --features ts export_bindings
    cd ui && npx tsc --noEmit --skipLibCheck 2>&1 | grep -v "path.*IntrinsicAttributes" || true
    @echo "Types generated in ui/src/lib/generated/"

# -- Quality --------------------------------------------------------
[group('quality')]
fmt:
    cargo fmt

[group('quality')]
lint:
    cargo clippy --all-features -- -D warnings

[group('quality')]
deny:
    cargo deny check

[group('quality')]
check: fmt lint deny

# -- Test -----------------------------------------------------------
#
# Seven test tiers, selected by nextest profiles + file naming convention.
# All tiers match across ALL workspace crates (not just root):
#   --lib                                → unit        (default profile)
#   crates/*/tests/*_integration.rs      → int         (--profile integration)
#   crates/*/tests/*_k8s.rs             → k8s          (--profile k8s)
#   crates/*/tests/*_contract.rs        → contract     (--profile contract)
#   crates/*/tests/*_api.rs             → api          (--profile api)
#   crates/*/tests/e2e_*.rs             → e2e          (--profile e2e)
#   crates/*/tests/llm_*.rs             → llm          (--profile llm)
#
# Profiles are defined in .config/nextest.toml using binary() filters.
# No #[ignore] attributes needed — the file name IS the tier selector.

# All workspace crates (package names from Cargo.toml)
_crate_all := "-p platform-types -p platform-auth -p platform-observe -p platform-secrets -p platform-k8s -p platform-git -p platform-registry -p platform-agent -p platform-ingest -p platform-k8s-watcher -p platform-proxy -p platform-proxy-init -p platform-pipeline -p platform-deployer -p platform-webhook -p platform-notify -p platform-mesh -p platform-ops-repo -p platform-agent-runner -p platform-next -p platform-operator"
# Crates with --lib targets for coverage (proxy included — coverage uses --lib to avoid binary crash)
# Excludes proxy-init and agent-runner (no lib.rs, binary-only crates)
_crate_lib := "-p platform-types -p platform-auth -p platform-observe -p platform-secrets -p platform-k8s -p platform-git -p platform-registry -p platform-agent -p platform-ingest -p platform-k8s-watcher -p platform-proxy -p platform-pipeline -p platform-deployer -p platform-webhook -p platform-notify -p platform-mesh -p platform-ops-repo"

# Unit tests (all packages or one crate)
# just test-unit                      → all unit tests
# just test-unit platform-auth        → one crate only
# just test-unit "" my_parser         → filter by name
[group('test')]
test-unit crate="" filter="":
    cargo nextest run \
        {{ if crate != "" { "-p " + crate } else { "" } }} \
        --lib \
        {{ if filter != "" { "-E 'test(" + filter + ")'" } else { "" } }}; \
    s=$?; bash hack/generate-test-report.sh 2>/dev/null || true; [ $s -eq 0 ]

# Doc tests
[group('test')]
test-doc:
    cargo test --doc

# Library integration tests (DB+Valkey+MinIO, crate-level)
# just test-int                       → root library tests (via test-in-cluster.sh)
# just test-int platform-auth         → one crate (direct, uses .env.dev)
# just test-int "" login              → filter by name
[group('test')]
test-int crate="" filter="":
    {{ if crate != "" { "cargo nextest run -p " + crate + " --profile integration" + if filter != "" { " -E 'test(" + filter + ")'" } else { "" } } else { "bash " + test_script + " --type integration" + if filter != "" { " --expr 'test(" + filter + ")'" } else { "" } } }}

# K8s integration tests (needs Kind cluster)
# just test-k8s                       → all crate K8s tests
# just test-k8s platform-k8s          → one crate
[group('test')]
test-k8s crate="" filter="":
    cargo nextest run \
        {{ if crate != "" { "-p " + crate } else { _crate_all } }} \
        --profile k8s \
        {{ if filter != "" { "-E 'test(" + filter + ")'" } else { "" } }}

# Contract tests (response shape stability)
# just test-contract                  → all contract tests
# just test-contract login            → filter by name
[group('test')]
test-contract filter="":
    bash {{test_script}} --type contract {{ if filter != "" { "--expr 'test(" + filter + ")'" } else { "" } }}

# HTTP handler / API tests (needs full cluster infra)
# just test-api                       → all handler tests
# just test-api auth                  → filter by name
[group('test')]
test-api filter="":
    bash {{test_script}} --type api {{ if filter != "" { "--expr 'test(" + filter + ")'" } else { "" } }}

# E2E multi-step journeys
# just test-e2e                       → all E2E tests
# just test-e2e project_flow          → filter by name
[group('test')]
test-e2e filter="":
    bash {{test_script}} --type e2e {{ if filter != "" { "--expr 'test(" + filter + ")'" } else { "" } }}

# LLM integration tests (real Claude CLI, requires Anthropic credentials)
# Uses the llm profile which selects tests/llm_*.rs + inline #[ignore] LLM tests
[group('test')]
test-llm:
    cargo nextest run --profile llm --run-ignored ignored-only

# LLM E2E test (full create-app flow with real Claude CLI + K8s)
[group('test')]
test-e2e-llm:
    bash hack/test-e2e-llm.sh

# Cleanup stale test namespaces
[group('test')]
test-cleanup:
    @echo "Deleting stale platform-test-* namespaces..."
    @kubectl get namespaces -o name | grep '^namespace/platform-test-' | xargs -r kubectl delete --wait=false

# Everything except LLM
[group('test')]
test-all: test-unit test-int test-k8s

# -- Coverage -------------------------------------------------------
[group('coverage')]
cov-unit:
    cargo llvm-cov nextest --lib --lcov --output-path coverage-unit.lcov \
        --ignore-filename-regex '(proto\.rs|ui\.rs)'

[group('coverage')]
cov-html:
    cargo llvm-cov nextest --lib --html --output-dir coverage-html \
        --ignore-filename-regex '(proto\.rs|ui\.rs)'
    @echo "Open coverage-html/index.html"

[group('coverage')]
cov-summary:
    @echo "=== Unit ==="
    @cargo llvm-cov nextest --lib --ignore-filename-regex '(proto\.rs|ui\.rs)' 2>&1 | tail -3

# Crate coverage (unit + integration + K8s, uses DB/Valkey/K8s)
# Excludes binary-only crates without lib.rs (proxy-init, ingest)
# --ignore-default-filter bypasses the default profile's exclusion of _integration/_k8s binaries
# just crate-cov                         → all library crates
# just crate-cov platform-auth           → one crate
[group('coverage')]
crate-cov crate="":
    cargo llvm-cov nextest \
        {{ if crate != "" { "-p " + crate } else { _crate_all } }} \
        --lib --test '*' --ignore-default-filter --no-fail-fast \
        --ignore-filename-regex 'main\.rs' \
        --lcov --output-path crate-coverage.lcov
    @echo "Coverage written to crate-coverage.lcov"

# Crate coverage HTML report
# just crate-cov-html                    → all library crates
# just crate-cov-html platform-auth      → one crate
[group('coverage')]
crate-cov-html crate="":
    cargo llvm-cov nextest \
        {{ if crate != "" { "-p " + crate } else { _crate_all } }} \
        --lib --test '*' --ignore-default-filter --no-fail-fast \
        --ignore-filename-regex 'main\.rs' \
        --html --output-dir crate-coverage-html
    @echo "Open crate-coverage-html/index.html"

# Crate coverage: lcov + HTML in one run (unit + integration + K8s)
# just test-cov                          → all library crates
# just test-cov platform-auth            → one crate
[group('coverage')]
test-cov crate="":
    cargo llvm-cov nextest \
        {{ if crate != "" { "-p " + crate } else { _crate_all } }} \
        --lib --test '*' --ignore-default-filter --no-fail-fast \
        --ignore-filename-regex 'main\.rs' \
        --lcov --output-path crate-coverage.lcov
    cargo llvm-cov report \
        {{ if crate != "" { "-p " + crate } else { _crate_all } }} \
        --ignore-filename-regex 'main\.rs' \
        --html --output-dir crate-coverage-html
    @echo "Coverage written to crate-coverage.lcov"
    @echo "Open crate-coverage-html/index.html"

# -- Database -------------------------------------------------------
[group('db')]
db-add name:
    cargo sqlx migrate add -r {{ name }}

[group('db')]
db-migrate:
    cargo sqlx migrate run

[group('db')]
db-revert:
    cargo sqlx migrate revert

[group('db')]
db-prepare:
    cd crates/foundation/platform-types && cargo sqlx prepare
    cd crates/libs/platform-auth && cargo sqlx prepare
    cd crates/libs/platform-observe && cargo sqlx prepare
    cd crates/libs/platform-secrets && cargo sqlx prepare
    cd crates/libs/platform-agent && cargo sqlx prepare
    cd crates/libs/platform-webhook && cargo sqlx prepare
    cd crates/libs/platform-notify && cargo sqlx prepare
    cd crates/libs/platform-git && cargo sqlx prepare
    cd crates/libs/platform-registry && cargo sqlx prepare
    cd crates/libs/platform-pipeline && cargo sqlx prepare

[group('db')]
db-check:
    cd crates/foundation/platform-types && cargo sqlx prepare --check
    cd crates/libs/platform-auth && cargo sqlx prepare --check
    cd crates/libs/platform-observe && cargo sqlx prepare --check
    cd crates/libs/platform-secrets && cargo sqlx prepare --check
    cd crates/libs/platform-agent && cargo sqlx prepare --check

[group('db')]
db-seed:
    cargo run -p platform-seed

# -- Build ----------------------------------------------------------
[group('build')]
build:
    just ui build
    SQLX_OFFLINE=true cargo build --release

# Build seed images + cross-compiled agent-runner (cached, worktree-scoped)
[group('build')]
build-agent-images:
    bash hack/build-agent-images.sh

[group('build')]
docker tag="platform:dev":
    docker build -f docker/Dockerfile -t {{ tag }} .

[group('build')]
agent-image registry_url="${PLATFORM_REGISTRY_URL:-localhost:8080}":
    docker build -f docker/Dockerfile.platform-runner -t {{registry_url}}/platform-runner:latest .
    docker push {{registry_url}}/platform-runner:latest

[group('build')]
agent-image-bare registry_url="${PLATFORM_REGISTRY_URL:-localhost:8080}":
    docker build -f docker/Dockerfile.platform-runner-bare -t {{registry_url}}/platform-runner-bare:latest .
    docker push {{registry_url}}/platform-runner-bare:latest

[group('build')]
agent-images registry_url="${PLATFORM_REGISTRY_URL:-localhost:8080}":
    just agent-image {{registry_url}}
    just agent-image-bare {{registry_url}}

registry-login:
    @echo "Login to the platform's built-in registry (admin/admin in dev mode):"
    @echo "  docker login localhost:8080"

# -- Docs Viewer ---------------------------------------------------
[group('docs')]
docs-viewer:
    cd docs/viewer && npm ci && npm run build

[group('docs')]
docs-serve:
    cd docs/viewer && npm run dev

# -- Docker images (build + load into Kind) -------------------------
#
# Build individual container images and load them into the local Kind cluster
# so they can be deployed/tested independently without relying on seed images.
#
# Usage:
#   just docker-platform       → main platform binary
#   just docker-ingest         → standalone OTLP ingest
#   just docker-k8s-watcher    → K8s event watcher
#   just docker-proxy          → service mesh proxy (cross-compiled musl)
#   just docker-proxy-init     → transparent proxy init container
#   just docker-all            → all of the above

_version := `grep '^version' crates/bins/platform/Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/'`
_kind_cluster := "platform"

[group('docker')]
docker-platform tag=("platform-next:" + _version):
    docker build -f docker/Dockerfile.platform-next -t {{tag}} .
    kind load docker-image {{tag}} --name {{_kind_cluster}}
    @echo "Loaded {{tag}} into Kind cluster"

[group('docker')]
docker-ingest tag=("platform-ingest:" + _version):
    docker build -f docker/Dockerfile.platform-ingest -t {{tag}} .
    kind load docker-image {{tag}} --name {{_kind_cluster}}
    @echo "Loaded {{tag}} into Kind cluster"

[group('docker')]
docker-k8s-watcher tag=("platform-k8s-watcher:" + _version):
    docker build -f docker/Dockerfile.platform-k8s-watcher -t {{tag}} .
    kind load docker-image {{tag}} --name {{_kind_cluster}}
    @echo "Loaded {{tag}} into Kind cluster"

[group('docker')]
docker-proxy tag=("platform-proxy:" + _version):
    #!/usr/bin/env bash
    set -euo pipefail
    bash hack/build-proxy.sh
    WORKTREE="$(bash hack/detect-worktree.sh)"
    PROXY_DIR="/tmp/platform-e2e/${WORKTREE}/proxy"
    TMPCTX=$(mktemp -d)
    trap 'rm -rf "$TMPCTX"' EXIT
    cp "${PROXY_DIR}/amd64" "${TMPCTX}/platform-proxy-amd64"
    cp "${PROXY_DIR}/arm64" "${TMPCTX}/platform-proxy-arm64"
    cp docker/Dockerfile.platform-proxy "${TMPCTX}/Dockerfile"
    docker build -t {{tag}} "$TMPCTX"
    kind load docker-image {{tag}} --name {{_kind_cluster}}
    echo "Loaded {{tag}} into Kind cluster"

[group('docker')]
docker-proxy-init tag=("platform-proxy-init:" + _version):
    #!/usr/bin/env bash
    set -euo pipefail
    bash hack/build-proxy.sh
    WORKTREE="$(bash hack/detect-worktree.sh)"
    PROXY_DIR="/tmp/platform-e2e/${WORKTREE}/proxy"
    PROXY_INIT_DIR="/tmp/platform-e2e/${WORKTREE}/proxy-init"
    TMPCTX=$(mktemp -d)
    trap 'rm -rf "$TMPCTX"' EXIT
    cp "${PROXY_INIT_DIR}/amd64" "${TMPCTX}/platform-proxy-init-amd64"
    cp "${PROXY_INIT_DIR}/arm64" "${TMPCTX}/platform-proxy-init-arm64"
    cp "${PROXY_DIR}/amd64" "${TMPCTX}/platform-proxy-amd64"
    cp "${PROXY_DIR}/arm64" "${TMPCTX}/platform-proxy-arm64"
    cp docker/Dockerfile.platform-proxy-init "${TMPCTX}/Dockerfile"
    docker build -t {{tag}} "$TMPCTX"
    kind load docker-image {{tag}} --name {{_kind_cluster}}
    echo "Loaded {{tag}} into Kind cluster"

[group('docker')]
docker-runner tag=("platform-runner:" + _version):
    docker build -f docker/Dockerfile.platform-runner -t {{tag}} .
    kind load docker-image {{tag}} --name {{_kind_cluster}}
    @echo "Loaded {{tag}} into Kind cluster"

[group('docker')]
docker-runner-bare tag=("platform-runner-bare:" + _version):
    docker build -f docker/Dockerfile.platform-runner-bare -t {{tag}} .
    kind load docker-image {{tag}} --name {{_kind_cluster}}
    @echo "Loaded {{tag}} into Kind cluster"

[group('docker')]
docker-all: docker-platform docker-ingest docker-k8s-watcher docker-proxy docker-proxy-init docker-runner docker-runner-bare
    @echo "All images loaded into Kind cluster (version {{_version}})"

# -- Full CI locally ------------------------------------------------
ci: fmt lint deny test-unit test-int cli::lint cli::test mcp::test build
    @echo "All checks passed"

ci-full: fmt lint deny test-unit test-int test-k8s cli::lint cli::test mcp::test build
    @echo "All checks passed (including K8s tests)"
