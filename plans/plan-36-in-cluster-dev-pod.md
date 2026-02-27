# Plan 36: In-Cluster Dev Pod for Claude Code CLI

## Context

Currently development runs on macOS: Claude Code desktop, local Kind cluster, port-forwarding from host to cluster services, `just` commands from outside. The goal is to move to a GCP VPS (n2d-standard-4: 4 vCPUs, 16GB, AMD Milan x86_64) where Claude Code CLI runs inside a pod in a k3s cluster in `--dangerously-skip-permissions` mode with `/plan`, `/dev`, `/review`, `/finalize`. Everything — build, test, deploy — happens from inside the cluster. No port-forwarding, no Docker Desktop, no Kind.

**Constraint**: All changes are additive. The existing local Mac + Kind workflow stays fully functional.

## Architecture Overview

```
GCP VPS (n2d-standard-4)
├── k3s (single-node, containerd)
│   ├── namespace: platform-dev  ←── SINGLE NS: services + dev pod together
│   │   ├── postgres (PVC-backed)
│   │   ├── valkey
│   │   ├── minio (PVC-backed)
│   │   └── dev-pod (Rust toolchain + Claude Code CLI + Playwright)
│   │       ├── PVC: /workspace (source, target/, cargo registry, node_modules)
│   │       └── hostPath: /tmp/platform-e2e (shared with pipeline pods)
│   └── namespace: test-{run-id}-* (ephemeral, per test run)
│       ├── postgres, valkey, minio (lightweight, from hack/test-manifests/)
│       └── pipeline/agent pods (spawned by E2E tests)
```

**Single namespace design**: All services + dev pod live in one namespace (e.g., `platform-dev`). This means spinning up a second instance later is as simple as `kubectl apply` with a different namespace name — each instance is fully isolated with its own Postgres, Valkey, MinIO, and dev pod.

Key differences from current (Mac) setup:
- **k3s not Kind** — no Docker-in-Docker, lighter, runs natively on the VPS
- **No port-forwarding** — tests connect via K8s DNS (`postgres.platform-dev.svc.cluster.local`)
- **Pod IP for pipeline reachability** — replaces `host.docker.internal` / Docker bridge IP
- **In-cluster kube auth** — ServiceAccount token, no KUBECONFIG file needed

---

## Deliverables

### A. `docker/Dockerfile.dev-pod` (new)

Dev pod image with everything needed to build, test, and run the platform.

```dockerfile
FROM rust:1.88-bookworm

# System deps
RUN apt-get update && apt-get install -y --no-install-recommends \
    git openssh-client ca-certificates curl jq \
    pkg-config libssl-dev \
    python3 python3-pip python3-venv pipx \
    netcat-openbsd \
    && rm -rf /var/lib/apt/lists/*

# kubectl
RUN curl -fsSL "https://dl.k8s.io/release/$(curl -sL https://dl.k8s.io/release/stable.txt)/bin/linux/amd64/kubectl" \
    -o /usr/local/bin/kubectl && chmod +x /usr/local/bin/kubectl

# Node.js 22
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | bash - && \
    apt-get install -y nodejs && rm -rf /var/lib/apt/lists/*

# Rust components
RUN rustup component add rustfmt clippy llvm-tools-preview

# Cargo tools (matching Justfile/CI requirements)
RUN cargo install cargo-nextest --locked && \
    cargo install cargo-llvm-cov --locked && \
    cargo install cargo-sqlx --locked && \
    cargo install cargo-deny --locked && \
    cargo install just --locked

# Python tools (for diff-cover)
ENV PATH="/root/.local/bin:$PATH"
RUN pipx install diff-cover

# Claude Code CLI
RUN npm install -g @anthropic-ai/claude-code

# Playwright (headless Chromium for FE E2E)
RUN npx playwright install --with-deps chromium

WORKDIR /workspace
CMD ["sleep", "infinity"]
```

Size estimate: ~4-5GB. Build once, push to registry or import via `ctr image import`.

### B. `hack/k3s/dev-env.yaml` (new)

**Single-namespace manifest** containing all services + dev pod + RBAC. Parameterized by namespace name so multiple instances can coexist.

The namespace name defaults to `platform-dev`. Within it:

```yaml
apiVersion: v1
kind: Namespace
metadata:
  name: platform-dev       # change for additional instances
---
# ═══════════════════════════════════════════════════
# RBAC (ClusterRole + SA + Binding)
# ═══════════════════════════════════════════════════
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: dev-pod-admin
rules:
  - apiGroups: [""]
    resources: [pods, pods/log, pods/exec, services, configmaps, secrets,
                persistentvolumeclaims, serviceaccounts]
    verbs: [get, list, watch, create, update, patch, delete]
  - apiGroups: [batch]
    resources: [jobs]
    verbs: [get, list, watch, create, update, patch, delete]
  - apiGroups: [""]
    resources: [namespaces]
    verbs: [get, list, create, delete]
  - apiGroups: [""]
    resources: [events]
    verbs: [get, list, watch]
  - apiGroups: [apps]
    resources: [deployments, replicasets]
    verbs: [get, list, watch, create, update, patch, delete]
  - apiGroups: [rbac.authorization.k8s.io]
    resources: [clusterroles, clusterrolebindings, roles, rolebindings]
    verbs: [get, list, create, delete, bind]
---
apiVersion: v1
kind: ServiceAccount
metadata:
  name: dev-pod
  namespace: platform-dev
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: dev-pod-admin-platform-dev
subjects:
  - kind: ServiceAccount
    name: dev-pod
    namespace: platform-dev
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: dev-pod-admin
---
# ═══════════════════════════════════════════════════
# Postgres
# ═══════════════════════════════════════════════════
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: postgres-data
  namespace: platform-dev
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests: { storage: 5Gi }
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: postgres
  namespace: platform-dev
spec:
  replicas: 1
  selector:
    matchLabels: { app: postgres }
  template:
    metadata:
      labels: { app: postgres }
    spec:
      containers:
        - name: postgres
          image: postgres:16-alpine
          env:
            - { name: POSTGRES_USER, value: platform }
            - { name: POSTGRES_PASSWORD, value: dev }
            - { name: POSTGRES_DB, value: platform_dev }
            - { name: PGDATA, value: /var/lib/postgresql/data/pgdata }
          ports: [{ containerPort: 5432 }]
          volumeMounts:
            - { name: data, mountPath: /var/lib/postgresql/data }
          readinessProbe:
            exec: { command: [pg_isready, -U, platform] }
            periodSeconds: 2
          resources:
            requests: { cpu: 100m, memory: 256Mi }
            limits: { memory: 512Mi }
      volumes:
        - name: data
          persistentVolumeClaim: { claimName: postgres-data }
---
apiVersion: v1
kind: Service
metadata:
  name: postgres
  namespace: platform-dev
spec:
  selector: { app: postgres }
  ports: [{ port: 5432, targetPort: 5432 }]
---
# ═══════════════════════════════════════════════════
# Valkey
# ═══════════════════════════════════════════════════
apiVersion: apps/v1
kind: Deployment
metadata:
  name: valkey
  namespace: platform-dev
spec:
  replicas: 1
  selector:
    matchLabels: { app: valkey }
  template:
    metadata:
      labels: { app: valkey }
    spec:
      containers:
        - name: valkey
          image: valkey/valkey:8-alpine
          ports: [{ containerPort: 6379 }]
          resources:
            requests: { cpu: 50m, memory: 64Mi }
            limits: { memory: 256Mi }
---
apiVersion: v1
kind: Service
metadata:
  name: valkey
  namespace: platform-dev
spec:
  selector: { app: valkey }
  ports: [{ port: 6379, targetPort: 6379 }]
---
# ═══════════════════════════════════════════════════
# MinIO
# ═══════════════════════════════════════════════════
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: minio-data
  namespace: platform-dev
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests: { storage: 10Gi }
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: minio
  namespace: platform-dev
spec:
  replicas: 1
  selector:
    matchLabels: { app: minio }
  template:
    metadata:
      labels: { app: minio }
    spec:
      containers:
        - name: minio
          image: minio/minio:latest
          args: [server, /data, --console-address, ":9001"]
          env:
            - { name: MINIO_ROOT_USER, value: platform }
            - { name: MINIO_ROOT_PASSWORD, value: devdevdev }
          ports: [{ containerPort: 9000 }, { containerPort: 9001 }]
          volumeMounts:
            - { name: data, mountPath: /data }
          resources:
            requests: { cpu: 50m, memory: 128Mi }
            limits: { memory: 512Mi }
      volumes:
        - name: data
          persistentVolumeClaim: { claimName: minio-data }
---
apiVersion: v1
kind: Service
metadata:
  name: minio
  namespace: platform-dev
spec:
  selector: { app: minio }
  ports:
    - { name: api, port: 9000, targetPort: 9000 }
    - { name: console, port: 9001, targetPort: 9001 }
---
# ═══════════════════════════════════════════════════
# Dev Pod
# ═══════════════════════════════════════════════════
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: dev-workspace
  namespace: platform-dev
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests: { storage: 100Gi }
---
apiVersion: v1
kind: Pod
metadata:
  name: dev
  namespace: platform-dev
  labels: { app: dev-pod }
spec:
  serviceAccountName: dev-pod
  containers:
    - name: dev
      image: platform-dev-pod:latest
      resources:
        requests: { cpu: "3", memory: 12Gi }
        limits: { memory: 14Gi }
      env:
        # Services in SAME namespace — just use short DNS names
        - { name: DATABASE_URL, value: "postgres://platform:dev@postgres:5432/platform_dev" }
        - { name: VALKEY_URL, value: "redis://valkey:6379" }
        - { name: MINIO_ENDPOINT, value: "http://minio:9000" }
        - { name: MINIO_ACCESS_KEY, value: platform }
        - { name: MINIO_SECRET_KEY, value: devdevdev }
        - { name: PLATFORM_MASTER_KEY, value: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef" }
        - { name: PLATFORM_DEV, value: "true" }
        - { name: SQLX_OFFLINE, value: "true" }
        - { name: RUST_LOG, value: warn }
        - { name: CARGO_BUILD_JOBS, value: "4" }
        # Pod IP — Kubernetes downward API, used by e2e_helpers::host_addr_for_kind()
        - name: POD_IP
          valueFrom:
            fieldRef: { fieldPath: status.podIP }
        # Anthropic API key — from secret (created by setup.sh)
        - name: ANTHROPIC_API_KEY
          valueFrom:
            secretKeyRef: { name: claude-credentials, key: api-key }
      volumeMounts:
        - { name: workspace, mountPath: /workspace }
        - { name: e2e-shared, mountPath: /tmp/platform-e2e }
  volumes:
    - name: workspace
      persistentVolumeClaim: { claimName: dev-workspace }
    - name: e2e-shared
      hostPath: { path: /tmp/platform-e2e, type: DirectoryOrCreate }
  restartPolicy: Never
```

Since services are in the **same namespace** as the dev pod, env vars use short DNS (`postgres:5432` not `postgres.platform-dev.svc.cluster.local:5432`).

To spin up a second instance: `sed 's/platform-dev/platform-dev-2/g' hack/k3s/dev-env.yaml | kubectl apply -f -`

### C. `hack/k3s/setup.sh` (new)

One-time VPS bootstrap. Takes optional namespace name.

```bash
#!/usr/bin/env bash
set -euo pipefail

NS="${1:-platform-dev}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# 1. Install k3s if not present
if ! command -v k3s &>/dev/null; then
  echo "==> Installing k3s"
  curl -sfL https://get.k3s.io | INSTALL_K3S_EXEC="--disable traefik" sh -
fi

export KUBECONFIG=/etc/rancher/k3s/k3s.yaml
mkdir -p /tmp/platform-e2e

# 2. Apply all manifests (namespace, services, RBAC, dev pod)
echo "==> Deploying dev environment in namespace: ${NS}"
if [[ "$NS" != "platform-dev" ]]; then
  sed "s/platform-dev/${NS}/g" "${SCRIPT_DIR}/dev-env.yaml" | kubectl apply -f -
else
  kubectl apply -f "${SCRIPT_DIR}/dev-env.yaml"
fi

# 3. Wait for services
echo "==> Waiting for services..."
kubectl wait -n "${NS}" --for=condition=Available deploy/postgres --timeout=120s
kubectl wait -n "${NS}" --for=condition=Available deploy/valkey --timeout=60s
kubectl wait -n "${NS}" --for=condition=Available deploy/minio --timeout=60s

# 4. Post-deploy: CREATEDB + MinIO buckets
echo "==> Post-deploy setup"
kubectl exec -n "${NS}" deploy/postgres -- psql -U postgres -c "ALTER USER platform CREATEDB;"
sleep 2
kubectl exec -n "${NS}" deploy/minio -- sh -c \
  'mc alias set local http://localhost:9000 platform devdevdev 2>/dev/null; mc mb local/platform --ignore-existing; mc mb local/platform-e2e --ignore-existing'

# 5. Claude credentials secret (prompt if not exists)
if ! kubectl get secret claude-credentials -n "${NS}" &>/dev/null; then
  echo "Enter your ANTHROPIC_API_KEY:"
  read -s API_KEY
  kubectl create secret generic claude-credentials -n "${NS}" \
    --from-literal=api-key="$API_KEY"
fi

# 6. Wait for dev pod
echo "==> Waiting for dev pod..."
kubectl wait -n "${NS}" pod/dev --for=condition=Ready --timeout=300s

echo ""
echo "Dev environment ready in namespace: ${NS}"
echo "  kubectl exec -it -n ${NS} dev -- bash"
echo "  Then: cd /workspace && git clone <repo> && just ci"
```

### D. `hack/test-in-pod.sh` (new)

In-cluster test runner. Adapted from `hack/test-in-cluster.sh` — drops port-forwarding, uses K8s DNS.

**What changes vs `test-in-cluster.sh`**:
- Pre-flight: checks `kubectl cluster-info` instead of Kind binary / KUBECONFIG file
- No `find_free_port()`, no `PF_PIDS`, no `kubectl port-forward`
- Service URLs use DNS: `postgres.${NS}.svc.cluster.local:5432`
- Readiness wait: `kubectl wait` + DNS probe (instead of `nc -z` on forwarded ports)
- Cleanup: delete namespaces only (no port-forward PIDs to kill)
- Test execution (cargo nextest / cargo llvm-cov): **identical**

**What stays the same**:
- Ephemeral namespace per test run (`test-{timestamp}-{random}`)
- Deploys from `hack/test-manifests/` into ephemeral namespace
- E2E RBAC setup (ClusterRole, ServiceAccount, ClusterRoleBinding)
- Coverage mode, LCOV output, `--type total`
- All env vars (`PLATFORM_MASTER_KEY`, `PLATFORM_DEV`, etc.)

### E. `tests/e2e_helpers/mod.rs` change (line 607-613)

**This is backward-compatible.** On macOS (local dev), `POD_IP` is not set, so it falls through to the existing `host.docker.internal` path.

```rust
// BEFORE:
fn host_addr_for_kind() -> String {
    if cfg!(target_os = "macos") {
        "host.docker.internal".into()
    } else {
        std::env::var("E2E_HOST_ADDR").unwrap_or_else(|_| "172.18.0.1".into())
    }
}

// AFTER:
fn host_addr_for_kind() -> String {
    // In-cluster: use pod IP (set by Kubernetes downward API in dev-env.yaml)
    if let Ok(pod_ip) = std::env::var("POD_IP") {
        return pod_ip;
    }
    // Explicit override (existing)
    if let Ok(addr) = std::env::var("E2E_HOST_ADDR") {
        return addr;
    }
    // macOS: Docker Desktop bridge
    if cfg!(target_os = "macos") {
        "host.docker.internal".into()
    } else {
        // Linux: Docker bridge gateway
        "172.18.0.1".into()
    }
}
```

Behavior by environment:
- **macOS (current)**: `POD_IP` not set → `E2E_HOST_ADDR` not set → `cfg!(macos)` true → `"host.docker.internal"` (unchanged)
- **Linux local Kind**: `POD_IP` not set → `E2E_HOST_ADDR` not set → `"172.18.0.1"` (unchanged)
- **In k3s dev pod**: `POD_IP=10.42.x.x` → returns pod IP (new)

### F. `Justfile` changes

**Purely additive** — existing recipes unchanged, new `in_cluster` detection auto-routes.

Add at top of file:
```just
# Detect in-cluster environment (KUBERNETES_SERVICE_HOST is auto-set in pods)
in_cluster := env("KUBERNETES_SERVICE_HOST", "")
test_script := if in_cluster != "" { "hack/test-in-pod.sh" } else { "hack/test-in-cluster.sh" }
```

Update test recipes to use variable:
```just
test-integration:
    bash {{test_script}} --filter '*_integration'

test-e2e:
    bash {{test_script}} --type e2e
```

Same for `cov-integration`, `cov-e2e`, `cov-total`, `cov-diff`, `cov-diff-check`.

Guard cluster commands (no-op in-cluster):
```just
cluster-up:
    @if [ -n "{{in_cluster}}" ]; then echo "Already in-cluster"; exit 0; fi
    bash hack/kind-up.sh

cluster-down:
    @if [ -n "{{in_cluster}}" ]; then echo "Already in-cluster"; exit 0; fi
    bash hack/kind-down.sh
```

Add new FE E2E recipe:
```just
test-ui-headless:
    @echo "Starting platform server in background..."
    cargo run &
    @sleep 5
    cd ui && PLATFORM_URL=http://localhost:8080 npx playwright test
    @kill %1 2>/dev/null || true
```

---

## What stays unchanged (local Mac workflow)

- `hack/test-in-cluster.sh` — still used when `KUBERNETES_SERVICE_HOST` is unset
- `hack/kind-up.sh`, `hack/kind-down.sh`, `hack/kind-config.yaml` — still needed for Mac
- `hack/test-manifests/*.yaml` — reused by **both** `test-in-cluster.sh` and `test-in-pod.sh`
- `tests/helpers/mod.rs` — integration tests use env vars, no K8s
- All `tests/e2e_*.rs` files — use helpers, not infrastructure directly
- `src/config.rs`, `src/main.rs` — already read config from env vars
- `.claude/commands/*.md` — reference `just` recipes which auto-adapt
- `docker/Dockerfile` — production image unchanged

## Backward-compatibility proof

| Component | Mac behavior | In-pod behavior |
|---|---|---|
| `host_addr_for_kind()` | `POD_IP` unset → existing macOS/Linux path | `POD_IP` set → pod IP |
| `Justfile` test commands | `KUBERNETES_SERVICE_HOST` unset → `test-in-cluster.sh` | set → `test-in-pod.sh` |
| `cluster-up/down` | Runs Kind scripts | No-ops with message |
| `kube::Client::try_default()` | Reads KUBECONFIG file | Reads ServiceAccount token |

## Performance Considerations

| Operation | Mac (current) | VPS 4-vCPU (estimated) |
|---|---|---|
| Full compile | ~3 min | ~8-15 min |
| Incremental build | ~5-15s | ~15-30s |
| `just test-unit` | ~1s | ~2s |
| `just test-integration` | ~2.5 min | ~3-4 min |
| `just test-e2e` | ~2.5 min | ~3-4 min |

Mitigations:
- PVC for workspace — `target/` and cargo registry survive pod restarts
- `CARGO_BUILD_JOBS=4` matches vCPU count
- Consider `sccache` later if compile times are painful

## Verification

After deploying, run in sequence from inside the dev pod:
1. `just test-unit` — no infra needed, sanity check
2. `just test-integration` — verifies DNS-based service connectivity
3. `just test-e2e` — verifies pipeline pod ↔ dev pod connectivity via `POD_IP`
4. `just ci-full` — full quality gate
5. `just test-ui-headless` — Playwright with headless Chromium

## New Files Summary

| File | Purpose |
|---|---|
| `docker/Dockerfile.dev-pod` | Dev pod image (Rust, Node, Playwright, Claude Code) |
| `hack/k3s/dev-env.yaml` | Single-namespace manifest: services + RBAC + dev pod |
| `hack/k3s/setup.sh` | One-time VPS bootstrap (k3s install, deploy, secrets) |
| `hack/test-in-pod.sh` | In-cluster test runner (DNS instead of port-forward) |

## Modified Files Summary

| File | Change | Backward-compatible? |
|---|---|---|
| `tests/e2e_helpers/mod.rs` | +6 lines: `POD_IP` env check in `host_addr_for_kind()` | Yes — `POD_IP` unset on Mac, falls through |
| `Justfile` | +15 lines: `in_cluster` detection, `test_script` var, `test-ui-headless` | Yes — `KUBERNETES_SERVICE_HOST` unset locally |
