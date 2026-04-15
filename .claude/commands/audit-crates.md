# Skill: Crate Architecture Audit — Boundaries, Traits, Types & Dependency DAG

You are the **Crate Architecture Auditor**. You perform a deep structural review of every workspace crate under `crates/`, analysing module boundaries, trait design, type safety, dependency graphs, and public API surfaces. You produce a persistent report with concrete, actionable findings.

---

## Severity Levels

| Level | Meaning |
|-------|---------|
| **CRITICAL** | Circular dependency, unsound type safety, broken abstraction (leaks AppState / concrete types through trait boundaries) |
| **HIGH** | Over-coupled crates, trait design that blocks testability or future extraction, re-export of internal types |
| **MEDIUM** | Unnecessary pub visibility, missing newtypes, inconsistent error handling across crate boundary |
| **LOW** | Naming inconsistency, redundant re-exports, minor API surface bloat |
| **INFO** | Observation, positive pattern worth preserving, or suggestion for future refactor |

---

## Phase 0 — Inventory

Before launching agents, gather baseline data:

Use the Glob, Grep, and Read tools (NOT bash grep/find/jq) to gather this data:

1. **Workspace member list + dependency edges**: Read `Cargo.toml` at the workspace root to list members, then read each crate's `Cargo.toml` to extract workspace dependencies (lines containing `path =`).

2. **Lines of code per crate**: Use `Glob` with pattern `crates/**/*.rs` to list all Rust files, then count lines per crate directory using the Read tool on key files.

3. **Public item counts**: Use `Grep` with pattern `^pub ` and glob `crates/**/*.rs` to find public items. Count per crate from the file paths.

4. **Trait definitions**: Use `Grep` with pattern `^pub trait ` and glob `crates/**/*.rs`.

5. **Cross-crate imports**: Use `Grep` with pattern `use platform_` and glob `crates/**/*.rs` to see which crates import which.

Record the results — you will distribute them to each parallel agent.

---

## Phase 1 — Parallel Deep Analysis (7 agents)

Launch all 7 agents concurrently. Each agent gets the full inventory from Phase 0 plus its specific scope.

### Agent 1: Crate Boundary & Dependency DAG

**Scope**: All `Cargo.toml` files under `crates/`, all `lib.rs` / `mod.rs` files.

**Read every file**: `crates/*/Cargo.toml`, `crates/*/*/Cargo.toml`, every `lib.rs` under `crates/`.

**Checklist**:
- [ ] Draw the dependency DAG (text format). Flag any cycles or near-cycles (A→B→C→A via transitive deps)
- [ ] For each crate, list its direct workspace dependencies. Flag if a crate depends on more than 3 other workspace crates (coupling smell)
- [ ] Check for diamond dependencies (two paths to same crate with different features)
- [ ] Verify foundation crate (`platform-types`) has ZERO workspace dependencies
- [ ] Check that binary crates (`crates/bins/`) depend on lib crates, never the reverse
- [ ] Flag any lib-to-lib dependency that violates layering (e.g., `platform-auth` depending on `platform-deployer`)
- [ ] Check for unnecessary dependencies — crate listed in `[dependencies]` but no `use` of it in source
- [ ] Verify feature flags are used correctly (no `default = ["full"]` pulling in heavy optional deps)
- [ ] Check `dev-dependencies` — are test helpers duplicated across crates vs shared?

**Output format**: Dependency DAG (ASCII art or indented list), then findings as `[SEVERITY] crate — description\n  Fix: ...`

### Agent 2: Trait Design & Abstraction Quality

**Scope**: All trait definitions across `crates/`, focusing on `platform-types/src/traits.rs`, `platform-types/src/git_traits.rs`, and trait impls in each crate.

**Read every file**: `crates/foundation/platform-types/src/traits.rs`, `crates/foundation/platform-types/src/git_traits.rs`, and every file that contains `impl ... for` or `pub trait` under `crates/`.

**Checklist**:
- [ ] Catalog all traits: name, crate where defined, crate(s) where implemented, number of methods
- [ ] Flag "god traits" (>8 methods) — should they be split?
- [ ] Flag traits with only one implementation — is the abstraction premature?
- [ ] Check trait method signatures: do they take concrete types (PgPool, fred::Pool) or abstractions?
- [ ] Verify async trait patterns: are they using `async fn` in trait (Rust 1.75+) or `#[async_trait]`?
- [ ] Check for trait coherence issues — orphan rule violations, blanket impls blocking downstream
- [ ] Flag traits that require `Send + Sync + 'static` unnecessarily
- [ ] Check supertraits — are they minimal or over-constrained?
- [ ] Look for "trait proliferation" — too many small traits that should be one
- [ ] Verify trait objects (`dyn Trait`) vs generics (`impl Trait`) usage is appropriate
- [ ] Check if any trait could be replaced by a simple function pointer or closure

**Output format**: Trait catalog table, then findings as `[SEVERITY] trait_name (crate) — description\n  Fix: ...`

### Agent 3: Type System & Domain Modelling

**Scope**: All type definitions (`struct`, `enum`, newtype wrappers) across `crates/`.

**Read every file** that defines domain types: `error.rs`, `types.rs`, `state.rs`, `config.rs`, `lib.rs` across all crates.

**Checklist**:
- [ ] Catalog newtype wrappers (e.g., `UserId(Uuid)`) — are they used consistently or do raw `Uuid`s leak?
- [ ] Check state structs (`*State`) — do they hold owned resources or references? Are they Clone-able when they shouldn't be?
- [ ] Verify enum exhaustiveness — are `#[non_exhaustive]` annotations used where appropriate for cross-crate enums?
- [ ] Check for type duplication — same logical type defined in multiple crates (e.g., Config types)
- [ ] Flag `String` where an enum or newtype would be safer (e.g., status fields, permission names)
- [ ] Check `Option` vs dedicated "not set" variants — are optional fields modelled correctly?
- [ ] Verify `serde` derives — are `Serialize`/`Deserialize` on internal-only types? (leaks internal representation)
- [ ] Check for large structs (>10 fields) — should they be decomposed?
- [ ] Flag `pub` fields on structs that should use builder or constructor pattern
- [ ] Verify `Copy`/`Clone` derive correctness — are large types accidentally `Copy`?
- [ ] Check `PartialEq`/`Eq`/`Hash` consistency
- [ ] Look for `Any` or `Box<dyn Any>` usage — type erasure smell

**Output format**: Type inventory highlights, then findings as `[SEVERITY] TypeName (crate) — description\n  Fix: ...`

### Agent 4: Error Handling Chain

**Scope**: All `error.rs` files and error type conversions across `crates/`.

**Read every file**: Every `error.rs` under `crates/`, and grep for `impl From<...Error>` conversions.

**Checklist**:
- [ ] Catalog all error types: name, crate, variants, which types they convert From
- [ ] Draw the error conversion DAG — does error context get lost in conversions?
- [ ] Flag `#[error(transparent)]` chains longer than 2 — context is being swallowed
- [ ] Check for error types that expose internal implementation details (e.g., `sqlx::Error` in public API)
- [ ] Verify all crate-public errors implement `std::error::Error` + `Send + Sync`
- [ ] Check for string-typed errors (`String` variants) — should be structured
- [ ] Flag `anyhow::Error` in library crate public APIs (should use typed errors)
- [ ] Verify error-to-HTTP-status mapping consistency across crates
- [ ] Check for duplicate error variants across crates (e.g., `NotFound` in 5 different error types)
- [ ] Look for `unwrap()`/`expect()` in non-test code — potential panics at runtime
- [ ] Verify `?` operator chains preserve context (not silently dropping source errors)

**Output format**: Error DAG, then findings as `[SEVERITY] ErrorType (crate) — description\n  Fix: ...`

### Agent 5: Public API Surface & Re-exports

**Scope**: All `lib.rs` files and `pub` items across `crates/`.

**Read every file**: Every `lib.rs` under `crates/`, and sample the most pub-heavy source files.

**Checklist**:
- [ ] For each crate, list what's `pub` in `lib.rs` — is the API surface minimal?
- [ ] Flag `pub use` of internal implementation details (non-domain types, helper functions)
- [ ] Check for `pub(crate)` usage — are internal helpers properly scoped?
- [ ] Verify documentation on public items — are pub functions/types documented?
- [ ] Flag re-exports that expose transitive dependencies (e.g., `pub use sqlx::PgPool`)
- [ ] Check for `pub mod` that should be `pub(crate) mod` — internal modules exposed
- [ ] Verify binary crate (`crates/bins/`) public surfaces are minimal (bins shouldn't export much)
- [ ] Check consistency of re-export style — some crates use `pub use mod::*`, others explicit items
- [ ] Flag items that are `pub` but have no external consumers (dead public API)
- [ ] Verify `#[doc(hidden)]` is used appropriately for implementation details that must be pub for macro reasons

**Output format**: Per-crate API surface summary, then findings as `[SEVERITY] item (crate) — description\n  Fix: ...`

### Agent 6: State Management & Configuration

**Scope**: All `state.rs` and `config.rs` files, `AppState` composition in binary crates.

**Read every file**: Every `state.rs`, `config.rs` under `crates/`, plus `crates/bins/platform/src/state.rs` and `crates/bins/platform/src/main.rs`.

**Checklist**:
- [ ] Catalog all State structs — what resources does each hold? (pool, valkey, kube, config, channels)
- [ ] Check for state duplication — is the same resource (e.g., PgPool) held in multiple state types?
- [ ] Verify state composition in `platform` binary — does it compose lib states cleanly or duplicate?
- [ ] Flag `Arc<RwLock<...>>` in state — is the lock necessary or can it be lock-free (dashmap, atomics)?
- [ ] Check config types — are they shared or duplicated across crates?
- [ ] Verify config defaults — are they sensible? Are required fields actually required (not silently defaulting)?
- [ ] Flag state fields that are `pub` but should be accessor methods (encapsulation)
- [ ] Check for state initialization ordering issues — does crate A's state need crate B initialized first?
- [ ] Look for global/static state (`lazy_static`, `once_cell`, `static`) — should it be in state struct?
- [ ] Verify `Clone` on state — is it cheap (Arc-wrapped) or accidentally deep-cloning?

**Output format**: State composition diagram, then findings as `[SEVERITY] StateType (crate) — description\n  Fix: ...`

### Agent 7: Call Chain Analysis & Cross-Crate Coupling

**Scope**: Function call patterns across crate boundaries. Focus on the most coupled paths.

**Read**: `lib.rs` of each crate for public functions, then trace 3-5 key call chains:
1. HTTP request → auth → RBAC → DB (auth chain)
2. Pipeline trigger → executor → K8s pod → status update (pipeline chain)
3. OTLP ingest → channel → flush → Parquet → MinIO (observe chain)
4. Git push → hooks → pipeline trigger (git→pipeline chain)
5. Deploy reconcile → ops repo → render → apply (deploy chain)

**Checklist**:
- [ ] Trace each call chain: which crates are touched, what types cross boundaries?
- [ ] Flag chains that pass through >4 crates — coupling complexity
- [ ] Check for "data shuttle" anti-pattern — types created in one crate, passed through 2+ crates untouched, consumed in a 4th
- [ ] Verify function signatures at crate boundaries use trait types, not concrete implementations
- [ ] Flag synchronous blocking calls in async chains (`.block_on()`, `std::sync::Mutex` in async)
- [ ] Check for "god functions" (>100 lines) at crate boundaries
- [ ] Look for callback/closure patterns that could be simplified with traits
- [ ] Verify error propagation across crate boundaries preserves context
- [ ] Check for hidden coupling — crate A and B both depend on a shared database table without going through a shared type/trait
- [ ] Flag any `unsafe` code (should be zero per project rules)

**Output format**: Call chain diagrams (ASCII), then findings as `[SEVERITY] chain_name — description\n  Fix: ...`

---

## Phase 2 — Synthesis

After all 7 agents complete:

1. **Deduplicate** — merge findings flagged by multiple agents, keep highest severity
2. **Categorize** — group by theme:
   - Dependency structure
   - Trait & abstraction design
   - Type safety
   - Error handling
   - API surface
   - State management
   - Cross-crate coupling
3. **Prioritize** — CRITICAL/HIGH first, then MEDIUM, then LOW/INFO
4. **Count** — tally findings by severity and category

---

## Phase 3 — Write Report

Write the full report to `plans/crate-audit-<YYYY-MM-DD>.md` with this structure:

```markdown
# Crate Architecture Audit — <YYYY-MM-DD>

## Executive Summary

<2-3 sentences: overall health, biggest risk, top recommendation>

## Crate Inventory

| Crate | Type | LOC | Workspace Deps | Pub Items | Traits Defined | Traits Implemented |
|-------|------|-----|----------------|-----------|----------------|-------------------|
| ... | ... | ... | ... | ... | ... | ... |

## Dependency DAG

<ASCII dependency graph with arrows showing direction>

## Findings

### Critical (<count>)

<findings>

### High (<count>)

<findings>

### Medium (<count>)

<findings>

### Low (<count>)

<findings>

### Info / Positive Patterns (<count>)

<findings>

## Trait Catalog

| Trait | Defined In | Implemented By | Methods | Assessment |
|-------|-----------|----------------|---------|------------|
| ... | ... | ... | ... | ... |

## Error Type Map

| Error Type | Crate | Variants | Converts From | Converts To |
|-----------|-------|----------|---------------|-------------|
| ... | ... | ... | ... | ... |

## State Composition

<Diagram showing how binary crate(s) compose lib state types>

## Key Call Chains

<ASCII diagrams of the 5 traced call chains with crate boundaries marked>

## Recommendations

### Immediate (CRITICAL/HIGH fixes)

1. ...

### Near-term (MEDIUM fixes, next sprint)

1. ...

### Long-term (architectural improvements)

1. ...
```

---

## Phase 4 — Summary

Print a concise summary to the user:

```
Crate Audit complete. Report: plans/crate-audit-<date>.md

  Crates reviewed: <N>
  CRITICAL: <N>  HIGH: <N>  MEDIUM: <N>  LOW: <N>  INFO: <N>

  Top 3 findings:
  1. [SEVERITY] ...
  2. [SEVERITY] ...
  3. [SEVERITY] ...
```
