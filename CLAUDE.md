# Platform — Coding Guidelines

Single Rust binary (~23K LOC) replacing 8+ off-the-shelf services (Gitea, Woodpecker, Authelia, OpenObserve, Maddy, OpenBao) with a unified platform. Architecture: `docs/architecture.md`. Testing: `docs/testing.md`. Schema & design rationale: `plans/unified-platform.md`. Toolchain: `plans/rust-dev-process.md`.

**Current status**: All modules implemented. Security hardening applied across all phases. `plans/` is for new work-in-progress only (completed plans archived in git history).

## Commands

```
just watch          # bacon file watcher (cargo check on save)
just run            # cargo run
just fmt            # cargo fmt
just lint           # cargo clippy --all-features -- -D warnings
just deny           # cargo deny check
just check          # fmt + lint + deny
just test           # cargo nextest run (all tests)
just test-unit      # cargo nextest run --lib (unit only, no DB)
just test-integration  # integration tests (ephemeral Kind services)
just test-e2e       # E2E tests (ephemeral Kind services, run-ignored)
just test-doc       # cargo test --doc
just ui             # build Preact SPA (esbuild)
just db-add <name>  # create new migration
just db-migrate     # apply migrations
just db-revert      # revert last migration
just db-prepare     # regenerate .sqlx/ offline cache
just db-check       # verify .sqlx/ is up to date
just build          # UI + release build (SQLX_OFFLINE=true)
just docker [tag]   # docker build
just deploy-local [tag]  # build + load into kind + kubectl apply
just cov-unit       # unit test coverage → coverage-unit.lcov
just cov-integration # integration coverage → coverage-integration.lcov
just cov-e2e        # E2E coverage → coverage-e2e.lcov
just cov-all        # all tiers combined → coverage-all.lcov
just cov-total      # ★ combined report: unit + integration + E2E (needs Kind + DB)
just cov-diff       # diff coverage on changed lines vs main (unit+int+E2E, needs Kind)
just cov-diff-check # diff coverage strict: fail if changed lines < 100% covered
just cov-html       # unit coverage as HTML report
just cov-summary    # quick terminal summary (unit + integration)
just ci             # full local CI: fmt lint deny test-unit test-integration build
just ci-full        # ci + test-e2e
just cluster-up     # create kind cluster + Postgres + Valkey + MinIO
just cluster-down   # destroy kind cluster
```

## Architecture Rules

- **Single crate** — 11 modules under `src/`. No workspace. Only split if `cargo check` exceeds 30s.
- **AppState** — shared state passed to all handlers via `axum::extract::State`:
  ```rust
  pub struct AppState {
      pub pool: PgPool,
      pub valkey: fred::clients::Pool,
      pub minio: opendal::Operator,
      pub kube: kube::Client,
      pub config: Arc<Config>,
      pub webauthn: Arc<webauthn_rs::prelude::Webauthn>,
      pub pipeline_notify: Arc<tokio::sync::Notify>,
  }
  ```
- **Module boundaries** — each `src/<module>/mod.rs` re-exports its public API. Modules communicate through `AppState`, never import each other's internals. Cross-module types live in `src/error.rs` or `src/config.rs`.
- **No unsafe** — `unsafe_code = "forbid"` in `Cargo.toml` lints.
- **No openssl** — `deny.toml` bans `openssl`/`openssl-sys`. Use rustls everywhere.
- **sqlx compile-time checking** — all queries use `sqlx::query!` or `sqlx::query_as!`. Run `just db-prepare` after any query change. CI uses `SQLX_OFFLINE=true`.

## Auth & RBAC Patterns (Phase 02)

### Handler authentication

Use `AuthUser` as an axum extractor. It checks Bearer token then session cookie:

```rust
use crate::auth::middleware::AuthUser;

async fn my_handler(
    State(state): State<AppState>,
    auth: AuthUser,  // extracts user_id, user_name, ip_addr
    // ... other extractors
) -> Result<Json<Response>, ApiError> {
    // auth.user_id, auth.user_name available
}
```

### Permission checks — inline (for few endpoints)

```rust
use crate::rbac::{Permission, resolver};

async fn admin_handler(State(state): State<AppState>, auth: AuthUser) -> Result<..., ApiError> {
    let allowed = resolver::has_permission(&state.pool, &state.valkey, auth.user_id, None, Permission::AdminUsers)
        .await.map_err(ApiError::Internal)?;
    if !allowed { return Err(ApiError::Forbidden); }
    // ...
}
```

### Permission checks — helper function (preferred for sub-routers)

Sub-routers (`Router<AppState>`) don't have a concrete state at construction time, so `from_fn_with_state` can't be used. Instead, define a helper:

```rust
async fn require_project_write(state: &AppState, auth: &AuthUser, project_id: Uuid) -> Result<(), ApiError> {
    let allowed = resolver::has_permission(&state.pool, &state.valkey, auth.user_id, Some(project_id), Permission::ProjectWrite)
        .await.map_err(ApiError::Internal)?;
    if !allowed { return Err(ApiError::Forbidden); }
    Ok(())
}
// Then call in each handler: require_project_write(&state, &auth, id).await?;
```

### Permission checks — route layer (only when state is available)

```rust
use crate::rbac::middleware::require_permission;
use crate::rbac::Permission;

// Only works if you have a concrete `state` value at construction time
pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/api/projects", get(list).post(create))
        .route_layer(axum::middleware::from_fn_with_state(state, require_permission(Permission::ProjectRead)))
}
```

### Audit logging

All mutations must write to `audit_log`. Use the `AuditEntry` struct pattern:

```rust
struct AuditEntry<'a> {
    actor_id: Uuid,
    actor_name: &'a str,
    action: &'a str,        // e.g. "user.create", "role.assign"
    resource: &'a str,      // e.g. "user", "role"
    resource_id: Option<Uuid>,
    project_id: Option<Uuid>,
    detail: Option<serde_json::Value>,
    ip_addr: Option<&'a str>,  // from auth.ip_addr
}
```

### Permission cache invalidation

After any role or delegation change, invalidate the affected user's permission cache:

```rust
resolver::invalidate_permissions(&state.valkey, user_id, project_id).await;
```

## Project Management Patterns (Phase 04)

### Auto-incrementing project-scoped numbers

Issues and MRs use project-scoped auto-incrementing numbers (not UUIDs in URLs):

```rust
let number = sqlx::query_scalar!(
    r#"UPDATE projects SET next_issue_number = next_issue_number + 1
    WHERE id = $1 AND is_active = true RETURNING next_issue_number"#,
    project_id,
).fetch_optional(&state.pool).await?
.ok_or_else(|| ApiError::NotFound("project".into()))?;
```

### Webhook dispatch

Use `fire_webhooks()` from `api::webhooks` after mutations that external systems care about:

```rust
crate::api::webhooks::fire_webhooks(
    &state.pool, project_id, "issue",  // event name: push, mr, issue, build, deploy
    &serde_json::json!({"action": "created", "issue": {...}}),
).await;
```

Webhooks use HMAC-SHA256 signing (`X-Platform-Signature` header) when a secret is configured.

### Soft-delete pattern

Projects use soft-delete (`is_active = false`). Always filter with `AND is_active = true` in queries.

### API module files

- `src/api/projects.rs` — Project CRUD
- `src/api/issues.rs` — Issues + comments
- `src/api/merge_requests.rs` — MRs + reviews + comments + merge
- `src/api/webhooks.rs` — Webhook CRUD + `fire_webhooks()` utility
- `src/api/pipelines.rs` — Pipeline CRUD + run triggers (Phase 05)
- `src/api/deployments.rs` — Deployment status + logs (Phase 06)
- `src/api/sessions.rs` — Agent session management (Phase 07)
- `src/api/secrets.rs` — Secrets CRUD (Phase 09)
- `src/api/notifications.rs` — Notification queries (Phase 09)
- `src/api/passkeys.rs` — WebAuthn registration/authentication
- `src/api/admin.rs` — Admin CRUD (users, roles, delegations)
- `src/api/helpers.rs` — Common extraction/validation utilities

## Build Engine Patterns (Phase 05)

### Pipeline definition

Pipeline YAML (`.platform.yaml`) is parsed in `src/pipeline/definition.rs`. Validates steps, images, and commands.

### Pipeline execution

`src/pipeline/executor.rs` spawns K8s pods per step. Uses `pipeline_notify: Arc<tokio::sync::Notify>` to wake the executor loop when a new pipeline is queued — avoids polling.

```rust
// Wake executor after creating a pipeline run:
state.pipeline_notify.notify_one();
```

### Pipeline status state machine

`PipelineStatus`: Pending → Running → Success/Failure/Cancelled. Uses `can_transition_to()` pattern.

### Container image validation

`check_container_image()` and `check_setup_commands()` in `src/pipeline/definition.rs` validate user-supplied container images and setup commands against injection.

### K8s namespaces

- `PLATFORM_PIPELINE_NAMESPACE` (default: `platform-pipelines`) — pipeline pods
- `PLATFORM_AGENT_NAMESPACE` (default: `platform-agents`) — agent pods

## Deployer Patterns (Phase 06)

### Reconciler loop

`src/deployer/reconciler.rs` — continuous reconciliation of desired vs actual state. Runs as a background task.

### Ops repo + manifest rendering

`src/deployer/ops_repo.rs` — manages operations repos (Kustomize/Helm). `src/deployer/renderer.rs` — renders Kustomize overlays.

### Preview environments

`src/deployer/preview.rs` — ephemeral namespaces per branch. `slugify_branch()` in `src/pipeline/mod.rs` converts branch names to K8s-safe slugs. TTL-based cleanup removes stale previews.

### K8s applier

`src/deployer/applier.rs` — applies rendered manifests to K8s (kubectl apply equivalent).

## Agent Patterns (Phase 07)

### Session lifecycle

`src/agent/service.rs` — agent orchestration. Sessions track ephemeral agent pods.

### Ephemeral identity

`src/agent/identity.rs` — each agent session gets a temporary identity with scoped permissions.

### Provider configuration

`src/agent/provider.rs` — provider interface. `resolve_image()` determines the container image to use with priority: explicit config → registry URL → default.

## Observability Patterns (Phase 08)

### OTLP ingest

`src/observe/ingest.rs` — HTTP endpoints for OTLP traces, logs, metrics. Protobuf types in `src/observe/proto.rs`.

### Parquet storage

`src/observe/parquet.rs` — time-based rotation of Parquet files to MinIO. `src/observe/store.rs` — columnar query engine.

### Query API

`src/observe/query.rs` — traces, logs, metrics query endpoints with time-range filtering.

### Alert evaluation

`src/observe/alert.rs` — background loop evaluates alert rules against stored data, dispatches notifications.

### Background tasks

The observe module spawns 5 background tasks: traces flush, logs flush, metrics flush, Parquet rotation, alert evaluation.

## Secrets & Notify Patterns (Phase 09)

### Secrets engine

`src/secrets/engine.rs` — AES-256-GCM encryption with `PLATFORM_MASTER_KEY`. Encrypt-at-rest, decrypt-on-read.

### Notification dispatch

`src/notify/dispatch.rs` — routes events to email/webhooks. `src/notify/email.rs` — SMTP via lettre. `src/notify/webhook.rs` — HMAC-SHA256 signed delivery.

## Auth Improvements (Phase 11)

### WebAuthn/Passkeys

`src/auth/passkey.rs` — WebAuthn registration and authentication via `webauthn_rs`. API in `src/api/passkeys.rs`. Requires `WEBAUTHN_RP_ID`, `WEBAUTHN_RP_ORIGIN`, `WEBAUTHN_RP_NAME` env vars.

### User types

`src/auth/user_type.rs` — `UserType` enum distinguishes human vs agent users.

## Web UI (Phase 10)

Preact SPA in `ui/src/`, built with esbuild, embedded via `rust-embed` in `src/ui.rs`.

### Structure

- `ui/src/pages/` — Dashboard, Projects, ProjectDetail, IssueDetail, MRDetail, PipelineDetail, Sessions, Login, admin/, observe/
- `ui/src/components/` — Layout, Pagination, Table, Modal, Toast, Badge, Markdown, CodeBlock, FilterBar, NotificationBell, ErrorBoundary
- `ui/src/lib/` — api.ts (client), auth.tsx (context), ws.ts (WebSocket), types.ts, format.ts

### Build

`just ui` runs the esbuild pipeline. `just build` includes UI build + release Rust build.

## MCP Servers

6 MCP servers under `mcp/servers/` for external Claude agent integration:

- `platform-core.js` — Core platform operations
- `platform-admin.js` — Admin operations
- `platform-issues.js` — Issues/comments
- `platform-pipeline.js` — Pipeline management
- `platform-deploy.js` — Deployment operations
- `platform-observe.js` — Observability queries

Shared client library: `mcp/lib/client.js`.

## Security Patterns

### Input validation

All API handlers must validate inputs before processing. Use helpers from `src/validation.rs`:

```rust
use crate::validation;

// In handler, before any DB or business logic:
validation::check_name(&body.name)?;              // 1-255, alphanumeric + -_.
validation::check_email(&body.email)?;             // 1-254, contains @
validation::check_length("password", &body.password, 8, 1024)?;
validation::check_length("description", &desc, 0, 10_000)?;
validation::check_branch_name(&body.branch)?;      // 1-255, no "..", no null bytes
validation::check_labels(&body.labels)?;            // max 50, each 1-100
validation::check_url(&body.url)?;                 // 1-2048, http(s) only
validation::check_lfs_oid(&oid)?;                  // exactly 64 hex chars
```

**Field limits** (enforce these for all new endpoints):

| Field type | Min | Max |
|---|---|---|
| Name/slug | 1 | 255 |
| Email | 3 | 254 |
| Password | 8 | 1,024 |
| Title | 1 | 500 |
| Body/description | 0 | 100,000 |
| URL | 1 | 2,048 |
| Labels | - | 50 items, each 1-100 chars |
| Display name | 1 | 255 |

### Rate limiting

Use `src/auth/rate_limit.rs` for endpoints vulnerable to brute force:

```rust
crate::auth::rate_limit::check_rate(&state.valkey, "login", &identifier, 10, 300).await?;
// prefix: "login", max: 10 attempts, window: 300 seconds
```

Currently applied to: login. Apply to any new authentication or password-related endpoints.

### SSRF protection

Webhook URLs (and any user-supplied URLs that the server will fetch) must be validated against SSRF:

```rust
// In src/api/webhooks.rs — validate_webhook_url() blocks:
// - Private IPs (10/8, 172.16/12, 192.168/16, 127/8)
// - Link-local (169.254/16)
// - Loopback (::1, localhost)
// - Cloud metadata (169.254.169.254, metadata.google.internal)
// - Non-HTTP schemes (ftp://, file://, etc.)
```

Apply the same pattern to any new feature that makes outbound HTTP requests to user-supplied URLs.

### Webhook dispatch security

- **Shared client**: `WEBHOOK_CLIENT` static with 5s connect / 10s total timeout, no redirects
- **Concurrency limit**: `WEBHOOK_SEMAPHORE` (50 concurrent deliveries); excess dropped with warning
- **HMAC signing**: `X-Platform-Signature: sha256={hex}` when secret configured
- **Audit sanitization**: never log webhook URLs (may contain tokens)

### Authorization patterns

**Read endpoints on sub-resources** (issues, MRs, comments, reviews) must check project-level read access:

```rust
async fn require_project_read(state: &AppState, auth: &AuthUser, project_id: Uuid) -> Result<(), ApiError> {
    let project = sqlx::query!("SELECT visibility, owner_id FROM projects WHERE id = $1 AND is_active = true", project_id)
        .fetch_optional(&state.pool).await?.ok_or_else(|| ApiError::NotFound("project".into()))?;
    if project.visibility == "public" || project.visibility == "internal" || project.owner_id == auth.user_id {
        return Ok(());
    }
    let allowed = resolver::has_permission(&state.pool, &state.valkey, auth.user_id, Some(project_id), Permission::ProjectRead)
        .await.map_err(ApiError::Internal)?;
    if !allowed { return Err(ApiError::NotFound("project".into())); }  // 404 to avoid leaking existence
    Ok(())
}
```

Key: return **404** (not 403) for private resources the user can't access — avoids leaking resource existence.

### Request-level defense (configured in `main.rs`)

- **Body size limits**: 10 MB default for API, 500 MB for Git push/LFS routes
- **Security headers**: `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: strict-origin-when-cross-origin`
- **CORS**: configured via `PLATFORM_CORS_ORIGINS` (comma-separated), denied by default
- **Session cleanup**: hourly background task deletes expired sessions/tokens

### Auth hardening

- **Timing-safe login**: always run argon2 verify (use `password::dummy_hash()` for missing users)
- **Secure cookies**: `Secure` flag when `PLATFORM_SECURE_COOKIES=true`
- **Token expiry**: 1-365 days, default 90 days; enforce at creation time
- **User deactivation**: deletes all sessions + API tokens + invalidates permission cache
- **Proxy trust**: `PLATFORM_TRUST_PROXY` controls X-Forwarded-For parsing

### Security-related config env vars

| Env var | Default | Purpose |
|---|---|---|
| `PLATFORM_SECURE_COOKIES` | `false` | Add `Secure` flag to session cookies |
| `PLATFORM_CORS_ORIGINS` | (empty = deny) | Comma-separated allowed CORS origins |
| `PLATFORM_TRUST_PROXY` | `false` | Trust `X-Forwarded-For` for client IP |
| `PLATFORM_DEV` | `false` | Dev mode (allows default credentials) |
| `PLATFORM_PERMISSION_CACHE_TTL` | `300` | Permission cache TTL in seconds |
| `PLATFORM_MASTER_KEY` | — | AES-256-GCM encryption key for secrets |
| `PLATFORM_NAMESPACE` | `platform` | K8s namespace where the platform itself runs |
| `PLATFORM_PIPELINE_NAMESPACE` | `platform-pipelines` | K8s namespace for pipeline pods |
| `PLATFORM_AGENT_NAMESPACE` | `platform-agents` | K8s namespace for agent pods |
| `PLATFORM_OPS_REPOS_PATH` | `/data/ops-repos` | Ops repo storage path |
| `WEBAUTHN_RP_ID` | — | WebAuthn relying party ID |
| `WEBAUTHN_RP_ORIGIN` | — | WebAuthn relying party origin |
| `WEBAUTHN_RP_NAME` | — | WebAuthn relying party display name |
| `PLATFORM_SMTP_HOST` | — | SMTP server host |
| `PLATFORM_API_URL` | `http://platform.platform.svc.cluster.local:8080` | HTTP URL for agent/pipeline pods to reach the platform |
| `PLATFORM_REGISTRY_URL` | — | Container registry URL |

## Type System Patterns

### Newtype wrappers for domain IDs

Every table's primary key gets a newtype. Prevents passing a `ProjectId` where a `UserId` is expected.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, sqlx::Type)]
#[sqlx(transparent)]
pub struct UserId(Uuid);

impl UserId {
    pub fn new() -> Self { Self(Uuid::new_v4()) }
    pub fn as_uuid(&self) -> &Uuid { &self.0 }
}
```

### Status enums as state machines

Every SQL `CHECK` constraint enum gets a Rust enum. Invalid transitions are caught in application logic.

```rust
#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type, serde::Serialize, serde::Deserialize)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum PipelineStatus {
    Pending,
    Running,
    Success,
    Failure,
    Cancelled,
}

impl PipelineStatus {
    pub fn can_transition_to(&self, next: &Self) -> bool {
        matches!(
            (self, next),
            (Self::Pending, Self::Running)
                | (Self::Running, Self::Success | Self::Failure | Self::Cancelled)
        )
    }
}
```

### Request/Response types

Separate from DB model structs. Never expose internal DB types directly in the API.

```rust
// API types — not the DB row struct
pub struct CreateProjectRequest { pub name: String, pub description: Option<String> }
pub struct ProjectResponse { pub id: ProjectId, pub name: String, pub created_at: DateTime<Utc> }
```

## Error Handling

### Per-module error enums

Each module defines its own error type with `thiserror`:

```rust
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("invalid credentials")]
    InvalidCredentials,
    #[error("token expired")]
    TokenExpired,
    #[error("session not found")]
    SessionNotFound,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}
```

### Conversion to ApiError

Map domain errors to HTTP status codes:

```rust
impl From<AuthError> for ApiError {
    fn from(err: AuthError) -> Self {
        match err {
            AuthError::InvalidCredentials | AuthError::TokenExpired => Self::Unauthorized,
            AuthError::SessionNotFound => Self::NotFound("session".into()),
            _ => Self::Internal(err.into()),
        }
    }
}
```

### Rules

- `thiserror` for types crossing module boundaries
- `.context("descriptive message")` from anyhow when propagating errors
- No `.unwrap()` in production code — only in tests and infallible cases with a comment

## Observability

### Instrument all async functions with side effects

```rust
#[tracing::instrument(skip(pool), fields(user_id = %user_id), err)]
pub async fn get_user(pool: &PgPool, user_id: UserId) -> Result<User, AuthError> {
    sqlx::query_as!(User, "SELECT * FROM users WHERE id = $1", user_id.as_uuid())
        .fetch_optional(pool)
        .await?
        .ok_or(AuthError::SessionNotFound)
}
```

### Rules

- `skip(pool, state, config)` — always skip large/non-Debug fields
- `fields(key = %value)` — add IDs, user context, project context
- `err` attribute — automatically log errors at ERROR level
- **Structured fields only**: `tracing::info!(count = 5, "processed")` not `tracing::info!("processed {count}")`
- **Correlation context**: always include `user_id`, `project_id`, `session_id` where available
- **Error chains**: `tracing::error!(error = %err, "operation failed")`
- **Never log sensitive data**: passwords, tokens, secret values, API keys

### Span hierarchy

```
platform::main
  http::request{method=POST, path=/api/projects}
    auth::validate_token{token_prefix=plat_}
    rbac::check_permission{user_id=..., perm=project:write}
    store::create_project{project_name=my-app}
      sqlx::query{db.statement=INSERT INTO projects...}
```

## Testing Standards

Full testing guide: `docs/testing.md`. Frontend-backend testing: `docs/fe-be-testing.md`.

### MANDATORY: Run all tests before finishing

**Before considering any code change complete, you MUST run all three test tiers and verify they pass:**

```bash
just ci-full          # fmt + lint + deny + test-unit + test-integration + test-e2e + build
```

If `just ci-full` is too slow for iterative development, run at minimum:
```bash
just test-unit        # fast (~1s), run after every code change
just test-integration # after API/DB/auth changes (~2.5 min, requires Kind cluster)
just test-e2e         # after K8s/pipeline/deployer/agent/git/webhook changes (~2.5 min)
```

**Never skip E2E tests.** They catch real issues that unit and integration tests miss (K8s pod behavior, git operations, webhook delivery, cross-service interactions). If any tier fails, fix the issue before declaring the work done.

### Testing pyramid

| Tier | Count | Runtime | Infra | Command | When to run |
|---|---|---|---|---|---|
| Unit | 716 | ~1s | None | `just test-unit` | Every code change |
| Integration | 574 | ~2.5 min | Kind cluster | `just test-integration` | API/DB/auth changes |
| E2E | 49 | ~2.5 min | Kind cluster | `just test-e2e` | K8s/pipeline/deploy/git/webhook changes |
| FE-BE | 33+ | ~30s | Kind cluster | `just test-integration` / `just types` | API response shape changes |

### Test helpers — integration (`tests/helpers/mod.rs`)

- `test_state(pool: PgPool) -> (AppState, String)` — builds full state with real Valkey, MinIO, dummy K8s. Returns `(state, admin_token)`. The admin API token is created directly in the DB, bypassing the login endpoint's rate limiter.
- `test_router(state: AppState) -> Router` — merges API + observe + registry routers.
- `admin_login(&app) -> String` — login via POST `/api/auth/login`. Only for tests that test login/session behavior (~2 tests). All other tests use the pre-created `admin_token` from `test_state()`.
- `create_user(&app, token, name, email) -> (Uuid, String)` — create user + login.
- `assign_role(&app, token, user_id, role, project_id, &pool)` — assign role.
- `get_json`, `post_json`, `patch_json`, `put_json`, `delete_json` — HTTP helpers with bearer auth.

### Test helpers — E2E (`tests/e2e_helpers/mod.rs`)

- `e2e_state(pool: PgPool) -> (AppState, String)` — full state with real K8s, MinIO (bucket: `platform-e2e`), Valkey. Returns `(state, admin_token)`.
- `test_router(state: AppState) -> Router` — full API router.
- Git: `create_bare_repo()`, `create_working_copy()`, `git_cmd()`.
- K8s: `wait_for_pod()`, `cleanup_k8s()`, `poll_pipeline_status()`.

### Integration test pattern

```rust
#[sqlx::test(migrations = "./migrations")]
async fn my_test(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());
    let (status, body) = helpers::get_json(&app, &admin_token, "/api/my-endpoint").await;
    assert_eq!(status, StatusCode::OK);
}
```

### Critical test patterns

**No FLUSHDB** — test helpers never call FLUSHDB on Valkey. All Valkey keys are UUID-scoped and never collide between parallel tests. The admin token bypasses the only shared key (`rate:login:admin`).

**Pipeline tests must spawn an executor** — the test router doesn't include background tasks:
```rust
let _executor = ExecutorGuard::spawn(&state);
state.pipeline_notify.notify_one();  // wake executor after trigger
```

**SSRF blocks localhost webhook URLs** — insert webhooks directly into DB in tests.

**Use dynamic queries in test files** — `sqlx::query()` not `sqlx::query!()` in `tests/`.

**Git repos under `/tmp/platform-e2e/`** — shared mount between host and Kind node.

### When to write tests first (TDD)

State machine transitions, permission resolution, parsers, encryption round-trips, business rules.

### When tests come alongside

HTTP handler wiring, database CRUD, integration glue.

## API Design

### Handler signature convention

```rust
async fn handler_name(
    State(state): State<AppState>,        // always first
    auth: AuthUser,                       // auth second (plain struct, not tuple)
    Path(id): Path<Uuid>,                 // path params
    Query(params): Query<ListParams>,     // query params
    Json(body): Json<CreateRequest>,      // body last
) -> Result<Json<Response>, ApiError> {
    // auth.user_id, auth.user_name, auth.ip_addr available
}
```

### Pagination

```rust
#[derive(Debug, serde::Deserialize)]
pub struct ListParams {
    pub limit: Option<i64>,   // default 50, max 100
    pub offset: Option<i64>,  // default 0
}

#[derive(Debug, serde::Serialize)]
pub struct ListResponse<T: serde::Serialize> {
    pub items: Vec<T>,
    pub total: i64,
}
```

## Database Conventions

- All timestamps `TIMESTAMPTZ`, stored in UTC
- All primary keys `UUID` with `gen_random_uuid()`
- All tables have `created_at TIMESTAMPTZ NOT NULL DEFAULT now()`
- Mutable tables also have `updated_at TIMESTAMPTZ NOT NULL DEFAULT now()`
- Reversible migrations (`just db-add` creates up/down pairs)
- After any SQL change: `just db-migrate && just db-prepare`
- Commit `.sqlx/` changes with the code

## Crate API Gotchas

- **rand 0.10**: Use `rand::fill(&mut bytes)` (free function). `rand::rng().fill_bytes()` doesn't work — `RngCore` is not re-exported from the crate root.
- **argon2 + rand**: Use `argon2::password_hash::rand_core::OsRng` for salt generation, NOT `rand::rng()`. They use incompatible `rand_core` versions (0.6 vs 0.9).
- **fred Pool**: `pool.next().publish()` for pub/sub — `Pool` doesn't impl `PubsubInterface`, only `Client` does.
- **axum 0.8**: `.patch()`, `.put()`, `.delete()` are `MethodRouter` methods, not standalone `axum::routing` functions. Chain on routes directly.
- **sqlx INET**: Postgres `INET` columns need the `ipnetwork` crate + sqlx feature. Without it, skip binding those columns.
- **Clippy `too_many_arguments`**: Threshold is 7 params. Use a params struct (e.g., `AuditEntry`, `CreateDelegationParams`) from the start.
- **Clippy `too_many_lines`**: Threshold is 100 lines per function. Extract helpers (e.g., `get_project_repo_path()`) when handlers grow large.
- **Clippy `collapsible_if`**: Use `if let ... && condition { }` instead of nested `if let { if { } }`.
- **Clippy `trivially_copy_pass_by_ref`**: For `Copy` types, use `self` not `&self` (e.g., `fn as_str(self)`).
- **`require_permission` route layer**: Requires `from_fn_with_state(state.clone(), ...)` — won't work in sub-routers that return `Router<AppState>` without a concrete state. Use inline permission checks instead.
- **K8s `kind_to_plural` in applier**: `src/deployer/applier.rs` has a `kind_to_plural()` map for server-side apply. When adding new K8s resource types (e.g., `NetworkPolicy`), add the correct plural to this map — the generic fallback just appends "s" which is wrong for irregular plurals (`"networkpolicies"`, not `"networkpolicys"`).

## Git Workflow

- Run `just ci-full` before pushing (includes E2E tests)
- Pre-commit hooks enforce `rustfmt --check` and `clippy`
- Never commit `.env` (gitignored), update `.env.example` for new vars
- Commit `Cargo.lock` (binary project)
