# Skill: Implementation Plan Writer

**Description:** Creates detailed, PR-based implementation plans grounded in the actual codebase. Plans focus on **implementation design**: architecture, code structure, PR decomposition, migration SQL, and file changes. Test strategy is kept as a lightweight outline — the `planReview` skill owns detailed TDD test tables and coverage targets.

**Pipeline position:**
```
★ plan ★ → planReview → dev → review → finalize
```

## Core Principles

1. **Code-first, not assumption-first** — Read every file you reference before writing about it. Never guess at types, function signatures, table schemas, or module structure.
2. **Atomic PRs** — Each PR must be independently mergeable and leave the codebase in a working state. Dependencies between PRs flow forward, never backward.
3. **Security by default** — Every new endpoint gets auth, permission checks, input validation, and audit logging. SSRF protection on outbound URLs. Rate limiting on brute-forceable paths.
4. **Implementation focus** — This skill designs WHAT to build and HOW to structure it. `planReview` handles the detailed test strategy and validates the approach.
5. **Cascading impact awareness** — Every PR must identify existing code/tests affected by the change.

---

## Phase 1: Deep Codebase Investigation

Before writing a single line of the plan, investigate the codebase. This is the most important phase — plans fail when they're written against imagined code.

### 1.1 Understand the requirement

1. Read the user's feature description / issue / conversation context
2. If a draft plan exists in `plans/`, read it end-to-end
3. Read `docs/architecture.md` for system context
4. Read `CLAUDE.md` for all coding patterns, security rules, and conventions
5. Identify the affected domain(s): auth, rbac, api, git, pipeline, deployer, agent, observe, secrets, notify, store, ui

### 1.2 Read affected source files

For each affected module, read these files (use the Explore agent or direct reads):

**Rust source:**
- `src/<module>/mod.rs` — public API, re-exports, types
- `src/<module>/error.rs` — error enum variants
- `src/api/<relevant>.rs` — existing handlers, request/response types
- `src/api/mod.rs` — current route tree (to understand where new routes fit)
- `src/api/helpers.rs` — shared permission check helpers
- `src/error.rs` — `ApiError` type and existing `From` conversions
- `src/config.rs` — config struct (if new env vars needed)
- `src/validation.rs` — existing validation helpers (to reuse, not reinvent)

**Database:**
- `migrations/` — existing migrations for affected tables (read the UP and DOWN SQL)
- `plans/unified-platform.md` — canonical schema reference
- `.sqlx/` — verify offline cache exists for affected queries

**Tests (for blast radius):**
- `tests/helpers/mod.rs` — integration test helpers
- `tests/e2e_helpers/mod.rs` — E2E test helpers
- `tests/<relevant>_integration.rs` — existing integration tests for affected modules
- `tests/e2e_<relevant>.rs` — existing E2E tests

**UI (if API changes):**
- `ui/src/lib/types.ts` — existing TypeScript types
- `ui/src/lib/api.ts` — existing API client
- `ui/src/pages/<relevant>.tsx` — affected page components

**MCP (if agent-facing APIs change):**
- `mcp/servers/platform-<relevant>.js` — affected MCP server
- `mcp/lib/client.js` — shared client

### 1.3 Map dependencies and blast radius

Before designing the solution, answer:

1. **What tables are involved?** Read their CREATE TABLE migrations. Note constraints, indexes, foreign keys.
2. **What types exist?** Read the Rust structs/enums for affected domain objects. Note derive macros, sqlx attributes.
3. **What permission checks exist?** Search for `has_permission`, `require_project_read`, `require_project_write` in affected handlers.
4. **What tests exist?** Count existing tests per file. Identify tests that will break from your changes.
5. **What background tasks are affected?** Check if changes touch pipeline executor, deployer reconciler, observe flush, session cleanup.
6. **What's the current `AppState`?** Read `src/store/mod.rs` or wherever AppState is defined. Note all fields — adding a field breaks all test helpers.

Record your findings. You will reference them in the plan.

---

## Phase 2: Solution Design

### 2.1 Decompose into PRs

Break the feature into PRs ordered by dependency. Rules:

- **Migrations first** — schema changes in the earliest PR they're needed
- **Types before logic** — new enums, structs, error variants before handlers that use them
- **Infrastructure before consumers** — new helpers/services before the API handlers that call them
- **Tests cascade** — if PR 2 changes a helper, all tests using that helper are "updated" in PR 2, not later
- **Max 3-4 PRs** for most features. Only exceed this for genuinely large architectural changes.

Each PR gets a clear scope statement: what it does, what it doesn't do.

### 2.2 Design each PR

For each PR, design (in order):

1. **Migration SQL** (if any) — write the full UP and DOWN SQL. Verify:
   - Version number follows `YYYYMMDDHHMMSS_name` convention (no underscores in version prefix)
   - DOWN undoes UP completely
   - No data loss without explicit backfill
   - No full-table locks on hot tables
   - Appropriate defaults, NOT NULL constraints, indexes
   - Foreign keys reference existing tables (verify they exist)

2. **Types** — new/modified Rust types:
   - Status enums with `can_transition_to()` where applicable
   - Request/Response structs (separate from DB model structs)
   - Error enum variants with `From<X> for ApiError` mappings
   - Newtype wrappers if needed (project uses raw `Uuid` currently)

3. **Logic** — business rules, services, helpers:
   - Permission check approach (inline vs helper function)
   - Audit logging for all mutations
   - Input validation using `crate::validation::*`
   - SSRF protection for outbound URLs
   - Rate limiting for brute-forceable endpoints

4. **API handlers** — endpoint design:
   - Route paths (verify they don't conflict with existing routes in `src/api/mod.rs`)
   - Handler signature: `State, AuthUser, Path, Query, Json` ordering
   - Keep under 100 lines (extract helpers)
   - Max 7 parameters (use param structs)

5. **Test outline** — lightweight summary of what needs testing (see Phase 3). `planReview` will expand this into detailed TDD test tables.

6. **File change table** — every file touched, with a description of the change

### 2.3 Cross-cutting concerns checklist

For each PR, explicitly address:

- [ ] Auth: every handler has `AuthUser` extractor
- [ ] Permissions: read endpoints check project-level access; write endpoints check write access
- [ ] Private resources return 404 (not 403) to avoid leaking existence
- [ ] Input validation at handler boundary (`crate::validation::*`)
- [ ] Audit logging for mutations (never log secrets/URLs in audit detail)
- [ ] Webhook dispatch for events external systems care about
- [ ] `tracing::instrument` on all async functions with side effects
- [ ] No `.unwrap()` in production code
- [ ] No handler exceeds 100 lines
- [ ] No function exceeds 7 parameters
- [ ] Sensitive data never logged (passwords, tokens, secrets, webhook URLs)
- [ ] AppState changes → test helper updates in BOTH `tests/helpers/mod.rs` AND `tests/e2e_helpers/mod.rs`
- [ ] `.sqlx/` offline cache regeneration after query changes
- [ ] UI type updates if API response shapes change
- [ ] MCP server updates if agent-facing APIs change

---

## Phase 3: Test Outline

For each PR, provide a lightweight test outline. This is NOT the full TDD test plan — `planReview` owns that. The outline helps planReview understand your intent.

### Per-PR test outline format

```markdown
### Test Outline — PR N

**New behaviors to test:**
- {Behavior 1} — likely tier: {unit/integration/e2e}
- {Behavior 2} — likely tier: {unit/integration/e2e}

**Error paths to test:**
- {Error path 1} — likely tier: {unit/integration}

**Existing tests affected:**
- `tests/<file>.rs` — {what changes and why}

**Estimated test count:** ~X unit + Y integration + Z E2E
```

### Test tier selection rules (for the outline)

| What you're testing | Tier | Why |
|---|---|---|
| Pure functions, enum parsing, state machine transitions, validation | Unit | No I/O needed |
| Encryption round-trips, HMAC verification | Unit | Crypto is pure logic |
| API handler wiring, CRUD, auth flows | Integration | Needs DB + HTTP stack |
| Cross-module interactions (webhook fires after issue create) | Integration | Needs multiple services |
| K8s pod execution, git operations, webhook delivery | E2E | Needs real K8s |

---

## Phase 4: Write the Plan Document

### Document structure

```markdown
# Plan {N}: {Feature Name}

## Context
- 2-3 paragraphs: what problem this solves, why now, what the current state is
- Reference existing code/patterns being extended

## Design Principles
- 3-5 bullet points: core design decisions and why

---

## PR 1: {Title}

{1-2 sentence scope statement}

- [ ] Types & errors defined
- [ ] Migration applied
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### Migration: `{version}_{name}`

**Up:**
```sql
{full SQL}
```

**Down:**
```sql
{full SQL}
```

### Code Changes

| File | Change |
|---|---|
| `src/...` | Description |

### Test Outline — PR 1

{Lightweight test outline per Phase 3}

### Verification
- {Concrete checks that PR works}

---

## PR 2: {Title}
{Same structure}
```

### Writing standards

- **Be specific** — file paths, function names, type names, SQL column names. Never say "update the relevant file."
- **Show the SQL** — full migration SQL, not pseudocode. Reviewers need to verify constraints and rollback safety.
- **Show the types** — Rust struct/enum definitions with derive macros. Reviewers need to verify sqlx/serde attributes.
- **Quantify** — approximate test counts per tier per PR (planReview will refine).
- **Reference existing patterns** — "Same pattern as `src/api/webhooks.rs::fire_webhooks`" with the file path.
- **Call out gotchas** — known crate issues (rand 0.10, fred Pool, axum 0.8), clippy lints, sqlx offline cache.
- **Progress checkboxes** — every PR section must have the 6-item progress checklist for `dev` to track.

---

## Phase 5: Self-Review

Before presenting the plan, verify:

### Codebase accuracy
- [ ] Every file path referenced exists (verified via Glob/Read)
- [ ] Every type/function referenced exists with the stated signature
- [ ] Every table referenced exists with the stated columns
- [ ] Migration SQL references only tables/columns that exist (or are created in an earlier PR)
- [ ] Import paths are correct for the project's module structure
- [ ] No references to deprecated patterns or removed code

### Completeness
- [ ] Every new handler has auth + permissions + validation + audit in the plan
- [ ] Every mutation has a corresponding webhook event (if applicable)
- [ ] Every new AppState field is reflected in test helper updates
- [ ] Every migration has both UP and DOWN
- [ ] Cascading test/code updates are identified
- [ ] Each PR section has the 6-item progress checklist

### Feasibility
- [ ] No PR depends on a later PR
- [ ] Each PR is independently testable
- [ ] The plan doesn't require features that don't exist yet (unless noted)
- [ ] Handler line counts are realistic (<100 lines each)

### Security
- [ ] No new endpoint without auth
- [ ] No user-supplied URL without SSRF validation
- [ ] No brute-forceable endpoint without rate limiting
- [ ] Private resources return 404 (not 403)
- [ ] Secrets/tokens/passwords never in logs or audit detail
- [ ] Webhook URLs never logged

---

## Parallel Agent Strategy

For large plans (3+ PRs), use parallel agents to investigate different domains simultaneously:

### Agent 1: Schema & Data
- Read existing migrations for affected tables
- Read `plans/unified-platform.md` for canonical schema
- Search for all `sqlx::query!` calls referencing affected tables
- Identify foreign keys, indexes, constraints

### Agent 2: Existing Code & Types
- Read affected `src/` modules
- Map existing types, traits, error enums
- Identify reusable patterns and helpers
- Check `Cargo.toml` for dependency versions

### Agent 3: Test Infrastructure & Blast Radius
- Read `tests/helpers/mod.rs` and `tests/e2e_helpers/mod.rs`
- Count existing tests per affected module
- Identify tests that will break from proposed changes
- Read 1-2 existing test files as format reference

### Agent 4: UI & MCP Surface
- Read `ui/src/lib/types.ts` and `ui/src/lib/api.ts`
- Read affected MCP servers
- Identify breaking API changes
- Map UI components that need updates

After agents return, synthesize findings into the plan. Reference specific file paths, line numbers, and code snippets from the investigation.

---

## Usage Notes

- This skill produces plans in `plans/{N}-{feature-name}.md`
- For small features (1-2 PRs, <10 tests), skip the parallel agent strategy and investigate directly
- For bug fixes, a plan may be overkill — use the `dev` skill instead
- **plan focuses on WHAT to build. planReview focuses on HOW to test it and validates the approach.**
- After planReview, the plan file becomes a living document. planReview adds the test strategy, dev updates it during implementation.
- After the plan is written, use the `planReview` skill to validate and enhance it before implementation

---

## Reflection & Improvement

After completing this skill's primary work, check if any triggers apply:

### Triggers
- [ ] Encountered a gotcha or crate quirk not documented in CLAUDE.md
- [ ] Found a missing instruction in THIS skill that caused confusion or rework
- [ ] Discovered a missing instruction in a PREVIOUS skill that should have caught something
- [ ] A new pattern emerged that should be standardized
- [ ] An existing instruction was outdated or misleading
- [ ] docs/ content (architecture.md, testing.md) no longer matches reality
- [ ] README.md no longer reflects current architecture
- [ ] Plan format caused confusion for the dev or planReview skill

### If any trigger fires, apply the minimum update:

| Target | When | What |
|---|---|---|
| `.claude/skills/plan.md` | Missing step or ambiguous instruction | Add/clarify |
| `CLAUDE.md` | New convention, gotcha, or architecture rule | Add to relevant section |
| `docs/*.md` | Architecture/testing docs don't match reality | Update affected section |
| `README.md` | Significant capability change | Update description |

### Rules
- Keep changes concise — 1-5 lines per update
- Check for duplicates before adding
- Update existing entries rather than adding contradictory new ones
- Do NOT commit these separately — they go with the skill's primary work
- Note what you changed in your summary to the user
