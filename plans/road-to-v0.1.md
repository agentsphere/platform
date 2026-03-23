# Road to v0.1-alpha

First public release of the platform.

Plans 30–48 are substantially implemented. This file tracks the remaining gaps found by cross-referencing each plan against the actual codebase (verified 2026-03-19).

## Steps

### 1. Squash migrations into single initial schema

**Status:** TODO

No users in production — consolidate all 63 migration pairs (126 files) into a single `initial_schema` migration. This establishes a clean baseline for the v0.1 release; all future migrations become real upgrade paths.

**Tasks:**
1. `pg_dump --schema-only` from a fully-migrated DB to capture the final schema
2. Clean up the dump (remove pg_dump comments, `public.` prefixes if desired, reorder for readability)
3. Delete all 126 migration files
4. Create single pair: `20260319000001_initial.up.sql` / `20260319000001_initial.down.sql`
5. Down migration: `DROP SCHEMA public CASCADE; CREATE SCHEMA public;`
6. Run `just db-prepare` to regenerate `.sqlx/` offline cache
7. Verify: `just cluster-up` (fresh DB) + `just test-unit` + `just test-integration` + `just test-e2e`

### 2. Justfile restructure + cluster scripts

**Status:** IN PROGRESS

Keep Kind for local dev (macOS, Windows WSL) and CI tests. Add k0s prod setup script separately. Restructure justfile with modules and clear dev/run semantics.

**Tasks:**
1. Finalize cluster scripts (`hack/cluster-up.sh`, `hack/cluster-down.sh`, `hack/cluster-info.sh`) — Kind-based for local/CI
2. Add k0s prod setup script (`hack/k0s-setup.sh`) — separate from local dev flow
3. Rename dev-env.sh → dev-up.sh, update for cluster-info.sh
4. Update test-in-cluster.sh for cluster-info.sh
5. Create just modules: `cli.just`, `ui.just`, `mcp.just`
6. Restructure justfile: modules, groups, dev/run semantics, test arg passthrough
7. Update all docs: README.md, CLAUDE.md, docs/testing.md, docs/fe-be-testing.md, docs/architecture.md
8. Update .claude/commands/ skill files for new recipe names
9. Verify: `just cluster-up` + `just dev-up` + `just dev` + `just ci-full`

### 3. Code cleanup (Plan 30)

**Status:** TODO

Architectural audit items that were never addressed. Not blocking but should be done before v0.1.

**Tasks:**
1. Remove ~72 stale `#[allow(dead_code)]` suppressions (most modules now fully used)
2. Remove unused `arrow = { features = ["json"] }` from `Cargo.toml` (zero usages found)
3. Move `slug()` / `slugify_branch()` from `src/pipeline/mod.rs` to a shared `src/util.rs` (used by pipeline + deployer)
4. Move `check_browser_config()` from `src/validation.rs` to `src/agent/provider.rs` (infra→domain layer violation)
5. Consolidate duplicated test helpers: `admin_login()`, `create_user()`, `create_project()`, `assign_role()`, `get_json/post_json/...` exist in both `tests/helpers/mod.rs` and `tests/e2e_helpers/mod.rs` (~300 lines duplicated) → extract shared `tests/common/mod.rs`
6. Move 15 git-spawning `#[tokio::test]` tests from `src/deployer/ops_repo.rs` to `tests/` (they spawn subprocesses, belong in integration tier)

### 4. Force-push enforcement (Plan 03 gap)

**Status:** TODO

Branch protection rules are defined (`src/api/branch_protection.rs`, `block_force_push` field exists) but enforcement in `git-receive-pack` (SSH push path) is not implemented.

**Tasks:**
1. In `src/git/ssh_server.rs` post-receive hook path: check branch protection rules before accepting push
2. Reject force-pushes to protected branches (compare old/new SHAs, detect non-fast-forward)
3. Unit test: force-push to protected branch → rejected
4. Integration test: SSH push with force flag → 403

### 5. Test tier migration (Plan 43 follow-up)

**Status:** TODO (low priority)

Plan 43 redefined integration vs E2E boundaries. ~50 E2E tests are actually single-endpoint tests that should be integration tests per the new definitions.

**Candidates:**
- `e2e_pipeline.rs`: ~10 tests (single endpoint + executor)
- `e2e_webhook.rs`: ~6 tests (single endpoint + async delivery)
- `e2e_agent.rs`: ~8 tests (single endpoint + pod lifecycle)
- `e2e_deployer.rs`: ~10 of 17 (single endpoint + reconciler)
- `e2e_git.rs`: ~8 tests (single endpoint + filesystem)

**Tasks:**
1. Move single-endpoint tests from `tests/e2e_*.rs` to `tests/*_integration.rs`
2. Use `helpers::test_state()` instead of `e2e_helpers::e2e_state()`
3. Remove `#[ignore]` attributes (integration tests run by default)
4. Verify all moved tests still pass with `just test-integration`

### 6. OTEL injection test coverage (Plan 34 R7)

**Status:** TODO (low priority)

`inject_otel_env_vars()` in `src/deployer/reconciler.rs` has zero test coverage. Code is correct but untested.

**Tasks:**
1. Unit test: verify OTEL env vars are injected into deployment manifests
2. Integration test: deploy with OTEL injection → verify K8s pod has `OTEL_EXPORTER_OTLP_*` env vars

### 7. Background cleanup for CLI auth sessions (Plan 47)

**Status:** TODO (low priority)

`CliAuthManager::evict_stale()` is implemented but not wired into any periodic background task.

**Tasks:**
1. Add periodic call to `evict_stale()` in `main.rs` background loop (e.g., every 5 minutes alongside session cleanup)

### 8. Nextest test-groups for Valkey isolation (Plan 30)

**Status:** TODO (low priority)

`.config/nextest.toml` has only a CI profile. No `[test-groups]` for Valkey serialization. Tests use `FLUSHDB` which can corrupt parallel test state.

**Tasks:**
1. Add `[test-groups]` config to `.config/nextest.toml` to serialize Valkey-dependent tests
2. Or: remove `FLUSHDB` calls and ensure all Valkey keys are UUID-scoped (preferred approach per CLAUDE.md)
