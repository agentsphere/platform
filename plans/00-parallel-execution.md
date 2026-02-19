# 00 — Parallel Execution Guide

## Plan Index

| # | Plan | Estimated LOC | Dependencies | Risk |
|---|------|--------------|-------------|------|
| 01 | [Foundation](01-foundation.md) | ~1,200 Rust | None (start here) | Low |
| 02 | [Identity & Auth](02-identity-auth.md) | ~1,500 Rust | 01 | Medium — RBAC delegation is subtle |
| 03 | [Git Server](03-git-server.md) | ~1,400 Rust | 01, 02 | Medium — smart HTTP streaming |
| 04 | [Project Management](04-project-mgmt.md) | ~1,000 Rust | 01, 02 | Low |
| 05 | [Build Engine](05-build-engine.md) | ~1,400 Rust | 01, 02 | High — K8s pod lifecycle, log streaming |
| 06 | [Deployer](06-deployer.md) | ~800 Rust | 01, 02 | Medium — reconciler correctness |
| 07 | [Agent Orchestration](07-agent-orchestration.md) | ~800 Rust | 01, 02 | High — pod exec/attach, WebSocket bridge |
| 08 | [Observability](08-observability.md) | ~2,200 Rust | 01, 02 | High — OTLP proto, Parquet, query engine |
| 09 | [Secrets & Notifications](09-secrets-notify.md) | ~1,000 Rust | 01, 02 | Low |
| 10 | [Web UI](10-web-ui.md) | ~2,500 TS | 01, 02 + APIs | Low |
| — | Integration testing + hardening | ~1,200 Rust | All | — |
| **Total** | | **~15,000** | | |

---

## Dependency Graph

```
                    ┌──────────────┐
                    │ 01-foundation│
                    │ store, config│
                    │ migrations   │
                    └──────┬───────┘
                           │
                    ┌──────▼───────┐
                    │02-identity   │
                    │auth + rbac   │
                    └──────┬───────┘
                           │
          ┌────────────────┼────────────────┐
          │                │                │
     ┌────▼────┐     ┌────▼────┐     ┌────▼────┐
     │   WAVE 1 (parallel)      │     │         │
     │                          │     │         │
  ┌──▼──┐ ┌──▼──┐ ┌──▼──┐ ┌──▼──┐ ┌──▼──┐ ┌──▼──┐ ┌──▼──┐
  │ 03  │ │ 04  │ │ 05  │ │ 06  │ │ 07  │ │ 08  │ │ 09  │
  │ git │ │proj │ │build│ │depl │ │agent│ │obsrv│ │secr │
  │     │ │mgmt │ │eng  │ │oyer │ │orch │ │     │ │noti │
  └──┬──┘ └──┬──┘ └──┬──┘ └──┬──┘ └──┬──┘ └──┬──┘ └──┬──┘
     │       │       │       │       │       │       │
     └───────┴───────┴───┬───┴───────┴───────┴───────┘
                         │
                  ┌──────▼───────┐
                  │ Integration  │
                  │ wiring       │
                  └──────┬───────┘
                         │
                  ┌──────▼───────┐
                  │   10-web-ui  │
                  │  (incremental│
                  │   as APIs    │
                  │   land)      │
                  └──────┬───────┘
                         │
                  ┌──────▼───────┐
                  │  E2E testing │
                  │  + hardening │
                  └──────────────┘
```

---

## Execution Phases

### Phase 1: Sequential Foundation (must complete in order)
```
01-foundation  →  02-identity-auth
```
These two are the sequential bottleneck. Every module depends on `AppState` (from 01) and `AuthUser`/`RequirePermission` middleware (from 02). Ship these first.

**Critical path risk**: If RBAC delegation (02) takes longer than expected, it delays everything. Mitigation: implement basic role-based auth first (no delegation), gate the parallel wave on that, and backfill delegation as a follow-up before integration wiring.

### Phase 2: Parallel Wave (all 7 modules concurrently)
```
┌─────────────────────────────────────────────┐
│ 03-git    04-project  05-build  06-deployer │
│ 07-agent  08-observe  09-secrets-notify     │
└─────────────────────────────────────────────┘
```
These 7 modules have **no compile-time dependencies on each other**. They all depend only on:
- `AppState` (pool, valkey, minio, kube, config)
- Auth middleware (AuthUser, RequirePermission)
- Shared DB tables (accessed via sqlx queries, not Rust type imports)

**Parallel execution strategies**:
- **Multiple Claude Code sessions**: assign each module to a separate agent session
- **Single developer**: work on 2-3 at a time, context-switching by module
- **Smallest first**: knock out 06 (800 LOC), 07 (800 LOC), 09 (1,000 LOC) quickly, then tackle 08 (2,200 LOC)

### Phase 2.5: Integration Wiring
Wire cross-module calls (see [Integration Wiring](#integration-wiring-after-parallel-wave) below). This is a distinct step — don't let it get absorbed into "hardening." Each wiring point is a small, testable change.

### Phase 3: UI (incremental, can overlap with Phase 2)
```
10-web-ui (start login + layout as soon as 02 ships, add pages as APIs land)
```
UI work can start as soon as auth API exists and grow incrementally. The UI session (Session F) can begin during Phase 2, adding pages as each module's API stabilizes.

### Phase 4: E2E Testing & Hardening
Cross-module integration tests, E2E flows, performance tuning, edge cases.

---

## Merge Conflicts & Shared Files

When multiple agents work in parallel, they will all touch a few shared files. Define conventions up front to avoid conflicts:

| Shared File | Convention |
|------------|-----------|
| `src/main.rs` | Each module adds its router via a `pub fn router() -> Router<AppState>` function. One final merge pass nests all routers. During parallel dev, each module can be tested independently with its own axum `Router`. |
| `src/lib.rs` | Each module adds one `pub mod <name>;` line. Append-only — merge conflicts are trivial. |
| `migrations/` | **All core migrations are created upfront in 01-foundation** (21 files). **IMPORTANT**: sqlx version numbers must be a single `_`-delimited segment — use `20260220010001_name.up.sql` (no underscore between date and sequence), NOT `20260220_010001_name.up.sql`. If a module needs additional migrations later (e.g., adding a column), use the module's timestamp range below. |
| `Cargo.toml` | Each module may add dependencies. Use a single merge pass after the parallel wave to reconcile. |

**Migration timestamp ranges** (for any additional migrations during parallel dev — core schema is already in `0100xx`).
Format: `{YYYYMMDDSSSSSS}_name.up.sql` — the version is everything before the first `_`. No underscores within the version number.

| Module | Timestamp prefix | Example filename | Notes |
|--------|-----------------|------------------|-------|
| 01-foundation | `202602200100xx` | `20260220010001_utility.up.sql` | Core schema (21 files, created upfront) |
| 02-identity | `202602200200xx` | `20260220020001_some_change.up.sql` | Only if auth needs schema changes |
| 03-git | `202602200300xx` | | |
| 04-project | `202602200400xx` | | |
| 05-build | `202602200500xx` | | |
| 06-deployer | `202602200600xx` | | |
| 07-agent | `202602200700xx` | | |
| 08-observe | `202602200800xx` | | |
| 09-secrets-notify | `202602200900xx` | | |

---

## Shared Trait & Type Contracts

Modules are decoupled at compile-time but must agree on certain patterns. Define these in Phase 1 so all parallel sessions share the same interface:

```rust
// Every module exposes a router constructor — defined by convention, not a trait
pub fn router() -> axum::Router<AppState> { ... }

// Background tasks follow this signature
pub async fn run(state: AppState, shutdown: tokio::sync::watch::Receiver<()>) { ... }
```

**Enum conventions**: All status enums (`PipelineStatus`, `DeployDesiredStatus`, `AgentSessionStatus`, etc.) live in their own module, not in a shared types crate. Each module owns its own types. Cross-module references go through the DB (text columns), not Rust imports.

---

## Runtime Integration Points (not compile-time)

These modules interact at runtime through the database or direct function calls, not through Rust type imports:

| Producer | Consumer | Mechanism | Direction |
|----------|----------|-----------|-----------|
| 03-git (push) | 05-build (trigger) | `pipelines` table row insert | DB |
| 04-project (MR merged) | 05-build (trigger) | `pipelines` table row insert | DB |
| 05-build (success) | 06-deployer (reconcile) | `deployments` table row update | DB |
| 05-build (done) | 09-notify (dispatch) | `notify::dispatch::on_build_complete()` | fn call |
| 06-deployer (status change) | 09-notify (dispatch) | `notify::dispatch::on_deploy_status()` | fn call |
| 07-agent (session done) | 09-notify (dispatch) | `notify::dispatch::on_agent_completed()` | fn call |
| 08-observe (alert fires) | 09-notify (dispatch) | `notify::dispatch::on_alert_firing()` | fn call |
| 09-secrets (resolve) | 05-build (env injection) | `secrets::engine::resolve_secrets_for_env()` | fn call |
| 09-secrets (resolve) | 07-agent (API keys) | `secrets::engine::resolve_secrets_for_env()` | fn call |

**Implication**: During parallel development, each module can stub these integration points and wire them later. For example:
- Build engine can trigger on API call only (wire git hook trigger later)
- Deployer reads `deployments` table regardless of who writes to it
- Notification dispatch functions exist but callers wire in last
- Secrets engine exposes its API; consumers call it only during integration wiring

---

## Integration Wiring (after parallel wave)

Once all modules compile independently, wire the cross-module calls. Each item is a small, focused PR:

1. **Git → Build**: in `git/hooks.rs`, call `pipeline::trigger::on_push()`
2. **Build → Deploy**: in `pipeline/executor.rs`, write `deployments` row on image build success
3. **Build → Notify**: in `pipeline/executor.rs`, call `notify::dispatch::on_build_complete()`
4. **Project → Build**: in `api/merge_requests.rs`, call `pipeline::trigger::on_mr()`
5. **Agent → Notify**: in `agent/service.rs`, call `notify::dispatch::on_agent_completed()`
6. **Alert → Notify**: in `observe/alert.rs`, call `notify::dispatch::on_alert_firing()`
7. **Deploy → Notify**: in `deployer/reconciler.rs`, call `notify::dispatch::on_deploy_status()`
8. **Secrets → Build**: in `pipeline/executor.rs`, call `secrets::engine::resolve_secrets_for_env()`
9. **Secrets → Agent**: in `agent/identity.rs`, resolve API keys via secrets engine

**Wiring order** (respects the data flow):
- Secrets first (8, 9) — build and agent need secrets before they can run real workloads
- Build triggers (1, 4) — git push and MR merge now kick off pipelines
- Build → Deploy (2) — successful builds trigger deployments
- Notification subscribers (3, 5, 6, 7) — all notification wiring can be done in one pass

---

## Claude Code Session Assignment (if using multiple agents)

Optimal assignment for parallel Claude Code sessions:

| Session | Modules | Why grouped | Estimated effort |
|---------|---------|-------------|-----------------|
| A | 03-git + 04-project | Both deal with git repos and project data, natural overlap | ~2,400 LOC, medium |
| B | 05-build + 06-deployer | Build produces what deployer consumes, related K8s patterns | ~2,200 LOC, high (K8s pod lifecycle) |
| C | 07-agent | Self-contained, ports from Go prototype (see `plans/mgr-reference.md`) | ~800 LOC, high (pod exec/attach spike) |
| D | 08-observability | Largest module, needs full attention (OTLP, Parquet, queries) | ~2,200 LOC, high |
| E | 09-secrets-notify | Two small modules, quick to implement | ~1,000 LOC, low |
| F | 10-web-ui | Separate language (TypeScript), independent build | ~2,500 LOC TS, low |

Sessions A-E can all start simultaneously after Phase 1 completes. Session F can start as soon as Phase 1 (02-identity) ships.

**Session context each agent needs**:
- `plans/01-foundation.md` + `plans/02-identity-auth.md` (to understand AppState, AuthUser, RequirePermission)
- `plans/unified-platform.md` SQL schema (for the tables they'll query)
- Their specific plan file(s)
- `src/` current state (to import from store/, auth/, rbac/)

**What to tell each agent**:
> You are implementing module XX of a unified Rust platform. Foundation (01) and auth (02) are complete — you can import from these existing modules:
> - `use crate::store::AppState;` — shared state (pool, valkey, minio, kube, config)
> - `use crate::auth::middleware::AuthUser;` — auth extractor (from Bearer token or session cookie)
> - `use crate::rbac::{Permission, resolver};` — permission enum and `has_permission()` / `effective_permissions()`
> - `use crate::rbac::middleware::require_permission;` — route-layer middleware for permission checks
> - `use crate::error::ApiError;` — error type with NotFound, Unauthorized, Forbidden, BadRequest, Conflict, Internal
>
> Your module should expose a `pub fn router() -> Router<AppState>` and optionally a background task via `pub async fn run(state, shutdown)`. Do NOT import from other Wave 1 modules (03-09). Cross-module calls should be left as `// TODO: wire integration` comments. Use `sqlx::query_as!` for all DB access. After adding queries, run `just db-prepare`. Refer to your plan file for the full spec.

---

## Risk Register

| Risk | Impact | Likelihood | Mitigation |
|------|--------|------------|------------|
| kube-rs pod exec/attach doesn't work for interactive agent sessions | Blocks 07-agent (core feature) | Medium | Spike this in Phase 1 before committing. Fallback: `kubectl exec` via `tokio::process::Command`. |
| OTLP proto decoding is more complex than expected (nested messages, different encodings) | Slows 08-observe | Medium | Start with logs only (simplest OTLP message type). Add traces and metrics incrementally. |
| Parallel agents create conflicting migrations | Merge pain in Phase 2.5 | High | Enforce timestamp ranges (see table above). Run `sqlx migrate run` in CI to catch conflicts early. |
| RBAC delegation logic has edge cases (circular delegation, expired grants) | Auth bugs in production | Medium | Extensive unit tests in 02. Property-based tests for delegation resolution. |
| Git smart HTTP streaming has subtle protocol issues | Blocks clone/push in 03 | Medium | Test against real `git` CLI early. Reference Gitea/gogs source for protocol edge cases. |
| Single-binary compile times grow as modules are added | Slows iteration in Phase 2 | Low | Already using `cargo-chef` for Docker layer caching. Use `cargo check` (not `cargo build`) for rapid iteration. `bacon` file watcher helps locally. |

---

## Definition of Done (per module)

A module is "done" and ready for integration wiring when ALL of the following are true:

- [ ] `cargo check` passes with the module included
- [ ] `cargo clippy -- -D warnings` clean
- [ ] All API endpoints return correct responses (tested with `cargo nextest`)
- [ ] RBAC enforced on all endpoints (unit test: unauthenticated → 401, unauthorized → 403)
- [ ] DB migrations run cleanly on a fresh database
- [ ] Background tasks (if any) start and stop cleanly with the shutdown signal
- [ ] Integration stubs are marked with `// TODO: wire integration` comments
- [ ] No `unsafe` code (enforced by `#![forbid(unsafe_code)]`)

---

## Verification Checkpoints

### Phase 1 done:
- [x] `cargo check` compiles (01-foundation complete 2026-02-19)
- [x] `cargo run` starts, connects to DB/Valkey/MinIO, creates admin user (01-foundation)
- [x] `/healthz` returns 200 (01-foundation)
- [x] Auth module implemented: login, sessions, API tokens (02-identity complete 2026-02-19)
- [x] RBAC permission enforcement works: role-based + delegation (02-identity complete 2026-02-19)
- [x] `just ci` passes: fmt + clippy + deny + 11 unit tests + release build (02-identity)
- [ ] kube-rs pod exec/attach spike validated (for 07-agent)

### Phase 2 done (per module):
- [ ] Module meets [Definition of Done](#definition-of-done-per-module) above
- [ ] Integration tests pass against kind cluster (for K8s-dependent modules: 05, 06, 07)

### Phase 2.5 done:
- [ ] All 9 integration wiring points connected
- [ ] Each wiring point has at least one integration test
- [ ] `cargo check` compiles with all modules and wiring

### Phase 3 done:
- [ ] UI builds (`just ui`)
- [ ] Login flow works in browser
- [ ] All pages render and consume API data

### Phase 4 done:
- [ ] E2E: push → build → deploy → observe → alert → notify
- [ ] E2E: create session → agent works → commits → pipeline triggers
- [ ] `just ci` passes all checks
- [ ] Load test: OTLP ingest at expected throughput (08)
- [ ] Graceful shutdown: all background tasks drain cleanly

---

## Lessons Learned

### From 01-Foundation (2026-02-19)

1. **sqlx migration version format**: The version number is everything before the first `_` in the filename. `20260220_010001_name.up.sql` creates version `20260220`, not `20260220010001`. All 21 migrations collided. Fix: `20260220010001_name.up.sql` — no underscore between date and sequence number.

2. **FK ordering in migrations**: `user_roles` references `projects(id)`, so `projects` must be created first. The original plan had projects at position 008 but user_roles at 004. Topologically sort tables by FK dependencies when planning migration order.

3. **rand 0.10 vs argon2 0.5 version conflict**: `argon2` re-exports `rand_core 0.6` via `password-hash`, but `rand 0.10` uses `rand_core 0.9`. These are incompatible types even though they look the same. Use `argon2::password_hash::rand_core::OsRng` for argon2 salt generation, not `rand::rng()`.

4. **fred `Pool` doesn't implement `PubsubInterface`**: Only `Client` does. Use `pool.next().publish()` to get a `&Client` reference that supports pub/sub.

5. **Dead code warnings with `-D warnings`**: Foundation types are consumed by later modules. Use targeted `#[allow(dead_code)]` on `Config`, `ApiError`, `AppState`, and valkey helpers. Remove annotations as modules adopt them.

6. **kind port conflicts with OrbStack/Docker**: The kind cluster maps ports 5432/6379/8080/9000/9001 to localhost. OrbStack may hold these ports even after containers exit (stale ESTABLISHED connections). Stop OrbStack services or remap kind ports before running `just cluster-up`.

7. **Bootstrap uses dynamic `sqlx::query()`, not `sqlx::query!()`**: The bootstrap module seeds data with dynamic queries (bind parameters, no compile-time checking). This means `cargo sqlx prepare` reports "no queries found" — that's expected until modules add `sqlx::query!()` / `sqlx::query_as!()` calls.

### From 02-Identity & Auth (2026-02-19)

8. **rand 0.10 API changed from 0.8**: `rand::RngCore` is no longer re-exported from the crate root. `rand::rng().fill_bytes()` doesn't work. Use `rand::fill(&mut bytes)` (free function) for random byte generation.

9. **axum 0.8 MethodRouter methods**: `.patch()`, `.put()`, `.delete()` are methods on `MethodRouter`, not standalone functions. Don't import them from `axum::routing` — chain directly: `.route("/path", get(handler).patch(other))`.

10. **INET column needs ipnetwork crate**: `audit_log.ip_addr` is INET in Postgres. sqlx needs the `ipnetwork` feature flag to bind Rust types. Without it, skip the column in INSERT (stays NULL). Consider adding ipnetwork when needed.

11. **Clippy `too_many_arguments` threshold is 7**: Any function with 7+ params triggers the lint. Use a params/options struct from the start (e.g., `AuditEntry`, `CreateDelegationParams`).

12. **Clippy `trivially_copy_pass_by_ref` on small enums**: For `Copy` types like `Permission`, use `self` not `&self` in method signatures (e.g., `fn as_str(self) -> &'static str`).

13. **RequirePermission middleware available but not required**: Phase 02 admin routes use inline `resolver::has_permission()` checks. The `RequirePermission` middleware (`rbac::middleware::require_permission`) is implemented and available for modules 03-09 as a route layer. Use it for modules with many endpoints sharing the same permission.

### For Future Modules

- When adding new `sqlx::query!()` calls, run `just db-prepare` to update `.sqlx/` cache, then commit the changes.
- Each module should use `sqlx::query_as!()` (compile-time checked) for all DB access, unlike bootstrap which used dynamic queries.
- Test migrations on a fresh DB (`just cluster-down && just cluster-up && just db-migrate`) before committing.
- **Auth imports for handlers**: `use crate::auth::middleware::AuthUser;` for the auth extractor, `use crate::rbac::{Permission, resolver};` for permission checks.
- **Audit logging**: Use `AuditEntry` struct pattern (see `api/users.rs` or `api/admin.rs`) with `write_audit()` for all mutations.
- **Permission cache invalidation**: Call `resolver::invalidate_permissions(valkey, user_id, project_id)` after any role/delegation change.
