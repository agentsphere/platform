#!/usr/bin/env bash
# deploy-services.sh — Deploy PostgreSQL, Valkey, and MinIO into a given namespace.
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

echo "==> Deploying services into namespace: ${NS}"
kubectl apply -n "${NS}" -f "${SCRIPT_DIR}/test-manifests/postgres.yaml"
kubectl apply -n "${NS}" -f "${SCRIPT_DIR}/test-manifests/valkey.yaml"
kubectl apply -n "${NS}" -f "${SCRIPT_DIR}/test-manifests/minio.yaml"
kubectl apply -n "${NS}" -f "${SCRIPT_DIR}/test-manifests/preview-proxy.yaml"

echo "==> Waiting for services to be ready"
kubectl wait -n "${NS}" --for=condition=Ready pod/postgres --timeout=60s
kubectl wait -n "${NS}" --for=condition=Ready pod/valkey --timeout=30s
kubectl wait -n "${NS}" --for=condition=Ready pod/minio --timeout=30s
kubectl wait -n "${NS}" --for=condition=Ready pod/preview-proxy --timeout=30s
echo "  All services ready"

# Grant CREATEDB (required by sqlx::test macro)
echo "==> Post-deploy setup"
kubectl exec -n "${NS}" postgres -- \
  psql -U platform -d platform_dev -c "SELECT 1;" -q
kubectl exec -n "${NS}" postgres -c postgres -- \
  psql -U postgres -c "ALTER USER platform CREATEDB;" -q 2>/dev/null || true

# Create MinIO buckets
kubectl exec -n "${NS}" minio -- sh -c '
  mc alias set local http://localhost:9000 platform devdevdev &&
  mc mb local/platform --ignore-existing &&
  mc mb local/platform-e2e --ignore-existing
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
