# Plan 24 — Code Review Nitpick Fixes (N1-N9)

## Overview

Address the 9 nitpick-level issues identified in the Plan 13 code review. These are quality improvements that should be made when touching relevant files. None are critical, but they collectively improve robustness, observability, and performance.

---

## N1: Return `&str` from Token Extraction (Avoid Allocation per Request)

**File**: `src/auth/middleware.rs:89-113`

**Problem**: `extract_bearer_token()` and `extract_session_cookie()` return `String` via `.to_owned()`, but the value is immediately used as `&str` in lookup functions. This creates one unnecessary allocation per authenticated request.

**Current code**:
```rust
fn extract_bearer_token(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    value.strip_prefix("Bearer ").map(|t| t.to_owned())
}
```

**Fix**: Return a borrowed string reference:

```rust
fn extract_bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    value.strip_prefix("Bearer ")
}

fn extract_session_cookie(headers: &HeaderMap) -> Option<&str> {
    let cookie_header = headers.get(COOKIE)?.to_str().ok()?;
    cookie_header.split(';')
        .find_map(|c| {
            let c = c.trim();
            c.strip_prefix("session=")
        })
}
```

**Callers**: Update `validate_token()` and `validate_session()` to accept `&str` instead of `String`. These already pass the value as `&str` to DB queries.

**Tests**: Existing auth tests should still pass. Add:
- `extract_bearer_token("Bearer abc")` → `Some("abc")`
- `extract_bearer_token("Bearer ")` → `Some("")`
- `extract_session_cookie("session=xyz; other=abc")` → `Some("xyz")`

---

## N2: Stricter Email Validation

**File**: `src/validation.rs:26-31`

**Problem**: `check_email()` only verifies `contains('@')` — accepts `@@`, `a@`, `@b`, `a@@b`.

**Current code**:
```rust
pub fn check_email(value: &str) -> Result<(), ApiError> {
    check_length("email", value, 3, 254)?;
    if !value.contains('@') {
        return Err(ApiError::BadRequest("email: invalid format".into()));
    }
    Ok(())
}
```

**Fix**: Check for exactly one `@` with non-empty local and domain parts:

```rust
pub fn check_email(value: &str) -> Result<(), ApiError> {
    check_length("email", value, 3, 254)?;
    let parts: Vec<&str> = value.splitn(3, '@').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(ApiError::BadRequest("email: must have exactly one @ with non-empty local and domain parts".into()));
    }
    Ok(())
}
```

**Tests**:
```rust
#[test]
fn email_valid() {
    assert!(check_email("user@example.com").is_ok());
    assert!(check_email("a@b").is_ok());  // minimal valid
}

#[test]
fn email_double_at_rejected() {
    assert!(check_email("a@@b").is_err());
}

#[test]
fn email_empty_local_rejected() {
    assert!(check_email("@domain.com").is_err());
}

#[test]
fn email_empty_domain_rejected() {
    assert!(check_email("user@").is_err());
}

#[test]
fn email_no_at_rejected() {
    assert!(check_email("nodomain").is_err());
}
```

**Impact**: May reject previously accepted invalid emails. Check existing test data and seed data for compliance.

---

## N3: Block Leading Dots in Names

**File**: `src/validation.rs:13-23`

**Problem**: `check_name()` allows leading/trailing dots (`.hidden`, `name.`). Leading dots create hidden files on Unix systems.

**Fix**: Add leading/trailing dot check:

```rust
pub fn check_name(value: &str) -> Result<(), ApiError> {
    check_length("name", value, 1, 255)?;
    if value.starts_with('.') || value.ends_with('.') {
        return Err(ApiError::BadRequest(
            "name: must not start or end with a dot".into(),
        ));
    }
    if !value.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') {
        return Err(ApiError::BadRequest(
            "name: must be alphanumeric, hyphens, underscores, or dots".into(),
        ));
    }
    Ok(())
}
```

**Tests**:
```rust
#[test]
fn name_leading_dot_rejected() {
    assert!(check_name(".hidden").is_err());
}

#[test]
fn name_trailing_dot_rejected() {
    assert!(check_name("name.").is_err());
}

#[test]
fn name_middle_dot_ok() {
    assert!(check_name("my.project").is_ok());
}
```

**Impact**: May reject previously accepted names with leading dots. Check existing project/user names.

---

## N4: Fix `parse_cors_origins("")` → Treat as Unset

**File**: `src/config.rs:27-29`

**Problem**: `parse_cors_origins("")` produces `vec![""]`. Empty env var should be treated as no CORS origins (deny all).

**Current code**:
```rust
fn parse_cors_origins(s: &str) -> Vec<String> {
    s.split(',').map(|s| s.trim().to_string()).collect()
}
```

**Fix**:
```rust
fn parse_cors_origins(s: &str) -> Vec<String> {
    if s.trim().is_empty() {
        return Vec::new();
    }
    s.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}
```

**Tests**:
```rust
#[test]
fn cors_empty_string_is_empty() {
    assert!(parse_cors_origins("").is_empty());
}

#[test]
fn cors_whitespace_only_is_empty() {
    assert!(parse_cors_origins("  ").is_empty());
}

#[test]
fn cors_single_origin() {
    assert_eq!(parse_cors_origins("http://localhost:3000"), vec!["http://localhost:3000"]);
}

#[test]
fn cors_multiple_trimmed() {
    assert_eq!(
        parse_cors_origins(" a.com , b.com "),
        vec!["a.com", "b.com"]
    );
}

#[test]
fn cors_trailing_comma_ignored() {
    assert_eq!(parse_cors_origins("a.com,"), vec!["a.com"]);
}
```

---

## N5: Configurable Permission Cache TTL

**File**: `src/rbac/resolver.rs:10`

**Problem**: Hard-coded 5-minute permission cache TTL. Should be configurable for operational flexibility (shorter for development, longer for production).

**Current code**:
```rust
const CACHE_TTL_SECS: u64 = 300; // 5 minutes
```

**Fix**: Add to `Config`:

```rust
// src/config.rs
pub struct Config {
    // ... existing fields ...
    pub permission_cache_ttl_secs: u64,  // NEW: default 300
}
```

Env var: `PLATFORM_PERMISSION_CACHE_TTL` (default: `300`).

**Modify `src/rbac/resolver.rs`**: Accept TTL from config instead of constant:

```rust
// Change function signatures to accept ttl:
pub async fn effective_permissions(
    pool: &PgPool,
    valkey: &fred::clients::Pool,
    user_id: Uuid,
    project_id: Option<Uuid>,
    cache_ttl_secs: u64,  // NEW parameter
) -> Result<HashSet<Permission>, anyhow::Error> {
    // ... use cache_ttl_secs instead of CACHE_TTL_SECS ...
}
```

**Alternative (less invasive)**: Thread the TTL through `AppState`:

```rust
// In effective_permissions, read from a global or pass through state
// Simplest: make it a static that's set once at startup
static CACHE_TTL: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

pub fn set_cache_ttl(ttl: u64) {
    CACHE_TTL.set(ttl).ok();
}

fn cache_ttl() -> u64 {
    *CACHE_TTL.get().unwrap_or(&300)
}
```

Call `resolver::set_cache_ttl(config.permission_cache_ttl_secs)` in `main.rs` at startup.

---

## N6: Log Audit Write Failures + Fix `ip_addr`

**File**: `src/audit.rs:19`

**Problem**: `write_audit` fire-and-forget silently drops DB errors. Also `ip_addr` is always NULL because the column is `INET` type which requires the `ipnetwork` crate.

**Fix (logging)**:
```rust
pub async fn write_audit(pool: &PgPool, entry: AuditEntry<'_>) {
    if let Err(e) = write_audit_inner(pool, entry).await {
        tracing::warn!(error = %e, "failed to write audit log entry");
    }
}

async fn write_audit_inner(pool: &PgPool, entry: AuditEntry<'_>) -> Result<(), sqlx::Error> {
    sqlx::query!(/* ... */).execute(pool).await?;
    Ok(())
}
```

**Fix (ip_addr)**: Two options:

**Option A**: Add `ipnetwork` crate + sqlx feature:
```toml
# Cargo.toml
ipnetwork = "0.20"
sqlx = { version = "0.8", features = ["...", "ipnetwork"] }
```

Then change `ip_addr` binding:
```rust
let ip: Option<ipnetwork::IpNetwork> = entry.ip_addr
    .and_then(|s| s.parse().ok());
```

**Option B**: Change the column type from `INET` to `TEXT`:
```sql
ALTER TABLE audit_log ALTER COLUMN ip_addr TYPE TEXT;
```

Option A is cleaner (proper type) but adds a dependency. Option B is simpler.

**Recommended**: Option A (use proper INET type with `ipnetwork` crate).

---

## N7: Distinguish Pod-Not-Found vs API Error in Step Log Streaming

**File**: `src/api/pipelines.rs:443-451`

**Problem**: `stream_live_logs` swallows all K8s errors as "Logs not yet available" — no distinction between pod not found vs K8s API error vs permission error.

**Current code**:
```rust
Err(_) => {
    // Pod might not exist yet
    Ok(Response::builder()
        .status(200)
        .body(Body::from("Logs not yet available"))
        .unwrap())
}
```

**Fix**:
```rust
Err(e) => {
    // Check if it's a 404 (pod not found) vs other error
    if let Some(kube::Error::Api(err_resp)) = e.downcast_ref::<kube::Error>() {
        if err_resp.code == 404 {
            return Ok(Response::builder()
                .status(200)
                .body(Body::from("Logs not yet available — pod not started"))
                .unwrap());
        }
    }
    tracing::warn!(error = %e, "failed to stream pod logs");
    Ok(Response::builder()
        .status(200)
        .body(Body::from("Logs temporarily unavailable"))
        .unwrap())
}
```

---

## N8: Timeout on Git Subprocesses in `browser.rs`

**File**: `src/git/browser.rs`

**Problem**: Git subprocesses (`git ls-tree`, `git show`, `git log`) have no timeout. A pathological repo could block indefinitely.

**Fix**: Wrap all `Command` calls with `tokio::time::timeout`:

```rust
use tokio::time::{timeout, Duration};

const GIT_TIMEOUT: Duration = Duration::from_secs(30);

async fn git_ls_tree(repo_path: &Path, reference: &str, path: &str) -> Result<String, ApiError> {
    let output = timeout(GIT_TIMEOUT, async {
        tokio::process::Command::new("git")
            .args(["ls-tree", "--name-only", reference, "--", path])
            .current_dir(repo_path)
            .output()
            .await
    })
    .await
    .map_err(|_| ApiError::Internal(anyhow::anyhow!("git ls-tree timed out after 30s")))?
    .map_err(|e| ApiError::Internal(e.into()))?;

    if !output.status.success() {
        return Err(ApiError::NotFound("path not found".into()));
    }

    String::from_utf8(output.stdout)
        .map_err(|e| ApiError::Internal(e.into()))
}
```

Apply to all git subprocess calls in `browser.rs`:
- `git ls-tree` (tree listing)
- `git show` (blob content)
- `git log` (commit history)
- `git branch` (branch listing)
- `git rev-parse` (ref resolution)

**Tests**: Hard to unit test timeouts. Verify manually with a large repo.

---

## N9: Wire Up Valkey Pub/Sub in Pipeline Executor

**File**: `src/pipeline/executor.rs:31`, `src/pipeline/trigger.rs:240`

**Problem**: `trigger.rs` publishes to a Valkey channel (`pipeline:notify`) when a new pipeline is created, but the executor only polls every 5 seconds and never subscribes to that channel. The pub/sub notification is wasted.

**Current executor**:
```rust
let mut interval = tokio::time::interval(Duration::from_secs(5));
loop {
    tokio::select! {
        _ = interval.tick() => {
            poll_pending(&state).await;
        }
        // ... no pub/sub subscription
    }
}
```

**Fix**: Subscribe to the Valkey channel and trigger immediate polling on notification:

```rust
pub async fn run(state: AppState, mut shutdown: watch::Receiver<bool>) {
    let mut interval = tokio::time::interval(Duration::from_secs(5));

    // Subscribe to pipeline notifications for immediate wake-up
    let subscriber = state.valkey.next(); // Get a client from pool
    let mut subscription = subscriber.subscribe("pipeline:notify").await;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                if let Err(e) = poll_pending(&state).await {
                    tracing::error!(error = %e, "pipeline poll failed");
                }
            }
            msg = subscription.recv() => {
                if msg.is_some() {
                    // Immediate poll on notification
                    if let Err(e) = poll_pending(&state).await {
                        tracing::error!(error = %e, "pipeline poll (notified) failed");
                    }
                    // Reset the interval to avoid immediate double-poll
                    interval.reset();
                }
            }
            _ = shutdown.changed() => {
                tracing::info!("pipeline executor shutting down");
                break;
            }
        }
    }
}
```

**Note**: `fred::clients::Pool` doesn't implement `PubsubInterface` — need to use `pool.next()` to get a `Client` that does. See CLAUDE.md gotcha.

**Alternative (simpler)**: Use a `tokio::sync::Notify` in `AppState` instead of Valkey pub/sub. The trigger sets the notify, executor waits on it. This works because both run in the same process.

```rust
// In AppState:
pub pipeline_notify: Arc<tokio::sync::Notify>,

// In trigger.rs:
state.pipeline_notify.notify_one();

// In executor.rs:
tokio::select! {
    _ = interval.tick() => { /* ... */ }
    _ = state.pipeline_notify.notified() => { /* ... */ }
    _ = shutdown.changed() => { break; }
}
```

**Recommended**: Use `tokio::sync::Notify` (simpler, no network dependency, same-process guaranteed).

---

## Implementation Sequence

These can be done in any order. Group by file touch:

| Priority | Nitpick | File(s) | Risk |
|----------|---------|---------|------|
| 1 | N4 (CORS empty) | `src/config.rs` | Very low |
| 2 | N2 (email validation) | `src/validation.rs` | Low (may reject bad emails) |
| 3 | N3 (leading dots) | `src/validation.rs` | Low (may reject names) |
| 4 | N1 (token &str) | `src/auth/middleware.rs` | Low |
| 5 | N6 (audit logging) | `src/audit.rs`, `Cargo.toml` | Low |
| 6 | N5 (cache TTL) | `src/rbac/resolver.rs`, `src/config.rs` | Low |
| 7 | N7 (pod log errors) | `src/api/pipelines.rs` | Low |
| 8 | N8 (git timeout) | `src/git/browser.rs` | Low |
| 9 | N9 (pub/sub) | `src/pipeline/executor.rs`, `src/store/mod.rs` | Medium |

---

## Verification

After each fix:
1. `just fmt && just lint` — no warnings
2. `just test-unit` — all tests pass
3. `just ci` — full gate passes

After all fixes:
1. Manual: verify CORS behavior with empty env var
2. Manual: verify email validation rejects `a@@b`
3. Manual: verify project names with leading dots rejected
4. Manual: verify git browser responds within 30s on large repos
5. Manual: verify pipeline triggers cause immediate executor response (N9)

---

## Estimated Scope

| Metric | Value |
|--------|-------|
| Files modified | ~9 |
| Estimated LOC changes | ~200 |
| New tests | ~15 |
| New dependencies | 1 (optional: `ipnetwork` for N6) |
| New env vars | 1 (`PLATFORM_PERMISSION_CACHE_TTL` for N5) |
