# Ecosystem Audit Report

**Date:** 2026-03-24
**Scope:** Platform ecosystem — CLI (~6K LOC), UI (~10K LOC), MCP (~2.8K LOC), Docker (4 images), Helm chart (~2.1K), infrastructure scripts (~1.7K), CI/CD (~228 LOC), templates/onboarding
**Auditor:** Claude Code (automated, 8 parallel agents)
**Pre-flight:** lint ✗ (520 errors — dirty working tree) | cli check ✓ | ui build ✓ | helm lint ✓

## Executive Summary

- **Overall ecosystem health: NEEDS ATTENTION**
- Integration contracts are mostly well-aligned — 7 of 9 seams are clean. The platform↔CLI pub-sub control channel has a latent JSON shape mismatch, and the Helm chart is missing RBAC permissions for Gateway API and ServiceAccounts. The most urgent issue is a **stored XSS vulnerability** in the UI's Markdown component. The MCP deploy server calls entirely non-existent API endpoints.
- **Findings: 3 critical, 14 high, 21 medium, 24 low**
- Top risks: (1) Stored XSS via unsanitized markdown, (2) `platform-deploy.js` MCP server is completely broken, (3) Helm RBAC missing Gateway API + ServiceAccount permissions
- Strengths: (1) Excellent pub-sub channel alignment across 3 codebases, (2) Smart Helm auto-wiring of env vars from ingress config, (3) Robust namespace isolation in test infrastructure

## Integration Seam Map

| Seam | Components | Status | Findings |
|---|---|---|---|
| Platform API ↔ CLI | api/, cli/ | ⚠ drift (control msg shape) | E3 |
| Platform API ↔ MCP | api/, mcp/ | ⚠ drift (deploy server broken) | E2, E10-E14 |
| Platform API ↔ UI | api/, ui/ | ✓ aligned | — |
| Platform ↔ Valkey | eventbus, cli/pubsub | ✓ aligned | — |
| Platform ↔ K8s | agent/, helm/ | ⚠ drift (RBAC gaps) | E4, E5 |
| Helm ↔ Config | helm/, config.rs | ⚠ drift (missing env vars) | E6, E7, E8, E9 |
| Docker ↔ Helm | docker/, helm/ | ✓ aligned | — |
| CI ↔ Everything | workflows, Justfile | ✓ aligned | — |
| Test Infra ↔ Prod | hack/, helm/ | ✓ aligned | — |

## Component Statistics

| Component | Files | LOC | Critical | High | Medium | Low |
|---|---|---|---|---|---|---|
| Agent Runner CLI | 11 | ~6K | 0 | 1 | 2 | 3 |
| Web UI | ~40 | ~10K | 1 | 2 | 4 | 3 |
| MCP Servers | ~12 | ~2.8K | 1 | 5 | 4 | 3 |
| Docker Images | 4+2 | ~350 | 1 | 4 | 4 | 4 |
| Helm Chart + Kustomize | ~20 | ~2.1K | 1 | 3 | 3 | 3 |
| Infra Scripts & CI/CD | ~25 | ~2K | 0 | 2 | 4 | 7 |
| Templates/Onboarding | ~15 | ~1.5K | 0 | 2 | 3 | 5 |
| **Cross-Component (Seams)** | — | — | 0 | 3 | 2 | 1 |
| **Total (deduplicated)** | **~130** | **~26K** | **3** | **14** | **21** | **24** |

## Strengths

1. **Pub-sub channel contract alignment** — `session:{id}:events` and `session:{id}:input` channels are consistent across platform Rust, CLI Rust, and MCP JS. Valkey ACL scoping (`&session:{id}:*`) correctly restricts agents. Three codebases, zero drift.

2. **Helm auto-wiring** — WebAuthn RP ID/Origin, CORS origins, PLATFORM_API_URL, and DATABASE_URL are automatically derived from ingress and release name. Reduces misconfiguration risk dramatically.

3. **Test infrastructure namespace isolation** — Every test run gets a `RUN_ID`-scoped namespace prefix with thorough `trap cleanup EXIT INT TERM` handlers. Worktree-aware caching prevents cross-worktree contamination.

4. **CLI secret isolation** — `env_clear()` + whitelist in `transport.rs:build_env()` prevents leaking `DATABASE_URL`, `PLATFORM_MASTER_KEY` to Claude CLI subprocess. `RESERVED_ENV_VARS` blocks proxy/TLS trust injection.

5. **Cookie-based auth in UI** — UI uses `credentials: 'include'` with HttpOnly session cookies rather than localStorage tokens. No tokens accessible to JavaScript.

6. **Multi-stage Docker builds** — Platform image uses cargo-chef for caching, final image is minimal (debian-slim + git + ca-certs + binary). No build toolchain leaks.

7. **MCP shared client library** — Clean separation in `mcp/lib/client.js` with consistent auth injection, error handling, and env-based configuration.

8. **Smart checksum-based build caching** — `hack/build-agent-images.sh` uses SHA-256 source checksums to skip unnecessary rebuilds.

9. **Forward-compatible event system** — Platform's `ProgressKind::Unknown` serde fallback and CLI's subset-enum pattern allow new event types without breaking existing clients.

10. **Helm secret upgrade safety** — `lookup` function preserves auto-generated secrets across `helm upgrade`, preventing DB password rotation.

---

## Critical Findings (must fix immediately)

### E1: [CRITICAL] Stored XSS via unsanitized Markdown rendering
- **Component:** Web UI
- **File:** `ui/src/components/Markdown.tsx:5`
- **Description:** The `Markdown` component uses `marked.parse()` and injects the result via `dangerouslySetInnerHTML` with zero sanitization. `marked` v15 does NOT sanitize HTML by default (the `sanitize` option was removed in v1.0). Any user-generated content rendered through this component — issue bodies, MR descriptions, comments, reviews — can execute arbitrary JavaScript.
- **Risk:** Any authenticated user can inject `<img onerror=...>`, `<script>`, or event handler payloads that execute in other users' sessions. Enables session hijacking, admin privilege escalation, and data exfiltration.
- **Attack surface:** `IssueDetail.tsx` (lines 69, 77), `MRDetail.tsx` (lines 82, 99, 129), `admin/Commands.tsx` (line 147)
- **Suggested fix:** Add DOMPurify: `npm install dompurify && import DOMPurify from 'dompurify'; const html = DOMPurify.sanitize(marked.parse(content, { async: false }) as string);`
- **Found by:** Agent 3 (Web UI)

### E2: [CRITICAL] MCP deploy server calls entirely non-existent API endpoints
- **Seam:** Platform API ↔ MCP
- **Component:** MCP Servers
- **File:** `mcp/servers/platform-deploy.js:180-224`
- **Description:** The entire `platform-deploy.js` server's mental model (environment-keyed deployments with rollback) does not match the platform's actual API model (target + release-based progressive delivery). Every tool call returns 404:
  - `GET /api/projects/{id}/deployments` → actual: `GET /api/projects/{id}/targets`
  - `GET /api/projects/{id}/deployments/{environment}` → actual: `GET /api/projects/{id}/targets/{target_id}`
  - `POST /api/projects/{id}/deployments/{environment}/rollback` → actual: `POST /api/projects/{id}/deploy-releases/{release_id}/rollback`
  - `GET /api/projects/{id}/previews` → no equivalent endpoint exists
- **Risk:** All deployment-related MCP tools are completely broken. Agents using these tools get 404 errors for every operation.
- **Suggested fix:** Rewrite `platform-deploy.js` to use the actual API: `/api/projects/{id}/targets`, `/api/projects/{id}/deploy-releases`, `/api/projects/{id}/deploy-releases/{release_id}/rollback`
- **Found by:** Agent 2 (MCP Servers)

### E3a: [CRITICAL] Helm PLATFORM_MASTER_KEY generated with invalid characters
- **Seam:** Helm ↔ Config
- **Component:** Helm Chart
- **File:** `helm/platform/templates/secret.yaml:50`
- **Description:** The secrets engine (`src/secrets/engine.rs:14`) calls `hex::decode()` on the master key, requiring exactly 64 hexadecimal characters (0-9, a-f). The Helm template uses `randAlphaNum 64` which produces characters from `[A-Za-z0-9]`. Any key containing letters G-Z (highly likely) fails with `"invalid PLATFORM_MASTER_KEY hex"`, breaking the entire secrets subsystem on first Helm install.
- **Risk:** Secrets engine completely broken on every fresh Helm deployment.
- **Suggested fix:** Replace `randAlphaNum 64` with a hex-generating expression. Simplest: pipe through sha256sum and truncate.
- **Found by:** Agent 5 (Helm Chart)

---

## High Findings (fix before release)

### E3b: [HIGH] Control message JSON shape mismatch between platform and CLI
- **Seam:** Platform API ↔ CLI
- **Components:** `src/agent/pubsub_bridge.rs:64` ↔ `cli/agent-runner/src/pubsub.rs:41-49`
- **Description:** Platform's `publish_control()` sends `{"type":"control","control_type":"interrupt"}` but CLI expects `{"type":"control","control":{"type":"interrupt"}}` (nested `ControlPayload`). Structurally incompatible.
- **Risk:** Agent interrupt/control commands silently fail. Mitigated: `publish_control()` is currently `#[allow(dead_code)]` and never called. Latent bug.
- **Suggested fix:** Align `publish_control()` to send `{"type":"control","control":{"type":"<control_type>"}}`.
- **Found by:** Agents 1, 7

### E4: [HIGH] Helm ClusterRole missing ServiceAccount RBAC permission
- **Seam:** Helm ↔ Config
- **Components:** `helm/platform/templates/clusterrole.yaml` ↔ `src/deployer/namespace.rs:119`
- **Description:** Platform creates `ServiceAccount` objects in per-project/session namespaces. ClusterRole grants core `""` apiGroup only for pods, services, configmaps, secrets, PVCs, namespaces — `serviceaccounts` is missing.
- **Risk:** Agent session creation fails with 403 in Helm-deployed production.
- **Suggested fix:** Add `serviceaccounts` to the core apiGroup resources list in ClusterRole.
- **Found by:** Agents 5, 7

### E5: [HIGH] Helm ClusterRole missing Gateway API (gateway.networking.k8s.io) permissions
- **Seam:** Helm ↔ Config
- **Components:** `helm/platform/templates/clusterrole.yaml` ↔ `src/deployer/gateway.rs`, `src/deployer/applier.rs:49-51`
- **Description:** Deployer creates `HTTPRoute` resources via `gateway.networking.k8s.io/v1`. ClusterRole has no rule for this API group.
- **Risk:** Progressive delivery traffic splitting (canary deployments) fails with 403 in production.
- **Suggested fix:** Add `apiGroups: ["gateway.networking.k8s.io"], resources: ["httproutes", "gateways"], verbs: ["get", "list", "watch", "create", "update", "patch", "delete"]`.
- **Found by:** Agents 5, 6, 7

### E6: [HIGH] Helm configmap missing PLATFORM_PIPELINE_NAMESPACE and PLATFORM_AGENT_NAMESPACE
- **Seam:** Helm ↔ Config
- **Components:** `helm/platform/templates/configmap.yaml` ↔ `src/config.rs:132-135`
- **Description:** `config.rs` reads these with defaults `platform-pipelines` and `platform-agents`. Helm never sets them. If deployed to namespace `my-platform`, pipeline pods spawn in `platform-pipelines` which may not exist.
- **Risk:** Pipeline/agent pods fail to spawn in non-default namespace deployments.
- **Suggested fix:** Add to configmap, defaulting to `{{ .Release.Namespace }}-pipelines` / `-agents`.
- **Found by:** Agent 5

### E7: [HIGH] Main platform Docker image runs as root
- **Component:** Docker Images
- **File:** `docker/Dockerfile:43-54`
- **Description:** Final stage has no `USER` directive. All other images (runner, runner-bare, dev-pod) correctly configure non-root users.
- **Risk:** Container compromise gives attacker full root privileges.
- **Suggested fix:** `RUN useradd -r -s /sbin/nologin -u 1000 platform && USER platform`. Ensure `/data/` owned by new user.
- **Found by:** Agent 4
- **Implementation note — writable directories & capabilities required:**
  The platform binary needs write access to everything under `/data` (PVC mount):
  - `/data/repos` — bare git repos (created on project creation, written on every push, worktree merges)
  - `/data/ops-repos` — deployer Kustomize overlay repos (created/updated by reconciler)
  - `/data/ssh_host_ed25519_key` — SSH host key (generated on first startup if absent)
  - `/data/agent-runner/` — copied by init container, read-only after
  - `/data/mcp-servers.tar.gz` — copied by init container, read-only after
  - `/data/seed-images/`, `/data/seed-commands/` — copied by init container, read-only after

  The platform also invokes `git` (init, receive-pack, upload-pack, worktree, merge) which needs to execute as the same user that owns the repo directories.

  **Recommended approach:** Use K8s `fsGroup` in the pod securityContext rather than `chown` in the Dockerfile. This makes the PVC writable by the platform user regardless of who created the files:
  ```yaml
  # In helm/platform/templates/deployment.yaml, under spec.template.spec:
  securityContext:
    runAsUser: 1000
    runAsGroup: 1000
    fsGroup: 1000
  ```
  With `fsGroup: 1000`, Kubernetes automatically sets group ownership on the PVC mount to GID 1000, making all files writable by the platform user — including files copied by the init container (which can remain root). The init container needs its own `securityContext.runAsUser: 0` override since it runs from the same image but needs to copy files before the fsGroup takes effect.

  The Dockerfile changes are minimal:
  ```dockerfile
  RUN useradd -r -s /sbin/nologin -u 1000 -g 1000 platform
  USER 1000
  ```
  No `chown` needed — fsGroup handles it at pod level.

### E8: ~~[HIGH]~~ → [ACCEPTED] Agent runner image has passwordless sudo (NOPASSWD: ALL)
- **Component:** Docker Images
- **File:** `docker/Dockerfile.platform-runner:57`
- **Description:** Agent user gets unrestricted sudo inside the container.
- **Status:** Accepted design trade-off. See `docs/design-decisions.md` DD-1 for full rationale.
- **Summary:** Agents need to install arbitrary tooling to complete open-ended tasks. The isolation boundary is the container + namespace + NetworkPolicy, not the UID inside the container. Containers are ephemeral, single-tenant, resource-limited, and network-restricted. The user's own API key drives the agent. Comparable to giving a developer a VM with sudo.
- **Review trigger:** Revisit if multi-tenant shared clusters are added.
- **Found by:** Agent 4

### E9: [HIGH] Kaniko image and Claude CLI use unpinned `:latest` tags
- **Component:** Docker Images
- **Files:** `docker/Dockerfile.platform-runner:22` (kaniko), `docker/Dockerfile.platform-runner:48` (claude-code), `docker/Dockerfile.platform-runner-bare:5` (kaniko)
- **Description:** `gcr.io/kaniko-project/executor:latest` and `npm install -g @anthropic-ai/claude-code` (no version) are non-reproducible.
- **Risk:** Supply-chain attack vector; breaking changes silently propagate.
- **Suggested fix:** Pin: `gcr.io/kaniko-project/executor:v1.23.2@sha256:<digest>` and `@anthropic-ai/claude-code@1.x.y`.
- **Found by:** Agent 4

### E10: [HIGH] MCP `create_merge_request` sends unsupported `labels` field
- **Seam:** Platform API ↔ MCP
- **Files:** `mcp/servers/platform-issues.js:139-155` ↔ `src/api/merge_requests.rs:26-33`
- **Description:** `CreateMrRequest` only accepts `source_branch`, `target_branch`, `title`, `body`, `auto_merge`. MCP sends `labels` — silently ignored. Users think labels are applied.
- **Suggested fix:** Remove `labels` from `create_merge_request` and `update_merge_request` tool schemas.
- **Found by:** Agent 2

### E11: [HIGH] MCP `create_merge_request` doesn't require `target_branch` but platform does
- **Files:** `mcp/servers/platform-issues.js:155` ↔ `src/api/merge_requests.rs:26`
- **Description:** `target_branch: String` is non-optional in platform. MCP schema says `required: ["title", "source_branch"]`, omitting `target_branch`. Missing field → 422 error.
- **Suggested fix:** Add `"target_branch"` to the `required` array.
- **Found by:** Agent 2

### E12: [HIGH] MCP `create_role` sends `permissions` but platform ignores them
- **Files:** `mcp/servers/platform-admin.js:117` ↔ `src/api/admin.rs:38-41`
- **Description:** `CreateRoleRequest` only accepts `name` and `description`. Permissions set separately via `PUT /api/admin/roles/{id}/permissions`. MCP sends permissions in create body — silently dropped.
- **Suggested fix:** Make a follow-up PUT call after role creation.
- **Found by:** Agent 2

### E13: [HIGH] MCP `remove_role` parameter name mismatch
- **Files:** `mcp/servers/platform-admin.js:258-261`
- **Description:** MCP tool uses `role_assignment_id` but platform route expects `role_id`. Semantically different — misleading.
- **Suggested fix:** Rename `role_assignment_id` to `role_id` in tool schema.
- **Found by:** Agent 2

### E14: [HIGH] MCP `list_users` advertises `search` but platform doesn't support it
- **Files:** `mcp/servers/platform-admin.js:29`
- **Description:** Platform's `ListParams` has only `limit`/`offset`. `search` parameter silently ignored.
- **Suggested fix:** Remove `search` from tool schema.
- **Found by:** Agent 2

### E15: [HIGH] `install.sh` downloads binaries with no checksum verification
- **Component:** Infrastructure
- **File:** `install.sh:97,143`
- **Description:** k0s via `curl | sudo sh`, Helm via `curl | bash` — no integrity verification. MITM → root code execution.
- **Suggested fix:** Download to temp file, verify checksum, then execute.
- **Found by:** Agent 6

### E16: [HIGH] UI iframes without sandbox attribute
- **Component:** Web UI
- **Files:** `ui/src/pages/ProjectDetail.tsx:103`, `ui/src/components/ProjectCard.tsx:77`
- **Description:** Iframes render `preview_url` without `sandbox` attribute. `IframePanel.tsx` correctly uses `sandbox="allow-scripts allow-same-origin allow-forms allow-popups"`, but these two locations don't.
- **Risk:** Unsandboxed iframe from untrusted URL can access parent page DOM, cookies, session.
- **Suggested fix:** Add `sandbox="allow-scripts allow-same-origin allow-forms allow-popups"` to both.
- **Found by:** Agent 3

### E17: [HIGH] Entrypoint `git add -A` may commit API token to git
- **Component:** Docker Images
- **Files:** `docker/entrypoint.sh:83-89,101-104`
- **Description:** Entrypoint writes `PLATFORM_API_TOKEN` to `/workspace/.platform/.env` (plaintext, default perms), then runs `git add -A` which stages it.
- **Risk:** Secrets pushed to git remote.
- **Suggested fix:** Add `.platform/` to `.gitignore` before `git add`, or `git add -A ':!.platform/'`. Set file perms to 0600.
- **Found by:** Agent 4

---

## Medium Findings (fix when touching the area)

### E18: [MEDIUM] Helm NetworkPolicy egress blocks preview proxy traffic
- **File:** `helm/platform/templates/networkpolicy-platform.yaml:54-68`
- **Description:** Egress only allows private-IP traffic on ports 5432, 6379, 9000, 53, 443, 6443. Preview proxying to pod port 8000 is blocked.
- **Found by:** Agent 5
- **Implementation notes:**
  The platform proxies to two kinds of preview services:
  1. **Agent session previews** — `preview-{short_id}.{session_ns}.svc.cluster.local:8000`
  2. **Deploy previews** — services labeled `platform.io/component=iframe-preview` in project deploy namespaces, arbitrary port from Service spec

  All platform-managed namespaces already carry `platform.io/managed-by: platform` (set by `build_namespace_object()` in `src/deployer/namespace.rs:258-263`). The platform namespace itself does NOT have this label (created by Helm).

  **Fix:** Add a namespace-selector-scoped egress rule allowing all ports to platform-managed namespaces:
  ```yaml
  # Preview proxy — egress to platform-managed namespaces (agent sessions, deploy previews)
  - to:
      - namespaceSelector:
          matchLabels:
            platform.io/managed-by: platform
  ```
  No port restriction needed — preview services may listen on any port (8000, 8080, 80, 3000, etc.). The namespace label is the security boundary: these namespaces are ephemeral, single-tenant, and platform-controlled. Adding pod-level selectors would be too tight (agent session preview Services don't carry the `iframe-preview` label, and NetworkPolicy `podSelector` matches Pods not Services so backing pod ports may differ from Service ports).

### E19: [MEDIUM] Kustomize readinessProbe uses `/healthz` instead of `/readyz`
- **File:** `deploy/base/deployment.yaml:41`
- **Description:** Platform has separate `/readyz` endpoint checking DB/Valkey/MinIO. Kustomize uses `/healthz` for readiness — pod marked ready even if backends are down. Helm chart is correct.
- **Suggested fix:** Change `path: /healthz` to `path: /readyz`.
- **Found by:** Agent 5

### E20: [MEDIUM] Kustomize base leaks DB credentials in ConfigMap
- **File:** `deploy/base/configmap.yaml:8,12`
- **Description:** `DATABASE_URL` (contains password) and `MINIO_SECRET_KEY` in ConfigMap (plain text). Helm correctly uses Secret.
- **Suggested fix:** Move to a Secret resource.
- **Found by:** Agent 5

### E21: [MEDIUM] Kustomize rbac.yaml not included in kustomization.yaml resources
- **File:** `deploy/base/kustomization.yaml:6-9`
- **Description:** `rbac.yaml` exists but is not listed in resources. Deployment references `serviceAccountName: platform` but ServiceAccount never created.
- **Suggested fix:** Add `- rbac.yaml` to resources list.
- **Found by:** Agent 5

### E22: [MEDIUM] CLI secrets written to disk without restrictive permissions
- **File:** `cli/agent-runner/src/main.rs:205-206`
- **Description:** `/workspace/.env.dev` created with umask-dependent permissions (typically 0644). Any process on pod can read secrets.
- **Suggested fix:** Set `0o600` via `std::os::unix::fs::PermissionsExt`.
- **Found by:** Agent 1

### E23: [MEDIUM] CLI pub-sub subscriber has no reconnection logic
- **File:** `cli/agent-runner/src/pubsub.rs:331-364`
- **Description:** Subscriber exits permanently on disconnect. If Valkey restarts, agent becomes deaf to all input.
- **Suggested fix:** Add reconnection with backoff.
- **Found by:** Agent 1

### E24: [MEDIUM] 3 of 6 MCP servers lack try/catch wrapper for tool calls
- **Files:** `mcp/servers/platform-core.js`, `mcp/servers/platform-issues.js`, `mcp/servers/platform-pipeline.js`
- **Description:** Missing try/catch around `CallToolRequestSchema` handler. Network/parse errors propagate as MCP protocol errors rather than tool errors with `isError: true`. Other 3 servers handle this correctly.
- **Suggested fix:** Add try/catch matching pattern in `platform-admin.js`.
- **Found by:** Agent 2

### E25: [MEDIUM] MCP client sends Content-Type: application/json on GET requests
- **File:** `mcp/lib/client.js:43`
- **Description:** Header set on all requests including GET/DELETE. Some proxies/WAFs reject this.
- **Suggested fix:** Only set when `body !== undefined`.
- **Found by:** Agent 2

### E26: [MEDIUM] MCP client has no request timeout
- **File:** `mcp/lib/client.js:50`
- **Description:** `fetch()` has no `AbortController` timeout. Platform unresponsive → tool hangs indefinitely.
- **Suggested fix:** Add 30s timeout via `AbortController`.
- **Found by:** Agent 2

### E27: [MEDIUM] MCP `list_alerts` sends `status` filter but platform expects `enabled` (boolean)
- **File:** `mcp/servers/platform-observe.js:131-148`
- **Description:** MCP sends `status` (string), platform accepts `enabled` (boolean). Silently ignored.
- **Suggested fix:** Replace `status` with `enabled: boolean` in tool schema.
- **Found by:** Agent 2

### E28: [MEDIUM] UI `AccountSettings` password change doesn't verify current password
- **File:** `ui/src/pages/AccountSettings.tsx:36`
- **Description:** `currentPw` collected in state but never sent to server. Attacker with session can change password without knowing current one.
- **Suggested fix:** Send `current_password` in request body; validate server-side.
- **Found by:** Agent 3

### E29: [MEDIUM] UI `admin/Tokens.tsx` API response type mismatch
- **File:** `ui/src/pages/admin/Tokens.tsx:15`
- **Description:** Calls `api.get<ApiToken[]>('/api/tokens')` but server returns `ListResponse<ApiToken>` with `{ items, total }`. Tokens page likely shows nothing.
- **Suggested fix:** Use `api.get<ListResponse<ApiToken>>` and access `.items`.
- **Found by:** Agent 3

### E30: [MEDIUM] Rust version mismatch across Dockerfiles
- **Files:** `docker/Dockerfile:10` (1.88), `docker/Dockerfile.dev-pod:8` (1.93), `docker/Dockerfile.platform-runner:8` (floating `1-slim`)
- **Description:** Three different Rust versions. Could cause compilation differences.
- **Suggested fix:** Align to a single pinned version. Use the one which is currenlty installed on my mac
- **Found by:** Agent 4

### E31: [MEDIUM] kubectl downloaded without checksum verification in Docker images
- **Files:** `docker/Dockerfile.platform-runner:33`, `docker/Dockerfile.dev-pod:34`
- **Description:** kubectl from `dl.k8s.io` with floating `stable.txt` version, no checksum.
- **Suggested fix:** Pin version and verify SHA-256 checksum.
- **Found by:** Agent 4

### E32: [MEDIUM] `just agent-images` overwrites full runner with bare image tag
- **File:** `Justfile:266-268`
- **Description:** Both full and bare runner built with same `platform-runner:latest` tag. Bare overwrites full.
- **Suggested fix:** Use distinct tags: `platform-runner:latest` and `platform-runner-bare:latest`.
- **Found by:** Agent 4

### E33: [MEDIUM] Test manifests use unpinned image tags
- **Files:** `hack/test-manifests/minio.yaml:12` (minio:latest), `hack/deploy-services.sh:74` (socat:latest)
- **Suggested fix:** Pin to specific versions.
- **Found by:** Agent 6

### E34: [MEDIUM] Test RBAC missing Gateway API and apps API group permissions
- **File:** `hack/test-manifests/rbac.yaml`
- **Description:** Missing `gateway.networking.k8s.io` and `apps` (deployments) permissions. E2E tests exercising deployer will get 403.
- **Suggested fix:** Add rules for both API groups.
- **Found by:** Agent 6

### E35: [MEDIUM] Release workflow publishes images on every push to main without CI gate
- **File:** `.github/workflows/release.yaml`
- **Description:** Triggers independently of CI. Broken commit still gets a Docker image published.
- **Suggested fix:** Gate release behind CI passing via `workflow_run` or tag-based triggers.
- **Found by:** Agent 6

### E36: [MEDIUM] .gitleaks.toml allowlists entire `test-in-cluster.sh`
- **File:** `.gitleaks.toml`
- **Description:** Any actual secret added to the file would be ignored.
- **Suggested fix:** Use targeted pattern-based allowlist.
- **Found by:** Agent 6

### E37: [MEDIUM] Git template CLAUDE.md `deploy.enable_staging` misleading
- **File:** `src/git/templates/CLAUDE.md:519`
- **Description:** Tells agents to set `deploy.enable_staging: true` in `.platform.yaml`, but staging is controlled via `include_staging` DB column set via API.
- **Suggested fix:** Update to reference project settings API. USERNOTE: JUST remove the mention (dev agent should not be able to decide whether to use staging env or not, remove the reference in git template claude md)
- **Found by:** Agent 8

### E38: [MEDIUM] Dead `platform.yaml` template references deleted `Dockerfile.canary`
- **File:** `src/onboarding/templates/platform.yaml:14,17`
- **Description:** References deleted files. Unused by Rust code but confusing.
- **Suggested fix:** Delete the dead file.
- **Found by:** Agent 8

---

## Low Findings (optional)

- **E39** [LOW] `cli/agent-runner/src/render.rs:111` — AppleScript injection possible if user-controlled strings passed (currently safe — hardcoded callers). Fix: use closed enum for notification kinds.
- **E40** [LOW] `cli/agent-runner/src/main.rs:199` — `PLATFORM_SECRET_NAMES` allows reading arbitrary env vars. Currently platform-controlled. Fix: cross-check against RESERVED_ENV_VARS.
- **E41** [LOW] `cli/agent-runner/src/repl.rs:276` — 600s init timeout may be too generous for pod mode. Fix: reduce to 120-180s.
- **E42** [LOW] `cli/agent-runner/src/pubsub.rs:223` — Inconsistent truncation semantics (bytes vs chars) between modules.
- **E43** [LOW] `ui/src/components/Layout.tsx:32` — `dangerouslySetInnerHTML` for SVG icons (static data, low risk).
- **E44** [LOW] `ui/src/pages/admin/Health.tsx:139` — Dead localStorage token code for SSE; SSE uses cookies.
- **E45** [LOW] `ui/src/pages/observe/Traces.tsx:237` — "View related logs" links by `span_id` instead of `trace_id`.
- **E46** [LOW] Multiple UI pages — `.catch(() => {})` silently swallows API errors. Users see stale data with no feedback.
- **E47** [LOW] `ui/package.json` — No `dompurify` dependency (confirms E1 has no sanitization).
- **E48** [LOW] `mcp/servers/platform-pipeline.js:42` (test) — Test passes `branch` but tool expects `git_ref`.
- **E49** [LOW] `mcp/tests/test-core.js:32` — Tool list assertion doesn't check all 12 tools.
- **E50** [LOW] `mcp/servers/platform-browser.js:20` — `JSON.parse` of env var at module load without try/catch.
- **E51** [LOW] `mcp/lib/client.js:65` — `JSON.parse(text)` on success response without try/catch.
- **E52** [LOW] `docker/Dockerfile.platform-runner:52` — `npm install --production` deprecated; use `npm ci --omit=dev`.
- **E53** [LOW] `docker/Dockerfile.dev-pod:53` — `pip install --break-system-packages`; use pipx instead.
- **E54** [LOW] `docker/Dockerfile.dev-pod:34` — kubectl hardcoded to amd64 only.
- **E55** [LOW] `docker/entrypoint.sh:94` — `--dangerously-skip-permissions` used unconditionally on Claude CLI.
- **E56** [LOW] `.github/workflows/release.yaml:14` — `:latest` tag overwritten on every main push.
- **E57** [LOW] `.github/dependabot.yml` — Missing npm entries for `mcp/` and `docs/viewer/`; missing Cargo entry for `cli/agent-runner`.
- **E58** [LOW] `hack/kind-up.sh` — Legacy script duplicates `cluster-up.sh` functionality.
- **E59** [LOW] `hack/test-in-cluster.sh:147` — NodePort connectivity wait loop doesn't fail on timeout.
- **E60** [LOW] `install.sh:233` — Fallback Helm repo URL `charts.agentsphere.dev` may not exist.
- **E61** [LOW] `src/git/templates/Dockerfile:6` — Copies `requirements.txt`, `app/`, `static/` not present in initial template.
- **E62** [LOW] `src/onboarding/templates/deploy/postgres.yaml:26` — Hardcoded `changeme` password in deploy manifests.

---

## Component Health Summary

### Agent Runner CLI — GOOD
Protocol alignment with platform is excellent. Pub-sub channels, event shapes, and auth are all consistent. One latent control message mismatch (dead code). Secret handling and process lifecycle are robust. No `.unwrap()` in production code. `fred` version matches platform.

### Web UI — NEEDS ATTENTION
API contract alignment is good (types generated from Rust via ts-rs, consistent pagination). However, the stored XSS vulnerability in the Markdown component is critical and must be fixed immediately. Unsandboxed iframes and missing current-password verification on password change are also concerning. Auth flow and cookie handling are well-designed.

### MCP Servers — NEEDS ATTENTION
The entire deploy server (`platform-deploy.js`) is broken against the current API. Multiple parameter mismatches in admin and issues servers (labels, target_branch, permissions, search). Three of six servers lack error handling wrappers. The shared client is well-designed but needs timeout support.

### Docker Images — NEEDS ATTENTION
Main platform image runs as root (only image that does). Agent runner has passwordless sudo. Multiple unpinned dependencies (kaniko, Claude CLI, kubectl, Rust). Entrypoint may commit API tokens to git. Multi-stage builds and cross-compilation are well-done.

### Helm Chart — NEEDS ATTENTION
PLATFORM_MASTER_KEY generation uses wrong character set (will break secrets on every fresh install). Missing RBAC for ServiceAccounts and Gateway API. Missing env vars for pipeline/agent namespaces and gateway config. Auto-wiring of database/Valkey/MinIO URLs is excellent. Secret upgrade safety via `lookup` is well-designed.

### Infrastructure & CI/CD — GOOD
Scripts are well-structured with proper trap handlers and namespace isolation. Main concerns: install.sh downloads without verification, release workflow lacks CI gate. Test infrastructure alignment with platform is excellent. deny.toml and pre-commit coverage are thorough.

### Templates & Onboarding — GOOD
Demo project progression (v0.1→v0.2 with progressive delivery) is well-designed. Unit test coverage for templates is thorough. Dead `platform.yaml` template and CLAUDE.md staging docs should be cleaned up. Hardcoded dev passwords are acceptable for demos.

---

## Env Var Reconciliation Table

| Env Var | config.rs Default | Helm Default | Match? |
|---|---|---|---|
| `PLATFORM_LISTEN` | `0.0.0.0:8080` | `0.0.0.0:8080` | ✓ |
| `DATABASE_URL` | `postgres://...localhost...` | auto-wired | ✓ |
| `VALKEY_URL` | `redis://localhost:6379` | auto-wired | ✓ |
| `MINIO_ENDPOINT` | `http://localhost:9000` | auto-wired | ✓ |
| `MINIO_ACCESS_KEY` | `platform` | from minio.auth | ✓ |
| `MINIO_SECRET_KEY` | `devdevdev` | from minio.auth | ✓ |
| `PLATFORM_MASTER_KEY` | None (optional) | **`randAlphaNum 64`** | **✗ (E3a)** |
| `PLATFORM_GIT_REPOS_PATH` | `/data/repos` | `/data/repos` | ✓ |
| `PLATFORM_OPS_REPOS_PATH` | `/data/ops-repos` | `/data/ops-repos` | ✓ |
| `PLATFORM_SECURE_COOKIES` | `false` | `false` | ✓ |
| `PLATFORM_CORS_ORIGINS` | `""` | auto-wired from ingress | ✓ |
| `PLATFORM_TRUST_PROXY` | `false` | `true` | ⚠ intentional |
| `PLATFORM_DEV` | `false` | `false` | ✓ |
| `PLATFORM_PERMISSION_CACHE_TTL` | `300` | `300` | ✓ |
| `PLATFORM_NAMESPACE` | `platform` | `.Release.Namespace` | ✓ |
| `PLATFORM_PIPELINE_NAMESPACE` | `platform-pipelines` | **missing** | **✗ (E6)** |
| `PLATFORM_AGENT_NAMESPACE` | `platform-agents` | **missing** | **✗ (E6)** |
| `PLATFORM_VALKEY_AGENT_HOST` | derived from VALKEY_URL | **missing** | **✗** |
| `PLATFORM_GATEWAY_NAME` | `platform-gateway` | **missing** | **✗** |
| `PLATFORM_GATEWAY_NAMESPACE` | = PLATFORM_NAMESPACE | **missing** | **✗** |
| `PLATFORM_API_URL` | `http://platform...svc:8080` | auto-wired | ✓ |
| `PLATFORM_SSH_HOST_KEY_PATH` | `/data/ssh_host_ed25519_key` | `/data/ssh_host_ed25519_key` | ✓ |
| `PLATFORM_AGENT_RUNNER_DIR` | `/data/agent-runner` | `/data/agent-runner` | ✓ |
| `PLATFORM_MCP_SERVERS_TARBALL` | `/data/mcp-servers.tar.gz` | `/data/mcp-servers.tar.gz` | ✓ |
| `PLATFORM_CLI_SPAWN_ENABLED` | `true` | `true` | ✓ |
| `PLATFORM_SEED_IMAGES_PATH` | `/data/seed-images` | `/data/seed-images` | ✓ |
| `PLATFORM_SEED_COMMANDS_PATH` | `/data/seed-commands` | `/data/seed-commands` | ✓ |
| `PLATFORM_HEALTH_CHECK_INTERVAL` | `15` | `15` | ✓ |
| `PLATFORM_SELF_OBSERVE_LEVEL` | `warn` | `warn` | ✓ |
| `PLATFORM_SESSION_IDLE_TIMEOUT` | `1800` | `1800` | ✓ |
| `PLATFORM_PIPELINE_MAX_PARALLEL` | `4` | `4` | ✓ |
| `WEBAUTHN_RP_ID` | `localhost` | auto-wired | ✓ |
| `WEBAUTHN_RP_ORIGIN` | `http://localhost:8080` | auto-wired | ✓ |
| `WEBAUTHN_RP_NAME` | `Platform` | `Platform` | ✓ |

---

## Recommended Action Plan

### Immediate (this week)
1. **E1** — Add DOMPurify to UI Markdown component (stored XSS — critical)
2. **E3a** — Fix Helm MASTER_KEY generation to produce hex characters
3. **E7** — Add non-root USER to main platform Dockerfile
4. **E4+E5** — Add ServiceAccount + Gateway API RBAC to Helm ClusterRole

### Short-term (this month)
5. **E2** — Rewrite `platform-deploy.js` MCP server for actual API
6. **E6** — Add PIPELINE_NAMESPACE, AGENT_NAMESPACE to Helm configmap
7. **E8** — Remove or restrict passwordless sudo in runner image
8. **E9** — Pin kaniko and Claude CLI versions
9. **E10-E14** — Fix MCP parameter mismatches (labels, target_branch, permissions, search, role_id)
10. **E16** — Add sandbox attribute to UI iframes
11. **E17** — Fix entrypoint.sh to not commit API tokens
12. **E18** — Fix Helm NetworkPolicy egress for preview proxy
13. **E24** — Add try/catch to remaining MCP servers

### Long-term (backlog)
14. **E15** — Add checksum verification to install.sh
15. **E19-E21** — Fix Kustomize overlay drift from Helm chart
16. **E30** — Align Rust versions across Dockerfiles
17. **E34** — Add Gateway + apps API RBAC to test manifests
18. **E35** — Gate release workflow behind CI passing
19. **E37** — Update CLAUDE.md staging documentation
20. Add integration contract tests that verify API type shapes match across codebases
