# Skill: Finalize — Triage, Fix, Verify Coverage, Branch & PR

**Description:** Takes a code review file (from the `review` skill), triages each finding, implements accepted fixes, verifies 100% test coverage on touched lines, runs the full test suite, commits to a feature branch, pushes, and creates a PR. This is the last step — it turns review feedback into a mergeable PR.

**Pipeline position:**
```
plan → planReview → dev → review → ★ finalize ★
```

---

## Step 0: Read Review File

Before triaging, read the persisted review artifact.

1. Look for `plans/<plan-name>_review.md` — this is the primary input
2. If it exists, read it completely
3. If it does NOT exist, check conversation context for review output
4. If neither exists, ask the user if they want to run the `review` skill first

The review file contains:
- Numbered findings (R1, R2, ...) with severity and suggested fixes
- Coverage analysis on touched lines
- Checklist pass/fail results

**Use the finding IDs (R1, R2, ...) when referencing findings throughout triage, fixes, and commit messages.**

---

## Step 1: Ingest Findings

Extract every finding from the review file into a working list. For each finding, capture:
- **ID**: R1, R2, ...
- **Severity**: CRITICAL / HIGH / MEDIUM / LOW
- **Domain**: Security / Rust Quality / Tests / Database
- **File + line**: where the issue is
- **Description**: what's wrong
- **Suggested fix**: how to fix it

Also note:
- **Coverage gaps**: uncovered touched lines from the coverage table
- **Checklist failures**: any FAIL entries

---

## Step 2: Triage Findings

Go through each finding and make a judgment call. Not every finding warrants a change — the reviewer may have missed context, or the suggested fix may introduce its own problems.

### Triage categories

| Decision | When to use | Action |
|---|---|---|
| **Accept** | Finding is correct and fix is straightforward | Implement the fix |
| **Accept (modified)** | Finding is valid but suggested fix isn't ideal | Implement a better fix |
| **Defer** | Valid issue but out of scope for this change | Note as follow-up, don't fix now |
| **Reject** | Finding is incorrect or based on misunderstanding | Skip with reasoning |

### Triage rules

1. **All CRITICAL findings must be addressed** — either Accept or Accept (modified). Never Defer or Reject a CRITICAL without explaining why to the user.

2. **All HIGH findings should be addressed** — Accept most. Only Defer if the fix is genuinely out of scope AND the existing behavior isn't dangerous.

3. **MEDIUM findings: use judgment** —
   - Accept if the fix is <10 lines and improves the code
   - Defer if the fix requires significant refactoring unrelated to the current feature
   - Reject if the finding is wrong (reviewer missed context)

4. **LOW findings: batch the trivial ones** —
   - Accept if the fix is a one-liner (rename, add a comment, fix formatting)
   - Skip if it's pure style preference with no functional impact

5. **Coverage gaps are HIGH** — every uncovered touched line needs a test unless it falls under documented exceptions (main.rs bootstrap, generated code, infra-failure paths).

### Read before deciding

**Before accepting or rejecting ANY finding, read the actual code at the referenced file:line.** The review file may have stale line numbers or misunderstand the context. Verify:
- Does the issue actually exist at that location?
- Is the suggested fix compatible with surrounding code?
- Would the fix break anything else?

### Output a triage table

Present the triage to the user before implementing:

```markdown
## Triage Summary

| # | Severity | Finding | Decision | Reasoning |
|---|---|---|---|---|
| R1 | CRITICAL | Missing auth on DELETE endpoint | Accept | Auth required per CLAUDE.md |
| R2 | HIGH | No input validation on title field | Accept | Field limit: 1-500 per convention |
| R3 | HIGH | Missing integration test for 404 | Accept | All handlers need not-found test |
| R4 | HIGH | Uncovered lines 112-114 | Accept | Add test for DB error path |
| R5 | MEDIUM | Handler exceeds 100 lines | Accept (modified) | Extract helper, but different than suggested |
| R6 | MEDIUM | Suggested refactor of error enum | Defer | Valid but out of scope |
| R7 | LOW | Variable naming style | Reject | Current name is clearer in context |

**Summary:** Accepting 5 fixes, deferring 1, rejecting 1.
```

Wait for user confirmation before proceeding to implementation. If the user disagrees with any triage decision, adjust.

---

## Step 3: Implement Fixes

Work through accepted findings in this order:

### 3.1 Fix order

1. **Security fixes first** — CRITICAL/HIGH security findings (missing auth, validation, SSRF)
2. **Bug fixes** — logic errors, incorrect error mappings, broken state machines
3. **Missing tests** — add tests for uncovered paths (including coverage gaps)
4. **Code quality** — handler extraction, tracing, naming, clippy compliance
5. **Database fixes** — migration corrections, query optimizations

### 3.2 Fix principles

- **Minimal changes** — fix the finding, don't refactor surrounding code
- **One concern per edit** — don't bundle unrelated fixes in a single file edit
- **Preserve existing tests** — if a fix changes behavior, update the affected tests
- **Follow project patterns** — use existing helpers and conventions from `CLAUDE.md`
- **Don't introduce new findings** — after each fix, mentally check if you've created a new issue

### 3.3 For each accepted finding

1. Read the file at the referenced location (verify the issue still exists)
2. Implement the fix using the Edit tool
3. If the fix changes an API response shape, update `ui/src/lib/types.ts`
4. If the fix changes a query, note that `.sqlx/` needs regeneration
5. Mark the finding as done in your working list

### 3.4 Adding missing tests

When the review identified missing test coverage (including uncovered touched lines):

**Unit tests:**
1. Add to the existing `#[cfg(test)] mod tests` block in the source file
2. Follow existing test naming pattern in that file
3. Test the specific branch/edge case the review identified

**Integration tests:**
1. Add to the existing `tests/<module>_integration.rs` file
2. Use `#[sqlx::test(migrations = "./migrations")]`
3. Use `helpers::test_state(pool).await` for state + token
4. Use dynamic `sqlx::query()` — never `sqlx::query!()` in tests

**E2E tests:**
1. Add to the existing `tests/e2e_<module>.rs` file
2. Mark with `#[ignore]`
3. Use `e2e_helpers::e2e_state(pool).await`

### 3.5 Track deferred items

For each deferred finding, note it clearly for the PR description:

```markdown
### Deferred Items
- [ ] R6: Refactor error enum to separate DB errors from domain errors
- [ ] R9: Add rate limiting to new token refresh endpoint
```

---

## Step 4: Run Full Test Suite

After all fixes are implemented, run the complete quality gate. **Do not skip any tier.**

### 4.1 Quick check first

```bash
just test-unit    # fast (~1s) — catches compilation errors and unit test regressions
```

If unit tests fail, fix immediately before proceeding.

### 4.2 Lint and format

```bash
just fmt          # auto-format
just lint         # clippy with -D warnings
```

If clippy finds issues, fix them. Common post-review clippy hits:
- `too_many_lines` after adding tracing/validation to a handler
- `collapsible_if` from adding nested conditions
- `unused_imports` after extracting helpers

### 4.3 Full CI

```bash
just ci-full      # fmt + lint + deny + test-unit + test-integration + test-e2e + build
```

If `ci-full` is too slow for iteration, run tiers relevant to the fixes:

```bash
just test-unit           # always
just test-integration    # if API/DB/auth fixes
just test-e2e            # if K8s/pipeline/deployer/git/webhook fixes
```

### 4.4 Database cache

If any `sqlx::query!` macros were modified:

```bash
just db-prepare    # regenerate .sqlx/ offline cache
just db-check      # verify cache is up to date
```

### 4.5 Handle failures

If tests fail after implementing review fixes:

1. **Read the failure output** — understand what broke
2. **Determine if the fix caused the failure** — or if it was a pre-existing flake
3. **Fix the root cause** — don't revert the review fix unless it was wrong
4. **Re-run the failing tier** — verify the fix works
5. **Run full suite again** — ensure no cascading breakage

**Never skip a failing test or mark it `#[ignore]` to get green.** Fix it or ask the user for guidance.

---

## Step 4.5: Verify Coverage on Touched Lines

Every new or modified line must be covered by at least one test (unit, integration, or E2E).

### 4.5.1 Identify touched files

```bash
# Compare current state to main (or base branch)
git diff --name-only main...HEAD -- 'src/**/*.rs'
# If working on main with uncommitted changes:
git diff --name-only HEAD -- 'src/**/*.rs'
```

### 4.5.2 Run coverage

```bash
# Minimum: unit coverage (fast, no infra needed)
just cov-unit
# Output: coverage-unit.lcov

# Recommended: combined coverage (needs Kind cluster)
just cov-total
```

### 4.5.3 Check coverage for touched files

For each file from git diff:

1. Parse the lcov file (e.g., `coverage-unit.lcov`)
2. Find `SF:<absolute-path-to-file>` section
3. For each line shown in `git diff` as added (`+`) or modified:
   - Find `DA:<line-number>,<execution-count>`
   - If execution-count is 0, the line is **UNCOVERED**

### 4.5.4 Handle uncovered lines

If any touched line is uncovered:

1. **Can you add a test?** — Add a unit or integration test that exercises the uncovered path
2. **Is it unreachable in tests?** (e.g., K8s-only code path) — Document why and ensure E2E covers it
3. **Re-run coverage** after adding tests to confirm the gap is closed

### 4.5.5 Coverage target

**100% of touched lines** must be covered by at least one test tier.

Exceptions (document explicitly in the PR description):
- `main.rs` bootstrap wiring (covered by E2E only)
- Error paths that require real infrastructure failures (document the gap)
- Generated code (`proto.rs`, `ui.rs`)

If you cannot reach 100% on touched lines, document the gaps and rationale in the PR description.

---

## Step 5: Final Self-Review

Before committing, do a rapid self-check on all changes made in Steps 3-4:

- [ ] No `.unwrap()` introduced in production code
- [ ] No sensitive data in new log statements or audit entries
- [ ] All new tests follow project patterns (test_state, dynamic queries, no FLUSHDB)
- [ ] No new `#[allow(dead_code)]` added
- [ ] Handler line counts still under 100
- [ ] Import cleanup — no unused imports from removed code

Quick scan: re-read the diff of your changes.

```bash
git diff --stat           # what changed
git diff                  # review your own changes
```

---

## Step 6: Branch, Commit & PR

Once all tests pass, coverage is verified, and self-review is clean:

### 6.1 Create feature branch (if on main)

```bash
# Check current branch
git branch --show-current

# If on main, create feature branch
git checkout -b feat/<plan-name>
# Examples:
#   feat/32-permission-redesign
#   fix/auth-token-expiry
#   refactor/error-handling
```

Branch naming convention:
- `feat/<plan-name>` — for new features from plans
- `fix/<plan-name>` — for bug fix plans
- `refactor/<plan-name>` — for refactoring plans

If already on a feature branch (from a previous dev session), stay on it.

### 6.2 Stage files

Stage specifically — never `git add .` or `git add -A`:

```bash
git add src/api/handler.rs src/validation.rs tests/handler_integration.rs
```

**Must include:**
- All changed source files (`src/**/*.rs`)
- Changed test files (`tests/**/*.rs`)
- New/changed migrations (`migrations/*.sql`)
- `.sqlx/` cache files (if queries changed)
- UI type changes (`ui/src/lib/types.ts`, `ui/src/lib/generated/`)
- MCP server changes (`mcp/servers/*.js`)
- Plan file updates (`plans/<name>.md`) — progress checkboxes, deviations
- Review file (`plans/<name>_review.md`)
- Skill file updates (`.claude/skills/*.md`) if reflection made changes
- `CLAUDE.md` / `docs/*.md` updates if reflection made changes

**Must exclude:**
- `.env` or credentials files
- Coverage reports (`*.lcov`, `coverage-html/`)
- Build artifacts (`target/`)
- `node_modules/`
- Unrelated changes from other work

### 6.3 Craft commit message

```bash
git commit -m "$(cat <<'EOF'
feat: implement <plan-name> — <1-line summary>

<2-3 line description of what was implemented>

Tests: N unit, M integration, P E2E
Coverage: 100% on touched lines

Review findings addressed:
- R1: <brief description> — fixed
- R3: <brief description> — fixed
- R6: <brief description> — deferred (reason)

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>
EOF
)"
```

Commit prefix rules:
- `feat:` — new feature (from a plan)
- `fix:` — bug fix or review finding fixes
- `refactor:` — code restructuring
- `test:` — test-only changes
- `docs:` — documentation only

### 6.4 Push to remote

```bash
git push -u origin <branch-name>
```

### 6.5 Create PR

```bash
gh pr create --title "<type>: <short title under 70 chars>" --body "$(cat <<'EOF'
## Summary
- <bullet 1: what problem this solves>
- <bullet 2: key change>
- <bullet 3: key change>

## Changes
- <specific file/module change 1>
- <specific file/module change 2>
- ...

## Test Plan
- Unit: N new/modified tests
- Integration: M new/modified tests
- E2E: P new/modified tests
- Coverage: 100% on touched lines (verified via cov-unit/cov-total)

## Review Findings Addressed
- R1: <description> — fixed
- R3: <description> — fixed

## Deferred Items
- [ ] R6: <description> — <reason>

---
Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

### 6.6 Update plan with PR link

After creating the PR:

1. Open `plans/<plan-name>.md`
2. Add/update the implementation status section at the top:
   ```markdown
   ## Implementation Status
   - **Branch:** `feat/<plan-name>`
   - **PR:** #<number> (<url>)
   - **Status:** In Review
   ```
3. Stage and amend the commit:
   ```bash
   git add plans/<plan-name>.md
   git commit --amend --no-edit
   git push --force-with-lease
   ```

### 6.7 Post-PR verification

```bash
git status                    # should be clean
git log --oneline -3          # verify commit
```

---

## Step 7: Report

Present a summary to the user:

```markdown
## Finalize Complete

### Fixes Applied (N of M findings)

| # | Finding | Fix applied |
|---|---|---|
| R1 | Missing auth on DELETE | Added AuthUser extractor + require_project_write check |
| R2 | No validation on title | Added check_length("title", &body.title, 1, 500) |
| R3 | Missing 404 test | Added test_unauthorized_returns_404 |
| R4 | Uncovered lines 112-114 | Added test_handler_db_error |
| ... | ... | ... |

### Test Results
- Unit: X passed
- Integration: Y passed
- E2E: Z passed
- Lint: clean
- Build: success
- Coverage: 100% on touched lines

### Deferred Items
- [ ] R6: Refactor error enum (out of scope) — consider for next PR

### Branch & PR
- Branch: `feat/<plan-name>`
- PR: #<number> — <url>
- Commit: `<sha>` <message>
```

---

## Reflection & Improvement

After completing this skill's primary work, check if any triggers apply:

### Triggers
- [ ] Encountered a gotcha or crate quirk not documented in CLAUDE.md
- [ ] Found a missing instruction in THIS skill that caused confusion or rework
- [ ] Coverage verification missed a gap that should have been caught
- [ ] The review file format was hard to parse or missing information
- [ ] The PR template was missing something the reviewer needed
- [ ] Branch naming convention was unclear
- [ ] A previous skill should have caught something earlier
- [ ] docs/ content (architecture.md, testing.md) no longer matches reality

### If any trigger fires, apply the minimum update:

| Target | When | What |
|---|---|---|
| `.claude/skills/finalize.md` | Missing step or ambiguous instruction | Add/clarify |
| `.claude/skills/review.md` | Review file format needs adjustment | Update Phase 3.5 spec |
| `.claude/skills/dev.md` | Dev should have caught something before review | Add to Step 7 checklist |
| `CLAUDE.md` | New convention, gotcha, or architecture rule | Add to relevant section |
| `docs/*.md` | Architecture/testing docs don't match reality | Update affected section |

### Rules
- Keep changes concise — 1-5 lines per update
- Check for duplicates before adding
- Update existing entries rather than adding contradictory new ones
- These updates go into the SAME commit as the primary work (already staged in Step 6.2)
- Note what you changed in your summary to the user
