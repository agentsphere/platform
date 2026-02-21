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
ci: fmt lint deny test-unit build
    @echo "All checks passed"
