# 06 — Continuous Deployer

## Prerequisite
- 01-foundation complete (store, AppState, kube client)
- 02-identity-auth complete (AuthUser, RequirePermission)

## Blocks
- Nothing — deployer is a self-contained reconciliation loop

## Can Parallelize With
- 03-git-server, 04-project-mgmt, 05-build-engine (deployer reads `deployments` table that build-engine writes to, but no compile-time dependency), 07-agent, 08-observability, 09-secrets-notify

---

## Scope

Background reconciliation loop that reads desired state from the `deployments` table and applies Kubernetes manifests from ops repos. Separates "what to deploy" (declared by pipelines/humans) from "how to deploy" (manifests in version-controlled ops repos). Runs as a tokio background task in the same binary.

---

## Deliverables

### 1. `src/deployer/mod.rs` — Module Root
Re-exports reconciler, ops_repo, renderer, applier. Spawns the reconciler as a tokio background task.

### 2. `src/deployer/reconciler.rs` — Main Reconciliation Loop

The core deployer logic — a poll-based reconciler:

```rust
pub async fn run(state: AppState, shutdown: tokio::sync::watch::Receiver<()>) {
    let mut interval = tokio::time::interval(Duration::from_secs(10));
    loop {
        tokio::select! {
            _ = interval.tick() => reconcile(&state).await,
            _ = shutdown.changed() => break,
        }
    }
}
```

**`reconcile()`**:
1. Query `deployments` where `desired_status != current_status` OR `image_ref` has changed since last apply
2. For each pending deployment:
   a. Set `current_status = 'syncing'`
   b. Pull latest from ops repo (if `ops_repo_id` is set)
   c. Render manifest template with current `image_ref` + `values_override`
   d. Apply rendered manifests to cluster via kube-rs
   e. Wait for rollout health (deployment available, pods ready)
   f. On success: set `current_status = 'healthy'`, update `deployed_at`, `current_sha`
   g. On failure: set `current_status = 'failed'`
   h. Write `deployment_history` row (action, status, image_ref, sha)

**Rollback handling**:
- When `desired_status = 'rollback'`: look up previous successful `deployment_history` entry, use that `image_ref`
- When `desired_status = 'stopped'`: scale deployment to 0 replicas

### 3. `src/deployer/ops_repo.rs` — Ops Repo Management

Manage local clones of ops repos (git repos containing K8s manifests):

- `pub async fn sync(pool: &PgPool, repos_dir: &Path) -> Result<()>`
  - For each `ops_repos` row: `git clone` if not exists, `git pull` if exists
  - Track last sync time, respect `sync_interval_s`
- `pub fn manifest_path(repos_dir: &Path, ops_repo: &OpsRepo, deployment: &Deployment) -> PathBuf`
  - Resolve the manifest template file within the ops repo
- Local clone directory: `/data/ops-repos/{ops_repo_name}/`

### 4. `src/deployer/renderer.rs` — Manifest Template Rendering

Render K8s manifest templates using minijinja:

- `pub fn render(template_path: &Path, vars: &RenderVars) -> Result<String>`
- `RenderVars`:
  - `image_ref: String` — container image to deploy
  - `project_name: String`
  - `environment: String` — preview/staging/production
  - `values: serde_json::Value` — merged from ops repo `values.yaml` + `deployments.values_override`
- Template format: standard Jinja2 / minijinja syntax
  - `{{ image_ref }}`, `{{ values.replicas }}`, `{{ environment }}`
- Output: rendered YAML string, can contain multiple documents (separated by `---`)

### 5. `src/deployer/applier.rs` — K8s Apply & Health Check

Apply rendered manifests to the cluster and verify health:

- `pub async fn apply(kube_client: &kube::Client, manifests_yaml: &str, namespace: &str) -> Result<ApplyResult>`
  - Parse multi-document YAML into individual resources
  - Use kube-rs `DynamicObject` with server-side apply (`fieldManager: "platform-deployer"`)
  - Handle: Deployment, Service, ConfigMap, Secret, Ingress, HPA, PDB, etc.
- `pub async fn wait_healthy(kube_client: &kube::Client, namespace: &str, deployment_name: &str, timeout: Duration) -> Result<bool>`
  - Watch Deployment conditions: `Available=True`, `Progressing!=False`
  - Check pod readiness
  - Timeout with configurable duration (default 5min)
- `pub async fn scale(kube_client: &kube::Client, namespace: &str, deployment_name: &str, replicas: i32) -> Result<()>`
  - For stop/scale operations

### 6. `src/api/deployments.rs` — Deployment API

- `GET /api/projects/:id/deployments` — list deployments for project
  - Returns: environment, image_ref, desired_status, current_status, deployed_at
  - Requires: `deploy:read`
- `GET /api/projects/:id/deployments/:env` — get deployment detail
  - Includes: recent history, current pod status
  - Requires: `deploy:read`
- `PATCH /api/projects/:id/deployments/:env` — update deployment desired state
  - Can update: `image_ref` (redeploy), `desired_status` (stop/rollback/active), `values_override`
  - Requires: `deploy:promote`
  - Writes audit log
- `POST /api/projects/:id/deployments/:env/rollback` — rollback to previous version
  - Sets `desired_status = 'rollback'`
  - Requires: `deploy:promote`
- `GET /api/projects/:id/deployments/:env/history` — deployment history
  - Requires: `deploy:read`

### 7. Ops Repo Admin API

- `POST /api/admin/ops-repos` — register ops repo
  - Required: `name`, `repo_url`, `branch`
  - Optional: `path` (subdirectory), `sync_interval_s`
  - Requires: admin role
- `GET /api/admin/ops-repos` — list ops repos
- `PATCH /api/admin/ops-repos/:id` — update ops repo config
- `DELETE /api/admin/ops-repos/:id` — unregister ops repo
- `POST /api/admin/ops-repos/:id/sync` — force sync now

---

## Reconciliation Flow

```
Pipeline succeeds → writes deployments row: {image_ref: "reg/app:sha-abc", desired_status: "active"}
                                              ↓
Deployer poll (every 10s) → finds deployment where desired != current
  → sync ops repo (git pull)
  → render manifest template with image_ref + values
  → kubectl apply (server-side apply via kube-rs)
  → wait for rollout health
  → update current_status = "healthy"
  → write deployment_history entry

Human/agent triggers rollback → sets desired_status = "rollback"
  → deployer picks up → finds previous healthy image_ref from history
  → re-renders manifest with old image_ref → applies → verifies health
```

---

## Testing

- Unit: manifest rendering (template → rendered YAML), vars substitution
- Integration:
  - Write deployment row → deployer picks up → applies to kind cluster → status becomes healthy
  - Update image_ref → deployer re-deploys → new pods running
  - Rollback → previous image restored
  - Stop → replicas scaled to 0
  - Failed deploy → current_status = failed, history recorded
  - Ops repo sync: clone, pull, path resolution

## Done When

1. Deployer background task runs and reconciles on interval
2. Ops repos cloned/pulled automatically
3. Manifest templates rendered with correct values
4. Manifests applied to cluster via kube-rs server-side apply
5. Health checks verify successful rollout
6. Rollback and stop operations work
7. Deployment history recorded for every action
8. API endpoints for deployment management

## Estimated LOC
~800 Rust

---

## Foundation & Auth Context (from 01+02 implementation)

Things the implementor must know from completed phases:

### What already exists
- **`src/store::AppState`** — `{ pool: PgPool, valkey: fred::clients::Pool, minio: opendal::Operator, kube: kube::Client, config: Arc<Config> }`
- **`src/config::Config`** — includes `git_repos_path: PathBuf` (ops repo clones could reuse or have their own dir)
- **`src/auth/middleware::AuthUser`** — axum `FromRequestParts` extractor. Fields: `user_id: Uuid`, `user_name: String`, `ip_addr: Option<String>`.
- **`src/rbac::Permission`** — enum with `DeployRead`, `DeployPromote`, `AdminUsers`, etc. `as_str(self)` takes `self` by value (it's `Copy`).
- **`src/rbac::resolver`** — `has_permission(pool, valkey, user_id, project_id, perm) -> Result<bool>`
- **`src/rbac::middleware::require_permission`** — route-layer middleware. Usage:
  ```rust
  .route_layer(axum::middleware::from_fn_with_state(
      state.clone(),
      require_permission(Permission::DeployRead),
  ))
  ```
  Extracts `project_id` from `/projects/:id` path segments automatically.
- **`src/error::ApiError`** — `NotFound`, `Unauthorized`, `Forbidden`, `BadRequest`, `Conflict`, `Internal`. Has `From<kube::Error>` so `?` works directly on kube-rs calls.
- **`src/store/valkey.rs`** — `publish(pool, channel, message)` for pub/sub notifications. Note: uses `pool.next().publish()` internally.
- **`src/api/mod.rs`** — `pub fn router() -> Router<AppState>`. Add deployment API routes here.

### Background task pattern
The reconciler should follow this convention (spawned from `main.rs`):
```rust
pub async fn run(state: AppState, shutdown: tokio::sync::watch::Receiver<()>) {
    let mut interval = tokio::time::interval(Duration::from_secs(10));
    loop {
        tokio::select! {
            _ = interval.tick() => reconcile(&state).await,
            _ = shutdown.changed() => break,
        }
    }
}
```

### Crate API gotchas (from 01+02)
- **axum 0.8**: `.patch()`, `.put()`, `.delete()` are `MethodRouter` methods — chain directly, don't import from `axum::routing`
- **Clippy**: Functions with 7+ params need a params struct. `Copy` types use `self` not `&self`.
- **sqlx**: After adding `sqlx::query!()` calls, run `just db-prepare` to update `.sqlx/` cache. Commit `.sqlx/` changes.
- **Audit logging**: Use `AuditEntry` struct pattern with `write_audit()` for deployment mutations.
- **kube::Error → ApiError**: Already implemented, so `?` propagation works from kube-rs calls in handlers.
