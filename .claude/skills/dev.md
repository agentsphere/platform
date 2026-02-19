# Development Process

Follow this 9-step protocol when implementing any feature, fix, or change. Do not skip steps.

Refer to `CLAUDE.md` for all coding patterns and conventions.

---

## Principles

Apply these throughout every step:

- **DRY (Don't Repeat Yourself)** — Extract shared logic into functions, traits, or modules. If you copy-paste code, refactor it into a single source of truth.
- **Single Responsibility** — Each function, struct, and module should do one thing well. If a function needs an "and" to describe it, split it.
- **Least Surprise** — Code should behave as a reader would expect. Name things clearly, avoid hidden side effects, keep public APIs obvious.
- **Fail Fast** — Validate inputs at boundaries and return errors early. Don't let invalid state propagate deep into the system.
- **YAGNI (You Aren't Gonna Need It)** — Don't build abstractions or features for hypothetical future needs. Solve the problem at hand.
- **Composition over Inheritance** — Prefer trait composition and small composable functions over deep hierarchies or god objects.
- **Plans are Living Documents** — Update the relevant `plans/` file as you implement. Plans rot when they diverge from reality — keep them honest.

---

## Step 1: Understand the requirement

1. Read the relevant plan in `plans/` if one exists for this feature
2. Identify which module(s) under `src/` are affected
3. **Read the existing source** of affected files — understand current code before proposing changes
4. Identify which database tables are involved (check `plans/unified-platform.md` for schema)
5. List the files that will be created or modified
6. State the acceptance criteria before writing any code

## Step 2: Design types first

Before writing logic, define the types:

1. Define status enums with `#[derive(sqlx::Type)]` for any new state fields
2. Add `can_transition_to()` methods for status enums that represent state machines
3. Define request/response structs for any new API endpoints (separate from DB model structs)
4. Define per-module error enum variants for all failure modes
5. Add `From<ModuleError> for ApiError` conversion mapping to HTTP status codes
6. Optionally create newtype wrappers for new IDs (`pub struct XId(Uuid)` with `#[sqlx(transparent)]`). The current codebase uses raw `Uuid` consistently — newtypes are a future improvement, not a blocker.

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

**Skip test-first for**: handler wiring, route registration, config loading glue.

## Step 4: Implement the code

Write the implementation to make tests pass:

1. Use `#[tracing::instrument(skip(pool, state), fields(...))]` on all async functions with side effects
2. Use structured tracing fields: `tracing::info!(user_id = %id, "description")`
3. Use `?` for error propagation with `.context("descriptive message")` from anyhow
4. Use trait-based DI (`impl Repository`) for business logic modules that benefit from test mocking. API handlers use `PgPool` directly via `State(state)` — trait indirection there adds complexity without value.
5. Use the builder pattern for constructing complex structs (K8s pod specs, query builders)
6. No `.unwrap()` in production code
7. Never log sensitive data (passwords, tokens, secrets, webhook URLs)
8. Keep handler functions under 100 lines (clippy `too_many_lines`). Extract helpers like `get_project_repo_path()`, `require_project_write()` for repeated DB lookups or shared setup logic.
9. For permission checks in sub-routers (`fn router() -> Router<AppState>`), use **inline checks** or a helper function like `require_project_write()` — NOT the `require_permission` route layer. The route layer needs `from_fn_with_state(state.clone(), ...)` which requires a concrete `AppState` value, unavailable at sub-router construction time.
10. **Update the plan** as you go — if the implementation deviates from the plan (different approach, extra complexity, changed schema, new dependencies), update the relevant `plans/` file immediately. Don't wait until the end.
11. **Validate all inputs** at the handler boundary using `crate::validation::*` helpers. Every string field from user input must have a length check. See `CLAUDE.md` Security Patterns for field limits.
12. **Check authorization for all read endpoints** on sub-resources (issues, MRs, comments, reviews) — use `require_project_read()` which returns 404 (not 403) for private resources.
13. **Apply SSRF protection** to any feature that makes outbound HTTP requests to user-supplied URLs. Follow the `validate_webhook_url()` pattern from `src/api/webhooks.rs`.
14. **Rate-limit brute-forceable endpoints** (login, password reset, token creation) using `crate::auth::rate_limit::check_rate()`.

Run `just test-unit` — all unit tests should pass (green phase).

## Step 5: Integration tests

If the change involves database queries or HTTP endpoints:

1. Write or update integration tests in `tests/`
2. For DB tests: use `#[sqlx::test(migrations = "migrations")]` with the `pool: PgPool` fixture
3. Test the full flow: setup data, call function, assert result
4. For endpoint tests: construct a test `Router`, send requests with `tower::ServiceExt::oneshot`
5. Use test helpers from `tests/helpers/mod.rs` for common setup
6. Use `insta::assert_json_snapshot!` for API response format stability
7. Run `just test` (requires `DATABASE_URL` pointing to a running Postgres)

If the change is pure logic with no I/O, skip to step 6.

## Step 6: Quality gate

Run the full local CI. Do not skip any checks:

```bash
just ci           # fmt + lint + deny + test-unit + build
```

If database queries changed, also run:

```bash
just db-prepare   # regenerate .sqlx/ offline cache
just db-check     # verify .sqlx/ is up to date
```

Fix all issues before proceeding.

## Step 7: Self-review checklist

Verify every item before considering the work done:

- [ ] Domain IDs use raw `Uuid` consistently (newtypes are a future improvement)
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
- [ ] No handler function exceeds 100 lines (extract helpers for shared DB lookups, repo path resolution, etc.)
- [ ] Sensitive data (passwords, tokens, secrets, webhook URLs) is never logged
- [ ] Migrations are reversible (up + down)
- [ ] `.sqlx/` offline cache is up to date
- [ ] Remove `#[allow(dead_code)]` from foundation items now consumed by new code
- [ ] Zero warnings with `cargo clippy --all-features -- -D warnings`
- [ ] All user-facing string inputs have length validation via `crate::validation::*`
- [ ] Read endpoints on sub-resources check project-level read access (`require_project_read`)
- [ ] Read endpoints for private resources return 404 (not 403) to avoid leaking existence
- [ ] Any outbound HTTP to user-supplied URLs has SSRF protection (block private IPs, metadata endpoints)
- [ ] Brute-forceable endpoints have rate limiting (`check_rate`)
- [ ] New mutations write to `audit_log` — audit detail fields never contain URLs or secrets
- [ ] Webhook dispatch uses the shared `WEBHOOK_CLIENT` (with timeouts) and `WEBHOOK_SEMAPHORE` (concurrency limit)

## Step 8: Update plan & summarize changes

After all checks pass:

**Update the plan file** (`plans/`):

1. Mark completed sections/tasks as done (e.g., `[x]` or `✅`)
2. Update status fields (phase, progress, completion %)
3. Document any deviations from the original plan — what changed and why
4. Note added complexity, new edge cases, or unexpected dependencies discovered during implementation
5. Record alternative approaches that were considered but rejected, with brief reasoning
6. Update any schema, API, or architecture descriptions that no longer match reality
7. Add or revise remaining work estimates if scope changed

**Provide a summary:**

1. What was implemented
2. What tests were added (unit + integration)
3. What files were created or modified
4. What plan changes were made (deviations, status updates)
5. Any follow-up items or known limitations

## Step 9: Capture lessons learned

After completing the work, reflect on what you encountered and update project knowledge:

**Update this dev skill** (`.claude/skills/dev.md`):

1. If a step was missing, ambiguous, or led you astray — fix it in this file
2. If you discovered a new pattern that should be standard — add it to the relevant step
3. If a checklist item in Step 7 would have caught a bug earlier — add it
4. If a principle was violated and caused rework — strengthen the guidance

**Update `CLAUDE.md`**:

1. New conventions discovered during implementation (e.g., derive ordering, trait bounds, sqlx quirks)
2. New architecture rules that emerged (e.g., "always do X when adding a module")
3. Gotchas and footguns — things that compiled but broke at runtime, or caused confusing errors
4. Dependency-specific pitfalls (version conflicts, feature flag requirements, API surprises)
5. New patterns that should be standardized across modules

**Update auto memory** (MEMORY.md in your auto memory directory):

1. Add critical gotchas that wasted significant time
2. Update project state (phase progress, what's complete)
3. Record workflow tips that sped things up or prevented mistakes

**Rules for this step:**

- Only record things that are **stable and confirmed** — not speculative
- Keep entries concise — a gotcha should be 1-3 lines, not a paragraph
- If something contradicts an existing entry, update the existing entry rather than adding a duplicate
- Don't add things that are already documented — check first
- Prefer updating existing sections over creating new ones
