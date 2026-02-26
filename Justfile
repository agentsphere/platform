# platform/Justfile

export DATABASE_URL := env("DATABASE_URL", "postgres://platform:dev@localhost:5432/platform_dev")
export VALKEY_URL := env("VALKEY_URL", "redis://localhost:6379")

default:
    @just --list

# -- Cluster --------------------------------------------------------
cluster-up:
    bash hack/kind-up.sh

cluster-down:
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

# Ephemeral services in Kind cluster (requires: just cluster-up)
test-integration:
    bash hack/test-in-cluster.sh --filter '*_integration'

test-e2e:
    bash hack/test-in-cluster.sh --type e2e

test-mcp:
    cd mcp && npm test

test-ui:
    @echo "Requires running server: just run"
    cd ui && npx playwright test

# Cleanup stale test namespaces
test-cleanup:
    @echo "Deleting stale test-* namespaces..."
    @kubectl get namespaces -o name | grep '^namespace/test-' | xargs -r kubectl delete --wait=false

# -- Coverage -------------------------------------------------------
cov-unit:
    cargo llvm-cov nextest --lib --lcov --output-path coverage-unit.lcov \
        --ignore-filename-regex '(proto\.rs|ui\.rs)'

# Ephemeral Kind services (requires: just cluster-up)
cov-integration:
    bash hack/test-in-cluster.sh --filter '*_integration' --coverage --lcov coverage-integration.lcov

cov-e2e:
    @echo "Requires Kind cluster: just cluster-up"
    bash hack/test-in-cluster.sh --type e2e --coverage --lcov coverage-e2e.lcov

# Combined: unit + integration + E2E (requires Kind cluster)
cov-total:
    @echo "=== Combined coverage: unit + integration + E2E ==="
    bash hack/test-in-cluster.sh --type total

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

docker tag="platform:dev":
    docker build -f docker/Dockerfile -t {{ tag }} .

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
