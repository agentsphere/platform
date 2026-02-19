# 01 — Foundation: Store, Config, Error, Bootstrap

## Prerequisite
- Rust dev process setup complete (Cargo.toml, Justfile, kind scripts, CI)

## Blocks
- Every other plan depends on this one completing first

## Scope

Build the shared infrastructure that all modules depend on: database connection pool, Valkey client, MinIO client, config loading, error types, and the main.rs bootstrap that wires everything together.

---

## Deliverables

### 1. `src/store/mod.rs` — Database & Cache Clients

```
src/store/
  mod.rs       — re-exports, AppState struct
  pool.rs      — sqlx PgPool setup, migration runner
  valkey.rs    — fred client, pub/sub helpers
```

**`AppState`** — the shared state passed to all axum handlers:
```rust
pub struct AppState {
    pub pool: PgPool,
    pub valkey: fred::clients::Pool,
    pub minio: opendal::Operator,
    pub kube: kube::Client,
    pub config: Arc<Config>,
}
```

**`pool.rs`**:
- `pub async fn connect(url: &str) -> Result<PgPool>` — create pool with reasonable defaults (max 10 connections dev, configurable)
- Run `sqlx::migrate!()` on startup (embedded migrations)
- Connection health check

**`valkey.rs`**:
- `pub async fn connect(url: &str) -> Result<fred::clients::Pool>` — create connection pool
- Helper: `pub async fn get_cached<T: DeserializeOwned>(pool, key) -> Option<T>`
- Helper: `pub async fn set_cached<T: Serialize>(pool, key, value, ttl_secs)`
- Helper: `pub async fn invalidate(pool, key)`
- Pub/sub: `pub async fn publish(pool, channel, message)`

### 2. `src/config.rs` — Enhanced Config

Extend existing config.rs:
- Add `master_key: String` — for AES-256-GCM secret encryption (from env `PLATFORM_MASTER_KEY`)
- Add `git_repos_path: PathBuf` — bare repo storage (default `/data/repos`)
- Add `smtp_host: Option<String>`, `smtp_port: u16`, `smtp_from: String` — for notifications
- Add `admin_password: Option<String>` — initial admin password on first boot
- Validation: fail fast if required vars are missing in production mode

### 3. `src/error.rs` — Extended Error Types

Extend existing error.rs:
- Add `Conflict(String)` → HTTP 409
- Add `ValidationError(Vec<String>)` → HTTP 422 with field-level errors
- Add `ServiceUnavailable(String)` → HTTP 503
- Implement `From<sqlx::Error>` — map DB errors to ApiError
- Implement `From<fred::error::Error>` — map Valkey errors to ApiError
- Implement `From<kube::Error>` — map K8s errors to ApiError

### 4. `src/main.rs` — Full Bootstrap

Rewrite main.rs to:
1. Load config
2. Connect to Postgres (run migrations)
3. Connect to Valkey
4. Create MinIO operator (opendal S3 backend)
5. Create kube::Client
6. Build AppState
7. Build axum Router (initially just `/healthz` + future module routers)
8. Spawn background tasks (deployer reconciler, log rotation — stubs for now)
9. Start server with graceful shutdown
10. Bootstrap admin user + system roles on first run (if `users` table is empty)

### 5. `src/lib.rs` — Module Registration

Update to export all modules:
```rust
pub mod config;
pub mod error;
pub mod store;
pub mod auth;
pub mod rbac;
pub mod api;
pub mod git;
pub mod pipeline;
pub mod deployer;
pub mod agent;
pub mod observe;
pub mod secrets;
pub mod notify;
```

### 6. Migrations — Core Schema

Create migrations in order. Each migration is a separate file:

```
migrations/
  20250219_001_users.sql           — users table
  20250219_002_roles_permissions.sql — roles, permissions, role_permissions
  20250219_003_user_roles.sql      — user_roles (global + project-scoped)
  20250219_004_delegations.sql     — delegation table
  20250219_005_auth_sessions.sql   — auth_sessions
  20250219_006_api_tokens.sql      — api_tokens
  20250219_007_projects.sql        — projects
  20250219_008_issues_comments.sql — issues, comments
  20250219_009_webhooks.sql        — webhooks
  20250219_010_merge_requests.sql  — merge_requests, mr_reviews
  20250219_011_agent_sessions.sql  — agent_sessions, agent_messages
  20250219_012_pipelines.sql       — pipelines, pipeline_steps, artifacts
  20250219_013_ops_repos.sql       — ops_repos
  20250219_014_deployments.sql     — deployments, deployment_history
  20250219_015_observability.sql   — traces, spans, log_entries, metric_series, metric_samples
  20250219_016_alerts.sql          — alert_rules, alert_events
  20250219_017_secrets.sql         — secrets
  20250219_018_notifications.sql   — notifications
  20250219_019_audit_log.sql       — audit_log with indexes
```

SQL schema exactly as defined in `plans/unified-platform.md` (the Core Tables section).

### 7. Bootstrap Logic

On first run (when `users` table is empty):
1. Insert system roles: `admin`, `developer`, `ops`, `agent`, `viewer`
2. Insert all permissions (from unified-platform.md RBAC section)
3. Wire role_permissions
4. Create initial admin user (username from config, password hashed with argon2id)
5. Assign admin role to initial user

---

## Testing

- Unit: config loading, error conversion
- Integration (`#[sqlx::test]`): pool connects, migrations apply, bootstrap creates roles/admin
- Health check endpoint returns 200

## Done When

1. `cargo check` compiles with all module stubs
2. `just db-migrate` applies all migrations
3. `just db-prepare` generates `.sqlx/` offline cache
4. `cargo run` starts, connects to Postgres + Valkey, runs bootstrap, serves `/healthz`
5. AppState is available in handlers

## Estimated LOC
~1,200 Rust
