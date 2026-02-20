# Phase E — Extensive Integration Tests

## Overview

Add comprehensive integration tests for the platform's API handler layer. Currently **zero** integration tests exist (no `tests/` directory). The entire API handler layer (78 handlers, ~7,259 LOC across 13 files) is untested at the HTTP level. All current tests are inline unit tests.

This plan uses `#[sqlx::test(migrations = "migrations")]` for per-test isolated temp databases, `tower::ServiceExt::oneshot` for HTTP-level request dispatch through the full axum `Router`, and real Valkey for cache operations.

**Target: 83 integration tests across 6 test files + 1 helper module.**

---

## 1. Test Infrastructure

### 1a. Dependencies (already present)

All required dev-dependencies are already in `Cargo.toml`:
- `tower = "0.5"` (with `util` feature) — for `ServiceExt::oneshot`
- `hyper = "1"` (with `full` feature) — for `Request` construction
- `insta = "1"` (with `json` feature) — for snapshot testing (future)
- `proptest = "1"` — already activated in Phase D
- `rstest = "0.25"` — added in Phase C

No new dependencies needed.

### 1b. Helper Module: `tests/helpers/mod.rs`

Shared test utilities used by all integration test files.

```rust
// tests/helpers/mod.rs

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use fred::prelude::*;
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

use platform::config::Config;
use platform::store::AppState;
use platform::store::bootstrap;

/// Build a fully-wired AppState suitable for integration tests.
///
/// Requires:
/// - `pool`: provided by `#[sqlx::test]` (isolated temp DB)
/// - Real Valkey at `VALKEY_URL` (default: redis://localhost:6379)
/// - MinIO: in-memory opendal operator (no real MinIO needed)
/// - Kube: dummy client (tests don't exercise K8s)
/// - WebAuthn: real instance with localhost RP
pub async fn test_state(pool: PgPool) -> AppState {
    // Bootstrap seed data (permissions, roles, admin user)
    bootstrap::run(&pool, Some("testpassword"))
        .await
        .expect("bootstrap failed");

    // Connect to real Valkey (tests need cache for RBAC)
    let valkey_url = std::env::var("VALKEY_URL")
        .unwrap_or_else(|_| "redis://localhost:6379".into());
    let valkey_config = fred::types::config::Config::from_url(&valkey_url)
        .expect("invalid valkey URL");
    let valkey = fred::clients::Pool::new(valkey_config, None, None, None, 2)
        .expect("valkey pool creation failed");
    valkey.init().await.expect("valkey connection failed");

    // Flush test keys to prevent cross-test pollution
    valkey.flushdb::<()>(false).await.ok();

    // In-memory object storage (no real MinIO)
    let minio = opendal::Operator::new(opendal::services::Memory::default())
        .expect("memory operator")
        .finish();

    // Dummy kube client — panics if actually called
    let kube = kube::Client::try_default()
        .await
        .unwrap_or_else(|_| {
            // Build a minimal client that will fail on use
            // Tests that need kube should be in a separate file
            panic!("kube client not available — skip K8s tests")
        });

    let config = Config::test_default();

    let webauthn = webauthn_rs::prelude::WebauthnBuilder::new(
        &config.webauthn_rp_id,
        &url::Url::parse(&config.webauthn_rp_origin).unwrap(),
    )
    .expect("webauthn builder")
    .rp_name(&config.webauthn_rp_name)
    .build()
    .expect("webauthn build");

    AppState {
        pool,
        valkey,
        minio,
        kube,
        config: Arc::new(config),
        webauthn: Arc::new(webauthn),
    }
}

/// Build the full API router wired with the given state.
pub fn test_router(state: AppState) -> Router {
    platform::api::router()
        .with_state(state)
}

/// Login as the bootstrapped admin user and return the session token.
pub async fn admin_login(app: &Router) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/login")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "name": "admin",
                        "password": "testpassword"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK, "admin login failed");
    let body = body_json(resp).await;
    body["token"].as_str().unwrap().to_owned()
}

/// Create a test user via admin API, return (user_id, raw_token).
pub async fn create_user(
    app: &Router,
    admin_token: &str,
    name: &str,
    email: &str,
) -> (Uuid, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/admin/users")
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {admin_token}"))
                .body(Body::from(
                    serde_json::json!({
                        "name": name,
                        "email": email,
                        "password": "testpassword123"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED, "create user failed");
    let body = body_json(resp).await;
    let user_id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();

    // Login as the new user to get a token
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/login")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "name": name,
                        "password": "testpassword123"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK, "user login failed");
    let body = body_json(resp).await;
    let token = body["token"].as_str().unwrap().to_owned();

    (user_id, token)
}

/// Create a project via API, return project_id.
pub async fn create_project(
    app: &Router,
    token: &str,
    name: &str,
    visibility: &str,
) -> Uuid {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/projects")
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::from(
                    serde_json::json!({
                        "name": name,
                        "visibility": visibility,
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED, "create project failed");
    let body = body_json(resp).await;
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

/// Assign a role to a user (optionally project-scoped).
pub async fn assign_role(
    app: &Router,
    admin_token: &str,
    user_id: Uuid,
    role_name: &str,
    project_id: Option<Uuid>,
    pool: &PgPool,
) {
    // Look up role_id by name
    let role = sqlx::query_scalar!(
        "SELECT id FROM roles WHERE name = $1",
        role_name,
    )
    .fetch_one(pool)
    .await
    .unwrap();

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/api/admin/users/{user_id}/roles"))
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {admin_token}"))
                .body(Body::from(
                    serde_json::json!({
                        "role_id": role,
                        "project_id": project_id,
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(
        resp.status().is_success(),
        "assign role failed: {}",
        resp.status()
    );
}

/// Helper: send GET with auth.
pub async fn get_json(
    app: &Router,
    token: &str,
    uri: &str,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = resp.status();
    let body = body_json(resp).await;
    (status, body)
}

/// Helper: send POST with auth and JSON body.
pub async fn post_json(
    app: &Router,
    token: &str,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = resp.status();
    let body = body_json(resp).await;
    (status, body)
}

/// Helper: send PATCH with auth and JSON body.
pub async fn patch_json(
    app: &Router,
    token: &str,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(uri)
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = resp.status();
    let body = body_json(resp).await;
    (status, body)
}

/// Helper: send DELETE with auth.
pub async fn delete_json(
    app: &Router,
    token: &str,
    uri: &str,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(uri)
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = resp.status();
    let body = body_json(resp).await;
    (status, body)
}

/// Extract JSON body from a response.
pub async fn body_json(resp: axum::http::Response<Body>) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}
```

**Key design decisions:**
- **Real Valkey**: RBAC permission resolution uses Valkey caching. A mock would not test the actual cache behavior. `flushdb` per test prevents cross-test pollution.
- **In-memory MinIO**: `opendal::services::Memory` avoids needing real MinIO for most tests. Only LFS/artifact tests would need real object storage.
- **Dummy Kube**: Integration tests don't exercise K8s. Tests that need it belong in E2E.
- **Bootstrap per test**: Each `#[sqlx::test]` gets a fresh temp DB. Bootstrap seeds permissions, roles, and admin user.
- **`oneshot` dispatch**: Full HTTP-level testing through the axum router. Tests auth middleware, JSON parsing, validation, DB queries, and response formatting in one shot.

### 1c. Kube Client Fallback

The dummy kube client will panic if called. For integration tests that don't touch K8s endpoints (auth, projects, issues, MRs, RBAC, secrets, webhooks), this is fine. If a test accidentally hits a K8s path, it fails loudly.

For the `test_state` function, if `kube::Client::try_default()` fails (no kubeconfig), we need a workaround. Options:

1. **Preferred**: Gate kube construction behind `cfg(test)` with a no-op client
2. **Fallback**: Skip kube-dependent tests via `#[ignore]` attribute

Implementation: use a conditional that checks for kubeconfig and creates a real or panicking client.

### 1d. Justfile Additions

```just
# -- Test -----------------------------------------------------------
test-integration:
    cargo nextest run --test '*'

ci-full: fmt lint deny test-unit test-integration build
    @echo "All checks passed (including integration tests)"
```

---

## 2. Test File Structure

```
tests/
  helpers/
    mod.rs                       # Shared test utilities (Section 1b)
  auth_integration.rs            # Login, sessions, tokens, user lifecycle
  project_integration.rs         # Project CRUD, visibility, soft-delete
  rbac_integration.rs            # Permission resolution, roles, delegation
  issue_mr_integration.rs        # Issues, MRs, comments, reviews, merge
  admin_integration.rs           # Admin user management, role management
  webhook_integration.rs         # Webhook CRUD, SSRF validation, dispatch
```

---

## 3. Test Specifications

### 3a. `tests/auth_integration.rs` — 13 tests

| # | Test Name | What It Verifies | Key Assertions |
|---|-----------|-----------------|----------------|
| 1 | `login_valid_credentials` | POST `/api/login` with correct admin credentials | 200, body has `token`, `user.name == "admin"`, `expires_at` in future |
| 2 | `login_wrong_password` | POST `/api/login` with wrong password | 401, body has `"error"`, no token leaked |
| 3 | `login_nonexistent_user` | POST `/api/login` with unknown username | 401 (timing-safe: same error as wrong password) |
| 4 | `login_inactive_user` | Deactivate user, then try login | 401 |
| 5 | `login_rate_limited` | Send 11 login attempts rapidly | First 10 get 200/401, 11th gets 429 |
| 6 | `get_me_with_valid_token` | GET `/api/me` with Bearer token | 200, correct user data |
| 7 | `get_me_without_token` | GET `/api/me` with no auth header | 401 |
| 8 | `get_me_with_expired_token` | Create token, expire it in DB, GET `/api/me` | 401 |
| 9 | `create_api_token` | POST `/api/tokens` | 201, token starts with `plat_`, has scopes |
| 10 | `create_api_token_scope_escalation_blocked` | Create token with permission user doesn't have | 403 or 400 |
| 11 | `list_and_delete_api_token` | Create token, list tokens, delete token, verify gone | 200 list, 200 delete, token removed from list |
| 12 | `update_own_profile` | PATCH `/api/me` with new display_name | 200, updated field |
| 13 | `non_human_user_cannot_login` | Create agent user, attempt login | 401 (user_type check) |

### 3b. `tests/project_integration.rs` — 14 tests

| # | Test Name | What It Verifies | Key Assertions |
|---|-----------|-----------------|----------------|
| 1 | `create_project` | POST `/api/projects` | 201, has `id`, `name`, `visibility` = "private" default |
| 2 | `create_project_with_visibility` | POST with `visibility: "public"` | 201, visibility = "public" |
| 3 | `create_project_invalid_name` | POST with `name: "has spaces"` | 400, validation error |
| 4 | `create_project_duplicate_name` | Create two projects with same name | 409 conflict |
| 5 | `get_project_by_id` | GET `/api/projects/{id}` | 200, correct data |
| 6 | `get_nonexistent_project` | GET `/api/projects/{random_uuid}` | 404 |
| 7 | `update_project` | PATCH `/api/projects/{id}` | 200, updated fields |
| 8 | `delete_project_soft_delete` | DELETE `/api/projects/{id}` | 200, then GET returns 404 (soft-deleted) |
| 9 | `list_projects_pagination` | Create 5 projects, GET with `limit=2&offset=0` | 200, `items.len() == 2`, `total == 5` |
| 10 | `private_project_hidden_from_non_owner` | User A creates private project, User B lists projects | User B's list does not include User A's private project |
| 11 | `public_project_visible_to_all` | User A creates public project, User B GETs it | 200 |
| 12 | `internal_project_visible_to_authenticated` | Create internal project, other authenticated user can see it | 200 |
| 13 | `project_owner_has_implicit_access` | Owner can read/update own project without explicit role | 200 on GET and PATCH |
| 14 | `delete_project_requires_permission` | Non-owner without `project:delete` tries DELETE | 403 |

### 3c. `tests/rbac_integration.rs` — 15 tests

| # | Test Name | What It Verifies | Key Assertions |
|---|-----------|-----------------|----------------|
| 1 | `admin_has_all_permissions` | Admin user can access admin-only endpoints | 200 on admin endpoints |
| 2 | `developer_role_permissions` | User with developer role can read/write projects | 200 on project CRUD |
| 3 | `viewer_role_read_only` | User with viewer role can read but not write | 200 on GET, 403 on POST/PATCH |
| 4 | `no_role_gets_forbidden` | User with no roles tries project write | 403 |
| 5 | `project_scoped_role` | Assign role to user for specific project only | 200 on that project, 403 on other project |
| 6 | `global_role_applies_to_all_projects` | Assign global developer role, access multiple projects | 200 on all projects |
| 7 | `role_assignment_creates_audit` | Assign role, check audit_log has entry | audit_log row with `action = "role.assign"` |
| 8 | `delegation_grants_temporary_access` | Create delegation, delegatee can access | 200 within delegation period |
| 9 | `expired_delegation_denied` | Create delegation with past `expires_at`, try access | 403 |
| 10 | `delegation_requires_admin_delegate` | Non-admin tries to create delegation | 403 |
| 11 | `system_role_cannot_be_deleted` | Try DELETE on system role (admin, developer, etc.) | 400 or 403 |
| 12 | `custom_role_crud` | Create custom role, set permissions, assign, verify access | Full lifecycle works |
| 13 | `permission_cache_invalidation` | Assign role, verify access, remove role, verify denied | Removal takes effect (cache invalidated) |
| 14 | `list_permissions_endpoint` | GET `/api/admin/permissions` | 200, returns all 13 system permissions |
| 15 | `list_roles_endpoint` | GET `/api/admin/roles` | 200, returns at least 5 system roles |

### 3d. `tests/issue_mr_integration.rs` — 15 tests

| # | Test Name | What It Verifies | Key Assertions |
|---|-----------|-----------------|----------------|
| 1 | `create_issue` | POST `/api/projects/{id}/issues` | 201, has auto-incremented `number`, `state == "open"` |
| 2 | `create_issue_validation` | POST with empty title | 400, validation error |
| 3 | `list_issues` | Create 3 issues, GET list | 200, `items.len() >= 3`, ordered by created_at DESC |
| 4 | `get_issue_by_number` | GET `/api/projects/{id}/issues/{number}` | 200, correct data |
| 5 | `update_issue` | PATCH issue (change title, add labels) | 200, updated fields |
| 6 | `close_issue` | PATCH with `state: "closed"` | 200, state changed |
| 7 | `issue_auto_increment_numbers` | Create 3 issues, verify numbers are 1, 2, 3 | Sequential numbers |
| 8 | `add_issue_comment` | POST `/api/projects/{id}/issues/{num}/comments` | 201, comment body correct |
| 9 | `create_merge_request` | POST `/api/projects/{id}/merge-requests` | 201, has auto-incremented `number` |
| 10 | `list_merge_requests` | Create 2 MRs, GET list | 200, items present |
| 11 | `update_merge_request` | PATCH MR (change title) | 200, updated |
| 12 | `add_mr_comment` | POST comment on MR | 201, comment present |
| 13 | `add_mr_review` | POST review on MR | 201, review with `approved` status |
| 14 | `issue_requires_project_read` | User without read access tries GET issue | 404 (not 403, to hide existence) |
| 15 | `issue_write_requires_project_write` | User with read-only tries POST issue | 403 |

**Note**: MR merge (git worktree + `--no-ff`) requires actual git repos. This is deferred to Tier 3 (E2E tests). The merge endpoint test can verify the API validation without the actual git merge.

### 3e. `tests/admin_integration.rs` — 14 tests

| # | Test Name | What It Verifies | Key Assertions |
|---|-----------|-----------------|----------------|
| 1 | `admin_create_user` | POST `/api/admin/users` | 201, user created |
| 2 | `admin_create_user_duplicate_name` | Create user with existing name | 409 |
| 3 | `admin_create_user_invalid_email` | POST with bad email | 400 |
| 4 | `admin_list_users` | GET `/api/admin/users` | 200, includes admin + created users |
| 5 | `admin_get_user_by_id` | GET `/api/admin/users/{id}` | 200, correct user |
| 6 | `admin_update_user` | PATCH `/api/admin/users/{id}` | 200, updated fields |
| 7 | `admin_deactivate_user` | POST deactivate endpoint | 200, user is_active = false |
| 8 | `deactivated_user_cannot_login` | Deactivate user, try login | 401 |
| 9 | `deactivated_user_token_revoked` | Deactivate user, try using existing token | 401 |
| 10 | `non_admin_cannot_create_user` | Regular user tries POST `/api/admin/users` | 403 |
| 11 | `non_admin_cannot_list_users` | Regular user tries GET `/api/admin/users` | 403 |
| 12 | `admin_create_service_account` | POST service account (user_type=agent) | 201, user_type = "agent" |
| 13 | `admin_create_token_for_user` | POST `/api/admin/users/{id}/tokens` | 201, token for other user |
| 14 | `admin_actions_create_audit_log` | Create user, check audit_log | audit_log row exists |

### 3f. `tests/webhook_integration.rs` — 12 tests

| # | Test Name | What It Verifies | Key Assertions |
|---|-----------|-----------------|----------------|
| 1 | `create_webhook` | POST `/api/projects/{id}/webhooks` | 201, has id, URL, events |
| 2 | `create_webhook_invalid_url` | POST with `ftp://example.com` | 400 |
| 3 | `create_webhook_ssrf_blocked` | POST with `http://localhost/hook` | 400, SSRF error message |
| 4 | `create_webhook_ssrf_private_ip` | POST with `http://10.0.0.1/hook` | 400, private IP blocked |
| 5 | `create_webhook_ssrf_metadata` | POST with `http://169.254.169.254/` | 400, metadata blocked |
| 6 | `create_webhook_invalid_event` | POST with `events: ["nonexistent"]` | 400, invalid event |
| 7 | `list_webhooks` | Create 2 webhooks, GET list | 200, 2 items |
| 8 | `update_webhook` | PATCH webhook URL and events | 200, updated |
| 9 | `delete_webhook` | DELETE webhook | 200, then GET returns 404 |
| 10 | `webhook_requires_project_write` | User with read-only tries webhook CRUD | 403 |
| 11 | `webhook_secret_not_exposed` | Create webhook with secret, GET webhook | Response does NOT include raw secret |
| 12 | `create_webhook_max_events` | POST with 21 events | 400, max 20 events |

---

## 4. Implementation Approach

### Pattern: `#[sqlx::test]` with `oneshot`

Every test follows this structure:

```rust
#[sqlx::test(migrations = "migrations")]
async fn test_name(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Login / setup
    let token = helpers::admin_login(&app).await;

    // Exercise
    let (status, body) = helpers::post_json(
        &app, &token, "/api/endpoint",
        serde_json::json!({ "field": "value" }),
    ).await;

    // Verify
    assert_eq!(status, StatusCode::CREATED);
    assert!(body["id"].is_string());
}
```

### Required `pub` Visibility Changes

The following items need `pub` visibility from the library crate for integration tests to import them:

| Item | Current | Needed | Location |
|------|---------|--------|----------|
| `api::router()` | `pub` | `pub` | `src/api/mod.rs` — already public |
| `store::AppState` | `pub` | `pub` | `src/store/mod.rs` — already public |
| `store::bootstrap::run()` | `pub` | `pub` | `src/store/bootstrap.rs` — already public |
| `config::Config` | `pub` | `pub` | `src/config.rs` — already public |
| `config::Config::test_default()` | `pub` (cfg test) | `pub` (cfg test) | `src/config.rs` — already public under `#[cfg(test)]` |

**Problem**: `Config::test_default()` is behind `#[cfg(test)]`, which is only available for the library's own tests, not for integration tests in `tests/`.

**Fix**: Move `test_default()` to be behind `#[cfg(any(test, feature = "test-helpers"))]` or add a `test-helpers` feature flag. Alternatively, construct `Config` directly in the test helper (more verbose but no feature flag needed).

**Recommended**: Construct `Config` directly in `test_state()` since it's only one place. No library changes needed.

### Kube Client Workaround

`kube::Client::try_default()` requires a kubeconfig. For CI without K8s:

**Option A** (preferred): Create a fake kubeconfig pointing to a non-existent server. Tests that don't use K8s won't fail because `kube::Client` is lazy — it only connects when you make a request.

```rust
// In test_state():
let kube = kube::Client::try_default().await.unwrap_or_else(|_| {
    // Create a client with a dummy config
    let config = kube::Config {
        cluster_url: "https://127.0.0.1:1".parse().unwrap(),
        ..kube::Config::default()
    };
    kube::Client::try_from(config).expect("dummy kube client")
});
```

**Option B**: Make `kube` field `Option<kube::Client>` in `AppState` (more invasive, not recommended).

---

## 5. Prerequisites

Before implementing integration tests:

1. **Valkey must be running** on `localhost:6379` (or `VALKEY_URL` env var set)
2. **Postgres** must be available (sqlx::test creates temp DBs automatically via the `DATABASE_URL` env var)
3. **No K8s cluster required** for Tier 2 integration tests (dummy client suffices)

### CI Requirements

Integration tests need both Postgres and Valkey. Options:

1. **Local dev**: `just cluster-up` provides both via Kind port-forwards
2. **CI (GitHub Actions)**: Add Postgres and Redis/Valkey services to the workflow

```yaml
services:
  postgres:
    image: postgres:17
    env:
      POSTGRES_USER: postgres
      POSTGRES_PASSWORD: postgres
      POSTGRES_DB: postgres
    ports: ["5432:5432"]
  valkey:
    image: valkey/valkey:8
    ports: ["6379:6379"]
```

---

## 6. Implementation Sequence

| Step | Scope | Files | Est. Tests |
|------|-------|-------|-----------|
| **E1** | Test infrastructure | `tests/helpers/mod.rs`, `Justfile` | 0 (infra) |
| **E2** | Auth tests | `tests/auth_integration.rs` | 13 |
| **E3** | Admin tests | `tests/admin_integration.rs` | 14 |
| **E4** | Project tests | `tests/project_integration.rs` | 14 |
| **E5** | RBAC tests | `tests/rbac_integration.rs` | 15 |
| **E6** | Issue/MR tests | `tests/issue_mr_integration.rs` | 15 |
| **E7** | Webhook tests | `tests/webhook_integration.rs` | 12 |

**Dependency order**: E1 must come first. E2 and E3 should come before E4-E7 since many tests need user creation and role assignment patterns established. E4-E7 can be done in parallel.

### Per-step verification

After each step:
1. `cargo nextest run --test '*'` — all integration tests pass
2. `just lint` — no clippy warnings
3. `just fmt` — formatted

After all steps:
1. `just ci` — existing unit tests still pass
2. `just test-integration` — all 83 integration tests pass
3. Confirm Valkey is flushed between tests (no cross-test pollution)

---

## 7. Risks and Mitigations

| Risk | Impact | Mitigation |
|------|--------|-----------|
| Valkey not available in CI | Tests fail | Use `VALKEY_URL` env var; add valkey service to CI |
| Kube client panics | Tests crash | Dummy client that never connects; gate K8s tests |
| Slow integration tests | CI takes too long | nextest parallelism; each `#[sqlx::test]` is independent |
| Bootstrap changes break tests | All tests fail | Bootstrap is idempotent; tests rely on seed data |
| API route paths change | Many tests break | Use constants or helper functions for URL construction |
| Argon2 hashing is slow | ~200ms per login | Acceptable for integration tests; consider reducing work factor in test config |

---

## 8. Future Extensions (Tier 3)

These are **not** part of Phase E but are the natural next step:

- **Git operation tests**: Bare repo init, branch listing, push/pull (need temp git repos)
- **MR merge tests**: Git worktree + `--no-ff` merge (need real git repos with commits)
- **Pipeline trigger tests**: Parse `.platformci.yml`, create steps (need K8s or mock)
- **Webhook dispatch with HMAC verification**: Mock HTTP server (e.g., `wiremock`) to receive deliveries
- **WebSocket agent streaming tests**: Connect to agent WS endpoint, send/receive messages
- **Notification tests**: Email dispatch via mock SMTP (e.g., `mailhog`)
- **Secrets integration**: Encrypt/store/resolve with real DB + master key
- **Observability**: OTLP ingest, trace storage, query API

---

## Summary

| Metric | Value |
|--------|-------|
| New test files | 7 (6 test files + 1 helper module) |
| New integration tests | 83 |
| External dependencies | Postgres (sqlx::test), Valkey (real) |
| Estimated LOC | ~2,500-3,000 |
| Implementation steps | 7 (E1-E7) |
