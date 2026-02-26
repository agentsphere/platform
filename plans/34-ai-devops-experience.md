# AI DevOps Platform — End-to-End Experience Plan

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

## Phase 1: Fix the Broken Core (Week 1)

**Why**: Agents can't work reliably today. `file://` clone breaks across namespaces, duplicate project names surface raw SQL, no pod security.

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

## Phase 2: Per-Project Namespaces + Network Isolation (Week 2)

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

---

## Phase 3: Deploy from Project Repo + Resource Cascade (Week 3-4)

**Why**: Agents know best what their app needs for deployment. Deploy config should live in the project repo. Ops repo tracks deployment state.

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

---

## Phase 4: Dev Images + Secrets (Week 4-5)

**Why**: Agents need project-specific tooling, and deployed apps need secrets. Secret UX should feel safe — never paste secrets into chat.

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

---

## Phase 5: Scoped Observability (Week 5)

**Why**: Any authenticated user can push OTLP data with any project_id. No enforcement.

### 5A. Per-project OTLP auth

**File**: `src/observe/ingest.rs`

After parsing OTLP protobuf, extract `project_id` from resource attributes. Check that `AuthUser` has `ProjectRead` permission for that project. Reject with 403 if unauthorized.

Cache the permission check per-request (many spans share the same project_id).

### 5B. OTLP config injection for deployed apps

When deployer creates the project secrets K8s Secret (Phase 4C), also include:
- `OTEL_EXPORTER_OTLP_ENDPOINT=http://platform.platform.svc.cluster.local:8080`
- `OTEL_SERVICE_NAME={project_name}`
- `OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer {token}` — scoped bearer token for OTLP ingest

**OTLP auto-token**: Auto-created per project on first deploy, stored in `api_tokens` with:
- `scopes: ["otlp:write"]` (new minimal scope — NOT `project:read` or `project:write`)
- `project_id` set (project-scoped hard boundary)
- `expires_at`: 365 days (with rotation support — new token created on deploy if <30 days remain)
- The token is stored in the K8s Secret, never logged.

The coding agent's prompt includes: "Apps should use OpenTelemetry SDK. OTLP endpoint, service name, and auth token are injected as env vars automatically."

### Tests to write FIRST (before implementation)

**Integration tests — `tests/observe_ingest_integration.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_otlp_ingest_rejects_unauthorized_project` | User without ProjectRead for project_id → 403 | Integration |
| `test_otlp_ingest_accepts_authorized_project` | User with ProjectRead → data accepted | Integration |
| `test_otlp_ingest_missing_project_id_rejected` | OTLP data without `project_id` resource attr → 400 | Integration |
| `test_otlp_ingest_caches_permission_per_request` | Multiple spans, same project → 1 permission check | Integration |

**Integration tests — `tests/deployment_integration.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_deploy_injects_otel_env_vars` | K8s Secret includes `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_SERVICE_NAME`, scoped token | Integration |
| `test_deploy_creates_scoped_otlp_token` | API token with `["otlp:write"]` scope and project_id | Integration |

Total: **0 unit + 6 integration + 0 E2E = 6 tests**

#### Existing tests to UPDATE

| Test file | Change | Reason |
|---|---|---|
| `tests/observe_ingest_integration.rs` | Add `project_id` resource attr to payloads | Permission check now required |
| `tests/observe_integration.rs` | Ensure test users have ProjectRead | Permission enforcement |

#### Branch coverage checklist

| Branch/Path | Test that covers it |
|---|---|
| OTLP with valid permission | `test_otlp_ingest_accepts_authorized_project` |
| OTLP without permission → 403 | `test_otlp_ingest_rejects_unauthorized_project` |
| Missing project_id attr → 400 | `test_otlp_ingest_missing_project_id_rejected` |
| OTEL env vars in K8s Secret | `test_deploy_injects_otel_env_vars` |
| OTLP token auto-creation | `test_deploy_creates_scoped_otlp_token` |

---

## Phase 6: Ops/Incident Agents (Week 6)

**Why**: Closing the loop — when things break in production, the platform investigates automatically.

### 6A. Alert-triggered agent spawn

**New event**: `AlertFired { alert_id, project_id, severity, message, query_result }` in `src/store/eventbus.rs`.

**Alert evaluation** (`src/observe/alert.rs`): When an alert fires, publish `AlertFired` event.

**Rate limiting / circuit breaker** (CRITICAL — prevents alert storm → agent storm):
1. **Per-project cooldown**: Check Valkey key `alert-agent:{project_id}:{alert_id}` with 15-minute TTL. If set, skip spawn and log warning.
2. **Per-project concurrent limit**: Max 3 active ops agent sessions per project. Check `agent_sessions` table before spawn.
3. **Deduplication**: Same alert firing repeatedly uses the same Valkey key, so only one agent per alert per 15 minutes.

**New handler** in `eventbus.rs`: On `AlertFired`:
```rust
// 1. Check cooldown
let cooldown_key = format!("alert-agent:{}:{}", alert.project_id, alert.alert_id);
if state.valkey.next().exists(&cooldown_key).await? { return Ok(()); }

// 2. Check concurrent limit
let active_ops = sqlx::query_scalar!("SELECT COUNT(*) ...").await?;
if active_ops >= 3 { tracing::warn!("ops agent limit reached"); return Ok(()); }

// 3. Spawn and set cooldown
state.valkey.next().set(&cooldown_key, "1", Some(Expiration::EX(900)), None, false).await?;
agent::service::create_session(CreateSessionParams {
    project_id,
    role: AgentRoleName::Ops,
    prompt: format!(
        "Alert '{}' fired (severity: {}). Message: {}. \
         Investigate: query logs/traces, check recent deploys, \
         review recent commits. Create an issue with findings.",
        alert.name, alert.severity, alert.message
    ),
    ...
})
```

The `agent-ops` role already exists with permissions for observability reads + issue creation. **Audit the role** to ensure it does NOT have `project:write` or `secret:read`.

### 6B. Ops agent capabilities

The ops agent spawns with MCP servers:
- `platform-observe.js` — query logs, traces, metrics
- `platform-issues.js` — create issues with findings
- `platform-core.js` — read project info, recent deployments

Its system prompt instructs it to:
1. Query recent error logs for the project
2. Check trace latency spikes
3. Review deployment history (was there a recent deploy?)
4. Check recent git commits
5. Create an issue with diagnosis and proposed remediation

### Tests to write FIRST (before implementation)

**Unit tests — `src/store/eventbus.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_alert_fired_event_serialization` | `AlertFired` serializes/deserializes correctly | Unit |
| `test_alert_fired_event_round_trip` | Serialize → deserialize = identical | Unit |

**Unit tests — `src/observe/alert.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_alert_evaluation_publishes_event` | Alert condition met → `AlertFired` event constructed correctly | Unit |

**Integration tests — `tests/eventbus_integration.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_alert_fired_triggers_agent_session` | `AlertFired` handler calls `create_session` with ops role | Integration |
| `test_alert_fired_session_has_ops_role` | Auto-spawned session has `AgentRoleName::Ops` | Integration |
| `test_alert_fired_prompt_includes_alert_info` | Session prompt contains alert name, severity, message | Integration |
| `test_alert_fired_missing_project_skipped` | AlertFired without valid project → logged, skipped | Integration |
| `test_alert_fired_cooldown_prevents_duplicate` | Same alert within 15 min → second spawn skipped | Integration |
| `test_alert_fired_concurrent_limit` | >3 active ops agents → spawn skipped | Integration |

**Integration tests — `tests/alert_eval_integration.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_alert_fires_publishes_event` | Alert eval trigger → `AlertFired` published | Integration |

**E2E tests — `tests/e2e_agent.rs`**

| Test | Validates | Layer |
|---|---|---|
| `test_alert_spawns_ops_agent` | Fire alert → ops agent session created with correct MCP config | E2E |

Total: **3 unit + 7 integration + 1 E2E = 11 tests**

#### Branch coverage checklist

| Branch/Path | Test that covers it |
|---|---|
| AlertFired serialization | `test_alert_fired_event_serialization` |
| Handler → create_session | `test_alert_fired_triggers_agent_session` |
| Session role = Ops | `test_alert_fired_session_has_ops_role` |
| Prompt includes alert details | `test_alert_fired_prompt_includes_alert_info` |
| Missing project → skip | `test_alert_fired_missing_project_skipped` |
| Cooldown hit → skip | `test_alert_fired_cooldown_prevents_duplicate` |
| Concurrent limit hit → skip | `test_alert_fired_concurrent_limit` |
| Full cycle: alert → agent | `test_alert_spawns_ops_agent` |

#### Tests NOT needed

- **Ops agent creating issue**: Requires Claude API call — not automatable. Verified by manual E2E.
- **Ops agent system prompt**: String constant, not testable logic.
- **MCP config for ops agent**: Determined by `AgentRoleName::Ops`, already tested in agent identity tests.

---

## Migration Summary

| Phase | Migration | Changes |
|-------|-----------|---------|
| 2 | `20260227010001_project_namespace` | `projects.namespace_slug TEXT` (3-step: add nullable, backfill, set NOT NULL) + UNIQUE partial index + `ops_repos.project_id UUID FK` + unique index |
| 3 | `20260228010001_tracked_resources` | `deployments.tracked_resources JSONB DEFAULT '[]'` |

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

**Phase 5**: `src/observe/ingest.rs`, `src/deployer/reconciler.rs`

**Phase 6**: `src/store/eventbus.rs`, `src/observe/alert.rs`

**All phases**: `ui/src/lib/generated/*.ts` (regenerate via `just types`), `.sqlx/` (regenerate via `just db-prepare`)

---

## Verification

After each phase, run the full test suite:
```bash
just ci-full    # fmt + lint + deny + test-unit + test-integration + test-e2e + build
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

Every new or modified line of code must be covered by at least one test (unit, integration, or E2E). The test strategy above maps each code path to a specific test. `review` and `finalize` will verify with `just cov-unit` / `just cov-total`.

### New test counts by phase

| Phase | Unit | Integration | E2E | Total |
|---|---|---|---|---|
| Phase 1: Fix the Broken Core | 17 | 2 | 2 | 21 |
| Phase 2: Per-Project Namespaces | 10 | 7 | 3 | 20 |
| Phase 3: Deploy + Resource Cascade | 14 | 5 | 2 | 21 |
| Phase 4: Dev Images + Secrets | 6 | 12 | 2 | 20 |
| Phase 5: Scoped Observability | 0 | 6 | 0 | 6 |
| Phase 6: Ops/Incident Agents | 3 | 7 | 1 | 11 |
| **Total** | **50** | **39** | **10** | **99** |

**Existing tests to update**: ~25 tests across 12 files (updated assertions, new config fields, changed namespace expectations).

**Testing pyramid**: 51% unit, 39% integration, 10% E2E — consistent with project's existing ratio.

### Coverage goals by module

| Module | Current tests | New tests added |
|---|---|---|
| `src/agent/claude_code/pod.rs` | ~10 unit | +9 unit (SecurityContext, HTTP clone) |
| `src/agent/service.rs` | ~5 unit | +2 unit (HTTP URL, secret injection) |
| `src/pipeline/executor.rs` | ~8 unit | +5 unit (HTTP clone, SecurityContext) |
| `src/pipeline/trigger.rs` | ~4 unit | +4 unit (Dockerfile.dev detection) |
| `src/deployer/namespace.rs` | 0 (new) | +10 unit |
| `src/deployer/applier.rs` | ~6 unit | +8 unit (resource tracking, cascade) |
| `src/deployer/ops_repo.rs` | ~4 unit | +3 unit (sync from project repo) |
| `src/deployer/reconciler.rs` | ~3 unit | +3 unit (namespace routing) |
| `src/store/eventbus.rs` | ~4 unit | +3 unit (new event types) |
| `src/observe/ingest.rs` | ~2 integration | +4 integration (OTLP auth) |
| `src/api/projects.rs` | ~12 integration | +5 integration (namespace, ops repo) |

---

## Plan Review Findings

**Date:** 2026-02-26
**Status:** APPROVED WITH CONCERNS

### Codebase Reality Check

Issues found and **corrected in-place above**:

1. **Migration would fail on existing rows** — `namespace_slug NOT NULL` without DEFAULT rejects when `projects` has data. Fixed: 3-step migration (add nullable → backfill → set NOT NULL).

2. **Token in clone URL leaks to logs/pod spec** — plan offered inline URL credentials as an option. Fixed: mandated `GIT_ASKPASS` approach only.

3. **`slugify_branch()` truncates at 63, not 40** — plan said to reuse it but needed 40-char namespace slugs. Fixed: new `slugify_namespace()` function.

4. **No UNIQUE constraint on namespace_slug** — two projects could collide on same K8s namespace. Fixed: added partial unique index.

5. **`ops_repos` has no `project_id` column** — plan assumed 1:1 linkage but table had no FK. Fixed: added `project_id UUID FK` in migration.

6. **`create_project` would exceed 100 lines** — adding namespace + ops repo + NetworkPolicy logic pushes handler past clippy threshold. Fixed: extracted `setup_project_infrastructure()` helper.

7. **Global `ApiError` catches 23505 before handler** — plan's constraint-specific catch needs to be in the handler, not rely on global conversion. Fixed: explicit match in handler.

8. **`"NetworkPolicy"` missing from `kind_to_plural()`** — applier would fail with wrong plural. Fixed: noted in Phase 2A.

9. **NetworkPolicy missing CIDRs** — 100.64.0.0/10 (CGNAT), 169.254.0.0/16 (link-local) were absent. Also missing ingress deny-all and DNS selector too broad. Fixed: complete CIDR list + ingress + kube-system selector.

10. **Alert-triggered spawn had no rate limiting** — alert storm could spawn unlimited agents. Fixed: per-project cooldown + concurrent limit.

11. **Secret request storage unspecified** — no table or mechanism for pending requests. Fixed: in-memory `DashMap` with 5-minute timeout.

12. **OTLP auto-token scope undefined** — plan said "long-lived, project scope" but no specific scope. Fixed: `["otlp:write"]` minimal scope with 365-day expiry + rotation.

### Remaining Concerns

1. **Event bus reliability**: Valkey pub/sub loses events if subscriber is down. Phase 3 (deploy/ sync on ImageBuilt) is deployment-critical. Consider adding a DB-based pending-sync flag for idempotent recovery on restart.

2. **Reaper scalability**: Phase 2B changes the reaper from 1 namespace scan to N namespace scans (one per active project). For 50+ projects, this means 50+ K8s API calls per cycle. Monitor and consider batch label selector queries.

3. **OTLP auth breaks existing apps**: Phase 5A adds permission checks to OTLP ingest. Existing apps sending data without project-scoped tokens will get 403s. Need a migration path: create OTLP tokens for existing projects before enabling, or add a grace period env flag.

4. **Kind cluster CNI**: NetworkPolicies require Calico/Cilium. Kind's kindnet doesn't enforce them. This is acceptable for dev but must be documented for production.

5. **In-process agent tool removal**: Phase 2D removes 3 tools from the create-app flow. Active sessions during deploy will error on removed tool calls. Sessions are ephemeral so impact is minimal, but consider a deprecation period.

### Simplification Opportunities

1. **Async project infrastructure**: Instead of blocking `create_project` on K8s namespace creation, consider publishing a `ProjectCreated` event and handling infrastructure setup asynchronously. This keeps the API fast and makes retry natural. Trade-off: project isn't "ready" immediately.

2. **Phase 3E (Agent self-testing)**: This is purely a prompt/workflow change, not code. It could be moved to a separate "agent prompt improvements" doc rather than being a numbered sub-phase.

### Security Notes (for dev to keep in mind)

1. **Never log decrypted secret values** — even in error traces. The current code is safe but new decrypt paths in deployer/agent must maintain this.

2. **Agent git push scope** — agents currently have `ProjectWrite` which allows pushing to any branch including `main`. Consider restricting agent tokens to `agent/*` branches in a future plan.

3. **`ask_for_secret` anti-phishing** — the UI modal must clearly show the requesting project and agent session. A compromised agent could call `ask_for_secret("SSH_KEY", "Your SSH key for deployment")` to social-engineer users.

4. **`agent-ops` role audit** — verify the role only has observability read + issue create permissions. Document the exact permission set.
