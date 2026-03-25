# Skill: Ecosystem Audit — Platform Components & Integration Seams

**Description:** Orchestrates 8 parallel AI agents that audit everything *outside* `src/` — the agent-runner CLI, MCP servers, Preact UI, Docker images, Helm chart, Kustomize overlays, CI/CD workflows, infrastructure scripts, and the installer. The core focus is on **integration contracts**: do all components agree on APIs, env vars, ports, image tags, protocols, and security boundaries? Finds the bugs that live in the gaps between components.

**When to use:** Before a release, after adding/changing a component boundary (new env var, API endpoint, Docker image, Helm value), or when onboarding. Unlike `/audit` (which reviews the 70K LOC Rust monolith), this reviews the ~24K LOC ecosystem around it and every seam where components touch.

---

## Orchestrator Instructions

You are the **Ecosystem Auditor**. Your job is to:

1. Run quick pre-flight checks on each component
2. Launch 8 parallel audit agents — 6 component-focused, 2 cross-cutting
3. Collect and synthesize findings into a prioritized report
4. Produce a persistent `plans/ecosystem-audit-<date>.md` report

### Severity Levels

| Severity | Meaning | Action |
|---|---|---|
| **CRITICAL** | Security hole, broken deployment, data loss, image escape | Must fix immediately |
| **HIGH** | Contract mismatch, missing env var, broken integration, auth bypass | Fix before release |
| **MEDIUM** | Inconsistency, missing validation, drift between components | Fix when touching the area |
| **LOW** | Style nit, minor naming, optional improvement | Fix only if trivial |
| **INFO** | Observation, good pattern worth noting | No action needed |

---

## Phase 0: Pre-flight Checks

Run quick checks to establish baseline. Don't block on failures — agents will root-cause them.

```bash
# Rust monolith compiles (ensures API types are current)
just lint

# Agent runner CLI
cd cli/agent-runner && cargo check && cargo clippy --all-features -- -D warnings && cd ../..

# UI builds
just ui build 2>&1 | tail -5

# MCP server deps
cd mcp && npm ci --ignore-scripts 2>&1 | tail -3 && cd ..

# Helm lint
helm lint helm/platform/ --values helm/platform/values.yaml 2>&1

# Docker lint (if hadolint available)
which hadolint && hadolint docker/Dockerfile docker/Dockerfile.platform-runner docker/Dockerfile.platform-runner-bare docker/Dockerfile.dev-pod 2>&1 || echo "hadolint not installed — skip"

# File counts for context
echo "=== Component sizes ==="
echo "CLI:"; find cli/agent-runner/src -name '*.rs' | xargs wc -l | tail -1
echo "UI:"; find ui/src -name '*.ts' -o -name '*.tsx' | xargs wc -l | tail -1
echo "MCP:"; find mcp -name '*.js' -not -path '*/node_modules/*' | xargs wc -l | tail -1
echo "Helm:"; find helm -type f | xargs wc -l | tail -1
echo "Hack:"; find hack -name '*.sh' | xargs wc -l | tail -1
echo "Docker:"; wc -l docker/Dockerfile*
```

---

## Phase 1: Parallel Component & Integration Audits

Launch **all 8 agents concurrently**. Each agent gets a specific scope and checklist.

**Critical instructions for EVERY agent prompt:**
- List exact files the agent must read
- Agent must READ every file completely — no skimming
- Output format: `[SEVERITY] file:line — description\n  Fix: ...`
- Agent is performing an AUDIT (read-only) — it must NOT edit any files
- Focus heavily on **contracts with other components** — the seams are where bugs hide

---

### Agent 1: Agent Runner CLI — Protocol & Transport Correctness

**Scope:** All files under `cli/agent-runner/` (~6K LOC Rust)

**Read ALL files in `cli/agent-runner/src/`, plus `cli/agent-runner/Cargo.toml`.**

_Protocol contract with platform API:_
- [ ] HTTP endpoints called match what `src/api/sessions.rs` and `src/api/commands.rs` expose
- [ ] Request/response types (JSON shapes) match between CLI and platform — field names, types, optionality
- [ ] WebSocket message format matches platform's WebSocket handler
- [ ] Auth header format matches what `AuthUser` extractor expects (Bearer token)
- [ ] API URL construction: base URL + path segments are correct (no double slashes, no missing prefixes)
- [ ] Error responses from platform are correctly parsed and surfaced to user

_Pub-sub contract with Valkey:_
- [ ] Channel naming convention matches `src/store/eventbus.rs` patterns
- [ ] Message serialization format (JSON shape) matches what platform publishes
- [ ] Subscriber correctly handles connection loss, reconnection
- [ ] Channel cleanup on session termination
- [ ] ACL credentials match what platform sets via `src/agent/identity.rs`

_MCP integration:_
- [ ] MCP server paths match what's installed in Docker images
- [ ] MCP server startup/shutdown lifecycle is correct
- [ ] MCP tool definitions match what servers actually expose
- [ ] Error propagation from MCP servers to REPL

_Security:_
- [ ] No secrets hardcoded (API keys, tokens)
- [ ] Token storage on disk (if any) has appropriate permissions
- [ ] Command injection prevention in shell-out paths
- [ ] Input sanitization for user-provided values that become API calls
- [ ] No `.unwrap()` on network responses

_Dependency alignment:_
- [ ] `fred` version matches platform's `fred` version (Valkey protocol compat)
- [ ] `serde_json` serialization format compatible
- [ ] TLS/rustls configuration compatible with platform's TLS
- [ ] `clap` CLI argument validation — invalid input handled gracefully

**Output:** Numbered findings with severity, file:line, description, and fix.

---

### Agent 2: MCP Servers — API Contract & Error Handling

**Scope:** All files under `mcp/` excluding `node_modules/` (~2.8K LOC JS)

**Read ALL files: `mcp/servers/*.js`, `mcp/lib/*.js`, `mcp/tests/*.js`, `mcp/package.json`.**

_API contract with platform:_
- [ ] Every HTTP endpoint called by MCP servers exists in the platform API (cross-reference with `src/api/mod.rs` router)
- [ ] Request bodies match the `Json<T>` types platform handlers expect
- [ ] Response parsing matches what platform returns (field names, nested structures)
- [ ] Pagination parameters (`limit`, `offset`) match platform's `ListParams`
- [ ] URL construction: correct path prefixes (`/api/`), correct parameter encoding
- [ ] HTTP methods match (GET vs POST vs PATCH vs PUT vs DELETE)

_Auth contract:_
- [ ] Auth token passed in correct header format (`Authorization: Bearer <token>`)
- [ ] Token scope matches required permissions for each endpoint
- [ ] Error handling for 401/403 responses — does it surface useful messages?
- [ ] No tokens hardcoded in source

_MCP protocol correctness:_
- [ ] Tool definitions have correct input schemas (required fields, types)
- [ ] Tool descriptions accurately describe behavior
- [ ] Error responses follow MCP protocol spec
- [ ] Resource definitions (if any) correctly exposed
- [ ] stdin/stdout transport handles large payloads

_Error handling:_
- [ ] Network errors caught and surfaced with context
- [ ] Platform error responses (4xx, 5xx) parsed and re-raised correctly
- [ ] Timeout handling on HTTP calls
- [ ] Malformed response handling (non-JSON, unexpected shapes)
- [ ] No unhandled promise rejections

_Shared client library (`mcp/lib/client.js`):_
- [ ] Base URL configuration from environment
- [ ] Auth token injection into all requests
- [ ] Response parsing is safe (handles non-200 gracefully)
- [ ] Request/response logging doesn't leak secrets

_Test coverage:_
- [ ] Tests exist for each MCP server
- [ ] Tests validate input schema edge cases
- [ ] Tests mock HTTP correctly (no real API calls in tests)

**Output:** Numbered findings with severity, file:line, description, and fix.

---

### Agent 3: Web UI — API Contract, Auth, Security

**Scope:** All files under `ui/src/` (~10K LOC TS/TSX), plus `ui/package.json`, `ui/build.mjs`

**Read ALL files in `ui/src/lib/` first (api.ts, auth.tsx, ws.ts, types.ts, format.ts), then pages and components.**

_API contract with platform:_
- [ ] Every `fetch()` or API call in `api.ts` targets an endpoint that exists in the platform API router
- [ ] Request bodies match the `Json<T>` types platform handlers expect — field names, types, required vs optional
- [ ] Response type definitions in `types.ts` match what platform actually returns (field names, types, nesting)
- [ ] Pagination: `limit`/`offset` query params match platform's `ListParams`
- [ ] URL construction: correct path prefixes, no trailing slash mismatches
- [ ] HTTP methods correct (especially PATCH vs PUT)
- [ ] Query parameter encoding matches platform's `Query<T>` extraction

_Auth flow:_
- [ ] Login flow matches platform's `POST /api/auth/login` expected body and response
- [ ] Token/cookie storage mechanism is secure (HttpOnly cookies vs localStorage)
- [ ] Auth context correctly detects expired sessions
- [ ] Logout properly clears state and calls platform logout endpoint
- [ ] Protected routes redirect to login on 401
- [ ] WebAuthn/passkey flow matches platform's `/api/passkeys/*` endpoints

_WebSocket contract:_
- [ ] WS URL construction correct (protocol upgrade, path)
- [ ] Message format matches platform's WebSocket handler
- [ ] Reconnection logic handles disconnects gracefully
- [ ] Message parsing handles malformed data

_Security:_
- [ ] No XSS vectors: user-generated content (issue bodies, MR descriptions, comments) properly sanitized before rendering
- [ ] Markdown rendering (`marked`) configured with sanitization
- [ ] No `dangerouslySetInnerHTML` or equivalent without sanitization
- [ ] CSRF protection: cookies + CORS alignment with platform
- [ ] No secrets in client-side code
- [ ] No sensitive data in console.log or error messages
- [ ] Content-Security-Policy compatibility with inline scripts/styles

_Build & embedding:_
- [ ] esbuild config produces correct output for `rust-embed`
- [ ] Source maps not included in production builds (or intentionally included)
- [ ] Dependencies pinned in package-lock.json
- [ ] No unused dependencies

_UX contract:_
- [ ] Error states rendered for API failures (not just blank screens)
- [ ] Loading states for async operations
- [ ] Pagination controls match API pagination behavior

**Output:** Numbered findings with severity, file:line, description, and fix.

---

### Agent 4: Docker Images — Build Chain, Security, Consistency

**Scope:** `docker/Dockerfile`, `docker/Dockerfile.platform-runner`, `docker/Dockerfile.platform-runner-bare`, `docker/Dockerfile.dev-pod`

**Read ALL 4 Dockerfiles completely. Also read `Justfile` (docker-related recipes), `.dockerignore`, and `.github/workflows/release.yaml`.**

_Image security:_
- [ ] No secrets baked into images (API keys, tokens, passwords)
- [ ] Non-root user configured in all runtime images
- [ ] Minimal base images (slim variants, no unnecessary packages)
- [ ] No `--privileged` or excessive capabilities
- [ ] `COPY` commands don't accidentally include `.env`, `.git`, or other sensitive files
- [ ] `.dockerignore` excludes sensitive and unnecessary files
- [ ] Base image versions pinned (not `latest` tags)

_Platform image (`Dockerfile`):_
- [ ] Multi-stage build doesn't leak build dependencies into final image
- [ ] UI assets correctly copied from builder stage
- [ ] Agent-runner binaries correct arch (amd64 + arm64) and placed at expected paths
- [ ] Runtime dependencies complete (libpq, ca-certificates, etc.)
- [ ] ENTRYPOINT/CMD matches expected startup behavior
- [ ] Health check configured (if applicable)
- [ ] Environment variable defaults align with `src/config.rs`

_Runner image (`Dockerfile.platform-runner`):_
- [ ] Claude Code CLI version pinned or managed
- [ ] MCP servers installed at paths that `cli/agent-runner` expects
- [ ] Kaniko executor available at expected path
- [ ] Git version sufficient for worktree operations (needs ≥2.17)
- [ ] Node.js version compatible with MCP servers
- [ ] User UID/GID matches K8s securityContext in Helm chart
- [ ] Tools installed match what agent sessions need (git, curl, etc.)

_Runner bare image (`Dockerfile.platform-runner-bare`):_
- [ ] Minimal toolset matches auto-setup expectations
- [ ] Kaniko executor path consistent with runner image
- [ ] Base image consistent with runner image

_Dev pod image (`Dockerfile.dev-pod`):_
- [ ] Rust version matches `rust-toolchain.toml` or `Cargo.toml`
- [ ] All dev tools installed (nextest, llvm-cov, sqlx-cli, deny, just)
- [ ] Claude CLI version pinned
- [ ] Git version matches production expectations
- [ ] Build cache optimization (correct layer ordering)

_Cross-image consistency:_
- [ ] UID/GID consistent between images that share volumes
- [ ] Tool versions aligned (git, node, etc.) where they should match
- [ ] Path conventions consistent (`/data/`, `/app/`, etc.)

_Build pipeline:_
- [ ] `just docker` builds correct image with correct tags
- [ ] `release.yaml` multi-arch build works (buildx, QEMU)
- [ ] Image tags in release workflow match Helm chart `image.tag` defaults
- [ ] No build args with secrets that persist in layer cache

**Output:** Numbered findings with severity, file:line, description, and fix.

---

### Agent 5: Helm Chart & Kustomize — Deployment Correctness

**Scope:** All files under `helm/platform/` and `deploy/`

**Read ALL files: `helm/platform/Chart.yaml`, `helm/platform/values.yaml`, `helm/platform/values-small.yaml`, `helm/platform/values-medium.yaml`, `helm/platform/values-large.yaml`, all files in `helm/platform/templates/`, `deploy/base/*`, `deploy/dev/*`.**

_Env var contract with platform binary:_
- [ ] Every env var in `configmap.yaml` / `secret.yaml` matches a var read by `src/config.rs`
- [ ] No env vars expected by `src/config.rs` are missing from Helm chart
- [ ] Default values in Helm `values.yaml` match defaults in `src/config.rs`
- [ ] Sensitive vars (MASTER_KEY, DB password, SMTP creds) are in `secret.yaml`, not `configmap.yaml`
- [ ] Boolean env vars use consistent format (`"true"` not `"1"` or `"yes"`)

_Kubernetes resource correctness:_
- [ ] Deployment resource limits/requests are reasonable for each values profile
- [ ] Liveness/readiness probes target the correct health endpoint and port
- [ ] Port definitions match what the platform binary listens on (8080, 2222 SSH, etc.)
- [ ] Volume mounts match paths the binary expects (`PLATFORM_OPS_REPOS_PATH`, git repos, etc.)
- [ ] PVC sizing is reasonable per profile (small/medium/large)

_RBAC:_
- [ ] ClusterRole permissions are minimal (principle of least privilege)
- [ ] Permissions cover all K8s operations the platform performs (create pods, services, namespaces, etc.)
- [ ] No wildcard (`*`) verbs or resources that shouldn't be there
- [ ] ServiceAccount correctly bound to ClusterRole
- [ ] Namespace-scoped resources vs cluster-scoped resources correct

_Network security:_
- [ ] NetworkPolicy for platform: ingress from correct sources only
- [ ] NetworkPolicy for data services: only platform can reach Postgres/Valkey/MinIO
- [ ] Egress policies (if any) allow necessary outbound (API calls, webhooks, SMTP)
- [ ] Service types correct (ClusterIP for internal, NodePort/LoadBalancer for external)

_Helm chart structure:_
- [ ] `_helpers.tpl` templates produce valid K8s names (63 char limit, valid chars)
- [ ] Chart dependencies (PostgreSQL, Valkey, MinIO) pinned to compatible versions
- [ ] `NOTES.txt` provides useful post-install instructions
- [ ] Values schema (if present) matches actual template usage
- [ ] Ingress template compatible with common ingress controllers (nginx, traefik)

_Kustomize overlays:_
- [ ] Base manifests are consistent with Helm chart (not diverged)
- [ ] Dev overlay patches are correct (image overrides, nodeport config)
- [ ] RBAC in Kustomize matches Helm chart RBAC

_Bitnami sub-charts:_
- [ ] PostgreSQL configuration matches what platform expects (port, DB name, user)
- [ ] Valkey configuration matches (port, auth, maxmemory)
- [ ] MinIO configuration matches (port, bucket, access key)
- [ ] Sub-chart resource limits reasonable per profile

**Output:** Numbered findings with severity, file:line, description, and fix.

---

### Agent 6: Infrastructure Scripts & CI/CD — Correctness & Security

**Scope:** `hack/*.sh`, `install.sh`, `Justfile`, `*.just`, `.github/workflows/*.yaml`, `.pre-commit-config.yaml`, `codecov.yml`, `.gitleaks.toml`, `deny.toml`

**Read ALL files listed above.**

_Shell script correctness:_
- [ ] All scripts use `set -euo pipefail` (or equivalent safe defaults)
- [ ] Variable expansions quoted (`"$VAR"` not `$VAR`) — especially paths and user input
- [ ] No command injection vectors via unquoted variables
- [ ] Temp files cleaned up on exit (trap handlers)
- [ ] Scripts portable across bash versions (no bash 5+ features if targeting bash 4)
- [ ] Exit codes meaningful (non-zero on failure)

_cluster-up.sh / kind scripts:_
- [ ] Kind config matches what tests expect (port mappings, extra mounts)
- [ ] Postgres/Valkey/MinIO manifests match what test helpers connect to
- [ ] Service ports in manifests match env vars in test setup
- [ ] RBAC in test manifests matches what test pods need
- [ ] Namespace creation correct (platform, platform-pipelines, platform-agents, per-project)
- [ ] Cleanup scripts actually remove all resources

_test-in-cluster.sh / test-in-pod.sh:_
- [ ] Test image built with correct context and Dockerfile
- [ ] Volume mounts give test pod access to everything it needs
- [ ] Env vars passed to test pod match what tests expect
- [ ] Test report generation correct (JUnit format, correct paths)
- [ ] Namespace isolation between test runs (RUN_ID based)
- [ ] Cleanup runs even on test failure

_install.sh:_
- [ ] OS/arch detection correct for all targets (Linux amd64/arm64, macOS, WSL)
- [ ] K0s / Docker Desktop installation is safe (checksums verified?)
- [ ] Helm chart deployment uses correct values
- [ ] Namespace creation and secret setup correct
- [ ] Idempotent: safe to run multiple times
- [ ] Error messages helpful for common failure modes
- [ ] No hardcoded versions that will go stale

_Justfile recipes:_
- [ ] All `just` recipes work (no broken commands or missing dependencies)
- [ ] Recipe dependencies correct (e.g., `build` depends on `ui`)
- [ ] Environment variable defaults reasonable
- [ ] Cross-platform compatibility (macOS + Linux)
- [ ] `just ci` / `just ci-full` correctly chain all checks

_CI/CD workflows:_
- [ ] All jobs have correct triggers (push, PR, release)
- [ ] Secrets used securely (not echoed, not in log-visible commands)
- [ ] Cache keys correct (Rust target, npm, etc.) — stale cache won't break builds
- [ ] Multi-arch build correct (QEMU setup, buildx platforms)
- [ ] Release workflow tags match Helm chart and Docker image tags
- [ ] Test jobs match what `just ci-full` runs locally
- [ ] Dependency between jobs correct (test before release)
- [ ] Concurrency limits to prevent resource exhaustion

_Security tooling:_
- [ ] `deny.toml` covers advisory DB, banned crates, license checks
- [ ] `.gitleaks.toml` patterns comprehensive (API keys, passwords, tokens)
- [ ] Pre-commit hooks enforced (no bypass path)
- [ ] Codecov token secure (if needed)
- [ ] Dependabot configured for all ecosystems (cargo, npm, github-actions)

**Output:** Numbered findings with severity, file:line, description, and fix.

---

### Agent 7: Integration Contracts — Cross-Component Seam Audit

**This is the most important agent.** It audits the *boundaries* where components touch.

**Scope:** Read contract-critical files from EVERY component:
- `src/config.rs` — all env vars the platform reads
- `src/api/mod.rs` — all API routes
- `src/api/sessions.rs`, `src/api/commands.rs` — agent session API
- `src/api/health.rs` — health endpoint
- `src/store/eventbus.rs` — Valkey pub-sub channels
- `src/agent/identity.rs` — agent credentials setup
- `src/agent/provider.rs` — image resolution
- `src/agent/service.rs` — pod spec construction
- `cli/agent-runner/src/main.rs`, `cli/agent-runner/src/transport.rs`, `cli/agent-runner/src/pubsub.rs`, `cli/agent-runner/src/control.rs`
- `mcp/lib/client.js`
- `ui/src/lib/api.ts`, `ui/src/lib/types.ts`, `ui/src/lib/ws.ts`
- `helm/platform/templates/configmap.yaml`, `helm/platform/templates/secret.yaml`, `helm/platform/templates/deployment.yaml`
- `helm/platform/values.yaml`
- `docker/Dockerfile`, `docker/Dockerfile.platform-runner`
- `hack/cluster-up.sh`, `hack/test-in-cluster.sh`

**Check every integration seam:**

_Seam 1: Platform API ↔ Agent Runner CLI_
- [ ] CLI's HTTP base URL construction matches platform's listen address/port
- [ ] CLI's API paths (`/api/sessions/*`, `/api/commands/*`) match platform's router
- [ ] Request/response JSON shapes match between CLI Rust structs and platform Rust structs
- [ ] WebSocket URL and message format match
- [ ] Auth token format (Bearer header) matches platform's AuthUser extractor
- [ ] Error code handling: CLI handles all error codes platform can return

_Seam 2: Platform API ↔ MCP Servers_
- [ ] MCP server endpoint paths match platform's API router
- [ ] JSON request/response shapes match
- [ ] Auth mechanism consistent (Bearer token via client.js)
- [ ] Pagination contract consistent

_Seam 3: Platform API ↔ Web UI_
- [ ] UI's `api.ts` endpoints match platform's router (every call has a target)
- [ ] UI's `types.ts` type definitions match platform's response types
- [ ] Auth flow (login, session, logout) sequences match
- [ ] WebSocket contract matches

_Seam 4: Platform ↔ Valkey (pub-sub channels)_
- [ ] Channel names in `eventbus.rs` match what CLI's `pubsub.rs` subscribes to
- [ ] Message types serialized by platform match what CLI deserializes
- [ ] ACL setup in `identity.rs` grants access to the right channels and key patterns
- [ ] Connection config (host, port, TLS, auth) consistent between components

_Seam 5: Platform ↔ K8s (pod specs)_
- [ ] Agent pod image references match what Docker builds produce
- [ ] Pod env vars match what agent-runner reads at startup
- [ ] Pod volume mounts match what agent-runner needs (git repos, MCP servers)
- [ ] SecurityContext (uid, gid, capabilities) matches Docker image user setup
- [ ] Resource limits in pod spec match Helm chart defaults
- [ ] Namespace in pod spec matches platform's namespace management

_Seam 6: Helm Chart ↔ Platform Config_
- [ ] Every env var in `src/config.rs` has a corresponding entry in Helm configmap/secret
- [ ] Default values match between Helm values.yaml and config.rs
- [ ] Port numbers consistent (deployment ports, service ports, config env vars)
- [ ] Volume mount paths match config env var defaults (OPS_REPOS_PATH, etc.)
- [ ] RBAC permissions cover all K8s operations the platform code performs
- [ ] Health check probe targets match actual health endpoint path

_Seam 7: Docker Images ↔ Helm/Kustomize_
- [ ] Image names in Helm deployment template match Docker build tags
- [ ] Exposed ports in Dockerfiles match Helm service/deployment ports
- [ ] Entrypoint/CMD matches what Helm deployment expects
- [ ] Volume mount paths in Helm match directory structure in Docker image

_Seam 8: CI/CD ↔ Everything_
- [ ] CI workflow builds same images as `just docker`
- [ ] CI test environment matches `just cluster-up` environment
- [ ] Release tags propagate correctly to Docker image tags and Helm chart appVersion
- [ ] CI cache keys include all relevant files (Cargo.lock, package-lock.json, etc.)

_Seam 9: Test Infrastructure ↔ Platform_
- [ ] Test cluster services (ports, config) match what test helpers connect to
- [ ] Test manifests (`hack/test-manifests/`) match production manifests (same Postgres version, etc.)
- [ ] Test env vars match production env var names
- [ ] Dev cluster setup matches CI cluster setup

**Output:** Numbered findings with severity, seam reference, file(s), description, and fix.

---

### Agent 8: Onboarding Templates & Demo Project — Content & Contract Audit

**Scope:** All files under `src/onboarding/`, `src/git/templates/`, `seed-commands/`

**Read ALL files. Also read `src/onboarding/demo_project.rs` to understand how templates are used.**

_Template content:_
- [ ] `.platform.yaml` templates are valid YAML that `src/pipeline/definition.rs` can parse
- [ ] Docker image references in templates exist and are pullable
- [ ] Deployment manifests in templates are valid K8s YAML
- [ ] Version references (v0.1, v0.2) are consistent and make sense as a progression
- [ ] Template variable substitution (if any) is safe — no injection
- [ ] `CLAUDE.md` template content is accurate and up-to-date with platform capabilities

_Demo project:_
- [ ] Demo project creation uses correct API calls (matches platform endpoints)
- [ ] Git repo initialization is correct (bare repo, default branch, initial commit)
- [ ] Template files placed at correct paths in the git repo
- [ ] Pipeline definition in demo project actually works when triggered
- [ ] Resource names are valid (K8s naming rules, git ref rules)

_Seed commands:_
- [ ] Seed command JSON is valid and matches expected schema
- [ ] Command descriptions are accurate
- [ ] No secrets or sensitive data in seed content

_CLAUDE.md templates:_
- [ ] Instructions are accurate for current platform version
- [ ] API endpoint references are correct
- [ ] Environment variable references match `src/config.rs`
- [ ] Build/deploy instructions work with current tooling

**Output:** Numbered findings with severity, file:line, description, and fix.

---

## Phase 2: Synthesis

Once all 8 agents return, synthesize into a single report.

### Synthesis rules

1. **Deduplicate** — merge same issue from multiple agents, keep highest severity
2. **Prioritize** — CRITICAL and HIGH first. Contract mismatches between components are always HIGH minimum.
3. **Categorize** — group findings by:
   - **Contract mismatches** (the core focus — API, env var, port, protocol disagreements)
   - **Security vulnerabilities** (secrets in images, missing auth, injection vectors)
   - **Deployment correctness** (Helm, Kustomize, Docker)
   - **CI/CD integrity** (build pipeline, test infrastructure)
   - **Component-level bugs** (within a single component)
   - **Consistency & drift** (components that should agree but don't)
4. **Be actionable** — every finding above LOW must have a concrete fix
5. **Credit good patterns** — note well-designed integration points
6. **Number every finding** — E1, E2, E3... (E for Ecosystem, distinguishing from A-prefix audit and R-prefix review)
7. **Tally statistics** — total findings by severity, per component, per seam

---

## Phase 3: Write Audit Report

Persist the report as `plans/ecosystem-audit-<YYYY-MM-DD>.md`.

### Report structure

```markdown
# Ecosystem Audit Report

**Date:** <today>
**Scope:** Platform ecosystem — CLI (~6K LOC), UI (~10K LOC), MCP (~2.8K LOC), Docker (4 images), Helm chart, infrastructure scripts, CI/CD
**Auditor:** Claude Code (automated)
**Pre-flight:** lint ✓/✗ | cli check ✓/✗ | ui build ✓/✗ | helm lint ✓/✗

## Executive Summary
- Overall ecosystem health: GOOD / NEEDS ATTENTION / CRITICAL ISSUES
- {2-3 sentences on integration contract health}
- Findings: X critical, Y high, Z medium, W low
- Top risks: {1-3 bullet points on broken seams}
- Strengths: {1-3 bullet points on well-designed integrations}

## Integration Seam Map

| Seam | Components | Status | Findings |
|---|---|---|---|
| Platform API ↔ CLI | api/, cli/ | ✓ aligned / ⚠ drift | E1, E5 |
| Platform API ↔ MCP | api/, mcp/ | ✓ aligned / ⚠ drift | E2 |
| Platform API ↔ UI | api/, ui/ | ✓ aligned / ⚠ drift | E3, E7 |
| Platform ↔ Valkey | eventbus, cli/pubsub | ✓ aligned / ⚠ drift | — |
| Platform ↔ K8s | agent/, helm/ | ✓ aligned / ⚠ drift | E4 |
| Helm ↔ Config | helm/, config.rs | ✓ aligned / ⚠ drift | E6 |
| Docker ↔ Helm | docker/, helm/ | ✓ aligned / ⚠ drift | — |
| CI ↔ Everything | workflows, Justfile | ✓ aligned / ⚠ drift | E8 |
| Test Infra ↔ Prod | hack/, helm/ | ✓ aligned / ⚠ drift | — |

## Component Statistics

| Component | Files | LOC | Critical | High | Medium | Low |
|---|---|---|---|---|---|---|
| Agent Runner CLI | N | ~6K | N | N | N | N |
| Web UI | N | ~10K | N | N | N | N |
| MCP Servers | N | ~2.8K | N | N | N | N |
| Docker Images | 4 | ~216 | N | N | N | N |
| Helm Chart | N | ~2.1K | N | N | N | N |
| Hack Scripts | N | ~1.7K | N | N | N | N |
| CI/CD | N | ~228 | N | N | N | N |
| Templates/Onboarding | N | N | N | N | N | N |
| **Cross-Component** | — | — | **N** | **N** | **N** | **N** |
| **Total** | **N** | **~24K** | **N** | **N** | **N** | **N** |

## Strengths
- {Good integration pattern 1 — which seam, why it's good}
- ...

## Critical & High Findings (must address)

### E1: [CRITICAL/HIGH] {title}
- **Seam:** {which integration boundary}
- **Components:** `component-a` ↔ `component-b`
- **Files:** `path/file1:42`, `path/file2:17`
- **Description:** {what's wrong — the contract mismatch}
- **Risk:** {what breaks in production}
- **Suggested fix:** {specific change in each component}
- **Found by:** Agent {N}

## Medium Findings (should address)

### EN: [MEDIUM] {title}
- **Seam:** {which boundary}
- **Files:** `path/file:42`
- **Description:** {what's wrong}
- **Suggested fix:** {approach}

## Low Findings (optional)

- [LOW] E{N}: `path/file:10` — {one-line} → {one-line fix}

## Component Health Summary

### Agent Runner CLI — {GOOD/NEEDS ATTENTION/CONCERNING}
{2-3 sentences on protocol alignment, transport, error handling}

### Web UI — {GOOD/NEEDS ATTENTION/CONCERNING}
{2-3 sentences on API contract, auth flow, security}

### MCP Servers — {GOOD/NEEDS ATTENTION/CONCERNING}
{2-3 sentences}

### Docker Images — {GOOD/NEEDS ATTENTION/CONCERNING}
{2-3 sentences on security, consistency}

### Helm Chart — {GOOD/NEEDS ATTENTION/CONCERNING}
{2-3 sentences on env var alignment, RBAC, networking}

### Infrastructure & CI/CD — {GOOD/NEEDS ATTENTION/CONCERNING}
{2-3 sentences}

### Templates & Onboarding — {GOOD/NEEDS ATTENTION/CONCERNING}
{2-3 sentences}

## Env Var Reconciliation Table

| Env Var | config.rs Default | Helm Default | Docker Default | Match? |
|---|---|---|---|---|
| PLATFORM_DEV | false | — | — | ✓/✗ |
| ... | ... | ... | ... | ... |

## Recommended Action Plan

### Immediate (this week)
1. {Fix critical contract mismatches}

### Short-term (this month)
1. {Fix high/medium findings}

### Long-term (backlog)
1. {Structural improvements to prevent drift}
```

### Rules
- Every finding gets a unique ID (E1, E2, ...) — the E-prefix distinguishes from `/audit` (A-prefix) and `/review` (R-prefix)
- **Contract mismatches are always HIGH minimum** — they cause runtime failures that no single-component test catches
- The env var reconciliation table is mandatory — it's the most common source of deployment bugs
- The report must be self-contained — readable without conversation context
- Do NOT include INFO-level items in the report
- Include the seam map even if all seams show aligned

---

## Phase 4: Summary to User

After writing the report, provide a concise summary:

1. Overall ecosystem health (one sentence)
2. Finding counts by severity
3. Seam map with status (one line per seam)
4. Top 3 most critical findings (one line each)
5. Top 3 strengths (one line each)
6. Path to the full report file
7. Suggested next steps

---

## Usage Notes

- This skill audits the **ecosystem around** the Rust monolith, not `src/` itself. For the monolith, use `/audit`.
- Run both `/audit` + `/audit-ecosystem` for a complete platform health check.
- Expect 10-20 minutes for the full ecosystem audit (8 parallel agents reading ~24K LOC).
- The audit is **read-only** — no files are modified. To fix findings, use `/dev`.
- For focused audits, tell the orchestrator which components or seams to audit — it can skip irrelevant agents.
- The integration seam audit (Agent 7) is the highest-value agent — if time is limited, run it alone.
