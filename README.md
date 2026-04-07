# asp — Agentic DevOps

A single Rust binary that gives AI coding agents (and the humans who supervise them) everything they need to ship software — git hosting, CI/CD, observability, secrets, and more.

Built by Steven Hooker. Officially backed and distributed by AgentSphere GmbH.

## Capabilities

- **Git hosting** — smart HTTP + SSH server, LFS, file browser, bare repo management
- **Project management** — issues, merge requests, code review, webhooks, workspaces
- **CI/CD pipelines** — `.platform.yaml` definitions, Kubernetes pod execution, log streaming
- **Continuous deployment** — GitOps reconciler, Kustomize/Helm rendering, preview environments
- **Service mesh** — process-wrapper proxy with automatic mTLS between in-cluster services
- **TLS termination** — ACME certificate provisioning and external TLS for gateway ingress
- **AI agent sessions** — ephemeral agent users, scoped identity, Claude CLI integration
- **Observability** — OTLP ingest (traces, logs, metrics), Parquet cold storage, alerting
- **Secrets management** — AES-256-GCM encrypted secrets, scoped access, injection into pipelines
- **Notifications** — email (SMTP), webhooks (HMAC-signed), in-app alerts
- **Auth & RBAC** — sessions, API tokens, passkeys (WebAuthn), roles, permissions, time-bounded delegation
- **Container registry** — OCI-compliant push, pull, and manifest management
- **Guided onboarding** — first-run setup, demo projects, CLI auth flow
- **Web UI** — embedded Preact SPA for dashboards, project detail, observability views

**Infrastructure dependencies**: PostgreSQL, Valkey, MinIO, Kubernetes.

## Architecture

15 modules in a single crate (~72K LOC):

```
src/
├── auth/        — password hashing, sessions, API tokens, passkeys
├── rbac/        — roles, permissions, time-bounded delegation
├── api/         — HTTP handlers (Axum)
├── git/         — git smart HTTP, SSH, LFS, file browser
├── pipeline/    — .platform.yaml parsing, K8s pod execution, log streaming
├── deployer/    — continuous reconciliation (desired vs current state)
├── agent/       — session lifecycle, ephemeral agent users, Claude CLI
├── observe/     — OTLP ingest, Parquet storage, queries, alerts
├── secrets/     — AES-256-GCM encryption, scoped access, request flow
├── notify/      — email (lettre), webhooks, in-app notifications
├── store/       — Postgres pool, Valkey client, MinIO operator, K8s client
├── registry/    — OCI container registry (push, pull, manifests)
├── workspace/   — workspace management and membership
├── onboarding/  — first-run setup, demo projects, Claude CLI auth
└── ui           — embedded Preact SPA (rust-embed)
```

See `docs/architecture.md` for the full architecture.

## Tech Stack

- **Language**: Rust (edition 2024), TypeScript (Preact SPA)
- **HTTP**: Axum 0.8 + Tower middleware
- **Database**: PostgreSQL + SQLx 0.8 (compile-time checked queries)
- **Cache/Pub-Sub**: Valkey via Fred
- **Object Storage**: MinIO via OpenDAL
- **Kubernetes**: kube-rs
- **Observability**: OTEL protobuf (prost), Arrow/Parquet for cold storage
- **Auth**: Argon2 (passwords), AES-GCM (secrets), SHA2 (hashing)
- **UI**: Preact SPA embedded via rust-embed

## Prerequisites

- Rust (latest stable, edition 2024)
- [just](https://github.com/casey/just) command runner
- Docker
- Node.js (for UI build)
- PostgreSQL, Valkey, MinIO (or use `just cluster-up` for a local kind cluster)

## Getting Started

1. **Clone and configure**:
   ```bash
   git clone https://github.com/agentsphere/platform.git
   cd platform
   cp .env.example .env
   # Edit .env with your connection details
   ```

2. **Start local infrastructure** (optional — sets up kind + Postgres + Valkey + MinIO):
   ```bash
   just cluster-up
   ```

3. **Run database migrations**:
   ```bash
   just db-migrate
   ```

4. **Run the server**:
   ```bash
   just run
   ```

5. **Development workflow**:
   ```bash
   just watch    # file watcher with cargo check on save
   ```

## Commands

```bash
just watch              # bacon file watcher
just run                # cargo run
just fmt                # cargo fmt
just lint               # cargo clippy --all-features -- -D warnings
just deny               # cargo deny check
just check              # fmt + lint + deny
just test               # cargo nextest run (all tests)
just test-unit          # unit tests only (no DB)
just test-integration   # integration tests (ephemeral cluster services)
just test-e2e           # E2E tests (ephemeral cluster services)
just ui test            # Playwright browser tests (requires running server)
just test-doc           # doc tests
just test-cleanup       # delete stale test namespaces
just types              # regenerate TypeScript types from Rust (ts-rs)
just db-add <name>      # create new migration
just db-migrate         # apply migrations
just db-revert          # revert last migration
just db-prepare         # regenerate .sqlx/ offline cache
just build              # UI + release build
just docker <tag>       # docker build
just ci                 # full local CI: fmt lint deny test-unit test-integration build
just ci-full            # ci + E2E tests (the full verification suite)
just cov-unit           # unit test coverage
just cov-integration    # integration test coverage (ephemeral cluster services)
just cov-total          # combined coverage: unit + integration + E2E
just cov-html           # unit coverage as HTML report
just cluster-up         # create kind cluster + Postgres + Valkey + MinIO
just cluster-down       # destroy kind cluster
```

## Testing

Three-tier testing pyramid:

| Tier | Runtime | Requires | Command |
|------|---------|----------|---------|
| Unit | ~1s | Nothing | `just test-unit` |
| Integration | ~2.5 min | dev cluster | `just test-integration` |
| E2E | ~2.5 min | dev cluster | `just test-e2e` |

All integration and E2E tests run against ephemeral services (Postgres, Valkey, MinIO) deployed in isolated cluster namespaces. No manual setup beyond `just cluster-up` (one-time).

```bash
just cluster-up       # one-time: create dev cluster
just ci-full          # run everything: fmt, lint, deny, unit, integration, E2E, build
```

See [`docs/testing.md`](docs/testing.md) for the full testing guide and [`docs/fe-be-testing.md`](docs/fe-be-testing.md) for frontend-backend type safety testing.

## Configuration

Configuration is via environment variables. See `.env.example` for defaults:

| Variable | Description | Default |
|----------|-------------|---------|
| `DATABASE_URL` | PostgreSQL connection string | — |
| `VALKEY_URL` | Valkey/Redis connection string | `redis://localhost:6379` |
| `MINIO_ENDPOINT` | MinIO S3 endpoint | `http://localhost:9000` |
| `MINIO_ACCESS_KEY` | MinIO access key | — |
| `MINIO_SECRET_KEY` | MinIO secret key | — |
| `PLATFORM_LISTEN` | HTTP listen address | `0.0.0.0:8080` |
| `PLATFORM_LOG` | Log level | `debug` |
| `PLATFORM_MASTER_KEY` | Secrets encryption key | — |
| `PLATFORM_GIT_REPOS_PATH` | Bare git repos location | — |

## License

This software is licensed under the [Business Source License 1.1](LICENSE). You may use, modify, and self-host asp freely. Providing asp to third parties as a managed or hosted service requires a commercial license from AgentSphere GmbH.

For commercial licensing inquiries, contact sales@agentsphere.cloud.
