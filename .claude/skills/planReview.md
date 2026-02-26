# Skill: Plan Review & Test Strategy Designer

**Description:** Orchestrates parallel AI agents that investigate the actual codebase, review the implementation plan, and design the comprehensive TDD test strategy. This skill has two responsibilities: (1) validate and improve the implementation plan by editing it directly, and (2) design detailed test tables, branch coverage checklists, and test count targets for each PR.

**Pipeline position:**
```
plan → ★ planReview ★ → dev → review → finalize
```

After planReview runs, the plan file should be **complete and ready for dev to implement** — implementation design + full test strategy + any findings.

## Core Objectives

1. **Ground Truth:** Verify the plan's assumptions about existing code, types, patterns, and dependencies match reality
2. **Fix in Place:** Directly edit the plan file to correct wrong patterns, missing steps, incorrect assumptions
3. **Test Strategy:** Design comprehensive TDD test tables for each PR — this skill OWNS the test strategy
4. **100% Touched-Line Coverage:** Every new or modified line of code must be covered by at least one test (unit, integration, or E2E). The test strategy must map every code path to a specific test. No line left uncovered.
5. **Completeness:** Identify missing edge cases, uncovered test scenarios, or broken cross-module dependencies
6. **Conciseness:** Flag over-engineering, unnecessary boilerplate, or overly complex abstractions

---

## Phase 1: Plan Analysis (You, the Orchestrator)

Before launching agents, read the plan yourself and extract:

1. **Affected modules** — which `src/` directories and files the plan touches
2. **New/modified DB tables** — migration names, schema changes
3. **New/modified API endpoints** — routes, handlers, request/response types
4. **Cross-module dependencies** — which existing modules are imported or extended
5. **Test outlines** — what the plan skill sketched as test needs per PR

Use this to craft targeted prompts for each agent.

---

## Phase 2: Parallel Investigation + Review Agents

Launch **all five agents concurrently** using the Task tool. Each agent combines codebase exploration with domain-specific review. Use `subagent_type: "general-purpose"` for all agents — they need both search and analysis capabilities.

**Critical:** In each agent's prompt, include:
- The full plan text (or the relevant sections for that agent's domain)
- The specific files/directories they should investigate
- Their review checklist

### Agent 1: Schema & Migration Analyst

**Investigates:** Existing migrations, DB schema, related SQL queries
**Reviews:** Proposed migrations, data model changes, rollback safety

**Investigation tasks:**
- Read the existing migrations in `migrations/` that relate to tables the plan modifies
- Search for all `sqlx::query!` and `sqlx::query_as!` calls that reference affected tables
- Read `plans/unified-platform.md` for the canonical schema if the plan references it
- Check for existing indexes, constraints, and foreign keys on affected tables

**Review tasks:**
- Analyze proposed migrations for rollback safety (DOWN migrations must undo UP completely)
- Check for data loss risks — does an ALTER DROP COLUMN lose data? Is there a backfill step?
- Identify locking issues on large tables (ALTER TABLE on hot tables = downtime)
- Verify new columns have appropriate defaults, NOT NULL constraints, and types
- Check that migration version numbers follow the `YYYYMMDDHHMMSS_name` convention (no underscores in version prefix)
- Verify all new `sqlx::query!` calls will work with `SQLX_OFFLINE=true` after `just db-prepare`

**Output:** List of migration flaws, schema inconsistencies, and query compatibility issues.

### Agent 2: Security & Authorization Analyst

**Investigates:** Existing auth middleware, RBAC resolver, validation helpers, rate limiting
**Reviews:** Security posture of proposed changes

**Investigation tasks:**
- Read `src/auth/middleware.rs` (AuthUser extractor) and `src/rbac/resolver.rs` (permission checks)
- Read `src/validation.rs` for existing validation helpers
- Read `src/auth/rate_limit.rs` for rate limiting patterns
- Read `src/api/webhooks.rs` for SSRF protection patterns (`validate_webhook_url`)
- Search for existing permission check patterns in handlers the plan modifies

**Review tasks:**
- Verify every new handler has appropriate auth (AuthUser extractor) and permission checks
- Check for TOCTOU vulnerabilities in multi-step operations (check permission, then act)
- Verify all user inputs have validation at the handler boundary (`crate::validation::*`)
- Check that read endpoints on sub-resources verify project-level access
- Verify private resources return 404 (not 403) to avoid leaking existence
- Check for missing rate limiting on brute-forceable endpoints
- Verify SSRF protection on any new outbound HTTP to user-supplied URLs
- Check that audit logging covers all mutations (and never logs secrets/URLs)
- Review any new credential handling, token generation, or cryptographic operations

**Output:** List of security vulnerabilities, missing validations, and authorization gaps.

### Agent 3: Rust Architecture & Patterns Analyst

**Investigates:** Existing module structure, types, traits, error handling in affected code
**Reviews:** Proposed Rust code for idiom compliance and architectural fit

**Investigation tasks:**
- Read the `mod.rs` of each affected module to understand its public API
- Read existing error enums (`error.rs`) in affected modules
- Read `src/error.rs` for the `ApiError` type and existing `From` conversions
- Read `src/config.rs` and `src/state.rs` (or wherever `AppState` is defined) if the plan adds new state fields
- Search for existing patterns that the plan claims to follow (e.g., "same pattern as X")
- Check `Cargo.toml` for existing dependency versions if the plan adds new crates

**Review tasks:**
- Verify proposed types follow existing conventions (derive ordering, sqlx attributes, serde attributes)
- Check error enum variants cover all failure modes and map to correct HTTP status codes
- Verify new `AppState` fields (if any) are threadsafe (`Arc`, `Clone`, etc.)
- Check for unnecessary `.clone()`, `String` allocations, or missing lifetimes
- Verify handler signatures follow the convention: `State, AuthUser, Path, Query, Json`
- Check that no handler will exceed 100 lines (clippy `too_many_lines`)
- Verify no function will exceed 7 parameters (clippy `too_many_arguments`)
- Check for potential issues with known crate gotchas (rand 0.10, argon2, fred Pool, axum 0.8 routing)
- Verify the plan's import paths and type names actually exist in the codebase

**Output:** List of pattern violations, type system issues, and Rust-specific improvements.

### Agent 4: Test Strategy Designer

**Investigates:** Existing test infrastructure, helpers, coverage patterns
**Designs:** Comprehensive TDD test tables for each PR in the plan
**Reviews:** Whether the plan's test outline is sufficient

This is the **most important agent** for planReview — it produces the detailed test strategy.

**Investigation tasks:**
- Read `tests/helpers/mod.rs` for available integration test helpers
- Read `tests/e2e_helpers/mod.rs` for available E2E test helpers
- Read 1-2 existing integration test files that follow similar patterns to what the plan proposes
- Search for existing tests on the modules the plan modifies (to check for cascading breakage)
- Read `docs/testing.md` for testing conventions

**Design tasks — for each PR in the plan, produce:**

1. **"Tests to write FIRST" tables** with concrete test names, descriptions, and tiers:

```markdown
#### Tests to write FIRST (before implementation)

**Unit tests — `src/<module>/<file>.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_name_descriptive` | One-sentence description | Unit |

**Integration tests — `tests/<file>_integration.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_name_descriptive` | One-sentence description | Integration |

**E2E tests — `tests/e2e_<file>.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_name_descriptive` | One-sentence description | E2E |

Total: X unit + Y integration + Z E2E tests
```

2. **"Existing tests to UPDATE"** — tests that will break and how to fix them:

| Test file | Change | Reason |
|---|---|---|
| `tests/...` | Description | Why it breaks |

3. **"Branch coverage checklist"** — every new code branch mapped to a test:

| Branch/Path | Test that covers it |
|---|---|
| `if !allowed → 404` | `test_unauthorized_returns_404` |

4. **"Tests NOT needed"** — explicitly state what's NOT tested and why (must justify each exclusion)

5. **"Coverage target: 100% of touched lines"** — verify the test tables above cover every new/modified line:

```markdown
#### Coverage Verification

Every new or modified line must be covered by at least one test:

| Code path / line range | Covered by test | Tier |
|---|---|---|
| `handler.rs:20-35` (happy path) | `test_create_widget_returns_201` | Integration |
| `handler.rs:37-42` (validation error) | `test_create_widget_missing_name_400` | Integration |
| `handler.rs:44-48` (permission denied) | `test_create_widget_unauthorized_404` | Integration |
| `types.rs:10-25` (enum transitions) | `test_status_valid_transitions` | Unit |

Exceptions (must document why):
- `main.rs` bootstrap wiring — covered by E2E only
- Generated code (`proto.rs`, `ui.rs`) — auto-generated
- Infrastructure-failure error paths — requires real K8s/DB failures
```

**Test tier selection rules:**

| What you're testing | Tier | Why |
|---|---|---|
| Pure functions, enum parsing, state machines, validation | Unit | No I/O needed |
| Encryption round-trips, HMAC verification | Unit | Crypto is pure logic |
| Permission resolution (given role X, can they do Y?) | Unit (basic) + Integration (DB) | Unit for logic, integration for real DB |
| API handler wiring (route → handler → response) | Integration | Needs DB + HTTP stack |
| CRUD operations (create, read, update, delete) | Integration | Needs DB |
| Auth flows (login, token creation, session management) | Integration | Needs DB + Valkey |
| Cross-module interactions (webhook fires after issue create) | Integration | Needs multiple services |
| K8s pod execution (pipeline, agent, deployer) | E2E | Needs real K8s |
| Git operations (push, clone, merge) | E2E | Needs real git repos + K8s mount |
| Webhook delivery to external endpoints | E2E | Needs wiremock + real HTTP |

**Test count heuristics:**
- A CRUD endpoint needs ~5-8 integration tests (create, read, list, update, delete, validation errors, permission denied, not found)
- A status enum with N states needs N*(N-1) transition tests (valid + invalid)
- A parsing function needs: valid input, empty input, boundary values, malformed input = ~4-6 unit tests
- An auth-gated handler needs: success, unauthorized, forbidden, not-found (scope leak) = ~4 integration tests
- A new error variant needs: a test that triggers it + verifies the HTTP status code

**Critical test patterns to enforce:**
- Use `helpers::test_state(pool).await` for `(AppState, admin_token)` — never `admin_login()` (rate limit collision)
- Use dynamic queries (`sqlx::query()`) in test files, not compile-time macros (`sqlx::query!()`)
- Never FLUSHDB — all Valkey keys are UUID-scoped
- Pipeline tests must spawn `ExecutorGuard` and call `state.pipeline_notify.notify_one()`
- SSRF blocks localhost — insert webhook URLs directly into DB in tests
- E2E git repos under `/tmp/platform-e2e/`

**Output:** Complete TDD test tables for each PR, ready to insert into the plan file.

### Agent 5: API & Integration Impact Analyst

**Investigates:** Existing API routes, UI types, MCP servers, webhook dispatch
**Reviews:** Backward compatibility, integration surface, and rollout safety

**Investigation tasks:**
- Read `src/api/mod.rs` for the current route tree
- Read affected handler files to understand current request/response shapes
- Read `ui/src/lib/types.ts` for existing TypeScript types that may need updating
- Read `ui/src/lib/api.ts` for existing API client calls
- Check relevant MCP servers in `mcp/servers/` if the plan touches agent-facing APIs
- Search for callers of any functions the plan modifies or removes

**Review tasks:**
- Identify breaking API changes — any response shape change, removed field, or renamed endpoint
- Verify the PR breakdown supports safe incremental rollout (no big-bang deploys)
- Check that UI types stay in sync with backend response types
- Verify MCP servers are updated if agent-facing APIs change
- Check webhook event payloads for backward compatibility
- Assess whether existing integrations (CLI tools, external APIs) will break
- Verify the plan accounts for data migration if schema changes affect existing rows

**Output:** List of breaking changes, integration risks, and rollout concerns.

---

## Phase 3: Synthesis & Plan Update

Once all five agents return, synthesize their findings and **edit the plan file directly**.

### 3.1 Edit the plan file in-place

Open `plans/<plan-name>.md` and make direct improvements:

**Fix implementation issues:**
- Correct wrong file paths, type names, or function signatures
- Fix incorrect patterns (e.g., plan uses `require_permission` route layer where inline checks are needed)
- Add missing steps the plan overlooked
- Simplify over-engineered approaches
- Update migration SQL if schema issues were found

**Insert test strategy sections:**
For each PR section in the plan, insert (or replace the lightweight outline with) the detailed test strategy from Agent 4:
- "Tests to write FIRST" tables
- "Existing tests to UPDATE" table
- "Branch coverage checklist"
- "Tests NOT needed"
- "Coverage target: 100% of touched lines" verification table
- Test count totals

**Add test plan summary** at the end of the plan:

```markdown
## Test Plan Summary

### Coverage target: 100% of touched lines

Every new or modified line of code must be covered by at least one test
(unit, integration, or E2E). The test strategy above maps each code path
to a specific test. `review` and `finalize` will verify this with `just cov-unit`
/ `just cov-total`.

### New test counts by PR

| PR | Unit | Integration | E2E | Total |
|---|---|---|---|---|
| PR 1 | X | Y | Z | T |
| **Total** | **X** | **Y** | **Z** | **T** |

### Coverage goals by module

| Module | Current tests | After plan |
|---|---|---|
| `src/<module>` | X unit + Y integration | +N unit + M integration |
```

### 3.2 Append Plan Review Findings

After editing, append a findings section for issues that are observations/warnings rather than direct fixes:

```markdown
## Plan Review Findings

**Date:** {today}
**Status:** {APPROVED / APPROVED WITH CONCERNS / NEEDS REWORK}

### Codebase Reality Check
{Where plan assumptions were wrong — corrected in-place above}

### Remaining Concerns
{Issues that couldn't be fixed in the plan — warnings for dev}

### Simplification Opportunities
{Where the plan over-engineers or could consolidate}

### Security Notes
{Security observations for dev to keep in mind}
```

### 3.3 Synthesis rules

1. **Fix > Report** — if you can fix an issue by editing the plan, do it. Only append to "Findings" for things that can't be fixed in the plan itself.
2. **Deduplicate** — if multiple agents flag the same issue, fix it once
3. **Be actionable** — every finding must have a concrete next step
4. **Credit good work** — note well-done patterns (reinforces good habits)

---

## Usage Notes

- This skill works best on plans that follow the project's PR-based format (from the `plan` skill)
- **planReview is the quality gate between planning and implementation.** It both validates and enhances the plan.
- After planReview runs, the plan file should be complete: implementation design + test strategy + findings
- For very large plans (10+ PRs), consider running the review on 2-3 PRs at a time
- The orchestrator should spend ~1 minute reading the plan before launching agents
- Each agent should have `max_turns` of 15-20 to allow thorough investigation
- If an agent finds a critical issue early, it should still complete its full checklist

---

## Reflection & Improvement

After completing this skill's primary work, check if any triggers apply:

### Triggers
- [ ] Encountered a gotcha or crate quirk not documented in CLAUDE.md
- [ ] Found a missing instruction in THIS skill that caused confusion or rework
- [ ] The `plan` skill should have caught something that planReview had to fix
- [ ] A new pattern emerged that should be standardized
- [ ] An existing instruction was outdated or misleading
- [ ] An agent's checklist was missing a relevant check
- [ ] docs/ content (architecture.md, testing.md) no longer matches reality

### If any trigger fires, apply the minimum update:

| Target | When | What |
|---|---|---|
| `.claude/skills/planReview.md` | Missing agent checklist item or ambiguous instruction | Add/clarify |
| `.claude/skills/plan.md` | Plan skill should have caught something earlier | Add missing check to Phase 5 |
| `CLAUDE.md` | New convention, gotcha, or architecture rule | Add to relevant section |
| `docs/*.md` | Architecture/testing docs don't match reality | Update affected section |

### Rules
- Keep changes concise — 1-5 lines per update
- Check for duplicates before adding
- Update existing entries rather than adding contradictory new ones
- Do NOT commit these separately — they go with the skill's primary work
- Note what you changed in your summary to the user
