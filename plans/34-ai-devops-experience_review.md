# Review: Plan 34 Phase 4 — Dev Images + Secrets

**Date:** 2026-02-28
**Scope:** 19 files changed (+1025 lines, -31 lines) across agent, api, deployer, pipeline, secrets, store modules
**Overall:** PASS WITH FINDINGS

## Summary
- Solid implementation of dev image auto-detection, scoped secrets injection (agent + deploy), and agent secret request flow
- 0 critical, 4 high findings: env var override risk, memory leak, missing auth tests
- 8 new integration tests + 6 new unit tests added; several auth/edge-case test gaps identified
- Touched-line coverage: 27% from unit tests only; integration tests cover api/secrets and secrets/engine but full diff-cover not run

## Critical & High Findings (must fix)

### R1: [HIGH] Env var override allows privilege escalation
- **File:** `src/agent/claude_code/pod.rs:202-204`
- **Domain:** Security
- **Description:** The `extra_env_vars` loop appends project secrets AFTER standard env vars (`PLATFORM_API_TOKEN`, `PLATFORM_API_URL`, `SESSION_ID`, etc.). K8s resolves duplicate env var names using the LAST definition. A project secret named `PLATFORM_API_TOKEN` would override the agent's scoped token, potentially escalating privileges. Similarly, overriding `PLATFORM_API_URL` could redirect agent API calls to a malicious server.
- **Risk:** A user with `SecretWrite` on a project could create a secret named `PLATFORM_API_TOKEN` and hijack the agent's identity.
- **Suggested fix:** Define a blocklist of reserved env var names and filter them out before appending:
  ```rust
  const RESERVED_ENV_VARS: &[&str] = &[
      "PLATFORM_API_TOKEN", "PLATFORM_API_URL", "SESSION_ID",
      "ANTHROPIC_API_KEY", "BRANCH", "AGENT_ROLE", "PROJECT_ID",
      "GIT_AUTH_TOKEN", "GIT_BRANCH", "BROWSER_ENABLED",
      "BROWSER_CDP_URL", "BROWSER_ALLOWED_ORIGINS",
  ];
  for (name, value) in params.extra_env_vars {
      if RESERVED_ENV_VARS.contains(&name.as_str()) {
          tracing::warn!(%name, "skipping reserved env var from project secrets");
          continue;
      }
      vars.push(env_var(name, value));
  }
  ```

### R2: [HIGH] No cleanup of in-memory secret requests (memory leak)
- **File:** `src/api/secrets.rs` + `src/secrets/request.rs`
- **Domain:** Rust Quality / Security
- **Description:** The `SecretRequests` HashMap grows unboundedly. Timed-out and completed requests are never evicted from the in-memory map. While `effective_status()` correctly computes timeout, stale entries persist forever. Over time in production, this causes monotonically increasing memory consumption.
- **Risk:** Memory leak in production. An attacker with write access could accelerate growth by creating many sessions with secret requests.
- **Suggested fix:** Add periodic cleanup (e.g., every 10 minutes) that removes entries older than `2 * TIMEOUT_SECS`:
  ```rust
  map.retain(|_, r| r.created_at.elapsed() < Duration::from_secs(600));
  ```
  Wire into the existing session cleanup loop in `main.rs` or spawn a dedicated task.

### R3: [HIGH] Missing 401 tests for all secret-request endpoints
- **File:** `tests/secrets_integration.rs`
- **Domain:** Tests
- **Description:** All three new secret-request handlers are protected by `AuthUser` + `require_secret_write/read`, but no test verifies unauthenticated access is rejected. Every new handler should have at least one 401 test.
- **Suggested fix:** Add tests:
  - `fn secret_request_no_token_returns_401` — POST without token
  - `fn get_secret_request_no_token_returns_401` — GET without token
  - `fn complete_secret_request_no_token_returns_401` — POST complete without token

### R4: [HIGH] Missing 404 tests for nonexistent secret requests
- **File:** `tests/secrets_integration.rs`
- **Domain:** Tests
- **Description:** `get_secret_request` and `complete_secret_request` both return `ApiError::NotFound` for nonexistent request IDs, but no tests exercise these paths.
- **Suggested fix:** Add tests:
  - `fn get_nonexistent_secret_request_returns_404`
  - `fn complete_nonexistent_secret_request_returns_404`

## Medium Findings (should fix)

### R5: [MEDIUM] Missing `#[tracing::instrument]` on 3 new async functions
- **File:** `src/store/eventbus.rs:499`, `src/deployer/reconciler.rs:351`, `src/pipeline/executor.rs:968`
- **Domain:** Rust Quality
- **Description:** `handle_dev_image_built`, `inject_project_secrets`, and `detect_and_publish_dev_image` are async functions with DB/K8s side effects that lack instrumentation.
- **Suggested fix:** Add `#[tracing::instrument(skip(state), fields(...))]` to each.

### R6: [MEDIUM] Missing audit log entries for secret request lifecycle
- **File:** `src/api/secrets.rs:320-495`
- **Domain:** Security
- **Description:** `create_secret_request` and `complete_secret_request` are mutations that don't write audit log entries. `complete_secret_request` creates real DB secrets — a high-value mutation. All other mutation handlers in the file properly write audit entries.
- **Suggested fix:** Add `write_audit()` calls:
  - `create_secret_request`: `action: "secret_request.create"`, include name, session_id
  - `complete_secret_request`: `action: "secret_request.complete"`, include name (NOT value)

### R7: [MEDIUM] Missing `get_secret_request` tracing instrument
- **File:** `src/api/secrets.rs:390`
- **Domain:** Rust Quality
- **Description:** `get_secret_request` handler is missing `#[tracing::instrument]`. Other handlers in the file are instrumented.
- **Suggested fix:** Add `#[tracing::instrument(skip(state), fields(%id, %request_id), err)]`.

### R8: [MEDIUM] Non-atomic multi-environment secret creation
- **File:** `src/api/secrets.rs:472-488`
- **Domain:** Database
- **Description:** `complete_secret_request` stores the same secret value for each environment in a loop without a transaction wrapper. If the 3rd `create_secret` fails, the first 2 are already committed, leaving partial state.
- **Suggested fix:** Wrap the loop in a transaction for atomicity.

### R9: [MEDIUM] `DevImageBuilt` missing from `all_event_types_have_correct_tag` test
- **File:** `src/store/eventbus.rs` (~line 614)
- **Domain:** Tests
- **Description:** The `all_event_types_have_correct_tag` test doesn't include the new `DevImageBuilt` variant (though the serialization roundtrip test does). If DevImageBuilt's serde tag were wrong, it wouldn't be caught.
- **Suggested fix:** Add `(PlatformEvent::DevImageBuilt { ... }, "DevImageBuilt")` to the cases vec.

### R10: [MEDIUM] Missing test for completing already-completed request
- **File:** `tests/secrets_integration.rs`
- **Domain:** Tests
- **Description:** `complete_secret_request` checks `req.status != Pending` and returns BadRequest, but this path is never tested.
- **Suggested fix:** Add `fn complete_already_completed_request_returns_400`.

### R11: [MEDIUM] Missing test for unauthorized user accessing secret-request endpoints
- **File:** `tests/secrets_integration.rs`
- **Domain:** Tests
- **Description:** No test verifies that a user without `SecretWrite` permission is rejected when accessing secret-request endpoints.
- **Suggested fix:** Add `fn non_authorized_user_cannot_create_secret_request`.

### R12: [MEDIUM] Missing validation edge-case tests
- **File:** `tests/secrets_integration.rs`
- **Domain:** Tests
- **Description:** Missing tests for: description >500 chars, >5 environments, empty value, value >65536 chars.
- **Suggested fix:** Add 4 boundary tests.

### R13: [MEDIUM] Missing integration test for `handle_dev_image_built`
- **File:** `src/store/eventbus.rs:499-526`
- **Domain:** Tests
- **Description:** No test verifies that the project's `agent_image` column is updated when a `DevImageBuilt` event is received.
- **Suggested fix:** Add an integration test that creates a project, fires a `DevImageBuilt` event, and verifies `agent_image` was set.

### R14: [MEDIUM] Inconsistent derive import style in secrets.rs
- **File:** `src/api/secrets.rs:44-53`
- **Domain:** Rust Quality
- **Description:** `CompleteSecretRequestBody` and `SecretRequestResponse` use `serde::Deserialize`/`serde::Serialize` via full path while other structs use the imported form.
- **Suggested fix:** Use `Deserialize`/`Serialize` from the import at the top of the file.

## Low Findings (optional)

- [LOW] R15: `src/agent/service.rs:38` — `create_session` has `#[allow(clippy::too_many_arguments, clippy::too_many_lines)]`. Consider a `CreateSessionParams` struct.
- [LOW] R16: `src/pipeline/executor.rs:104-120` — Pipeline execution JOIN missing `AND p.is_active = true` filter. Edge case: pipeline runs against soft-deleted project.
- [LOW] R17: `src/secrets/request.rs` — No boundary test for exactly 300s timeout (only 301s tested).
- [LOW] R18: `tests/secrets_integration.rs` — No cross-project isolation test for secret requests.
- [LOW] R19: `src/store/eventbus.rs:499-526` — `handle_dev_image_built` has no audit log entry for updating `agent_image`.

## Coverage — Touched Lines

Unit-only coverage (integration coverage expected to be significantly higher but not measured in this review):

| File | Lines changed | Lines covered | Coverage % | Uncovered lines |
|---|---|---|---|---|
| `src/agent/claude_code/pod.rs` | 56 | 56 | 100% | — |
| `src/store/eventbus.rs` | 25 | 5 | 20% | 179-182, 499-526 |
| `src/agent/claude_code/adapter.rs` | 1 | 0 | 0% | 23 |
| `src/agent/service.rs` | 15 | 0 | 0% | 564-586 |
| `src/api/secrets.rs` | 91 | 0 | 0% | 168-495 |
| `src/deployer/reconciler.rs` | 40 | 0 | 0% | 205, 351-422 |
| `src/pipeline/executor.rs` | 30 | 0 | 0% | 876, 968-1016 |
| `src/pipeline/trigger.rs` | 25 | 0 | 0% | 172, 211-258 |
| `src/secrets/engine.rs` | 10 | 0 | 0% | 480-519 |
| **Total** | **293** | **82** | **27%** | — |

### Uncovered Paths
- `src/api/secrets.rs:321-495` — All 3 secret-request handlers. Covered by integration tests (not in unit lcov).
- `src/secrets/engine.rs:480-519` — `query_scoped_secrets`. Covered by integration tests.
- `src/deployer/reconciler.rs:351-422` — `inject_project_secrets`. Requires K8s; covered by E2E.
- `src/pipeline/executor.rs:968-1016` — `detect_and_publish_dev_image`. Requires DB; covered by E2E.
- `src/pipeline/trigger.rs:211-258` — `insert_dev_image_step`, `has_dockerfile_dev`. Requires git repo; covered by E2E.
- `src/store/eventbus.rs:499-526` — `handle_dev_image_built`. Requires DB; needs integration test (see R13).
- `src/agent/service.rs:564-586` — `resolve_agent_secrets`. Requires DB + secrets engine; covered by E2E.

**Note:** Full diff-cover (unit + integration + E2E) not run during review. Finalize should run `just cov-diff` to get combined coverage.

## Checklist Results

| Category | Status | Notes |
|---|---|---|
| Error handling | PASS | Appropriate error handling, no unwrap in production |
| Auth & permissions | PASS | All handlers have AuthUser + permission checks |
| Input validation | PASS | Name, description, environments, value all validated |
| Audit logging | FAIL | Secret request create/complete missing audit entries (R6) |
| Tracing instrumentation | FAIL | 4 async functions missing `#[tracing::instrument]` (R5, R7) |
| Clippy compliance | PASS | 2 justified `#[allow]` attributes |
| Test patterns | PASS | Correct patterns used in all tests |
| Test coverage gaps | FAIL | Missing 401, 404, auth failure, edge-case tests (R3, R4, R10-R12) |
| Migration safety | N/A | No new migrations in Phase 4 |
| Query correctness | PASS | All queries compile-time checked, soft-delete respected |
| Security | FAIL | Env var override risk (R1), memory leak (R2) |
| Touched-line coverage | FAIL | 27% unit-only; integration/E2E expected to cover most gaps |
