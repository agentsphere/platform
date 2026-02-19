# 00 — Parallel Execution Guide

## Plan Index

| # | Plan | Estimated LOC | Dependencies |
|---|------|--------------|-------------|
| 01 | [Foundation](01-foundation.md) | ~1,200 Rust | None (start here) |
| 02 | [Identity & Auth](02-identity-auth.md) | ~1,500 Rust | 01 |
| 03 | [Git Server](03-git-server.md) | ~1,400 Rust | 01, 02 |
| 04 | [Project Management](04-project-mgmt.md) | ~1,000 Rust | 01, 02 |
| 05 | [Build Engine](05-build-engine.md) | ~1,400 Rust | 01, 02 |
| 06 | [Deployer](06-deployer.md) | ~800 Rust | 01, 02 |
| 07 | [Agent Orchestration](07-agent-orchestration.md) | ~800 Rust | 01, 02 |
| 08 | [Observability](08-observability.md) | ~2,200 Rust | 01, 02 |
| 09 | [Secrets & Notifications](09-secrets-notify.md) | ~1,000 Rust | 01, 02 |
| 10 | [Web UI](10-web-ui.md) | ~2,500 TS | 01, 02 + APIs |
| — | Integration testing + hardening | ~1,200 Rust | All |
| **Total** | | **~15,000** | |

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
     └───────┴───────┴───────┴───────┴───────┴───────┘
                           │
                    ┌──────▼───────┐
                    │   10-web-ui  │
                    │  (incremental│
                    │   as APIs    │
                    │   land)      │
                    └──────┬───────┘
                           │
                    ┌──────▼───────┐
                    │  Integration │
                    │  testing +   │
                    │  hardening   │
                    └──────────────┘
```

---

## Execution Phases

### Phase 1: Sequential Foundation (must complete in order)
```
01-foundation  →  02-identity-auth
```
These two are the sequential bottleneck. Every module depends on `AppState` (from 01) and `AuthUser`/`RequirePermission` middleware (from 02). Ship these first.

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

### Phase 3: UI (incremental, overlapping with Phase 2)
```
10-web-ui (start login + layout as soon as 02 ships, add pages as APIs land)
```
UI work can start as soon as auth API exists and grow incrementally.

### Phase 4: Integration & Hardening
Cross-module integration tests, E2E flows, performance tuning, edge cases.

---

## Runtime Integration Points (not compile-time)

These modules interact at runtime through the database, not through direct Rust imports:

| Producer | Consumer | Via |
|----------|----------|-----|
| 03-git (push) | 05-build (trigger) | `pipelines` table row insert |
| 05-build (success) | 06-deployer (reconcile) | `deployments` table row update |
| 08-observe (alert fires) | 09-notify (dispatch) | `pub async fn on_alert_firing()` call |
| 04-project (MR merged) | 05-build (trigger) | `pipelines` table row insert |
| 07-agent (session done) | 09-notify (dispatch) | `pub async fn on_agent_completed()` call |

**Implication**: During parallel development, each module can stub these integration points and wire them later. For example:
- Build engine can trigger on API call only (wire git hook trigger later)
- Deployer reads `deployments` table regardless of who writes to it
- Notification dispatch functions exist but callers wire in last

---

## Integration Wiring (after parallel wave)

Once all modules compile independently, wire the cross-module calls:

1. **Git → Build**: in `git/hooks.rs`, call `pipeline::trigger::on_push()`
2. **Build → Deploy**: in `pipeline/executor.rs`, write `deployments` row on image build success
3. **Build → Notify**: in `pipeline/executor.rs`, call `notify::dispatch::on_build_complete()`
4. **Project → Build**: in `api/merge_requests.rs`, call `pipeline::trigger::on_mr()`
5. **Agent → Notify**: in `agent/service.rs`, call `notify::dispatch::on_agent_completed()`
6. **Alert → Notify**: in `observe/alert.rs`, call `notify::dispatch::on_alert_firing()`
7. **Deploy → Notify**: in `deployer/reconciler.rs`, call `notify::dispatch::on_deploy_status()`
8. **Secrets → Build**: in `pipeline/executor.rs`, call `secrets::engine::resolve_secrets_for_env()`
9. **Secrets → Agent**: in `agent/identity.rs`, resolve API keys via secrets engine

---

## Claude Code Session Assignment (if using multiple agents)

Optimal assignment for parallel Claude Code sessions:

| Session | Modules | Why grouped |
|---------|---------|-------------|
| A | 03-git + 04-project | Both deal with git repos and project data, natural overlap |
| B | 05-build + 06-deployer | Build produces what deployer consumes, related K8s patterns |
| C | 07-agent | Self-contained, ports from mgr/ Go reference |
| D | 08-observability | Largest module, needs full attention (OTLP, Parquet, queries) |
| E | 09-secrets-notify | Two small modules, quick to implement |
| F | 10-web-ui | Separate language (TypeScript), independent build |

Sessions A-E can all start simultaneously after Phase 1 completes.

---

## Verification Checkpoints

After each phase, verify:

**Phase 1 done**:
- [ ] `cargo check` compiles
- [ ] `cargo run` starts, connects to DB, creates admin user
- [ ] `/healthz` returns 200
- [ ] Login → get session → access protected endpoint works
- [ ] RBAC permission enforcement works

**Phase 2 done** (per module):
- [ ] Module compiles and passes unit tests
- [ ] API endpoints return correct responses
- [ ] RBAC enforced on all endpoints
- [ ] Integration tests pass against kind cluster

**Phase 3 done**:
- [ ] UI builds (`just ui`)
- [ ] Login flow works in browser
- [ ] All pages render and consume API data

**Phase 4 done**:
- [ ] Cross-module integration wiring complete
- [ ] E2E: push → build → deploy → observe → alert → notify
- [ ] E2E: create session → agent works → commits → pipeline triggers
- [ ] `just ci` passes all checks
