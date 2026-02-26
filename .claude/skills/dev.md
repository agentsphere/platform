# Skill: Development — Implement, Test, Iterate

**Description:** The implementation skill. Takes a plan (or a direct requirement) and turns it into working, tested code. Follows a strict read-first → test-first → implement → verify loop. Keeps the plan file up-to-date as a living document throughout implementation.

**Pipeline position:**
```
plan → planReview → ★ dev ★ → review → finalize
```

Refer to `CLAUDE.md` for all coding patterns, conventions, and architecture rules.

---

## Entry Points

Choose the right entry point based on what you're implementing:

### A. Plan-driven implementation (most common)
A reviewed plan exists in `plans/`. Start at **Step 1** — read the plan, then implement PR by PR.

### B. Direct feature / bugfix (no plan)
The user describes what to build or fix. Start at **Step 0** — assess scope first.

### C. Resuming work
Picking up where you or someone else left off. Start at **Step 1** — read existing code + plan state.

---

## Step 0: Assess Scope (entry point B only)

Before writing code, decide if you need a plan:

| Scope | Action |
|---|---|
| **Trivial** — typo fix, one-liner, obvious bug | Skip to Step 1, no plan needed |
| **Small** — single handler, <50 lines of logic, no migration | Skip to Step 1, no plan needed |
| **Medium** — new endpoint with tests, migration, 2-3 files | Consider a lightweight plan (outline PRs + test list) |
| **Large** — multiple PRs, new module, cross-cutting changes | **Stop.** Use the `plan` skill first, then `planReview`, then come back here |

If in doubt, plan. Rework from a bad start costs more than 10 minutes of planning.

---

## Step 1: Read Before You Write

**This step is mandatory. Never skip it.** Bad assumptions are the #1 cause of rework.

### 1.1 Read the plan

If a plan exists in `plans/`:
1. Read the full plan (especially the current PR section you're implementing)
2. **Check for "## Plan Review Findings"** — planReview appends findings to the plan. Address any critical/high findings as you implement.
3. Read the TDD test tables that planReview added — these define what tests to write first
4. Note acceptance criteria, file change tables, and verification steps

### 1.2 Read affected source

**For every file you're about to modify, read it first.** Specifically:

| What | Why |
|---|---|
| `src/<module>/mod.rs` | Understand public API, re-exports |
| `src/<module>/error.rs` | Know existing error variants before adding new ones |
| `src/api/<handler>.rs` | See existing handler patterns in this file |
| `src/api/mod.rs` | Understand route tree — avoid path conflicts |
| `src/api/helpers.rs` | Know available permission check helpers |
| `src/validation.rs` | Know available validation helpers — don't reinvent |
| `src/error.rs` | Know `ApiError` variants and existing `From` impls |

For DB changes, also read:
| What | Why |
|---|---|
| Related `migrations/*.up.sql` | Understand current schema |
| `plans/unified-platform.md` | Canonical schema reference |

For test changes, also read:
| What | Why |
|---|---|
| `tests/helpers/mod.rs` | Available helpers: `test_state`, `test_router`, HTTP helpers |
| `tests/<module>_integration.rs` | Existing test patterns for this module |
| Relevant `tests/e2e_*.rs` | E2E patterns if needed |

### 1.3 State acceptance criteria

Before writing any code, write down:
1. What the change does (one sentence)
2. What endpoints/functions are added or modified
3. What tests will prove it works
4. What existing tests must still pass

---

## Step 2: Types & Errors First

Define the shape of the code before writing logic. This catches design issues at compile time.

1. **Status enums** with `#[derive(sqlx::Type)]` + `can_transition_to()` for state machines
2. **Request/Response structs** — always separate from DB model structs
3. **Error enum variants** — one per failure mode, using `thiserror`
4. **`From<ModuleError> for ApiError`** — map each variant to the correct HTTP status code
5. **Migrations** — write full UP + DOWN SQL. Verify:
   - Version: `YYYYMMDDHHMMSS_name` (no underscores in version prefix)
   - DOWN completely undoes UP
   - New columns have appropriate defaults + NOT NULL
   - Foreign keys reference existing tables

Run `cargo check` — types must compile before you proceed.

---

## Step 3: Write Tests First (TDD Red Phase)

For new business logic, write failing tests FIRST that define expected behavior. Use the TDD test tables from planReview in the plan file as your guide.

### What gets test-first treatment

| Category | Test tier | Write first? |
|---|---|---|
| State machine transitions | Unit | Yes |
| Permission resolution logic | Unit | Yes |
| Parsers, validators | Unit | Yes |
| Encryption/HMAC round-trips | Unit | Yes |
| New API handlers | Integration | Yes — at least happy path + auth fail |
| CRUD operations | Integration | Yes — create + read + validation error |
| Handler wiring, route registration | Integration | No — alongside implementation |
| Config loading glue | — | No — too trivial |

### Writing unit tests

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn my_enum_rejects_invalid_transition() {
        assert!(!MyStatus::Running.can_transition_to(&MyStatus::Pending));
    }
}
```

### Writing integration tests

```rust
#[sqlx::test(migrations = "./migrations")]
async fn create_widget_returns_201(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    let (status, body) = helpers::post_json(&app, &admin_token, "/api/widgets",
        serde_json::json!({"name": "test-widget"}),
    ).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["name"], "test-widget");
}
```

**Critical test patterns** (violating these causes flaky tests):
- Use `helpers::test_state(pool).await` — never `admin_login()` (rate limit collision)
- Use `sqlx::query()` (dynamic) in tests — never `sqlx::query!()` (needs offline cache)
- Never FLUSHDB — all Valkey keys are UUID-scoped
- Pipeline tests: spawn `ExecutorGuard` + `state.pipeline_notify.notify_one()`
- Webhook URLs: insert directly into DB (SSRF blocks localhost)
- E2E git repos: create under `/tmp/platform-e2e/`

Run `just test-unit` — tests should **compile but fail** (red phase).

---

## Step 4: Implement (TDD Green Phase)

Write the minimum code to make tests pass. Follow these rules in order of priority:

### 4.1 Security (non-negotiable)

Every handler must have ALL of these. No exceptions.

| Requirement | How | Reference |
|---|---|---|
| Authentication | `auth: AuthUser` extractor | `src/auth/middleware.rs` |
| Authorization | `require_project_read/write()` or inline `has_permission()` | `src/api/helpers.rs` |
| Existence leaking | Return **404** (not 403) for private resources | `CLAUDE.md` Security Patterns |
| Input validation | `crate::validation::check_*` at handler boundary | `src/validation.rs` |
| Audit logging | `AuditEntry` for all mutations | `CLAUDE.md` Auth & RBAC |
| SSRF protection | `validate_webhook_url()` on user-supplied outbound URLs | `src/api/webhooks.rs` |
| Rate limiting | `check_rate()` on brute-forceable endpoints | `src/auth/rate_limit.rs` |
| Sensitive data | Never log passwords, tokens, secrets, webhook URLs | `CLAUDE.md` Observability |

### 4.2 Rust quality

| Rule | Limit | Fix |
|---|---|---|
| No `.unwrap()` | 0 in production code | Use `?`, `.ok_or()`, `.unwrap_or_default()` |
| Handler length | 100 lines max | Extract helpers: `get_project_repo_path()`, `require_project_write()` |
| Function params | 7 max | Use param structs: `AuditEntry`, `CreateDelegationParams` |
| No nested if-let | — | Use `if let ... && condition { }` (clippy `collapsible_if`) |
| Tracing | All async fns with side effects | `#[tracing::instrument(skip(pool, state), fields(...), err)]` |
| Structured logs | Always | `tracing::info!(user_id = %id, "description")` — not string interpolation |
| Error propagation | Use `?` with context | `.context("descriptive message")` from anyhow |
| Error types | `thiserror` for module boundaries | `#[derive(Debug, thiserror::Error)]` |

### 4.3 API conventions

| Convention | Pattern |
|---|---|
| Handler signature order | `State(state), auth: AuthUser, Path(..), Query(..), Json(..)` |
| Pagination | `ListParams { limit: Option<i64>, offset: Option<i64> }` → default 50/0, max 100 |
| List response | `ListResponse<T> { items: Vec<T>, total: i64 }` |
| Soft-delete filtering | Always `AND is_active = true` on project queries |
| Auto-increment numbers | `UPDATE projects SET next_X_number = next_X_number + 1 ... RETURNING` |
| Webhook dispatch | `fire_webhooks(&pool, project_id, "event_name", &payload).await` after mutations |

### 4.4 Permission checks in sub-routers

Sub-routers return `Router<AppState>` without a concrete state. Use **inline checks** or helper functions — NOT the `require_permission` route layer (needs `from_fn_with_state` with concrete state).

```rust
// Correct: helper function
async fn require_project_write(state: &AppState, auth: &AuthUser, project_id: Uuid) -> Result<(), ApiError> {
    let allowed = resolver::has_permission(&state.pool, &state.valkey, auth.user_id, Some(project_id), Permission::ProjectWrite)
        .await.map_err(ApiError::Internal)?;
    if !allowed { return Err(ApiError::Forbidden); }
    Ok(())
}
```

### 4.5 Inner loop

After implementing each function/handler:

```bash
just test-unit     # ~1s — run after every change
```

All unit tests should pass (green phase). If not, fix before moving on.

---

## Step 5: Integration & E2E Tests

Once unit tests are green, write higher-tier tests for DB and API interactions.

### Integration tests

Write when: the change involves database queries, HTTP endpoints, or auth flows.

```rust
#[sqlx::test(migrations = "./migrations")]
async fn my_handler_test(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());
    // ... test with helpers::get_json, post_json, etc.
}
```

**Required coverage per handler:**
- Happy path (success response with correct body)
- Authentication failure (no token → 401)
- Authorization failure (wrong permissions → 404 for private resources)
- Validation failure (bad input → 400 with descriptive error)
- Not found (nonexistent resource → 404)
- Edge cases: empty input, boundary values, duplicate creation

### E2E tests

Write when: changes involve K8s pods, git operations, webhook delivery, deployment reconciliation, or agent sessions.

```rust
#[sqlx::test(migrations = "./migrations")]
#[ignore]  // only runs with `just test-e2e`
async fn my_e2e_test(pool: PgPool) {
    let (state, admin_token) = e2e_helpers::e2e_state(pool).await;
    let app = e2e_helpers::test_router(state.clone());
    // ... test with real K8s, git repos, etc.
}
```

### UI changes

If API response shapes changed:
1. Update types in `ui/src/lib/types.ts`
2. Update API client in `ui/src/lib/api.ts` if new endpoints
3. Update page components in `ui/src/pages/`
4. Run `just ui` to verify build

### MCP changes

If agent-facing APIs changed:
1. Update relevant MCP server in `mcp/servers/`
2. Shared client: `mcp/lib/client.js`

### Run integration tests

```bash
just test-integration   # ~2.5 min, requires Kind cluster
```

If E2E tests were added/changed:
```bash
just test-e2e           # ~2.5 min, requires Kind cluster
```

---

## Step 5.5: Update Plan Progress

After completing each PR section or implementation milestone, update the plan file.

### What to update

1. **Check off completed items** in the PR's progress checklist:
   ```markdown
   - [x] Types & errors defined
   - [x] Migration applied
   - [x] Tests written (red phase)
   - [x] Implementation complete (green phase)
   - [ ] Integration/E2E tests passing  <-- still in progress
   - [ ] Quality gate passed
   ```

2. **Note deviations** from the plan — if implementation differed:
   ```markdown
   > **Deviation:** Used `check_length` instead of custom validator.
   > Reason: existing helper covers the use case.
   ```

3. **Update test counts** if actual differs from what planReview specified

4. **Add discovered work** — if implementation revealed needed changes not in the original plan, add them as sub-items

### When to update

- After completing each PR section's implementation
- After discovering a deviation from the plan
- Before switching to a different PR section
- Before pausing work (so the next session knows the state)

### Rules
- Do NOT commit the plan updates — finalize handles all commits
- Keep updates factual and brief
- The plan should be readable as a status report at any point

---

## Step 6: Quality Gate

Run the full CI. **Do not skip any tier.**

```bash
just ci-full      # fmt + lint + deny + test-unit + test-integration + test-e2e + build
```

If iterating quickly, run tiers incrementally:
```bash
just test-unit           # always (~1s)
just test-integration    # after API/DB/auth changes (~2.5 min)
just test-e2e            # after K8s/pipeline/deployer/git/webhook changes (~2.5 min)
```

**Before declaring work complete, all three tiers MUST pass.**

If database queries changed:
```bash
just db-prepare   # regenerate .sqlx/ offline cache
just db-check     # verify cache is up to date
```

Fix all failures before proceeding. Never skip a failing test or mark it `#[ignore]` to get green.

---

## Step 7: Quick Sanity Check

Do a fast self-check before handing off to review. This is NOT a full review (that's the `review` skill's job) — just a quick scan for obvious misses.

### 5-second checks (if any fail, fix immediately)

- [ ] `cargo clippy --all-features -- -D warnings` is clean
- [ ] No `.unwrap()` in new production code
- [ ] No sensitive data in new log statements
- [ ] New migrations have both UP and DOWN files
- [ ] `.sqlx/` cache is up to date (if queries changed)

### 30-second checks (scan the diff)

```bash
git diff --stat    # see what changed
```

- [ ] Every new handler has `auth: AuthUser`
- [ ] Every new handler has input validation
- [ ] Every mutation has audit logging
- [ ] No files accidentally included (`.env`, coverage reports, build artifacts)
- [ ] Test helper updates in BOTH `tests/helpers/mod.rs` AND `tests/e2e_helpers/mod.rs` (if AppState changed)

If any check fails, fix it. If everything looks good, proceed.

---

## Step 8: Hand Off to Review

At this point, the code is implemented and tests pass. **Use the `review` skill** to get a thorough automated review before committing.

The `review` skill will:
1. Launch 4 parallel agents (Rust quality, test coverage, security, database)
2. Read every changed file and apply domain-specific checklists
3. Analyze test coverage on touched lines
4. Produce a `plans/<plan-name>_review.md` file with numbered findings

After the review report is ready, **use the `finalize` skill** to:
1. Read the review file and triage each finding
2. Implement accepted fixes
3. Verify 100% coverage on touched lines
4. Re-run full test suite
5. Commit to a feature branch, push, and create a PR

**For trivial changes** (typo fixes, one-liner bug fixes), you may skip the formal review/finalize cycle and commit directly after the quality gate passes. Use judgment — if the change touches auth, permissions, or money, always review.

---

## Step 9: Summary

Provide a summary to the user:

1. What was implemented
2. What tests were added (counts by tier)
3. What files were created or modified
4. Any deviations from the plan (already noted in the plan file)
5. Follow-up items or known limitations

---

## Reflection & Improvement

After completing this skill's primary work, check if any triggers apply:

### Triggers
- [ ] Encountered a gotcha or crate quirk not documented in CLAUDE.md
- [ ] Found a missing instruction in THIS skill that caused confusion or rework
- [ ] The plan was missing information that caused confusion during implementation
- [ ] The planReview test strategy was incomplete or had wrong test patterns
- [ ] The review skill's checklists should include something discovered during dev
- [ ] A new pattern emerged that should be standardized
- [ ] An existing instruction was outdated or misleading
- [ ] docs/ content (architecture.md, testing.md) no longer matches reality
- [ ] README.md no longer reflects current architecture

### If any trigger fires, apply the minimum update:

| Target | When | What |
|---|---|---|
| `.claude/skills/dev.md` | Missing step or ambiguous instruction | Add/clarify |
| `.claude/skills/plan.md` | Plan skill missed something that caused confusion | Add to Phase 5 checklist |
| `.claude/skills/planReview.md` | Test strategy was incomplete | Add to Agent 4 checklist |
| `.claude/skills/review.md` | Review should check for something dev discovered | Add to agent checklist |
| `CLAUDE.md` | New convention, gotcha, or architecture rule | Add to relevant section |
| `docs/*.md` | Architecture/testing docs don't match reality | Update affected section |
| `README.md` | Significant capability change | Update description |

### Rules
- Keep changes concise — 1-5 lines per update
- Check for duplicates before adding
- Update existing entries rather than adding contradictory new ones
- Do NOT commit these separately — they go with the skill's primary work
- Note what you changed in your summary to the user

---

## Principles

Apply throughout every step:

- **DRY** — Extract shared logic. If you copy-paste, refactor into a single source of truth.
- **Single Responsibility** — Each function/struct/module does one thing. If it needs "and" to describe it, split it.
- **Least Surprise** — Code should behave as a reader expects. Clear names, no hidden side effects.
- **Fail Fast** — Validate at boundaries, return errors early. Don't let invalid state propagate.
- **YAGNI** — Don't build for hypothetical future needs. Solve the problem at hand.
- **Composition over Inheritance** — Trait composition and small composable functions.
- **Plans are Living Documents** — Update `plans/` as you implement. Don't let plans rot.

---

## Crate Gotchas (Quick Reference)

| Crate | Gotcha | Fix |
|---|---|---|
| `rand 0.10` | `rng.fill_bytes()` doesn't work | `rand::fill(&mut bytes)` (free function) |
| `argon2` + `rand` | Incompatible `rand_core` versions (0.6 vs 0.9) | `argon2::password_hash::rand_core::OsRng` for salt |
| `fred Pool` | Doesn't impl `PubsubInterface` | `pool.next().publish()` |
| `axum 0.8` | `.patch()`, `.put()` are `MethodRouter` methods | Chain on routes, don't import from `axum::routing` |
| `sqlx INET` | `IpAddr` doesn't impl `Encode<Postgres>` | Use `ipnetwork` crate or skip binding |
| Clippy | `too_many_arguments` threshold = 7 | Use param structs |
| Clippy | `too_many_lines` threshold = 100 | Extract helpers |
| Clippy | `collapsible_if` | Use `if let ... && condition { }` |
| Clippy | `trivially_copy_pass_by_ref` | Use `self` not `&self` for `Copy` types |
