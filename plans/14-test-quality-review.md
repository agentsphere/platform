# Test Suite Quality Review & Improvement Plan

## Context

The platform codebase (~15K LOC, single Rust crate) has **264 unit tests across 31 files**, all inline `#[cfg(test)]`. There are **zero integration tests** (no `tests/` directory) and **zero usage** of dev-dependencies `insta` and `proptest` despite being in `Cargo.toml`. The **entire API handler layer** (78 handlers, ~7,259 LOC across 13 files) is untested except for 3 trivial helper tests in `api/pipelines.rs`.

This plan addresses: coverage gaps, existing test quality, test infrastructure, and a prioritized roadmap for new tests.

---

## 1. Security Bug: IPv6 SSRF Gap (Fix First)

**Both** `src/validation.rs:76` and `src/api/webhooks.rs:84` have duplicate `is_private_ip()` functions. The IPv6 arm only blocks `::1` (loopback) and `::` (unspecified). It does **NOT** block:
- `fc00::/7` — unique local addresses (IPv6 equivalent of RFC 1918)
- `fe80::/10` — link-local addresses

**Fix:**
- Add IPv6 unique-local and link-local checks to `validation.rs::is_private_ip()`
- Delete the duplicate in `webhooks.rs` and import from `validation`
- Add tests for `fc00::1`, `fe80::1`, `fd12::1`, `2001:db8::1` (allowed)

**Files:** `src/validation.rs`, `src/api/webhooks.rs`

---

## 2. Coverage Gap Analysis (by priority)

### P0 — Security-Critical (untested)

| Area | File | What's Missing |
|------|------|----------------|
| Login timing safety | `src/api/users.rs` | `dummy_hash()` always runs for missing users — untested |
| Login rate limiting | `src/api/users.rs` | 10 attempts / 300s — untested end-to-end |
| Token scope escalation | `src/api/users.rs` | User with `project:read` can't create token with `project:write` — untested |
| Auth helpers | `src/api/helpers.rs` | `require_project_read()` returns 404 (not 403) for private projects — untested |
| Error info leakage | `src/error.rs` | `From<sqlx::Error>` branches (RowNotFound→404, unique→409) — untested |
| Webhook HMAC signing | `src/api/webhooks.rs` | `X-Platform-Signature: sha256={hex}` computation — untested |
| IPv6 SSRF | `src/validation.rs` | fc00::/7 and fe80::/10 not blocked (see Section 1) |

### P1 — Core Business Logic (untested)

| Area | File | What's Missing |
|------|------|----------------|
| Project visibility filter | `src/api/projects.rs` | Private projects hidden from non-owners in list — untested |
| Auto-increment numbers | `src/api/issues.rs`, `merge_requests.rs` | Atomic `next_issue_number + 1` — untested |
| MR merge flow | `src/api/merge_requests.rs` | Git worktree --no-ff merge — untested |
| Role lifecycle | `src/api/admin.rs` | System role immutability, permission assignment — untested |
| Delegation expiry | `src/api/admin.rs` | Expired delegations excluded — untested |
| Secret scope enforcement | `src/secrets/engine.rs` | `"all"` scope matches everything, project > global preference — untested |
| RBAC resolution (async) | `src/rbac/resolver.rs` | `effective_permissions()`, `has_permission()` DB logic — untested |
| Soft-delete | `src/api/projects.rs` | `is_active = false` filtering — untested |

### P2 — Robustness

| Area | File | What's Missing |
|------|------|----------------|
| Config boolean parsing | `src/config.rs` | `"TRUE"`, `"1"`, `"yes"` all treated as false |
| CORS empty origin | `src/config.rs` | `parse_cors_origins("")` returns `[""]` — likely a bug |
| Pipeline complex globs | `src/pipeline/definition.rs` | Multi-wildcard patterns, prefix wildcards |
| Notification rate limit | `src/notify/dispatch.rs` | 100/hour per user — untested |

---

## 3. Existing Test Quality Improvements

### 3a. Weak Assertions to Strengthen

**Replace `.is_err()` with specific error matching:**
- `src/validation.rs` — All `assert!(check_*.is_err())` → `assert!(matches!(result, Err(ApiError::BadRequest(msg)) if msg.contains("...")))`
- `src/secrets/engine.rs` — `decrypt_wrong_key_fails`, `parse_master_key_*` → match on error message
- `src/pipeline/definition.rs` — `.to_string().contains(...)` → `matches!` on `PipelineError::InvalidDefinition`

**Replace `assert!(a == b)` with `assert_eq!(a, b)`:**
- Grep for `assert!(.*==.*)` across all test modules and replace

### 3b. Tautological Tests to Replace

| Test | File | Problem | Replace With |
|------|------|---------|-------------|
| `cache_key_deterministic` | `src/rbac/resolver.rs` | Calls same fn twice, same input | Test cache key **uniqueness**: different users produce different keys |
| `dev_master_key_is_deterministic` | `src/secrets/engine.rs` | Same fn, no randomness | Test key is non-zero AND works as valid encryption key |
| `default_smtp_port`, `default_pipeline_namespace` | `src/config.rs` | Guard with `if env::var(...).is_err()` — env-dependent | Use `Config::test_default()` and assert known values |

### 3c. Missing Edge Cases to Add (existing modules)

**`src/validation.rs`** — Add boundary tests:
- `check_name("")` → err, `check_name("a")` → ok, `check_name("a".repeat(255))` → ok, `check_name("a".repeat(256))` → err
- `check_name("café")` → err or ok? (`is_alphanumeric()` is Unicode-aware — verify intent)
- `check_email("a@b")` → ok (min 3), `check_email("@b")` → err (min 3 with @)
- `check_labels(vec!["".into()])` → err (empty label), `check_labels(vec!["a".repeat(100)])` → ok, 101 → err
- `check_branch_name("main\0evil")` → err (null byte mid-string)

**`src/auth/middleware.rs`** — Add whitespace/IPv6 edge cases:
- `"Bearer  abc"` (double space), session cookie with `=` in value, IPv6 in X-Forwarded-For

**`src/secrets/engine.rs`** — Add crypto boundaries:
- Empty plaintext roundtrip, 63/65 hex char master key, whitespace around key

**`src/pipeline/definition.rs`** — Add pattern matching:
- Prefix wildcard `"*-release"`, suffix match, empty branches/actions lists

### 3d. Activate Unused Dev Dependencies

**proptest** — Use for:
- `src/rbac/types.rs`: `Permission` as_str/from_str roundtrip (all variants)
- `src/validation.rs`: Random valid/invalid hex strings for `check_lfs_oid`
- `src/observe/proto.rs`: Trace/span ID hex roundtrips with arbitrary bytes

**rstest** — Add to `[dev-dependencies]` and use for:
- `src/validation.rs`: Parameterize SSRF blocked IPs (`#[case]` for each IP)
- `src/observe/proto.rs`: Parameterize severity/span_kind/status_code mapping
- `src/auth/user_type.rs`: Capability matrix (login, spawn_agents, requires_password)

**insta** — Use for API response snapshot tests in integration tests

---

## 4. New Test Infrastructure

### 4a. Add `rstest` dev-dependency

```toml
# Cargo.toml [dev-dependencies]
rstest = "0.25"
```

### 4b. Create Integration Test Helpers

**File: `tests/helpers/mod.rs`**
- `create_test_user(pool, name)` → returns `(user_id, raw_token)`
- `create_test_project(pool, owner_id, name)` → returns `project_id`
- `assign_role(pool, user_id, role_name, project_id)` → assigns role
- `test_app_state(pool)` → `AppState` with mock Valkey/MinIO/Kube

**File: `src/auth/middleware.rs` (test-only additions)**
- `AuthUser::test_human(user_id)` constructor
- `AuthUser::test_with_scopes(user_id, scopes)` constructor

### 4c. Integration Test Directory Structure

```
tests/
  helpers/
    mod.rs
  auth_integration.rs        # login/session/token flows
  project_integration.rs     # CRUD + visibility + soft-delete
  rbac_integration.rs        # permission resolution + delegation
  secrets_integration.rs     # encrypt/store/resolve with real DB
```

All use `#[sqlx::test(migrations = "migrations")]` — each test gets an isolated temp DB.

---

## 5. New Tests to Write (Prioritized)

### Tier 1: Unit Tests (no I/O, fast) — ~60 tests

**5.1.1 `src/error.rs`** — Error conversion tests:
- `sqlx_row_not_found_maps_to_404`
- `sqlx_unique_violation_maps_to_409`
- `sqlx_generic_error_maps_to_500_hides_details`

**5.1.2 `src/validation.rs`** — Boundary tests (see Section 3c for full list, ~20 tests)

**5.1.3 `src/validation.rs`** — IPv6 SSRF tests (after fix):
- `private_ip_ipv6_unique_local_blocked` (fc00::1)
- `private_ip_ipv6_link_local_blocked` (fe80::1)
- `ssrf_allows_external_ipv6` (2001:db8::1)

**5.1.4 `src/auth/middleware.rs`** — Edge cases (~5 tests)

**5.1.5 `src/secrets/engine.rs`** — Boundary tests (~5 tests)

**5.1.6 `src/pipeline/definition.rs`** — Pattern matching + field verification (~8 tests)

**5.1.7 `src/rbac/types.rs`** — proptest Permission roundtrip

### Tier 2: Integration Tests (DB required) — ~40 tests

**5.2.1 `tests/auth_integration.rs`:**
- Login valid/invalid/inactive/rate-limited/non-human
- Session creation and cookie flags
- Token CRUD with scope validation and expiry enforcement
- User deactivation cascade

**5.2.2 `tests/project_integration.rs`:**
- CRUD with permission checks
- Visibility filter (private/internal/public)
- Soft-delete behavior
- Owner bypass

**5.2.3 `tests/rbac_integration.rs`:**
- `effective_permissions()` with global + project-scoped roles
- Delegation with expiry
- Cache invalidation

**5.2.4 `tests/secrets_integration.rs`:**
- Create + resolve secret (roundtrip)
- Scope enforcement (`"all"` vs specific)
- Project-scoped overrides global
- Template substitution

### Tier 3: Complex Integration — ~15 tests (future)

- Git operations (bare repo init, branch listing, merge)
- Pipeline triggering (parse `.platformci.yml`, create steps)
- Webhook dispatch with HMAC verification (requires mock HTTP server)
- WebSocket agent session streaming

---

## 6. Efficiency & CI

### Tests That Can Run in Parallel
- All unit tests (Tier 1) — already parallel via `cargo nextest`
- `#[sqlx::test]` integration tests — each gets isolated temp DB, inherently parallel

### Identified Slow Tests
- `src/auth/password.rs` — argon2 hashing (~200ms each, 3 tests) — keep as-is (security-critical)
- `src/git/repo.rs` — `git init` subprocess (~50ms each) — keep as-is

### CI Addition
Add to `Justfile`:
```
test-integration:
    cargo nextest run --test '*_integration'
```

Update `just ci` or add a separate `just ci-full` that includes integration tests.

---

## 7. Implementation Sequence

| Phase | Scope | Est. Tests |
|-------|-------|-----------|
| **A** | Fix IPv6 SSRF bug + dedup `is_private_ip` | 3 tests + code fix |
| **B** | Tier 1 unit tests (validation boundaries, error conversion, auth edge cases) | ~60 tests |
| **C** | Test infrastructure (rstest dep, `tests/helpers/`, AuthUser constructors) | 0 tests (infra) |
| **D** | Existing test refactoring (assertions, tautologies, proptest activation) | ~10 tests modified |
| **E** | Tier 2 integration tests (auth, projects, RBAC, secrets) | ~40 tests |
| **F** | Tier 3 complex integration (git, pipelines, webhooks) | ~15 tests |

---

## Verification

After each phase:
1. `just test-unit` — all existing + new unit tests pass
2. `just lint` — no clippy warnings in test code
3. `just fmt` — test code formatted
4. For Tier 2+: `just test-integration` with running Postgres (via `just cluster-up` or local)
5. `just ci` — full gate passes

---

## Key Files to Modify

| File | Changes |
|------|---------|
| `Cargo.toml` | Add `rstest = "0.25"` to dev-deps |
| `src/validation.rs` | Fix IPv6 SSRF, add ~20 boundary tests |
| `src/api/webhooks.rs` | Remove duplicate `is_private_ip`, import from `validation` |
| `src/error.rs` | Add error conversion tests |
| `src/auth/middleware.rs` | Add edge case tests + `AuthUser::test_*` constructors |
| `src/secrets/engine.rs` | Add boundary tests |
| `src/pipeline/definition.rs` | Add pattern + field tests |
| `src/rbac/types.rs` | Add proptest roundtrip |
| `src/rbac/resolver.rs` | Replace tautological test |
| `src/config.rs` | Fix env-dependent tests |
| `src/observe/proto.rs` | Parameterize with rstest |
| `tests/helpers/mod.rs` | New: shared test fixtures |
| `tests/auth_integration.rs` | New: auth flow integration tests |
| `tests/project_integration.rs` | New: project CRUD integration tests |
| `tests/rbac_integration.rs` | New: RBAC resolution integration tests |
| `tests/secrets_integration.rs` | New: secrets engine integration tests |
