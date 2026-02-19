# Platform — Coding Guidelines

Single Rust binary (~15K LOC) replacing 8+ off-the-shelf services (Gitea, Woodpecker, Authelia, OpenObserve, Maddy, OpenBao) with a unified platform. Architecture: `plans/unified-platform.md`. Toolchain: `plans/rust-dev-process.md`. Phased delivery: `plans/01-foundation.md` through `plans/10-web-ui.md`.

**Current status**: Phases 01 (Foundation), 02 (Identity & Auth), 03 (Git Server), and 04 (Project Management) complete. Modules 05-09 can be implemented in parallel.

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
just test-doc       # cargo test --doc
just db-add <name>  # create new migration
just db-migrate     # apply migrations
just db-revert      # revert last migration
just db-prepare     # regenerate .sqlx/ offline cache
just db-check       # verify .sqlx/ is up to date
just build          # UI + release build (SQLX_OFFLINE=true)
just docker         # docker build
just deploy-local   # build + load into kind + kubectl apply
just ci             # full local CI: fmt lint deny test-unit build
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

### Testing pyramid

1. **Unit tests** (fast, no I/O) — business logic, parsers, state machines, permission resolution, encryption. Inline `#[cfg(test)] mod tests` in source files.
2. **Integration tests** (real DB) — API endpoint flows, DB queries, auth flows. In `tests/` with `#[sqlx::test]`.
3. **E2E** (rare) — full server + kind cluster. Only for deployer reconciliation and agent pod lifecycle.

### When to write tests first (TDD)

- State machine transitions
- Permission resolution logic
- Parsers (OTLP protobuf, pipeline definitions)
- Encryption/hashing round-trips
- Business rules (webhook filtering, alert conditions)

### When tests come alongside

- HTTP handler wiring
- Database CRUD
- Integration glue (WebSocket setup, K8s client wiring)

### Trait-based dependency injection

Use native async fn in traits (Rust 2024 edition, no `async-trait` crate). Business logic accepts `impl Trait`, not concrete types.

```rust
pub trait UserRepository: Send + Sync {
    async fn find_by_id(&self, id: UserId) -> Result<Option<User>>;
    async fn create(&self, req: CreateUserRequest) -> Result<User>;
}

// Production implementation
pub struct PgUserRepository { pool: PgPool }
impl UserRepository for PgUserRepository {
    async fn find_by_id(&self, id: UserId) -> Result<Option<User>> {
        sqlx::query_as!(User, "SELECT * FROM users WHERE id = $1", id.as_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(Into::into)
    }
    // ...
}

// Test mock
#[cfg(test)]
pub struct MockUserRepository {
    pub users: std::sync::Mutex<Vec<User>>,
}
#[cfg(test)]
impl UserRepository for MockUserRepository {
    async fn find_by_id(&self, id: UserId) -> Result<Option<User>> {
        Ok(self.users.lock().unwrap().iter().find(|u| u.id == id).cloned())
    }
    // ...
}
```

Use traits for: database access, Valkey cache, K8s client, MinIO/S3, SMTP.

### Integration tests with sqlx

```rust
#[sqlx::test(migrations = "migrations")]
async fn create_and_fetch_user(pool: PgPool) {
    let repo = PgUserRepository::new(pool);
    let user = repo.create(CreateUserRequest {
        name: "testuser".into(),
        email: "test@example.com".into(),
    }).await.unwrap();
    let fetched = repo.find_by_id(user.id).await.unwrap().unwrap();
    assert_eq!(fetched.name, "testuser");
}
```

### Snapshot testing (insta)

Use for API response format stability:

```rust
#[tokio::test]
async fn list_projects_response() {
    let response = /* ... */;
    insta::assert_json_snapshot!(response);
}
```

### Property-based testing (proptest)

Use for parser/serialization round-trips:

```rust
proptest! {
    #[test]
    fn permission_roundtrip(perm in any::<Permission>()) {
        let s = perm.as_str();
        let parsed = Permission::from_str(s).unwrap();
        assert_eq!(perm, parsed);
    }
}
```

### Test helpers

Common setup functions in `tests/helpers/mod.rs`:

```rust
pub async fn create_test_user(pool: &PgPool, name: &str) -> User { /* ... */ }
pub async fn create_test_project(pool: &PgPool, owner: &User) -> Project { /* ... */ }
pub fn test_app_state(pool: PgPool) -> AppState { /* mock valkey, minio, kube */ }
```

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

## Git Workflow

- Run `just ci` before pushing
- Pre-commit hooks enforce `rustfmt --check` and `clippy`
- Never commit `.env` (gitignored), update `.env.example` for new vars
- Commit `Cargo.lock` (binary project)
