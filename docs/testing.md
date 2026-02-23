# Testing Guide

This document covers all testing tiers for the platform: unit, integration, and E2E.

## Overview

| Tier | Count | Runtime | Infra required | Command |
|---|---|---|---|---|
| Unit | 442 | ~1s | None | `just test-unit` |
| Integration | varies | ~30s | Postgres (Kind or local) | `just test-integration` |
| E2E | 40 | ~90s | Kind cluster (Postgres, Valkey, MinIO, K8s) | `just test-e2e` |

All tests use [cargo-nextest](https://nexte.st/) as the test runner.

## Unit Tests

**Location**: inline `#[cfg(test)] mod tests` blocks in source files under `src/`.

**What they cover**: business logic, parsers, state machines, permission resolution, encryption, validation, error mapping.

**Run**:
```bash
just test-unit          # cargo nextest run --lib
just test-doc           # cargo test --doc (doc examples)
```

**Conventions**:
- No I/O, no network, no filesystem — pure functions only.
- Use `#[test]` for sync tests, `#[tokio::test]` for async.
- Mock dependencies with in-memory structs (no external crate needed).
- `proptest` for parser/serialization round-trips.
- `insta` for API response snapshot stability.
- Keep test modules at the bottom of each source file.

**Examples of well-tested modules**:
- `src/pipeline/definition.rs` — YAML parsing, trigger matching, branch pattern matching
- `src/rbac/types.rs` — permission round-trips, serde, display
- `src/rbac/resolver.rs` — cache keys, scope matching
- `src/validation.rs` — input validation, SSRF detection, container image checks
- `src/secrets/engine.rs` — AES-256-GCM encrypt/decrypt round-trips
- `src/observe/proto.rs` — OTLP protobuf encoding/decoding, severity/span-kind mapping

## Integration Tests

**Location**: `tests/*_integration.rs` (10 files).

**What they cover**: API endpoint flows end-to-end against a real Postgres database. Auth flows, RBAC, project CRUD, issues, MRs, webhooks, notifications.

**Run**:
```bash
just test-integration   # cargo nextest run --test '*_integration'
```

**Prerequisites**:
- Postgres accessible at `DATABASE_URL` (the `#[sqlx::test]` macro creates an ephemeral database per test).
- The `platform` DB user needs `CREATEDB` permission (`ALTER USER platform CREATEDB;`).
- If using the Kind cluster: Postgres is at `localhost:5432` (user: `platform`, password: `dev`).

**Key pattern — `#[sqlx::test]`**:
```rust
#[sqlx::test(migrations = "./migrations")]
async fn create_and_fetch_user(pool: PgPool) {
    // pool is a fresh database with all migrations applied
    // automatically cleaned up after test
}
```

**Integration test files**:
- `admin_integration.rs` — admin user/role management
- `auth_integration.rs` — login, tokens, sessions, password hashing
- `rbac_integration.rs` — role assignment, permission resolution, delegation
- `project_integration.rs` — project CRUD, soft-delete
- `issue_mr_integration.rs` — issues, comments, merge requests, reviews
- `webhook_integration.rs` — webhook CRUD, dispatch, HMAC signing
- `notification_integration.rs` — notification creation, queries
- `agent_spawn_integration.rs` — agent session DB operations
- `create_app_integration.rs` — app/bot session creation
- `workspace_integration.rs` — workspace CRUD, membership

## E2E Tests

**Location**: `tests/e2e_*.rs` (5 files, 40 tests total) + `tests/e2e_helpers/mod.rs`.

**What they cover**: full system behavior with real K8s pods, MinIO object storage, Valkey caching, and Postgres. Pipeline execution, git operations, webhook delivery, agent lifecycle, deployment management.

### Prerequisites

A Kind cluster with all services running. One-time setup:

```bash
just cluster-up
```

This creates:
- Kind cluster named `platform` with port mappings and `/tmp/platform-e2e` shared mount
- CNPG-managed Postgres at `localhost:5432` (user: `platform`, password: `dev`, db: `platform_dev`)
- Valkey at `localhost:6379`
- MinIO at `localhost:9000` (S3 API) / `localhost:9001` (console), credentials: `platform`/`devdevdev`
- MinIO buckets: `platform` and `platform-e2e`
- K8s namespaces: `e2e-pipelines` and `e2e-agents`
- OTel Collector (for observe module)

After cluster creation, apply migrations:
```bash
export KUBECONFIG=$HOME/.kube/kind-platform
export DATABASE_URL="postgres://platform:dev@127.0.0.1:5432/platform_dev"
just db-migrate
```

### Running E2E Tests

```bash
# All 40 E2E tests
just test-e2e

# Specific test file
KUBECONFIG=$HOME/.kube/kind-platform \
  DATABASE_URL="postgres://platform:dev@127.0.0.1:5432/platform_dev" \
  SQLX_OFFLINE=true \
  cargo nextest run --run-ignored=only --test e2e_pipeline --test-threads 2

# Single test
KUBECONFIG=$HOME/.kube/kind-platform \
  DATABASE_URL="postgres://platform:dev@127.0.0.1:5432/platform_dev" \
  SQLX_OFFLINE=true \
  cargo nextest run --run-ignored=only --test e2e_pipeline -E 'test(step_logs_captured)'

# With debug logging
KUBECONFIG=$HOME/.kube/kind-platform \
  DATABASE_URL="postgres://platform:dev@127.0.0.1:5432/platform_dev" \
  SQLX_OFFLINE=true \
  RUST_LOG=platform::pipeline=debug \
  cargo nextest run --run-ignored=only --test e2e_pipeline --no-capture
```

### E2E Test Architecture

Each E2E test:
1. Gets a fresh `PgPool` from `#[sqlx::test(migrations = "./migrations")]` (ephemeral DB)
2. Builds an `AppState` with real K8s, MinIO, Valkey via `e2e_helpers::e2e_state(pool)`
3. Creates a test router via `e2e_helpers::test_router(state)`
4. Logs in as admin via `e2e_helpers::admin_login(&app)`
5. Exercises API endpoints using HTTP helpers (`get_json`, `post_json`, etc.)
6. Asserts on HTTP status codes and JSON response bodies

The test router is an in-memory axum `Router` — no TCP listener. Requests go through `tower::ServiceExt::oneshot()`.

### E2E Helper Functions

All helpers are in `tests/e2e_helpers/mod.rs`:

**State & Router**:
- `e2e_state(pool: PgPool) -> AppState` — builds full state with real services. MinIO bucket: `platform-e2e`. Config: pipeline namespace `e2e-pipelines`, agent namespace `e2e-agents`.
- `test_router(state: AppState) -> Router` — merges `platform::api::router()` with state.

**Auth**:
- `admin_login(&app) -> String` — login as bootstrap admin (password: `testpassword`), returns bearer token.
- `create_user(&app, admin_token, name, email) -> (Uuid, String)` — create user + login, returns (user_id, token).
- `assign_role(&app, admin_token, user_id, role_name, project_id, &pool)` — assign role to user.

**Git**:
- `create_bare_repo() -> (TempDir, PathBuf)` — bare git repo under `/tmp/platform-e2e/` (visible to Kind).
- `create_working_copy(&bare_path) -> (TempDir, PathBuf)` — clone + initial commit + push to main.
- `git_cmd(&dir, &[args]) -> String` — run git command, panic on failure.

**HTTP**:
- `get_json(&app, token, path) -> (StatusCode, Value)` — GET with bearer auth.
- `post_json(&app, token, path, body) -> (StatusCode, Value)` — POST JSON with bearer auth.
- `patch_json(&app, token, path, body) -> (StatusCode, Value)` — PATCH JSON.
- `delete_json(&app, token, path) -> (StatusCode, Value)` — DELETE.
- `get_bytes(&app, token, path) -> (StatusCode, Vec<u8>)` — GET raw bytes.

**K8s**:
- `wait_for_pod(&kube, namespace, name, timeout_secs) -> String` — poll until Succeeded/Failed.
- `cleanup_k8s(&kube, namespace, label)` — delete pods by label selector.
- `poll_pipeline_status(&app, token, project_id, pipeline_id, timeout_secs) -> String` — poll until terminal status.
- `poll_deployment_status(&app, token, project_id, env, expected, timeout_secs) -> String` — poll deployment.

### E2E Test Files

#### `e2e_pipeline.rs` (10 tests)

Tests pipeline triggering, execution via real K8s pods, multi-step pipelines, cancellation, log capture, MinIO storage, and artifact upload/download.

**Critical patterns**:

1. **Spawn executor per test** — the test router does NOT include the background pipeline executor. Each test must spawn one:
   ```rust
   let _executor = ExecutorGuard::spawn(&state);
   // ... trigger pipeline ...
   state.pipeline_notify.notify_one();  // wake executor
   ```

2. **`.platform.yaml` format** — must have `pipeline:` top-level key:
   ```yaml
   pipeline:
     steps:
       - name: test
         image: alpine:3.19
         commands:
           - echo hello
   ```

3. **Git repo setup** — `setup_pipeline_project` creates a project, bare repo, working copy, commits `.platform.yaml`, and updates the project's `repo_path` in DB.

4. **Pod execution** — pipeline pods run in namespace `e2e-pipelines`. The executor creates pods with an init container (`alpine/git`) that clones the repo, then runs step commands. Repos must be under `/tmp/platform-e2e/` (shared mount).

Tests: `pipeline_trigger_and_execute`, `pipeline_with_multiple_steps`, `pipeline_step_failure`, `pipeline_cancel`, `step_logs_captured`, `step_logs_in_minio`, `artifact_upload_and_download`, `pipeline_branch_trigger_filter`, `pipeline_definition_parsing`, `concurrent_pipeline_limit`.

#### `e2e_git.rs` (8 tests)

Tests git operations: bare repo creation on project create, smart HTTP push/clone, branch listing, commit history, tree browsing, blob content retrieval, and merge request merge.

**Pattern**: create a bare repo + working copy, point a project at the bare repo via DB update, then exercise git browser API endpoints.

Tests: `bare_repo_init_on_project_create`, `smart_http_push`, `smart_http_clone`, `branch_listing`, `commit_history`, `tree_browsing`, `blob_content`, `merge_request_merge`.

#### `e2e_webhook.rs` (6 tests)

Tests webhook delivery, HMAC-SHA256 signing, pipeline completion events, concurrency limits, and timeout handling.

**Critical pattern — SSRF bypass**: the platform's SSRF protection blocks `127.0.0.1` URLs. Since wiremock binds to localhost, tests insert webhooks directly into DB instead of using the API:
```rust
sqlx::query("INSERT INTO webhooks (id, project_id, url, events, is_active) VALUES ($1,$2,$3,$4,true)")
    .bind(id).bind(project_id).bind(&wiremock_url).bind(&["push"]).execute(&pool).await.unwrap();
```

Tests: `webhook_fires_on_issue_create`, `webhook_hmac_signature`, `webhook_no_signature_without_secret`, `webhook_fires_on_pipeline_complete`, `webhook_concurrent_limit`, `webhook_timeout_doesnt_block`.

#### `e2e_agent.rs` (8 tests)

Tests agent session lifecycle: creation, identity provisioning, pod spec generation, provider configuration, session stop, custom images, and log capture by the reaper.

**Pattern**: tests that need real K8s pods check if the kube client works and skip gracefully if not. Most tests verify API responses and DB state without requiring actual pod execution.

Tests: `agent_session_creation`, `agent_identity_created`, `agent_identity_cleanup`, `agent_pod_spec_correct`, `agent_role_determines_mcp_config`, `agent_session_stop`, `agent_session_with_custom_image`, `agent_reaper_captures_logs`.

#### `e2e_deployer.rs` (8 tests)

Tests deployment API layer: CRUD, status transitions, history recording, rollback, image updates, stop, and preview environment lifecycle.

**Pattern**: tests exercise the API without running the reconciler loop. Deployments are created and managed via API calls; the reconciler is a separate background task not spawned in tests.

Tests: `deployment_status_transitions`, `deployment_get_returns_correct_fields`, `deployment_history_recorded`, `deployment_rollback`, `deployment_update_image`, `deployment_stop`, `preview_deployment_lifecycle`, `preview_cleanup_on_mr_merge`.

### Common Pitfalls

1. **Missing MinIO buckets** — `just cluster-up` creates `platform` and `platform-e2e` buckets. If you recreate MinIO without running the full script, buckets won't exist and log/artifact writes will fail silently (causing 500s on read).

2. **Stale `.sqlx/` cache** — never use `sqlx::query!` macros in `tests/` files. Use dynamic `sqlx::query()` / `sqlx::query_as()` instead. The compile-time macros require the offline cache to be regenerated every time queries change.

3. **`/tmp/platform-e2e` mount** — pipeline pods use HostPath volumes to mount git repos. If repos are created in macOS temp dirs (`/var/folders/...`), they're invisible inside the Kind Docker container. Always use `/tmp/platform-e2e/` as the base path (the helpers do this automatically).

4. **Port conflicts** — Kind maps `5432` (Postgres), `6379` (Valkey), `9000`/`9001` (MinIO), `8080` (app). Stop any conflicting services first (Docker Desktop, OrbStack, local Postgres).

5. **KUBECONFIG path** — in sandboxed environments `$HOME` may resolve to `/`. Use the full path: `KUBECONFIG=/Users/<you>/.kube/kind-platform`.

6. **Pipeline executor not running** — the test router does NOT spawn background tasks. Pipeline tests must create an `ExecutorGuard` and call `state.pipeline_notify.notify_one()` after triggering.

7. **SSRF blocking localhost** — webhook tests can't register `http://127.0.0.1:*` URLs via the API. Insert directly into DB.

8. **Race conditions** — after triggering a pipeline, the executor may pick it up before your next assertion. Don't assert `status == "pending"` immediately after trigger — use `poll_pipeline_status()` to wait for completion.

9. **Stale kubeconfig** — after Kind cluster restart or Docker Desktop restart, the kubeconfig may become stale (API server port changes). Refresh it:
   ```bash
   kind get kubeconfig --name platform > $HOME/.kube/kind-platform
   ```

10. **`.sqlx/` stale after Rust code changes** — `cargo sqlx prepare` must be re-run whenever `sqlx::query!` macros change in Rust code, not just when migration SQL changes. The `SQLX_OFFLINE=true` build will fail if the cache is stale. The `cargo llvm-cov` target dir is separate from the normal target dir, so even a working regular build may fail under coverage:
    ```bash
    just db-prepare   # regenerate .sqlx/ cache
    ```

11. **Stale E2E pods** — previous test runs leave pods in `e2e-agents` and `e2e-pipelines`. While tests usually tolerate this, cleaning up avoids noise:
    ```bash
    kubectl --kubeconfig=$HOME/.kube/kind-platform delete pods --all -n e2e-agents
    kubectl --kubeconfig=$HOME/.kube/kind-platform delete pods --all -n e2e-pipelines
    ```

12. **AppState changes require test helper updates** — when fields are added to `AppState`, both `tests/helpers/mod.rs` and `tests/e2e_helpers/mod.rs` must be updated. Missing fields cause all integration and E2E tests to fail to compile.

### Cluster Management

```bash
just cluster-up      # create Kind cluster + all services + buckets + namespaces
just cluster-down    # destroy Kind cluster completely

# Manual cluster recreation (if config changes)
kind delete cluster --name platform
just cluster-up
```

**What `just cluster-up` provisions** (via `hack/kind-up.sh`):
- Kind cluster with `hack/kind-config.yaml` (port mappings + `/tmp/platform-e2e` mount)
- CNPG operator + single-instance Postgres cluster
- Valkey deployment + NodePort service
- MinIO deployment + NodePort service + buckets (`platform`, `platform-e2e`)
- OTel Collector (forwards OTLP to platform ingest)
- `CREATEDB` grant for `platform` DB user (required by `#[sqlx::test]`)
- Namespaces: `e2e-pipelines`, `e2e-agents`
- Shared directory: `/tmp/platform-e2e`

### CI Integration

```bash
just ci              # fmt + lint + deny + test-unit + build (no external services)
just ci-full         # ci + test-integration + test-e2e (requires Kind cluster)
```

`just ci` runs without any external infrastructure. `just ci-full` requires a running Kind cluster.

## Coverage

Three-tier coverage reporting using [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov) with separate reports for unit, integration, and E2E tests. This makes the testing pyramid visible — if code is only covered by E2E tests, it should probably also have unit tests.

### Prerequisites

```bash
cargo install cargo-llvm-cov --locked
rustup component add llvm-tools-preview
```

### Commands

```bash
just cov-unit         # unit coverage → coverage-unit.lcov
just cov-integration  # integration coverage → coverage-integration.lcov
just cov-e2e          # E2E coverage → coverage-e2e.lcov (requires Kind cluster)
just cov-all          # all tiers combined → coverage-all.lcov
just cov-total        # ★ combined report: unit + integration + E2E (requires Kind cluster + DB)
just cov-html         # unit coverage as HTML report → coverage-html/
just cov-summary      # quick terminal summary of unit + integration coverage
```

Generated files (`*.lcov`, `coverage-html/`) are gitignored.

### Combined coverage (the meaningful number)

Separate per-tier coverage is diagnostic. The number that matters is combined: "when all tests run, what % of lines are hit?"

The easiest way is `just cov-total`, which requires a live database and Kind cluster:

```bash
# Prerequisites: Kind cluster running (just cluster-up), DB migrated (just db-migrate)
export KUBECONFIG=$HOME/.kube/kind-platform
export DATABASE_URL="postgres://platform:dev@127.0.0.1:5432/platform_dev"
just cov-total
```

Under the hood, `just cov-total` runs:

```bash
# 1. Clean previous profiling data
cargo llvm-cov clean --workspace

# 2. Run all three test tiers in a single instrumented build
#    --no-report: accumulate coverage without generating a report yet
cargo llvm-cov nextest --no-report \
  --lib --test '*_integration' --test 'e2e_*' \
  --run-ignored all --test-threads 2 --no-fail-fast

# 3. Generate the combined report (text summary to stdout)
cargo llvm-cov report --ignore-filename-regex '(proto\.rs|ui\.rs|main\.rs)'
```

**Note**: `SQLX_OFFLINE=true` does NOT work with `cargo llvm-cov` because it uses a separate target directory (`llvm-cov-target`) and some type annotations fail under the coverage configuration. Always use a live database connection for coverage runs.

### Excluded from coverage

- `src/observe/proto.rs` — generated protobuf types
- `src/ui.rs` — rust-embed static file serving
- `src/main.rs` — bootstrap wiring (tested via E2E)
- `tests/`, `ui/`, `mcp/` — non-source code

### CI

The `coverage` job in `.github/workflows/ci.yaml` runs after unit tests pass, generates unit and integration lcov reports, and uploads them to Codecov with separate flags (`unit`, `integration`). E2E coverage runs nightly or on demand.

Codecov configuration is in `codecov.yml`:
- **Unit coverage**: gated — target is auto-ratcheting, new code (patch) must have 70% coverage
- **Integration coverage**: informational — tracked but does not block PRs
- **E2E coverage**: informational with carryforward (nightly updates)

### Interpreting results

| Scenario | What it means | Action |
|---|---|---|
| High unit, low integration | Logic tested, wiring not | Add integration tests for key API paths |
| Low unit, high E2E | Logic only tested through slow paths | Extract pure functions, add unit tests |
| Low everywhere | Untested code | Prioritize unit tests for business logic |

### VS Code integration

Install the Coverage Gutters extension (`ryanluker.vscode-coverage-gutters`), run `just cov-unit`, then Cmd+Shift+P → "Coverage Gutters: Display Coverage" to see green/red line indicators inline.
