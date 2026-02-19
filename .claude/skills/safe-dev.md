# Safe Development Process

Follow this 8-step protocol when implementing any feature, fix, or change. Do not skip steps.

Refer to `CLAUDE.md` for all coding patterns and conventions.

---

## Step 1: Understand the requirement

1. Read the relevant plan in `plans/` if one exists for this feature
2. Identify which module(s) under `src/` are affected
3. Identify which database tables are involved (check `plans/unified-platform.md` for schema)
4. List the files that will be created or modified
5. State the acceptance criteria before writing any code

## Step 2: Design types first

Before writing logic, define the types:

1. Create newtype wrappers for any new IDs (`pub struct XId(Uuid)` with `#[sqlx(transparent)]`)
2. Define status enums with `#[derive(sqlx::Type)]` for any new state fields
3. Add `can_transition_to()` methods for status enums that represent state machines
4. Define request/response structs for any new API endpoints (separate from DB model structs)
5. Define per-module error enum variants for all failure modes
6. Add `From<ModuleError> for ApiError` conversion mapping to HTTP status codes

Run `cargo check` to verify types compile.

## Step 3: Write tests for business logic

For any new business logic (state machines, permission checks, parsers, validators, encryption):

1. Create `#[cfg(test)] mod tests` block in the source file
2. Write test cases FIRST that define expected behavior:
   - Happy path
   - Error cases and edge cases
   - State transition validation (valid and invalid)
   - Permission boundary tests (authorized and unauthorized)
3. Run `just test-unit` — tests should compile but fail (red phase)

For database-dependent code, write `#[sqlx::test]` integration tests in `tests/`:

1. Create test file in `tests/` directory
2. Use `#[sqlx::test(migrations = "migrations")]` with the `pool: PgPool` fixture
3. Test the full flow: setup data, call function, assert result

**Skip test-first for**: handler wiring, route registration, config loading glue.

## Step 4: Implement the code

Write the implementation to make tests pass:

1. Use `#[tracing::instrument(skip(pool, state), fields(...))]` on all async functions with side effects
2. Use structured tracing fields: `tracing::info!(user_id = %id, "description")`
3. Use `?` for error propagation with `.context("descriptive message")` from anyhow
4. Use trait-based DI: accept `impl Repository` not `PgPool` directly for business logic
5. Use the builder pattern for constructing complex structs (K8s pod specs, query builders)
6. No `.unwrap()` in production code
7. Never log sensitive data (passwords, tokens, secrets)

Run `just test-unit` — all unit tests should pass (green phase).

## Step 5: Integration tests

If the change involves database queries or HTTP endpoints:

1. Write or update integration tests in `tests/`
2. For endpoint tests: construct a test `Router`, send requests with `tower::ServiceExt::oneshot`
3. For DB tests: use `#[sqlx::test(migrations = "migrations")]`
4. Use test helpers from `tests/helpers/mod.rs` for common setup
5. Use `insta::assert_json_snapshot!` for API response format stability
6. Run `just test` (requires `DATABASE_URL` pointing to a running Postgres)

If the change is pure logic with no I/O, skip to step 6.

## Step 6: Quality gate

Run ALL checks. Do not skip any:

```bash
just fmt          # auto-format code
just lint         # clippy pedantic — fix all warnings
just test-unit    # all unit tests pass
just deny         # dependency audit passes
```

If database queries changed:

```bash
just db-prepare   # regenerate .sqlx/ offline cache
just db-check     # verify .sqlx/ is up to date
```

Fix all issues before proceeding.

## Step 7: Self-review checklist

Verify every item before considering the work done:

- [ ] New domain IDs use newtypes, not raw `Uuid`
- [ ] Module error enum has variants for all failure modes
- [ ] `From<ModuleError> for ApiError` conversion maps to correct HTTP status codes
- [ ] All async functions with side effects have `#[tracing::instrument]` with `skip` and `fields`
- [ ] All log statements use structured fields, not string interpolation
- [ ] Error paths include error chain in log: `tracing::error!(error = %err, ...)`
- [ ] Business logic has unit tests in `#[cfg(test)] mod tests`
- [ ] DB-dependent code has `#[sqlx::test]` integration tests
- [ ] Edge cases are covered (empty input, invalid transitions, unauthorized access)
- [ ] Request/Response types are separate from DB model types
- [ ] Handlers follow signature convention: State, AuthUser, Path, Query, Json
- [ ] No `.unwrap()` in production code
- [ ] Sensitive data (passwords, tokens, secrets) is never logged
- [ ] Migrations are reversible (up + down)
- [ ] `.sqlx/` offline cache is up to date
- [ ] Zero warnings with `cargo clippy --all-features -- -D warnings`

## Step 8: Summarize changes

After all checks pass, provide a summary:

1. What was implemented
2. What tests were added (unit + integration)
3. What files were created or modified
4. Any follow-up items or known limitations
