# Skill: Parallel Code Review — Rust Quality, Tests, Security & Coverage

**Description:** Orchestrates parallel AI agents that review *implemented code* (not plans) against Rust best practices, test coverage requirements, and security standards. Each agent reads real source files, runs targeted analysis, and produces categorized findings. Includes coverage analysis on touched lines. Outputs a persistent `_review.md` file for the `finalize` skill.

**Pipeline position:**
```
plan → planReview → dev → ★ review ★ → finalize
```

**When to use:** After implementing a feature (via the `dev` skill) but before finalizing and committing. Feed the output to the `finalize` skill.

---

## Orchestrator Instructions

You are the **Senior Code Reviewer Agent**. Your job is to:

1. Identify what changed (git diff, conversation context, or user direction)
2. Launch parallel review agents that each read the actual code and apply domain-specific review criteria
3. Analyze test coverage on touched lines
4. Synthesize findings into a persistent review file

### Severity Levels

Every finding MUST have a severity:

| Severity | Meaning | Action |
|---|---|---|
| **CRITICAL** | Security vulnerability, data loss risk, crash in production | Must fix before merge |
| **HIGH** | Logic bug, missing auth/validation, broken test, clippy error | Should fix before merge |
| **MEDIUM** | Code smell, missing test edge case, inconsistent pattern, readability | Fix if straightforward, defer if complex |
| **LOW** | Style nit, minor naming, optional improvement | Fix only if trivial |
| **INFO** | Observation, praise, context for future work | No action needed |

---

## Phase 1: Scope the Review

Before launching agents, determine what's being reviewed:

### Option A: Review uncommitted changes

```bash
git diff --stat                    # files changed
git diff                           # actual changes
git status -u                      # new untracked files
```

### Option B: Review specific files

The user points to specific files or modules. Read them directly.

### Option C: Review recent commit(s)

```bash
git log --oneline -5               # recent commits
git diff HEAD~1 --stat             # last commit's files
git diff HEAD~1                    # last commit's changes
```

From the diff, extract:
1. **Changed files** — list every modified/added/deleted file
2. **Changed modules** — which `src/` modules are affected
3. **New tests** — any new test files or test functions
4. **Migration changes** — any new/modified SQL migrations
5. **Config changes** — Cargo.toml, .env.example, Justfile changes

Build a manifest of what each agent should investigate.

---

## Phase 2: Parallel Review Agents

Launch **all four agents concurrently** using the Task tool with `subagent_type: "general-purpose"`. Each agent gets the list of changed files and its specific review checklist.

**Critical:** In each agent's prompt, include:
- The list of changed/added files (from Phase 1)
- Instructions to READ the actual source files before making judgments
- The specific checklist for their domain
- Instructions to output findings in the structured format (severity, file, line, description, suggested fix)

### Agent 1: Rust Quality & Patterns

**Reads:** Every changed `.rs` file under `src/`
**Focus:** Idiomatic Rust, project conventions, performance, maintainability

**Checklist — read each changed file and check for:**

_Error handling:_
- [ ] No `.unwrap()` in production code (only in tests or infallible cases with comment)
- [ ] Error enums cover all failure modes — no catch-all `Internal(String)` for known failures
- [ ] `From<ModuleError> for ApiError` maps to correct HTTP status codes (401 vs 403 vs 404 vs 500)
- [ ] Error messages don't leak internal details (SQL errors, file paths, stack traces) to API responses
- [ ] `?` propagation uses `.context("descriptive message")` where the original error is ambiguous
- [ ] `thiserror` used for types crossing module boundaries (not `anyhow::Error`)

_Type system:_
- [ ] Request/Response types are separate from DB model structs (never expose raw DB rows)
- [ ] Status enums have `can_transition_to()` for state machines
- [ ] Derive ordering follows project convention: `Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, sqlx::Type`
- [ ] `Copy` types use `self` not `&self` (clippy `trivially_copy_pass_by_ref`)
- [ ] No unnecessary `String` allocations — prefer `&str` in function parameters where possible
- [ ] No unnecessary `.clone()` — check if borrows would work

_Handler patterns:_
- [ ] Signature follows convention: `State(state), auth: AuthUser, Path(..), Query(..), Json(..)`
- [ ] No handler exceeds 100 lines (clippy `too_many_lines`) — extract helpers
- [ ] No function exceeds 7 parameters (clippy `too_many_arguments`) — use param structs
- [ ] Pagination uses `ListParams` with `limit` (default 50, max 100) and `offset` (default 0)
- [ ] List responses use `ListResponse<T> { items, total }`

_Observability:_
- [ ] All async functions with side effects have `#[tracing::instrument(skip(pool, state, config), fields(...), err)]`
- [ ] Log statements use structured fields: `tracing::info!(user_id = %id, "description")` — not string interpolation
- [ ] Error paths log with error chain: `tracing::error!(error = %err, "operation failed")`
- [ ] Correlation context included: `user_id`, `project_id`, `session_id` where available
- [ ] Sensitive data NEVER logged: passwords, tokens, secrets, webhook URLs, API keys

_Rust idioms:_
- [ ] No `if let { if { } }` nesting — use `if let ... && condition { }` (clippy `collapsible_if`)
- [ ] Pattern matching preferred over chains of `if/else`
- [ ] `impl Into<X>` or generics where callers pass owned vs borrowed
- [ ] `Iterator` combinators preferred over manual loops where readable
- [ ] No `pub` on items that should be `pub(crate)` or module-private

_Crate-specific gotchas:_
- [ ] `rand 0.10`: uses `rand::fill(&mut bytes)` not `rng.fill_bytes()`
- [ ] `argon2`: uses `argon2::password_hash::rand_core::OsRng` not `rand::rng()` for salt
- [ ] `fred Pool`: uses `pool.next().publish()` for pub/sub (Pool doesn't impl PubsubInterface)
- [ ] `axum 0.8`: `.patch()`, `.put()`, `.delete()` chained as MethodRouter methods, not standalone imports
- [ ] `sqlx INET`: uses `ipnetwork` crate or skips binding (no direct `IpAddr` encoding)

**Output format:**
```
[SEVERITY] file:line — description
  Suggested fix: ...
```

### Agent 2: Test Quality & Coverage

**Reads:** Every changed/added test file (`tests/*.rs`, `#[cfg(test)] mod tests` blocks in `src/`)
**Also reads:** `tests/helpers/mod.rs`, `tests/e2e_helpers/mod.rs`, `docs/testing.md` for context
**Focus:** Test completeness, correctness, and adherence to project test patterns

**Checklist — for each changed source file, verify corresponding tests exist:**

_Coverage analysis:_
- [ ] Every new public function has at least one test (unit or integration)
- [ ] Every new handler has integration tests: happy path + auth failure + validation failure + not found
- [ ] Every new error variant has a test that triggers it
- [ ] Every new state machine transition has tests for valid AND invalid transitions
- [ ] Every new match arm / if-else branch has a test that exercises it
- [ ] Edge cases covered: empty input, max-length input, boundary values, None/null fields
- [ ] Permission boundary tested: authorized access AND unauthorized access (returns 404 not 403 for private resources)

_Test patterns:_
- [ ] Integration tests use `#[sqlx::test(migrations = "./migrations")]` with `pool: PgPool`
- [ ] State built with `helpers::test_state(pool).await` — returns `(state, admin_token)`
- [ ] Router built with `helpers::test_router(state.clone())`
- [ ] Pre-created `admin_token` used for API calls — NOT `admin_login()` (rate limit collision)
- [ ] Dynamic queries `sqlx::query()` in test files — NOT compile-time `sqlx::query!()`
- [ ] No `FLUSHDB` or global Valkey operations (all keys are UUID-scoped)
- [ ] Pipeline tests spawn `ExecutorGuard` and call `state.pipeline_notify.notify_one()`
- [ ] Webhook test URLs inserted directly into DB (SSRF blocks localhost)
- [ ] E2E git repos created under `/tmp/platform-e2e/` (Kind shared mount)
- [ ] E2E tests marked `#[ignore]` and placed in `tests/e2e_*.rs`

_Test quality:_
- [ ] Tests assert on specific values, not just `is_ok()` or `status == 200`
- [ ] Test names are descriptive: `create_project_without_name_returns_400` not `test_1`
- [ ] No flaky patterns: timing-dependent assertions, shared mutable state, ordering assumptions
- [ ] Tests are independent — each creates its own state, no inter-test dependencies
- [ ] Cleanup handled by `#[sqlx::test]` (ephemeral DB) — no manual teardown needed
- [ ] No redundant tests — don't test the same code path twice at the same tier

_Missing test categories (check each):_
- [ ] Concurrent access / race conditions (if applicable)
- [ ] Large input handling (e.g., 100K body, 50 labels, maximum pagination)
- [ ] Token/session expiry behavior
- [ ] Soft-delete filtering (`AND is_active = true` respected)
- [ ] Cascade effects (deleting a project cleans up issues, webhooks, etc.)

**Output format:**
```
[SEVERITY] Missing test — file:function — description
  Suggested test: fn test_name — what it should assert
```

### Agent 3: Security & Authorization

**Reads:** Every changed handler file under `src/api/`, plus `src/auth/`, `src/rbac/`, `src/validation.rs`
**Focus:** Auth gaps, input validation, SSRF, injection, information leakage, audit logging

**Checklist — for each new or modified handler:**

_Authentication:_
- [ ] Handler takes `auth: AuthUser` extractor (no unauthenticated access without explicit reason)
- [ ] If unauthenticated access is intentional, it's documented in a comment
- [ ] Session/token validation includes expiry check

_Authorization:_
- [ ] Read endpoints call `require_project_read()` or equivalent
- [ ] Write endpoints call `require_project_write()` or equivalent
- [ ] Admin endpoints check `Permission::Admin*` via `has_permission()`
- [ ] Sub-resource endpoints (issues, MRs, comments) verify parent project access
- [ ] Private resources return **404** (not 403) — prevents existence leakage
- [ ] No IDOR: user can't access/modify another user's resources by changing IDs in URLs
- [ ] Token scope enforcement: `auth.check_project_scope()` / `auth.check_workspace_scope()` if applicable

_Input validation:_
- [ ] Every string field from user input has a length check via `crate::validation::*`
  - Names/slugs: 1-255
  - Emails: 3-254
  - Passwords: 8-1024
  - Titles: 1-500
  - Bodies/descriptions: 0-100,000
  - URLs: 1-2048
  - Labels: max 50 items, each 1-100 chars
- [ ] No raw SQL string concatenation — all queries parameterized via `sqlx::query!` / `sqlx::query()`
- [ ] JSON parsing uses typed deserialization (axum `Json<T>`) — no raw `serde_json::Value` for user input
- [ ] File paths from user input are validated (no path traversal: `..`, null bytes)
- [ ] Container images validated via `check_container_image()` (no shell injection via image names)

_SSRF & outbound requests:_
- [ ] User-supplied URLs that the server fetches are validated against SSRF (`validate_webhook_url` pattern)
- [ ] Private IPs blocked: 10/8, 172.16/12, 192.168/16, 127/8, ::1
- [ ] Cloud metadata blocked: 169.254.169.254, metadata.google.internal
- [ ] Non-HTTP schemes blocked: file://, ftp://, etc.

_Secrets & sensitive data:_
- [ ] Passwords hashed with argon2 (never stored plaintext)
- [ ] Tokens hashed with SHA-256 before DB storage
- [ ] API responses never include password hashes, token hashes, or secret values
- [ ] Audit log detail field never contains URLs (may have tokens), secrets, or passwords
- [ ] Error responses don't leak internal details (SQL errors, file paths, config values)

_Rate limiting:_
- [ ] Login / authentication endpoints have `check_rate()` applied
- [ ] Token creation / password-related endpoints have rate limiting
- [ ] Any endpoint that sends emails has rate limiting

_Audit logging:_
- [ ] Every mutation (create, update, delete) writes to `audit_log`
- [ ] Audit entry includes: `actor_id`, `actor_name`, `action`, `resource`, `resource_id`, `ip_addr`
- [ ] Audit detail is sanitized — no secrets, tokens, or URLs

_Webhook security:_
- [ ] Webhooks use shared `WEBHOOK_CLIENT` with timeouts (5s connect, 10s total)
- [ ] Webhooks use `WEBHOOK_SEMAPHORE` (50 concurrent, excess dropped with warning)
- [ ] HMAC-SHA256 signing via `X-Platform-Signature` when secret configured
- [ ] Webhook URLs never logged in audit or tracing output

**Output format:**
```
[SEVERITY] Security — file:line — description
  Risk: what could go wrong
  Fix: specific remediation
```

### Agent 4: Database & Migration Quality

**Reads:** Every changed migration file in `migrations/`, every changed `sqlx::query!` call
**Also reads:** Existing migrations for affected tables, `plans/unified-platform.md`
**Focus:** Schema correctness, migration safety, query efficiency, offline cache

**Checklist:**

_Migration safety:_
- [ ] Version number follows `YYYYMMDDHHMMSS_name` (no underscores in version prefix — causes sqlx conflicts)
- [ ] DOWN migration completely undoes UP (verified by reading both)
- [ ] No data loss — `DROP COLUMN` preceded by backfill or documented as intentional
- [ ] No long locks on hot tables (`ALTER TABLE` with `NOT NULL` on large tables needs `DEFAULT`)
- [ ] New columns on existing tables have appropriate `DEFAULT` values
- [ ] Foreign keys reference tables that exist (check other migrations)
- [ ] Indexes created for columns used in WHERE clauses or JOINs
- [ ] `ON DELETE CASCADE` / `SET NULL` behavior is intentional and documented

_Query correctness:_
- [ ] `sqlx::query!` macros used in `src/` (compile-time checked), NOT dynamic `sqlx::query()`
- [ ] `sqlx::query()` (dynamic) used in `tests/` only
- [ ] All queries respect soft-delete: `AND is_active = true` for projects
- [ ] Pagination queries use `LIMIT $X OFFSET $Y` with validated bounds
- [ ] `RETURNING` clauses used for auto-generated values (avoiding extra SELECT)
- [ ] `INSERT ... ON CONFLICT` used appropriately (not masking real constraint violations)
- [ ] Transactions used for multi-step operations that must be atomic

_Performance:_
- [ ] No `SELECT *` — only fetch needed columns
- [ ] `COUNT(*)` queries use appropriate indexes
- [ ] No N+1 query patterns (loop of SELECTs when a JOIN would work)
- [ ] Large result sets use pagination, never unbounded `fetch_all`

_Offline cache:_
- [ ] If any `sqlx::query!` changed, `.sqlx/` cache must be regenerated (`just db-prepare`)
- [ ] New compile-time queries will work with `SQLX_OFFLINE=true`

**Output format:**
```
[SEVERITY] Database — file:line — description
  Impact: what breaks
  Fix: specific SQL or code change
```

---

## Phase 2.5: Coverage Analysis on Touched Lines

After the parallel agents complete but before synthesis, analyze test coverage specifically for lines changed in this implementation.

### 2.5.1 Identify touched files

```bash
# Get list of changed Rust source files (vs main branch or last commit)
git diff --name-only main...HEAD -- 'src/**/*.rs'
# If working on main with uncommitted changes:
git diff --name-only HEAD -- 'src/**/*.rs'
```

### 2.5.2 Run coverage

```bash
# Fast: unit coverage only (always available)
just cov-unit
# Output: coverage-unit.lcov

# Full: combined unit + integration + E2E (requires Kind cluster)
just cov-total
```

### 2.5.3 Analyze touched-file coverage

For each file in the touched list:

1. Find the file's section in the lcov output (starts with `SF:<absolute-path>`)
2. Find `DA:<line>,<hits>` entries for each line
3. Cross-reference with `git diff` to identify which DA lines are new/modified (added `+` lines)
4. A touched line with `DA:<line>,0` is **UNCOVERED**
5. Record uncovered touched lines per file

### 2.5.4 Output format

Produce a coverage table:

| File | Lines changed | Lines covered | Coverage % | Uncovered lines |
|---|---|---|---|---|
| `src/api/handler.rs` | 45 | 42 | 93% | 112-114 |

If any touched file has < 100% coverage on changed lines, produce a **[HIGH]** finding with the specific uncovered lines and what test would cover them.

**Exceptions** (document but don't flag):
- `main.rs` bootstrap wiring (covered by E2E only)
- Generated code (`proto.rs`, `ui.rs`)
- Error paths requiring real infrastructure failures

---

## Phase 3: Synthesis

Once all four agents return and coverage analysis is complete, synthesize into a single report.

### Synthesis rules

1. **Deduplicate** — if multiple agents flag the same issue, merge into one finding with the highest severity
2. **Prioritize** — CRITICAL and HIGH first, always. Don't bury security issues under style nits.
3. **Be actionable** — every finding above LOW must have a concrete fix, not just "improve this"
4. **Credit good work** — include 1-2 INFO items noting well-done patterns (reinforces good habits)
5. **Count the gaps** — provide exact numbers of missing tests, not "some tests are missing"
6. **Number every finding** — R1, R2, R3... for finalize to reference

---

## Phase 3.5: Write Review File

After synthesizing the report, persist it as a file artifact for the `finalize` skill.

### File location and naming

`plans/<plan-name>_review.md`

Where `<plan-name>` matches the plan file name. Examples:
- Plan: `plans/32-permission-redesign.md` → Review: `plans/32-permission-redesign_review.md`
- No plan (ad-hoc): `plans/<feature-name>_review.md`

### File structure

```markdown
# Review: <plan-name or feature-name>

**Date:** <today>
**Scope:** <list of changed files from Phase 1>
**Overall:** PASS / PASS WITH FINDINGS / NEEDS WORK

## Summary
- {1-2 sentences: overall quality assessment}
- {Critical/High count}: X critical, Y high findings
- {Test coverage}: N new tests added, M gaps identified
- {Touched-line coverage}: X% overall on changed lines

## Critical & High Findings (must fix)

### R1: [CRITICAL] {title}
- **File:** `src/path/file.rs:42`
- **Domain:** Security / Rust Quality / Tests / Database
- **Description:** {what's wrong}
- **Risk:** {what could happen}
- **Suggested fix:** {specific code change or approach}

### R2: [HIGH] {title}
...

## Medium Findings (should fix)

### RN: [MEDIUM] {title}
- **File:** ...
- **Description:** ...
- **Suggested fix:** ...

## Low Findings (optional)

- [LOW] R{N}: `src/path/file.rs:10` — {one-line description} → {one-line fix}

## Coverage — Touched Lines

| File | Lines changed | Lines covered | Coverage % | Uncovered lines |
|---|---|---|---|---|
| `src/api/handler.rs` | 45 | 42 | 93% | 112-114 |

### Uncovered Paths
- `src/api/handler.rs:112-114` — error path when DB connection fails; needs integration test `test_handler_db_error`

## Checklist Results

| Category | Status | Notes |
|---|---|---|
| Error handling | PASS/FAIL | {details if FAIL} |
| Auth & permissions | PASS/FAIL | |
| Input validation | PASS/FAIL | |
| Audit logging | PASS/FAIL | |
| Tracing instrumentation | PASS/FAIL | |
| Clippy compliance | PASS/FAIL | |
| Test patterns | PASS/FAIL | |
| Migration safety | PASS/FAIL | |
| Touched-line coverage | PASS/FAIL | {X% — target 100%} |
```

### Rules
- Every finding gets a unique ID (R1, R2, ...) — finalize references these in commit messages
- The file must be self-contained — readable without conversation context
- Do NOT include INFO-level items in the file (keep it actionable)
- Include the coverage table even if coverage is 100% (proves it was checked)

---

## Usage Notes

- This skill reviews **implemented code**, not plans. Use `planReview` for plan review.
- Best used after the `dev` skill's Step 4-5 (code + tests written) but before finalize.
- The review file (`plans/<name>_review.md`) is consumed by the `finalize` skill. It must exist before finalize runs.
- For small changes (<3 files), you can skip the parallel agents and review directly — but still follow the checklists and write the review file.
- Each agent should have `max_turns` of 15-20 to allow thorough file reading.
- Always read the actual code — never review based on memory or assumptions.
- Coverage analysis requires `cargo-llvm-cov`. If not available, note the gap and let finalize handle coverage verification.

---

## Reflection & Improvement

After completing this skill's primary work, check if any triggers apply:

### Triggers
- [ ] Encountered a gotcha or crate quirk not documented in CLAUDE.md
- [ ] Found a missing instruction in THIS skill that caused confusion or rework
- [ ] An agent's checklist was missing a relevant check that would have caught an issue
- [ ] Coverage analysis missed an important code path
- [ ] The review file format caused confusion for finalize
- [ ] The dev skill should have caught something before review
- [ ] A new pattern emerged that should be standardized
- [ ] docs/ content (architecture.md, testing.md) no longer matches reality

### If any trigger fires, apply the minimum update:

| Target | When | What |
|---|---|---|
| `.claude/skills/review.md` | Missing checklist item or ambiguous instruction | Add/clarify |
| `.claude/skills/dev.md` | Dev should have caught something before review | Add to Step 7 sanity check |
| `.claude/skills/finalize.md` | Review file format needs adjustment | Update format spec |
| `CLAUDE.md` | New convention, gotcha, or architecture rule | Add to relevant section |
| `docs/*.md` | Architecture/testing docs don't match reality | Update affected section |

### Rules
- Keep changes concise — 1-5 lines per update
- Check for duplicates before adding
- Update existing entries rather than adding contradictory new ones
- Do NOT commit these separately — they go with the skill's primary work
- Note what you changed in your summary to the user
