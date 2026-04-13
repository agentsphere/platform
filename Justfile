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
    cargo run 2>&1 | tee server.log

# Run server with custom env file
[group('dev')]
run env=".env":
    #!/usr/bin/env bash
    set -euo pipefail
    if [ ! -f "{{env}}" ]; then echo "ERROR: {{env}} not found."; exit 1; fi
    set -a; source "{{env}}"; set +a
    exec cargo run

[group('dev')]
watch:
    bacon

# -- Types (ts-rs) --------------------------------------------------
types:
    SQLX_OFFLINE=true cargo test --lib -- export_bindings
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

# Platform unit tests only (no CLI, no UI)
# just test-unit             → all unit tests
# just test-unit my_parser   → filter by name
[group('test')]
test-unit filter="":
    cargo nextest run --lib {{ if filter != "" { "-E 'test(" + filter + ")'" } else { "" } }}; \
    s=$?; bash hack/generate-test-report.sh 2>/dev/null || true; [ $s -eq 0 ]

[group('test')]
test-doc:
    cargo test --doc

# Integration tests (ephemeral cluster services)
# just test-integration                → all integration tests
# just test-integration login          → filter by test name
[group('test')]
test-integration filter="":
    bash {{test_script}} --filter '*_integration' {{ if filter != "" { "--expr 'test(" + filter + ")'" } else { "" } }}

# Subsystem filter: integration tests that require real K8s operations (pods, namespaces, etc.)
subsystem_filter := "binary(executor_integration) | binary(executor_deploy_test_integration) | binary(executor_coverage_integration) | binary(session_integration) | binary(registry_pull_integration) | binary(mesh_integration) | binary(gateway_controller_integration) | binary(deployment_integration)"

# Core integration tests — excludes K8s-heavy subsystem tests for faster iteration
# just test-core                       → all core integration tests (~87% of suite)
# just test-core login                 → filter by test name
[group('test')]
test-core filter="":
    bash {{test_script}} --filter '*_integration' --expr 'not ({{subsystem_filter}}){{ if filter != "" { " & test(" + filter + ")" } else { "" } }}'

# Subsystem integration tests — only K8s-heavy tests (executor, deployer, mesh, gateway, registry pull)
# just test-subsystem                  → all subsystem tests (~13% of suite)
# just test-subsystem executor         → filter by test name
[group('test')]
test-subsystem filter="":
    bash {{test_script}} --filter '*_integration' --threads 8 --expr '({{subsystem_filter}}){{ if filter != "" { " & test(" + filter + ")" } else { "" } }}'

# Run a specific test binary
# just test-bin auth_integration           → all tests in binary
# just test-bin auth_integration login     → filter within binary
[group('test')]
test-bin bin filter="":
    bash {{test_script}} --filter '{{bin}}' {{ if filter != "" { "--expr 'test(" + filter + ")'" } else { "" } }}

# E2E tests
# just test-e2e                            → all E2E tests
# just test-e2e project_flow               → filter by name
[group('test')]
test-e2e filter="":
    bash {{test_script}} --type e2e {{ if filter != "" { "--expr 'test(" + filter + ")'" } else { "" } }}

# E2E specific binary + filter
# just test-e2e-bin e2e_agent                     → all tests in e2e_agent binary
# just test-e2e-bin e2e_agent git_clone_push      → specific test in binary
[group('test')]
test-e2e-bin bin filter="":
    bash {{test_script}} --type e2e --filter '{{bin}}' {{ if filter != "" { "--expr 'test(" + filter + ")'" } else { "" } }}

# LLM integration tests (real Claude CLI, requires Anthropic credentials)
[group('test')]
test-llm:
    cargo nextest run --test llm_create_app --test llm_create_app_e2e --run-ignored ignored-only

# LLM E2E test (full create-app flow with real Claude CLI + K8s)
[group('test')]
test-e2e-llm:
    bash hack/test-e2e-llm.sh

# Cleanup stale test namespaces
[group('test')]
test-cleanup:
    @echo "Deleting stale platform-test-* namespaces..."
    @kubectl get namespaces -o name | grep '^namespace/platform-test-' | xargs -r kubectl delete --wait=false

# All tests except LLM (unit + integration + e2e)
[group('test')]
test-all: test-unit test-integration test-e2e

# -- Crate Tests ----------------------------------------------------

# All workspace crates (package names from Cargo.toml)
_crate_all := "-p platform-types -p platform-auth -p platform-observe -p platform-secrets -p platform-k8s -p platform-git -p platform-registry -p platform-agent -p platform-ingest -p platform-k8s-watcher -p platform-proxy -p platform-proxy-init"
# Crates with --lib targets for coverage (proxy included — coverage uses --lib to avoid binary crash)
# Excludes proxy-init and ingest (no lib.rs, binary-only crates)
_crate_lib := "-p platform-types -p platform-auth -p platform-observe -p platform-secrets -p platform-k8s -p platform-git -p platform-registry -p platform-agent -p platform-k8s-watcher -p platform-proxy"
# Crates with integration tests (need DB + Valkey from .env.dev)
_crate_int := "-p platform-auth -p platform-observe -p platform-secrets -p platform-types -p platform-registry"
# K8s integration tests use `#[ignore = "requires K8s"]` — scanned across ALL crates
# (--run-ignored ignored-only is the filter, not the package list)
_crate_k8s := _crate_all

# Unit tests for workspace crates
# just crate-test-unit                        → all crates
# just crate-test-unit platform-auth          → one crate
# just crate-test-unit platform-proxy parse   → one crate + filter
[group('crate')]
crate-test-unit crate="" filter="":
    cargo nextest run \
        {{ if crate != "" { "-p " + crate } else { _crate_all } }} \
        --lib \
        {{ if filter != "" { "-E 'test(" + filter + ")'" } else { "" } }}

# Integration tests for workspace crates (uses DB + Valkey from .env.dev)
# just crate-test-integration                     → all crates with int tests
# just crate-test-integration platform-auth       → one crate
# just crate-test-integration platform-auth rate  → one crate + filter
[group('crate')]
crate-test-integration crate="" filter="":
    cargo nextest run \
        {{ if crate != "" { "-p " + crate } else { _crate_int } }} \
        --test '*' \
        {{ if filter != "" { "-E 'test(" + filter + ")'" } else { "" } }}

# K8s integration tests for workspace crates (needs Kind cluster)
# just crate-test-kubernetes                        → all K8s crates
# just crate-test-kubernetes platform-k8s           → one crate
# just crate-test-kubernetes platform-k8s ensure    → one crate + filter
[group('crate')]
crate-test-kubernetes crate="" filter="":
    cargo nextest run \
        {{ if crate != "" { "-p " + crate } else { _crate_k8s } }} \
        --test '*' \
        --run-ignored ignored-only \
        {{ if filter != "" { "-E 'test(" + filter + ")'" } else { "" } }}

# All crate tests (unit + integration + kubernetes)
# Crates without integration tests only run unit tests
[group('crate')]
crate-test-all crate="":
    just crate-test-unit {{crate}}
    {{ if crate == "platform-proxy" { "" } else if crate == "platform-proxy-init" { "" } else if crate == "platform-ingest" { "" } else if crate == "platform-k8s-watcher" { "" } else if crate == "platform-k8s" { "" } else if crate == "platform-git" { "" } else { "just crate-test-integration " + crate } }}
    just crate-test-kubernetes

# Coverage for workspace crates (unit + integration + K8s, uses DB/Valkey/K8s)
# Excludes binary-only crates without lib.rs (proxy-init, ingest)
# just crate-cov                         → all library crates
# just crate-cov platform-auth           → one crate
[group('crate')]
crate-cov crate="":
    cargo llvm-cov nextest \
        {{ if crate != "" { "-p " + crate } else { _crate_lib } }} \
        --lib --test '*' --run-ignored all \
        --lcov --output-path crate-coverage.lcov
    @echo "Coverage written to crate-coverage.lcov"

# Coverage HTML report for workspace crates
# just crate-cov-html                    → all library crates
# just crate-cov-html platform-auth      → one crate
[group('crate')]
crate-cov-html crate="":
    cargo llvm-cov nextest \
        {{ if crate != "" { "-p " + crate } else { _crate_lib } }} \
        --lib --test '*' --run-ignored all \
        --html --output-dir crate-coverage-html
    @echo "Open crate-coverage-html/index.html"

# -- Coverage -------------------------------------------------------
[group('coverage')]
cov-unit:
    cargo llvm-cov nextest --lib --lcov --output-path coverage-unit.lcov \
        --ignore-filename-regex '(proto\.rs|ui\.rs)'

[group('coverage')]
cov-integration:
    bash {{test_script}} --filter '*_integration' --coverage --lcov coverage-integration.lcov

[group('coverage')]
cov-e2e:
    bash {{test_script}} --type e2e --coverage --lcov coverage-e2e.lcov

# Combined: unit + integration (default coverage target)
[group('coverage')]
cov-total:
    @echo "=== Combined coverage: unit + integration ==="
    bash {{test_script}} --type total

# Diff coverage: only lines changed vs a branch
[group('coverage')]
cov-diff branch="main":
    bash {{test_script}} --type total --lcov coverage-total.lcov
    diff-cover coverage-total.lcov --compare-branch={{branch}} --show-uncovered

# Diff coverage strict: fail if changed lines < 100% covered
[group('coverage')]
cov-diff-check branch="main":
    bash {{test_script}} --type total --lcov coverage-total.lcov
    diff-cover coverage-total.lcov --compare-branch={{branch}} --show-uncovered --fail-under=100

[group('coverage')]
cov-html:
    cargo llvm-cov nextest --lib --html --output-dir coverage-html \
        --ignore-filename-regex '(proto\.rs|ui\.rs)'
    @echo "Open coverage-html/index.html"

[group('coverage')]
cov-summary:
    @echo "=== Unit ==="
    @cargo llvm-cov nextest --lib --ignore-filename-regex '(proto\.rs|ui\.rs)' 2>&1 | tail -3

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
    cargo sqlx prepare
    cd crates/foundation/platform-types && cargo sqlx prepare
    cd crates/libs/platform-auth && cargo sqlx prepare
    cd crates/libs/platform-observe && cargo sqlx prepare
    cd crates/libs/platform-secrets && cargo sqlx prepare
    cd crates/libs/platform-agent && cargo sqlx prepare

[group('db')]
db-check:
    cargo sqlx prepare --check
    cd crates/foundation/platform-types && cargo sqlx prepare --check
    cd crates/libs/platform-auth && cargo sqlx prepare --check
    cd crates/libs/platform-observe && cargo sqlx prepare --check
    cd crates/libs/platform-secrets && cargo sqlx prepare --check
    cd crates/libs/platform-agent && cargo sqlx prepare --check

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

# -- Deploy to cluster ----------------------------------------------
[group('build')]
deploy-local tag="platform:dev":
    just docker {{ tag }}
    kind load docker-image {{ tag }} --name platform
    kubectl apply -k deploy/dev
    kubectl rollout status deployment/platform -n platform --timeout=60s

# -- Full CI locally ------------------------------------------------
ci: fmt lint deny test-unit cli::lint cli::test mcp::test test-integration build
    @echo "All checks passed"

ci-full: fmt lint deny test-unit cli::lint cli::test mcp::test test-integration test-e2e build
    @echo "All checks passed (including E2E tests)"
