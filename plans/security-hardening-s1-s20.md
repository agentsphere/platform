# Implementation Plan: Security Hardening S3–S20 (Unique Findings)

**Source:** `plans/security-audit-2026-03-24.md` — UNIQUE findings only (not covered by `/audit` A-prefix or `/audit-ecosystem` E-prefix)
**Scope:** S3, S6, S7, S9, S10, S11, S12, S13, S15, S16, S19, S20 (12 findings)
**Excluded:** S1/S2 (covered A3/A81), S4 (covered A38/E8), S5 (accepted DD-2), S8 (covered E31), S14 (covered A1), S17/S18 (covered A19/A21)

---

## Step 1: S11 — Fix deleted workspace permission persistence

**Problem:** `add_workspace_permissions()` in `src/rbac/resolver.rs:194-203` joins `workspace_members → projects` but never checks `workspaces.is_active = true`. Soft-deleted workspace members retain project permissions permanently.

### 1a. Fix the resolver query

**File:** `src/rbac/resolver.rs:194-203`

Add a JOIN to the workspaces table with `is_active` check:

```sql
-- Before:
SELECT wm.role as "role!"
FROM workspace_members wm
JOIN projects p ON p.workspace_id = wm.workspace_id
WHERE p.id = $1 AND p.is_active = true AND wm.user_id = $2

-- After:
SELECT wm.role as "role!"
FROM workspace_members wm
JOIN projects p ON p.workspace_id = wm.workspace_id
JOIN workspaces w ON w.id = wm.workspace_id
WHERE p.id = $1 AND p.is_active = true AND w.is_active = true AND wm.user_id = $2
```

### 1b. Invalidate caches on workspace deletion

**File:** `src/workspace/service.rs:174-182`

After soft-deleting the workspace, query all `workspace_members` for this workspace and invalidate their permission caches:

```rust
// After UPDATE workspaces SET is_active = false:
let members = sqlx::query_scalar!(
    "SELECT user_id FROM workspace_members WHERE workspace_id = $1",
    workspace_id
).fetch_all(pool).await?;

for user_id in &members {
    resolver::invalidate_permissions(valkey, *user_id, None).await;
}
```

### 1c. Tests

**Integration test** (`tests/setup_integration.rs` or new `tests/workspace_permissions.rs`):

```
test_deleted_workspace_revokes_permissions:
  1. Create workspace + project in workspace
  2. Add user as workspace member
  3. Verify user has ProjectRead on the project
  4. Soft-delete workspace
  5. Verify user NO LONGER has ProjectRead (should get 404)
```

**Unit test** (`src/rbac/resolver.rs` #[cfg(test)]):

Not needed — the fix is a SQL query change. The integration test covers the full path.

### sqlx changes

Query uses `sqlx::query_as!` → run `just db-prepare` after the query change to update `.sqlx/` offline cache.

---

## Step 2: S20 — Prevent workspace owner demotion via add_member

**Problem:** `add_member` in `src/api/workspaces.rs:306-342` allows a workspace admin to overwrite the owner's role via `ON CONFLICT DO UPDATE SET role = EXCLUDED.role`.

### 2a. Add owner protection check

**File:** `src/api/workspaces.rs` — in `add_member` handler, after role validation (line ~319), before calling `service::add_member()`:

```rust
// Check if target user is the workspace owner — prevent demotion
let existing = sqlx::query_scalar!(
    "SELECT role FROM workspace_members WHERE workspace_id = $1 AND user_id = $2",
    id, body.user_id
).fetch_optional(&state.pool).await.map_err(ApiError::Internal)?;

if existing.as_deref() == Some("owner") {
    return Err(ApiError::BadRequest("cannot modify workspace owner role".into()));
}
```

### 2b. Tests

**Integration test:**

```
test_add_member_cannot_demote_owner:
  1. Create workspace (creator is owner)
  2. Add admin user to workspace
  3. As admin, call POST /api/workspaces/{id}/members with owner's user_id and role="member"
  4. Assert 400 BadRequest "cannot modify workspace owner role"
  5. Verify owner's role is still "owner"

test_add_member_can_change_member_to_admin:
  1. Create workspace
  2. Add user as member
  3. As workspace admin, call POST with user_id and role="admin"
  4. Assert 200
  5. Verify user is now admin
```

---

## Step 3: S3 — PodSecurityAdmission `baseline` on session namespaces

**Problem:** No PodSecurityAdmission labels on session namespaces. Agent pods can create privileged containers.

### 3a. Add PSA labels to namespace creation

**File:** `src/deployer/namespace.rs:252-264` — in `build_namespace_object()`, add PSA labels to session namespaces:

```rust
// Add to the labels BTreeMap for session namespaces:
if env == "session" {
    labels.insert("pod-security.kubernetes.io/enforce".into(), "baseline".into());
    labels.insert("pod-security.kubernetes.io/enforce-version".into(), "latest".into());
    labels.insert("pod-security.kubernetes.io/warn".into(), "restricted".into());
    labels.insert("pod-security.kubernetes.io/warn-version".into(), "latest".into());
}
```

### 3b. Add ResourceQuota to session namespaces

**File:** `src/deployer/namespace.rs` — in `ensure_session_namespace()` (lines 93-154), after RBAC and NetworkPolicy creation, create a ResourceQuota:

```rust
let quota = serde_json::json!({
    "apiVersion": "v1",
    "kind": "ResourceQuota",
    "metadata": {
        "name": "session-quota",
        "namespace": &ns_name,
    },
    "spec": {
        "hard": {
            "pods": "10",
            "requests.cpu": "4",
            "requests.memory": "8Gi",
            "limits.cpu": "8",
            "limits.memory": "16Gi",
        }
    }
});
```

Apply via the same `kube::Api<DynamicObject>` pattern used for the Role and NetworkPolicy.

### 3c. Add LimitRange to session namespaces

Same location — create a LimitRange so agent-created pods get default limits:

```rust
let limit_range = serde_json::json!({
    "apiVersion": "v1",
    "kind": "LimitRange",
    "metadata": {
        "name": "session-limits",
        "namespace": &ns_name,
    },
    "spec": {
        "limits": [{
            "type": "Container",
            "default": {
                "cpu": "1",
                "memory": "1Gi",
            },
            "defaultRequest": {
                "cpu": "100m",
                "memory": "128Mi",
            }
        }]
    }
});
```

### 3d. Tests

**Integration test:**

```
test_session_namespace_has_psa_labels:
  1. Create a project + agent session (or call ensure_session_namespace directly)
  2. GET the namespace via kube client
  3. Assert labels contain:
     - pod-security.kubernetes.io/enforce = "baseline"
     - pod-security.kubernetes.io/warn = "restricted"

test_session_namespace_has_resource_quota:
  1. Create session namespace
  2. GET ResourceQuota "session-quota" in the namespace
  3. Assert hard limits: pods=10, requests.cpu=4, requests.memory=8Gi

test_session_namespace_has_limit_range:
  1. Create session namespace
  2. GET LimitRange "session-limits" in the namespace
  3. Assert container defaults: cpu=1, memory=1Gi

test_session_namespace_blocks_privileged_pod (E2E):
  1. Create session namespace (has baseline PSA)
  2. Attempt to create a pod with privileged: true via kube client
  3. Assert the pod is REJECTED by admission controller
```

**Note:** The PSA E2E test requires K8s 1.25+ with PodSecurity admission enabled (default in Kind).

---

## Step 4: S6 — Scope ClusterRole secrets to per-namespace RoleBindings

**Problem:** ClusterRole grants secrets CRUD cluster-wide.

### 4a. Create shared ClusterRole template in Helm

**New file:** `helm/platform/templates/clusterrole-secrets.yaml`

```yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: {{ include "platform.fullname" . }}-secrets-manager
  labels: {{ include "platform.labels" . | nindent 4 }}
rules:
  - apiGroups: [""]
    resources: ["secrets"]
    verbs: ["get", "list", "watch", "create", "update", "patch", "delete"]
```

### 4b. Remove secrets from main ClusterRole

**File:** `helm/platform/templates/clusterrole.yaml:38-40` — remove the `secrets` entry from the core API group resources list.

### 4c. Create per-namespace RoleBinding in namespace.rs

**File:** `src/deployer/namespace.rs` — in `ensure_namespace()` (called for all managed namespaces: dev, staging, prod, session), add a RoleBinding for secrets:

```rust
let secrets_binding = serde_json::json!({
    "apiVersion": "rbac.authorization.k8s.io/v1",
    "kind": "RoleBinding",
    "metadata": {
        "name": "platform-secrets-access",
        "namespace": &ns_name,
    },
    "roleRef": {
        "apiGroup": "rbac.authorization.k8s.io",
        "kind": "ClusterRole",
        "name": format!("{}-secrets-manager", platform_release_name),
    },
    "subjects": [{
        "kind": "ServiceAccount",
        "name": platform_sa_name,
        "namespace": platform_namespace,
    }]
});
```

The platform SA name and namespace come from `config.platform_namespace` (already available in AppState).

### 4d. Update test RBAC

**File:** `hack/test-manifests/rbac.yaml` — add the secrets-manager ClusterRole and remove secrets from the main ClusterRole, matching the Helm change.

### 4e. Tests

**Integration test:**

```
test_managed_namespace_has_secrets_rolebinding:
  1. Create a project (triggers namespace creation)
  2. GET RoleBinding "platform-secrets-access" in the project-dev namespace
  3. Assert roleRef.name matches the secrets-manager ClusterRole
  4. Assert subject is the platform ServiceAccount

test_platform_cannot_read_kube_system_secrets:
  (This is hard to test directly — requires an SA token with the scoped permissions.
   Best verified manually or via a policy test. Skip for automated tests.)
```

---

## Step 5: S16 — Add tag pattern to pipeline registry token

**Problem:** Pipeline registry token at `src/pipeline/executor.rs:1177-1186` has no `registry_tag_pattern`, allowing cross-project image pushes.

### 5a. Add tag pattern to token INSERT

**File:** `src/pipeline/executor.rs:1177-1186` — modify the INSERT to include project scope and tag pattern:

```sql
-- Before:
INSERT INTO api_tokens (id, user_id, name, token_hash, expires_at)
VALUES ($1, $2, $3, $4, now() + interval '1 hour')

-- After:
INSERT INTO api_tokens (id, user_id, name, token_hash, expires_at, project_id, registry_tag_pattern)
VALUES ($1, $2, $3, $4, now() + interval '1 hour', $5, $6)
```

Pass `project_id` and `registry_tag_pattern = format!("{project_name}/*")` as the new bind params.

### 5b. Tests

**Integration test:**

```
test_pipeline_registry_token_has_tag_pattern:
  1. Create a project
  2. Trigger a pipeline with an imagebuild step
  3. Query api_tokens where name LIKE 'pipeline-%'
  4. Assert project_id = the project's ID
  5. Assert registry_tag_pattern = "{project_name}/*"

test_pipeline_cannot_push_to_other_project_registry:
  (E2E — harder to test. Could verify token scope by attempting a registry
   push with the wrong tag pattern. Lower priority.)
```

### sqlx changes

Query change → run `just db-prepare`.

---

## Step 6: S15 — Add securityContext to non-kaniko pipeline steps

**Problem:** All pipeline step containers run as root with no capability drops. Only kaniko actually needs root.

### 6a. Apply container_security() to non-kaniko steps

**File:** `src/pipeline/executor.rs:1373-1396` — the step container definition.

The existing `container_security()` function (lines 1663-1672) already defines the hardened context. Apply it conditionally:

```rust
// In the step container construction:
let security_context = if step_type == StepKind::ImageBuild {
    None // kaniko needs root + capabilities for image builds
} else {
    Some(container_security()) // drop ALL, no privilege escalation
};
```

### 6b. Tests

**Integration test:**

```
test_pipeline_command_step_has_security_context:
  1. Create a project with .platform.yaml containing a "command" step
  2. Trigger pipeline
  3. Wait for pod creation
  4. GET the pod spec via kube client
  5. Assert step container has:
     - allowPrivilegeEscalation: false
     - capabilities.drop: ["ALL"]

test_pipeline_imagebuild_step_has_no_security_context:
  1. Create project with .platform.yaml containing an "imagebuild" step
  2. Trigger pipeline
  3. Wait for pod creation
  4. GET the pod spec
  5. Assert step container has NO restrictive securityContext (kaniko needs root)
```

---

## Step 7: S19 — Validate pod specs in deployer manifests

**Problem:** Deployer applies Deployments/DaemonSets/StatefulSets without validating the inner pod spec. Malicious manifests can include `privileged: true`, `hostNetwork`, `hostPath`.

### 7a. Add pod spec validation function

**File:** `src/deployer/applier.rs` — new function:

```rust
fn validate_pod_spec(manifest: &serde_json::Value) -> Result<(), ApplierError> {
    // Extract pod spec from Deployment/StatefulSet/DaemonSet/Job/CronJob
    let pod_spec = extract_pod_spec(manifest);
    if let Some(spec) = pod_spec {
        // Block dangerous fields
        if spec.pointer("/hostNetwork") == Some(&serde_json::Value::Bool(true)) {
            return Err(ApplierError::ForbiddenManifest("hostNetwork is not allowed".into()));
        }
        if spec.pointer("/hostPID") == Some(&serde_json::Value::Bool(true)) {
            return Err(ApplierError::ForbiddenManifest("hostPID is not allowed".into()));
        }
        if spec.pointer("/hostIPC") == Some(&serde_json::Value::Bool(true)) {
            return Err(ApplierError::ForbiddenManifest("hostIPC is not allowed".into()));
        }
        // Check containers for privileged
        if let Some(containers) = spec.pointer("/containers").and_then(|c| c.as_array()) {
            for container in containers {
                if container.pointer("/securityContext/privileged") == Some(&serde_json::Value::Bool(true)) {
                    return Err(ApplierError::ForbiddenManifest("privileged containers are not allowed".into()));
                }
            }
        }
        // Check for hostPath volumes
        if let Some(volumes) = spec.pointer("/volumes").and_then(|v| v.as_array()) {
            for vol in volumes {
                if vol.get("hostPath").is_some() {
                    return Err(ApplierError::ForbiddenManifest("hostPath volumes are not allowed".into()));
                }
            }
        }
    }
    Ok(())
}

fn extract_pod_spec(manifest: &serde_json::Value) -> Option<&serde_json::Value> {
    // Deployment/StatefulSet/DaemonSet → spec.template.spec
    manifest.pointer("/spec/template/spec")
        // Job → spec.template.spec
        .or_else(|| manifest.pointer("/spec/template/spec"))
        // CronJob → spec.jobTemplate.spec.template.spec
        .or_else(|| manifest.pointer("/spec/jobTemplate/spec/template/spec"))
}
```

### 7b. Call validation before apply

**File:** `src/deployer/applier.rs:60-122` — in `apply_with_tracking()`, after the ALLOWED_KINDS check (line ~92), before server-side apply:

```rust
// After kind validation, before apply:
validate_pod_spec(&manifest)?;
```

### 7c. Add ForbiddenManifest error variant

**File:** `src/deployer/error.rs` — add variant:

```rust
#[error("forbidden manifest: {0}")]
ForbiddenManifest(String),
```

### 7d. Tests

**Unit tests** (`src/deployer/applier.rs` #[cfg(test)]):

```
test_validate_pod_spec_rejects_privileged:
  Input: Deployment JSON with spec.template.spec.containers[0].securityContext.privileged: true
  Assert: Err(ForbiddenManifest("privileged containers..."))

test_validate_pod_spec_rejects_host_network:
  Input: Deployment JSON with spec.template.spec.hostNetwork: true
  Assert: Err(ForbiddenManifest("hostNetwork..."))

test_validate_pod_spec_rejects_host_path:
  Input: Deployment JSON with spec.template.spec.volumes[0].hostPath: {path: "/"}
  Assert: Err(ForbiddenManifest("hostPath..."))

test_validate_pod_spec_rejects_host_pid:
  Input: Deployment with hostPID: true
  Assert: Err

test_validate_pod_spec_allows_normal_deployment:
  Input: Normal Deployment with no dangerous fields
  Assert: Ok(())

test_validate_pod_spec_allows_configmap_secret:
  Input: Deployment with configMap and secret volumes
  Assert: Ok(())

test_validate_pod_spec_checks_init_containers:
  Input: Deployment with privileged init container
  Assert: Err

test_validate_cronjob_pod_spec:
  Input: CronJob with hostNetwork in nested spec
  Assert: Err
```

**Integration test:**

```
test_deployer_rejects_privileged_manifest:
  1. Create project + deployment target
  2. Push ops-repo manifest with a privileged Deployment
  3. Trigger reconciliation
  4. Assert deployment fails with ForbiddenManifest error (check release status or logs)
```

---

## Step 8: S12 — PostgreSQL TLS everywhere

**Problem:** Plaintext Postgres connections.

Implementation already detailed in the security audit report (S12 section). Summary:

### 8a. Dev/test cluster

**File:** `hack/test-manifests/postgres.yaml` — add init container for self-signed cert generation, SSL args to postgres container.

**File:** `hack/test-in-cluster.sh` — append `?sslmode=require` to DATABASE_URL.

### 8b. Production

**File:** `helm/platform/values.yaml` — add `postgresql.tls.enabled: true`, `postgresql.tls.autoGenerated: true`.

**File:** `helm/platform/templates/secret.yaml` — append `?sslmode=require` to DATABASE_URL.

### 8c. Tests

No new tests needed — existing integration and E2E tests run against the dev cluster Postgres. If the cert setup is correct, all existing tests pass with `?sslmode=require`. If not, they fail immediately (good — catches misconfiguration).

**Smoke verification:** After applying the change, run `just test-unit` (no DB), then `just test-integration` (real DB with TLS). If integration tests pass, TLS is working.

---

## Step 9: S13 — Valkey auth + TLS

**Problem:** Valkey has no authentication and no TLS.

### 9a. Dev/test cluster — enable auth

**File:** `hack/test-manifests/valkey.yaml` — add `--requirepass dev` to Valkey args:

```yaml
args: ["--save", "", "--appendonly", "no", "--requirepass", "dev"]
```

**File:** `hack/test-in-cluster.sh` — update VALKEY_URL:

```bash
# Before:
export VALKEY_URL="redis://${NODE_IP}:${VALKEY_PORT}"
# After:
export VALKEY_URL="redis://:dev@${NODE_IP}:${VALKEY_PORT}"
```

**File:** `hack/cluster-up.sh` — same URL update if Valkey URL is set there.

### 9b. Production — enable auth in Helm

**File:** `helm/platform/values.yaml`:

```yaml
valkey:
  auth:
    enabled: true
    password: ""  # auto-generated by Bitnami if empty
```

The Helm secret template already constructs VALKEY_URL — ensure it includes the password:

```
redis://:PASSWORD@release-valkey-master:6379
```

### 9c. TLS (follow-up)

Valkey TLS requires cert setup similar to Postgres. For now, auth alone mitigates the primary threat (unauthenticated access from any pod in the network). TLS can follow the same init-container pattern as Postgres.

### 9d. Tests

Existing tests use VALKEY_URL from the environment. Updating the URL to include `:dev@` password is sufficient. All existing tests pass if the password is correct.

**Verify:** `just test-integration` — if Valkey auth is misconfigured, every test that touches Valkey (permissions, rate limiting, sessions, pub/sub) will fail immediately.

---

## Step 10: S7 — Pin GitHub Actions to SHA digests

**Problem:** All Actions use mutable tags.

### 10a. Pin actions

**Files:** `.github/workflows/ci.yaml`, `.github/workflows/release.yaml`

For each action, look up the current SHA for the tag and replace:

```yaml
# Before:
- uses: actions/checkout@v6
# After:
- uses: actions/checkout@<full-sha>  # v6
```

Do this for every action in both workflows: `actions/checkout`, `Swatinem/rust-cache`, `taiki-e/install-action`, `helm/kind-action`, `codecov/codecov-action`, `docker/login-action`, `docker/setup-buildx-action`, `docker/build-push-action`, etc.

### 10b. Tests

None — CI configuration. Verify by pushing to a branch and confirming the workflow runs.

---

## Step 11: S10 — Add top-level permissions to CI workflow

**Problem:** No `permissions` block → default token permissions may be too broad.

### 11a. Add deny-all default

**File:** `.github/workflows/ci.yaml` — add at the top level:

```yaml
permissions: {}
```

Then add minimal permissions per job:

```yaml
jobs:
  lint:
    permissions:
      contents: read
  test:
    permissions:
      contents: read
  # etc.
```

**File:** `.github/workflows/release.yaml` — already has per-job permissions (good). Add top-level `permissions: {}` as defense-in-depth.

### 11b. Tests

None — CI configuration. Verify by pushing and confirming workflows still succeed.

---

## Step 12: S9 — Replace curl|bash NodeSource in dev-pod

**Problem:** `curl -fsSL https://deb.nodesource.com/setup_22.x | bash -` in Dockerfile.dev-pod.

### 12a. Use multi-stage copy from official Node image

**File:** `docker/Dockerfile.dev-pod:38` — replace the curl|bash block:

```dockerfile
# Before:
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | bash - && \
    apt-get install -y nodejs

# After:
COPY --from=node:22-slim /usr/local/bin/node /usr/local/bin/node
COPY --from=node:22-slim /usr/local/bin/npm /usr/local/bin/npm
COPY --from=node:22-slim /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -sf /usr/local/lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm && \
    ln -sf /usr/local/lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx
```

### 12b. Tests

None — Dockerfile change. Verify by building the image: `docker build -f docker/Dockerfile.dev-pod .` and running `node --version` inside.

---

## Execution Order & Dependencies

```
         ┌─────────────────────────────────────┐
         │  Independent — can be parallelized   │
         └─────────────────────────────────────┘

 Step 1 (S11): Workspace permission fix      ← SQL change, needs db-prepare
 Step 2 (S20): Owner demotion protection     ← API handler change
 Step 5 (S16): Pipeline registry tag pattern ← SQL change, needs db-prepare
 Step 6 (S15): Pipeline step securityContext ← executor.rs
 Step 7 (S19): Deployer pod spec validation  ← applier.rs, new unit tests
 Step 10 (S7): Pin GitHub Actions            ← CI config only
 Step 11 (S10): CI permissions               ← CI config only
 Step 12 (S9): Replace curl|bash             ← Dockerfile only

         ┌─────────────────────────────────────┐
         │  Depends on infrastructure changes   │
         └─────────────────────────────────────┘

 Step 8 (S12): Postgres TLS                  ← test-manifests + Helm
 Step 9 (S13): Valkey auth                   ← test-manifests + Helm
   └── Run `just test-integration` after to verify connectivity

         ┌─────────────────────────────────────┐
         │  Depends on Helm + namespace.rs      │
         └─────────────────────────────────────┘

 Step 3 (S3): PSA labels + ResourceQuota     ← namespace.rs
 Step 4 (S6): Scoped secrets ClusterRole     ← Helm + namespace.rs
   └── Must be tested together (both touch namespace creation)
```

## Test Summary

| Step | Finding | Unit Tests | Integration Tests | E2E Tests |
|---|---|---|---|---|
| 1 | S11 | — | `test_deleted_workspace_revokes_permissions` | — |
| 2 | S20 | — | `test_add_member_cannot_demote_owner`, `test_add_member_can_change_role` | — |
| 3 | S3 | — | `test_session_ns_psa_labels`, `test_session_ns_resource_quota`, `test_session_ns_limit_range` | `test_session_ns_blocks_privileged_pod` |
| 4 | S6 | — | `test_managed_ns_has_secrets_rolebinding` | — |
| 5 | S16 | — | `test_pipeline_registry_token_has_tag_pattern` | — |
| 6 | S15 | — | `test_command_step_has_security_context`, `test_imagebuild_step_no_restriction` | — |
| 7 | S19 | `test_rejects_privileged`, `test_rejects_host_network`, `test_rejects_host_path`, `test_rejects_host_pid`, `test_allows_normal`, `test_allows_configmap_secret`, `test_checks_init_containers`, `test_cronjob_nested` | `test_deployer_rejects_privileged_manifest` | — |
| 8 | S12 | — | Existing tests verify (pass = TLS works) | — |
| 9 | S13 | — | Existing tests verify (pass = auth works) | — |
| 10 | S7 | — | — | — |
| 11 | S10 | — | — | — |
| 12 | S9 | — | — | — |
| **Total** | | **8 unit** | **~10 integration** | **1 E2E** |

## Verification

After all steps are implemented:

```bash
just test-unit          # Verify unit tests (S19 pod spec validation)
just test-integration   # Verify all integration tests (S11, S20, S3, S6, S15, S16, S12, S13)
just test-e2e           # Verify E2E (S3 privileged pod rejection)
just ci-full            # Full CI pass
```
