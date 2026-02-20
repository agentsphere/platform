# Plan 16 — Next Features Roadmap

## Current State Assessment

The platform has **all 10 phases implemented** (18.7K LOC Rust + Preact UI + MCP servers), plus:

- **Phase 11**: Auth improvements (user types, token scope enforcement, passkeys/WebAuthn)
- **Phase 12**: Unit test gap-filling (57 tests across 7 modules)
- **Phase 13**: Code review fixes (5 critical security + 10 idiomatic improvements)
- **Phase 14**: Test quality review plan + agent DX improvement plans (written, not all executed)
- **Phase 15**: Integration test plan (written, not yet executed)

### What's Implemented

| Module | Status | LOC |
|--------|--------|-----|
| Foundation (config, error, store) | Complete | ~1.5K |
| Identity & Auth (password, tokens, sessions, passkeys) | Complete | ~2.5K |
| RBAC (roles, permissions, delegation, resolver) | Complete | ~1.5K |
| Git Server (smart HTTP, hooks, browser, LFS) | Complete | ~3K |
| Project Management (CRUD, issues, MRs, webhooks) | Complete | ~5K |
| Build Engine (pipelines, executor, triggers) | Complete | ~3K |
| Continuous Deployer (reconciler, ops repos, renderer, applier) | Complete | ~2.5K |
| Agent Orchestration (service, identity, Claude Code provider) | Complete | ~1.8K |
| Observability (OTLP ingest, parquet, query, alerts) | Complete | ~7K |
| Secrets Engine (AES-256-GCM, CRUD) | Complete | ~1K |
| Notifications (dispatch, email, webhook) | Complete | ~900 |
| Web UI (Preact SPA) | Complete (skeleton) | ~1.5K TS |
| MCP Servers (core, pipeline, issues) | Complete (3 of 6) | ~1.2K JS |

### What's Planned but Not Executed

1. **Integration tests** (plan 15) — 83 tests across 6 files, zero written
2. **Agent DX Phase B** — configurable container images (migration + pod builder changes)
3. **Agent DX Phase C** — deploy/observe MCP servers + deploy permission delegation
4. **Agent DX Phase D** — preview environments per branch
5. **Agent DX Phase E** — admin MCP server
6. **Test quality Phase B-F** — boundary tests, proptest, rstest, integration tests
7. **Code review nitpicks N1-N9** — various small fixes

---

## Next Features — Prioritized

Features are ordered by impact: what makes the platform most useful, soonest, for its primary users (AI agents and human developers/operators).

### Priority 1: Integration Tests (High Impact, Blocks Safe Iteration)

**Why first**: The platform has 78 API handlers and zero HTTP-level integration tests. Every subsequent feature change risks silent regressions. Without integration tests, you can't safely refactor or add features.

**Scope**: Implement plan 15 (steps E1-E7):
- E1: Test infrastructure (`tests/helpers/mod.rs`, Justfile additions)
- E2: Auth integration tests (13 tests)
- E3: Admin integration tests (14 tests)
- E4: Project integration tests (14 tests)
- E5: RBAC integration tests (15 tests)
- E6: Issue/MR integration tests (15 tests)
- E7: Webhook integration tests (12 tests)

**Prerequisite**: Running Postgres + Valkey (via `just cluster-up` or standalone).

**Deliverable**: `just test-integration` runs 83 tests against real DB/cache.

---

### Priority 2: Deploy & Observe MCP Servers (Agent DX Phase C)

**Why next**: Agents currently can't interact with deployments or observability data via MCP tools. This is a core gap — an ops-role agent can't check deployment status, read logs, or trigger rollbacks. The APIs already exist; only the MCP wrappers are missing.

**Scope**:
- Create `mcp/servers/platform-deploy.js` (8 tools: list/get/create/update deployments, rollback, history, previews)
- Create `mcp/servers/platform-observe.js` (4 tools: search logs, get trace, query metrics, list alerts)
- Update `docker/entrypoint.sh` to include deploy+observe for `ops` and `admin` roles
- Extend permission delegation in `src/agent/identity.rs` to support `DeployRead`/`DeployPromote`
- Add `delegate_deploy`/`delegate_observe` flags to session creation API

**Deliverable**: `ops` role agents can query logs, check deployments, and trigger rollbacks.

---

### Priority 3: Configurable Agent Container Images (Agent DX Phase B)

**Why**: The hardcoded `node:22-slim` agent image limits agents to Node.js projects. Projects using Go, Rust, Python, or multi-language stacks need custom runtime environments.

**Scope**:
- Migration: add `agent_image TEXT` column to `projects`
- Extend `ProviderConfig` with `image` and `setup_commands` fields
- Add `check_container_image()` validation (reject shell metacharacters)
- Update pod builder: session override > project default > platform default
- Support init container for `setup_commands` (run after git clone, before claude)
- API validation in session creation + project update

**Deliverable**: `POST /api/projects/{id}/sessions` with `{"config": {"image": "golang:1.23"}}` spawns an agent in a Go environment.

---

### Priority 4: Preview Environments (Agent DX Phase D)

**Why**: Feature branches need a way to show running previews without manually creating deployments. This is table-stakes for modern dev workflows and essential for agent-driven development.

**Scope**:
- Migration: `preview_deployments` table
- Extend pipeline executor: non-main branches auto-create previews
- Preview reconciler background task (every 15s)
- Auto-cleanup on MR merge + TTL expiry (default 24h)
- Preview API endpoints (list, get, delete)
- `slugify_branch()` helper for K8s-safe naming

**Deliverable**: Push to feature branch → pipeline succeeds → preview URL available → auto-cleaned on merge.

---

### Priority 5: Test Quality Improvements (Plan 14 Phases B-D)

**Why**: The existing 264 unit tests have weak assertions, tautological tests, and missing edge cases. Fixing these catches bugs that the current suite silently misses.

**Scope**:
- Phase B: ~60 boundary/edge-case unit tests (validation, error conversion, auth, secrets, pipeline)
- Phase C: Test infrastructure (add `rstest` dep, `AuthUser::test_*` constructors)
- Phase D: Refactor existing tests (strengthen assertions, fix tautologies, activate proptest)

**Deliverable**: ~330 meaningful unit tests (up from 264 with weak assertions).

---

### Priority 6: Admin MCP Server (Agent DX Phase E)

**Why**: Admin-role agents need to manage users, roles, delegations, and platform configuration. This completes the MCP server suite.

**Scope**:
- Create `mcp/servers/platform-admin.js` (~150 lines)
- Tools: user CRUD, role management, delegation management, platform config
- Only loaded for `admin` role agents

**Deliverable**: Admin agents can create users, assign roles, and manage delegations via MCP tools.

---

### Priority 7: Web UI Completion

**Why**: The current UI is a skeleton (dashboard, basic navigation). Real usability for human operators requires functional pages.

**Scope** (in priority order):
1. **Project detail page**: file browser, issues list, MR list, settings
2. **Pipeline/build viewer**: step list, log streaming, artifact download
3. **Observability dashboard**: log search, trace waterfall, metric charts
4. **Agent session viewer**: session list, live streaming, chat interface
5. **Admin panel**: user management, role/permission editor, delegation viewer
6. **Deployment dashboard**: environment status, history, rollback controls

**Deliverable**: Functional UI pages for the most common human workflows.

---

### Priority 8: Remaining Code Review Fixes (Plan 13 Nitpicks N1-N9)

**Why**: These are lower-priority quality improvements that should be addressed when touching relevant files.

**Scope**:
- N1: Return `&str` from token extraction (avoid allocation per request)
- N2: Stricter email validation (exactly one `@`, non-empty local+domain)
- N3: Block leading dots in names
- N4: Fix `parse_cors_origins("")` → treat as unset
- N5: Configurable permission cache TTL
- N6: Log audit write failures + fix `ip_addr` (needs `ipnetwork` crate)
- N7: Distinguish pod-not-found vs API error in step log streaming
- N8: Timeout on git subprocesses in `browser.rs`
- N9: Wire up Valkey pub/sub in pipeline executor (currently polling only)

**Deliverable**: Cleaner, more robust code across 9 small fixes.

---

### Priority 9: E2E Test Suite

**Why**: After integration tests cover the API layer, E2E tests validate the full stack: git push → pipeline → deploy, agent session lifecycle, WebSocket streaming.

**Scope**:
- Git operation tests (bare repo init, push, pull, branch listing, merge)
- Pipeline trigger E2E (`.platformci.yml` parse → K8s pod → logs → artifact)
- Webhook dispatch with HMAC verification (mock HTTP server via `wiremock`)
- WebSocket agent session streaming
- Deployer reconciliation with real K8s (via Kind)

**Prerequisite**: Integration tests (Priority 1) and Kind cluster.

**Deliverable**: `just test-e2e` runs full-stack tests against Kind cluster.

---

## Dependency Graph

```
Priority 1 (Integration Tests)  ←  blocks safe iteration on everything
    │
    ├── Priority 2 (Deploy/Observe MCP)  ←  no code deps on P1, but safer with tests
    │       │
    │       └── Priority 4 (Preview Envs)  ←  depends on deploy API
    │
    ├── Priority 3 (Custom Images)  ←  independent, parallel with P2
    │
    ├── Priority 5 (Test Quality)  ←  independent, parallel with P2-P4
    │
    ├── Priority 6 (Admin MCP)  ←  depends on MCP infra from P2
    │
    ├── Priority 7 (UI Completion)  ←  independent, can parallelize
    │
    ├── Priority 8 (Nitpicks)  ←  independent, address opportunistically
    │
    └── Priority 9 (E2E Tests)  ←  depends on P1, needs Kind cluster
```

---

## Recommended Execution Order

**Wave 1** (parallel):
- Integration tests (P1) — foundational
- Deploy/Observe MCP servers (P2) — unblocks ops agents
- Configurable container images (P3) — unblocks non-Node projects

**Wave 2** (parallel, after Wave 1):
- Preview environments (P4) — depends on P2
- Test quality improvements (P5) — independent
- Admin MCP server (P6) — depends on P2 infra

**Wave 3** (after Wave 2):
- UI completion (P7) — can start any time but most value after APIs stabilize
- Nitpick fixes (P8) — opportunistic
- E2E tests (P9) — after integration tests prove the pattern

---

## Estimated Scope

| Priority | New Files | Est. LOC | Migrations |
|----------|-----------|----------|------------|
| P1: Integration Tests | 7 | ~2,500 | 0 |
| P2: Deploy/Observe MCP | 2 JS + modify 4 Rust | ~600 | 0 |
| P3: Custom Images | 1 migration + modify 6 Rust | ~300 | 1 |
| P4: Preview Environments | 1 migration + 1 Rust + modify 4 | ~500 | 1 |
| P5: Test Quality | modify ~12 Rust | ~800 | 0 |
| P6: Admin MCP | 1 JS | ~150 | 0 |
| P7: UI Completion | ~15 TS/TSX | ~3,000 | 0 |
| P8: Nitpicks | modify ~9 Rust | ~200 | 0 |
| P9: E2E Tests | ~5 Rust | ~1,500 | 0 |
| **Total** | **~40 files** | **~9,550** | **2** |
