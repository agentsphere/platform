#!/usr/bin/env bash
# deploy-services.sh — Deploy PostgreSQL, Valkey, MinIO, and preview-proxy into a given namespace.
#
# Usage: hack/deploy-services.sh <namespace>
#
# Applies the test manifests, waits for readiness, grants CREATEDB,
# and creates the MinIO bucket. Used by both `just dev` and test-in-cluster.sh.

set -euo pipefail

NS="${1:?Usage: hack/deploy-services.sh <namespace>}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# Create namespace if needed
kubectl create namespace "${NS}" --dry-run=client -o yaml | kubectl apply -f -

# Wait for default service account (K8s creates it asynchronously after namespace)
for _ in $(seq 1 30); do
  kubectl get serviceaccount default -n "${NS}" &>/dev/null && break
  sleep 0.2
done

# ── Compute proxy path + platform API URL for sed replacement ────────────
WORKTREE="${WORKTREE:-$(bash "${SCRIPT_DIR}/detect-worktree.sh")}"
PLATFORM_URL="http://${REGISTRY_BACKEND_HOST:-host.docker.internal}:${REGISTRY_BACKEND_PORT:-8080}"
PROXY_PATH="/tmp/platform-e2e/${WORKTREE}/proxy"

echo "==> Deploying services into namespace: ${NS}"
echo "  Proxy path: ${PROXY_PATH}"
echo "  Platform URL: ${PLATFORM_URL}"

# Apply proxy-wrapped manifests (sed replaces __PLATFORM_API_URL__ and __PROXY_PATH__)
for f in postgres.yaml valkey.yaml minio.yaml; do
  sed -e "s|__PLATFORM_API_URL__|${PLATFORM_URL}|g" \
      -e "s|__PROXY_PATH__|${PROXY_PATH}|g" \
    "${SCRIPT_DIR}/test-manifests/${f}" \
    | kubectl apply -n "${NS}" -f -
done

# preview-proxy has no placeholders — apply directly
kubectl apply -n "${NS}" -f "${SCRIPT_DIR}/test-manifests/preview-proxy.yaml"

echo "==> Waiting for services to be ready"
kubectl wait -n "${NS}" --for=condition=Ready pod/postgres --timeout=60s
kubectl wait -n "${NS}" --for=condition=Ready pod/valkey --timeout=30s
kubectl wait -n "${NS}" --for=condition=Ready pod/minio --timeout=30s
kubectl wait -n "${NS}" --for=condition=Ready pod/preview-proxy --timeout=30s
echo "  All services ready"

# Grant CREATEDB (required by sqlx::test macro)
# Postgres does a shutdown/restart cycle after init — wait for it to stabilize.
echo "==> Post-deploy setup"
for i in $(seq 1 15); do
  if kubectl exec -n "${NS}" postgres -- \
    psql -U platform -d platform_dev -c "SELECT 1;" -q 2>/dev/null; then
    break
  fi
  echo "  Waiting for Postgres to stabilize ($i/15)..."
  sleep 1
done
kubectl exec -n "${NS}" postgres -c postgres -- \
  psql -U platform -d platform_dev -c "ALTER USER platform CREATEDB;" -q 2>/dev/null || true

# Create MinIO buckets (S55: MinIO serves HTTPS with self-signed cert)
kubectl exec -n "${NS}" minio -- sh -c '
  mc alias set local https://localhost:9000 platform devdevdev --insecure &&
  mc mb local/platform --ignore-existing --insecure &&
  mc mb local/platform-e2e --ignore-existing --insecure
'

# Deploy registry proxy DaemonSet if backend host/port are set
if [[ -n "${REGISTRY_BACKEND_HOST:-}" && -n "${REGISTRY_BACKEND_PORT:-}" ]]; then
  REGISTRY_NODE_PORT="${REGISTRY_NODE_PORT:-5000}"
  echo "==> Deploying registry proxy DaemonSet (${REGISTRY_BACKEND_HOST}:${REGISTRY_BACKEND_PORT} → hostPort:${REGISTRY_NODE_PORT})"
  cat <<DAEMONSET | kubectl apply -n "${NS}" -f -
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: registry-proxy
  labels:
    app: registry-proxy
spec:
  selector:
    matchLabels:
      app: registry-proxy
  template:
    metadata:
      labels:
        app: registry-proxy
    spec:
      tolerations:
        - operator: Exists
      containers:
        - name: socat
          image: alpine/socat:1.8.0.1
          args:
            - "TCP-LISTEN:${REGISTRY_NODE_PORT},fork,reuseaddr"
            - "TCP:${REGISTRY_BACKEND_HOST}:${REGISTRY_BACKEND_PORT}"
          ports:
            - containerPort: ${REGISTRY_NODE_PORT}
              hostPort: ${REGISTRY_NODE_PORT}
              protocol: TCP
          resources:
            requests:
              cpu: 10m
              memory: 16Mi
            limits:
              memory: 32Mi
DAEMONSET
  kubectl rollout status -n "${NS}" daemonset/registry-proxy --timeout=30s 2>/dev/null || true
fi

echo "  Services ready in namespace: ${NS}"
