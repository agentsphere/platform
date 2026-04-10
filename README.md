# asp — Agentic DevOps

A platform that gives AI coding agents (and the humans who supervise them) everything they need to ship software — git hosting, CI/CD, observability, secrets, service mesh, and more. Five purpose-built binaries, zero off-the-shelf middleware.

Built by Steven Hooker. Officially backed and distributed by AgentSphere GmbH.

## Capabilities

- **Git hosting** — smart HTTP + SSH server, LFS, file browser, bare repo management
- **Project management** — issues, merge requests, code review, webhooks, workspaces
- **CI/CD pipelines** — `.platform.yaml` definitions, Kubernetes pod execution, log streaming
- **Continuous deployment** — GitOps reconciler, Kustomize/Helm rendering, preview environments
- **Service mesh** — transparent proxy with automatic mTLS (SPIFFE), replaces Envoy
- **Ingress gateway** — HTTPRoute-driven routing, ACME TLS provisioning, traffic splitting
- **AI agent sessions** — ephemeral pods with Claude CLI, MCP servers, scoped identity
- **Observability** — OTLP ingest (traces, logs, metrics), Parquet cold storage, alerting
- **Secrets management** — AES-256-GCM encrypted secrets, scoped access, injection into pipelines
- **Container registry** — OCI-compliant push, pull, manifest management, backed by MinIO
- **Auth & RBAC** — sessions, API tokens, passkeys (WebAuthn), roles, delegation, SPIFFE mesh CA
- **Web UI** — embedded Preact SPA for dashboards, project detail, observability views

## Architecture

```
┌────────────────────────────────────────────────────────────────────────────┐
│                           Kubernetes Cluster                               │
│                                                                            │
│  ┌──────────────────────────────────────────────────────────────────────┐  │
│  │                     PLATFORM (control plane)                         │  │
│  │               Rust binary — Axum HTTP + SSH server                   │  │
│  │                                                                      │  │
│  │  API · Git · Pipelines · Deployer · Agents · Observe · Registry     │  │
│  │  Auth/RBAC · Secrets · Notify · Mesh CA · Gateway controller        │  │
│  │  Preact SPA (embedded) · Background tasks (9 loops)                 │  │
│  └─────┬──────────┬──────────┬──────────────────────────────────────────┘  │
│        │          │          │                                              │
│  ┌─────▼───┐ ┌───▼────┐ ┌──▼─────┐                                       │
│  │Postgres │ │ Valkey  │ │ MinIO  │                                       │
│  │ (state) │ │ (cache, │ │ (blobs,│                                       │
│  │         │ │ pub/sub)│ │ images)│                                       │
│  └─────────┘ └────────┘ └────────┘                                       │
│                                                                            │
│  ┌──────────────────────────────────────────────────────────────────────┐  │
│  │                     GATEWAY (ingress)                                │  │
│  │               platform-proxy --gateway                               │  │
│  │                                                                      │  │
│  │  HTTPRoute CRD watcher · TLS termination (ACME + mesh CA)          │  │
│  │  Host/path routing · Rate limiting · Connection pooling              │  │
│  └──────────────────────────────────────────────────────────────────────┘  │
│                                                                            │
│  ┌──────────────────────────────────────────────────────────────────────┐  │
│  │                     AGENT PODS (per session)                         │  │
│  │                                                                      │  │
│  │  init: git-clone → setup (MCP + Claude CLI) → proxy-init (iptables) │  │
│  │  ┌──────────────┐  ┌─────────────┐  ┌─────────────────────────────┐ │  │
│  │  │ agent-runner  │  │  browser    │  │  platform-proxy (sidecar)   │ │  │
│  │  │ Claude CLI    │  │  Playwright │  │  mTLS · logs · metrics      │ │  │
│  │  │ MCP servers   │  │  (optional) │  │  transparent interception   │ │  │
│  │  └──────────────┘  └─────────────┘  └─────────────────────────────┘ │  │
│  └──────────────────────────────────────────────────────────────────────┘  │
│                                                                            │
│  ┌──────────────────────────────────────────────────────────────────────┐  │
│  │                     PIPELINE PODS (per step)                         │  │
│  │                                                                      │  │
│  │  init: git-clone · Kaniko (image builds)                            │  │
│  │  Shell commands · Artifact upload · Log streaming                    │  │
│  └──────────────────────────────────────────────────────────────────────┘  │
│                                                                            │
│  ┌──────────────────────────────────────────────────────────────────────┐  │
│  │                     DEPLOYED WORKLOADS                                │  │
│  │                                                                      │  │
│  │  Per-project namespaces · Preview envs (branch-scoped, TTL)         │  │
│  │  ┌────────────────────────────────────────────────────────────────┐  │  │
│  │  │ app container → platform-proxy --wrap (sidecar)                │  │  │
│  │  │                 mTLS · stdout/stderr capture · process metrics  │  │  │
│  │  └────────────────────────────────────────────────────────────────┘  │  │
│  └──────────────────────────────────────────────────────────────────────┘  │
└────────────────────────────────────────────────────────────────────────────┘
```

### Binaries

| Binary | Crate | Image | Role |
|--------|-------|-------|------|
| **platform** | `src/` | `platform` | Control plane — API, git, background tasks, embedded UI |
| **agent-runner** | `cli/agent-runner/` | `platform-runner` | Runs inside agent pods — wraps Claude CLI, manages MCP servers, Valkey pub/sub |
| **platform-proxy** | `crates/proxy/` | `platform-proxy` | Mesh sidecar (mTLS, logs, metrics), process wrapper, or ingress gateway |
| **proxy-init** | `crates/proxy-init/` | `platform-proxy-init` | Init container — copies proxy binary, sets up iptables redirect rules |
| **claude-mock** | `cli/claude-mock/` | — | Test harness — mocks Claude CLI for integration tests |

### Infrastructure

| Service | Role | Accessed via |
|---------|------|--------------|
| **PostgreSQL** | Primary state — users, projects, pipelines, deployments, observability | SQLx (compile-time checked) |
| **Valkey** | Cache (permissions, rate limits), pub/sub (agent sessions), per-session ACLs | Fred client pool |
| **MinIO** | Object storage — OCI image blobs, Parquet files, pipeline artifacts, Git LFS | OpenDAL (S3 API) |
| **Kubernetes** | Pod orchestration — agent sessions, pipeline steps, deployer workloads, gateway | kube-rs |

### Spawned Pods

The platform dynamically creates and manages several types of Kubernetes pods:

- **Agent pods** — one per session. Init containers clone the repo, install MCP servers and Claude CLI, and configure iptables for transparent proxy. Main container runs the agent-runner binary wrapping Claude Code. Optional browser sidecar (Playwright) for UI/test roles. Network proxy sidecar for mesh mTLS.
- **Pipeline pods** — one per pipeline step. Git clone init, then shell commands or Kaniko for image builds. Logs streamed back to platform via OTLP.
- **Gateway pod** — single deployment, auto-reconciled. Runs `platform-proxy --gateway`, watches HTTPRoute CRDs, terminates TLS (ACME or mesh CA), routes traffic by host/path.
- **Deployed workloads** — user applications managed by the GitOps reconciler. Each gets a `platform-proxy --wrap` sidecar for mTLS, log capture, and process metrics.

### MCP Servers

7 Node.js servers in `mcp/servers/` provide tool interfaces for Claude agents inside pods:

`platform-core` · `platform-admin` · `platform-issues` · `platform-pipeline` · `platform-deploy` · `platform-observe` · `platform-browser`

### Modules

15 modules in the main crate (~72K LOC):

```
src/
├── auth/        — password hashing, sessions, API tokens, passkeys, rate limiting
├── rbac/        — roles, permissions, time-bounded delegation, Valkey-cached resolution
├── api/         — 30+ HTTP handler modules (Axum), wired via .merge()
├── git/         — smart HTTP + SSH server, LFS, file browser, post-receive hooks
├── pipeline/    — .platform.yaml parsing, K8s pod execution, log streaming
├── deployer/    — GitOps reconciler, Kustomize rendering, K8s applier, preview envs
├── agent/       — session lifecycle, ephemeral identity, Claude Code provider, pod specs
├── observe/     — OTLP ingest, Parquet storage, query API, alerts, K8s event correlation
├── secrets/     — AES-256-GCM encryption engine, scoped access, request flow
├── notify/      — email (lettre SMTP), webhooks (HMAC-SHA256), in-app notifications
├── store/       — Postgres pool, Valkey pool, MinIO operator, K8s client, bootstrap
├── registry/    — OCI container registry (push, pull, manifests, GC), backed by MinIO
├── mesh/        — SPIFFE mesh CA, leaf cert issuance, ACME (Let's Encrypt), trust bundles
├── gateway/     — auto-deploys ingress gateway, reconciles Deployment + Service + RBAC
├── workspace/   — workspace management and membership
├── onboarding/  — first-run setup, demo projects, Claude CLI auth
└── ui.rs        — embedded Preact SPA (rust-embed)
```

See `docs/architecture.md` for data flows, schema overview, and background task inventory.

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
