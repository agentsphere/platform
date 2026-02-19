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
| `migrations/` | Timestamp-prefixed filenames (e.g., `20260220_010100_create_pipelines.sql`). No ordering conflicts as long as timestamps don't collide. **Convention**: each module claims a timestamp range (see below). |
| `Cargo.toml` | Each module may add dependencies. Use a single merge pass after the parallel wave to reconcile. |

**Migration timestamp ranges** (prevents collisions during parallel dev):

| Module | Timestamp prefix |
|--------|-----------------|
| 01-foundation | `20260220_0100xx` |
| 02-identity | `20260220_0200xx` |
| 03-git | `20260220_0300xx` |
| 04-project | `20260220_0400xx` |
| 05-build | `20260220_0500xx` |
| 06-deployer | `20260220_0600xx` |
| 07-agent | `20260220_0700xx` |
| 08-observe | `20260220_0800xx` |
| 09-secrets-notify | `20260220_0900xx` |

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
| C | 07-agent | Self-contained, ports from mgr/ Go reference | ~800 LOC, high (pod exec/attach spike) |
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
> You are implementing module XX of a unified Rust platform. Foundation (01) and auth (02) are complete — you can import `AppState`, `AuthUser`, `RequirePermission` from the existing crate. Your module should expose a `pub fn router() -> Router<AppState>` and optionally a background task via `pub async fn run(state, shutdown)`. Do NOT import from other Wave 1 modules (03-09). Cross-module calls should be left as `// TODO: wire integration` comments. Use `sqlx::query_as!` for all DB access. Refer to your plan file for the full spec.

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
- [ ] `cargo check` compiles
- [ ] `cargo run` starts, connects to DB/Valkey/MinIO, creates admin user
- [ ] `/healthz` returns 200
- [ ] Login → get session → access protected endpoint works
- [ ] RBAC permission enforcement works (role-based)
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
