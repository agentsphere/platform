# Review: Plan 34 Phase 2 — Per-Project Namespaces + Network Isolation

**Date:** 2026-02-27
**Scope:** 7 source files, 2 migrations, 4 test files, 10 .sqlx cache files, 1 UI type
**Overall:** PASS WITH FINDINGS

## Summary
- Solid implementation of per-project K8s namespace isolation with good test coverage of the pure functions (slugify, network policy builders). The 5→2 tool simplification in the in-process agent is clean.
- **Findings:** 1 critical, 3 high, 7 medium
- **Tests:** 14 new unit tests in namespace.rs, existing tests updated. Integration tests all pass (728/728). E2E tests pass (43/43 non-SSH).
- **Touched-line coverage (unit only):** 48% — expected since most changes are in async handlers requiring integration tests. `config.rs` and `applier.rs` at 100%; `namespace.rs` pure functions at 76.9%.

## Critical & High Findings (must fix)

### R1: [CRITICAL] Migration SQL backfill produces wrong slugs for mixed-case project names
- **File:** `migrations/20260227010001_project_namespace.up.sql:7-10`
- **Domain:** Database
- **Description:** The inner `regexp_replace(name, '[^a-z0-9-]', '-', 'g')` runs on the original mixed-case name. Uppercase letters A-Z match `[^a-z0-9-]` and are replaced with hyphens rather than being lowercased. Example: project "MyProject" → SQL: "y-roject", Rust: "myproject".
- **Risk:** Existing projects get incorrect namespace_slug values that don't match what the Rust `slugify_namespace()` would produce. K8s namespace names won't align with DB values.
- **Suggested fix:** Apply `lower()` to `name` first:
  ```sql
  UPDATE projects SET namespace_slug = regexp_replace(
      regexp_replace(lower(name), '[^a-z0-9]', '-', 'g'),
      '-{2,}', '-', 'g'
  );
  ```

### R2: [HIGH] Migration backfill does not truncate to 40 chars
- **File:** `migrations/20260227010001_project_namespace.up.sql:7-12`
- **Domain:** Database
- **Description:** The Rust `slugify_namespace()` truncates output to 40 chars (leaving room for `-dev`/`-prod` suffix). The SQL backfill has no truncation. Projects with long names get slugs exceeding 40 chars, which could exceed the 63-char K8s DNS label limit when `-dev` is appended.
- **Risk:** K8s namespace creation could fail for long-named existing projects.
- **Suggested fix:** Add after the trim step:
  ```sql
  UPDATE projects SET namespace_slug = left(namespace_slug, 40);
  UPDATE projects SET namespace_slug = rtrim(namespace_slug, '-');
  ```

### R3: [HIGH] In-process agent bypasses namespace_slug collision retry
- **File:** `src/agent/inprocess.rs:433-452`
- **Domain:** Rust Quality / Security
- **Description:** `execute_create_project` does a raw INSERT with namespace_slug but lacks the collision-retry logic in `insert_project_row()` (API handler). If two project names produce the same slug (e.g., "my_project" and "my-project"), the INSERT fails with a generic error instead of retrying with a hash suffix.
- **Risk:** In-process agent project creation fails silently on namespace slug collision.
- **Suggested fix:** Call `crate::api::projects::insert_project_row()` from the in-process path, or refactor project creation into a shared function.

### R4: [HIGH] In-process agent missing display_name/description validation
- **File:** `src/agent/inprocess.rs:400-407`
- **Domain:** Security
- **Description:** `execute_create_project` calls `check_name()` on the name but passes display_name and description from the LLM tool call directly to the DB without length validation. The API handler validates these at `src/api/projects.rs:329-346`.
- **Risk:** A confused LLM could pass oversized strings (up to DB limit).
- **Suggested fix:** Add:
  ```rust
  if let Some(ref dn) = display_name { validation::check_length("display_name", dn, 1, 255)?; }
  if let Some(ref d) = description { validation::check_length("description", d, 0, 10_000)?; }
  ```

## Medium Findings (should fix)

### R5: [MEDIUM] NetworkPolicy only covers agent pods, not pipeline pods in shared namespace
- **File:** `src/deployer/namespace.rs:68-130`
- **Domain:** Security
- **Description:** The NetworkPolicy `podSelector` matches `"platform.io/component": "agent-session"`. Pipeline pods use different labels (`"platform.io/pipeline"`, `"platform.io/step"`) and are now co-located in the same `{slug}-dev` namespace. Pipeline pods have unrestricted cluster-internal network access.
- **Suggested fix:** Add a second NetworkPolicy for pipeline pods, or use an empty `podSelector {}` to apply to all pods in the namespace. Can defer to a follow-up if pipeline network policy design needs more thought.

### R6: [MEDIUM] resolve_session_namespace uses hardcoded fallback instead of config
- **File:** `src/agent/service.rs:439`
- **Domain:** Rust Quality
- **Description:** The fallback `Ok("platform-agents".into())` is hardcoded rather than using `state.config.agent_namespace`. The reaper at line 337-339 correctly uses `state.config.agent_namespace.clone()`, creating an inconsistency.
- **Suggested fix:** Either pass `&AppState` to the function and use `state.config.agent_namespace`, or pass the fallback namespace as a parameter.

### R7: [MEDIUM] ops_repos INSERT uses ON CONFLICT DO NOTHING without target
- **File:** `src/api/projects.rs:253`
- **Domain:** Database
- **Description:** `ON CONFLICT DO NOTHING` without specifying a conflict target silently swallows violations on ANY unique constraint (name OR project_id). A name collision with a different project would be hidden.
- **Suggested fix:** Change to `ON CONFLICT (project_id) DO NOTHING`.

### R8: [MEDIUM] setup_project_infrastructure mixed error semantics
- **File:** `src/api/projects.rs:191-267`
- **Domain:** Rust Quality
- **Description:** K8s namespace/policy steps are best-effort (log and continue), but ops repo init returns `Err` on failure. The caller catches this at line 446-449, but the function's return type `Result<(), ApiError>` doesn't communicate which steps are best-effort. The function also doesn't create a NetworkPolicy for `-prod` namespace.
- **Suggested fix:** Make ops repo init also best-effort (just log), or split into separate functions. Document that `-prod` NetworkPolicy is intentionally omitted (agents only run in `-dev`).

### R9: [MEDIUM] Stale test exercising removed tool
- **File:** `tests/inprocess_integration.rs:818`
- **Domain:** Tests
- **Description:** `inprocess_create_ops_repo_tool` references the removed `create_ops_repo` tool. It accidentally passes because `create_project` now auto-creates the ops repo via `setup_project_infrastructure`, and the unknown tool error is gracefully handled. The test name, comments, and structure are misleading.
- **Suggested fix:** Rename to `inprocess_create_project_auto_creates_ops_repo` and remove the second turn that calls the nonexistent tool.

### R10: [MEDIUM] No integration test for namespace_slug collision retry
- **File:** `src/api/projects.rs:76-118`
- **Domain:** Tests
- **Description:** The `insert_project_row` collision-retry logic (SHA256 hash suffix fallback) has zero test coverage at any tier. This is a critical code path.
- **Suggested fix:** Add integration test: create two projects with names that produce the same slug (e.g., "my_project" and "my-project"), verify both succeed with different namespace_slug values.

### R11: [MEDIUM] No integration test for namespace_slug in API responses
- **File:** `tests/project_integration.rs`
- **Domain:** Tests
- **Description:** No integration test asserts that `ProjectResponse` includes the `namespace_slug` field. Existing `create_project` tests only check `id`, `name`, `visibility`.
- **Suggested fix:** Add assertion `assert!(body["namespace_slug"].is_string())` to an existing create_project test.

## Low Findings (optional)

- [LOW] R12: `src/deployer/namespace.rs:133,163` — `ensure_namespace` and `ensure_network_policy` lack `#[tracing::instrument]` attributes → Add instrument with `skip(kube_client)`.
- [LOW] R13: `src/api/projects.rs:76` — `insert_project_row` lacks `#[tracing::instrument]` → Add instrument with `skip(pool, body)`.
- [LOW] R14: `src/config.rs` — `PLATFORM_NAMESPACE` env var not documented in CLAUDE.md security env vars table → Add row to table.
- [LOW] R15: `src/agent/service.rs:430` — `resolve_session_namespace` filters `AND is_active = true`, preventing operations on sessions whose projects were soft-deleted → Remove the filter (reaper correctly uses LEFT JOIN without it).
- [LOW] R16: `src/deployer/namespace.rs:10` — `slugify_namespace` is `pub` but only used within the crate → Consider `pub(crate)`.

## Coverage — Touched Lines (unit tests only)

| File | Lines changed | Lines covered | Coverage % | Uncovered lines |
|---|---|---|---|---|
| `src/agent/inprocess.rs` | 15 | 6 | 40% | 422,434-435,444,462-466,487 |
| `src/agent/service.rs` | 22 | 0 | 0% | 322-326,337-341,424-441 |
| `src/api/projects.rs` | 95 | 0 | 0% | 76-118,138-189,194-198,243-285,501,529,542,580 |
| `src/config.rs` | 3 | 3 | 100% | — |
| `src/deployer/applier.rs` | 1 | 1 | 100% | — |
| `src/deployer/namespace.rs` | 234 | 180 | 76.9% | 133-194 (async K8s fns) |
| `src/pipeline/executor.rs` | 11 | 0 | 0% | 231,513,578-583,1063-1064 |
| **Total** | **381** | **186** | **48%** | — |

### Notes on uncovered paths
- `src/agent/service.rs`, `src/api/projects.rs`, `src/pipeline/executor.rs` — all async handler/service code covered by integration tests (728/728 pass) but not captured in unit coverage
- `src/deployer/namespace.rs:133-194` — async K8s functions require real cluster; covered by E2E tests
- Unit coverage 48% is expected for a change dominated by async DB/K8s handlers

## Checklist Results

| Category | Status | Notes |
|---|---|---|
| Error handling | PASS | Proper `?` propagation, thiserror for module boundaries |
| Auth & permissions | PASS | create_project checks ProjectWrite, setup_project_infrastructure gated |
| Input validation | WARN | API handler validates; in-process path missing display_name/description checks (R4) |
| Audit logging | PASS | Both API and in-process paths audit project creation |
| Tracing instrumentation | WARN | New async fns missing instrument attributes (R12, R13) |
| Clippy compliance | PASS | All warnings resolved, allow attribute on too_many_arguments |
| Test patterns | WARN | Stale test for removed tool (R9), missing collision test (R10) |
| Migration safety | FAIL | SQL backfill mismatch (R1), missing truncation (R2) |
| Touched-line coverage | WARN | 48% unit-only; integration tests cover the async paths |
