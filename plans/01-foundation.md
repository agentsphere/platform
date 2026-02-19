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

### 6. Migrations — Full Schema (created upfront)

All migrations are created in this foundation step so the schema is complete on day 1. This enables `sqlx` compile-time query checking across all modules from the start of parallel development. Use `just db-add <name>` to create up/down pairs.

**Important**: The schema includes all fixes from the review — `set_updated_at()` trigger function, renamed reserved-word columns (`git_ref`, `metric_type`, `notification_type`), missing indexes, fixed FK cascades, `comments` table with MR support, `projects.next_issue_number`/`next_mr_number` counters, `pipeline_steps.step_order`, and immutable `audit_log` (no FK on `actor_id`).

```
migrations/
  20260220_010001_utility.sql          — set_updated_at() trigger function
  20260220_010002_users.sql            — users table + trigger
  20260220_010003_roles_permissions.sql — roles, permissions, role_permissions
  20260220_010004_user_roles.sql       — user_roles + index
  20260220_010005_delegations.sql      — delegations + delegate_id index
  20260220_010006_auth_sessions.sql    — auth_sessions + user_id index
  20260220_010007_api_tokens.sql       — api_tokens + user_id index
  20260220_010008_projects.sql         — projects (with is_active, next_issue/mr_number) + trigger
  20260220_010009_issues.sql           — issues + trigger
  20260220_010010_merge_requests.sql   — merge_requests, mr_reviews (with project_id) + trigger
  20260220_010011_comments.sql         — comments (issue_id nullable, mr_id, project_id, CHECK)
  20260220_010012_webhooks.sql         — webhooks
  20260220_010013_agent_sessions.sql   — agent_sessions + indexes, agent_messages
  20260220_010014_pipelines.sql        — pipelines (git_ref, not ref) + indexes, pipeline_steps (step_order, project_id), artifacts
  20260220_010015_ops_repos.sql        — ops_repos
  20260220_010016_deployments.sql      — deployments + trigger + reconcile index, deployment_history
  20260220_010017_observability.sql    — traces, spans, log_entries + indexes, metric_series (metric_type), metric_samples
  20260220_010018_alerts.sql           — alert_rules, alert_events + status index
  20260220_010019_secrets.sql          — secrets + trigger + global name index
  20260220_010020_notifications.sql    — notifications (notification_type) + user_status index
  20260220_010021_audit_log.sql        — audit_log (no FK on actor_id, has actor_name) + indexes
```

SQL schema exactly as defined in `plans/unified-platform.md` (the Core Tables section), with all reviewed fixes applied.

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
