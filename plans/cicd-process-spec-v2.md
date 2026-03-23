# CI/CD Pipeline — Full Process Specification (v2)

Complete lifecycle of a code change: MR creation → build → test → merge → deploy staging → promote production.

Legend: **[EXISTS]** = implemented, **[NEW]** = needs building, **[FIX]** = exists but broken/incomplete

---

## Overview: Two Pipelines, Two Repos

```
CODE REPO (project git)              OPS REPO (deploy git)
  feature/shop-app                      staging branch
  └─ .platform.yaml                     └─ deploy/          (from code repo)
  └─ app/                               └─ platform.yaml    (from code repo)
  └─ deploy/                            └─ values/staging.yaml (auto-generated)
  │   └─ deployment-stable.yaml         main branch (production)
  │   └─ deployment-canary.yaml         └─ deploy/
  │   └─ postgres.yaml                  └─ platform.yaml
  │   └─ service-stable.yaml            └─ values/production.yaml
  │   └─ variables_staging.yaml
  │   └─ variables_prod.yaml
  └─ testinfra/
  └─ Dockerfile
  └─ Dockerfile.canary
  └─ Dockerfile.dev
  └─ Dockerfile.test
```

**Build Pipeline** — triggered by code repo events (MR open, push to main)
**Deploy Pipeline** — triggered by ops repo updates (push to staging, merge to main)

### Environment Variables Files

Developers specify per-environment values (resource limits, feature toggles, DB connection strings, etc.) in the code repo under `deploy/`:

- `deploy/variables_staging.yaml` — staging-specific values
- `deploy/variables_prod.yaml` — production-specific values

These are **merged into** `values/{environment}.yaml` in the ops repo during gitops_sync, alongside the auto-generated image refs. The renderer uses them as minijinja template context when rendering deploy manifests.

---

## Phase 1: MR Creation → Build Pipeline Trigger

### Trigger Mechanism **[EXISTS]**

Pipeline is triggered via **direct function call** (not pubsub event):

```
create_demo_project()
  → git commit on feature/shop-app
  → INSERT merge_requests row
  → trigger_mr_pipeline() [direct call]
    → pipeline::trigger::on_mr()
      → read .platform.yaml from repo at SHA
      → parse + validate pipeline definition
      → match trigger: mr.actions contains "opened" ✓
      → INSERT pipelines + pipeline_steps rows
    → notify_executor()
      → state.pipeline_notify.notify_one()  (tokio Notify, immediate wake)
      → valkey publish "pipeline:run"        (redundant delivery)
```

The API handler path is identical: `create_mr()` → `run_mr_create_side_effects()` → `spawn_mr_pipeline_trigger()` → `pipeline::trigger::on_mr()`.

### Schema Validation **[FIX]**

Currently validation is structural (serde parse + DAG cycle check). Missing:
- That referenced Dockerfiles exist in the repo at that SHA
- That `deploy_test.manifests` directory exists
- That `deploy.specs[].canary.stable_service` / `canary_service` are present in `deploy/` manifests
- That `flags[].key` follows naming conventions
- That `deploy/` directory exists when `deploy` config is present

---

## Phase 2: Build Pipeline Execution

### New Step Type: `imagebuild` **[NEW]**

Replaces raw kaniko step declarations. Platform manages registry URL, image path, push credentials, insecure flags. Developer only specifies what matters:

```yaml
pipeline:
  steps:
    - name: build-app
      type: imagebuild
      imageName: app                    # → $REGISTRY/$PROJECT/app:$COMMIT_SHA
      dockerfile: Dockerfile            # optional, defaults to Dockerfile
      secrets:                          # optional: injected as kaniko --build-arg
        - SECRET_NEEDED_DURING_BUILD

    - name: build-canary
      type: imagebuild
      imageName: canary
      dockerfile: Dockerfile.canary

    - name: build-dev
      type: imagebuild
      imageName: dev
      dockerfile: Dockerfile.dev

    - name: build-test
      type: imagebuild
      imageName: test
      dockerfile: Dockerfile.test
      only:
        events: [mr]
```

**Platform auto-generates the kaniko command:**
```
/kaniko/executor
  --context=dir:///workspace
  --dockerfile={dockerfile}
  --destination={REGISTRY}/{PROJECT}/{imageName}:{COMMIT_SHA}
  --insecure --insecure-registry={REGISTRY}
  --cache=true --cache-repo={REGISTRY}/{PROJECT}/cache
  --build-arg=SECRET_NEEDED_DURING_BUILD={decrypted_value}  ← from project secrets
```

This replaces:
- Raw kaniko steps where agent could accidentally delete `$REGISTRY` or `$PROJECT` vars
- The magic `build-dev-image` auto-injection (now it's an explicit `imagebuild` step)

### Pipeline Steps (from target demo `.platform.yaml`)

| Step | Type | Trigger | Depends On | Purpose |
|------|------|---------|------------|---------|
| `build-app` | imagebuild | push, mr | — | Production image |
| `build-canary` | imagebuild | push, mr | — | Canary image |
| `build-dev` | imagebuild | push, mr | — | Agent dev environment |
| `build-test` | imagebuild | mr only | — | Test runner image |
| `e2e` | deploy_test | mr only | build-app, build-test | Deploy testinfra + run tests |
| `sync-ops-repo` | gitops_sync | push only, main | build-app, build-canary | Copy deploy/ to ops repo |
| `watch-deploy` | deploy_watch | push only, main | sync-ops-repo | Wait for staging deploy |

### Per-Pipeline Namespace **[NEW]**

Currently all pipelines share `{namespace_slug}-dev`. Change to:

```
{namespace_slug}-pipeline-{pipeline_id[:8]}
```

Each pipeline run gets its own isolated namespace. Cleaned up after pipeline completes.

Benefits:
- No shared state between concurrent pipeline runs
- Clean separation of pipeline artifacts
- Easier debugging (inspect specific pipeline's pods)

### Step Execution Flow (per step)

```
execute_step_dispatch(step)
  │
  ├─ [NEW]  Create pipeline namespace: {namespace_slug}-pipeline-{pipeline_id[:8]}
  │           └─ (replaces shared {namespace_slug}-dev)
  │           └─ NetworkPolicy: egress to platform API + DNS + internet
  │
  ├─ [EXISTS] Init container: git clone → /workspace
  │
  ├─ For type=imagebuild:
  │   ├─ [NEW] Resolve secrets listed in step.secrets from project secrets (scope: pipeline/all)
  │   ├─ [NEW] Generate kaniko command with --build-arg for each secret
  │   ├─ [EXISTS] Mount /kaniko/.docker for push credentials
  │   └─ [EXISTS] All standard env vars injected (PLATFORM_*, COMMIT_*, OTEL_*, etc.)
  │
  ├─ For type=deploy_test:
  │   ├─ [EXISTS] Create test namespace: {namespace_slug}-test-{pipeline_id[:8]}
  │   ├─ [NEW]  Inject project secrets (scope: test/all) into test namespace
  │   ├─ [NEW]  Create OTEL tokens for test namespace, inject as env vars
  │   ├─ [EXISTS] Apply testinfra manifests, wait for readiness
  │   ├─ [EXISTS] Spawn test pod
  │   └─ [EXISTS] Cleanup test namespace
  │
  ├─ [EXISTS] Poll pod status (3s interval, 900s timeout)
  ├─ [EXISTS] Capture logs → MinIO
  └─ [EXISTS] Update step status
```

### Updated Secret Scopes **[NEW]**

Replace current 4 scopes with 6 scopes. No backwards compatibility needed.

| Scope | Used By | Fallback |
|-------|---------|----------|
| `all` | Everything | — |
| `pipeline` | Build steps (imagebuild, regular) | + `all` |
| `agent` | Agent sessions | + `all` |
| `test` | Deploy-test steps | + `all` |
| `staging` | Staging deployments | + `all` |
| `prod` | Production deployments | + `all` |

DB migration: `ALTER TABLE secrets DROP CONSTRAINT ...; ADD CONSTRAINT scope CHECK (scope IN ('all', 'pipeline', 'agent', 'test', 'staging', 'prod'))`.

Query pattern: `WHERE scope IN ('{specific_scope}', 'all')`.

---

## Phase 3: Pipeline Completion → Finalize

```
finalize_pipeline(pipeline_id, all_succeeded)
  │
  ├─ [EXISTS] Set status = success/failure
  │
  ├─ If success + MR trigger:
  │   └─ [EXISTS] try_auto_merge() → check eligible MRs
  │
  ├─ If success + push trigger + main branch:
  │   └─ (gitops_sync and deploy_watch are now explicit steps,
  │       so finalize just updates status — no more hidden logic)
  │
  ├─ [EXISTS] fire_build_webhook()
  ├─ [EXISTS] emit_pipeline_log()
  │
  └─ [NEW] Cleanup pipeline namespace: {namespace_slug}-pipeline-{pipeline_id[:8]}
```

Note: `detect_and_write_deployment()` and `detect_and_publish_dev_image()` are **removed** from finalize — replaced by explicit `gitops_sync` and `imagebuild` steps.

---

## Phase 4: MR Auto-Merge → Main Branch Pipeline

### Auto-Merge **[FIX]**

Demo project must set `auto_merge=true` on MR creation:

```rust
// In demo_project.rs create_demo_mr_and_pipeline():
INSERT INTO merge_requests (..., auto_merge, auto_merge_by, auto_merge_method)
VALUES (..., true, owner_id, 'merge')
```

After MR pipeline succeeds → `try_auto_merge()` fires → gates pass → `do_merge()`.

### Post-Merge Pipeline Trigger **[FIX]**

`do_merge()` creates merge commit directly in bare repo via worktree — git HTTP hooks never fire. Fix: after merge, explicitly trigger push pipeline:

```rust
// In do_merge() after successful merge:
pipeline::trigger::on_push(&state.pool, &PushTriggerParams {
    project_id,
    user_id: auth.user_id,
    repo_path,
    branch: "main".into(),
    commit_sha: merge_commit_sha,
}).await;
```

This triggers the main-branch pipeline which runs:
- `build-app` ✓ (push + main)
- `build-canary` ✓ (push + main)
- `build-dev` ✓ (push + main)
- `build-test` ✗ SKIPPED (mr-only)
- `e2e` ✗ SKIPPED (mr-only)
- `sync-ops-repo` ✓ (push + main) — copies deploy/ to ops repo
- `watch-deploy` ✓ (push + main) — watches staging deploy

---

## Phase 5: New Step Type: `gitops_sync` **[NEW]**

Replaces internal `gitops_handoff()` logic with an explicit pipeline step.

```yaml
- name: sync-ops-repo
  type: gitops_sync
  depends_on: [build-app, build-canary]
  only:
    events: [push]
    branches: ["main"]
  gitops:
    copy: [deploy/, .platform.yaml]    # files/dirs to copy from code repo to ops repo
```

### Execution Flow

```
execute_gitops_sync_step(step)
  │
  ├─ Look up ops repo for project
  │
  ├─ Read .platform.yaml from project repo at commit_sha
  │
  ├─ Parse deploy config (enable_staging, specs, flags)
  │
  ├─ Copy files listed in gitops.copy from project repo → ops repo
  │   └─ deploy/          → ops repo deploy/
  │   └─ .platform.yaml   → ops repo platform.yaml
  │
  ├─ Copy variables files from code repo → ops repo values/
  │   └─ deploy/variables_staging.yaml → ops repo values/staging.yaml (merged with auto-generated)
  │   └─ deploy/variables_prod.yaml    → ops repo values/production.yaml (merged)
  │
  ├─ Determine target branch (from project DB setting, NOT platform.yaml):
  │   └─ project.include_staging=true → staging branch
  │   └─ project.include_staging=false → main branch (direct to production)
  │
  ├─ Build + merge values JSON:
  │   {
  │     "image_ref": "$REGISTRY/$PROJECT/app:$COMMIT_SHA",
  │     "canary_image_ref": "$REGISTRY/$PROJECT/canary:$COMMIT_SHA",
  │     "project_name": "platform-demo",
  │     "environment": "staging",
  │     ... user variables from variables_staging.yaml ...
  │   }
  │
  ├─ Commit to ops repo on target branch
  │
  ├─ Publish OpsRepoUpdated event
  │
  └─ Register feature flags from platform.yaml
      └─ INSERT new flags (ON CONFLICT DO NOTHING)
      └─ Delete flags NOT in current + previous commit's platform.yaml
```

### Step status: success if ops repo commit succeeds, failure otherwise.

---

## Phase 6: New Step Type: `deploy_watch` **[NEW]**

Watches the deploy pipeline (triggered by ops repo update) and reports result back to the build pipeline.

```yaml
- name: watch-deploy
  type: deploy_watch
  depends_on: [sync-ops-repo]
  only:
    events: [push]
    branches: ["main"]
  deploy_watch:
    environment: staging       # which environment to watch
    timeout: 300               # seconds
```

### Execution Flow

```
execute_deploy_watch_step(step)
  │
  ├─ Find the latest deploy_releases row for project + environment
  │   (created by handle_ops_repo_updated after gitops_sync published the event)
  │
  ├─ Poll deploy_releases.phase every 5 seconds:
  │   ├─ pending → wait (reconciler hasn't picked it up yet)
  │   ├─ progressing → wait (canary/rolling in progress)
  │   ├─ holding → wait (analysis still deciding)
  │   ├─ promoting → wait (promotion in progress)
  │   ├─ completed → SUCCESS (deploy finished, containers healthy)
  │   ├─ failed → FAILURE
  │   ├─ rolled_back → FAILURE (canary failed, rolled back)
  │   └─ timeout exceeded → FAILURE
  │
  └─ Write deploy result to step log
```

This step does NOT spawn a K8s pod. It runs inside the executor process, polling the DB. Status is written to `pipeline_steps` like any other step.

---

## Phase 7: Deploy Pipeline (OpsRepoUpdated → Reconciler)

### Event Handler **[EXISTS]**

```
OpsRepoUpdated event received (from gitops_sync step)
  │
  ├─ [EXISTS] Read .platform.yaml from ops repo
  ├─ [EXISTS] Extract strategy + rollout_config
  ├─ [EXISTS] Upsert deploy_targets
  ├─ [EXISTS] Create deploy_releases (phase=pending)
  ├─ [EXISTS] Register feature flags
  ├─ [NEW]  Feature flag pruning:
  │           Keep flags from current + previous release's platform.yaml
  │           Delete older flags for this project
  └─ [EXISTS] Wake reconciler
```

### Reconciler: handle_pending() **[EXISTS]**

```
handle_pending(release)
  │
  ├─ [EXISTS] Create namespace: {namespace_slug}-{env_suffix}
  │           e.g., platform-demo-staging, platform-demo-prod
  │
  ├─ [EXISTS] inject_project_secrets():
  │     Query secrets: scope IN ('{env_scope}', 'all')
  │     ┌─────────────────────────────────────────────────────────────┐
  │     │ User secrets:                                               │
  │     │   DATABASE_URL, VALKEY_URL, APP_SECRET_KEY, SENTRY_DSN      │
  │     │                                                             │
  │     │ OTEL (auto-injected):                                       │
  │     │   OTEL_EXPORTER_OTLP_ENDPOINT, OTEL_SERVICE_NAME,           │
  │     │   OTEL_RESOURCE_ATTRIBUTES, OTEL_EXPORTER_OTLP_HEADERS      │
  │     │                                                             │
  │     │ Platform tokens (auto-created per-environment):              │
  │     │   PLATFORM_API_TOKEN (project:read), PLATFORM_API_URL,      │
  │     │   PLATFORM_PROJECT_ID                                       │
  │     └─────────────────────────────────────────────────────────────┘
  │
  ├─ [EXISTS] ensure_registry_pull_secret()
  │
  ├─ [EXISTS] render_manifests():
  │     Read from ops repo, load values/{env}.yaml (now includes user variables)
  │     Render via minijinja
  │
  ├─ [EXISTS] apply_manifests() → server-side apply
  │
  ├─ Strategy-specific:
  │   ├─ Rolling: wait for Deployment health → completed
  │   └─ Canary/AB: set initial traffic weight → progressing
  │       └─ apply_gateway_resources() → Envoy Gateway + HTTPRoute
  │
  └─ Staging → Production promotion:
      Staging is for teams who want manual QA before production.
      Promotion is triggered manually via POST /api/projects/{id}/promote-staging.
      Whether staging is used is controlled by a **project setting** `include_staging`
      (DB column on `projects` table, NOT in .platform.yaml — prevents dev agent
      from accidentally disabling it). The gitops_sync step reads this flag to
      decide target branch (staging vs main).
```

---

## Phase 8: Canary Progression **[EXISTS]**

```
Analysis loop (15s interval):
  ├─ Check rollback triggers → instant failure if breached
  ├─ Evaluate progress gates → pass/fail/inconclusive
  └─ Reconciler reads verdict:
      pass → advance step (10% → 25% → 50% → 100%)
      fail → rolling_back
      inconclusive → wait

Promotion (all steps passed):
  ├─ Route 100% to stable
  ├─ Re-render manifests (canary becomes stable)
  └─ completed, health=healthy

Rollback (gate failure):
  ├─ Route 0% to canary (100% to stable)
  └─ rolled_back, health=unhealthy
```

---

## Phase 9: Feature Flags **[EXISTS + FIX]**

### Registration (during deploy)
- Flags in `.platform.yaml` → registered via `handle_ops_repo_updated`
- `INSERT INTO feature_flags ON CONFLICT DO NOTHING`

### Pruning **[NEW]**
- On each deployment: read flags from current commit's `platform.yaml`
- Read flags from previous release's `platform.yaml` (for rollback compatibility)
- Delete all project flags NOT in either set

### Evaluation
```
POST /api/flags/evaluate
  Auth: Bearer <PLATFORM_API_TOKEN> (auto-injected to deployed apps)
  Body: { project_id, keys: ["new_checkout_flow", "dark_mode"] }
  → { values: { "new_checkout_flow": false, "dark_mode": false } }
```

---

## Target: Complete Auto-Triggered Flow

```
┌─ 1. create_demo_project()
│    Creates: project, repos, secrets, issues
│    Creates: feature branch + demo app (29 files)
│    Creates: MR with auto_merge=true
│
├─ 2. MR Pipeline (auto-triggered via on_mr)
│    ├─ build-app      (imagebuild) → registry/demo/app:SHA
│    ├─ build-canary   (imagebuild) → registry/demo/canary:SHA
│    ├─ build-dev      (imagebuild) → registry/demo/dev:SHA
│    ├─ build-test     (imagebuild, mr-only) → registry/demo/test:SHA
│    └─ e2e            (deploy_test, mr-only, depends: build-app+build-test)
│         └─ Deploys testinfra + runs tests with secrets + OTEL
│
├─ 3. Auto-Merge (try_auto_merge after pipeline success)
│    Pipeline succeeded ✓, auto_merge=true ✓
│    → do_merge() → merge to main
│    → trigger on_push(main, merge_sha)
│
├─ 4. Main Pipeline (auto-triggered via on_push)
│    ├─ build-app      (imagebuild) → fresh images
│    ├─ build-canary   (imagebuild)
│    ├─ build-dev      (imagebuild)
│    ├─ sync-ops-repo  (gitops_sync, push+main, depends: build-app+build-canary)
│    │    └─ Copy deploy/ + platform.yaml + variables to ops repo
│    │    └─ Commit values to staging branch
│    │    └─ Publish OpsRepoUpdated(staging)
│    │    └─ Register flags + prune old flags
│    └─ watch-deploy   (deploy_watch, push+main, depends: sync-ops-repo)
│         └─ Poll staging deploy_releases until completed/failed
│
├─ 5. Staging Deploy (reconciler, auto-triggered by OpsRepoUpdated)
│    ├─ Create namespace: platform-demo-staging
│    ├─ Inject secrets (scope: staging/all) + OTEL tokens
│    ├─ Registry pull secret
│    ├─ Render manifests with staging variables
│    ├─ Apply to K8s
│    └─ Canary progression: 10% → 25% → 50% → 100% → completed
│
├─ 6. Manual QA on staging
│    Team tests staging environment, then promotes manually:
│    POST /api/projects/{id}/promote-staging
│
├─ 7. Production Deploy (reconciler, triggered by promote-staging)
│    ├─ Create namespace: platform-demo-prod
│    ├─ Inject secrets (scope: prod/all) + OTEL tokens
│    ├─ Render manifests with prod variables
│    ├─ Apply to K8s
│    └─ Canary progression → completed
│
└─ 8. Feature Flags evaluable
     Apps call /api/flags/evaluate with auto-injected PLATFORM_API_TOKEN
```

---

## Target: Demo `.platform.yaml`

```yaml
pipeline:
  on:
    push:
      branches: ["main"]
    mr:
      actions: [opened, synchronized]

  steps:
    - name: build-app
      type: imagebuild
      imageName: app
      dockerfile: Dockerfile

    - name: build-canary
      type: imagebuild
      imageName: canary
      dockerfile: Dockerfile.canary

    - name: build-dev
      type: imagebuild
      imageName: dev
      dockerfile: Dockerfile.dev

    - name: build-test
      type: imagebuild
      imageName: test
      dockerfile: Dockerfile.test
      only:
        events: [mr]

    - name: e2e
      depends_on: [build-app, build-test]
      only:
        events: [mr]
      deploy_test:
        test_image: $REGISTRY/$PROJECT/test:$COMMIT_SHA
        manifests: testinfra/
        readiness_timeout: 120
        wait_for_services: [platform-demo-app, platform-demo-db]

    - name: sync-ops-repo
      type: gitops_sync
      depends_on: [build-app, build-canary]
      only:
        events: [push]
        branches: ["main"]
      gitops:
        copy: [deploy/, .platform.yaml]

    - name: watch-deploy
      type: deploy_watch
      depends_on: [sync-ops-repo]
      only:
        events: [push]
        branches: ["main"]
      deploy_watch:
        environment: staging
        timeout: 300

flags:
  - key: new_checkout_flow
    default_value: false
    description: "Enable the new checkout experience"
  - key: dark_mode
    default_value: false
    description: "Enable dark mode theme"

deploy:
  # NOTE: enable_staging is NOT here — it's a project DB setting (include_staging)
  # so the dev agent can't accidentally disable it via platform.yaml changes.
  variables:
    staging: deploy/variables_staging.yaml
    production: deploy/variables_prod.yaml
  specs:
    - name: api
      type: canary
      canary:
        stable_service: platform-demo-app-stable
        canary_service: platform-demo-app-canary
        steps: [10, 25, 50, 100]
        interval: 120
        progress_gates:
          - metric: error_rate
            condition: lt
            threshold: 0.05
```

---

## Implementation Work Items

### Tier 1: Critical path (enables the full flow)

| # | Item | Files | Status | Description |
|---|------|-------|--------|-------------|
| 1 | `imagebuild` step type | `definition.rs`, `executor.rs`, `trigger.rs` | **DONE** | Parse `type: imagebuild`, generate kaniko command, inject secrets as `--build-arg` |
| 2 | `gitops_sync` step type | `definition.rs`, `executor.rs` | **DONE** | Parse `type: gitops_sync`, execute ops repo sync + values commit + event publish |
| 3 | `deploy_watch` step type | `definition.rs`, `executor.rs` | **DONE** | Parse `type: deploy_watch`, poll deploy_releases until terminal |
| 4 | Demo MR `auto_merge=true` | `demo_project.rs` | **DONE** | Set auto_merge fields on MR insert |
| 5 | Post-merge pipeline trigger | `merge_requests.rs` | **DONE** | Call `on_push()` after `do_merge()` |
| 6 | Per-pipeline namespace | `executor.rs` | **DONE** | Change from `{slug}-dev` to `{slug}-pipeline-{id[:8]}` + cleanup |
| 7 | Remove magic dev image | `trigger.rs`, `executor.rs` | **DONE** | Remove `insert_dev_image_step()`, dev image is now explicit `imagebuild` step |
| 8 | Remove magic gitops_handoff | `executor.rs` | **DONE** | Remove from `finalize_pipeline()`, old functions marked `dead_code` for reference |

### Tier 2: Secret & deploy enhancements

| # | Item | Files | Description |
|---|------|-------|-------------|
| 9 | Secret scope migration | `migrations/` | **DONE** | Change CHECK to `(all, pipeline, agent, test, staging, prod)` |
| 10 | Test namespace secrets + OTEL | `executor.rs` | **DONE** | Inject secrets (scope: test/all) + OTEL tokens into deploy_test namespace |
| 11 | Variables files support | `executor.rs` | **DONE** | Read `deploy/variables_{env}.yaml` from code repo, merge into ops repo values JSON |
| 12 | `include_staging` project setting | `migrations/`, `projects` table, `executor.rs` | **DONE** | DB column on `projects` — gitops_sync reads it to decide target branch. Not in platform.yaml (dev agent can't override). |

### Tier 3: Polish

| # | Item | Files | Description |
|---|------|-------|-------------|
| 13 | Feature flag pruning | `eventbus.rs` | **DONE** | Prune flags not in current platform.yaml (preserves user-configured flags with rules/overrides) |
| 14 | Schema validation | `definition.rs` | DEFERRED | Check Dockerfiles/manifests exist at SHA (enhancement, not blocking) |
| 15 | Demo platform.yaml update | `templates/platform.yaml` | **DONE** | Rewrite with new step types (imagebuild, gitops_sync, deploy_watch) |
| 16 | Demo variables files | `templates/deploy/` | **DONE** | Add `variables_staging.yaml`, `variables_prod.yaml` |
| 17 | Branch idempotency fix | `demo_project.rs` | **DONE** | Prune worktrees + delete branch before creation |
| 18 | E2E lifecycle test | `tests/e2e_demo.rs` | **DONE** | Full lifecycle test: 12 stages, ~50 assertions, 2 explicit actions |

---

## E2E Lifecycle Test: `demo_full_lifecycle`

Single test that triggers the demo project and **observes** the entire auto-triggered flow end-to-end. The test makes exactly **two** explicit API calls: `create_demo_project()` (kicks off everything) and `POST /promote-staging` (manual QA gate). Everything else is observed via DB polling.

### Test Infrastructure Setup

```rust
#[ignore = "requires Kind cluster"]
#[sqlx::test(migrations = "./migrations")]
async fn demo_full_lifecycle(pool: PgPool) {
    // Real TCP server (pods need to reach us for git clone + registry + OTLP)
    let (state, token, _server, shutdown_tx) =
        e2e_helpers::start_observe_pipeline_server(pool.clone()).await;
    let app = e2e_helpers::observe_pipeline_test_router(state.clone(), ...);

    // Spawn ALL background tasks — the test observes, never drives
    let _executor = ExecutorGuard::spawn(&state);
    let _reconciler = ReconcilerGuard::spawn(&state);
    let _eventbus = EventBusGuard::spawn(&state);
    let _analysis = AnalysisGuard::spawn(&state);  // canary analysis loop

    let admin_id = admin_user_id(&pool).await;
```

### New Guards Needed in `e2e_helpers/mod.rs`

```rust
// ReconcilerGuard — same pattern as ExecutorGuard
struct ReconcilerGuard { shutdown_tx, handle }
impl ReconcilerGuard {
    fn spawn(state) -> Self {
        // tokio::spawn(platform::deployer::reconciler::run(state, shutdown_rx))
    }
}

// EventBusGuard — subscribes to Valkey pub/sub
struct EventBusGuard { shutdown_tx, _handle }
impl EventBusGuard {
    fn spawn(state) -> Self {
        // tokio::spawn(platform::store::eventbus::run(state, shutdown_rx))
    }
}

// AnalysisGuard — canary/AB metric analysis loop
struct AnalysisGuard { shutdown_tx, _handle }
impl AnalysisGuard {
    fn spawn(state) -> Self {
        // tokio::spawn(platform::deployer::analysis::run(state, shutdown_rx))
    }
}
```

### Stage 1: Demo Project Creation (~5s)

```
let (project_id, project_name) =
    platform::onboarding::demo_project::create_demo_project(&state, admin_id).await?;
```

**Assertions (all via DB queries):**

| # | Check | Query |
|---|-------|-------|
| 1.1 | Project exists, is_active=true | `SELECT is_active FROM projects WHERE id = $1` |
| 1.2 | project_name = "platform-demo" | from return value |
| 1.3 | namespace_slug set | `SELECT namespace_slug FROM projects WHERE id = $1` — non-empty |
| 1.4 | Ops repo exists | `SELECT id, repo_path FROM ops_repos WHERE project_id = $1` — row exists, path is a directory |
| 1.5 | Git bare repo exists | `SELECT repo_path FROM projects WHERE id = $1` — path exists on disk |
| 1.6 | Feature branch exists | `git rev-parse --verify refs/heads/feature/shop-app` in bare repo |
| 1.7 | MR exists | `SELECT number, source_branch, target_branch, status, auto_merge FROM merge_requests WHERE project_id = $1` |
| 1.8 | MR has auto_merge=true | from 1.7 — `auto_merge = true` |
| 1.9 | MR source_branch = feature/shop-app | from 1.7 |
| 1.10 | MR target_branch = main | from 1.7 |
| 1.11 | MR status = open | from 1.7 |
| 1.12 | 4 sample issues | `SELECT COUNT(*) FROM issues WHERE project_id = $1` = 4 |
| 1.13 | 6 sample secrets | `SELECT COUNT(*) FROM secrets WHERE project_id = $1` = 6 |
| 1.14 | demo_project_id setting | `SELECT value FROM platform_settings WHERE key = 'demo_project_id'` — exists |
| 1.15 | include_staging = true | `SELECT include_staging FROM projects WHERE id = $1` = true |
| 1.16 | MR pipeline triggered | `SELECT id, trigger, status FROM pipelines WHERE project_id = $1 ORDER BY created_at LIMIT 1` — trigger='mr', status IN ('pending','running') |

### Stage 2: MR Pipeline Execution (~60-300s)

```
// Wake executor (belt-and-suspenders, should already be awake from notify)
state.pipeline_notify.notify_one();

// Poll MR pipeline to terminal state
let (mr_pipeline_id, mr_status, mr_steps) =
    poll_project_pipeline(&app, &token, project_id, 300).await;
```

**Assertions:**

| # | Check | How |
|---|-------|-----|
| 2.1 | Pipeline trigger = 'mr' | `SELECT trigger FROM pipelines WHERE id = $1` |
| 2.2 | Pipeline status = 'success' (or at minimum all build steps attempted) | from poll result |
| 2.3 | build-app step exists, status != 'skipped' | step name + status from steps JSON |
| 2.4 | build-canary step exists, status != 'skipped' | same |
| 2.5 | build-dev step exists, status != 'skipped' | same (imagebuild, not magic) |
| 2.6 | build-test step exists, status != 'skipped' | same (mr-only, should run since trigger=mr) |
| 2.7 | e2e step exists | same (deploy_test) |
| 2.8 | If build-app + build-test succeeded → e2e status != 'skipped' | DAG check |
| 2.9 | All steps terminal | no step in pending/running state |
| 2.10 | Pipeline namespace created | `{namespace_slug}-pipeline-{pipeline_id[:8]}` — K8s namespace exists (or existed) |
| 2.11 | OTEL pipeline token created | `SELECT COUNT(*) FROM api_tokens WHERE project_id = $1 AND scopes @> ARRAY['observe:write'] AND name LIKE 'otlp-pipeline-%'` ≥ 1 |
| 2.12 | Pipeline steps have log_ref | `SELECT log_ref FROM pipeline_steps WHERE pipeline_id = $1 AND status IN ('success','failure')` — non-null |
| 2.13 | sync-ops-repo step SKIPPED | only: events: [push], trigger=mr → skipped |
| 2.14 | watch-deploy step SKIPPED | only: events: [push], trigger=mr → skipped |

### Stage 3: Auto-Merge (~10s)

After MR pipeline succeeds, `try_auto_merge()` fires automatically (called from `finalize_pipeline`). Wait for MR status to change.

```
// Poll MR status until merged (auto-merge fires from finalize_pipeline)
poll_until(30, || async {
    let row = sqlx::query("SELECT status FROM merge_requests WHERE project_id = $1 AND number = 1")
        .bind(project_id).fetch_one(&pool).await?;
    Ok(row.get::<String, _>("status") == "merged")
}).await;
```

**Assertions:**

| # | Check | Query |
|---|-------|-------|
| 3.1 | MR status = 'merged' | `SELECT status, merge_commit_sha FROM merge_requests WHERE project_id = $1 AND number = 1` |
| 3.2 | merge_commit_sha is set | from 3.1 — non-null |
| 3.3 | Main branch pipeline triggered | `SELECT id, trigger FROM pipelines WHERE project_id = $1 AND trigger = 'push' ORDER BY created_at DESC LIMIT 1` — exists |

### Stage 4: Main Branch Pipeline Execution (~60-300s)

```
// Find the push-triggered pipeline (created by post-merge on_push call)
let main_pipeline = poll_until(30, || async {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM pipelines WHERE project_id = $1 AND trigger = 'push' ORDER BY created_at DESC LIMIT 1"
    ).bind(project_id).fetch_optional(&pool).await
}).await;

// Poll it to completion
let main_status = e2e_helpers::poll_pipeline_status(&app, &token, project_id, &main_pipeline.to_string(), 300).await;
```

**Assertions:**

| # | Check | How |
|---|-------|-----|
| 4.1 | Pipeline trigger = 'push' | from DB |
| 4.2 | Pipeline git_ref contains 'main' | `SELECT git_ref FROM pipelines WHERE id = $1` |
| 4.3 | build-app ran (success/failure) | step status != 'skipped' |
| 4.4 | build-canary ran | same |
| 4.5 | build-dev ran | same |
| 4.6 | build-test SKIPPED | only: events: [mr], trigger=push → skipped |
| 4.7 | e2e SKIPPED | only: events: [mr] → skipped |
| 4.8 | sync-ops-repo ran | only: events: [push], branches: [main] → matches |
| 4.9 | watch-deploy ran | same |
| 4.10 | All steps terminal | no pending/running |

### Stage 5: GitOps Sync Verification (~1s after stage 4)

After sync-ops-repo step succeeded, verify ops repo state.

**Assertions:**

| # | Check | How |
|---|-------|-------|
| 5.1 | Ops repo has staging branch | `git rev-parse --verify refs/heads/staging` in ops repo path |
| 5.2 | Ops repo deploy/ directory exists on staging | `git show staging:deploy/` succeeds |
| 5.3 | Ops repo platform.yaml exists on staging | `git show staging:platform.yaml` succeeds |
| 5.4 | Ops repo values/staging.yaml exists | `git show staging:values/staging.yaml` succeeds |
| 5.5 | values/staging.yaml contains image_ref | parse YAML, check `image_ref` key present |
| 5.6 | values/staging.yaml contains canary_image_ref | same |
| 5.7 | OpsRepoUpdated event was handled | `SELECT id FROM deploy_releases WHERE project_id = $1 AND phase != 'pending' OR created_at > {stage4_start}` — exists |

### Stage 6: Staging Deploy Verification (~30-120s)

The eventbus handler creates a deploy_releases row when OpsRepoUpdated fires. The reconciler picks it up. The watch-deploy step polls it.

```
// Poll staging deploy_releases to a terminal phase
let staging_release = poll_until(180, || async {
    let row = sqlx::query(
        "SELECT dr.id, dr.phase, dr.health, dr.strategy
         FROM deploy_releases dr
         JOIN deploy_targets dt ON dr.target_id = dt.id
         WHERE dr.project_id = $1 AND dt.environment = 'staging'
         ORDER BY dr.created_at DESC LIMIT 1"
    ).bind(project_id).fetch_optional(&pool).await?;
    // completed or rolled_back or failed
    Ok(matches!(phase, "completed" | "rolled_back" | "failed"))
}).await;
```

**Assertions:**

| # | Check | How |
|---|-------|-------|
| 6.1 | deploy_targets row exists for staging | `SELECT id, environment, strategy FROM deploy_targets WHERE project_id = $1 AND environment = 'staging'` |
| 6.2 | deploy_releases row exists | from poll above |
| 6.3 | Strategy = 'canary' | from release row |
| 6.4 | Phase reached terminal (completed ideally) | from release row |
| 6.5 | Feature flags registered | `SELECT key FROM feature_flags WHERE project_id = $1` — contains 'new_checkout_flow' AND 'dark_mode' |
| 6.6 | K8s namespace exists: {ns_prefix}-platform-demo-staging | `kube::Api::<Namespace>::get()` |
| 6.7 | K8s Secret exists in staging ns | `kube::Api::<Secret>::get("{ns}-staging-secrets")` |
| 6.8 | Secret has OTEL_EXPORTER_OTLP_ENDPOINT | decode secret data, check key exists |
| 6.9 | Secret has OTEL_SERVICE_NAME = platform-demo | same |
| 6.10 | Secret has OTEL_EXPORTER_OTLP_HEADERS (Bearer token) | same |
| 6.11 | Secret has PLATFORM_API_TOKEN | same |
| 6.12 | Secret has PLATFORM_API_URL | same |
| 6.13 | Secret has PLATFORM_PROJECT_ID | same |
| 6.14 | Secret has DATABASE_URL (staging override) | same, value contains 'shop_staging' |
| 6.15 | OTEL token in DB | `SELECT id FROM api_tokens WHERE project_id = $1 AND name LIKE 'otlp-staging-%' AND scopes @> ARRAY['observe:write']` |
| 6.16 | API token in DB | `SELECT id FROM api_tokens WHERE project_id = $1 AND name LIKE 'api-staging-%' AND scopes @> ARRAY['project:read']` |
| 6.17 | Registry pull secret | `kube::Api::<Secret>::get("platform-registry-pull")` in staging ns |
| 6.18 | Release history entries exist | `SELECT COUNT(*) FROM release_history WHERE release_id = $1` ≥ 1 |

### Stage 7: OTEL Telemetry Round-Trip (~5s)

Extract the OTEL token from the staging K8s Secret and use it to send synthetic telemetry. This proves the auto-created token actually works for OTLP ingest.

```
// Extract OTEL token from K8s secret
let secrets_api: Api<Secret> = Api::namespaced(state.kube.clone(), &staging_ns);
let secret = secrets_api.get(&secret_name).await?;
let otel_header = decode_secret_key(&secret, "OTEL_EXPORTER_OTLP_HEADERS");
let otel_token = otel_header.strip_prefix("Authorization=Bearer ").unwrap();

// Send synthetic OTLP data using extracted token
post_protobuf(&app, otel_token, "/v1/traces", build_shop_trace(project_id)).await;
post_protobuf(&app, otel_token, "/v1/logs", build_shop_logs(project_id)).await;
post_protobuf(&app, otel_token, "/v1/metrics", build_shop_metrics(project_id)).await;

tokio::time::sleep(Duration::from_secs(3)).await;  // flush
```

**Assertions:**

| # | Check | How |
|---|-------|-------|
| 7.1 | Trace ingest returns 200 | status code |
| 7.2 | Log ingest returns 200 | same |
| 7.3 | Metric ingest returns 200 | same |
| 7.4 | Traces queryable | `GET /api/observe/traces?project_id={id}` — total ≥ 1 |
| 7.5 | Trace has ≥ 2 spans | trace detail — spans array length |
| 7.6 | Logs queryable | `GET /api/observe/logs?project_id={id}` — total ≥ 2 |
| 7.7 | Metric names include shop.* | `GET /api/observe/metrics/names?project_id={id}` — contains shop.product_views, shop.revenue_cents, etc. |

### Stage 8: Feature Flag Evaluation (~1s)

```
let (status, body) = post_json(&app, &token,
    "/api/flags/evaluate",
    json!({ "project_id": project_id, "keys": ["new_checkout_flow", "dark_mode"] })
).await;
```

**Assertions:**

| # | Check | How |
|---|-------|-------|
| 8.1 | Response 200 | status code |
| 8.2 | new_checkout_flow = false (default) | body.values.new_checkout_flow |
| 8.3 | dark_mode = false (default) | body.values.dark_mode |

### Stage 9: Promote Staging → Production (~1s for API call)

This is the **only manual action** the test takes beyond initial project creation.

```
let (status, body) = post_json(&app, &token,
    &format!("/api/projects/{project_id}/promote-staging"),
    json!({})
).await;
assert_eq!(status, StatusCode::OK);
```

**Assertions:**

| # | Check | How |
|---|-------|-------|
| 9.1 | Promote returns 200 | status code |
| 9.2 | Response has status=promoted | body.status |

### Stage 10: Production Deploy Verification (~30-120s)

After promote-staging publishes OpsRepoUpdated(production), the eventbus handler creates a production deploy_releases row. Reconciler processes it.

```
// Poll production deploy_releases to terminal
let prod_release = poll_until(180, || async {
    let row = sqlx::query(
        "SELECT dr.id, dr.phase, dr.health
         FROM deploy_releases dr
         JOIN deploy_targets dt ON dr.target_id = dt.id
         WHERE dr.project_id = $1 AND dt.environment = 'production'
         ORDER BY dr.created_at DESC LIMIT 1"
    ).bind(project_id).fetch_optional(&pool).await?;
    Ok(matches!(phase, "completed" | "rolled_back" | "failed"))
}).await;
```

**Assertions:**

| # | Check | How |
|---|-------|-------|
| 10.1 | deploy_targets row exists for production | DB query |
| 10.2 | deploy_releases row exists | from poll |
| 10.3 | Phase reached terminal | from release row |
| 10.4 | K8s namespace exists: {ns_prefix}-platform-demo-prod | kube API |
| 10.5 | K8s Secret exists in prod ns | `{ns}-prod-secrets` |
| 10.6 | Secret has DATABASE_URL (production override) | value contains 'shop_production' |
| 10.7 | Secret has OTEL_* env vars | decode + check keys |
| 10.8 | OTEL token: otlp-prod-* | DB query |
| 10.9 | API token: api-prod-* | DB query |
| 10.10 | Registry pull secret in prod ns | kube API |
| 10.11 | Ops repo main branch updated | `git show main:values/production.yaml` exists in ops repo |

### Stage 11: Production OTEL Round-Trip (~5s)

Same as Stage 7 but using production tokens.

```
// Extract prod OTEL token from K8s secret
let prod_secrets = secrets_api_prod.get(&prod_secret_name).await?;
let prod_otel_token = decode_secret_key(&prod_secrets, "OTEL_EXPORTER_OTLP_HEADERS");

// Send synthetic data with prod token
post_protobuf(&app, prod_otel_token, "/v1/traces", build_shop_trace(project_id)).await;
```

**Assertions:**

| # | Check | How |
|---|-------|-------|
| 11.1 | Trace ingest with prod token returns 200 | status code |
| 11.2 | Traces queryable for project | observe API |

### Stage 12: Cleanup Verification (~1s)

```
// Verify pipeline namespaces were cleaned up
// (per-pipeline ns should be deleted after pipeline completes)
```

**Assertions:**

| # | Check | How |
|---|-------|-------|
| 12.1 | MR pipeline namespace deleted | kube `get_opt` returns None for `{slug}-pipeline-{mr_pipeline_id[:8]}` |
| 12.2 | Main pipeline namespace deleted | same for main pipeline |
| 12.3 | Test namespace deleted | `{slug}-test-{mr_pipeline_id[:8]}` — cleaned up by deploy_test |

### Helper: `poll_until`

```rust
/// Poll a closure until it returns Ok(true), with timeout.
async fn poll_until<F, Fut>(timeout_secs: u64, f: F) -> ()
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<bool, anyhow::Error>>,
{
    let start = Instant::now();
    loop {
        match f().await {
            Ok(true) => return,
            Ok(false) => {}
            Err(e) => tracing::warn!(error = %e, "poll_until check failed"),
        }
        assert!(start.elapsed().as_secs() <= timeout_secs, "poll_until timed out after {timeout_secs}s");
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}
```

### Helper: `decode_secret_key`

```rust
/// Decode a key from a K8s Secret's data map (base64 → String).
fn decode_secret_key(secret: &k8s_openapi::api::core::v1::Secret, key: &str) -> String {
    let data = secret.data.as_ref().expect("secret has no data");
    let bytes = data.get(key).unwrap_or_else(|| panic!("secret missing key: {key}"));
    String::from_utf8(bytes.0.clone()).expect("secret value not UTF-8")
}
```

### Time Budget

| Stage | Description | Estimated |
|-------|-------------|-----------|
| 1 | Project creation | 5s |
| 2 | MR pipeline (kaniko builds) | 60-300s |
| 3 | Auto-merge | 10s |
| 4 | Main pipeline (kaniko, cached) | 30-120s |
| 5 | GitOps sync verification | 1s |
| 6 | Staging deploy (reconciler + canary) | 30-120s |
| 7 | OTEL round-trip (staging) | 5s |
| 8 | Feature flag evaluation | 1s |
| 9 | Promote staging → production | 1s |
| 10 | Production deploy | 30-120s |
| 11 | OTEL round-trip (production) | 5s |
| 12 | Cleanup verification | 1s |
| **Total** | | **~180-690s** |

### Existing Tests to Keep

The existing 9 tests in `e2e_demo.rs` (Tests 1-9) should be **kept as-is** for now. They test smaller slices and will continue to work with the current code. The new `demo_full_lifecycle` test is additive — it's Test 10, the comprehensive lifecycle test.

After the Tier 1-3 implementation is complete, some of the existing tests may become redundant (e.g., `demo_pipeline_mr_steps_not_filtered` is a subset of Stage 2). They can be cleaned up then.

---

## Appendix: Key Files

| File | Purpose |
|------|---------|
| `src/pipeline/definition.rs` | `.platform.yaml` parsing + validation |
| `src/pipeline/trigger.rs` | Pipeline trigger matching + dev image injection |
| `src/pipeline/executor.rs` | Step execution, gitops_handoff, finalize |
| `src/deployer/reconciler.rs` | Release state machine, namespace/secret setup |
| `src/deployer/ops_repo.rs` | Ops repo management (sync, read, write) |
| `src/deployer/applier.rs` | K8s manifest application |
| `src/deployer/renderer.rs` | Manifest rendering (minijinja) |
| `src/deployer/gateway.rs` | HTTPRoute/Gateway for canary/AB |
| `src/deployer/analysis.rs` | Canary metric analysis loop |
| `src/store/eventbus.rs` | Event publish/subscribe + handlers |
| `src/api/merge_requests.rs` | MR lifecycle, auto-merge, post-merge |
| `src/api/deployments.rs` | Deploy targets, releases, promote-staging |
| `src/api/flags.rs` | Feature flag CRUD + evaluation |
| `src/onboarding/demo_project.rs` | Demo project bootstrap |
| `src/secrets/mod.rs` | Secret encryption/decryption |
