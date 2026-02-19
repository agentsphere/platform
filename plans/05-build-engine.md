# 05 — Build Engine (Pipelines)

## Prerequisite
- 01-foundation complete (store, AppState, kube client)
- 02-identity-auth complete (AuthUser, RequirePermission)
- 03-git-server partial (pre-receive hook triggers pipeline — can stub this initially)

## Blocks
- 06-deployer (pipelines write `deployments` rows with new image_ref after successful build)

## Can Parallelize With
- 04-project-mgmt, 07-agent, 08-observability, 09-secrets-notify

---

## Scope

Parse `.platform.yaml` pipeline definitions, execute pipeline steps as Kubernetes pods, stream logs, store artifacts in MinIO. On successful build of a container image, write desired state to the `deployments` table. Replaces Woodpecker CI's build capabilities.

---

## Deliverables

### 1. `src/pipeline/mod.rs` — Module Root
Re-exports definition, executor, trigger.

### 2. `src/pipeline/definition.rs` — Pipeline Config Parser

Parse `.platform.yaml` from a project's git repo:

```yaml
# .platform.yaml
pipeline:
  steps:
    - name: test
      image: rust:1.85-slim
      commands:
        - cargo nextest run

    - name: build-image
      image: gcr.io/kaniko-project/executor:latest
      environment:
        DOCKER_CONFIG: /kaniko/.docker
      commands:
        - /kaniko/executor
          --context=.
          --dockerfile=Dockerfile
          --destination=$REGISTRY/$PROJECT:$COMMIT_SHA
          --cache=true

  artifacts:
    - name: test-results
      path: target/nextest/
      expires: 7d

  on:
    push:
      branches: [main, develop]
    mr:
      actions: [opened, synchronized]
```

Implementation:
- `pub struct PipelineDefinition` — serde-deserializable from YAML
- `pub struct PipelineStep { name, image, commands, environment, depends_on }`
- `pub struct ArtifactDef { name, path, expires }`
- `pub struct Trigger { push: Option<PushTrigger>, mr: Option<MrTrigger>, schedule: Option<CronExpr> }`
- `pub fn parse(yaml: &str) -> Result<PipelineDefinition>`
- Validation: image is required, at least one step, valid YAML

### 3. `src/pipeline/trigger.rs` — Pipeline Triggers

Handle the various ways a pipeline can start:

- **Git push** (from pre-receive hook in git module):
  - `pub async fn on_push(pool, project_id, ref_name, commit_sha, triggered_by) -> Result<Option<Uuid>>`
  - Read `.platform.yaml` from the pushed commit (`git show <sha>:.platform.yaml`)
  - Check if push matches trigger filter (branch pattern)
  - If match: insert `pipelines` row (column is `git_ref`, not `ref` — avoids SQL reserved word), insert `pipeline_steps` rows with `step_order` set from definition order, `project_id` denormalized on each step
  - Return pipeline ID

- **API trigger**:
  - `POST /api/projects/:id/pipelines` — manually trigger pipeline
  - Required: `git_ref` (branch or tag)
  - Requires: `project:write`

- **MR trigger** (called when MR is created/updated):
  - `pub async fn on_mr(pool, project_id, source_branch, commit_sha, triggered_by) -> Result<Option<Uuid>>`

### 4. `src/pipeline/executor.rs` — K8s Pod Execution

The core build engine — runs pipeline steps as Kubernetes pods:

- `pub async fn run_pipeline(state: &AppState, pipeline_id: Uuid) -> Result<()>`
  - Main execution loop:
    1. Update pipeline status to `running`, set `started_at`
    2. For each step (in order):
       a. Create K8s Pod spec:
          - Image from step definition
          - Commands as container args
          - Environment variables (merge step env + platform-injected vars)
          - Volume mount: clone project repo into `/workspace` (init container with `git clone`)
          - Resource limits from platform config
          - Pod labels: `platform.io/pipeline={id}`, `platform.io/step={name}`
       b. Create pod via kube-rs
       c. Wait for pod to start, stream logs via kube-rs `pod.log_stream()`
       d. Store logs in MinIO: `logs/pipelines/{pipeline_id}/{step_name}.log`
       e. On completion: update `pipeline_steps` row (status, exit_code, duration_ms, log_ref)
       f. If step fails: mark pipeline as `failure`, skip remaining steps
    3. On all steps success: mark pipeline as `success`
    4. Upload artifacts to MinIO (if defined in pipeline config)
    5. Clean up pods (delete completed pods after log capture)

- **Injected environment variables** (available to all steps):
  - `PLATFORM_PROJECT_ID`, `PLATFORM_PROJECT_NAME`
  - `COMMIT_SHA`, `COMMIT_REF`, `COMMIT_BRANCH`
  - `PIPELINE_ID`, `STEP_NAME`
  - `REGISTRY` (container registry URL from config)
  - `PROJECT` (project name for image tagging)
  - Secrets: resolved `${{ secrets.NAME }}` values (requires 09-secrets integration)

- **After successful image build** (step that uses Kaniko):
  - Detect image build step by convention (image contains `kaniko` or step has `deploy` flag)
  - Write/update `deployments` row: `{project_id, image_ref: "registry/project:sha", desired_status: "active"}`
  - This is the handoff to the deployer (06-deployer)

### 5. `src/api/pipelines.rs` — Pipeline API

- `POST /api/projects/:id/pipelines` — trigger pipeline manually
- `GET /api/projects/:id/pipelines` — list pipelines for project
  - Filter by: status, ref, trigger type
  - Paginated
- `GET /api/projects/:id/pipelines/:pipeline_id` — get pipeline detail
  - Includes step list with status, duration, log_ref
- `GET /api/projects/:id/pipelines/:pipeline_id/steps/:step_id/logs` — stream step logs
  - If step is running: stream live logs via kube-rs log_stream → SSE/WebSocket
  - If step is finished: redirect to MinIO presigned URL for stored logs
- `POST /api/projects/:id/pipelines/:pipeline_id/cancel` — cancel running pipeline
  - Delete running pods, mark pipeline as `cancelled`
  - Requires: `project:write`

### 6. Artifact Storage

- After pipeline completes, upload artifact files to MinIO:
  - Path: `artifacts/{pipeline_id}/{artifact_name}/`
  - Store metadata in `artifacts` table (name, minio_path, content_type, size_bytes, expires_at)
- `GET /api/projects/:id/pipelines/:pipeline_id/artifacts` — list artifacts
- `GET /api/projects/:id/pipelines/:pipeline_id/artifacts/:artifact_id/download` — presigned MinIO URL

---

## Execution Model

```
git push → pre-receive hook → trigger.on_push()
  → insert pipeline (pending) + steps (pending)
  → spawn executor task

executor task:
  → pipeline.status = running
  → for each step:
      → create K8s pod (clone repo + run commands)
      → stream logs to MinIO
      → wait for completion
      → update step status
  → if all pass: pipeline.status = success
      → upload artifacts
      → if image built: write deployments row
  → if any fail: pipeline.status = failure
  → fire webhook: build.success / build.failure
```

---

## Testing

- Unit: YAML parsing (valid, invalid, missing fields), trigger matching (branch patterns)
- Integration:
  - Create pipeline → run with mock step (simple echo command) → verify success flow
  - Step failure → pipeline marked as failure, remaining steps skipped
  - Cancel pipeline → pods deleted, status updated
  - Artifact upload → artifact listed in API → download URL works
  - Log streaming: step logs accessible during and after execution

## Done When

1. `.platform.yaml` parsed correctly
2. Git push triggers pipeline
3. Pipeline steps execute as K8s pods
4. Logs streamed and stored in MinIO
5. Artifacts uploaded and downloadable
6. Successful image build writes to `deployments` table
7. Pipeline API for listing, details, logs, cancel

## Estimated LOC
~1,400 Rust

---

## Foundation & Auth Context (from 01+02 implementation)

Things the implementor must know from completed phases:

### What already exists
- **`src/store::AppState`** — `{ pool: PgPool, valkey: fred::clients::Pool, minio: opendal::Operator, kube: kube::Client, config: Arc<Config> }`
- **`src/config::Config`** — includes `git_repos_path: PathBuf`, `minio_endpoint`, `minio_access_key`, `minio_secret_key`
- **`src/auth/middleware::AuthUser`** — axum `FromRequestParts` extractor. Fields: `user_id: Uuid`, `user_name: String`, `ip_addr: Option<String>`.
- **`src/rbac::Permission`** — enum with `ProjectWrite`, `ProjectRead`, `DeployRead`, `DeployPromote`, etc. `as_str(self)` takes `self` by value (it's `Copy`).
- **`src/rbac::resolver`** — `has_permission(pool, valkey, user_id, project_id, perm) -> Result<bool>`
- **`src/rbac::middleware::require_permission`** — route-layer middleware for permission checks. Extracts `project_id` from `/projects/:id` automatically.
- **`src/error::ApiError`** — `NotFound`, `Unauthorized`, `Forbidden`, `BadRequest`, `Conflict`, `Internal`. Has `From<sqlx::Error>`, `From<fred::error::Error>`, `From<kube::Error>`.
- **`src/store/valkey.rs`** — `publish(pool, channel, message)` for pub/sub (use `pool.next().publish()` internally — `Pool` doesn't impl `PubsubInterface`).
- **`src/api/mod.rs`** — `pub fn router() -> Router<AppState>`. Add pipeline API routes here.

### Router pattern
Each module exposes `pub fn router() -> Router<AppState>`. Merge into `api::router()`.

### Background task pattern
The plan calls for a pipeline executor that spawns pipeline runs. This should follow the background task convention:
```rust
pub async fn run(state: AppState, shutdown: tokio::sync::watch::Receiver<()>) { ... }
```
Spawned from `main.rs` before the axum server starts.

### DB column notes
- Pipeline table uses `git_ref` (not `ref` — avoids SQL reserved word). Already in migration.
- `pipeline_steps` has `step_order` column and `project_id` denormalized. Already in migration.

### Crate API gotchas (from 01+02)
- **rand 0.10**: Use `rand::fill(&mut bytes)` (free function), not `rng().fill_bytes()`
- **axum 0.8**: `.patch()`, `.put()`, `.delete()` are `MethodRouter` methods — chain directly, don't import from `axum::routing`
- **Clippy**: Functions with 7+ params need a params struct. `Copy` types use `self` not `&self`.
- **sqlx**: After adding `sqlx::query!()` calls, run `just db-prepare` to update `.sqlx/` cache. Commit `.sqlx/` changes.
- **Audit logging**: Use `AuditEntry` struct pattern with `write_audit()` for mutations.
- **kube::Error → ApiError**: `From<kube::Error>` is already implemented on `ApiError`, so `?` works directly in handlers that use kube-rs.
