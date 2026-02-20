# Plan 17 — Integration Tests

## Overview

Implement comprehensive HTTP-level integration tests for the platform's API handler layer. Currently **zero** integration tests exist — the entire API handler layer (78 handlers, ~7,259 LOC across 13 files) is untested at the HTTP level. All current tests are inline unit tests (264 tests).

This plan executes the specification from Plan 15 with expanded detail, concrete code patterns, and additional edge cases discovered during codebase review.

**Target: 95+ integration tests across 7 test files + 1 helper module.**

---

## Motivation

- Every API handler is untested at the HTTP level — no middleware, routing, or JSON serialization coverage
- Auth middleware (`AuthUser` extractor) has never been tested through a real request
- RBAC permission checks are only unit-tested in isolation — never through handler + middleware + DB
- Validation errors, 404s, 409 conflicts, and 403 Forbidden responses are all unverified at the HTTP level
- Any refactoring to API handlers risks silent regressions with zero safety net

---

## Prerequisites

| Requirement | How to Provide |
|---|---|
| PostgreSQL | `just cluster-up` or standalone on `DATABASE_URL` |
| Valkey/Redis | `just cluster-up` or standalone on `VALKEY_URL` (default `redis://localhost:6379`) |
| Kubernetes | NOT required — dummy client for non-K8s tests |
| MinIO | NOT required — in-memory `opendal::services::Memory` |

---

## Architecture Decisions

### Test Execution Strategy

- **`#[sqlx::test(migrations = "migrations")]`** — each test gets an isolated temp database, automatic migration, automatic cleanup
- **`tower::ServiceExt::oneshot`** — full HTTP-level dispatch through the axum `Router`, testing auth middleware, JSON parsing, validation, DB queries, and response formatting in one shot
- **Real Valkey** — RBAC permission resolution uses Valkey caching; `flushdb` per test prevents cross-test pollution
- **In-memory object storage** — `opendal::services::Memory` avoids needing real MinIO for most tests
- **Dummy Kube client** — integration tests don't exercise K8s; panics loudly if accidentally called
- **Bootstrap per test** — each test bootstraps permissions, roles, and admin user in its fresh temp DB

### Why Real Valkey (Not Mock)

The RBAC permission resolver (`src/rbac/resolver.rs`) caches permissions in Valkey with a 5-minute TTL. Using a mock would not test:
- Cache key generation correctness
- Cache invalidation after role changes
- Race conditions between cache set and read
- JSON serialization/deserialization of `HashSet<Permission>`

### Kube Client Strategy

`kube::Client::try_default()` requires a kubeconfig. For CI without K8s:

```rust
let kube = kube::Client::try_default().await.unwrap_or_else(|_| {
    let config = kube::Config {
        cluster_url: "https://127.0.0.1:1".parse().unwrap(),
        ..kube::Config::default()
    };
    kube::Client::try_from(config).expect("dummy kube client")
});
```

This creates a lazy client that only fails when actually used. Tests that don't touch K8s endpoints pass fine.

---

## File Structure

```
tests/
  helpers/
    mod.rs                       # Shared test utilities (state, login, helpers)
  auth_integration.rs            # Login, sessions, tokens, user lifecycle (13 tests)
  admin_integration.rs           # Admin user CRUD, role management (14 tests)
  project_integration.rs         # Project CRUD, visibility, soft-delete (16 tests)
  rbac_integration.rs            # Permission resolution, roles, delegation (15 tests)
  issue_mr_integration.rs        # Issues, MRs, comments, reviews (15 tests)
  webhook_integration.rs         # Webhook CRUD, SSRF validation (12 tests)
  notification_integration.rs    # Notification list, mark-read, unread-count (10 tests)
```

---

## Step E1: Test Infrastructure (`tests/helpers/mod.rs`)

### Core Helper: `test_state(pool: PgPool) -> AppState`

```rust
pub async fn test_state(pool: PgPool) -> AppState {
    // 1. Bootstrap seed data (permissions, roles, admin user)
    bootstrap::run(&pool, Some("testpassword")).await.expect("bootstrap failed");

    // 2. Connect to real Valkey
    let valkey_url = std::env::var("VALKEY_URL")
        .unwrap_or_else(|_| "redis://localhost:6379".into());
    let valkey = /* ... connect and flushdb ... */;

    // 3. In-memory MinIO
    let minio = opendal::Operator::new(opendal::services::Memory::default())
        .expect("memory operator").finish();

    // 4. Dummy kube client (lazy, only fails on actual use)
    let kube = /* ... dummy client ... */;

    // 5. Config with test defaults
    let config = Config { /* test defaults: dev mode, localhost webauthn, etc. */ };

    // 6. WebAuthn with localhost RP
    let webauthn = /* ... localhost config ... */;

    AppState { pool, valkey, minio, kube, config: Arc::new(config), webauthn: Arc::new(webauthn) }
}
```

### HTTP Helper Functions

| Helper | Signature | Purpose |
|---|---|---|
| `test_router` | `(AppState) -> Router` | Build full API router with state |
| `admin_login` | `(&Router) -> String` | Login as bootstrap admin, return token |
| `create_user` | `(&Router, &str, &str, &str) -> (Uuid, String)` | Create user via admin API, login, return (id, token) |
| `create_project` | `(&Router, &str, &str, &str) -> Uuid` | Create project, return id |
| `assign_role` | `(&Router, &str, Uuid, &str, Option<Uuid>, &PgPool)` | Assign role to user |
| `get_json` | `(&Router, &str, &str) -> (StatusCode, Value)` | GET with Bearer auth |
| `post_json` | `(&Router, &str, &str, Value) -> (StatusCode, Value)` | POST with Bearer auth and JSON body |
| `patch_json` | `(&Router, &str, &str, Value) -> (StatusCode, Value)` | PATCH with Bearer auth |
| `delete_json` | `(&Router, &str, &str) -> (StatusCode, Value)` | DELETE with Bearer auth |
| `put_json` | `(&Router, &str, &str, Value) -> (StatusCode, Value)` | PUT with Bearer auth |
| `body_json` | `(Response<Body>) -> Value` | Extract JSON body from response |

### Justfile Additions

```just
test-integration:
    cargo nextest run --test '*'

ci-full: fmt lint deny test-unit test-integration build
    @echo "All checks passed (including integration tests)"
```

### Config::test_default() Visibility Fix

`Config::test_default()` is behind `#[cfg(test)]` which only applies to the library's own tests, not `tests/` integration tests. Fix: construct Config directly in `test_state()` (no library changes needed).

---

## Step E2: Auth Integration Tests (13 tests)

**File: `tests/auth_integration.rs`**

| # | Test | Verifies | Expected |
|---|------|----------|----------|
| 1 | `login_valid_credentials` | POST `/api/login` with correct admin creds | 200, body has `token`, `user.name == "admin"`, `expires_at` in future |
| 2 | `login_wrong_password` | POST `/api/login` with wrong password | 401, body has `"error"`, no token leaked |
| 3 | `login_nonexistent_user` | POST `/api/login` with unknown username | 401 (timing-safe: same error as wrong password) |
| 4 | `login_inactive_user` | Deactivate user via admin, then try login | 401 |
| 5 | `login_rate_limited` | Send 11 login attempts rapidly | First 10 get 200/401, 11th gets 429 |
| 6 | `get_me_with_valid_token` | GET `/api/me` with Bearer token | 200, correct user data |
| 7 | `get_me_without_token` | GET `/api/me` with no auth header | 401 |
| 8 | `get_me_with_expired_token` | Create token, expire it in DB, GET `/api/me` | 401 |
| 9 | `create_api_token` | POST `/api/tokens` with valid scopes | 201, token starts with `plat_`, has scopes |
| 10 | `create_api_token_scope_escalation_blocked` | Create token with permission user doesn't have | 403 or 400 |
| 11 | `list_and_delete_api_token` | Create token, list, delete, verify gone | Token removed from list |
| 12 | `update_own_profile` | PATCH `/api/me` with new display_name | 200, updated field |
| 13 | `non_human_user_cannot_login` | Create agent-type user, attempt login | 401 (user_type check) |

### Key Pattern (every test follows this):

```rust
#[sqlx::test(migrations = "migrations")]
async fn login_valid_credentials(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::post_json(
        &app, "", "/api/login",
        serde_json::json!({ "name": "admin", "password": "testpassword" }),
    ).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body["token"].is_string(), "response must include token");
    assert_eq!(body["user"]["name"], "admin");
    // Verify expires_at is in the future
    let expires = body["expires_at"].as_str().unwrap();
    let parsed = chrono::DateTime::parse_from_rfc3339(expires).unwrap();
    assert!(parsed > chrono::Utc::now());
}
```

### Edge Cases to Test:

- **Timing safety**: `login_nonexistent_user` should take approximately the same time as `login_wrong_password` (both run argon2)
- **Rate limiting**: Requires 11 rapid sequential requests to the same endpoint
- **Token expiry**: Directly update `expires_at` in DB to past, then test

---

## Step E3: Admin Integration Tests (14 tests)

**File: `tests/admin_integration.rs`**

| # | Test | Verifies | Expected |
|---|------|----------|----------|
| 1 | `admin_create_user` | POST `/api/admin/users` | 201, user created with all fields |
| 2 | `admin_create_user_duplicate_name` | Create user with existing name | 409 conflict |
| 3 | `admin_create_user_invalid_email` | POST with `email: "not-an-email"` | 400 validation error |
| 4 | `admin_create_user_short_password` | POST with 3-char password | 400 (min 8) |
| 5 | `admin_list_users` | GET `/api/admin/users` | 200, includes admin + created users |
| 6 | `admin_get_user_by_id` | GET `/api/admin/users/{id}` | 200, correct user data |
| 7 | `admin_update_user` | PATCH `/api/admin/users/{id}` | 200, updated fields |
| 8 | `admin_deactivate_user` | POST deactivate endpoint | 200, user `is_active = false` |
| 9 | `deactivated_user_cannot_login` | Deactivate user, try login | 401 |
| 10 | `deactivated_user_token_revoked` | Deactivate user, use existing token for GET /api/me | 401 |
| 11 | `non_admin_cannot_create_user` | Regular user tries POST `/api/admin/users` | 403 |
| 12 | `non_admin_cannot_list_users` | Regular user tries GET `/api/admin/users` | 403 |
| 13 | `admin_create_token_for_user` | POST `/api/admin/users/{id}/tokens` | 201, token for other user |
| 14 | `admin_actions_create_audit_log` | Create user, query `audit_log` table directly | audit_log row with `action = "user.create"` exists |

### Edge Cases:

- **Cascade on deactivation**: When a user is deactivated, all sessions and API tokens must be deleted, and permission cache must be invalidated
- **Audit trail**: Admin mutations must produce audit_log entries with correct `actor_id`, `action`, `resource`, `resource_id`

---

## Step E4: Project Integration Tests (16 tests)

**File: `tests/project_integration.rs`**

| # | Test | Verifies | Expected |
|---|------|----------|----------|
| 1 | `create_project` | POST `/api/projects` | 201, has `id`, `name`, default `visibility = "private"` |
| 2 | `create_project_with_visibility` | POST with `visibility: "public"` | 201, `visibility = "public"` |
| 3 | `create_project_invalid_name` | POST with `name: "has spaces"` | 400 validation error |
| 4 | `create_project_empty_name` | POST with `name: ""` | 400 |
| 5 | `create_project_name_too_long` | POST with 256-char name | 400 |
| 6 | `create_project_duplicate_name` | Create two projects with same name | 409 conflict |
| 7 | `get_project_by_id` | GET `/api/projects/{id}` | 200, correct data |
| 8 | `get_nonexistent_project` | GET `/api/projects/{random_uuid}` | 404 |
| 9 | `update_project` | PATCH `/api/projects/{id}` | 200, updated fields |
| 10 | `delete_project_soft_delete` | DELETE `/api/projects/{id}`, then GET | 200 on delete, then 404 (soft-deleted) |
| 11 | `list_projects_pagination` | Create 5 projects, GET `limit=2&offset=0` | `items.len() == 2`, `total >= 5` |
| 12 | `list_projects_pagination_offset` | GET `limit=2&offset=2` on 5 projects | Different items than offset=0 |
| 13 | `private_project_hidden_from_non_owner` | User A creates private project, User B lists | Not in User B's list |
| 14 | `public_project_visible_to_all` | User A creates public project, User B GETs it | 200 |
| 15 | `project_owner_has_implicit_access` | Owner can read/update own project without explicit role | 200 on GET and PATCH |
| 16 | `delete_project_requires_permission` | Non-owner without `project:delete` tries DELETE | 403 |

### Edge Cases:

- **Soft-delete**: After DELETE, the project still exists in DB with `is_active = false`. LIST and GET both exclude it.
- **Visibility cascade**: Private projects return 404 (not 403) to non-authorized users — prevents leaking existence.
- **Pagination**: Verify `total` count is accurate across visibility boundaries (User B shouldn't count User A's private projects in `total`).

---

## Step E5: RBAC Integration Tests (15 tests)

**File: `tests/rbac_integration.rs`**

| # | Test | Verifies | Expected |
|---|------|----------|----------|
| 1 | `admin_has_all_permissions` | Admin user can access admin-only endpoints | 200 on admin endpoints |
| 2 | `developer_role_permissions` | User with developer role can read/write projects | 200 on project CRUD |
| 3 | `viewer_role_read_only` | User with viewer role can read but not write | 200 on GET, 403 on POST/PATCH |
| 4 | `no_role_gets_forbidden` | User with no roles tries project write | 403 |
| 5 | `project_scoped_role` | Assign role for specific project only | 200 on that project, 403 on other |
| 6 | `global_role_applies_to_all_projects` | Assign global developer role, access multiple projects | 200 on all projects |
| 7 | `role_assignment_creates_audit` | Assign role, check `audit_log` has entry | Row with `action = "role.assign"` |
| 8 | `delegation_grants_temporary_access` | Create delegation, delegatee can access | 200 within delegation period |
| 9 | `expired_delegation_denied` | Create delegation with past `expires_at`, try access | 403 |
| 10 | `revoked_delegation_denied` | Create delegation, revoke it, try access | 403 |
| 11 | `delegation_requires_delegator_holds_permission` | User without perm tries to delegate it | 403 |
| 12 | `system_role_cannot_be_deleted` | Try DELETE on system role (admin, developer) | 400 or 403 |
| 13 | `custom_role_crud` | Create custom role, set permissions, assign, verify | Full lifecycle works |
| 14 | `permission_cache_invalidation` | Assign role → verify access → remove role → verify denied | Removal takes effect immediately |
| 15 | `list_permissions_and_roles` | GET endpoints for permissions and roles | 200, correct counts |

### Key Test: Permission Cache Invalidation (test 14)

This is critical — tests that role removal actually invalidates the Valkey cache:

```rust
#[sqlx::test(migrations = "migrations")]
async fn permission_cache_invalidation(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    // Create user + project
    let (user_id, user_token) = helpers::create_user(&app, &admin_token, "cacheuser", "cache@test.com").await;
    let project_id = helpers::create_project(&app, &admin_token, "cacheproject", "private").await;

    // Assign developer role for project
    helpers::assign_role(&app, &admin_token, user_id, "developer", Some(project_id), &pool).await;

    // Verify access works (this will prime the permission cache)
    let (status, _) = helpers::get_json(&app, &user_token, &format!("/api/projects/{project_id}")).await;
    assert_eq!(status, StatusCode::OK);

    // Remove role
    // ... (DELETE role assignment)

    // Verify access denied (cache must have been invalidated)
    let (status, _) = helpers::get_json(&app, &user_token, &format!("/api/projects/{project_id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND); // 404, not 403
}
```

---

## Step E6: Issue/MR Integration Tests (15 tests)

**File: `tests/issue_mr_integration.rs`**

| # | Test | Verifies | Expected |
|---|------|----------|----------|
| 1 | `create_issue` | POST `/api/projects/{id}/issues` | 201, auto-incremented `number`, `state == "open"` |
| 2 | `create_issue_empty_title` | POST with empty title | 400 validation error |
| 3 | `create_issue_title_too_long` | POST with 501-char title | 400 |
| 4 | `list_issues` | Create 3 issues, GET list | 200, `items.len() >= 3` |
| 5 | `get_issue_by_number` | GET `/api/projects/{id}/issues/{number}` | 200, correct data |
| 6 | `update_issue` | PATCH (change title, add labels) | 200, updated fields |
| 7 | `close_and_reopen_issue` | PATCH `state: "closed"` then `state: "open"` | State transitions correctly |
| 8 | `issue_auto_increment_numbers` | Create 3 issues, verify numbers 1, 2, 3 | Sequential per-project numbers |
| 9 | `add_issue_comment` | POST comment | 201, correct body |
| 10 | `create_merge_request` | POST `/api/projects/{id}/merge-requests` | 201, auto-incremented `number` |
| 11 | `list_merge_requests` | Create 2 MRs, GET list | 200, items present |
| 12 | `update_merge_request` | PATCH MR title | 200, updated |
| 13 | `add_mr_comment` | POST comment on MR | 201 |
| 14 | `issue_requires_project_read` | User without read access tries GET issue | 404 (not 403) |
| 15 | `issue_write_requires_project_write` | User with read-only tries POST issue | 403 |

### Auto-Increment Isolation

Test that issue numbers are scoped per project:
- Project A: issues 1, 2, 3
- Project B: issues 1, 2
- Verify they don't interfere

---

## Step E7: Webhook Integration Tests (12 tests)

**File: `tests/webhook_integration.rs`**

| # | Test | Verifies | Expected |
|---|------|----------|----------|
| 1 | `create_webhook` | POST with valid URL and events | 201, has `id`, `url`, `events` |
| 2 | `create_webhook_invalid_url` | POST with `ftp://example.com` | 400, scheme validation |
| 3 | `create_webhook_ssrf_localhost` | POST with `http://localhost/hook` | 400, SSRF blocked |
| 4 | `create_webhook_ssrf_private_10` | POST with `http://10.0.0.1/hook` | 400, private IP |
| 5 | `create_webhook_ssrf_private_172` | POST with `http://172.16.0.1/hook` | 400, private IP |
| 6 | `create_webhook_ssrf_private_192` | POST with `http://192.168.1.1/hook` | 400, private IP |
| 7 | `create_webhook_ssrf_metadata` | POST with `http://169.254.169.254/` | 400, cloud metadata |
| 8 | `create_webhook_ssrf_ipv6_loopback` | POST with `http://[::1]/hook` | 400, IPv6 loopback |
| 9 | `list_webhooks` | Create 2 webhooks, GET list | 200, 2 items |
| 10 | `update_and_delete_webhook` | PATCH URL, then DELETE | Updated, then 404 on re-GET |
| 11 | `webhook_requires_project_write` | Read-only user tries webhook CRUD | 403 |
| 12 | `webhook_secret_not_exposed` | Create with secret, GET webhook | Response does NOT include raw secret |

---

## Step E8: Notification Integration Tests (10 tests)

**File: `tests/notification_integration.rs`**

| # | Test | Verifies | Expected |
|---|------|----------|----------|
| 1 | `list_notifications_empty` | GET `/api/notifications` for new user | 200, `items: []`, `total: 0` |
| 2 | `unread_count_zero` | GET `/api/notifications/unread-count` | 200, `count: 0` |
| 3 | `list_notifications_with_limit` | Insert notifications via DB, list with `limit=2` | 200, 2 items |
| 4 | `list_notifications_filter_by_status` | Filter by `status=unread` | Only unread returned |
| 5 | `list_notifications_filter_by_type` | Filter by `type=build` | Only build notifications |
| 6 | `mark_notification_read` | PATCH `/api/notifications/{id}/read` | 200, status becomes "read" |
| 7 | `mark_read_updates_unread_count` | Mark one read, check count decremented | Count decreases by 1 |
| 8 | `mark_nonexistent_notification` | PATCH with random UUID | 404 |
| 9 | `cannot_read_other_users_notifications` | User A marks User B's notification | 404 (not 403) |
| 10 | `notifications_ordered_by_created_at` | Multiple notifications, verify DESC order | Most recent first |

---

## Required Source Changes

### `src/config.rs` — Expose test config

The `Config::test_default()` is behind `#[cfg(test)]` which doesn't apply to integration tests. Rather than adding a feature flag, construct Config directly in the test helper.

### `Cargo.toml` — Dev dependencies (already present)

All required dev-deps are already in `Cargo.toml`:
- `tower = "0.5"` (with `util` feature)
- `hyper = "1"` (with `full` feature)
- `insta = "1"` (with `json` feature)
- `rstest = "0.25"` (from Plan 14)

### `src/lib.rs` — Public re-exports

Ensure these are `pub` for integration test import:
- `api::router()` — already public
- `store::AppState` — already public
- `store::bootstrap::run()` — already public
- `config::Config` — already public

---

## CI Configuration

### GitHub Actions Service Containers

```yaml
services:
  postgres:
    image: postgres:17
    env:
      POSTGRES_USER: postgres
      POSTGRES_PASSWORD: postgres
      POSTGRES_DB: postgres
    ports: ["5432:5432"]
    options: >-
      --health-cmd pg_isready
      --health-interval 10s
      --health-timeout 5s
      --health-retries 5
  valkey:
    image: valkey/valkey:8
    ports: ["6379:6379"]
    options: >-
      --health-cmd "valkey-cli ping"
      --health-interval 10s
      --health-timeout 5s
      --health-retries 5
```

### Local Development

```bash
# Option A: Kind cluster (provides Postgres + Valkey via port-forward)
just cluster-up

# Option B: Standalone containers
docker run -d --name pg -p 5432:5432 -e POSTGRES_PASSWORD=postgres postgres:17
docker run -d --name valkey -p 6379:6379 valkey/valkey:8

# Run integration tests
just test-integration
```

---

## Implementation Sequence

| Step | Scope | Files | Tests |
|------|-------|-------|-------|
| **E1** | Test infrastructure | `tests/helpers/mod.rs`, `Justfile` | 0 (infra) |
| **E2** | Auth tests | `tests/auth_integration.rs` | 13 |
| **E3** | Admin tests | `tests/admin_integration.rs` | 14 |
| **E4** | Project tests | `tests/project_integration.rs` | 16 |
| **E5** | RBAC tests | `tests/rbac_integration.rs` | 15 |
| **E6** | Issue/MR tests | `tests/issue_mr_integration.rs` | 15 |
| **E7** | Webhook tests | `tests/webhook_integration.rs` | 12 |
| **E8** | Notification tests | `tests/notification_integration.rs` | 10 |

**Order**: E1 first (infra). E2-E3 next (auth/admin establish user creation patterns). E4-E8 can be parallelized.

---

## Verification

After each step:
1. `cargo nextest run --test '*'` — all integration tests pass
2. `just lint` — no clippy warnings
3. `just fmt` — formatted

After all steps:
1. `just ci` — existing unit tests still pass
2. `just test-integration` — all 95+ integration tests pass
3. Confirm Valkey is flushed between tests (no cross-test pollution)

---

## Risks and Mitigations

| Risk | Impact | Mitigation |
|------|--------|-----------|
| Valkey unavailable in CI | Tests fail | `VALKEY_URL` env var + Valkey service container |
| Kube client panics | Tests crash | Dummy client; gate K8s tests with `#[ignore]` |
| Slow tests (argon2 ~200ms/login) | CI slow | Acceptable; consider reduced work factor in test config |
| Bootstrap changes | All tests fail | Bootstrap is idempotent; tests use deterministic seed data |
| API route changes | Many tests break | URL construction via helper functions |
| Cross-test Valkey pollution | Flaky tests | `flushdb` in `test_state()` per test |

---

## Estimated Scope

| Metric | Value |
|--------|-------|
| New test files | 8 (7 test files + 1 helper module) |
| New integration tests | 95+ |
| External dependencies | Postgres (sqlx::test), Valkey (real) |
| Estimated LOC | ~3,000-3,500 |
| Implementation steps | 8 (E1-E8) |
