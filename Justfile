# platform/Justfile

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

test-integration:
    cargo nextest run --test '*_integration'

test-e2e:
    @echo "Requires Kind cluster: just cluster-up"
    cargo nextest run --test 'e2e_*' --run-ignored ignored-only --test-threads 2

# -- Coverage -------------------------------------------------------
cov-unit:
    cargo llvm-cov nextest --lib --lcov --output-path coverage-unit.lcov \
        --ignore-filename-regex '(proto\.rs|ui\.rs)'

cov-integration:
    cargo llvm-cov nextest --test '*_integration' --lcov --output-path coverage-integration.lcov \
        --ignore-filename-regex '(proto\.rs|ui\.rs)'

cov-e2e:
    @echo "Requires Kind cluster: just cluster-up"
    cargo llvm-cov nextest --test 'e2e_*' --run-ignored ignored-only --test-threads 2 \
        --lcov --output-path coverage-e2e.lcov \
        --ignore-filename-regex '(proto\.rs|ui\.rs)'

cov-all:
    cargo llvm-cov nextest --lcov --output-path coverage-all.lcov \
        --ignore-filename-regex '(proto\.rs|ui\.rs)'

cov-total:
    @echo "=== Combined coverage: unit + integration + E2E ==="
    cargo llvm-cov clean --workspace
    cargo llvm-cov nextest --no-report \
        --lib --test '*_integration' --test 'e2e_*' \
        --run-ignored all --test-threads 2 --no-fail-fast
    cargo llvm-cov report --ignore-filename-regex '(proto\.rs|ui\.rs|main\.rs)'

cov-html:
    cargo llvm-cov nextest --lib --html --output-dir coverage-html \
        --ignore-filename-regex '(proto\.rs|ui\.rs)'
    @echo "Open coverage-html/index.html"

cov-summary:
    @echo "=== Unit ==="
    @cargo llvm-cov nextest --lib --ignore-filename-regex '(proto\.rs|ui\.rs)' 2>&1 | tail -3
    @echo ""
    @echo "=== Integration ==="
    @cargo llvm-cov nextest --test '*_integration' --ignore-filename-regex '(proto\.rs|ui\.rs)' 2>&1 | tail -3

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
ci: fmt lint deny test-unit test-integration build
    @echo "All checks passed"

ci-full: fmt lint deny test-unit test-integration test-e2e build
    @echo "All checks passed (including E2E tests)"
