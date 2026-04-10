# Production Readiness — "Don't Crash and Burn" Tier

Critical fixes before alpha. Addresses: queue locking for HA, graceful shutdown
with task supervision, connection pool configurability, and timeout/streaming
safety. Each section is a self-contained work unit with exact file locations,
current code, and target code.

**Dependency**: `tokio-util` needs the `rt` feature added (for `TaskTracker`).
Everything else uses deps already in `Cargo.toml`.

**Verified**: OpenDAL v0.55 has `Operator::writer()` returning `Writer` with
incremental `write(impl Into<Buffer>)` + `close()`, and `Operator::reader()`
with `into_bytes_stream()` yielding `Bytes` chunks. Both use `futures::` traits
(not `tokio::io::`), but `tokio_util::compat` is already in Cargo.toml for
bridging where needed.

---

## 1. Graceful Shutdown & Task Supervision

**Problem**: All 15+ background tasks are spawned via bare `tokio::spawn()` with
`JoinHandle` immediately dropped (`main.rs:379-422`). On SIGTERM:

1. `shutdown_tx.send(())` fires (line 338)
2. Process exits **immediately** — no wait for tasks to drain
3. Observe flush buffers (up to 30K records across 3 channels) are lost
4. In-flight pipeline executions and reconciliations are orphaned
5. If any spawned task panics, it dies silently — no restart, no detection beyond
   stale health check heartbeats (which take 30-90s to trigger)

**Current code** (`main.rs:329-341`):
```rust
axum::serve(listener, app)
    .with_graceful_shutdown(shutdown_signal())
    .await?;

// Signal background tasks to stop
let _ = shutdown_tx.send(());

tracing::info!("platform stopped");
Ok(())
```

**Current code** (`main.rs:368-424`):
```rust
fn spawn_background_tasks(...) -> (watch::Sender<()>, IngestChannels) {
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());

    tokio::spawn(label_platform_namespace(state.clone()));
    tokio::spawn(pipeline::executor::run(state.clone(), shutdown_rx.clone()));
    tokio::spawn(store::eventbus::run(state.clone(), shutdown_tx.subscribe()));
    // ... 12 more tokio::spawn() calls, all JoinHandles dropped ...

    (shutdown_tx, observe_channels)
}
```

### Changes

#### 1a. Add `rt` feature to `tokio-util` in `Cargo.toml`

**File**: `Cargo.toml:26`

```toml
# Before
tokio-util = { version = "0.7", features = ["io", "compat"] }

# After
tokio-util = { version = "0.7", features = ["io", "compat", "rt"] }
```

This unlocks `tokio_util::task::TaskTracker`.

#### 1b. Rewrite `spawn_background_tasks` to use `CancellationToken` + `TaskTracker`

**File**: `src/main.rs` — `spawn_background_tasks()`

Replace `watch::channel` with `CancellationToken` (cleaner API — child tokens,
no `changed()` gotchas). Replace bare `tokio::spawn()` with
`tracker.spawn(...)` so every task is tracked.

```rust
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

fn spawn_background_tasks(
    state: &store::AppState,
    pool: &sqlx::PgPool,
) -> (CancellationToken, TaskTracker, observe::ingest::IngestChannels) {
    let token = CancellationToken::new();
    let tracker = TaskTracker::new();

    // One-shot setup (no shutdown needed)
    tokio::spawn(label_platform_namespace(state.clone()));

    tracker.spawn(pipeline::executor::run(state.clone(), token.clone()));
    tracker.spawn(store::eventbus::run(state.clone(), token.clone()));
    tracker.spawn(deployer::reconciler::run(state.clone(), token.clone()));
    tracker.spawn(deployer::analysis::run(state.clone(), token.clone()));
    tracker.spawn(agent::service::run_reaper(state.clone(), token.clone()));
    tracker.spawn(agent::preview_watcher::run(state.clone(), token.clone()));
    let observe_channels =
        observe::spawn_background_tasks(state.clone(), token.clone(), &tracker);
    tracker.spawn(registry::gc::run(state.clone(), token.clone()));
    if state.config.ssh_listen.is_some() {
        tracker.spawn(git::ssh_server::run(state.clone(), token.clone()));
    }
    tracker.spawn(run_session_cleanup(
        pool.clone(),
        state.minio.clone(),
        state.secret_requests.clone(),
        token.clone(),
    ));
    tracker.spawn(health::checks::run(state.clone(), token.clone()));
    tracker.spawn(mesh::sync_trust_bundles(state.clone(), token.clone()));
    if state.config.gateway_auto_deploy {
        tracker.spawn(gateway::reconcile_gateway(state.clone(), token.clone()));
    }

    (token, tracker, observe_channels)
}
```

#### 1c. Rewrite shutdown sequence in `main()` to drain tasks

**File**: `src/main.rs` — end of `main()`

```rust
// After HTTP server stops gracefully:
tracing::info!("http server stopped, draining background tasks...");

// Signal all tasks to stop
token.cancel();

// Close the tracker — no new tasks can be spawned after this
tracker.close();

// Wait for all tracked tasks to finish, with a hard deadline
let drain_timeout = std::time::Duration::from_secs(30);
if tokio::time::timeout(drain_timeout, tracker.wait()).await.is_err() {
    tracing::warn!(
        "background tasks did not drain within {drain_timeout:?}, forcing exit"
    );
}

tracing::info!("platform stopped");
```

#### 1d. Migrate all background tasks from `watch::Receiver<()>` to `CancellationToken`

Every background task currently takes `mut shutdown: tokio::sync::watch::Receiver<()>`
and calls `shutdown.changed()` in a `tokio::select!`. Change to `CancellationToken`:

**Pattern** (apply to every background loop):
```rust
// Before (e.g. executor.rs:30)
pub async fn run(state: AppState, mut shutdown: tokio::sync::watch::Receiver<()>) {
    loop {
        tokio::select! {
            _ = shutdown.changed() => { break; }
            _ = interval.tick() => { /* work */ }
        }
    }
}

// After
pub async fn run(state: AppState, cancel: CancellationToken) {
    loop {
        tokio::select! {
            () = cancel.cancelled() => { break; }
            _ = interval.tick() => { /* work */ }
        }
    }
}
```

**Files requiring this signature change** (each has the same pattern):
- `src/pipeline/executor.rs:30` — `run()`
- `src/deployer/reconciler.rs:32` — `run()`
- `src/deployer/analysis.rs` — `run()`
- `src/agent/service.rs` — `run_reaper()`
- `src/agent/preview_watcher.rs` — `run()`
- `src/observe/ingest.rs:561,602,665` — `flush_spans()`, `flush_logs()`, `flush_metrics()`
- `src/observe/mod.rs` — `spawn_background_tasks()`, retention loop, `evaluate_alerts_loop()`
- `src/observe/alert.rs` — `evaluate_alerts_loop()`
- `src/registry/gc.rs` — `run()`
- `src/git/ssh_server.rs` — `run()`
- `src/store/eventbus.rs` — `run()`
- `src/health/checks.rs` — `run()`
- `src/mesh/mod.rs` — `sync_trust_bundles()`
- `src/gateway/mod.rs` — `reconcile_gateway()`
- `src/main.rs:448` — `run_session_cleanup()`

This is a mechanical find-and-replace. Each function:
1. Change parameter from `mut shutdown: watch::Receiver<()>` to `cancel: CancellationToken`
2. Change `shutdown.changed()` to `cancel.cancelled()` in `select!`
3. Change `shutdown_tx.subscribe()` call sites to `token.clone()` (already done in 1b)

#### 1e. Update `observe::spawn_background_tasks` to accept `TaskTracker`

**File**: `src/observe/mod.rs`

Currently spawns 7 tasks internally. Change to accept `&TaskTracker` and use
`tracker.spawn()` instead of `tokio::spawn()`:

```rust
// Before
pub fn spawn_background_tasks(
    state: AppState,
    shutdown: watch::Receiver<()>,
) -> IngestChannels {
    // ...
    tokio::spawn(ingest::flush_spans(pool.clone(), spans_rx, shutdown_rx.clone()));
    // ...
}

// After
pub fn spawn_background_tasks(
    state: AppState,
    cancel: CancellationToken,
    tracker: &TaskTracker,
) -> IngestChannels {
    // ...
    tracker.spawn(ingest::flush_spans(pool.clone(), spans_rx, cancel.clone()));
    // ...
}
```

#### 1f. Add timeout to observe flush drain on shutdown

**File**: `src/observe/ingest.rs` — all three flush functions

Currently, on shutdown the flush tasks do one `drain_*()` call which writes to
Postgres. If Postgres is slow/down, the drain hangs forever, blocking the entire
shutdown sequence.

```rust
// Before (flush_spans, line 571-573)
_ = shutdown.changed() => {
    drain_spans(&pool, &mut rx, &mut buffer).await;
    break;
}

// After
() = cancel.cancelled() => {
    // Best-effort drain with timeout — don't block shutdown
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        drain_spans(&pool, &mut rx, &mut buffer),
    ).await;
    break;
}
```

Apply same pattern to `flush_logs` and `flush_metrics`.

### Test plan

- **Unit**: Verify `CancellationToken` propagation — cancel parent, child tasks
  see cancellation
- **Integration**: Start platform, trigger SIGTERM, verify observe buffers are
  flushed (check DB row count before/after)
- **Integration**: Verify tasks that panic are tracked by `TaskTracker` (panicked
  task's `JoinHandle` resolves, tracker `wait()` completes)

---

## 2. Queue Locking for HA (FOR UPDATE SKIP LOCKED)

**Problem**: Pipeline executor (`executor.rs:84-93`) and deployer reconciler
(`reconciler.rs:83-98`) poll with plain `SELECT` — no row locking. Two replicas
will both fetch the same pending rows and race to process them.

The executor has a partial mitigation: `execute_pipeline()` does a conditional
`UPDATE ... WHERE status = 'pending'` (line 121-131), so only one replica
"claims" each pipeline. But between the initial `SELECT` and the `UPDATE`, both
replicas spawn tasks and hit the DB — wasted work and potential for subtle bugs.

The reconciler has no such mitigation — `reconcile_one()` begins work immediately
on the fetched row.

**Fix**: Use `SELECT ... FOR UPDATE SKIP LOCKED` to make polling queries
self-coordinating. Each replica locks only the rows it can claim; others skip
them and grab the next available work.

### Changes

#### 2a. Pipeline executor — locked polling query

**File**: `src/pipeline/executor.rs:83-106`

The current query uses `sqlx::query_scalar!` (compile-time checked). Since
`FOR UPDATE SKIP LOCKED` works with `query_scalar!`, we can keep compile-time
checking.

```rust
// Before (executor.rs:84-93)
async fn poll_pending(state: &AppState) -> Result<(), PipelineError> {
    let pending = sqlx::query_scalar!(
        r#"
        SELECT id FROM pipelines
        WHERE status = 'pending'
        ORDER BY created_at ASC
        LIMIT 5
        "#,
    )
    .fetch_all(&state.pool)
    .await?;

    for pipeline_id in pending {
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = execute_pipeline(&state, pipeline_id).await {
                tracing::error!(error = %e, %pipeline_id, "pipeline execution failed");
                let _ = mark_pipeline_failed(&state.pool, pipeline_id).await;
            }
        });
    }

    Ok(())
}

// After — atomic claim-and-spawn
async fn poll_pending(state: &AppState) -> Result<(), PipelineError> {
    // Claim up to 5 pending pipelines atomically: SELECT + UPDATE in one query.
    // FOR UPDATE SKIP LOCKED ensures other replicas skip already-claimed rows.
    let claimed = sqlx::query_scalar!(
        r#"
        UPDATE pipelines
        SET status = 'running', started_at = now()
        WHERE id IN (
            SELECT id FROM pipelines
            WHERE status = 'pending'
            ORDER BY created_at ASC
            LIMIT 5
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id
        "#,
    )
    .fetch_all(&state.pool)
    .await?;

    for pipeline_id in claimed {
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = execute_pipeline(&state, pipeline_id).await {
                tracing::error!(error = %e, %pipeline_id, "pipeline execution failed");
                let _ = mark_pipeline_failed(&state.pool, pipeline_id).await;
            }
        });
    }

    Ok(())
}
```

Then **remove the claim logic inside `execute_pipeline()`** (lines 121-137) since
the row is already claimed by the polling query. Replace with a simple metadata
fetch:

```rust
// Before (executor.rs:121-137)
let claimed = sqlx::query_scalar!(
    r#"UPDATE pipelines SET status = $2, started_at = now()
    WHERE id = $1 AND status = $3 RETURNING project_id"#,
    pipeline_id, to.as_str(), from.as_str(),
).fetch_optional(&state.pool).await?;

let Some(project_id) = claimed else {
    tracing::debug!(%pipeline_id, "pipeline already claimed or not in pending state");
    return Ok(());
};

// After
let project_id = sqlx::query_scalar!(
    r#"SELECT project_id FROM pipelines WHERE id = $1"#,
    pipeline_id,
).fetch_one(&state.pool).await?;
```

**Why this is safe without an explicit transaction**: The `UPDATE ... WHERE id IN
(SELECT ... FOR UPDATE SKIP LOCKED) RETURNING id` is a **single atomic
statement**. The lock is held for the duration of the statement, the rows are
updated to `'running'`, and the lock is released. Other pollers won't see these
rows because the status is no longer `'pending'` — the race is eliminated at the
SQL level.

**Note**: Since the `UPDATE` + `RETURNING` in `poll_pending` now atomically sets
`status = 'running'`, the state machine assertion (`from.can_transition_to(to)`)
in `execute_pipeline()` should be removed — the transition already happened.

#### 2b. Deployer reconciler — locked polling query

**File**: `src/deployer/reconciler.rs:82-98`

The reconciler uses a `sqlx::query()` (dynamic, not compile-time checked) with a
complex JOIN. We add `FOR UPDATE SKIP LOCKED` on the `deploy_releases` row:

```rust
// Before (reconciler.rs:83-95)
let pending = sqlx::query(
    "SELECT r.id, r.target_id, r.project_id, r.image_ref, r.commit_sha,
            r.strategy, r.phase, r.traffic_weight, r.current_step,
            r.rollout_config, r.values_override, r.deployed_by,
            r.tracked_resources, r.pipeline_id,
            dt.environment, dt.ops_repo_id, dt.manifest_path, dt.branch_slug, dt.hostname as target_hostname,
            p.name as project_name, p.namespace_slug
     FROM deploy_releases r
     JOIN deploy_targets dt ON dt.id = r.target_id
     JOIN projects p ON p.id = r.project_id AND p.is_active = true
     WHERE r.phase IN ('pending','progressing','holding','promoting','rolling_back')
     ORDER BY r.created_at ASC
     LIMIT 10",
)
.fetch_all(&state.pool)
.await?;

// After — add FOR UPDATE SKIP LOCKED on the release row
let pending = sqlx::query(
    "SELECT r.id, r.target_id, r.project_id, r.image_ref, r.commit_sha,
            r.strategy, r.phase, r.traffic_weight, r.current_step,
            r.rollout_config, r.values_override, r.deployed_by,
            r.tracked_resources, r.pipeline_id,
            dt.environment, dt.ops_repo_id, dt.manifest_path, dt.branch_slug, dt.hostname as target_hostname,
            p.name as project_name, p.namespace_slug
     FROM deploy_releases r
     JOIN deploy_targets dt ON dt.id = r.target_id
     JOIN projects p ON p.id = r.project_id AND p.is_active = true
     WHERE r.phase IN ('pending','progressing','holding','promoting','rolling_back')
     ORDER BY r.created_at ASC
     LIMIT 10
     FOR UPDATE OF r SKIP LOCKED",
)
.fetch_all(&state.pool)
.await?;
```

The `FOR UPDATE OF r` targets only the `deploy_releases` row (not the joined
tables), which is exactly what we want.

**Transaction semantics**: Unlike the executor's atomic `UPDATE ... RETURNING`,
this is a plain `SELECT ... FOR UPDATE SKIP LOCKED` — the row lock is held only
for the duration of the implicit autocommit transaction (i.e. the fetch). This is
fine: the lock prevents other pollers from selecting the same rows in the same
instant. Once `reconcile_one()` starts, it reads/updates the row via its own
queries. The `SKIP LOCKED` ensures no contention — competing replicas simply grab
different rows.

For stronger guarantees (lock held through reconciliation), wrap in an explicit
transaction. But this adds complexity (the spawned tasks would need to share the
transaction) and isn't necessary — the momentary de-duplication from `SKIP LOCKED`
is sufficient to prevent the duplicate-work race.

### Test plan

- **Integration**: Simulate two concurrent `poll_pending()` calls in a single
  test. Create 3 pending pipelines. Call `poll_pending` twice concurrently (two
  tasks). Verify each pipeline is claimed exactly once (no duplicates).
- **Integration**: Same test for reconciler — create 3 pending releases, two
  concurrent `reconcile()` calls, verify no duplicates.

---

## 3. Connection Pool Configurability

**Problem**: Both pools are hardcoded literals with no env var exposure:
- `src/store/pool.rs:12` — PgPool: `max_connections(20)`
- `src/store/valkey.rs:11` — Valkey: `Pool::new(..., 4)`

At ~50 concurrent requests the PG pool exhausts (20 connections shared between
API handlers, executor, reconciler, observe flush, session cleanup, health
checks). Valkey at 4 connections bottlenecks permission checks (every API
request).

**Confirmation**: sqlx does NOT hardcode any pool limit. The 20-connection cap is
entirely platform code. Same for fred — the `4` is the platform's choice.

### Changes

#### 3a. Add pool config fields to `Config`

**File**: `src/config.rs` — add to `Config` struct (after existing fields like
`pipeline_max_parallel`):

```rust
/// Maximum PostgreSQL connections (default 20).
pub db_max_connections: u32,
/// PostgreSQL connection acquire timeout in seconds (default 10).
pub db_acquire_timeout_secs: u64,
/// Maximum Valkey connections (default 6).
pub valkey_pool_size: usize,
```

**File**: `src/config.rs` — in `Config::from_env()`:

```rust
db_max_connections: env::var("PLATFORM_DB_MAX_CONNECTIONS")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(20),
db_acquire_timeout_secs: env::var("PLATFORM_DB_ACQUIRE_TIMEOUT")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(10),
valkey_pool_size: env::var("PLATFORM_VALKEY_POOL_SIZE")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(6),
```

Default PG stays at 20 (safe for dev/kind clusters). Default Valkey bumped
from 4 to 6 (reasonable for dev, won't break existing setups). Production
deployments set via env vars.

#### 3b. Pass config to pool creation

**File**: `src/store/pool.rs`

```rust
// Before
pub async fn connect(url: &str) -> anyhow::Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(20)
        .acquire_timeout(Duration::from_secs(10))
        .idle_timeout(Duration::from_secs(300))
        .connect(url)
        .await?;

// After
pub async fn connect(
    url: &str,
    max_connections: u32,
    acquire_timeout_secs: u64,
) -> anyhow::Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(acquire_timeout_secs))
        .idle_timeout(Duration::from_secs(300))
        .max_lifetime(Duration::from_secs(1800)) // 30 min — recycle stale conns
        .connect(url)
        .await?;
```

Also adds `max_lifetime` — connections are recycled after 30 minutes, preventing
stale TCP state after Postgres failovers. This was missing entirely.

**File**: `src/store/valkey.rs`

```rust
// Before
pub async fn connect(url: &str) -> anyhow::Result<fred::clients::Pool> {
    let config = fred::types::config::Config::from_url(url)?;
    let pool = fred::clients::Pool::new(config, None, None, None, 4)?;

// After
pub async fn connect(url: &str, pool_size: usize) -> anyhow::Result<fred::clients::Pool> {
    let config = fred::types::config::Config::from_url(url)?;
    let pool = fred::clients::Pool::new(config, None, None, None, pool_size)?;
```

#### 3c. Update call sites in `main.rs`

**File**: `src/main.rs` — where pools are created:

```rust
// Before
let pool = store::pool::connect(&cfg.database_url).await?;
let valkey = store::valkey::connect(&cfg.valkey_url).await?;

// After
let pool = store::pool::connect(
    &cfg.database_url,
    cfg.db_max_connections,
    cfg.db_acquire_timeout_secs,
).await?;
let valkey = store::valkey::connect(&cfg.valkey_url, cfg.valkey_pool_size).await?;
```

#### 3d. Update `.env.example`

Add the new env vars with comments:

```bash
# Connection pool sizing (increase for production)
# PLATFORM_DB_MAX_CONNECTIONS=20
# PLATFORM_DB_ACQUIRE_TIMEOUT=10
# PLATFORM_VALKEY_POOL_SIZE=6
```

#### 3e. Update test helpers

**File**: `tests/helpers/mod.rs` — `test_state()` creates its own pool. The
`connect()` signature change means test helpers need updating:

```rust
// Tests can continue using small pools (they're isolated per-test)
let pool = store::pool::connect(&database_url, 5, 10).await?;
let valkey = store::valkey::connect(&valkey_url, 2).await?;
```

Small pool sizes in tests are fine — each test has its own PG database via
`#[sqlx::test]`.

### Test plan

- **Unit**: Verify `Config::from_env()` parses the new env vars correctly
  (set env, construct config, assert values)
- **Integration**: Existing tests pass with updated `connect()` signatures
  (compile-time verified)

---

## 4. Global Request Timeout

**Problem**: No timeout middleware on the axum router (`main.rs:260-327`). A slow
DB query, K8s API call, or hung git process blocks a handler forever, consuming
a connection and a tokio task.

`tower 0.5` is already a dep with `features = ["full"]` (Cargo.toml:20), which
includes `tower::timeout::TimeoutLayer`.

### Changes

#### 4a. Add global timeout to router

**File**: `src/main.rs` — add `TimeoutLayer` to the router layer stack, **after**
the body limit layers but **before** the CORS layer (CORS preflight should be
fast and won't trigger timeout):

```rust
use tower::timeout::TimeoutLayer;

let app = axum::Router::new()
    .route("/healthz", ...)
    .route("/readyz", ...)
    .merge(api::router())
    .merge(api::preview::router())
    .merge(observe::router(observe_channels))
    .merge(
        git::git_protocol_router()
            .layer(DefaultBodyLimit::disable())
            .layer(RequestBodyLimitLayer::new(cfg.registry_http_body_limit_bytes)),
    )
    .merge(
        registry::router()
            .layer(DefaultBodyLimit::disable())
            .layer(RequestBodyLimitLayer::new(cfg.registry_http_body_limit_bytes)),
    )
    .layer(axum::middleware::from_fn(request_tracing_middleware))
    .with_state(state)
    .fallback(ui::static_handler)
    .layer(DefaultBodyLimit::max(10 * 1024 * 1024))
    // Global request timeout: 5 minutes.
    // Git push and registry uploads may need more — they get per-route overrides.
    .layer(TimeoutLayer::new(std::time::Duration::from_secs(300)))
    // Security headers...
    .layer(SetResponseHeaderLayer::if_not_present(...))
    // ...
```

**Gotcha — JSON error body**: Tower's `TimeoutLayer` generates a `BoxError`
wrapping `tower::timeout::error::Elapsed` at the **service layer**, which bypasses
Axum's handler-level `IntoResponse` for `ApiError`. The client would get a raw
408 with no JSON body. Fix: add a `HandleError` layer that converts the timeout
into our JSON error format.

Add this handler function in `src/main.rs`:

```rust
use axum::BoxError;

/// Convert tower service-layer errors (e.g. timeout) into JSON API responses.
async fn handle_timeout_error(err: BoxError) -> Response {
    if err.is::<tower::timeout::error::Elapsed>() {
        ApiError::ServiceUnavailable("request timeout".into()).into_response()
    } else {
        tracing::error!(error = %err, "unhandled tower service error");
        ApiError::Internal(anyhow::anyhow!("service error")).into_response()
    }
}
```

Then use axum's documented `ServiceBuilder` + `HandleErrorLayer` pattern (from
axum 0.8's own error_handling docs — this is the idiomatic way):

```rust
use tower::ServiceBuilder;
use axum::error_handling::HandleErrorLayer;

let app = axum::Router::new()
    // ... routes ...
    .layer(DefaultBodyLimit::max(10 * 1024 * 1024))
    .layer(
        ServiceBuilder::new()
            .layer(HandleErrorLayer::new(handle_timeout_error))
            .timeout(std::time::Duration::from_secs(300))
    )
    // ... security headers ...
```

**Layer order in `ServiceBuilder`**: `HandleErrorLayer` is listed first but wraps
the timeout — it catches the `Elapsed` error from `timeout()` and converts it to
our JSON `ApiError` format. Without it, clients get a raw 408 with no body.

For git/registry streaming endpoints, a timeout mid-stream will close the
connection — correct behavior (better than hanging forever).

#### 4b. Per-route timeout overrides for long-running operations

Git push/clone and registry uploads may legitimately take longer than 5 minutes
(large repos, large images). Apply a longer timeout on those sub-routers:

```rust
.merge(
    git::git_protocol_router()
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(cfg.registry_http_body_limit_bytes))
        .layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(handle_timeout_error))
                .timeout(std::time::Duration::from_secs(1800)) // 30 min
        ),
)
.merge(
    registry::router()
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(cfg.registry_http_body_limit_bytes))
        .layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(handle_timeout_error))
                .timeout(std::time::Duration::from_secs(1800)) // 30 min
        ),
)
```

The per-route `ServiceBuilder` timeout (innermost) takes precedence over the
global 5-minute timeout. Each sub-router gets its own `HandleErrorLayer` so
timeout errors on git/registry routes also produce JSON responses.

### Test plan

- **Integration**: Send a request to a test endpoint that sleeps for 10s, verify
  HTTP 408 response with global timeout set to 2s
- **Integration**: Verify git clone completes within 30-min window (existing tests
  should still pass)

---

## 5. Git Process Timeouts

**Problem**: `git receive-pack` and `git upload-pack` child processes have no
timeout. A stalled git process blocks the handler forever.

- `smart_http.rs:500-545` — `receive_pack()`: `tokio::join!` on stdin/stdout +
  `child.wait()` — no timeout on any of these
- `smart_http.rs:698-740` — `run_git_service()` (upload-pack): `body.collect()`
  buffers entire request, `child.wait()` implicit via stdout stream — no timeout

Meanwhile, `git/browser.rs` **already** uses `tokio::time::timeout(GIT_TIMEOUT, ...)`
with a 30s constant. The smart HTTP handlers just never got the same treatment.

### Changes

#### 5a. Add git operation timeout config

**File**: `src/config.rs` — add:

```rust
/// Git smart HTTP operation timeout in seconds (default 600 = 10 min).
pub git_http_timeout_secs: u64,
```

In `from_env()`:

```rust
git_http_timeout_secs: env::var("PLATFORM_GIT_HTTP_TIMEOUT")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(600),
```

#### 5b. Wrap `receive_pack` in timeout

**File**: `src/git/smart_http.rs` — around the `tokio::join!` + `child.wait()`
block in `receive_pack()` (lines 514-545):

```rust
let git_timeout = std::time::Duration::from_secs(state.config.git_http_timeout_secs);

let git_result = tokio::time::timeout(git_timeout, async {
    // Phase 2: Pipe buffered pkt-line header + stream remaining body to git stdin
    let (stdin_result, stdout_bytes) = tokio::join!(
        async {
            stdin.write_all(&pkt_buf).await?;
            if let Some(remaining) = remaining_frame {
                stdin.write_all(&remaining).await?;
            }
            while let Some(frame_result) = body_stream.next().await {
                let frame = frame_result.map_err(std::io::Error::other)?;
                stdin.write_all(&frame).await?;
            }
            stdin.shutdown().await?;
            Ok::<(), std::io::Error>(())
        },
        async {
            let mut buf = Vec::new();
            stdout.read_to_end(&mut buf).await?;
            Ok::<Vec<u8>, std::io::Error>(buf)
        }
    );
    stdin_result.map_err(|e| anyhow::anyhow!("stdin write: {e}"))?;
    let output = stdout_bytes.map_err(|e| anyhow::anyhow!("stdout read: {e}"))?;

    let status = child.wait().await
        .map_err(|e| anyhow::anyhow!("git wait: {e}"))?;
    Ok::<(Vec<u8>, std::process::ExitStatus), anyhow::Error>((output, status))
}).await;

let (output, status) = match git_result {
    Ok(Ok(v)) => v,
    Ok(Err(e)) => return Err(ApiError::Internal(e)),
    Err(_elapsed) => {
        // Kill the hung child process
        let _ = child.kill().await;
        return Err(ApiError::Internal(anyhow::anyhow!(
            "git receive-pack timed out after {}s", state.config.git_http_timeout_secs
        )));
    }
};
```

**Critical**: On timeout, we `child.kill()` the git process. Without this, the
zombie process would linger.

#### 5c. Fix `run_git_service` body buffering + add timeout

**File**: `src/git/smart_http.rs` — `run_git_service()` (lines 698-740)

Current problem: `body.collect().await.to_bytes()` buffers the **entire** upload-
pack request body (up to 2 GB) before writing to stdin.

```rust
// Before (line 714-720)
let bytes = body
    .collect()
    .await
    .map_err(|e| anyhow::anyhow!("body read: {e}"))?
    .to_bytes();
stdin.write_all(&bytes).await?;

// After — stream body to stdin frame-by-frame (same as receive_pack)
let mut body_stream = body.into_data_stream();
while let Some(frame_result) = body_stream.next().await {
    let frame = frame_result.map_err(|e| anyhow::anyhow!("body read: {e}"))?;
    stdin.write_all(&frame).await?;
}
stdin.shutdown().await?;
```

Also add a timeout to the stdin-piping spawn so it can't hang forever:

```rust
let git_timeout = std::time::Duration::from_secs(config.git_http_timeout_secs);
let stdin_handle = tokio::spawn(async move {
    let result = tokio::time::timeout(git_timeout, async {
        let mut body_stream = body.into_data_stream();
        while let Some(frame_result) = body_stream.next().await {
            let frame = frame_result.map_err(|e| anyhow::anyhow!("body read: {e}"))?;
            stdin.write_all(&frame).await?;
        }
        stdin.shutdown().await?;
        Ok::<(), anyhow::Error>(())
    }).await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "stdin pipe failed"),
        Err(_) => tracing::warn!("git upload-pack stdin pipe timed out"),
    }
});
```

Pass `config` (or just the timeout duration) into `run_git_service()`. The
function signature changes from:

```rust
fn run_git_service(repo_path: &Path, service: &str, body: Body) -> Result<Response, ApiError>
```

to:

```rust
fn run_git_service(
    repo_path: &Path,
    service: &str,
    body: Body,
    timeout_secs: u64,
) -> Result<Response, ApiError>
```

### Test plan

- **Integration**: Existing git push/clone tests pass (timeout is 10 min, well
  above test duration)
- **Unit**: Verify timeout fires — mock a never-completing child process, assert
  timeout error

---

## 6. Registry Blob Streaming

**Problem**: Three code paths buffer entire blobs in memory:

1. **Chunk upload** (`blobs.rs:178,201`) — `body: Bytes` extracts full chunk,
   `body.to_vec()` copies it. A 2 GB chunk = 4+ GB heap.
2. **Complete upload** (`blobs.rs:287-298`) — reassembles all parts into
   `full_data: Vec<u8>` in memory. 5 GB blob = 5 GB heap + SHA256 pass.
3. **Blob GET** (`blobs.rs:97`) — when `registry_proxy_blobs=true`, reads entire
   blob from MinIO into memory.

### Changes

#### 6a. Stream chunk uploads to MinIO

**File**: `src/registry/blobs.rs` — `upload_chunk()` (line 174)

Change the body extractor from `Bytes` to `axum::body::Body` and stream it.
Requires adding `use futures_util::StreamExt;` to `blobs.rs` (already used in
`smart_http.rs:13`):

```rust
// Before
pub async fn upload_chunk(
    State(state): State<AppState>,
    user: RegistryUser,
    Path((name, upload_id)): Path<(String, String)>,
    body: axum::body::Bytes,  // full buffer
) -> Result<Response, RegistryError> {
    // ...
    let chunk_data = body.to_vec();
    state.minio.write(&part_path, chunk_data).await?;

// After — stream body to MinIO via opendal Writer
pub async fn upload_chunk(
    State(state): State<AppState>,
    user: RegistryUser,
    Path((name, upload_id)): Path<(String, String)>,
    body: axum::body::Body,  // streaming
) -> Result<Response, RegistryError> {
    // ... session lookup unchanged ...

    let part_path = format!("registry/uploads/{upload_uuid}/part-{}", session.part_count);
    let mut writer = state.minio.writer(&part_path).await?;

    let mut chunk_size: i64 = 0;
    let mut body_stream = body.into_data_stream();
    while let Some(frame) = body_stream.next().await {
        let frame = frame.map_err(|e| RegistryError::Internal(e.into()))?;
        chunk_size += i64::try_from(frame.len())
            .map_err(|e| RegistryError::Internal(anyhow::anyhow!("{e}")))?;
        writer.write(frame).await?;
    }
    writer.close().await?;

    session.offset += chunk_size;
    // ... rest unchanged ...
```

This streams the body frame-by-frame to MinIO with constant memory usage.

**Verified**: OpenDAL 0.55's `Operator::writer()` returns `Writer` with
`write(impl Into<Buffer>)` — accepts `Bytes`, `Vec<u8>`, `&[u8]` directly.
`close()` finalizes the multipart upload and returns `Metadata`. For higher
throughput, use `writer_with(path).chunk(5*1024*1024).concurrent(8).await?`
to enable concurrent multipart upload with 5 MB chunks.

#### 6b. Streaming digest verification for complete_upload

**File**: `src/registry/blobs.rs` — `complete_upload()` (line 240)

The hardest problem: complete_upload reassembles all parts + final body, computes
SHA256, and writes the final blob. Currently all in memory.

Strategy: **incremental SHA256** using `sha2::Sha256` and stream-through write.
Peak memory drops from "all parts combined" to "one part at a time" (or one chunk
if using opendal's streaming reader).

```rust
use sha2::{Sha256, Digest as Sha2Digest};

// Replace lines 287-313
let mut hasher = Sha256::new();
let mut total_size: i64 = 0;
let final_path = expected_digest.minio_path();
let mut writer = state.minio
    .writer_with(&final_path)
    .chunk(5 * 1024 * 1024)  // 5 MB multipart chunks
    .concurrent(4)            // 4 concurrent uploads
    .await?;

// Stream existing parts through hasher → MinIO (one part at a time)
for i in 0..session.part_count {
    let part_path = format!("registry/uploads/{upload_uuid}/part-{i}");
    let reader = state.minio.reader(&part_path).await?;
    let mut stream = reader.into_bytes_stream(..).await?;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        total_size += i64::try_from(chunk.len()).unwrap_or(0);
        writer.write(chunk).await?;
    }
}

// Stream final chunk through hasher → MinIO
if !body.is_empty() {
    hasher.update(&body);
    total_size += i64::try_from(body.len()).unwrap_or(0);
    writer.write(body).await?;  // Bytes impl Into<Buffer>
}

writer.close().await?;

// Verify digest
let actual_hash = hex::encode(hasher.finalize());
let actual_digest = Digest::new("sha256", &actual_hash);
if actual_digest != expected_digest {
    // Delete the incorrectly-written blob
    let _ = state.minio.delete(&final_path).await;
    return Err(RegistryError::DigestInvalid(format!(
        "expected {expected_digest}, got {actual_digest}"
    )));
}

let size_bytes = total_size;
```

This streams each part chunk-by-chunk from MinIO reader through the SHA256 hasher
into the MinIO writer. Peak memory is one read chunk (~few MB), not the entire
blob. The `writer_with().chunk(5MB).concurrent(4)` enables efficient multipart
upload on the write side.

**Fallback**: If `reader.into_bytes_stream()` proves problematic (e.g. small parts
aren't worth streaming), `state.minio.read(&part_path).await?.to_bytes()` reads
one part at a time — still a major improvement over the current all-in-memory
approach.

#### 6c. Stream blob GET responses

**File**: `src/registry/blobs.rs` — `get_blob()` handler

When `registry_proxy_blobs = true`, current code reads entire blob into memory:

```rust
// Before
let data = state.minio.read(&blob.minio_path).await?;
Ok((StatusCode::OK, headers, data.to_vec()).into_response())

// After — stream from MinIO using into_bytes_stream()
let reader = state.minio.reader(&blob.minio_path).await?;
let stream = reader.into_bytes_stream(..).await?;
// into_bytes_stream yields Result<Bytes> items — compatible with Body::from_stream
let body = axum::body::Body::from_stream(stream);
Ok((StatusCode::OK, headers, body).into_response())
```

This streams the blob directly from MinIO to the HTTP client with constant memory.

**Verified**: OpenDAL 0.55 `Reader::into_bytes_stream(range)` returns a
`FuturesBytesStream` implementing `futures::Stream<Item = Result<Bytes>>`. Axum's
`Body::from_stream()` accepts any `Stream<Item = Result<T, E>>` where `T: Into<Bytes>`,
so this connects directly — no `tokio_util::compat` bridge needed for this path.

For the `into_futures_async_read()` path (if needed elsewhere), OpenDAL returns a
`FuturesAsyncReader` implementing `futures::AsyncRead`. Bridge to tokio with:
```rust
use tokio_util::compat::FuturesAsyncReadCompatExt;
let tokio_reader = reader.into_futures_async_read(..).await?.compat();
let stream = tokio_util::io::ReaderStream::new(tokio_reader);
```

### Test plan

- **Integration**: Existing registry push/pull tests pass (streaming is
  transparent to the protocol)
- **Integration**: Upload a multi-chunk blob, verify digest matches
- **Manual**: Push a large image (~500 MB) and monitor RSS — should stay flat
  instead of spiking

---

## Implementation Order

The sections have minimal interdependence. Recommended order:

```
3. Connection pools (15 min)         — trivial, unblocks everything else
   ↓
4. Global request timeout (15 min)   — trivial, immediate safety net
   ↓
5. Git process timeouts (1 hr)       — requires config plumbing + careful error handling
   ↓
1. Graceful shutdown (2-3 hr)        — largest change, touches all background tasks
   ↓
2. Queue locking (1 hr)              — SQL changes + remove redundant claim logic
   ↓
6. Registry streaming (2-3 hr)       — opendal Writer API exploration needed
```

Total: ~8-10 hours of focused work.

After each section, run `just test-unit` to verify compilation. After all
sections complete, run `just ci-full`.

---

## Env Var Summary

New environment variables added by this plan:

| Env Var | Default | Purpose |
|---------|---------|---------|
| `PLATFORM_DB_MAX_CONNECTIONS` | `20` | PgPool max connections |
| `PLATFORM_DB_ACQUIRE_TIMEOUT` | `10` | PgPool acquire timeout (seconds) |
| `PLATFORM_VALKEY_POOL_SIZE` | `6` | Valkey connection pool size |
| `PLATFORM_GIT_HTTP_TIMEOUT` | `600` | Git smart HTTP operation timeout (seconds) |

## Dependency Changes

| Crate | Change |
|-------|--------|
| `tokio-util` | Add `rt` feature (for `TaskTracker`) |

No new crate dependencies.
