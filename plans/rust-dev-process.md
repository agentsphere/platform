# Rust Development Process — Unified Platform

## Context

The unified platform (`plans/unified-platform.md`) replaces 8+ off-the-shelf services with a single Rust binary (~13K LOC Rust + ~2.7K LOC TypeScript). An existing Go prototype in `mgr/` (~2,600 LOC) provides reference patterns. This plan defines the development toolchain, workflow, and CI/CD pipeline so that the dev setup is efficient from day one.

The platform lives in its own GitHub repository (open source), not as a subfolder of the infra repo. Development happens on macOS (Apple Silicon). CI runs on GitHub Actions with GitHub Enterprise runners. Container images are pushed to GitHub Container Registry (ghcr.io). Local dev uses a kind cluster for Kubernetes integration — no external cluster dependencies. Single developer + Claude Code.

---

## 0. Prerequisites

### Must exist before starting

| Prerequisite | Notes |
|---|---|
| GitHub repository | Created at `github.com/agentsphere/platform` (or org of choice), public, MIT/Apache-2.0 license |
| `plans/unified-platform.md` | Architectural blueprint — module structure, crate decisions, data model |
| Go prototype `mgr/` | Reference for migration SQL, UI code, pod exec patterns (stays in infra repo, read-only reference) |

### Local tooling (one-time install)

Everything the developer needs. Nothing else is assumed pre-installed beyond macOS + Homebrew.

```bash
# Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup component add clippy rustfmt

# Cargo extensions
cargo install cargo-nextest --locked
cargo install cargo-deny --locked
cargo install bacon --locked
cargo install sqlx-cli --no-default-features --features rustls,postgres --locked
cargo install just --locked

# Container + cluster tooling
brew install docker           # or Docker Desktop / OrbStack
brew install kind             # local K8s clusters
brew install kubectl          # cluster interaction
brew install helm             # for installing CNPG operator, Valkey, etc. into kind

# Node (for UI build)
brew install node@22          # or via nvm

# Pre-commit (for local hooks)
brew install pre-commit
```

No `sccache` locally — incremental compilation + lld on macOS is fast enough for a single-crate project.

---

## 1. Repository Structure

The platform is a standalone repository. Fully self-contained — no references to parent repos, external pre-commit configs, or pre-existing clusters.

```
platform/                        # repo root
  Cargo.toml                     # single crate manifest
  Cargo.lock                     # committed (binary project)
  .cargo/config.toml             # linker, profiles
  .config/nextest.toml           # test runner
  rustfmt.toml                   # formatter
  deny.toml                      # dependency audit
  Justfile                       # task runner
  .pre-commit-config.yaml        # self-contained hooks (not inherited)
  .sqlx/                         # committed — sqlx offline query cache
  .env.example                   # template (committed), .env is gitignored
  .gitignore
  LICENSE
  src/
    main.rs                      # binary entrypoint
    lib.rs                       # re-exports for integration tests
    config.rs
    error.rs
    auth/
    rbac/
    api/
    git/
    pipeline/
    deployer/
    agent/
    observe/
    secrets/
    notify/
    store/
  migrations/                    # sqlx migrations
  tests/                         # integration tests
  ui/                            # Preact SPA (carried from mgr/ui/)
  docker/
    Dockerfile                   # app image (multi-stage, cargo-chef)
    Dockerfile.claude-runner     # agent runtime image
  hack/                          # local dev scripts
    kind-config.yaml             # kind cluster config
    kind-up.sh                   # create cluster + install deps (Postgres, Valkey)
    kind-down.sh                 # tear down cluster
    port-forward.sh              # forward Postgres + Valkey to localhost
  deploy/                        # k8s manifests for the platform itself
    base/                        # kustomize base
      kustomization.yaml
      deployment.yaml
      service.yaml
      configmap.yaml
    dev/                         # kustomize overlay for kind
      kustomization.yaml
      patch-image.yaml
  .github/
    workflows/
      ci.yaml                   # lint, test, deny, build
      release.yaml               # build + push image + deploy
    dependabot.yml               # automated dependency updates
```

**Single crate, not workspace.** 11 Rust modules at ~500-2200 LOC each — one coherent binary. If compilation times grow past ~30s for `cargo check`, split `observe/` (the largest module) into a separate crate then.

---

## 2. Toolchain & Configuration

### `.cargo/config.toml`

```toml
# macOS Apple Silicon — lld is the default linker on Rust 1.85+
# No explicit linker override needed on macOS

[target.x86_64-unknown-linux-gnu]
linker = "clang"
rustflags = ["-C", "link-arg=-fuse-ld=mold"]

[profile.dev.package."*"]
# Optimize deps (sqlx proc macros, serde_derive) even in dev
opt-level = 2

[profile.release]
opt-level = 3
lto = "thin"
codegen-units = 1   # maximizes optimization but makes release builds slow (~2-3x); CI-only concern
strip = "symbols"
panic = "abort"
```

The `[profile.dev.package."*"]` setting is the single most impactful change — it makes `cargo check` on sqlx projects 3-5x faster after first build. Note: `codegen-units = 1` in the release profile significantly slows `cargo build --release` — this is fine since release builds only happen in CI/Docker, never in the local dev loop.

### `.env.example` (committed) / `.env` (gitignored)

```bash
DATABASE_URL=postgres://platform:dev@localhost:5432/platform_dev
VALKEY_URL=redis://localhost:6379
MINIO_ENDPOINT=http://localhost:9000
MINIO_ACCESS_KEY=platform
MINIO_SECRET_KEY=devdevdev
PLATFORM_LOG=debug                  # tracing filter — see observe module in unified-platform.md
PLATFORM_LISTEN=0.0.0.0:8080
KUBECONFIG=${HOME}/.kube/kind-platform
```

---

## 3. Local Kubernetes — kind

No pre-existing cluster required. Everything spins up locally via kind.

### `hack/kind-config.yaml`

```yaml
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
  - role: control-plane
    extraPortMappings:
      - containerPort: 30080
        hostPort: 8080        # platform service (NodePort)
      - containerPort: 30432
        hostPort: 5432        # postgres
      - containerPort: 30379
        hostPort: 6379        # valkey
      - containerPort: 30900
        hostPort: 9000        # minio (S3 API)
      - containerPort: 30901
        hostPort: 9001        # minio (console)
```

### `hack/kind-up.sh`

```bash
#!/usr/bin/env bash
set -euo pipefail

CLUSTER_NAME="platform"

# Create cluster if it doesn't exist
if ! kind get clusters 2>/dev/null | grep -q "^${CLUSTER_NAME}$"; then
  kind create cluster --name "$CLUSTER_NAME" --config hack/kind-config.yaml
fi

# Export kubeconfig
kind get kubeconfig --name "$CLUSTER_NAME" > "${HOME}/.kube/kind-platform"
export KUBECONFIG="${HOME}/.kube/kind-platform"

# Install CNPG operator
helm repo add cnpg https://cloudnative-pg.github.io/charts --force-update
helm upgrade --install cnpg cnpg/cloudnative-pg -n cnpg-system --create-namespace --wait

# Create platform namespace
kubectl create namespace platform --dry-run=client -o yaml | kubectl apply -f -

# Postgres cluster (single instance, ephemeral for dev)
kubectl apply -n platform -f - <<'EOF'
apiVersion: postgresql.cnpg.io/v1
kind: Cluster
metadata:
  name: platform-db
spec:
  instances: 1
  storage:
    size: 1Gi
  bootstrap:
    initdb:
      database: platform_dev
      owner: platform
      secret:
        name: platform-db-creds
---
apiVersion: v1
kind: Secret
metadata:
  name: platform-db-creds
type: kubernetes.io/basic-auth
stringData:
  username: platform
  password: dev
---
apiVersion: v1
kind: Service
metadata:
  name: platform-db-external
spec:
  type: NodePort
  selector:
    cnpg.io/cluster: platform-db
    role: primary
  ports:
    - port: 5432
      targetPort: 5432
      nodePort: 30432
EOF

# Valkey (standalone, minimal)
kubectl apply -n platform -f - <<'EOF'
apiVersion: apps/v1
kind: Deployment
metadata:
  name: valkey
spec:
  replicas: 1
  selector:
    matchLabels:
      app: valkey
  template:
    metadata:
      labels:
        app: valkey
    spec:
      containers:
        - name: valkey
          image: valkey/valkey:8-alpine
          ports:
            - containerPort: 6379
          resources:
            requests:
              cpu: 50m
              memory: 64Mi
---
apiVersion: v1
kind: Service
metadata:
  name: valkey-external
spec:
  type: NodePort
  selector:
    app: valkey
  ports:
    - port: 6379
      targetPort: 6379
      nodePort: 30379
EOF

# MinIO (standalone, ephemeral for dev)
kubectl apply -n platform -f - <<'EOF'
apiVersion: apps/v1
kind: Deployment
metadata:
  name: minio
spec:
  replicas: 1
  selector:
    matchLabels:
      app: minio
  template:
    metadata:
      labels:
        app: minio
    spec:
      containers:
        - name: minio
          image: minio/minio:latest
          args: ["server", "/data", "--console-address", ":9001"]
          env:
            - name: MINIO_ROOT_USER
              value: platform
            - name: MINIO_ROOT_PASSWORD
              value: devdevdev
          ports:
            - containerPort: 9000
            - containerPort: 9001
          resources:
            requests:
              cpu: 50m
              memory: 128Mi
---
apiVersion: v1
kind: Service
metadata:
  name: minio-external
spec:
  type: NodePort
  selector:
    app: minio
  ports:
    - name: api
      port: 9000
      targetPort: 9000
      nodePort: 30900
    - name: console
      port: 9001
      targetPort: 9001
      nodePort: 30901
EOF

# OTel Collector (minimal, forwards OTLP to platform ingest endpoint)
kubectl apply -n platform -f - <<'EOF'
apiVersion: v1
kind: ConfigMap
metadata:
  name: otel-collector-config
data:
  config.yaml: |
    receivers:
      otlp:
        protocols:
          grpc:
            endpoint: 0.0.0.0:4317
          http:
            endpoint: 0.0.0.0:4318
    exporters:
      otlphttp:
        endpoint: http://platform:8080/v1
        tls:
          insecure: true
      debug:
        verbosity: basic
    service:
      pipelines:
        traces:
          receivers: [otlp]
          exporters: [otlphttp, debug]
        logs:
          receivers: [otlp]
          exporters: [otlphttp, debug]
        metrics:
          receivers: [otlp]
          exporters: [otlphttp, debug]
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: otel-collector
spec:
  replicas: 1
  selector:
    matchLabels:
      app: otel-collector
  template:
    metadata:
      labels:
        app: otel-collector
    spec:
      containers:
        - name: collector
          image: otel/opentelemetry-collector-contrib:latest
          args: ["--config=/etc/otel/config.yaml"]
          ports:
            - containerPort: 4317
            - containerPort: 4318
          volumeMounts:
            - name: config
              mountPath: /etc/otel
          resources:
            requests:
              cpu: 50m
              memory: 64Mi
      volumes:
        - name: config
          configMap:
            name: otel-collector-config
---
apiVersion: v1
kind: Service
metadata:
  name: otel-collector
spec:
  selector:
    app: otel-collector
  ports:
    - name: grpc
      port: 4317
      targetPort: 4317
    - name: http
      port: 4318
      targetPort: 4318
EOF

echo "Waiting for Postgres to be ready..."
kubectl wait --for=condition=Ready cluster/platform-db -n platform --timeout=120s

echo ""
echo "Dev cluster ready."
echo "  Postgres: localhost:5432 (platform/dev)"
echo "  Valkey:   localhost:6379"
echo "  MinIO:    localhost:9000 (S3 API), localhost:9001 (console)"
echo "            credentials: platform / devdevdev"
echo "  export KUBECONFIG=${HOME}/.kube/kind-platform"
```

### `hack/kind-down.sh`

```bash
#!/usr/bin/env bash
kind delete cluster --name platform
rm -f "${HOME}/.kube/kind-platform"
```

### `hack/port-forward.sh`

Alternative to NodePort if port conflicts occur:

```bash
#!/usr/bin/env bash
export KUBECONFIG="${HOME}/.kube/kind-platform"
kubectl port-forward -n platform svc/platform-db-rw 5432:5432 &
kubectl port-forward -n platform svc/valkey-external 6379:6379 &
echo "Port-forwarding active. Ctrl-C to stop."
wait
```

---

## 4. Task Runner — `Justfile`

`just` over `make` — cleaner argument passing, no tab sensitivity, better output.

```just
# platform/Justfile

default:
    @just --list

# ── Cluster ─────────────────────────────────────────────────
cluster-up:
    bash hack/kind-up.sh

cluster-down:
    bash hack/kind-down.sh

# ── Dev ──────────────────────────────────────────────────────
watch:
    bacon

run:
    cargo run

ui:
    cd ui && npm run build

# ── Quality ──────────────────────────────────────────────────
fmt:
    cargo fmt

lint:
    cargo clippy --all-features -- -D warnings

deny:
    cargo deny check

check: fmt lint deny

# ── Test ─────────────────────────────────────────────────────
test:
    cargo nextest run

test-unit:
    cargo nextest run --lib

test-doc:
    cargo test --doc

# ── Database ─────────────────────────────────────────────────
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

# ── Build ────────────────────────────────────────────────────
build:
    just ui
    SQLX_OFFLINE=true cargo build --release

docker tag="platform:dev":
    docker build -f docker/Dockerfile -t {{ tag }} .

# ── Deploy to kind ──────────────────────────────────────────
deploy-local tag="platform:dev":
    just docker {{ tag }}
    kind load docker-image {{ tag }} --name platform
    kubectl apply -k deploy/dev
    kubectl rollout status deployment/platform -n platform --timeout=60s

# ── Full CI locally ──────────────────────────────────────────
ci: fmt lint deny test-unit db-check build
    @echo "All checks passed"
```

---

## 5. Code Quality

### `rustfmt.toml`

```toml
edition = "2024"
max_width = 100
imports_granularity = "Module"
group_imports = "StdExternalCrate"
```

### Clippy

Configured in `Cargo.toml` via `[lints]` (no separate file):

```toml
[lints.rust]
unsafe_code = "forbid"

[lints.clippy]
pedantic = { level = "warn", priority = -1 }
module_name_repetitions = "allow"
missing_errors_doc = "allow"
missing_panics_doc = "allow"
must_use_candidate = "allow"
```

CI command: `cargo clippy --all-features -- -D warnings`

### `deny.toml`

```toml
[advisories]
version = 2

[licenses]
version = 2
allow = ["MIT", "Apache-2.0", "BSD-2-Clause", "BSD-3-Clause",
         "ISC", "Unicode-3.0", "MPL-2.0", "OpenSSL"]

[bans]
multiple-versions = "warn"
wildcards = "deny"
deny = [{ name = "openssl" }, { name = "openssl-sys" }]

[sources]
unknown-registry = "deny"
unknown-git = "deny"
```

Ban `openssl`/`openssl-sys` — use `rustls` everywhere for simpler builds and smaller images.

### `.pre-commit-config.yaml` (self-contained, in repo root)

```yaml
fail_fast: true
repos:
  - repo: https://github.com/pre-commit/pre-commit-hooks
    rev: v5.0.0
    hooks:
      - id: trailing-whitespace
      - id: end-of-file-fixer
      - id: check-yaml
      - id: check-toml
      - id: check-merge-conflict
      - id: detect-private-key

  - repo: https://github.com/gitleaks/gitleaks
    rev: v8.22.1
    hooks:
      - id: gitleaks

  - repo: local
    hooks:
      - id: rust-fmt
        name: rustfmt
        language: system
        entry: cargo fmt --check
        types: [rust]
        pass_filenames: false

      - id: rust-clippy
        name: clippy
        language: system
        entry: cargo clippy --all-features -- -D warnings
        types: [rust]
        pass_filenames: false
```

Keep it lean — `cargo-deny` and `sqlx prepare --check` run in CI, not pre-commit (too slow).

### `bacon.toml`

Defaults are good — no custom config needed. Bacon auto-detects the project and runs `cargo check` on save. Press `t` to switch to `cargo nextest run`, `c` to switch back to check.

---

## 6. Testing Strategy

### Unit tests

Inline `#[cfg(test)] mod tests` in source files. No external test files for unit tests.

### Integration tests

`tests/` directory. Use `#[sqlx::test]` for database tests — it creates a temporary database per test function, applies migrations, and drops it after:

```rust
#[sqlx::test(migrations = "migrations")]
async fn user_crud(pool: PgPool) {
    // pool is a fresh database with all migrations applied
}
```

### `.config/nextest.toml`

```toml
[profile.default]
failure-output = "immediate-final"
success-output = "never"

[profile.ci]
retries = 1
failure-output = "immediate"
```

### Running tests

```bash
just test-unit    # fast, no database
just test         # all tests including integration (needs DATABASE_URL)
just test-doc     # doctests
bacon nextest     # watch mode — press 't' in bacon
```

---

## 7. Database Workflow

### sqlx approach

- Migrations in `migrations/` using sqlx's timestamp-prefixed format
- `cargo sqlx prepare` generates `.sqlx/` directory (committed) for offline CI builds
- `SQLX_OFFLINE=true` in CI — no live database needed to compile
- Locally: `DATABASE_URL` points to kind Postgres (localhost:5432) for compile-time query checking

### Day-to-day

```bash
just db-add create_users_table   # creates migration file
# edit the migration SQL
just db-migrate                   # apply to local database
just db-prepare                   # regenerate .sqlx/ offline cache
# commit .sqlx/ changes with your code
```

---

## 8. Docker Build

Multi-stage Dockerfile with `cargo-chef` for dependency layer caching. 4 stages: UI build, chef planner, builder (deps + binary), distroless runtime:

```dockerfile
# Stage 1: UI
FROM node:22-slim AS ui-builder
WORKDIR /ui
COPY ui/package.json ui/package-lock.json* ./
RUN npm ci --ignore-scripts
COPY ui/ .
RUN npm run build

# Stage 2: Dependency recipe
FROM rust:1.85-slim-bookworm AS planner
RUN cargo install cargo-chef --locked
WORKDIR /app
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Stage 3: Build deps + binary (single builder — chef cooks deps, then builds source)
FROM rust:1.85-slim-bookworm AS builder
RUN apt-get update && apt-get install -y mold && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-chef --locked
WORKDIR /app
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
# ↑ deps are cached here — source changes below don't rebuild deps
COPY . .
COPY --from=ui-builder /ui/dist ./ui/dist
ENV SQLX_OFFLINE=true
RUN cargo build --release

# Stage 4: Runtime
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /app/target/release/platform /platform
EXPOSE 8080
ENTRYPOINT ["/platform"]
```

`distroless/cc` (not `static`) because the binary uses glibc (needed by some crates). `nonroot` user for least privilege.

---

## 9. CI/CD — GitHub Actions

GitHub Enterprise runners eliminate the need for self-hosted CI infra. Images pushed to GitHub Container Registry (ghcr.io). No custom CI image needed — GitHub runners come with Rust toolchain support and Actions cache handles the rest.

### `.github/workflows/ci.yaml`

Runs on every push and PR. Parallel jobs for speed.

```yaml
name: CI

on:
  push:
    branches: [main]
  pull_request:

env:
  CARGO_TERM_COLOR: always
  SQLX_OFFLINE: "true"

jobs:
  fmt:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt
      - run: cargo fmt --check

  lint:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy
      - uses: Swatinem/rust-cache@v2
      - run: cargo clippy --all-features -- -D warnings

  test-unit:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - uses: taiki-e/install-action@nextest
      - run: cargo nextest run --lib --profile ci

  test-integration:
    runs-on: ubuntu-latest
    services:
      postgres:
        image: postgres:17
        env:
          POSTGRES_USER: platform
          POSTGRES_PASSWORD: dev
          POSTGRES_DB: platform_test
        ports:
          - 5432:5432
        options: >-
          --health-cmd pg_isready
          --health-interval 10s
          --health-timeout 5s
          --health-retries 5
    env:
      DATABASE_URL: postgres://platform:dev@localhost:5432/platform_test
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - uses: taiki-e/install-action@nextest
      - run: cargo nextest run --profile ci

  deny:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: EmbarkStudios/cargo-deny-action@v2

  build:
    runs-on: ubuntu-latest
    needs: [fmt, lint, test-unit, test-integration, deny]
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo build --release
```

### `.github/workflows/release.yaml`

Builds and pushes container image on push to `main`. Deploys via kustomize image tag update.

```yaml
name: Release

on:
  push:
    branches: [main]

env:
  REGISTRY: ghcr.io
  IMAGE_NAME: ${{ github.repository }}

jobs:
  build-and-push:
    runs-on: ubuntu-latest
    permissions:
      contents: read
      packages: write
    steps:
      - uses: actions/checkout@v4

      - uses: docker/login-action@v3
        with:
          registry: ${{ env.REGISTRY }}
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}

      - uses: docker/metadata-action@v5
        id: meta
        with:
          images: ${{ env.REGISTRY }}/${{ env.IMAGE_NAME }}
          tags: |
            type=sha,prefix=
            type=raw,value=latest

      - uses: docker/build-push-action@v6
        with:
          context: .
          file: docker/Dockerfile
          push: true
          tags: ${{ steps.meta.outputs.tags }}
          cache-from: type=gha
          cache-to: type=gha,mode=max
```

### `.github/dependabot.yml`

```yaml
version: 2
updates:
  - package-ecosystem: cargo
    directory: /
    schedule:
      interval: weekly
    groups:
      rust-deps:
        patterns: ["*"]

  - package-ecosystem: github-actions
    directory: /
    schedule:
      interval: weekly

  - package-ecosystem: npm
    directory: /ui
    schedule:
      interval: weekly
```

### Why no custom CI image

GitHub Actions + `Swatinem/rust-cache@v2` caches `~/.cargo` and `target/` between runs. Combined with purpose-built Actions (`dtolnay/rust-toolchain`, `taiki-e/install-action`, `EmbarkStudios/cargo-deny-action`), setup time is <30s per job. No image to maintain.

---

## 10. Daily Developer Workflow

### First time setup

```bash
git clone https://github.com/agentsphere/platform
cd platform
cp .env.example .env
just cluster-up          # creates kind cluster, installs Postgres + Valkey
just db-migrate          # apply migrations
pre-commit install       # install git hooks
just watch               # start developing
```

### The loop

```
1. bacon           — runs `cargo check` on save, errors in ~2s
2. Write code      — fix type errors guided by bacon
3. Press 't'       — switch bacon to test mode
4. just fmt        — auto-format before commit
5. git commit      — pre-commit runs rustfmt + clippy
6. Push            — GitHub Actions runs full CI
```

### Adding a new API endpoint

1. Add handler in `src/api/module.rs`
2. Add query in the handler using `sqlx::query_as!`
3. Wire route in `src/api/mod.rs` router
4. `just db-prepare` (if new SQL)
5. Add test in `tests/`

### Adding a new migration

1. `just db-add short_name`
2. Edit the generated SQL file
3. `just db-migrate`
4. `just db-prepare`
5. Commit the migration + `.sqlx/` changes

### Testing against kind cluster

```bash
just deploy-local        # builds image, loads into kind, applies manifests
kubectl logs -n platform deployment/platform -f
```

### Updating dependencies

1. Edit version in `Cargo.toml` `[dependencies]`
2. `cargo update -p crate_name`
3. `just ci` to verify nothing broke
4. `cargo deny check` for license/CVE audit

### Tearing down

```bash
just cluster-down        # deletes kind cluster, cleans kubeconfig
```

---

## 11. Key Crate Decisions (from research)

| Choice | Pick | Why |
|--------|------|-----|
| Async runtime | `tokio` | Only serious option (async-std discontinued March 2025) |
| Error handling | `thiserror` (types) + `anyhow` (propagation) | Consensus approach; thiserror for domain errors, anyhow in handlers |
| Valkey client | `fred` | Better connection pooling and reconnect than `redis` crate |
| Templates | `minijinja` | Smaller compile footprint than tera, active maintenance |
| Test runner | `cargo-nextest` | 60% faster than cargo test, better output |
| File watcher | `bacon` | cargo-watch is deprecated |
| Task runner | `just` | Clean args, no tab issues, better than make for this use case |
| Linker (Linux/CI) | `mold` | Fastest available; lld is default on macOS since Rust 1.85 |
| Docker cache | `cargo-chef` | Standard approach, 5x faster Docker rebuilds |

---

## 12. Files to Create (Implementation Order)

When implementation starts, create these files in order:

1. `Cargo.toml` — single crate with all dependencies + `[lints]`
2. `.cargo/config.toml` — linker + profile settings
3. `rustfmt.toml` — formatter config
4. `deny.toml` — dependency audit config
5. `.config/nextest.toml` — test runner config
6. `Justfile` — task runner (including cluster-up/down)
7. `.gitignore` — `target/`, `.env`, `ui/node_modules/`, `ui/dist/`
8. `.env.example` — template env vars
9. `.pre-commit-config.yaml` — self-contained hooks
10. `src/main.rs` + `src/lib.rs` — entrypoint skeleton
11. `migrations/` — first migration (ported from `mgr/internal/store/migrations/`)
12. `hack/kind-config.yaml` — kind cluster definition
13. `hack/kind-up.sh` — cluster bootstrap script
14. `hack/kind-down.sh` — cluster teardown
15. `hack/port-forward.sh` — alternative port forwarding
16. `deploy/base/` — kustomize base manifests
17. `deploy/dev/` — kind overlay
18. `docker/Dockerfile` — multi-stage with cargo-chef
19. `.github/workflows/ci.yaml` — CI pipeline
20. `.github/workflows/release.yaml` — image build + push
21. `.github/dependabot.yml` — automated updates
22. `LICENSE` — MIT or Apache-2.0

---

## Verification

After the dev setup is created:

1. `just cluster-up` creates kind cluster with Postgres + Valkey
2. `just db-migrate` applies migrations to local Postgres
3. `cargo check` compiles with no errors
4. `cargo test` passes (even if just a placeholder test)
5. `just ci` runs all quality checks end-to-end
6. `bacon` watches and re-checks on file save
7. `just deploy-local` builds image, loads into kind, deploys successfully
8. Pre-commit hooks catch formatting/lint issues on commit
9. `just cluster-down` cleanly tears down everything
