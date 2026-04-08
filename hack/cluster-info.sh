#!/usr/bin/env bash
# cluster-info.sh — Shared helper for node IP / container discovery.
#
# Auto-detects cluster backend: Kind (current), k0s (future on Linux).
# Exports: NODE_IP, NODE_CONTAINER, KUBECONFIG_PATH, PLATFORM_HOST
# Also defines: find_free_node_port()
#
# Usage: source "$(dirname "$0")/cluster-info.sh"

set -euo pipefail

KUBECONFIG_PATH="${HOME}/.kube/platform"

# ── Detect cluster backend ──────────────────────────────────────────────
if docker ps --format '{{.Names}}' 2>/dev/null | grep -q '^platform-control-plane$'; then
  # Kind
  NODE_CONTAINER="platform-control-plane"
elif docker ps --format '{{.Names}}' 2>/dev/null | grep -q '^platform-k0s$'; then
  # k0s-in-Docker (future, Linux only)
  NODE_CONTAINER="platform-k0s"
else
  echo "ERROR: No cluster container found. Run: just cluster-up"
  exit 1
fi

export KUBECONFIG="${KUBECONFIG_PATH}"

# Refresh kubeconfig if Kind (API server port changes on Docker restart)
if [[ "$NODE_CONTAINER" == "platform-control-plane" ]] && command -v kind &>/dev/null; then
  kind get kubeconfig --name platform > "$KUBECONFIG_PATH" 2>/dev/null || true
fi

if [[ ! -f "$KUBECONFIG" ]]; then
  echo "ERROR: Kubeconfig not found at ${KUBECONFIG}"
  exit 1
fi

# ── Get node IP ─────────────────────────────────────────────────────────
NODE_IP=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "${NODE_CONTAINER}")
if [[ -z "$NODE_IP" ]]; then
  echo "ERROR: Could not determine node IP for ${NODE_CONTAINER}"
  exit 1
fi

# ── Detect host address (for in-pod → host communication) ──────────────
if [[ "$(uname)" == "Darwin" ]]; then
  PLATFORM_HOST="host.docker.internal"
else
  PLATFORM_HOST=$(docker network inspect kind \
    -f '{{range .IPAM.Config}}{{.Gateway}}{{end}}' 2>/dev/null || echo "172.18.0.1")
fi

# ── Find free port inside the cluster node (in K8s NodePort range) ──────
find_free_node_port() {
  docker exec "${NODE_CONTAINER}" \
    python3 -c "
import socket, random
while True:
    port = random.randint(30000, 32767)
    s = socket.socket()
    try:
        s.bind(('', port))
        print(port)
        s.close()
        break
    except OSError:
        s.close()
"
}

export NODE_IP NODE_CONTAINER KUBECONFIG_PATH PLATFORM_HOST
