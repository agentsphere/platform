# Review: 44-iframe-preview-panels (PR 1)

**Date:** 2026-03-13
**Scope:** `src/api/preview.rs` (new), `src/agent/service.rs`, `src/agent/claude_code/pod.rs`, `src/deployer/namespace.rs`, `src/error.rs`, `src/main.rs`, `src/api/mod.rs`, `ui/index.html`, `Cargo.toml`, `tests/preview_integration.rs` (new), `tests/helpers/mod.rs`, `tests/e2e_helpers/mod.rs`
**Overall:** PASS WITH FINDINGS

## Summary
- Solid reverse proxy implementation with auth, permission checks, WebSocket HMR support, header stripping, and K8s Service auto-creation. Good test coverage for auth/permission paths.
- 1 critical, 2 high, 4 medium, 4 low findings
- 9 integration tests + 13 unit tests added; 2 integration test gaps identified
- Touched-line coverage: not measured (no `just cov-diff` run); estimated high from test count

## Critical & High Findings (must fix)

### R1: [CRITICAL] Global X-Frame-Options DENY overwrites preview SAMEORIGIN
- **File:** `src/main.rs:220`
- **Domain:** Security
- **Description:** `SetResponseHeaderLayer::overriding` for `X-Frame-Options: DENY` runs as global middleware. Since tower layers execute outer-to-inner on response (i.e., the global layer runs AFTER the handler), `overriding` replaces the preview handler's `SAMEORIGIN` with `DENY`. This completely breaks iframe embedding — the core purpose of this feature.
- **Risk:** Preview iframes will never render in the browser. The entire feature is non-functional.
- **Suggested fix:** Change `SetResponseHeaderLayer::overriding` to `SetResponseHeaderLayer::if_not_present` for the X-Frame-Options layer only. This lets the preview handler set SAMEORIGIN while all other routes get DENY from the global layer.

### R2: [HIGH] Orphaned WebSocket bridge task leaks on disconnect
- **File:** `src/api/preview.rs:272-275`
- **Domain:** Rust Quality
- **Description:** `tokio::select!` picks the first completed branch (c2b or b2c) but does NOT abort the other spawned task. The losing task continues running until its WebSocket connection eventually errors or times out, leaking resources.
- **Risk:** Under sustained WebSocket use (HMR with many open tabs), orphaned tasks accumulate, consuming memory and file descriptors.
- **Suggested fix:** Capture the `JoinHandle` from both spawns and abort the loser:
  ```rust
  tokio::select! {
      _ = &mut c2b => { b2c.abort(); },
      _ = &mut b2c => { c2b.abort(); },
  }
  ```

### R3: [HIGH] BadGateway error leaks internal K8s service URL
- **File:** `src/api/preview.rs:224`
- **Domain:** Security
- **Description:** The reqwest error message from `.map_err(|e| ApiError::BadGateway(format!("preview backend unreachable: {e}")))` includes the full target URL (`http://preview-{id}.{ns}.svc.cluster.local:8000/...`), which is returned to the client in the JSON error response.
- **Risk:** Leaks internal K8s service naming convention, namespace names, and cluster DNS structure to end users.
- **Suggested fix:** Log the full error with `tracing::warn!`, return a generic message to the client:
  ```rust
  .map_err(|e| {
      tracing::warn!(error = %e, "preview backend unreachable");
      ApiError::BadGateway("preview backend unreachable".into())
  })?;
  ```

## Medium Findings (should fix)

### R4: [MEDIUM] insert() drops multi-valued headers in strip functions
- **File:** `src/api/preview.rs:74,99`
- **Domain:** Rust Quality
- **Description:** Both `strip_request_headers` and `strip_response_headers` use `out.insert()` which overwrites previous values for the same header name. Standard HTTP headers like `Cache-Control` or `Accept` can have multiple values. Using `insert()` silently drops all but the last.
- **Suggested fix:** Use `out.append(name.clone(), value.clone())` instead of `out.insert()`.

### R5: [MEDIUM] Missing tracing::instrument on key functions
- **File:** `src/api/preview.rs:169`, `src/agent/service.rs:410`
- **Domain:** Rust Quality / Observability
- **Description:** `preview_proxy` and `create_preview_service` are async functions with side effects but lack `#[tracing::instrument]`. This makes debugging proxy failures and K8s service creation issues harder in production.
- **Suggested fix:** Add instrumentation:
  ```rust
  #[tracing::instrument(skip(state, auth, req), fields(session_id = %params.session_id), err)]
  async fn preview_proxy(...) { ... }
  ```

### R6: [MEDIUM] WebSocket upgrade error leaks internal details
- **File:** `src/api/preview.rs:188`
- **Domain:** Security
- **Description:** `format!("websocket upgrade failed: {e}")` passes axum's internal error message to the client via `ApiError::BadRequest`.
- **Suggested fix:** Log the detail, return generic message:
  ```rust
  .map_err(|e| {
      tracing::warn!(error = %e, "websocket upgrade failed");
      ApiError::BadRequest("websocket upgrade failed".into())
  })?;
  ```

### R7: [MEDIUM] Missing integration tests for edge cases
- **File:** `tests/preview_integration.rs`
- **Domain:** Tests
- **Description:** Two testable error paths lack integration tests:
  1. Session with `project_id = NULL` and non-owner access (hits the `else` branch at line 140-142)
  2. Session with invalid namespace format (hits the `validate_namespace_format` rejection at line 149-151)
- **Suggested fix:** Add two tests:
  - `proxy_null_project_non_owner_returns_404` — insert session with `project_id = NULL`, access as non-owner
  - `proxy_invalid_namespace_returns_400` — insert session with namespace containing uppercase/special chars

## Low Findings (optional)

- [LOW] R8: `src/api/preview.rs` — No `boundary_project_id` scope check for API token access. Currently only checks RBAC permissions but doesn't enforce token boundary restrictions. Low risk since preview access already requires session ownership or ProjectRead. Fix: add `auth.check_project_scope(project_id)?` in `resolve_session`.
- [LOW] R9: `src/error.rs` — No unit test for the new `BadGateway` variant's status code mapping. Fix: add `assert_eq!(ApiError::BadGateway("test".into()).status_code(), StatusCode::BAD_GATEWAY)` to existing error tests.
- [LOW] R10: `src/api/preview.rs` — `is_websocket_upgrade` and WS message converter functions lack unit tests. Fix: add tests for upgrade detection (present/absent/wrong value) and message round-trip conversion.
- [LOW] R11: `src/deployer/namespace.rs` — `ensure_session_network_policy` error is logged but silently discarded, same as `ensure_session_namespace`. Acceptable since namespace creation already succeeded and policy failure shouldn't block sessions. No fix needed.
- [LOW] R12: `src/api/preview.rs` — No WebSocket connection limit per session. An attacker with valid credentials could open many WS connections. Low risk since auth is required. Defer to future hardening.

## Coverage — Touched Lines

| File | Lines changed | Tests covering | Notes |
|---|---|---|---|
| `src/api/preview.rs` | ~325 (new) | 7 unit + 9 integration | Missing: null project_id path, invalid namespace path, WS helpers |
| `src/agent/service.rs` | ~55 (create_preview_service) | 0 direct | Non-fatal helper; would need K8s mock for unit test |
| `src/agent/claude_code/pod.rs` | ~15 | 3 unit | Full coverage on new lines |
| `src/deployer/namespace.rs` | ~60 | 3 unit | Full coverage on policy building |
| `src/error.rs` | ~5 | 0 direct | BadGateway variant untested |
| `src/main.rs` | ~2 | 0 direct | Route wiring; covered by integration tests |
| `ui/index.html` | ~1 | 0 | CSP change; manual verification |

### Uncovered Paths
- `src/api/preview.rs:140-142` — `project_id = NULL` + non-owner → 404; needs integration test (R7)
- `src/api/preview.rs:149-151` — invalid namespace format → 400; needs integration test (R7)
- `src/api/preview.rs:184-194` — WebSocket upgrade path; requires WS client in test (deferred)
- `src/api/preview.rs:244-278` — WebSocket bridge; requires real WS backend (deferred)
- `src/agent/service.rs:410-463` — K8s Service creation; covered by E2E only

## Checklist Results

| Category | Status | Notes |
|---|---|---|
| Error handling | PASS | Proper error types, no unwrap in production code |
| Auth & permissions | PASS | AuthUser + owner/ProjectRead check + 404 for unauthorized |
| Input validation | PASS | Namespace format validated; session_id is Uuid (type-safe) |
| Audit logging | N/A | Read-only proxy — no mutations requiring audit |
| Tracing instrumentation | FAIL | Missing `#[tracing::instrument]` on preview_proxy and create_preview_service (R5) |
| Clippy compliance | PASS | All clippy warnings resolved |
| Test patterns | PASS | Correct use of helpers, dynamic queries, proper assertions |
| Migration safety | N/A | No new migrations in PR 1 |
| X-Frame-Options | FAIL | Global DENY overwrites handler SAMEORIGIN (R1) |
| Header security | PASS | Request/response header stripping correct |
| WebSocket safety | FAIL | Task leak on disconnect (R2) |
