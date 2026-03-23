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

# Export kubeconfig to unified path + merge into default ~/.kube/config
kind get kubeconfig --name "$CLUSTER_NAME" > "$KUBECONFIG_FILE"
KUBECONFIG="${HOME}/.kube/config" kind export kubeconfig --name "$CLUSTER_NAME"
export KUBECONFIG="$KUBECONFIG_FILE"

# Install CNPG operator (cluster-wide, needed by PG clusters)
helm repo add cnpg https://cloudnative-pg.github.io/charts --force-update
helm upgrade --install cnpg cnpg/cloudnative-pg -n cnpg-system --create-namespace --wait

# Create shared temp directory for e2e test repos (mounted via extraMounts)
mkdir -p /tmp/platform-e2e

echo ""
echo "Cluster ready (Kind)."
echo "  KUBECONFIG: ${KUBECONFIG_FILE}"
echo "  Next: just dev-up"
