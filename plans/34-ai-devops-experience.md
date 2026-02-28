# AI DevOps Platform — End-to-End Experience Plan

## Implementation Status
- **Phase 1:** Merged — PR #7
- **Phase 2:** Merged — PR #10
- **Phase 3:** Merged — PR #11
- **Phase 4:** Merged — PR #12
- **Phase 5:** Merged — PR #13
- **Phase 6:** In Review — PR #14 (https://github.com/agentsphere/platform/pull/14)
- **Status:** Phases 1-5 merged, phase 6 in review

## Context

The platform (~23K LOC Rust) replaces 8+ services with a unified AI-powered tool for developing and operating software. Primary interaction: chat with agents. The core building blocks exist (auth, git, pipelines, deployer, observability, secrets, agents) but the end-to-end experience is rough — no per-project isolation, agents clone via `file://` (breaks across namespaces), fixed dev images, no automated ops response, and raw SQL errors surfacing to users.

**Goal**: Define a clear, cohesive flow from "idea → running production app with monitoring and auto-incident response" and fix the gaps that make it brittle.

---

## The Target Experience

```
User: "Build me a Node.js API for managing bookmarks"
  │
  ▼
┌─────────────────────────────────────┐
│  In-Process Manager Agent (chat)    │  ← clarifies idea, picks tech stack
│  Tools: create_project, spawn_agent │
└──────────┬──────────────────────────┘
           │ creates project "bookmark-api"
           ▼
┌─────────────────────────────────────┐
│  Platform auto-creates:             │
│  • Git repo (bare, smart HTTP)      │
│  • Ops repo (auto, 1:1 per project) │
│  • bookmark-api-dev namespace       │  ← permanent: dev agents + pipelines
│  • bookmark-api-prod namespace      │  ← permanent: production workloads
│  • NetworkPolicy (agents → API + internet, no intra-cluster)
└──────────┬──────────────────────────┘
           │ spawns coding agent
           ▼
┌─────────────────────────────────────┐
│  Dev Agent Pod (bookmark-api-dev)   │  ← runs in project's dev namespace
│  • Clones via HTTP (scoped token)   │  ← same token for MCP calls to BE
│  • Writes code, Dockerfile,         │
│    .platform.yaml, deploy/ folder   │
│  • Can install tools (npm, etc.)    │  ← internet access allowed
│  • Cannot reach other pods          │  ← NetworkPolicy enforced
│  • Pushes to main                   │
└──────────┬──────────────────────────┘
           │ git push triggers pipeline
           ▼
┌─────────────────────────────────────┐
│  Pipeline (bookmark-api-dev ns)     │
│  • Builds app image (kaniko)        │
│  • Optionally builds dev image      │  ← from Dockerfile.dev if changed
│  • Pushes to platform registry      │
└──────────┬──────────────────────────┘
           │ ImageBuilt event
           ▼
┌─────────────────────────────────────┐
│  Event Bus → Deployer               │
│  • Syncs deploy/ from repo → ops    │
│    repo (auto-created, 1:1)         │
│  • Renders manifests with image_ref │
│  • Injects secrets as K8s Secret    │
│  • Applies to bookmark-api-prod ns  │
│  • Tracks resources for cascade     │
│    deletes                          │
└──────────┬──────────────────────────┘
           │ app running in prod
           ▼
┌─────────────────────────────────────┐
│  Observability                      │
│  • App sends OTLP via scoped token  │  ← per-project auth key
│  • Traces/logs/metrics scoped to    │
│    project + environment            │
│  • Dashboards in UI per project     │
└──────────┬──────────────────────────┘
           │ alert fires (e.g. error spike)
           ▼
┌─────────────────────────────────────┐
│  Ops Agent (auto-spawned)           │
│  • Reads logs, traces, code         │
│  • Investigates root cause          │
│  • Creates issue with findings      │
│  • Optionally proposes PR fix       │
└─────────────────────────────────────┘
```

---

## Key Architecture Decisions

### Image builds: Kaniko (Chainguard) → BuildKit (later)

**Kaniko was archived by Google (June 2025)**. Chainguard maintains a fork (`cgr.dev/chainguard/kaniko`).

**Short-term**: Use Chainguard's Kaniko. Zero special security context needed — just an unprivileged pod running a single binary. Already used in the platform's `.platform.yaml` examples.

**Medium-term**: Migrate to BuildKit rootless (`moby/buildkit:rootless`). 2-3x faster builds, proper multi-stage cache (`mode=max` caches ALL intermediate stages, not just the final layer), active development. Tradeoff: needs `seccomp=Unconfined` + `apparmor=Unconfined` on the pod. Acceptable since each build pod is ephemeral and network-isolated.

### Deployment format: Bare YAML + minijinja + inventory pruning

**Not Helm** — no Rust Helm SDK exists (would shell out to CLI), Go templates are error-prone for AI agents, adds dual state (Helm releases + DB). **Not Kustomize** — also needs CLI binary, doesn't solve deletion, patch semantics harder for AI agents.

**Plain K8s YAML wins** — AI agents produce valid K8s YAML far more reliably than any templated format. The platform already has minijinja templating (`renderer.rs`) + server-side apply (`applier.rs`).

**Resource deletion**: Inventory-based pruning (same pattern as ArgoCD and Flux). Track applied resources as JSON per deployment, diff on next apply, delete orphans. ~100-150 lines added to `applier.rs`.

### Agent self-testing: push to branch → watch pipeline

The dev agent should test its own Dockerfile, `Dockerfile.dev`, `deploy/` manifests, and `.platform.yaml` by pushing to a branch and watching the pipeline. The agent already has:
- Git push access via scoped HTTP token
- `get_pipeline` / `list_pipelines` MCP tools to check pipeline status
- Pipeline triggers on push (existing `on_push()` in `trigger.rs`)

The agent's workflow becomes: write code → push to branch → watch pipeline via MCP → if pipeline fails, read logs, fix, push again → when green, merge to main → deployment triggers.

---

## Phase 1: Fix the Broken Core (Week 1) — COMPLETED ✓

**Why**: Agents can't work reliably today. `file://` clone breaks across namespaces, duplicate project names surface raw SQL, no pod security.

### Implementation Status

- **Branch:** `feat/34-ai-devops-phase1`
- **PR:** #7 (https://github.com/agentsphere/platform/pull/7)
- **Status:** Merged

- [x] **1A. HTTP git clone for agents + pipelines** — Replaced `file://` with HTTP clone using `GIT_ASKPASS` for both agents and pipelines. Added `platform_api_url` to Config. Removed `repos` hostPath volume from pipeline pods. Added short-lived project-scoped git auth tokens in executor.
- [x] **1B. Friendly duplicate project error** — Catches 23505 unique constraint for duplicate project names, returns `ApiError::Conflict("A project named 'foo' already exists")` in both API handler and inprocess agent.
- [x] **1C. Pod SecurityContext** — Added `PodSecurityContext` (runAsNonRoot, runAsUser:1000, fsGroup:1000) and container-level `SecurityContext` (drop ALL caps, no privilege escalation) to all agent containers (including browser sidecar) and pipeline pods.

**Additional fixes discovered during implementation:**
- Fixed `info_refs` handler passing `git-upload-pack` (from query string) to `git` as a subcommand instead of stripping the `git-` prefix → used `service.strip_prefix("git-")` to fix.
- Added `WWW-Authenticate: Basic realm="platform"` header to 401 responses (per RFC 7235) so git clients know to use GIT_ASKPASS for authentication.
- Added token-only auth fallback in `authenticate_basic()` for GIT_ASKPASS scenarios where the token is used as both username and password.
- Added init container log capture to MinIO for pipeline debugging.
- Updated E2E pipeline tests to use a real TCP server (`start_pipeline_server`) so Kind pods can reach the platform API.

**Review findings (13 total: 9 fixed, 1 deferred, 3 low/optional):**

| # | Severity | Finding | Status |
|---|----------|---------|--------|
| R1 | HIGH | Shell injection via branch in pipeline pods | ✓ Fixed — branch passed as `GIT_BRANCH` env var |
| R2 | HIGH | Shell injection via branch in agent pods | ✓ Fixed — same `GIT_BRANCH` env var approach |
| R3 | HIGH | Unscoped pipeline git auth token | ✓ Fixed — added `project_id` to token INSERT |
| R4 | HIGH | Missing token-only auth integration test | ✓ Fixed — added `authenticate_token_only_auth_succeeds` |
| R5 | HIGH | Missing inactive user token-only auth test | ✓ Fixed — added `authenticate_token_only_inactive_user_returns_401` |
| R6 | MEDIUM | Browser sidecar missing SecurityContext | ✓ Fixed — added `security_context: Some(container_security())` |
| R7 | MEDIUM | Duplicate project 409 test body assertion | ✓ Fixed — enhanced test asserts "already exists" |
| R8 | MEDIUM | Integration test for inprocess agent duplicate project | Deferred — E2E-only code path |
| R9 | MEDIUM | `PLATFORM_API_URL` not in CLAUDE.md | ✓ Fixed — added to env var table |
| R10 | MEDIUM | No logging for token-only auth | ✓ Fixed — added `tracing::debug!` on fallback path |
| R11 | LOW | Debug log for missing `git-` prefix | Optional |
| R12 | LOW | Hardcoded init container name `"clone"` | Optional |
| R13 | LOW | Duplicated `container_security()` helper | Optional |

**Test results:** 897 unit, 658 integration, 49 E2E — all passing. 100% diff-coverage on touched lines.

### 1A. HTTP git clone for agents

Replace `file://{repo_path}` with HTTP clone via the platform's smart HTTP git server.

**Files**:
- `src/agent/service.rs` — `get_project_repo_info()` (line 420): return HTTP URL instead of `file://`
- `src/agent/claude_code/pod.rs` — `build_git_clone_container()`: use `GIT_ASKPASS` env var for HTTP auth (see security note below)
- `src/config.rs` — add `platform_api_url` config (e.g. `http://platform.platform.svc.cluster.local:8080`) so agents know how to reach the API. Also add to `Config::test_default()`.

**How it works**: Agent already gets `PLATFORM_API_TOKEN` in its env. The init container uses that same token as HTTP basic auth password to clone. The platform's git smart HTTP handler (`src/git/smart_http.rs`) already validates project-scoped tokens.

**SECURITY: Use `GIT_ASKPASS`, NOT inline URL credentials.** Do NOT embed tokens in clone URLs (`http://{user}:{token}@...`). Tokens in URLs leak to: git error messages, `/proc/{pid}/cmdline`, K8s pod spec (visible via `kubectl get pod -o yaml`), and audit logs. Instead, inject a small `GIT_ASKPASS` script via env var:
```bash
# In init container env:
GIT_ASKPASS=/tmp/git-askpass.sh
# The init container command creates the script first:
echo '#!/bin/sh\necho "$PLATFORM_API_TOKEN"' > /tmp/git-askpass.sh && chmod +x /tmp/git-askpass.sh
# Then: git clone http://agent@{platform_api_url}/{owner}/{project}.git /workspace
```
The username in the URL (`agent@`) is for HTTP Basic Auth user field; the password comes from `GIT_ASKPASS`. Token never appears in args, URLs, or process lists.

**Also fix pipeline executor**: `src/pipeline/executor.rs` uses `file://` with host-path volume mount — switch to HTTP clone for consistency. This removes the need for the `repos` hostPath volume in pipeline pods.

### 1B. Friendly duplicate project error

**File**: `src/api/projects.rs` — `create_project` handler

Catch sqlx unique constraint violation (`23505`) and return:
```json
{"error": "A project named 'foo' already exists. Try 'foo-2' or a different name."}
```

Pattern: match on `sqlx::Error::Database` with `.constraint() == Some("projects_owner_id_name_key")`, return `ApiError::Conflict(msg)`. **Important**: Catch this in the handler BEFORE the error hits the global `From<sqlx::Error> for ApiError` (which already catches all `23505` as generic "resource already exists"). Wrap the INSERT with explicit error handling:
```rust
match sqlx::query!(/* ... */).fetch_one(&state.pool).await {
    Ok(project) => { /* success */ },
    Err(sqlx::Error::Database(db_err)) if db_err.constraint() == Some("projects_owner_id_name_key") => {
        return Err(ApiError::Conflict(format!("A project named '{}' already exists.", body.name)));
    },
    Err(e) => return Err(e.into()),
}
```
Also update `execute_create_project()` in `src/agent/inprocess.rs` — it calls DB directly, not the API.

### 1C. Pod SecurityContext

**Files**: `src/agent/claude_code/pod.rs`, `src/pipeline/executor.rs`

Add to all pod specs:
- Pod level: `run_as_non_root: true`, `run_as_user: 1000`, `fs_group: 1000`
- Container level: `allow_privilege_escalation: false`, `capabilities.drop: ["ALL"]`
- Keep `read_only_root_filesystem: false` (agents need to install tools)

### Tests to write FIRST (before implementation)

**Unit tests — `src/agent/claude_code/pod.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_git_clone_uses_http_url` | `build_git_clone_container()` uses HTTP URL (not `file://`) | Unit |
| `test_git_clone_sets_git_askpass` | Init container has `GIT_ASKPASS` env var for HTTP auth | Unit |
| `test_repo_clone_url_http_format` | `PodBuildParams.repo_clone_url` passes HTTP URL to clone script | Unit |
| `test_pod_security_context_run_as_non_root` | Pod-level `run_as_non_root == true` | Unit |
| `test_pod_security_context_run_as_user_1000` | Pod-level `run_as_user == 1000` | Unit |
| `test_pod_security_context_fs_group_1000` | Pod-level `fs_group == 1000` | Unit |
| `test_container_security_no_privilege_escalation` | Main container `allow_privilege_escalation == false` | Unit |
| `test_container_security_drop_all_capabilities` | Main container `capabilities.drop == ["ALL"]` | Unit |
| `test_init_container_security_context` | Init containers also have restricted security context | Unit |

**Unit tests — `src/pipeline/executor.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_pipeline_pod_spec_uses_http_clone` | Init container uses HTTP clone instead of `file://` | Unit |
| `test_pipeline_pod_spec_no_repos_hostpath_volume` | No `repos` hostPath volume when using HTTP clone | Unit |
| `test_pipeline_pod_security_context` | Pod-level SecurityContext: `run_as_non_root`, `run_as_user: 1000`, `fs_group: 1000` | Unit |
| `test_pipeline_container_security_context` | Step containers: `allow_privilege_escalation: false`, drop ALL caps | Unit |
| `test_pipeline_init_container_security_context` | Clone init container has matching restrictions | Unit |

**Unit tests — `src/agent/service.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_get_project_repo_info_returns_http_url` | Returns `http://platform.{ns}.svc:8080/{owner}/{project}` not `file://` | Unit |

**Unit tests — `src/config.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_platform_api_url_config_loaded` | `Config::load()` reads `PLATFORM_API_URL` env var | Unit |
| `test_platform_api_url_default` | Defaults to `http://platform.platform.svc.cluster.local:8080` | Unit |

**Integration tests — `tests/project_integration.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_duplicate_project_returns_409_friendly` | Creating project with same name returns 409 with friendly message including project name | Integration |
| `test_duplicate_project_different_owner_ok` | Two different users can create same-named project | Integration |

**E2E tests — `tests/e2e_agent.rs` / `tests/e2e_pipeline.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_agent_pod_clones_via_http` | Agent pod init container uses HTTP URL and clones successfully | E2E |
| `test_pipeline_pod_clones_via_http` | Pipeline pod clones via HTTP (no hostPath mount) | E2E |

Total: **17 unit + 2 integration + 2 E2E = 21 tests**

#### Existing tests to UPDATE

| Test file | Change | Reason |
|---|---|---|
| `src/agent/claude_code/pod.rs::init_container_clones_repo` | Assert HTTP URL format | Clone URL format changes |
| All pod.rs tests passing `repo_clone_url: "file://..."` | Update to HTTP URL | Clone URL format changes |
| `src/pipeline/executor.rs::build_pod_spec_structure` | Remove `repos` hostPath assertion | No more hostPath volume |
| `src/pipeline/executor.rs::volumes_repos_host_path` | Delete or replace | Volume removed |
| `tests/project_integration.rs::create_project_duplicate_name` | Add assertion on friendly body text | 409 body changes |
| `tests/helpers/mod.rs` + `tests/e2e_helpers/mod.rs` | Add `platform_api_url` to Config | Config struct changes |

#### Branch coverage checklist

| Branch/Path | Test that covers it |
|---|---|
| `get_project_repo_info` → HTTP URL | `test_get_project_repo_info_returns_http_url` |
| `PLATFORM_API_URL` env present | `test_platform_api_url_config_loaded` |
| `PLATFORM_API_URL` absent (default) | `test_platform_api_url_default` |
| Agent clone via HTTP | `test_git_clone_uses_http_url` |
| Pipeline clone via HTTP | `test_pipeline_pod_spec_uses_http_clone` |
| Unique constraint → 409 friendly | `test_duplicate_project_returns_409_friendly` |
| Pod SecurityContext on agent | `test_pod_security_context_run_as_non_root` etc. |
| Container SecurityContext on pipeline | `test_pipeline_container_security_context` |

---

## Phase 2: Per-Project Namespaces + Network Isolation (Week 2) ✅ IMPLEMENTED

**Why**: All agents/pipelines/deployments currently share global namespaces. No isolation between projects.

### 2A. Project namespace lifecycle

**Migration** (`20260227010001_project_namespace.up.sql`): Three-step migration for existing rows:
```sql
-- UP
ALTER TABLE projects ADD COLUMN namespace_slug TEXT;
UPDATE projects SET namespace_slug = lower(regexp_replace(name, '[^a-z0-9-]', '-', 'g'));
ALTER TABLE projects ALTER COLUMN namespace_slug SET NOT NULL;
CREATE UNIQUE INDEX idx_projects_namespace_slug ON projects(namespace_slug) WHERE is_active = true;

ALTER TABLE ops_repos ADD COLUMN project_id UUID REFERENCES projects(id) ON DELETE CASCADE;
CREATE UNIQUE INDEX idx_ops_repos_project ON ops_repos(project_id) WHERE project_id IS NOT NULL;

-- DOWN
DROP INDEX IF EXISTS idx_ops_repos_project;
ALTER TABLE ops_repos DROP COLUMN IF EXISTS project_id;
DROP INDEX IF EXISTS idx_projects_namespace_slug;
ALTER TABLE projects DROP COLUMN IF EXISTS namespace_slug;
```

**Why 3-step**: Adding `NOT NULL` directly fails on existing rows. The `UNIQUE` partial index (filtered on `is_active = true`) prevents namespace collisions — two projects producing the same K8s namespace would be catastrophic. The `ops_repos.project_id` FK enables the 1:1 auto-creation mapping.

**New function**: `slugify_namespace(name: &str) -> String` in `src/deployer/namespace.rs` — NOT `slugify_branch()` (which truncates at 63 chars, too long for namespace prefix). This function: lowercases, replaces non-alphanumeric with hyphens, collapses runs, strips leading/trailing hyphens, truncates to **40 chars** (leaves room for `-dev`/`-prod`/`-staging` suffix, total ≤ 48, well under K8s 63-char DNS label limit). Handle collisions by appending a short hash suffix if DB insert fails uniqueness check.

**On project create** (`src/api/projects.rs`):
1. Derive `namespace_slug` via `slugify_namespace(&body.name)`
2. INSERT project row with `namespace_slug` (catch unique violation, retry with hash suffix)
3. Call `setup_project_infrastructure(state, project_id, &namespace_slug)` helper

**Extract helper** to keep `create_project` under 100 lines (clippy `too_many_lines`):
```rust
async fn setup_project_infrastructure(
    state: &AppState, project_id: Uuid, namespace_slug: &str,
) -> Result<(), ApiError> {
    // 1. Create {slug}-dev namespace with labels
    // 2. Create {slug}-prod namespace with labels
    // 3. Apply NetworkPolicy to -dev namespace
    // 4. Auto-create ops repo (git init + DB insert with project_id FK)
    Ok(())
}
```
**Error handling**: If namespace creation succeeds but ops repo fails, the namespaces are orphaned but harmless (idempotent re-creation on retry). Consider logging a warning. Do NOT block project creation on K8s failures — the project row is the source of truth.

**Also update**: Add `namespace_slug` to `ProjectResponse` struct and all SELECT queries in `projects.rs`. Run `just types` to regenerate TypeScript types.

**Auto-created ops repo**: Each project gets its own ops repo at `{ops_repos_path}/{namespace_slug}.git`. Row inserted in `ops_repos` table with `project_id` FK. No `create_ops_repo` tool call needed.

**New file**: `src/deployer/namespace.rs` — `slugify_namespace()`, `ensure_namespace()`, and `ensure_network_policy()` using server-side apply (idempotent). Also add `"NetworkPolicy" => "networkpolicies".into()` to `kind_to_plural()` in `src/deployer/applier.rs` (currently missing — server-side apply would fail for NetworkPolicy).

### 2B. Route agents + pipelines to project namespaces

**Agent pods**: `src/agent/service.rs` — change from `state.config.agent_namespace` to `{project.namespace_slug}-dev`.

**Pipeline pods**: `src/pipeline/executor.rs` — change from `state.config.pipeline_namespace` to `{project.namespace_slug}-dev`. Pipelines are a dev-time activity.

**Agent reaper**: `src/agent/service.rs` `run_reaper()` — currently scans one namespace. Change to: query all running sessions from DB, group by project, check pods in respective `{slug}-dev` namespaces.

**Config**: Keep `PLATFORM_PIPELINE_NAMESPACE` and `PLATFORM_AGENT_NAMESPACE` as fallbacks for global (non-project) operations. Add `PLATFORM_NAMESPACE` (default: `platform`) for the platform's own namespace.

### 2C. NetworkPolicy for agent pods

Apply to every `{slug}-dev` namespace. **Both ingress deny-all and egress whitelist**:

```yaml
# Agents can reach: platform API + internet. Cannot reach: other cluster pods.
# No other pod can reach agents (ingress deny-all).
spec:
  podSelector:
    matchLabels:
      platform.io/component: agent-session
  policyTypes: [Ingress, Egress]     # Ingress deny-all (no ingress rules = block all)
  egress:
  - to:                               # Platform API (for MCP + git clone)
    - namespaceSelector:
        matchLabels:
          kubernetes.io/metadata.name: platform
    ports:
    - port: 8080
  - to:                               # DNS (kube-system only, not any namespace)
    - namespaceSelector:
        matchLabels:
          kubernetes.io/metadata.name: kube-system
      podSelector:
        matchLabels:
          k8s-app: kube-dns
    ports:
    - port: 53
      protocol: UDP
    - port: 53
      protocol: TCP
  - to:                               # Internet (npm install, apt-get, etc.)
    - ipBlock:
        cidr: 0.0.0.0/0
        except:
        - 10.0.0.0/8                  # Block cluster-internal CIDRs
        - 172.16.0.0/12
        - 192.168.0.0/16
        - 100.64.0.0/10               # CGNAT (cloud-internal routing)
        - 169.254.0.0/16              # Link-local (cloud metadata)
```

This allows internet access (agents can `npm install`, `pip install`) but blocks all intra-cluster communication except to the platform API and DNS. Ingress deny-all prevents other pods from connecting to agent pods.

**Note**: NetworkPolicies require a CNI that supports them (Calico, Cilium). Kind's default kindnet does NOT enforce them — acceptable for dev, but production clusters must use Calico/Cilium.

### 2D. Simplify in-process agent flow

Remove `create_ops_repo` and `seed_ops_repo` tool calls from the create-app flow. Ops repo is now auto-created with the project.

**Simplified tool sequence**: `create_project` → `spawn_coding_agent` (with expanded prompt including `deploy/` instructions).

**Update** `src/agent/inprocess.rs` — `CREATE_APP_SYSTEM_PROMPT`: remove steps 2-4, update spawn_coding_agent prompt to include `deploy/production.yaml` instructions.

### Tests to write FIRST (before implementation)

**Unit tests — `src/deployer/namespace.rs` (new file)**

| Test | Validates | Layer |
|---|---|---|
| `test_slugify_namespace_basic` | `slugify_namespace("my-project")` → `"my-project"` | Unit |
| `test_slugify_namespace_max_40_chars` | Long names truncated to 40 chars | Unit |
| `test_slugify_namespace_lowercase` | Mixed case → lowercase | Unit |
| `test_slugify_namespace_special_chars` | Special chars replaced with hyphens, runs collapsed | Unit |
| `test_namespace_object_has_correct_labels` | Namespace has `platform.io/project` and `platform.io/env` labels | Unit |
| `test_network_policy_egress_platform_api` | Egress rule to platform namespace on port 8080 | Unit |
| `test_network_policy_egress_dns_kube_system` | DNS rule targets kube-system, UDP+TCP port 53 | Unit |
| `test_network_policy_egress_internet_except_cluster` | Internet rule blocks 10/8, 172.16/12, 192.168/16, 100.64/10, 169.254/16 | Unit |
| `test_network_policy_ingress_deny_all` | `policyTypes: [Ingress, Egress]` with no ingress rules | Unit |
| `test_network_policy_pod_selector` | Selects `platform.io/component: agent-session` | Unit |

**Integration tests — `tests/project_integration.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_create_project_returns_namespace_slug` | Response includes `namespace_slug` field | Integration |
| `test_project_namespace_slug_is_k8s_safe` | Slug is valid K8s DNS label | Integration |
| `test_project_namespace_slug_unique` | Two projects with same name → second gets hash suffix | Integration |
| `test_project_auto_creates_ops_repo` | `ops_repos` table has entry with matching `project_id` FK | Integration |
| `test_project_ops_repo_path_uses_slug` | Ops repo path is `{ops_repos_path}/{namespace_slug}.git` | Integration |

**Integration tests — `tests/deployment_integration.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_deploy_target_namespace_from_project_slug` | Deployment targets `{namespace_slug}-{env}` namespace | Integration |

**Integration tests — `tests/create_app_integration.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_simplified_create_app_two_tools` | Create-app flow has 2 tools: `create_project`, `spawn_coding_agent` (not 5) | Integration |

**E2E tests — `tests/e2e_agent.rs` / `tests/e2e_pipeline.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_agent_pod_runs_in_project_dev_namespace` | Agent pod created in `{slug}-dev`, not global namespace | E2E |
| `test_agent_pod_network_policy_exists` | `{slug}-dev` namespace has NetworkPolicy | E2E |
| `test_pipeline_pod_runs_in_project_dev_namespace` | Pipeline pod in `{slug}-dev` | E2E |

Total: **10 unit + 7 integration + 3 E2E = 20 tests**

#### Existing tests to UPDATE

| Test file | Change | Reason |
|---|---|---|
| `src/agent/inprocess.rs::create_app_tools_returns_five_tools` | Assert 2 tools, not 5 | Tool count changes |
| `src/agent/inprocess.rs::system_prompt_mentions_tools` | Remove `create_ops_repo`/`seed_ops_repo`/`create_deployment` | Steps removed |
| E2E agent/pipeline tests | Check namespace is `{slug}-dev` not global | Routing changes |
| `tests/helpers/mod.rs` + `tests/e2e_helpers/mod.rs` | Add `platform_namespace` to Config | New config field |
| `.sqlx/` offline cache | Regenerate after migration | Schema change |

#### Branch coverage checklist

| Branch/Path | Test that covers it |
|---|---|
| `slugify_namespace` basic/long/special | 4 slugify unit tests |
| Namespace UNIQUE collision → hash suffix | `test_project_namespace_slug_unique` |
| NetworkPolicy egress rules (3 rules) | 3 egress unit tests |
| Ingress deny-all | `test_network_policy_ingress_deny_all` |
| Agent → `{slug}-dev` | `test_agent_pod_runs_in_project_dev_namespace` |
| Pipeline → `{slug}-dev` | `test_pipeline_pod_runs_in_project_dev_namespace` |
| Auto-create ops repo with project_id FK | `test_project_auto_creates_ops_repo` |
| Simplified 2-tool flow | `test_simplified_create_app_two_tools` |

### Implementation Status

- **Branch:** `claude/exciting-mccarthy`
- **PR:** #10 (https://github.com/agentsphere/platform/pull/10)
- **Status:** In Review

### Implementation Knowledge

**Deviations from plan:**
- `ops_repos` table was not modified — ops repo linkage uses existing `ops_repos.project_id` column added inline with the project infrastructure setup, no separate migration needed for this FK since the table was created with the column in the same migration
- In-process agent tools simplified from 5 → 2 (`create_project` + `spawn_coding_agent`); `create_ops_repo`, `seed_ops_repo`, `create_deployment` removed
- Agent reaper not refactored for N-namespace scanning yet (deferred — works with existing single-namespace scan for now)
- `PLATFORM_NAMESPACE` config added but agents/pipelines still fall back to global namespace when project has no `namespace_slug` (backward compat)

**Key implementation patterns:**

1. **Namespace slug collision handling** — both `src/api/projects.rs` and `src/agent/inprocess.rs` use identical collision retry: on unique constraint violation, append 6-char SHA256 hash suffix `{slug[..33]}-{hash}` (total ≤ 40 chars). The same logic is duplicated in both paths because the in-process agent calls DB directly, not the API.

2. **`setup_project_infrastructure()` is best-effort** — all K8s operations (namespace creation, NetworkPolicy, ops repo init) are logged-and-continued on failure. The project DB row is the source of truth; infrastructure catches up on next access. This prevents K8s outages from blocking project creation.

3. **`build_namespace_object()` and `build_network_policy()`** return `serde_json::Value` (not typed K8s structs) — applied via `Api::<DynamicObject>::server_side_apply()` with `force()`. This avoids pulling in typed K8s API structs for simple namespace/policy objects.

4. **NetworkPolicy targets only `-dev` namespaces** — `-prod` namespaces don't get NetworkPolicy because deployed apps need arbitrary networking. Agent isolation is the security boundary.

5. **`kind_to_plural()` in `applier.rs`** — added `"NetworkPolicy" => "networkpolicies"` entry (was missing; server-side apply would silently fail with wrong plural).

**Test results:** PR #10 — 984 unit, 703 integration, 54 E2E — all passing.

**Review findings (PR #10):**

| # | Severity | Finding | Status |
|---|----------|---------|--------|
| See PR #10 review file for full details | | | |

**Files modified:**
- `src/deployer/namespace.rs` (new) — `slugify_namespace()`, `build_namespace_object()`, `build_network_policy()`, `ensure_namespace()`, `ensure_network_policy()`
- `src/api/projects.rs` — `namespace_slug` field, `setup_project_infrastructure()` helper, collision retry
- `src/agent/inprocess.rs` — simplified to 2 tools, `execute_create_project()` with namespace setup
- `src/agent/service.rs` — route agents to `{slug}-dev`
- `src/pipeline/executor.rs` — route pipelines to `{slug}-dev`
- `src/deployer/applier.rs` — `kind_to_plural()` update for NetworkPolicy
- `src/config.rs` — `PLATFORM_NAMESPACE` config
- `migrations/20260227010001_project_namespace.{up,down}.sql` — namespace_slug column + indexes

---

## Phase 3: Deploy from Project Repo + Resource Cascade (Week 3-4) ✅ IMPLEMENTED

**Why**: Agents know best what their app needs for deployment. Deploy config should live in the project repo. Ops repo tracks deployment state.

**Implementation status**: All sub-items complete. 16 new unit tests, existing integration + E2E tests updated. Full CI green (1006 unit, 730 integration, 54 E2E).

**Deviations from plan**:
- Removed standalone `apply()` (dead code after `apply_with_tracking()` replaced it)
- Added `ensure_branch_exists()` helper in ops_repo.rs to bootstrap empty bare repos
- Added `ensure_namespace()` call in `handle_active()` to create project namespace on-demand
- Extracted `upsert_deployment()` from `handle_image_built()` to stay under 100-line clippy limit
- New integration/E2E tests not written (plan specified 5 + 2 = 7 new tests); existing tests updated to cover new behavior

### 3A. The `deploy/` convention

Agent writes **plain K8s YAML** with minijinja template variables in `deploy/`:

```
deploy/
  production.yaml     # K8s manifests for prod (Deployment, Service, Ingress, etc.)
  staging.yaml        # optional: staging manifests
  preview.yaml        # optional: preview template
```

Example `deploy/production.yaml` (what the agent writes):
```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{ project_name }}
  labels:
    app: {{ project_name }}
spec:
  replicas: {{ values.replicas | default(1) }}
  selector:
    matchLabels:
      app: {{ project_name }}
  template:
    metadata:
      labels:
        app: {{ project_name }}
    spec:
      containers:
      - name: app
        image: {{ image_ref }}
        ports:
        - containerPort: 8080
        envFrom:
        - secretRef:
            name: {{ project_name }}-prod-secrets
---
apiVersion: v1
kind: Service
metadata:
  name: {{ project_name }}
spec:
  selector:
    app: {{ project_name }}
  ports:
  - port: 80
    targetPort: 8080
```

The platform renders with minijinja (existing `renderer.rs`), substituting `image_ref`, `project_name`, `environment`, and arbitrary `values.*`.

### 3B. Pipeline syncs `deploy/` to ops repo

**Event flow** (extends existing `ImageBuilt` handler in `src/store/eventbus.rs`):

1. Pipeline builds image, publishes `ImageBuilt` event (existing)
2. Event handler: read `deploy/` folder from project git repo at the pipeline's commit SHA
3. Sync contents to the project's auto-created ops repo:
   - `git show {sha}:deploy/` → write all files to ops repo worktree
   - Delete files in ops repo not present in `deploy/` (cascade)
   - Commit with message: `"sync deploy/ from {sha}"`
4. Write `values/{environment}.yaml` with `image_ref: {new_image}` (existing pattern)
5. Publish `OpsRepoUpdated` (existing)

**Files**:
- `src/store/eventbus.rs` — extend `handle_image_built()` to call sync (keep handler thin — delegate to ops_repo)
- `src/deployer/ops_repo.rs` — new `sync_from_project_repo()` function (extract here to keep eventbus handler under 100 lines)
- `src/agent/inprocess.rs` — update `CREATE_APP_SYSTEM_PROMPT` to tell the coding agent to create `deploy/production.yaml`

**Note**: `handle_image_built()` is already ~60 lines. Adding sync logic directly would exceed clippy's 100-line threshold. Extract `sync_from_project_repo()` as a standalone function in `ops_repo.rs`.

### 3C. Deployer applies to project namespace

**File**: `src/deployer/reconciler.rs`

Change target namespace from `state.config.pipeline_namespace` to `{project.namespace_slug}-{environment}`:
- `production` environment → `{slug}-prod` namespace
- `staging` → `{slug}-staging` namespace (created on-demand)
- `preview` → existing preview namespace logic

### 3D. Resource tracking for cascade deletes (ArgoCD/Flux pattern)

**Migration**: `ALTER TABLE deployments ADD COLUMN tracked_resources JSONB DEFAULT '[]'`

The applier injects a managed-by label on every resource before applying:
```yaml
metadata:
  labels:
    platform.io/managed-by: platform-deployer
    platform.io/deployment-id: {uuid}
```

After each apply in `src/deployer/applier.rs`:
1. Record applied resources: `[{apiVersion, kind, name, namespace}, ...]`
2. Store in `deployments.tracked_resources`
3. Before next apply: diff old tracked vs new. Delete resources in old but not in new.
4. Resources with `platform.io/prune: disabled` annotation are skipped (opt-out, same as Flux)

This is the same inventory-based pruning pattern used by ArgoCD and Flux. ~100-150 lines of Rust.

**New type** in `src/deployer/applier.rs`:
```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackedResource {
    pub api_version: String,
    pub kind: String,
    pub name: String,
    pub namespace: String,
}
```

**Also update**: Add `tracked_resources: Option<Vec<TrackedResource>>` to `PendingDeployment` struct in `reconciler.rs` so the cascade logic has access to the previous inventory. Add `tracked_resources` to `DeploymentResponse` (optional). Run `just types` for TypeScript types.

### 3E. Agent self-testing workflow

The dev agent tests its own artifacts by pushing to a feature branch and watching the pipeline:

1. Agent pushes code + Dockerfile + `.platform.yaml` + `deploy/` to branch `agent/{session-id}`
2. Push triggers pipeline (existing `on_push()` in `trigger.rs`)
3. Agent polls pipeline status via `get_pipeline` MCP tool
4. If pipeline fails → agent reads step logs via MCP → fixes code → pushes again
5. When pipeline succeeds → agent merges to main (or creates MR for human review)
6. Push to main triggers production deploy

The agent already has all needed MCP tools: `list_pipelines`, `get_pipeline`, `get_pipeline_logs` (in `platform-pipeline.js`). The git push permission comes from the scoped HTTP token.

### Tests to write FIRST (before implementation)

**Unit tests — `src/deployer/ops_repo.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_sync_from_project_repo_copies_deploy_dir` | Reads `deploy/` at given SHA and writes all files to ops repo | Unit |
| `test_sync_from_project_repo_deletes_orphans` | Files in ops repo not in `deploy/` are deleted | Unit |
| `test_sync_from_project_repo_commit_message` | Commit message is `"sync deploy/ from {sha}"` | Unit |

**Unit tests — `src/deployer/applier.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_inject_managed_by_labels` | Resources get `platform.io/managed-by` and `platform.io/deployment-id` labels | Unit |
| `test_resource_diff_finds_orphans` | Old `[A,B,C]` vs new `[A,B]` → orphans `[C]` | Unit |
| `test_resource_diff_empty_old_no_orphans` | Empty old → no orphans | Unit |
| `test_resource_diff_same_set_no_orphans` | Identical sets → no orphans | Unit |
| `test_resource_diff_all_removed` | All old removed → all are orphans | Unit |
| `test_prune_skip_annotation` | `platform.io/prune: disabled` → not deleted | Unit |
| `test_tracked_resources_json_round_trip` | `TrackedResource` serializes/deserializes correctly | Unit |
| `test_tracked_resources_equality` | Same apiVersion/kind/name/namespace → equal | Unit |

**Unit tests — `src/deployer/reconciler.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_deploy_target_namespace_production` | `production` → `{slug}-prod` | Unit |
| `test_deploy_target_namespace_staging` | `staging` → `{slug}-staging` | Unit |
| `test_deploy_target_namespace_preview` | Preview → existing preview logic | Unit |

**Integration tests — `tests/eventbus_integration.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_image_built_syncs_deploy_dir_to_ops_repo` | ImageBuilt handler reads `deploy/` and writes to ops repo | Integration |
| `test_image_built_writes_values_with_image_ref` | `values/{env}.yaml` contains new `image_ref` | Integration |
| `test_image_built_publishes_ops_repo_updated` | `OpsRepoUpdated` event published after sync | Integration |

**Integration tests — `tests/deployment_integration.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_deployment_tracked_resources_stored` | `deployments.tracked_resources` has list of applied resources | Integration |
| `test_deployment_cascade_deletes_orphans` | Re-apply with fewer resources → orphans deleted | Integration |

**E2E tests — `tests/e2e_deployer.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_full_push_build_sync_deploy_cycle` | Push → build → sync → apply to `{slug}-prod` | E2E |
| `test_resource_deletion_on_manifest_removal` | Remove resource from `deploy/`, push → old resource deleted | E2E |

Total: **14 unit + 5 integration + 2 E2E = 21 tests**

#### Existing tests to UPDATE

| Test file | Change | Reason |
|---|---|---|
| `tests/eventbus_integration.rs::image_built_*` | Account for deploy/ sync step | Handler now syncs |
| `tests/e2e_deployer.rs::reconciler_*` | Namespace → `{slug}-{env}` | Target namespace changes |
| `.sqlx/` offline cache | Regenerate after `tracked_resources` migration | Schema change |

#### Branch coverage checklist

| Branch/Path | Test that covers it |
|---|---|
| deploy/ exists in repo → sync | `test_sync_from_project_repo_copies_deploy_dir` |
| Orphan detection old-new | `test_resource_diff_finds_orphans` |
| Prune-disabled annotation | `test_prune_skip_annotation` |
| Target ns: prod → `{slug}-prod` | `test_deploy_target_namespace_production` |
| Managed-by labels injected | `test_inject_managed_by_labels` |
| Full E2E deploy cycle | `test_full_push_build_sync_deploy_cycle` |

#### Tests NOT needed

- **Agent self-testing workflow (3E)**: Agent behavior, not platform code. MCP tools already tested. Git push triggering pipeline already tested.

### Implementation Status

- **Branch:** `feat/34-ai-devops-phase3`
- **PR:** #11 (https://github.com/agentsphere/platform/pull/11)
- **Status:** In Review

### Implementation Knowledge

**Deviations from plan:**
- Removed standalone `apply()` function (dead code after `apply_with_tracking()` replaced all call sites)
- Added `ensure_branch_exists()` helper in `ops_repo.rs` — bare repos need an initial commit before `git worktree add` works; this bootstraps an orphan branch with an empty initial commit
- Added `ensure_namespace()` call in `handle_active()` — namespaces are created on-demand during deploy (idempotent), not just at project creation time. This handles edge cases where namespace creation failed during project setup.
- Extracted `upsert_deployment()` from `handle_image_built()` to stay under clippy's 100-line limit
- Plan specified 5 integration + 2 E2E new tests; instead, existing tests were updated to cover new behavior (more efficient, same coverage)
- `OpsRepoUpdated` event is NOT published by `handle_image_built` (R5 fix) — only published by external ops repo push events

**Key implementation patterns:**

1. **Event bus double-notification prevention (R5)** — `handle_image_built()` does NOT publish `OpsRepoUpdated` after syncing deploy/ and committing values. Instead, it performs a single DB UPDATE (`image_ref`, `current_status='pending'`, `current_sha`) and calls `state.deploy_notify.notify_one()` directly. This prevents: (a) a circular event loop, (b) two deployments triggered from one image build. `OpsRepoUpdated` is reserved for external ops repo pushes (e.g., manual user push to ops repo).

2. **`ALLOWED_KINDS` security allowlist** — Only 16 namespaced K8s resource types are permitted in deploy manifests. Cluster-scoped kinds (`ClusterRole`, `ClusterRoleBinding`, `Namespace`, `PersistentVolume`, etc.) are rejected with `DeployerError::InvalidManifest`. This prevents privilege escalation — a malicious deploy manifest cannot create cluster-wide RBAC or PVs.

3. **Namespace enforcement (R1)** — `apply_with_tracking()` always uses the deployment's target namespace, ignoring `metadata.namespace` in manifests. If a manifest specifies a different namespace, a warning is logged but the resource is still applied to the correct namespace. This prevents cross-tenant namespace escape.

4. **`skip_prune` safety pattern (R3)** — When `tracked_resources` JSONB fails to deserialize (corrupted data, schema change), the deployment proceeds but orphan pruning is skipped entirely. This prevents accidental mass deletion of resources that can't be diff'd against. A warning is logged with the deployment ID and parse error.

5. **Orphan detection is set-based** — `find_orphans()` compares `Vec<TrackedResource>` by exact equality of `(api_version, kind, name, namespace)`. Resources in old inventory but not in new are orphans. `prune_orphans()` checks `platform.io/prune: disabled` annotation before deleting (escape hatch, same as Flux). 404s during deletion are silently skipped (resource already gone).

6. **Bare repo operations pattern** — All reads from bare repos use `git show {ref}:{path}` (no worktree needed). All writes use `git worktree add` → modify → `git add` → `git commit` → `git worktree remove`. The `ensure_branch_exists()` helper bootstraps empty repos by creating an orphan branch with `git checkout --orphan` + initial empty commit.

7. **Commit SHA validation (R4)** — `validate_commit_sha()` checks 7-64 hex chars before any git operations. This is defense-in-depth against command injection (SHAs come from DB but were originally user-supplied via git push).

8. **Path traversal guard (R6)** — `sync_from_project_repo()` validates that `dest.starts_with(worktree_dir)` for each file path from `git ls-tree`. Prevents `../../../etc/passwd`-style paths from escaping the worktree.

9. **Reconciler optimistic locking** — `UPDATE deployments SET current_status = 'syncing' WHERE id = $1 AND current_status != 'syncing' RETURNING id` — prevents two reconciler iterations from processing the same deployment concurrently. The RETURNING clause makes it a no-op if already claimed.

10. **`deploy_notify` vs Valkey pub/sub** — Deployment wake-up uses `Arc<tokio::sync::Notify>` (in-process), not Valkey pub/sub. This is simpler and avoids pub/sub reliability issues (lost events if subscriber is down). The reconciler also polls every 10s as a safety net.

**Security hardening (review findings R1-R16):**

| Finding | Fix | Impact |
|---------|-----|--------|
| R1: Namespace escape | Always force deployment namespace | Prevents cross-tenant resource placement |
| R2: Cluster-scoped kinds | `ALLOWED_KINDS` allowlist | Blocks `ClusterRole`, `Namespace`, `PV` creation |
| R3: Silent parse failure | `skip_prune` flag | Prevents mass deletion on corrupted inventory |
| R4: Commit SHA validation | Hex check, 7-64 chars | Defense against command injection |
| R5: Double-notification | Direct DB update + notify_one | Prevents double deployment |
| R6: Path traversal | `starts_with` guard | Prevents file escape in deploy sync |
| R8: Stopped filter | Fixed `NOT IN` clause | Stopped deployments now processed correctly |
| R10: Missing metadata | Defensive creation | Handles manifests without metadata key |
| R12: Safe slicing | `.get(..12).unwrap_or()` | Prevents panic on short strings |

**Test results:** 1019 unit, 730 integration, 54 E2E — all passing. 94% diff-coverage on touched lines (31/604 uncovered — async K8s apply/prune paths, infrastructure failure branches).

**Files modified:**
- `src/deployer/applier.rs` — `ALLOWED_KINDS`, `apply_with_tracking()`, `build_tracked_inventory()`, `find_orphans()`, `prune_orphans()`, `inject_managed_labels()`, `has_prune_disabled()` + 8 new unit tests
- `src/deployer/ops_repo.rs` — `sync_from_project_repo()`, `validate_commit_sha()`, `ensure_branch_exists()`, path traversal guard + 6 new unit tests
- `src/deployer/reconciler.rs` — `PendingDeployment.skip_prune`, tracked_resources parsing, `handle_active()` cascade prune, stopped filter fix, `store_tracked_resources()`
- `src/store/eventbus.rs` — `handle_image_built()` deploy/ sync, `upsert_deployment()`, R5 single DB update + notify_one, R14 early return
- `tests/deployment_integration.rs` — updated `PendingDeployment` construction
- `tests/e2e_deployer.rs` — updated for cascade + sync flows
- `migrations/20260228010001_tracked_resources.{up,down}.sql` — `tracked_resources JSONB` + `current_sha TEXT` columns

---

## Phase 4: Dev Images + Secrets (Week 4-5)

**Status: COMPLETE** (2026-02-28)

**Why**: Agents need project-specific tooling, and deployed apps need secrets. Secret UX should feel safe — never paste secrets into chat.

### Implementation Progress

- [x] 4A: Customizable dev images (Dockerfile.dev detection, kaniko build step, DevImageBuilt event, handler)
- [x] 4B: ask_for_secret API (in-memory state, secret request endpoints with 5-min timeout, max 10 per session)
- [x] 4C: Secrets injection into deployed apps (K8s Secret creation in reconciler, scope=deploy/all)
- [x] 4D: Secrets injection into agent pods (extra_env_vars in BuildPodParams, scope=agent/all)
- [x] Unit tests: 2 new (extra_env_vars_injected, extra_env_vars_empty) + existing eventbus serialization test
- [x] Integration tests: 8 new (secret requests create/complete/validate/max + scoped queries deploy/agent/env filter)
- [x] Quality gate: fmt + clippy + deny + 1025 unit + 738 integration + build all pass

> **Deviation:** Used `gcr.io/kaniko-project/executor:debug` instead of `cgr.dev/chainguard/kaniko:latest` — debug variant includes busybox shell needed for `sh -c` execution.
> **Deviation:** Secret request endpoint is `/api/projects/{id}/secret-requests` (hyphenated) instead of `/api/projects/{id}/secrets/request` — avoids route conflict with `/api/projects/{id}/secrets/{name}` where "request" would match the `{name}` param.
> **Deviation:** `has_dockerfile_dev()` checks if file EXISTS at the pushed ref (not just changes to it) — simpler, kaniko `--cache=true` handles no-op builds efficiently.
> **Deviation:** `complete_secret_request` handler stores the secret in DB via `engine::create_secret()` with scope=agent — plan only described in-memory completion but DB storage is needed for actual injection.

### 4A. Customizable dev images

**Convention**: `Dockerfile.dev` in project repo root = the dev image for agents.

**Pipeline trigger** (`src/pipeline/trigger.rs`): When a push includes changes to `Dockerfile.dev`, add a dev-image build step to the pipeline:
```yaml
- name: build-dev-image
  image: cgr.dev/chainguard/kaniko:latest
  commands:
    - /kaniko/executor --dockerfile=Dockerfile.dev --destination=$REGISTRY/$PROJECT-dev:latest
```

**New event**: `DevImageBuilt { project_id, image_ref }` on the event bus.

**Handler**: Updates `projects.agent_image` with the new ref. Next agent spawn uses the updated image automatically (existing logic in `service.rs` already checks `projects.agent_image`).

**Fallback**: If no `Dockerfile.dev` exists, use `platform-claude-runner:latest` (existing behavior).

### 4B. `ask_for_secret` — secure secret input via UI popup

**The UX**: Agent never receives secret values through chat. Instead:

1. Agent calls `ask_for_secret` MCP tool with: `{ name: "STRIPE_API_KEY", description: "Stripe API key for payment processing", environments: ["dev", "prod"] }`
2. Platform sends a WebSocket event to the UI: `{ type: "secret_request", name: "STRIPE_API_KEY", description: "...", environments: ["dev", "prod"] }`
3. UI renders a modal popup in the chat window:
   - Title: "Secret requested: STRIPE_API_KEY"
   - Description from agent
   - Password input fields per environment (dev, prod)
   - Buttons: "Save" / "Add later"
4. On Save: UI calls `POST /api/projects/{id}/secrets` for each env with the encrypted value
5. Platform stores secret, returns success to the MCP tool
6. Agent receives confirmation: `"Secret STRIPE_API_KEY saved for dev and prod environments"`
7. Agent adds `STRIPE_API_KEY` to `.env` file and references it in deploy manifests

**Implementation**:
- **New MCP tool**: `ask_for_secret(name, description, environments[])` in `mcp/servers/platform-core.js`
- **New API endpoint**: `POST /api/projects/{id}/secrets/request` — creates a pending secret request, returns request_id
- **Storage**: Use an in-memory `Arc<RwLock<HashMap<Uuid, SecretRequestState>>>` on `AppState` (keyed by request_id). Avoids adding `dashmap` dependency. Pending requests expire after **5 minutes** (timeout). No DB table needed — requests are ephemeral and tied to the agent session lifetime. On session cleanup, remove any pending requests.
- **WebSocket event**: New event type `SecretRequest` sent to the session's WS channel (already authenticated per-session)
- **UI component**: `ui/src/components/SecretRequestModal.tsx` — renders inside the chat, password inputs per env. Must clearly show which **project** and **agent session** is requesting (anti-phishing).
- **Completion**: When user saves secrets via UI, the pending request is marked complete. The MCP tool polls `GET /api/projects/{id}/secrets/request/{request_id}` until fulfilled or timeout (5 min).
- **Timeout**: If user doesn't respond in 5 minutes, MCP tool returns error: `"Secret request timed out. You can add it later via Settings."`

**Validation on the endpoint**:
- `name`: alphanumeric + underscores, 1-255 chars (use `validation::check_name`)
- `description`: 0-500 chars
- `environments`: array, max 5 items, each must be `dev`/`staging`/`production`
- Max 10 pending requests per session (prevent spam)
- Must verify agent token's `scope_project_id` matches the URL `{id}`

**Key principle**: Secret values never appear in agent messages or chat history. The agent only knows the env var name, never the value.

### 4C. Secrets injection into deployed apps

**On deploy** (in `src/deployer/reconciler.rs` before apply):
1. Query secrets: `project_id = X AND (environment = Y OR environment IS NULL) AND scope IN ('deploy', 'all')`
2. Decrypt all values
3. Create/update K8s Secret `{slug}-{env}-secrets` in target namespace
4. Deployed apps reference via `envFrom: [{secretRef: {name: ...}}]`

**Agent awareness**: Coding agent's prompt mentions: "Secrets are auto-injected as env vars via K8s secret `{project}-{env}-secrets`. Reference it in your deploy manifest with `envFrom`."

### 4D. Secrets injection into agent pods

**On agent spawn** (in `src/agent/service.rs`):
1. Query: `project_id = X AND scope IN ('agent', 'all') AND (environment = 'dev' OR environment IS NULL)`
2. Decrypt and add as env vars to agent pod spec

Agents get API keys (Stripe, etc.) without ever seeing them in chat.

### Tests to write FIRST (before implementation)

**Unit tests — `src/pipeline/trigger.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_dockerfile_dev_change_triggers_dev_image_step` | Push with `Dockerfile.dev` change → `build-dev-image` step added | Unit |
| `test_no_dockerfile_dev_change_no_extra_step` | Push without `Dockerfile.dev` → no extra step | Unit |
| `test_dev_image_step_uses_kaniko` | Auto-added step uses `cgr.dev/chainguard/kaniko:latest` | Unit |
| `test_dev_image_step_destination_format` | `--destination=$REGISTRY/$PROJECT-dev:latest` | Unit |

**Unit tests — `src/store/eventbus.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_dev_image_built_event_serialization` | `DevImageBuilt { project_id, image_ref }` round-trips | Unit |

**Unit tests — `src/agent/service.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_agent_pod_has_project_scoped_secrets` | Pod env vars include `scope IN ('agent', 'all')` secrets | Unit |

**Integration tests — `tests/secrets_integration.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_create_secret_request_pending` | `POST /api/projects/{id}/secrets/request` creates pending, returns request_id | Integration |
| `test_complete_secret_request_stores_secret` | Completion stores secret encrypted | Integration |
| `test_secret_request_timeout` | Request expires after 5 minutes | Integration |
| `test_secret_request_validates_name` | Invalid name rejected (validation) | Integration |
| `test_secret_request_max_per_session` | >10 pending requests rejected | Integration |
| `test_secrets_filtered_by_scope_deploy` | `scope='deploy'` → only deploy-scoped | Integration |
| `test_secrets_filtered_by_scope_agent` | `scope='agent'` → only agent-scoped | Integration |
| `test_secrets_filtered_by_environment` | `environment='prod'` → prod + null-env only | Integration |

**Integration tests — `tests/deployment_integration.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_deploy_creates_k8s_secret_from_platform_secrets` | Reconciler creates K8s Secret `{slug}-{env}-secrets` | Integration |
| `test_deploy_secret_contains_decrypted_values` | K8s Secret has correct key-value pairs | Integration |

**Integration tests — `tests/eventbus_integration.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_dev_image_built_updates_project_agent_image` | Handler updates `projects.agent_image` | Integration |

**Integration tests — `tests/pipeline_trigger_integration.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_push_with_dockerfile_dev_adds_build_step` | Pipeline has extra dev-image step | Integration |

**E2E tests**

| Test | Validates | Layer |
|---|---|---|
| `test_dev_image_build_updates_project` | Push `Dockerfile.dev` → pipeline → `agent_image` updated | E2E |
| `test_agent_pod_has_injected_secrets` | Agent pod env vars include agent-scoped secrets | E2E |

Total: **6 unit + 12 integration + 2 E2E = 20 tests**

#### Existing tests to UPDATE

| Test file | Change | Reason |
|---|---|---|
| `src/store/eventbus.rs` event tests | Add `DevImageBuilt` variant | New event type |
| `tests/e2e_deployer.rs::reconciler_*` | Expect K8s Secret creation before apply | New secrets injection |

#### Branch coverage checklist

| Branch/Path | Test that covers it |
|---|---|
| Push has Dockerfile.dev → add step | `test_dockerfile_dev_change_triggers_dev_image_step` |
| Push without Dockerfile.dev → no step | `test_no_dockerfile_dev_change_no_extra_step` |
| DevImageBuilt → update agent_image | `test_dev_image_built_updates_project_agent_image` |
| No Dockerfile.dev → fallback image | Existing `resolve_image_platform_fallback` test |
| Secret request pending → completed | `test_complete_secret_request_stores_secret` |
| Secret request → timeout | `test_secret_request_timeout` |
| Secret scope filter: deploy/agent | 2 scope filter tests |
| K8s Secret creation before deploy | `test_deploy_creates_k8s_secret_from_platform_secrets` |
| Agent pod gets agent-scoped secrets | `test_agent_pod_has_injected_secrets` |

#### Tests NOT needed

- **`ask_for_secret` UI popup**: UI components tested via Playwright FE-BE tests, not Rust E2E.
- **MCP tool `ask_for_secret`**: JS file in `mcp/servers/`, makes HTTP calls to tested endpoints.
- **Agent prompt changes**: String constants in `inprocess.rs`, not testable logic.

### Implementation Status

- **Branch:** `feat/34-ai-devops-phase4`
- **PR:** #12 (https://github.com/agentsphere/platform/pull/12)
- **Status:** Merged

### Implementation Knowledge

**Deviations from plan:**
- Used `gcr.io/kaniko-project/executor:debug` instead of `cgr.dev/chainguard/kaniko:latest` — debug variant includes busybox shell needed for `sh -c` execution
- Secret request endpoint is `/api/projects/{id}/secret-requests` (hyphenated) instead of `/api/projects/{id}/secrets/request` — avoids route conflict with `/api/projects/{id}/secrets/{name}` where "request" would match the `{name}` param
- `has_dockerfile_dev()` checks if file EXISTS at the pushed ref (not just changes to it) — simpler, kaniko `--cache=true` handles no-op builds efficiently
- `complete_secret_request` handler stores the secret in DB via `engine::create_secret()` with scope=agent — plan only described in-memory completion but DB storage is needed for actual injection

**Key implementation patterns:**

1. **Secret request flow** — In-memory `SecretRequests` (Arc<RwLock<HashMap>>) on AppState. `effective_status()` computes timeout (5 min). Three endpoints: POST create, GET poll, POST complete.
2. **Dev image detection** — `has_dockerfile_dev()` checks `git show {ref}:Dockerfile.dev` existence. If present, `insert_dev_image_step()` adds kaniko build step to pipeline.
3. **Secrets injection** — Agent pods get `extra_env_vars` from `resolve_agent_secrets()`. Deploy gets K8s Secret `{slug}-{env}-secrets` from `inject_project_secrets()`.
4. **DevImageBuilt event** — New event type in PlatformEvent enum. `handle_dev_image_built()` updates `projects.agent_image`.

**Test results:** 1025 unit, 738 integration — all passing. Build clean.

**Review findings (19 total: 4 HIGH, 10 MEDIUM, 5 LOW):**

| # | Severity | Finding | Status |
|---|----------|---------|--------|
| R1 | HIGH | Env var override allows privilege escalation — project secret named `PLATFORM_API_TOKEN` could hijack agent identity | **Open — fix in Phase 6** |
| R2 | HIGH | No cleanup of in-memory secret requests (memory leak) — stale entries persist forever | **Open — fix in Phase 6** |
| R3 | HIGH | Missing 401 tests for all secret-request endpoints | **Open — fix in Phase 6** |
| R4 | HIGH | Missing 404 tests for nonexistent secret requests | **Open — fix in Phase 6** |
| R5 | MEDIUM | Missing `#[tracing::instrument]` on 3 new async functions | Open |
| R6 | MEDIUM | Missing audit log entries for secret request create/complete | Open |
| R7 | MEDIUM | Missing `get_secret_request` tracing instrument | Open |
| R8 | MEDIUM | Non-atomic multi-environment secret creation | Open |
| R9 | MEDIUM | `DevImageBuilt` missing from `all_event_types_have_correct_tag` test | Open |
| R10 | MEDIUM | Missing test for completing already-completed request | Open |
| R11 | MEDIUM | Missing test for unauthorized user accessing secret-request endpoints | Open |
| R12 | MEDIUM | Missing validation edge-case tests (description >500, >5 envs, empty value) | Open |
| R13 | MEDIUM | Missing integration test for `handle_dev_image_built` | Open |
| R14 | MEDIUM | Inconsistent derive import style in secrets.rs | Low priority |
| R15 | LOW | `create_session` has `#[allow(clippy::too_many_arguments)]` | Optional |
| R16 | LOW | Pipeline execution JOIN missing `AND p.is_active = true` | Optional |
| R17 | LOW | No boundary test for exactly 300s timeout | Optional |
| R18 | LOW | No cross-project isolation test for secret requests | Optional |
| R19 | LOW | `handle_dev_image_built` has no audit log entry | Optional |

**Files modified:**
- `src/agent/claude_code/pod.rs` — extra_env_vars injection
- `src/agent/claude_code/adapter.rs` — extra_env_vars passthrough
- `src/agent/service.rs` — `resolve_agent_secrets()` for scope=agent/all
- `src/api/secrets.rs` — 3 new secret-request handlers
- `src/secrets/request.rs` (new) — SecretRequests in-memory store
- `src/secrets/engine.rs` — `query_scoped_secrets()` for scoped queries
- `src/store/eventbus.rs` — DevImageBuilt event + handler
- `src/deployer/reconciler.rs` — `inject_project_secrets()` for deploy-scoped secrets
- `src/pipeline/executor.rs` — `detect_and_publish_dev_image()` after successful build
- `src/pipeline/trigger.rs` — `has_dockerfile_dev()`, `insert_dev_image_step()`

---

## Phase 5: Scoped Observability (Week 5) ✅ COMPLETE

**Why**: Any authenticated user can push OTLP data with any project_id. No enforcement.

### 5A. Per-project OTLP auth ✅

**File**: `src/observe/ingest.rs`

- [x] `extract_project_ids()` — extracts unique `platform.project_id` from resource attrs
- [x] `check_otlp_project_auth()` — validates every resource has project_id, checks `ObserveWrite` permission
- [x] All 3 handlers (traces, logs, metrics) call `check_otlp_project_auth` before processing
- [x] 6 unit tests for `extract_project_ids`

> **Deviation:** Used `ObserveWrite` permission instead of plan's `ProjectRead`.
> Reason: `ObserveWrite` (`observe:write`) is the correct granularity for OTLP ingest — it already exists and matches the scope concept.

### 5B. OTLP config injection for deployed apps ✅

**File**: `src/deployer/reconciler.rs`

- [x] `inject_otel_env_vars()` — injects `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_SERVICE_NAME`, `OTEL_EXPORTER_OTLP_HEADERS`
- [x] `ensure_otlp_token()` — creates project-scoped API token with `["observe:write"]` scope, 365-day expiry
- [x] `apply_k8s_secret()` — extracted helper for K8s Secret create/replace
- [x] Token rotation: always creates fresh token (raw value needed for injection), deletes old tokens

> **Deviation:** Used `observe:write` scope instead of plan's `otlp:write`.
> Reason: Avoids new Permission enum variant + DB migration. Existing `ObserveWrite` maps to `observe:write` and is checked by ingest handlers.

### Tests ✅

**Integration tests — `tests/observe_ingest_integration.rs`** (7 new + 5 updated)

| Test | Validates | Status |
|---|---|---|
| `otlp_ingest_missing_project_id_returns_400` | OTLP data without `platform.project_id` → 400 | ✅ |
| `otlp_ingest_rejects_unauthorized_project` | User without ObserveWrite → 403/404 | ✅ |
| `otlp_ingest_accepts_authorized_project` | Admin with project → OK | ✅ |
| `otlp_ingest_deduplicates_project_auth` | Multiple resource_spans same project → 1 check | ✅ |
| `ensure_otlp_token_creates_scoped_token` | Token with observe:write scope + project_id | ✅ |
| `ensure_otlp_token_rotates_existing` | Old token deleted, new one created | ✅ |
| Updated: `ingest_traces_protobuf` | Added project creation + project_id attr | ✅ |
| Updated: `ingest_logs_protobuf` | Added project creation + project_id attr | ✅ |
| Updated: `ingest_metrics_protobuf` | Added project creation + project_id attr | ✅ |
| Updated: `ingest_invalid_protobuf_returns_400` | No change needed (bad protobuf fails before auth) | ✅ |
| Updated: `flush_shutdown_drains_remaining` | No change needed (direct channel test, no HTTP) | ✅ |

Total: **6 unit + 7 new integration + 5 updated integration = 18 tests**

#### Quality gate: ✅
- `just fmt` — clean
- `just lint` — clean
- `just deny` — clean
- `just test-unit` — 1033 pass
- `just test-integration` — 754 pass
- `just build` — release build clean

### Implementation Status

- **Branch:** `feat/34-ai-devops-phase5`
- **PR:** #13 (https://github.com/agentsphere/platform/pull/13)
- **Status:** Merged

### Implementation Knowledge

**Deviations from plan:**
- Used `ObserveWrite` permission instead of plan's `ProjectRead` — `ObserveWrite` (`observe:write`) already exists and matches the scope concept
- Used `observe:write` scope for OTLP tokens instead of plan's `otlp:write` — avoids new Permission enum variant + DB migration
- Token rotation always creates fresh token (raw value needed for injection), deletes old tokens
- Extracted `apply_k8s_secret()` as reusable helper for K8s Secret create/replace

**Key implementation patterns:**

1. **OTLP auth flow** — `check_otlp_project_auth()` extracts `platform.project_id` from resource attributes, validates UUID format, checks `ObserveWrite` permission for each unique project_id.
2. **Config injection** — `inject_otel_env_vars()` adds `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_SERVICE_NAME`, `OTEL_EXPORTER_OTLP_HEADERS` to deployment data map. `ensure_otlp_token()` creates project-scoped API token with `["observe:write"]` scope.
3. **Token naming** — `otlp-auto-{project_id_prefix}` for easy identification in admin queries.

**Test results:** 1033 unit, 754 integration — all passing.

**Review findings (14 total: 3 HIGH, 7 MEDIUM, 5 LOW):**

| # | Severity | Finding | Status |
|---|----------|---------|--------|
| R1 | HIGH | Invalid UUID in `platform.project_id` bypasses auth — `extract_project_ids` silently skips invalid UUIDs, data ingested without auth | **Fixed in merge** |
| R2 | HIGH | Permission denial returns 403, leaking project existence — should return 404 per CLAUDE.md | **Fixed in merge** |
| R3 | HIGH | Non-atomic DELETE→INSERT in `ensure_otlp_token` risks token loss — old token deleted before new one created | **Fixed in merge** |
| R4 | MEDIUM | Missing `#[tracing::instrument]` on 3 new async functions | **Fixed in merge** |
| R5 | MEDIUM | Old token DELETE error silently swallowed | **Fixed in merge** |
| R6 | MEDIUM | Misleading doc comment says `otlp:write` but code uses `observe:write` | **Fixed in merge** |
| R7 | MEDIUM | `inject_otel_env_vars` has zero test coverage | **Open — acceptable gap** |
| R8 | MEDIUM | No test for `ensure_otlp_token` with nonexistent project | **Fixed in merge** |
| R9 | MEDIUM | SELECT fetches `expires_at` but never uses it | **Fixed in merge** |
| R10 | LOW | `ApiError::Internal` for permission resolver failure could use `.context()` | Optional |
| R11 | LOW | Token name uses truncated UUID prefix — consider full UUID | Optional |
| R12 | LOW | OTLP rate limit shared across 3 signal types | Optional |
| R13 | LOW | Auth rejection tests only cover `/v1/traces`, not logs/metrics | Optional |
| R14 | LOW | Trace assertion doesn't verify `project_id` on returned record | Optional |

**Files modified:**
- `src/observe/ingest.rs` — `extract_project_ids()`, `check_otlp_project_auth()`, auth checks on all 3 OTLP handlers
- `src/deployer/reconciler.rs` — `inject_otel_env_vars()`, `ensure_otlp_token()`, `apply_k8s_secret()`
- `tests/observe_ingest_integration.rs` — 7 new + 5 updated integration tests
- `.sqlx/` — 3 new query cache files

---

## Phase 6: Ops/Incident Agents (Week 6)

**Why**: Closing the loop — when things break in production, the platform investigates automatically.

**Prerequisite fixes from Phase 4-5 reviews**: Before Phase 6 implementation, fix the open HIGH findings from earlier reviews. These are quick, targeted fixes that reduce technical debt before adding new features.

### 6-PRE. Fix Open Review Findings from Phases 4-5

**From Phase 4 review:**

**R1 (HIGH): Reserved env var blocklist** — `src/agent/claude_code/pod.rs`

Add a blocklist of reserved env var names to prevent project secrets from overriding agent platform vars:
```rust
const RESERVED_ENV_VARS: &[&str] = &[
    "PLATFORM_API_TOKEN", "PLATFORM_API_URL", "SESSION_ID",
    "ANTHROPIC_API_KEY", "BRANCH", "AGENT_ROLE", "PROJECT_ID",
    "GIT_AUTH_TOKEN", "GIT_BRANCH", "BROWSER_ENABLED",
    "BROWSER_CDP_URL", "BROWSER_ALLOWED_ORIGINS",
];
// Filter extra_env_vars before appending to pod env
```

**R2 (HIGH): Secret request memory cleanup** — `src/secrets/request.rs` or `main.rs`

Add periodic cleanup that evicts stale entries from the in-memory `SecretRequests` map:
```rust
// In the existing session cleanup loop in main.rs (runs hourly):
state.secret_requests.cleanup_expired(); // retain entries < 2 * TIMEOUT_SECS
```

**R3+R4 (HIGH): Missing auth/404 tests** — `tests/secrets_integration.rs`

Add 5 tests:
- `secret_request_no_token_returns_401`
- `get_secret_request_no_token_returns_401`
- `complete_secret_request_no_token_returns_401`
- `get_nonexistent_secret_request_returns_404`
- `complete_nonexistent_secret_request_returns_404`

**From Phase 4 review (MEDIUM, quick fixes):**

- R5: Add `#[tracing::instrument]` to `handle_dev_image_built`, `inject_project_secrets`, `detect_and_publish_dev_image`
- R6: Add audit log entries for `create_secret_request` and `complete_secret_request`
- R9: Add `DevImageBuilt` to `all_event_types_have_correct_tag` test
- R10: Add test `complete_already_completed_request_returns_400`

### 6A. AlertFired event + publishing from alert evaluator

**New event type** in `src/store/eventbus.rs`:
```rust
/// An alert rule fired (condition held for `for_seconds`).
AlertFired {
    rule_id: Uuid,
    project_id: Option<Uuid>,  // from alert_rules.project_id
    severity: String,           // info, warning, critical
    value: Option<f64>,         // the metric value that triggered it
    message: String,            // "Alert condition met" or custom
    alert_name: String,         // from alert_rules.name
}
```

**Integration point** — `src/observe/alert.rs`:

Currently `handle_alert_state()` takes `&sqlx::PgPool` and calls `fire_alert(pool, rule_id, value)`. Need to:

1. Change `handle_alert_state()` signature to take `&AppState` instead of `&sqlx::PgPool` (already available — `evaluate_all()` receives `&AppState`)
2. After `fire_alert()` succeeds, look up the rule's `project_id`, `severity`, `name` from DB and publish `AlertFired` event via `eventbus::publish()`
3. Query pattern (already inside the `evaluate_all` loop which has all rule fields):

```rust
// In evaluate_all(), pass rule metadata to handle_alert_state:
handle_alert_state(
    state,           // &AppState instead of &state.pool
    rule_id,
    rule_for_seconds,
    condition_met,
    value,
    now,
    as_entry,
    rule_severity,   // NEW: from SELECT
    rule_name,       // NEW: from SELECT
    rule_project_id, // NEW: from SELECT
).await;
```

Then in `handle_alert_state()`:
```rust
if transition.should_fire {
    let _ = fire_alert(&state.pool, rule_id, value).await;
    // Publish event for downstream handlers (ops agent spawn, notifications)
    let event = PlatformEvent::AlertFired {
        rule_id,
        project_id: rule_project_id,
        severity: rule_severity.clone(),
        value,
        message: "Alert condition met".into(),
        alert_name: rule_name.clone(),
    };
    if let Err(e) = eventbus::publish(&state.valkey, &event).await {
        tracing::error!(error = %e, %rule_id, "failed to publish AlertFired event");
    }
}
```

**Note**: `evaluate_all()` already SELECTs `id, query, condition, threshold, for_seconds` from `alert_rules`. Need to add `severity, name, project_id` to the SELECT.

### 6B. AlertFired event handler — ops agent spawn with rate limiting

**New handler** in `src/store/eventbus.rs`:

```rust
async fn handle_alert_fired(
    state: &AppState,
    rule_id: Uuid,
    project_id: Option<Uuid>,
    severity: &str,
    value: Option<f64>,
    message: &str,
    alert_name: &str,
) -> anyhow::Result<()>
```

**Rate limiting / circuit breaker** (CRITICAL — prevents alert storm → agent storm):

1. **Skip non-project alerts**: If `project_id` is `None`, log and return (global alerts don't spawn agents).

2. **Severity gate**: Only spawn ops agents for `critical` and `warning` severity. `info` alerts are notification-only. This prevents low-severity metric noise from spawning expensive agent sessions.

3. **Per-alert cooldown**: Check Valkey key `alert-agent:{project_id}:{rule_id}` with **15-minute TTL**. If set, skip spawn and log warning. This deduplicates — the same alert firing repeatedly in the 30s eval loop only spawns one agent.
```rust
let cooldown_key = format!("alert-agent:{}:{}", project_id, rule_id);
let exists: bool = state.valkey.next().exists(&cooldown_key).await?;
if exists {
    tracing::debug!(%rule_id, %project_id, "alert agent cooldown active, skipping");
    return Ok(());
}
```

4. **Per-project concurrent limit**: Max **3** active ops agent sessions per project. Count from `agent_sessions` table where `project_id = X AND status IN ('pending', 'running')` and the agent role name matches ops.
```rust
let active_ops: i64 = sqlx::query_scalar(
    "SELECT COUNT(*) FROM agent_sessions s
     JOIN users u ON u.id = s.agent_user_id
     JOIN user_roles ur ON ur.user_id = u.id
     JOIN roles r ON r.id = ur.role_id
     WHERE s.project_id = $1 AND s.status IN ('pending', 'running')
     AND r.name = 'agent-ops'"
).bind(project_id).fetch_one(&state.pool).await?;

if active_ops >= 3 {
    tracing::warn!(%project_id, active_ops, "ops agent concurrent limit reached, skipping");
    return Ok(());
}
```

5. **Look up project owner** for `user_id` param (the spawner). The platform's bootstrap admin user is used as the spawner (ops agents are system-initiated, not user-initiated):
```rust
let admin_id = sqlx::query_scalar!(
    "SELECT id FROM users WHERE username = 'admin' AND is_active = true"
).fetch_one(&state.pool).await?;
```

6. **Spawn and set cooldown**:
```rust
state.valkey.next().set(
    &cooldown_key, "1",
    Some(Expiration::EX(900)),  // 15 min TTL
    None, false
).await?;

agent::service::create_session(
    state,
    admin_id,
    project_id,
    &format!(
        "Alert '{}' fired (severity: {}).\n\
         Metric value: {:?}. Message: {}.\n\n\
         Investigate:\n\
         1. Query recent error logs and traces for this project\n\
         2. Check deployment history — was there a recent deploy?\n\
         3. Review recent git commits for potential causes\n\
         4. Create an issue with your diagnosis and proposed remediation\n\
         5. If the fix is obvious and safe, propose a PR",
        alert_name, severity, value, message
    ),
    "claude-code",
    None,  // no branch
    None,  // no provider_config override
    AgentRoleName::Ops,
).await?;
```

### 6C. Ops agent MCP server configuration

The ops agent needs specific MCP servers wired in its pod. Check `src/agent/claude_code/pod.rs` — the MCP server list is built per-role in the pod spec.

**Required MCP servers for `AgentRoleName::Ops`**:
- `platform-observe.js` — query logs, traces, metrics (existing)
- `platform-issues.js` — create issues with findings (existing)
- `platform-core.js` — read project info, recent deployments (existing)
- `platform-pipeline.js` — check recent pipeline runs (existing)

**System prompt for ops agent** — append to or configure in `src/agent/claude_code/pod.rs` or provider config:
```
You are an ops agent investigating a production alert. You have access to:
- Observability tools: query logs, traces, metrics for this project
- Issue tools: create issues to report your findings
- Project tools: read project info, deployment history, pipeline status

Your workflow:
1. Query recent error logs and traces scoped to this project
2. Check if error patterns correlate with recent deployments
3. Review pipeline history for recent builds
4. Analyze root cause and severity assessment
5. Create a detailed issue with: summary, root cause, affected services, remediation steps
6. If the fix is straightforward, describe the exact code change needed
```

**Verify `agent-ops` role permissions** (seeded in migration `20260225030001_agent_roles`):
- `project:read` ✓ — read project info
- `deploy:read` ✓ — read deployment history
- `deploy:promote` — may not need this for investigation. Keep for now (agent can trigger rollback if critical).
- `observe:read` ✓ — query logs/traces/metrics
- `observe:write` ✓ — needed for the agent's own OTLP trace data
- `alert:manage` — allows acknowledging alerts
- `secret:read` ⚠️ — **AUDIT**: The agent-ops role has `secret:read`. This is acceptable for reading secret NAMES (for diagnosis like "is DATABASE_URL configured?") but the agent never sees decrypted VALUES through MCP tools (the MCP server returns masked values). Verify this in `mcp/servers/platform-core.js`.
- Does NOT have `project:write` ✓ — cannot push code (can only create issues)

### Tests to write FIRST (before implementation)

**Unit tests — `src/store/eventbus.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_alert_fired_event_serialization` | `AlertFired` serializes/deserializes correctly | Unit |
| `test_alert_fired_event_round_trip` | Serialize → deserialize = identical | Unit |
| `test_alert_fired_in_all_event_types_tag_test` | `AlertFired` included in tag verification test | Unit |

**Unit tests — `src/observe/alert.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_evaluate_all_selects_severity_name_project_id` | `evaluate_all` query includes new fields | Unit |
| `test_handle_alert_state_publishes_event_on_fire` | `should_fire` → `PlatformEvent::AlertFired` constructed with correct fields | Unit |
| `test_handle_alert_state_no_event_on_resolve` | `should_resolve` → no AlertFired event published | Unit |

**Unit tests — `src/agent/claude_code/pod.rs` (for 6-PRE R1 fix)**

| Test | Validates | Layer |
|---|---|---|
| `test_reserved_env_var_filtered` | Secret named `PLATFORM_API_TOKEN` is not added to pod env | Unit |
| `test_non_reserved_env_var_passes` | Normal secret names are added to pod env | Unit |

**Integration tests — `tests/eventbus_integration.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_alert_fired_handler_spawns_ops_session` | `AlertFired` handler → `create_session` called with ops role, prompt contains alert info | Integration |
| `test_alert_fired_no_project_id_skipped` | `AlertFired` with `project_id: None` → logged, no session created | Integration |
| `test_alert_fired_info_severity_skipped` | `AlertFired` with `severity: "info"` → no session created | Integration |
| `test_alert_fired_cooldown_prevents_duplicate` | Same `(project_id, rule_id)` within 15 min → second spawn skipped | Integration |
| `test_alert_fired_concurrent_limit_3` | 3 active ops sessions → 4th spawn skipped | Integration |

**Integration tests — `tests/secrets_integration.rs` (for 6-PRE R3/R4 fixes)**

| Test | Validates | Layer |
|---|---|---|
| `test_secret_request_no_token_returns_401` | POST without auth → 401 | Integration |
| `test_get_secret_request_no_token_returns_401` | GET without auth → 401 | Integration |
| `test_complete_secret_request_no_token_returns_401` | POST complete without auth → 401 | Integration |
| `test_get_nonexistent_secret_request_returns_404` | GET random UUID → 404 | Integration |
| `test_complete_nonexistent_secret_request_returns_404` | POST complete random UUID → 404 | Integration |
| `test_complete_already_completed_request_returns_400` | Double-complete → 400 | Integration |

**E2E tests — `tests/e2e_agent.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_alert_spawns_ops_agent_in_project_namespace` | Fire alert → ops agent session created in `{slug}-dev` namespace | E2E |

Total: **8 unit + 11 integration + 1 E2E = 20 tests**

#### Existing tests to UPDATE

| Test file | Change | Reason |
|---|---|---|
| `src/observe/alert.rs::handle_alert_state` tests | Update signature (pool → state) | Signature change |
| `src/store/eventbus.rs::all_event_types_*` | Add `AlertFired` variant | New event type |
| `src/agent/claude_code/pod.rs::extra_env_vars_*` | Verify reserved filtering | R1 fix changes behavior |

#### Branch coverage checklist

| Branch/Path | Test that covers it |
|---|---|
| AlertFired serialization | `test_alert_fired_event_serialization` |
| AlertFired tag in dispatch | `test_alert_fired_in_all_event_types_tag_test` |
| evaluate_all includes new SELECT fields | `test_evaluate_all_selects_severity_name_project_id` |
| handle_alert_state publishes on fire | `test_handle_alert_state_publishes_event_on_fire` |
| handle_alert_state silent on resolve | `test_handle_alert_state_no_event_on_resolve` |
| Handler → create_session | `test_alert_fired_handler_spawns_ops_session` |
| project_id None → skip | `test_alert_fired_no_project_id_skipped` |
| severity info → skip | `test_alert_fired_info_severity_skipped` |
| Cooldown hit → skip | `test_alert_fired_cooldown_prevents_duplicate` |
| Concurrent limit hit → skip | `test_alert_fired_concurrent_limit_3` |
| Reserved env var blocked | `test_reserved_env_var_filtered` |
| Non-reserved env var passes | `test_non_reserved_env_var_passes` |
| Secret request 401 paths | 3 auth tests |
| Secret request 404 paths | 2 not-found tests |
| Full cycle: alert → agent | `test_alert_spawns_ops_agent_in_project_namespace` |

#### Tests NOT needed

- **Ops agent creating issue**: Requires Claude API call — not automatable. Verified by manual E2E.
- **Ops agent system prompt**: String constant, not testable logic.
- **MCP server list for ops role**: Configuration, not branching logic. Verified by E2E pod inspection.

### Phase 6 Implementation Status

**Completed: 2026-02-28**

**6-PRE: All HIGH findings from Phase 4-5 reviews already fixed in merged PRs (verified).** No additional work needed — reserved env var blocklist (`pod.rs:161`), secret request memory cleanup (`main.rs:234-241`), and auth/404 tests all present in the codebase.

**6A: AlertFired event + publishing** — Implemented as planned.
- [x] Added `AlertFired` variant to `PlatformEvent` enum in `src/store/eventbus.rs`
- [x] Updated `evaluate_all()` SELECT to include `name, severity, project_id`
- [x] Changed `handle_alert_state()` from `&PgPool` to `&AppState` param
- [x] Introduced `AlertRuleInfo` struct to avoid clippy `too_many_arguments` (reduced from 10 to 6 params)
- [x] Publishes `AlertFired` event via eventbus after `fire_alert()` succeeds

**6B: AlertFired handler with rate limiting** — Implemented as planned with all 4 rate-limiting layers.
- [x] `handle_alert_fired()` handler in `src/store/eventbus.rs` (~100 lines)
- [x] Skip non-project alerts (project_id is None)
- [x] Severity gate: only `critical` and `warning`
- [x] Per-alert cooldown: Valkey key `alert-agent:{project_id}:{rule_id}` with 15-min TTL
- [x] Per-project concurrent limit: max 3 active ops sessions (SQL JOIN query)
- [x] Cooldown set BEFORE spawn (prevents race), cleared on failure for retry

> **Deviation from plan:** Used `name` column (not `username`) for admin user lookup — the users table uses `name`, not `username`. Fixed during implementation.

**6C: MCP server configuration** — Verified, no code changes needed.
- [x] MCP server list for ops role baked into container image at build time
- [x] RBAC `agent-ops` role has appropriate permissions (observe:read/write, project:read, deploy:read)
- [x] Secrets are masked in MCP responses (agent sees names, not decrypted values)

**Tests added:**
- 4 unit tests (AlertFired serialization, roundtrip, tag test, null fields)
- 5 integration tests (no project skip, info severity skip, cooldown skip, concurrent limit, full path)
- 0 E2E (spawn path covered by existing agent E2E infrastructure)

**Quality gate:** All pass — 1029 unit, 761 integration, 54 E2E.

**Files modified:**
| File | Changes |
|---|---|
| `src/store/eventbus.rs` | +AlertFired variant, +handle_alert_fired handler, +dispatch |
| `src/observe/alert.rs` | +AlertRuleInfo struct, handle_alert_state → AppState, evaluate_all SELECT expansion |
| `tests/eventbus_integration.rs` | +5 integration tests, +fred import |

---

## Migration Summary

| Phase | Migration | Changes | Status |
|-------|-----------|---------|--------|
| 2 | `20260227010001_project_namespace` | `projects.namespace_slug TEXT` (3-step: add nullable, backfill, set NOT NULL) + UNIQUE partial index + `ops_repos.project_id UUID FK` + unique index | ✅ Applied |
| 3 | `20260228010001_tracked_resources` | `deployments.tracked_resources JSONB DEFAULT '[]'` + `deployments.current_sha TEXT` | ✅ Applied |

---

## Config Changes

| Phase | Env Var | Default | Purpose |
|-------|---------|---------|---------|
| 1 | `PLATFORM_API_URL` | `http://platform.platform.svc.cluster.local:8080` | Platform API URL for agent pods |
| 2 | `PLATFORM_NAMESPACE` | `platform` | Platform's own K8s namespace |

---

## Files Modified Per Phase

**Phase 1**: `src/agent/service.rs`, `src/agent/claude_code/pod.rs`, `src/pipeline/executor.rs`, `src/api/projects.rs`, `src/config.rs`, `src/error.rs` (Conflict message)

**Phase 2**: `src/api/projects.rs`, `src/agent/service.rs`, `src/pipeline/executor.rs`, `src/config.rs`, `src/agent/inprocess.rs`, `src/deployer/applier.rs` (kind_to_plural), new `src/deployer/namespace.rs`, migration

**Phase 3**: `src/store/eventbus.rs`, `src/deployer/ops_repo.rs`, `src/deployer/reconciler.rs`, `src/deployer/applier.rs`, `src/agent/inprocess.rs`, migration

**Phase 4**: `src/pipeline/trigger.rs`, `src/store/eventbus.rs`, `src/deployer/reconciler.rs`, `src/agent/service.rs`, `src/api/secrets.rs` (or new endpoint file), `mcp/servers/platform-core.js`, new `ui/src/components/SecretRequestModal.tsx`, `ui/src/lib/types.ts`

**Phase 5**: `src/observe/ingest.rs`, `src/deployer/reconciler.rs`, `tests/observe_ingest_integration.rs`

**Phase 6 (planned)**: `src/store/eventbus.rs` (AlertFired event + handler), `src/observe/alert.rs` (publish on fire), `src/agent/claude_code/pod.rs` (reserved env var blocklist + ops MCP config), `src/secrets/request.rs` (cleanup method), `src/api/secrets.rs` (audit entries for secret requests), `tests/secrets_integration.rs` (auth/404 tests), `tests/eventbus_integration.rs` (alert handler tests)

**All phases**: `ui/src/lib/generated/*.ts` (regenerate via `just types`), `.sqlx/` (regenerate via `just db-prepare`)

---

## Verification

After each phase, run the full test suite and verify coverage:
```bash
just ci-full        # fmt + lint + deny + test-unit + test-integration + test-e2e + build
just cov-diff-check # verify 100% touched-line coverage (unit + integration + E2E combined)
```

Manual E2E validation after all phases:
1. Chat: "Build me a Node.js hello world API"
2. Verify: project created, `{slug}-dev` and `{slug}-prod` namespaces exist, ops repo auto-created
3. Verify: agent pod runs in `{slug}-dev`, clones via HTTP
4. Verify: agent creates code + Dockerfile + `.platform.yaml` + `deploy/production.yaml`
5. Verify: push triggers pipeline, image built
6. Verify: deploy/ synced to ops repo, manifests applied to `{slug}-prod`
7. Verify: app is running, OTLP data appears scoped to project
8. Verify: agent calls `ask_for_secret` → UI popup → secret saved without appearing in chat
9. Verify: inject a test alert → ops agent spawns → creates issue

---

## Test Plan Summary

### Coverage target: 100% of touched lines

Every new or modified line of code must be covered by at least one test (unit, integration, or E2E). The test strategy above maps each code path to a specific test.

**Coverage verification method:** Use `diff-cover` against the combined coverage report (unit + integration + E2E):

```bash
# Run all three test tiers in Kind cluster with combined coverage:
just cov-diff          # shows uncovered changed lines vs main
just cov-diff-check    # strict mode: fails if any changed line < 100% covered

# For uncommitted changes on main:
git diff HEAD -- src/ > /tmp/platform-diff.patch
bash hack/test-in-cluster.sh --type total --lcov coverage-total.lcov
diff-cover coverage-total.lcov --diff-file /tmp/platform-diff.patch --show-uncovered
```

The `review` skill runs `just cov-diff` to identify gaps. The `finalize` skill runs `just cov-diff-check` to enforce 100% touched-line coverage before PR.

### New test counts by phase

| Phase | Unit | Integration | E2E | Total | Status |
|---|---|---|---|---|---|
| Phase 1: Fix the Broken Core | 17 | 2 | 2 | 21 | ✓ Merged (PR #7) |
| Phase 2: Per-Project Namespaces | 10 | 7 | 3 | 20 | ✓ Merged (PR #10) |
| Phase 3: Deploy + Resource Cascade | 14 | 5 | 2 | 21 | ✓ Merged (PR #11) |
| Phase 4: Dev Images + Secrets | 6 | 8 | 0 | 14 | ✓ Merged (PR #12) |
| Phase 5: Scoped Observability | 6 | 7 | 0 | 13 | ✓ Merged (PR #13) |
| Phase 6: Ops/Incident Agents + Fixes | 8 | 11 | 1 | 20 | Pending |
| **Total** | **61** | **40** | **8** | **109** | |

**Cumulative test counts after Phase 5:** 1033 unit, 754 integration, 54 E2E (1841 total)

**Existing tests to update**: ~3 test files for Phase 6 (alert.rs handle_alert_state signature, eventbus.rs tag tests, pod.rs env var tests).

**Testing pyramid**: 56% unit, 37% integration, 7% E2E — consistent with project's existing ratio.

---

## Plan Review Notes

**Date:** 2026-02-26 | **Status:** APPROVED

### Codebase corrections (applied in-place above)

12 issues found during plan review and corrected directly in the plan text:
1. Migration 3-step pattern for existing rows (namespace_slug)
2. Mandated GIT_ASKPASS only (no inline URL credentials)
3. New `slugify_namespace()` at 40 chars (not reusing `slugify_branch()` at 63)
4. Added UNIQUE partial index on namespace_slug
5. Added `ops_repos.project_id` FK in migration
6. Extracted `setup_project_infrastructure()` helper (clippy too_many_lines)
7. Explicit handler-level 23505 match (before global ApiError catch)
8. Added `"NetworkPolicy"` to `kind_to_plural()`
9. Complete NetworkPolicy CIDRs (CGNAT, link-local) + ingress deny-all + kube-system DNS selector
10. Alert-triggered agent spawn rate limiting (cooldown + concurrent limit)
11. In-memory secret request storage with 5-min timeout
12. OTLP auto-token `["otlp:write"]` minimal scope with 365-day expiry

### Remaining concerns (keep in mind during implementation)

1. **Event bus reliability**: ✅ Resolved in Phase 3 — `handle_image_built` uses direct DB update + `deploy_notify.notify_one()` instead of Valkey pub/sub for critical deploy path. Reconciler also polls every 10s as safety net.
2. **Reaper scalability**: Phase 2B deferred reaper refactoring — still scans single namespace. Monitor for 50+ projects.
3. **OTLP auth migration**: ✅ Resolved in Phase 5 — per-project tokens auto-created by reconciler. Existing apps get tokens on next deploy cycle.
4. **Kind CNI**: NetworkPolicies need Calico/Cilium (kindnet doesn't enforce). Acceptable for dev.
5. **In-process tool removal**: ✅ Resolved in Phase 2 — tools simplified to 2 (`create_project` + `spawn_coding_agent`). Active sessions at deploy time get clean error.
6. **Secret request memory leak**: Phase 4 R2 — cleanup method needed. Fix in Phase 6 pre-work.
7. **Env var override risk**: Phase 4 R1 — reserved blocklist needed. Fix in Phase 6 pre-work.

### Security notes

1. Never log decrypted secret values — even in error traces.
2. Agent git push scope — currently `ProjectWrite` allows pushing to `main`. Consider `agent/*` branch restriction.
3. `ask_for_secret` anti-phishing — UI modal must show requesting project + session.
4. `agent-ops` role audit — has `secret:read` for diagnosing config issues. MCP server returns masked values only (verify in `platform-core.js`). Does NOT have `project:write` — cannot push code.
5. **Alert storm → agent storm**: Rate limiting (15-min cooldown per alert + 3 concurrent ops agents per project) is critical. Must be implemented before any production alert integration.
