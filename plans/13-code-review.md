# Senior Rust Engineer Code Review: Platform (~15K LOC)

## Context

Full code review of the single-crate Rust platform (Phases 01-05 complete). The codebase replaces 8+ off-the-shelf services with a unified binary. Review covers all 22 source files across 11 modules, focusing on safety, idiomatic patterns, performance, and maintainability. The modified files in git status (new unit tests for error/auth/rbac/pipeline) were the initial trigger but the review scope is the full codebase.

---

## 1. Executive Summary

The architecture is disciplined: clear module boundaries, compile-time SQL via `sqlx`, proper `thiserror` enums, structured tracing, and consistent audit logging on mutations. No `unsafe`, no `.unwrap()` in production paths, solid test coverage for helpers.

The review found **5 critical issues** (2 security bugs, 1 test that silently does nothing, 1 header injection vector, 1 CORS spec violation), **10 idiomatic improvements** (mostly duplication that will compound as modules 06-09 land), and **9 nitpicks**. None require architectural rework — all are targeted fixes.

---

## 2. Actionable Suggestions

### CRITICAL — fix before next deploy

**C1. Tautological test assertion — `internal_hides_details` is a no-op**
- File: `src/error.rs:203`
- `!body.iter().any(|_| false)` is always `true`, so the `||` short-circuits and `!json.to_string().contains("secret")` is never evaluated. Test passes unconditionally.
- Fix: `assert!(!json.to_string().contains("secret"), "internal error must not leak details");`

**C2. Timing oracle in git Basic Auth enables user enumeration**
- File: `src/git/smart_http.rs:89-143`
- Line 102: when username doesn't exist, `.ok_or(ApiError::Unauthorized)?` returns immediately — no hashing work. When user exists, SHA-256 token hash + argon2 password verify (~100ms) run. Attacker can distinguish by measuring response latency.
- The login endpoint (`src/api/users.rs:147`) already handles this correctly with `password::dummy_hash()`. Same pattern needed here.
- Fix: restructure `authenticate_basic` to always run argon2 verify regardless of user existence (dummy hash for missing users), matching the login handler pattern.

**C3. CORS `allow_headers(Any)` + `allow_credentials(true)` violates Fetch spec**
- File: `src/main.rs:176-177`
- CORS spec: when `Access-Control-Allow-Credentials: true`, `Access-Control-Allow-Headers` must not be wildcard `*`. Browsers will reject the preflight response.
- Fix: enumerate allowed headers explicitly: `CONTENT_TYPE, AUTHORIZATION, ACCEPT, COOKIE`.

**C4. Content-Disposition header injection in artifact download**
- File: `src/api/pipelines.rs:534`
- `format!("attachment; filename=\"{}\"", artifact.name)` — if name contains `"` or `\r\n`, attacker injects arbitrary headers. Name originates from user-supplied pipeline definitions.
- Fix: sanitize filename to alphanumeric + `-_.` before embedding in header.

**C5. Private project access returns 403 instead of 404 — leaks resource existence**
- Files: `src/git/smart_http.rs:432`, `src/git/lfs.rs:107`
- CLAUDE.md states: "return 404 (not 403) for private resources — avoids leaking existence."
- `check_access` resolves project first (returns 404 if not found), then authenticates and checks RBAC. If RBAC fails, returns `Forbidden` — confirming repo exists.
- Fix: change `ApiError::Forbidden` to `ApiError::NotFound("repository".into())` in both files.

### IDIOMATIC — improve before modules 06-09

**I1. Six duplicated `require_project_read/write` helpers across API modules**
- Identical 15-20 line functions in: `api/pipelines.rs`, `api/issues.rs`, `api/merge_requests.rs`, `api/webhooks.rs`, `api/projects.rs`
- Fix: extract into `src/api/helpers.rs` and re-export from `src/api/mod.rs`.

**I2. Five duplicate `ListResponse<T>` definitions**
- Defined identically in `projects.rs`, `merge_requests.rs`, `pipelines.rs`, `issues.rs`, `users.rs`.
- Fix: define once in `src/api/helpers.rs`.

**I3. Duplicated `slug()` function**
- `src/pipeline/executor.rs:680` and `src/api/pipelines.rs:571` — identical 6-line function.
- Fix: expose as `pub fn slug()` in `src/pipeline/mod.rs`, import in both consumers.

**I4. `build_pod_spec` takes 9 params with `#[allow(clippy::too_many_arguments)]`**
- File: `src/pipeline/executor.rs:367`
- CLAUDE.md says use params struct from the start. Create `PodSpecParams<'a>` struct.

**I5. Missing audit logging on delegation create/revoke**
- File: `src/rbac/delegation.rs` — `create_delegation` (line 36) and `revoke_delegation` (line 102) mutate DB but never call `write_audit`. Every other mutation in the codebase audits.
- Fix: accept audit info as parameters or call `write_audit` directly.

**I6. `list_delegations` returns unbounded results**
- File: `src/rbac/delegation.rs:127` — no `LIMIT`/`OFFSET`. All other list endpoints use `ListParams` (default 50, max 100).

**I7. No self-delegation guard**
- File: `src/rbac/delegation.rs:36` — doesn't check `delegator_id != delegate_id`. Users can delegate permissions to themselves.

**I8. Silent error swallowing in `get_cached`**
- File: `src/store/valkey.rs:15-18` — both Redis errors and deserialization errors become `None` silently. Schema changes after deploy cause infinite cache-miss cycles with zero log visibility.
- Fix: log deserialization errors at `warn` level.

**I9. Permission cache silently drops unknown permission strings**
- File: `src/rbac/resolver.rs:33-35` — `filter_map(|s| Permission::from_str(s).ok())` on cache hit. During rolling deploys with new permissions, cached old-format strings are silently dropped, causing intermittent permission denials.
- Fix: log warning for unparseable permission strings.

**I10. Session cleanup swallows DB errors without logging**
- File: `src/main.rs:89-96` — `let _ =` on cleanup queries. If queries fail, `tracing::debug!("cleanup complete")` still runs, masking the failure.
- Fix: `if let Err(e) = ... { tracing::warn!(...) }`

### NITPICKS — address on next touch

**N1.** `extract_bearer_token`/`extract_session_cookie` return `String` via `.to_owned()` but value is immediately used as `&str` in lookup functions. Could return `&str` to avoid one allocation per request. (`src/auth/middleware.rs:89-113`)

**N2.** `check_email` only verifies `contains('@')` — accepts `@@`, `a@`, `@b`. Consider checking for exactly one `@` with non-empty local+domain parts. (`src/validation.rs:26-31`)

**N3.** `check_name` allows leading/trailing dots (`.hidden`, `name.`). Leading dots = hidden files on Unix. (`src/validation.rs:13-23`)

**N4.** `parse_cors_origins("")` produces `vec![""]`. Empty env var should be treated as unset. (`src/config.rs:27-29`)

**N5.** Hard-coded 5-minute permission cache TTL. Should be configurable for operational flexibility. (`src/rbac/resolver.rs:10`)

**N6.** `write_audit` fire-and-forget silently drops DB errors. Add `tracing::warn!` on failure. Also `ip_addr` always NULL (needs `ipnetwork` crate). (`src/audit.rs:19`)

**N7.** `stream_live_logs` swallows all K8s errors as "Logs not yet available" — no distinction between pod not found vs API error. (`src/api/pipelines.rs:443-451`)

**N8.** Git subprocesses in `browser.rs` have no timeout. A pathological repo could block indefinitely. Wrap with `tokio::time::timeout`. (`src/git/browser.rs`)

**N9.** Executor polls every 5s but never subscribes to the Valkey channel that `notify_executor` publishes to. The pub/sub notification is wasted. (`src/pipeline/executor.rs:31`, `src/pipeline/trigger.rs:240`)

---

## 3. Refactored Snippets

### C1 Fix — Tautological test

```rust
// BEFORE (src/error.rs:203)
assert!(!body.iter().any(|_| false) || !json.to_string().contains("secret"));

// AFTER
assert!(
    !json.to_string().contains("secret"),
    "internal error response must not leak error details"
);
```

### C2 Fix — Timing-safe git Basic Auth

```rust
// BEFORE (src/git/smart_http.rs:89-143) — early return on missing user
let user = sqlx::query!(...)
    .fetch_optional(pool).await?
    .ok_or(ApiError::Unauthorized)?;  // <-- no hashing, instant return

// AFTER — always run argon2 verify
let user_row = sqlx::query!(...)
    .fetch_optional(pool).await?;

let hash_to_verify = user_row
    .as_ref()
    .map(|u| u.password_hash.as_str())
    .unwrap_or_else(|| password::dummy_hash());

// Try password as API token first (SHA-256 is constant-time relative to user existence)
let token_hash = token::hash_token(&password_raw);
let token_match = if let Some(ref user) = user_row {
    sqlx::query_scalar!(/* ... */, token_hash, user.id)
        .fetch_one(pool).await?
} else {
    0
};

if token_match > 0 {
    let user = user_row.unwrap(); // safe: token_match > 0 implies user exists
    if !user.is_active { return Err(ApiError::Unauthorized); }
    return Ok(GitUser { user_id: user.id, user_name: user.name, ip_addr: None });
}

// Always run argon2 verify to prevent timing oracle
let valid = password::verify_password(&password_raw, hash_to_verify)
    .map_err(ApiError::Internal)?;

let Some(user) = user_row else {
    return Err(ApiError::Unauthorized);
};
if !user.is_active || !valid {
    return Err(ApiError::Unauthorized);
}
Ok(GitUser { user_id: user.id, user_name: user.name, ip_addr: None })
```

### C4 Fix — Content-Disposition sanitization

```rust
// Add to src/api/pipelines.rs
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
        .collect()
}

// In download_artifact handler:
format!("attachment; filename=\"{}\"", sanitize_filename(&artifact.name))
```

### I1/I2 Fix — Shared API helpers

```rust
// New file: src/api/helpers.rs
use uuid::Uuid;

use crate::auth::middleware::AuthUser;
use crate::error::ApiError;
use crate::rbac::{Permission, resolver};
use crate::store::AppState;

#[derive(Debug, serde::Serialize)]
pub struct ListResponse<T: serde::Serialize> {
    pub items: Vec<T>,
    pub total: i64,
}

pub async fn require_project_read(
    state: &AppState,
    auth: &AuthUser,
    project_id: Uuid,
) -> Result<(), ApiError> {
    let allowed = resolver::has_permission(
        &state.pool, &state.valkey, auth.user_id,
        Some(project_id), Permission::ProjectRead,
    ).await.map_err(ApiError::Internal)?;
    if !allowed {
        return Err(ApiError::NotFound("project".into()));
    }
    Ok(())
}

pub async fn require_project_write(
    state: &AppState,
    auth: &AuthUser,
    project_id: Uuid,
) -> Result<(), ApiError> {
    let allowed = resolver::has_permission(
        &state.pool, &state.valkey, auth.user_id,
        Some(project_id), Permission::ProjectWrite,
    ).await.map_err(ApiError::Internal)?;
    if !allowed {
        return Err(ApiError::NotFound("project".into()));
    }
    Ok(())
}
```

### I8 Fix — Valkey `get_cached` with logged errors

```rust
// BEFORE (src/store/valkey.rs:15-18)
pub async fn get_cached<T: DeserializeOwned>(pool: &fred::clients::Pool, key: &str) -> Option<T> {
    let value: Option<String> = pool.get(key).await.ok()?;
    value.and_then(|v| serde_json::from_str(&v).ok())
}

// AFTER
pub async fn get_cached<T: DeserializeOwned>(pool: &fred::clients::Pool, key: &str) -> Option<T> {
    let value: Option<String> = pool.get(key).await.ok()?;
    let raw = value?;
    match serde_json::from_str(&raw) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(error = %e, %key, "cache deserialization failed, treating as miss");
            None
        }
    }
}
```

---

## 4. Final Recommendation

**Verdict: Second Pass required** — fix the 5 Critical items, then production-ready.

The codebase is well-architected. The critical items are all targeted, low-risk fixes:
- **C1** (tautological test): 1-line fix, zero production risk
- **C2** (timing oracle): pattern already exists at `src/api/users.rs:147`, replicate to `smart_http.rs`
- **C3** (CORS violation): enumerate headers, 3-line change
- **C4** (header injection): add `sanitize_filename` helper, small function
- **C5** (403→404): change two error variants

The Idiomatic items (I1-I10) should be addressed before modules 06-09 land to prevent duplication from compounding. The Nitpicks (N1-N9) can be addressed when touching relevant files.

---

## 5. Critical Files to Modify

| File | Changes |
|------|---------|
| `src/error.rs:203` | C1: fix tautological assertion |
| `src/git/smart_http.rs:89-143` | C2: timing-safe auth; C5: 403→404 on line 432 |
| `src/git/lfs.rs:107` | C5: 403→404 |
| `src/main.rs:176-177` | C3: enumerate CORS headers |
| `src/api/pipelines.rs:534` | C4: sanitize filename |
| `src/api/helpers.rs` (new) | I1/I2: shared `require_project_*` + `ListResponse` |
| `src/api/{issues,merge_requests,projects,webhooks,users,pipelines}.rs` | I1/I2: use shared helpers |
| `src/pipeline/mod.rs` | I3: expose shared `slug()` |
| `src/pipeline/executor.rs:367` | I4: `PodSpecParams` struct |
| `src/rbac/delegation.rs` | I5: audit logging; I6: pagination; I7: self-delegation guard |
| `src/store/valkey.rs:15-18` | I8: log deserialization errors |
| `src/rbac/resolver.rs:33-35` | I9: log unparseable permissions |
| `src/main.rs:89-96` | I10: log cleanup errors |

## 6. Verification

After implementing:
1. `just fmt && just lint` — no new warnings
2. `just test-unit` — all tests pass, specifically:
   - `error::tests::internal_hides_details` now actually validates the assertion
   - New tests for `sanitize_filename`
   - New tests for timing-safe `authenticate_basic`
3. `just deny` — no new advisory warnings
4. `just build` — clean build with `SQLX_OFFLINE=true`
5. Manual: attempt git clone with non-existent username, verify response time is ~same as wrong password (~100ms argon2)
