# Production Hardening — Tranche 2

Second wave of production readiness fixes. Tranche 1 (`production-readiness.md`)
covers the crash-prevention tier: graceful shutdown, HA locking, pool config,
timeouts, and streaming. This tranche covers the next layer: safe startup,
health observability, rate limiting, resource configurability, and database
performance.

**Prerequisite**: Tranche 1 landed (CancellationToken, FOR UPDATE SKIP LOCKED,
configurable pools, TimeoutLayer, git timeouts, streaming blobs).

**No new crate dependencies.** All changes use existing deps.

---

## 1. Safe Production Startup

**Problem**: The platform starts happily with dev defaults in production. No
validation fails startup when critical config is missing. Specific gaps:

- `PLATFORM_MASTER_KEY` missing in production: logs a warning, silently disables
  secrets engine (`main.rs:105`)
- MinIO credentials default to `platform`/`devdevdev` (`config.rs:216-217`)
- `PLATFORM_DEV=true` allows 50 auth attempts vs 5, random master key per restart
- No warning that dev mode is active
- `parse_master_key()` uses `.expect()` (line 93) — panics on invalid key instead
  of returning a clean error
- No startup timeout — `pool::connect()` and `valkey::connect()` hang indefinitely
  if network is misconfigured

### Changes

#### 1a. Add `Config::validate()` method

**File**: `src/config.rs` — add after `Config::load()`:

```rust
impl Config {
    /// Validate configuration for production readiness.
    /// Returns a list of warnings and a list of fatal errors.
    /// Called after load() but before connecting to services.
    pub fn validate(&self) -> (Vec<String>, Vec<String>) {
        let mut warnings = Vec::new();
        let mut errors = Vec::new();

        if self.dev_mode {
            warnings.push(
                "PLATFORM_DEV=true — dev mode enabled. \
                 DO NOT use in production."
                    .into(),
            );
        }

        if !self.dev_mode {
            // Production-only checks
            if self.master_key.is_none() {
                errors.push(
                    "PLATFORM_MASTER_KEY is required in production. \
                     Set it to a 64-character hex string (32 bytes). \
                     Secrets engine cannot function without it."
                        .into(),
                );
            }

            if self.minio_access_key == "platform" && self.minio_secret_key == "devdevdev" {
                errors.push(
                    "MinIO credentials are still set to dev defaults \
                     (platform/devdevdev). Set MINIO_ACCESS_KEY and \
                     MINIO_SECRET_KEY to production values."
                        .into(),
                );
            }

            if !self.secure_cookies {
                warnings.push(
                    "PLATFORM_SECURE_COOKIES=false — session cookies lack \
                     Secure flag. Set to true when behind HTTPS."
                        .into(),
                );
            }

            if self.cors_origins.is_empty() {
                warnings.push(
                    "PLATFORM_CORS_ORIGINS is empty — all cross-origin \
                     requests will be denied."
                        .into(),
                );
            }
        }

        // Universal checks
        if let Some(ref mk) = self.master_key {
            if let Err(e) = secrets::engine::validate_master_key(mk) {
                errors.push(format!("PLATFORM_MASTER_KEY is invalid: {e}"));
            }
        }

        (warnings, errors)
    }
}
```

#### 1b. Call validate() in main() and fail on errors

**File**: `src/main.rs` — after `Config::load()`, before connecting to services:

```rust
let cfg = config::Config::load();

// Validate configuration before connecting to anything
let (warnings, errors) = cfg.validate();
for w in &warnings {
    tracing::warn!("{w}");
}
if !errors.is_empty() {
    for e in &errors {
        tracing::error!("{e}");
    }
    anyhow::bail!(
        "startup aborted: {} configuration error(s). \
         Fix the errors above and restart.",
        errors.len()
    );
}
```

#### 1c. Add `validate_master_key()` to secrets engine

**File**: `src/secrets/engine.rs` — add a non-panicking validation function:

```rust
/// Validate master key format without panicking.
/// Returns Ok(()) if the key is a valid 64-char hex string (32 bytes).
pub fn validate_master_key(key: &str) -> Result<(), String> {
    if key.len() != 64 {
        return Err(format!("expected 64 hex characters, got {}", key.len()));
    }
    hex::decode(key).map_err(|e| format!("not valid hex: {e}"))?;
    Ok(())
}
```

Then replace the `.expect()` in `main.rs:93`:

```rust
// Before
secrets::engine::parse_master_key(mk).expect("PLATFORM_MASTER_KEY is invalid");

// After — already validated by Config::validate(), but belt-and-suspenders
let _key = secrets::engine::parse_master_key(mk)
    .context("PLATFORM_MASTER_KEY is invalid (passed validation but failed parse)")?;
```

#### 1d. Add startup connection timeout

**File**: `src/main.rs` — wrap connection establishment in a timeout:

```rust
let startup_timeout = std::time::Duration::from_secs(30);

let pool = tokio::time::timeout(
    startup_timeout,
    store::pool::connect(&cfg.database_url, cfg.db_max_connections, cfg.db_acquire_timeout_secs),
)
.await
.context("timed out connecting to Postgres (30s)")??;

let valkey = tokio::time::timeout(
    startup_timeout,
    store::valkey::connect(&cfg.valkey_url, cfg.valkey_pool_size),
)
.await
.context("timed out connecting to Valkey (30s)")??;
```

### Test plan

- **Unit**: `Config::validate()` returns errors for missing master key in non-dev mode
- **Unit**: `Config::validate()` returns errors for default MinIO credentials in non-dev mode
- **Unit**: `Config::validate()` passes in dev mode with missing master key
- **Unit**: `validate_master_key()` rejects short/invalid hex strings

---

## 2. Health Check Improvements

**Problem**: Health checks have significant gaps:

- `/healthz` (liveness) always returns "ok" with no logic — K8s will never
  restart a stuck process (`main.rs:261`)
- `/readyz` (readiness) only checks Postgres + Valkey, ignores MinIO and K8s API
  (`checks.rs:362-380`)
- Background task stale detection takes 3x interval (30-90s for most tasks) before
  marking unhealthy — too aggressive for false positives, too slow for real failures
- Health snapshot can go stale indefinitely if the health check loop panics
- `is_ready()` uses a 60-second cached snapshot with no fallback when stale

### Changes

#### 2a. Make liveness check meaningful

**File**: `src/main.rs` — replace the static healthz handler:

```rust
// Before
.route("/healthz", axum::routing::get(|| async { "ok" }))

// After — check that critical background tasks are alive
.route(
    "/healthz",
    axum::routing::get({
        let s = state.clone();
        move || {
            let s = s.clone();
            async move {
                // Liveness: are critical tasks still running?
                // If the task registry shows critical tasks as stale,
                // the process is wedged and should be restarted.
                let critical_tasks = [
                    "pipeline_executor",
                    "deployer_reconciler",
                ];
                let all_alive = critical_tasks.iter().all(|name| {
                    s.task_registry.is_healthy(name)
                });
                if all_alive {
                    (StatusCode::OK, "ok")
                } else {
                    (StatusCode::SERVICE_UNAVAILABLE, "critical task stale")
                }
            }
        }
    }),
)
```

This requires adding an `is_healthy(name) -> bool` method to `TaskRegistry`:

**File**: `src/health/types.rs` — add to `TaskRegistry`:

```rust
/// Check if a named task is healthy (not stale, no recent error).
///
/// Uses `read()` on the RwLock — safe for concurrent K8s probe calls.
/// Background tasks must only hold the `write()` lock for the duration
/// of updating their heartbeat timestamp (microseconds), never across
/// any async .await point, to avoid blocking liveness probes.
pub fn is_healthy(&self, name: &str) -> bool {
    let tasks = self.tasks.read().unwrap_or_else(|e| e.into_inner());
    match tasks.get(name) {
        Some(hb) => {
            let elapsed = std::time::Instant::now().duration_since(hb.last_beat);
            let stale_threshold =
                std::time::Duration::from_secs(hb.expected_interval_secs * 3);
            elapsed <= stale_threshold
        }
        None => true, // Task not registered yet (startup race) — assume healthy
    }
}
```

#### 2b. Add MinIO to readiness check

**File**: `src/health/checks.rs` — update `is_ready()`:

```rust
// Before (checks.rs:362-380)
pub async fn is_ready(state: &AppState) -> bool {
    if let Ok(snap) = state.health.read() {
        let age = Utc::now() - snap.checked_at;
        if age.num_seconds() < 60 {
            return snap.subsystems.iter()
                .any(|s| s.name == "postgres" && s.status == SubsystemStatus::Healthy)
                && snap.subsystems.iter()
                .any(|s| s.name == "valkey" && s.status == SubsystemStatus::Healthy);
        }
    }
    let (pg, vk) = tokio::join!(
        check_postgres(&state.pool),
        check_valkey(&state.valkey),
    );
    pg.status == SubsystemStatus::Healthy && vk.status == SubsystemStatus::Healthy
}

// After — add MinIO, short cache, add fallback
pub async fn is_ready(state: &AppState) -> bool {
    if let Ok(snap) = state.health.read() {
        let age = Utc::now() - snap.checked_at;
        if age.num_seconds() < 15 {  // 15s cache — must be ≤ K8s probe period
            let required = ["postgres", "valkey", "minio"];
            return required.iter().all(|name| {
                snap.subsystems.iter().any(|s| {
                    s.name == *name
                        && matches!(
                            s.status,
                            SubsystemStatus::Healthy | SubsystemStatus::Degraded
                        )
                })
            });
        }
    }
    // Stale or missing snapshot — run live probes
    let (pg, vk, minio) = tokio::join!(
        check_postgres(&state.pool),
        check_valkey(&state.valkey),
        check_minio(&state.minio),
    );
    let ok = |s: &SubsystemCheck| {
        matches!(s.status, SubsystemStatus::Healthy | SubsystemStatus::Degraded)
    };
    ok(&pg) && ok(&vk) && ok(&minio)
}
```

**Design choices**:
- MinIO added to readiness (required for artifacts, registry, parquet rotation)
- K8s intentionally NOT added — platform can serve API requests without K8s access
  (pipeline/deploy will fail, but API is still useful)
- Degraded counts as ready — only Unhealthy fails readiness
- Cache TTL lowered to 15s (≤ K8s probe period). The old 60s cache was too long:
  if the DB drops, K8s would keep routing traffic to the pod for up to 60s because
  the cached "healthy" snapshot wouldn't expire. With 15s, the pod drops out of
  the load balancer within one probe cycle. K8s already rate-limits probe frequency,
  so caching beyond the probe interval just delays failure detection.

### Test plan

- **Integration**: Kill MinIO, verify `/readyz` returns 503
- **Integration**: Verify `/healthz` returns 200 when all tasks are alive
- **Unit**: `TaskRegistry::is_healthy()` returns false when heartbeat is stale

---

## 3. Rate Limiting Gaps

**Problem**: 5 critical mutation endpoints have no rate limiting. An attacker or
runaway automation can create unlimited projects, trigger unlimited pipelines,
or generate unlimited API tokens.

**Current coverage**: 20+ endpoints protected (auth, passkeys, CLI, OTLP, git
auth, sessions, SSH/GPG keys). The gaps are all creation/mutation endpoints.

### Changes

#### 3a. Add rate limits to 5 unprotected endpoints

All use the existing `check_rate()` API. Each is a one-line addition at the
top of the handler, after auth extraction but before any business logic.

**File**: `src/api/projects.rs` — `create_project()`:

```rust
async fn create_project(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<CreateProjectRequest>,
) -> Result<Json<ProjectResponse>, ApiError> {
    // Rate limit: 10 project creations per hour per user
    crate::auth::rate_limit::check_rate(
        &state.valkey, "project_create", &auth.user_id.to_string(), 10, 3600,
    ).await?;
    // ... existing logic ...
```

**File**: `src/api/pipelines.rs` — `trigger_pipeline()`:

```rust
async fn trigger_pipeline(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(project_id): Path<Uuid>,
    Json(body): Json<TriggerPipelineRequest>,
) -> Result<Json<PipelineResponse>, ApiError> {
    // Rate limit: 60 pipeline triggers per hour per user
    crate::auth::rate_limit::check_rate(
        &state.valkey, "pipeline_trigger", &auth.user_id.to_string(), 60, 3600,
    ).await?;
    // ... existing logic ...
```

**File**: `src/api/deployments.rs` — `create_release()`:

```rust
async fn create_release(
    State(state): State<AppState>,
    auth: AuthUser,
    // ...
) -> Result<Json<ReleaseResponse>, ApiError> {
    // Rate limit: 30 releases per hour per user
    crate::auth::rate_limit::check_rate(
        &state.valkey, "release_create", &auth.user_id.to_string(), 30, 3600,
    ).await?;
    // ... existing logic ...
```

**File**: `src/api/users.rs` — `create_api_token()`:

```rust
async fn create_api_token(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<CreateTokenRequest>,
) -> Result<Json<TokenResponse>, ApiError> {
    // Rate limit: 20 token creations per hour per user
    crate::auth::rate_limit::check_rate(
        &state.valkey, "token_create", &auth.user_id.to_string(), 20, 3600,
    ).await?;
    // ... existing logic ...
```

**File**: `src/api/users.rs` — `update_user()` (password change path):

The password change is embedded inside `update_user()`. Add rate limiting only
when the password field is present:

```rust
async fn update_user(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(user_id): Path<Uuid>,
    Json(body): Json<UpdateUserRequest>,
) -> Result<Json<UserResponse>, ApiError> {
    // Rate limit password changes: 5 per hour per user
    if body.password.is_some() {
        crate::auth::rate_limit::check_rate(
            &state.valkey, "password_change", &auth.user_id.to_string(), 5, 3600,
        ).await?;
    }
    // ... existing logic ...
```

#### Rate limit summary table

| Endpoint | Prefix | Limit | Window | Scope |
|----------|--------|-------|--------|-------|
| Project creation | `project_create` | 10 | 1 hour | per user |
| Pipeline trigger | `pipeline_trigger` | 60 | 1 hour | per user |
| Release creation | `release_create` | 30 | 1 hour | per user |
| API token creation | `token_create` | 20 | 1 hour | per user |
| Password change | `password_change` | 5 | 1 hour | per user |

### Test plan

- **Integration**: Trigger 11 project creations, verify 11th returns 429
- **Integration**: Verify different users have independent counters
- **Integration**: Verify existing tests still pass (limits are generous enough)

> **Status: COMPLETE** — Rate limits added to all 5 endpoints. No deviations from plan.

---

## 4. Resource Limit Configurability

**Problem**: Three resource limits are hardcoded with no env var:

- Webhook concurrency: 50 (`src/api/webhooks.rs:41`)
- Agent manager sessions: 10 per user (`src/agent/service.rs:951`)
- Observe buffer capacity: 10,000 per signal (`src/observe/ingest.rs:27`)

### Changes

#### 4a. Add config fields

**File**: `src/config.rs` — add to `Config` struct:

```rust
/// Maximum concurrent webhook deliveries (default 50).
pub webhook_max_concurrent: usize,
/// Maximum running manager sessions per user (default 10).
pub manager_session_max_per_user: i64,
/// Observe ingest buffer capacity per signal type (default 10_000).
pub observe_buffer_capacity: usize,
```

In `Config::load()`:

```rust
webhook_max_concurrent: env::var("PLATFORM_WEBHOOK_MAX_CONCURRENT")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(50),
manager_session_max_per_user: env::var("PLATFORM_MANAGER_SESSION_MAX")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(10),
observe_buffer_capacity: env::var("PLATFORM_OBSERVE_BUFFER_CAPACITY")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(10_000),
```

#### 4b. Wire through to usage sites

**File**: `src/api/webhooks.rs` — replace the `LazyLock` semaphore:

The webhook semaphore is currently a `static LazyLock<Semaphore>` initialized
with literal `50`. Since we need config at runtime, move the semaphore into
`AppState` (it's already there as `webhook_semaphore`).

Check if `state.webhook_semaphore` already exists. If so, just change where it's
initialized in `main.rs`:

```rust
// In main.rs where AppState is constructed:
webhook_semaphore: Arc::new(tokio::sync::Semaphore::new(cfg.webhook_max_concurrent)),
```

Remove the `static LazyLock` if it exists separately in `webhooks.rs`.

**File**: `src/agent/service.rs` — replace the hardcoded `10`:

```rust
// Before (line 951)
if running_count.0 >= 10 {

// After
if running_count.0 >= state.config.manager_session_max_per_user {
```

**File**: `src/observe/ingest.rs` — replace the constant:

```rust
// Before (line 27)
const BUFFER_CAPACITY: usize = 10_000;

// After — accept capacity as parameter to create_channels()
pub fn create_channels(buffer_capacity: usize) -> IngestChannels {
    let (spans_tx, spans_rx) = mpsc::channel(buffer_capacity);
    let (logs_tx, logs_rx) = mpsc::channel(buffer_capacity);
    let (metrics_tx, metrics_rx) = mpsc::channel(buffer_capacity);
    // ...
}
```

Update the call site in `observe::spawn_background_tasks()`:

```rust
let channels = ingest::create_channels(state.config.observe_buffer_capacity);
```

### Test plan

- **Unit**: Verify config parsing for new env vars
- **Integration**: Existing tests pass (defaults match current values)

---

## 5. Observe Retention Batching

**Problem**: Retention cleanup runs unbounded `DELETE FROM {table} WHERE {col} < $1`
every hour (`observe/mod.rs:74`). On large deployments this can lock entire tables
for seconds and starve concurrent reads/writes.

### Changes

#### 5a. Batch deletion with LIMIT

**File**: `src/observe/mod.rs` — replace the retention cleanup loop body:

```rust
// Before (line 74)
let sql = format!("DELETE FROM {table} WHERE {col} < $1");
match sqlx::query(&sql).bind(cutoff).execute(&pool).await {
    Ok(result) => {
        tracing::info!(
            table,
            deleted = result.rows_affected(),
            "retention cleanup"
        );
    }
    Err(e) => tracing::error!(error = %e, table, "retention cleanup failed"),
}

// After — batched deletion to avoid long table locks
let batch_size: i64 = 50_000;
let mut total_deleted: u64 = 0;
loop {
    let sql = format!(
        "DELETE FROM {table} WHERE ctid IN (
            SELECT ctid FROM {table} WHERE {col} < $1 LIMIT $2
        )"
    );
    match sqlx::query(&sql)
        .bind(cutoff)
        .bind(batch_size)
        .execute(&pool)
        .await
    {
        Ok(result) => {
            let deleted = result.rows_affected();
            total_deleted += deleted;
            if deleted < batch_size as u64 {
                break; // No more rows to delete
            }
            // Yield between batches to let other queries through
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        Err(e) => {
            tracing::error!(error = %e, table, "retention cleanup batch failed");
            break;
        }
    }
}
if total_deleted > 0 {
    tracing::info!(table, deleted = total_deleted, "retention cleanup complete");
}
```

**Why `ctid`**: Using a subquery with `ctid` (Postgres physical row ID) is the
most efficient way to batch deletes — it avoids the optimizer issues with
`DELETE ... LIMIT` (which Postgres doesn't support directly) and ensures each
batch processes exactly `batch_size` rows.

**Why `LIMIT $2` not `LIMIT 50000`**: Parameterized to avoid query plan caching
issues across different batch sizes.

**100ms sleep between batches**: Prevents the cleanup from monopolizing the
connection pool. At 50K rows/batch with 100ms gaps, cleanup of 1M rows takes
~2 seconds of wall time and ~20 DELETE operations — much better than one massive
lock.

### Test plan

- **Integration**: Insert 100 old records, run retention, verify all deleted
- **Unit**: Verify batch loop exits when fewer than batch_size rows deleted

---

## 6. Metric Write Batching

**Problem**: `write_metrics()` (`observe/store.rs:236-248`) processes each metric
sequentially with 2 DB queries per metric (series upsert + sample insert). At
high cardinality (1000 metrics/batch), that's 2000 sequential round-trips per
flush cycle.

### Changes

#### 6a. Batch metric series upsert

**File**: `src/observe/store.rs` — replace `write_metrics()`:

```rust
pub async fn write_metrics(pool: &PgPool, metrics: &[MetricRecord]) -> Result<(), ObserveError> {
    if metrics.is_empty() {
        return Ok(());
    }

    // Step 1: Batch upsert all metric series and get their IDs.
    // Collect unique (project_id, name, labels) combinations.
    let mut series_names = Vec::with_capacity(metrics.len());
    let mut series_labels = Vec::with_capacity(metrics.len());
    let mut series_project_ids = Vec::with_capacity(metrics.len());

    for m in metrics {
        series_names.push(m.name.as_str());
        series_labels.push(&m.labels);
        series_project_ids.push(m.project_id);
    }

    // Guard: UNNEST requires all arrays to be the same length.
    // A length mismatch would silently misalign rows (wrong value → wrong series).
    debug_assert_eq!(series_names.len(), series_labels.len());
    debug_assert_eq!(series_names.len(), series_project_ids.len());
    if series_names.len() != series_labels.len()
        || series_names.len() != series_project_ids.len()
    {
        return Err(ObserveError::Internal(anyhow::anyhow!(
            "metric batch array length mismatch: names={}, labels={}, projects={}",
            series_names.len(), series_labels.len(), series_project_ids.len()
        )));
    }

    // Batch upsert using UNNEST — single round-trip for all series
    let series_ids: Vec<(Uuid,)> = sqlx::query_as(
        "INSERT INTO metric_series (name, labels, project_id)
         SELECT * FROM UNNEST($1::text[], $2::jsonb[], $3::uuid[])
         ON CONFLICT (name, labels, project_id) DO UPDATE SET name = EXCLUDED.name
         RETURNING id"
    )
    .bind(&series_names)
    .bind(&series_labels)
    .bind(&series_project_ids)
    .fetch_all(pool)
    .await?;

    // Step 2: Batch insert all samples — single round-trip
    let mut sample_series_ids = Vec::with_capacity(metrics.len());
    let mut sample_timestamps = Vec::with_capacity(metrics.len());
    let mut sample_values = Vec::with_capacity(metrics.len());

    for (i, m) in metrics.iter().enumerate() {
        sample_series_ids.push(series_ids[i].0);
        sample_timestamps.push(m.timestamp);
        sample_values.push(m.value);
    }

    sqlx::query(
        "INSERT INTO metric_samples (series_id, timestamp, value)
         SELECT * FROM UNNEST($1::uuid[], $2::timestamptz[], $3::double precision[])
         ON CONFLICT DO NOTHING"
    )
    .bind(&sample_series_ids)
    .bind(&sample_timestamps)
    .bind(&sample_values)
    .execute(pool)
    .await?;

    Ok(())
}
```

**Performance**: 2 queries total regardless of batch size (was 2N). For a batch
of 500 metrics, this is 2 queries instead of 1000.

**Note**: The UNNEST approach requires the arrays to be the same length and
maintain order. The `RETURNING id` from the upsert returns IDs in the same order
as the input arrays, so the `series_ids[i]` mapping is correct.

**Gotcha**: Check the current `metric_series` table schema for the unique constraint.
The `ON CONFLICT (name, labels, project_id)` assumes a unique index on those
three columns. Verify this exists in migrations — if not, add it.

### Test plan

- **Integration**: Existing metric ingestion tests pass
- **Integration**: Ingest 100 metrics with mixed series, verify all stored correctly
- **Benchmark**: Compare flush time with old vs new implementation (expect 10-50x
  improvement at batch size 500)

> **Status: COMPLETE** — Batched UNNEST implementation done.
> **Deviation:** Actual `metric_series` unique constraint is `UNIQUE (name, labels)`,
> NOT `(name, labels, project_id)`. Implementation uses `ON CONFLICT (name, labels)`
> to match actual schema. Also includes `metric_type`, `unit`, `project_id`, `last_value`
> columns in the upsert which the plan omitted.

---

## 7. Alert Evaluation Hardening

**Problem**: Alert rules are evaluated sequentially with no timeout per rule
(`observe/alert.rs:693-758`). A slow metric query blocks all subsequent rules.
100 rules = 100 sequential DB queries every 30 seconds.

### Changes

#### 7a. Add per-rule evaluation timeout

**File**: `src/observe/alert.rs` — in `evaluate_all()`:

```rust
// Before (line 704)
for rule in &rules {
    let value = evaluate_metric(&state.pool, ...).await;
    // ...
}

// After — timeout per rule, continue on failure
for rule in &rules {
    let rule_timeout = std::time::Duration::from_secs(10);
    match tokio::time::timeout(rule_timeout, evaluate_one_rule(state, rule)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::warn!(
                rule_id = %rule.id, rule_name = %rule.name,
                error = %e, "alert rule evaluation failed"
            );
        }
        Err(_elapsed) => {
            tracing::warn!(
                rule_id = %rule.id, rule_name = %rule.name,
                "alert rule evaluation timed out (10s)"
            );
        }
    }
}
```

Extract the per-rule body into `evaluate_one_rule()` for clarity.

#### 7b. Add pagination to rule fetch

```rust
// Before (line 697-702)
let rules = sqlx::query(
    "SELECT ... FROM alert_rules WHERE enabled = true",
)
.fetch_all(&state.pool)
.await?;

// After — limit to prevent unbounded fetch
let rules = sqlx::query(
    "SELECT ... FROM alert_rules WHERE enabled = true ORDER BY id LIMIT 500",
)
.fetch_all(&state.pool)
.await?;
```

500 rules is a reasonable upper bound. Log a warning if the limit is hit:

```rust
if rules.len() >= 500 {
    tracing::warn!("alert rule limit reached (500) — some rules may not be evaluated");
}
```

### Test plan

- **Unit**: Verify timeout fires on slow rule (mock a never-completing query)
- **Integration**: Existing alert tests pass with timeout wrapper

---

## 8. UI Asset Compression Header

**Problem**: `rust-embed` `compression` feature is enabled in Cargo.toml
(`features = ["compression"]`), but `src/ui.rs` serves assets without the
`Content-Encoding: gzip` header. Browsers receive compressed bytes but don't
know to decompress them — assets may appear corrupt or oversized.

### Changes

#### 8a. Add Content-Encoding header for compressed assets

**File**: `src/ui.rs` — update the `serve()` function:

First, check if rust-embed's `compression` feature actually pre-compresses. With
`compression` enabled, `UiAssets::get(path)` returns the raw (uncompressed) data
at runtime — the feature compresses at compile time and decompresses on access.
This means **no Content-Encoding header is needed** because the data returned by
`.get()` is already decompressed.

**BUT** — this means we're paying decompression cost on every request and sending
uncompressed data over the wire. The compile-time compression only saves binary
size, not bandwidth.

The real fix is to add **runtime gzip compression** via tower-http's compression
layer, which is already in Cargo.toml (`tower-http` with `compression-gzip`
feature):

```rust
// In src/main.rs — add compression layer to the router
use tower_http::compression::CompressionLayer;

let app = axum::Router::new()
    // ... routes ...
    .fallback(ui::static_handler)
    .layer(DefaultBodyLimit::max(10 * 1024 * 1024))
    // Compress responses (gzip) when client sends Accept-Encoding: gzip
    .layer(CompressionLayer::new())
    // ... timeout, security headers ...
```

`CompressionLayer` from `tower-http` automatically:
- Checks `Accept-Encoding: gzip` in request
- Compresses response body on the fly
- Sets `Content-Encoding: gzip` header
- Skips already-compressed content types (images, etc.)
- Works with streaming responses

**Placement**: Add below `DefaultBodyLimit` but above `TimeoutLayer` — compression
should happen after body limits are checked but before timeout starts counting
(compression adds latency).

**Note**: `tower-http 0.6` with `compression-gzip` feature is already in Cargo.toml
(line 21). No new dependencies needed.

### Test plan

- **Manual**: Verify `curl -H "Accept-Encoding: gzip" /index.html` returns
  `Content-Encoding: gzip` header
- **Integration**: Verify UI assets still load correctly in browser

---

## 9. Unbounded Query Pagination

**Problem**: Two admin API endpoints return unbounded result sets:

- `admin.rs:187` — `SELECT ... FROM roles ORDER BY name` (no LIMIT)
- `commands.rs:645` — `SELECT ... FROM platform_commands` (no LIMIT)

### Changes

#### 9a. Add LIMIT to unbounded queries

**File**: `src/api/admin.rs` — roles list:

```rust
// Before
"SELECT id, name, description, is_system, created_at FROM roles ORDER BY name"

// After — cap at 200 (roles are admin-created, unlikely to exceed this)
"SELECT id, name, description, is_system, created_at FROM roles ORDER BY name LIMIT 200"
```

**File**: `src/api/commands.rs` — workspace commands list:

```rust
// Before
"SELECT ... FROM platform_commands
 WHERE (workspace_id = $1 AND project_id IS NULL)
    OR (project_id IS NULL AND workspace_id IS NULL)
 ORDER BY workspace_id IS NULL ASC, name ASC"

// After
"SELECT ... FROM platform_commands
 WHERE (workspace_id = $1 AND project_id IS NULL)
    OR (project_id IS NULL AND workspace_id IS NULL)
 ORDER BY workspace_id IS NULL ASC, name ASC
 LIMIT 200"
```

These are admin/internal endpoints where 200 is a safe upper bound. If pagination
is needed later, add standard `ListParams` (offset/limit) like other endpoints.

### Test plan

- **Integration**: Existing admin and commands tests pass

> **Status: COMPLETE** — `LIMIT 200` added to both queries. No deviations from plan.

---

## 10. Composite Index for Pipeline Filtering

**Problem**: Pipeline list queries filter on `(project_id, status)` but only
`idx_pipelines_project(project_id, created_at DESC)` and
`idx_pipelines_status(status)` exist. The optimizer can't efficiently combine
both filters on large tables.

### Changes

#### 10a. Add migration for composite index

```bash
just db-add pipeline_status_index
```

**File**: `migrations/YYYYMMDDHHMMSS_pipeline_status_index.up.sql`:

**Important**: `CREATE INDEX CONCURRENTLY` cannot run inside a transaction block.
sqlx wraps migrations in transactions by default. Add the sqlx directive comment
to disable the transaction for this migration:

```sql
-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_pipelines_project_status
ON pipelines(project_id, status, created_at DESC);
```

> **Note:** Removed `WHERE is_active = true` — `pipelines` table has no `is_active` column.

**File**: `migrations/YYYYMMDDHHMMSS_pipeline_status_index.down.sql`:

```sql
DROP INDEX IF EXISTS idx_pipelines_project_status;
```

**Notes**:
- `-- no-transaction` is **required** — Postgres forbids `CREATE INDEX
  CONCURRENTLY` inside a transaction block, and sqlx wraps migrations in
  transactions by default. Without this directive, `just db-migrate` will crash.
- `CONCURRENTLY` prevents table lock during index creation on large tables
- Partial index (`WHERE is_active = true`) matches the soft-delete filter used in
  all queries, keeping the index smaller
- Covers the common query pattern: filter by project + status, order by created_at

### Test plan

- **Migration**: `just db-migrate && just db-prepare` succeeds
- **Integration**: Pipeline list queries still work (verify with EXPLAIN ANALYZE
  if desired)

> **Status: COMPLETE** — Migration created as `20260410010001_pipeline_status_index`.
> **Deviation:** Removed `WHERE is_active = true` partial index clause — the `pipelines`
> table has no `is_active` column (only `projects` uses soft-delete).
> Used `-- no-transaction` directive (not `-- sqlx-disable-transaction`).

---

## Implementation Order

```
1. Safe production startup (1 hr)      — high value, low risk
   ↓
2. Health check improvements (1 hr)    — enables better K8s orchestration
   ↓
3. Rate limiting gaps (30 min)         — one-line additions per endpoint
   ↓
4. Resource limit configurability (1 hr) — config plumbing + wire-through
   ↓
8. UI compression (15 min)             — single layer addition
   ↓
9. Unbounded query pagination (15 min) — trivial SQL changes
   ↓
10. Composite index (15 min)           — migration only
   ↓
5. Observe retention batching (1 hr)   — careful with DELETE logic
   ↓
6. Metric write batching (2 hr)        — UNNEST queries, test carefully
   ↓
7. Alert evaluation hardening (1 hr)   — timeout + extract helper
```

Total: ~8-9 hours.

After each section, run `just test-unit`. After all sections complete, run
`just ci-full`.

---

## Env Var Summary

New environment variables added by this plan:

| Env Var | Default | Purpose |
|---------|---------|---------|
| `PLATFORM_WEBHOOK_MAX_CONCURRENT` | `50` | Max concurrent webhook deliveries |
| `PLATFORM_MANAGER_SESSION_MAX` | `10` | Max running manager sessions per user |
| `PLATFORM_OBSERVE_BUFFER_CAPACITY` | `10000` | Observe ingest buffer per signal type |

## New Rate Limits

| Endpoint | Key prefix | Limit | Window |
|----------|-----------|-------|--------|
| Project creation | `project_create` | 10/hr | per user |
| Pipeline trigger | `pipeline_trigger` | 60/hr | per user |
| Release creation | `release_create` | 30/hr | per user |
| API token creation | `token_create` | 20/hr | per user |
| Password change | `password_change` | 5/hr | per user |
