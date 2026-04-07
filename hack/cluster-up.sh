#!/usr/bin/env bash
# cluster-up.sh — Create the dev cluster.
#
# Currently uses Kind. Future: detect platform and use k0s on Linux.
# Writes kubeconfig to ~/.kube/platform (unified path for all backends).

set -euo pipefail

CLUSTER_NAME="platform"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
KUBECONFIG_FILE="${HOME}/.kube/platform"

# Create cluster if it doesn't exist
if ! kind get clusters 2>/dev/null | grep -q "^${CLUSTER_NAME}$"; then
  kind create cluster --name "$CLUSTER_NAME" --config "${SCRIPT_DIR}/kind-config.yaml"
fi

# Inject Docker Hub credentials into Kind node (avoids rate limits).
inject_dockerhub_auth() {
  local node="${CLUSTER_NAME}-control-plane"
  local creds_dir="/root/.config/containerd/creds.d/docker.io"
  local auth=""
  if command -v docker-credential-osxkeychain &>/dev/null; then
    auth=$(echo "https://index.docker.io/v1/" | docker-credential-osxkeychain get 2>/dev/null \
      | python3 -c "import sys,json,base64; d=json.load(sys.stdin); print(base64.b64encode(f'{d[\"Username\"]}:{d[\"Secret\"]}'.encode()).decode())" 2>/dev/null || true)
  fi
  if [[ -z "$auth" ]] && [[ -f "${HOME}/.docker/config.json" ]]; then
    auth=$(python3 -c "import json; c=json.load(open('${HOME}/.docker/config.json')); print(c.get('auths',{}).get('https://index.docker.io/v1/',{}).get('auth',''))" 2>/dev/null || true)
  fi
  if [[ -n "$auth" ]]; then
    docker exec "$node" mkdir -p "$creds_dir"
    docker exec "$node" sh -c "echo '[host.\"https://registry-1.docker.io\"]
  capabilities = [\"pull\", \"resolve\"]
  [host.\"https://registry-1.docker.io\".header]
    authorization = \"Basic ${auth}\"' > ${creds_dir}/hosts.toml"
    echo "Docker Hub auth injected into Kind node."
  else
    echo "Warning: no Docker Hub credentials found — pulls may be rate-limited."
  fi
}
inject_dockerhub_auth

# Export kubeconfig to unified path + merge into default ~/.kube/config
kind get kubeconfig --name "$CLUSTER_NAME" > "$KUBECONFIG_FILE"
KUBECONFIG="${HOME}/.kube/config" kind export kubeconfig --name "$CLUSTER_NAME"
export KUBECONFIG="$KUBECONFIG_FILE"

# Install CNPG operator (cluster-wide, needed by PG clusters)
helm repo add cnpg https://cloudnative-pg.github.io/charts --force-update
helm upgrade --install cnpg cnpg/cloudnative-pg -n cnpg-system --create-namespace --wait

# Install Gateway API CRDs (standalone, no Envoy Gateway)
kubectl apply -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.2.1/standard-install.yaml

# Ensure platform namespace exists before creating Gateway resource in it
kubectl create namespace platform --dry-run=client -o yaml | kubectl apply -f -

# Create GatewayClass + shared platform Gateway (platform-proxy --gateway is the controller)
cat <<'EOF' | kubectl apply -f -
apiVersion: gateway.networking.k8s.io/v1
kind: GatewayClass
metadata:
  name: platform
spec:
  controllerName: io.platform/gateway-controller
---
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: platform-gateway
  namespace: platform
  labels:
    platform.io/managed-by: platform
spec:
  gatewayClassName: platform
  listeners:
    - name: http
      protocol: HTTP
      port: 80
      allowedRoutes:
        namespaces:
          from: Selector
          selector:
            matchLabels:
              platform.io/managed-by: platform
EOF

# Create shared temp directory for e2e test repos (mounted via extraMounts)
mkdir -p /tmp/platform-e2e

echo ""
echo "Cluster ready (Kind)."
echo "  KUBECONFIG: ${KUBECONFIG_FILE}"
echo "  Next: just dev-up"
