# Plan 12: Unit Test Gap Remediation

## Context

The codebase has 39 `.rs` files but only 9 have tests (~49 total tests). Existing tests cover parsers, crypto, and git protocol helpers well, but security-critical auth extraction, error mapping, RBAC path parsing, and pipeline utilities have zero coverage. This plan adds **~57 pure unit tests** (no DB/Valkey/K8s needed) to more than double test coverage.

## Current State

**Tested (good):** `validation.rs` (9), `auth/password.rs` (3), `auth/token.rs` (4), `pipeline/definition.rs` (14), `rbac/types.rs` (4), `git/hooks.rs` (6), `git/smart_http.rs` (8), `git/browser.rs` (9), `git/repo.rs` (2)

**Untested (this plan):** `auth/middleware.rs`, `error.rs`, `rbac/middleware.rs`, `pipeline/error.rs`, `pipeline/executor.rs`, `rbac/resolver.rs`, `config.rs`

## Changes

### 1. `src/auth/middleware.rs` — 14 tests (security-critical)

Test `extract_bearer_token`, `extract_session_cookie`, `extract_ip` (all private, accessible from `#[cfg(test)]`):

- **Bearer token (6):** valid extraction, missing header, wrong scheme (Basic), empty after prefix, preserves full value, case-sensitive "Bearer " prefix
- **Session cookie (5):** valid, among multiple cookies, missing session key, empty value, no Cookie header
- **IP extraction (3):** X-Forwarded-For when trusted (first IP), ignored when untrusted, ConnectInfo fallback

Helper: `make_parts(headers)` builds `axum::http::request::Parts` from header pairs.

### 2. `src/error.rs` — 12 tests (API contract)

Test `ApiError::into_response()` and `From<sqlx::Error>`:

- **Status codes (9):** NotFound→404, Unauthorized→401, Forbidden→403, BadRequest→400, Conflict→409, TooManyRequests→429, Validation→422, ServiceUnavailable→503, Internal→500
- **JSON body (3):** NotFound body has message, Validation body has `fields` array, Internal hides details (shows "internal server error")

Uses `#[tokio::test]` + `axum::body::to_bytes` for body assertions.

### 3. `src/rbac/middleware.rs` — 6 tests

Test `extract_project_id_from_path`:

- `/api/projects/{uuid}/issues` → Some(uuid)
- `/projects/{uuid}` → Some(uuid)
- `/api/users/123` → None
- `/api/projects/not-a-uuid` → None
- Trailing slash handling
- Nested path doesn't confuse extraction

### 4. `src/pipeline/error.rs` — 5 tests

Test `From<PipelineError> for ApiError`:

- InvalidDefinition → BadRequest
- NotFound → NotFound("pipeline")
- StepFailed → Internal
- Other → Internal
- Display format includes step name + exit code

### 5. `src/pipeline/executor.rs` — 12 tests

Test `slug`, `extract_exit_code`, `build_pod_spec`:

- **slug (6):** simple, uppercase, special chars, leading/trailing dashes, empty, all-special
- **extract_exit_code (5):** terminated container, no container_statuses, empty statuses, no terminated state, no state
- **build_pod_spec (1):** labels, restart policy, init container image, main container image/commands, resource limits, volumes

### 6. `src/rbac/resolver.rs` — 3 tests

Test `cache_key`:

- With project_id → `"perms:{user}:{project}"`
- Without project_id → `"perms:{user}:global"`
- Deterministic output

### 7. `src/config.rs` — 5 tests

Test `Config::load()` defaults only (env var setting is `unsafe` in edition 2024 + `unsafe_code = "forbid"`):

- Default listen address `0.0.0.0:8080`
- Default SMTP port `587`
- Default git repos path `/data/repos`
- Default secure_cookies `false`
- Default cors_origins empty

## Implementation Order

1. `auth/middleware.rs` (14 tests) — highest security impact
2. `error.rs` (12 tests) — API contract correctness
3. `rbac/middleware.rs` (6 tests) — security path parsing
4. `pipeline/error.rs` (5 tests) — domain error mapping
5. `pipeline/executor.rs` (12 tests) — pipeline utilities
6. `rbac/resolver.rs` (3 tests) — cache key format
7. `config.rs` (5 tests) — configuration defaults

## Not In Scope (Future Work)

- **Integration tests** (need real DB): `rbac/resolver` permission resolution, `rbac/delegation` CRUD, API handler flows, auth session/token lookup
- **Valkey-dependent tests:** `auth/rate_limit`, cache hit/miss behavior
- **K8s-dependent tests:** pipeline executor E2E, pod lifecycle
- **API handler tests:** all `src/api/*.rs` handlers (CRUD, webhooks, merge logic)

## Verification

```bash
just test-unit   # all 49 existing + ~57 new tests pass
just lint        # no clippy warnings from new test code
```
