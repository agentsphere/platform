#!/usr/bin/env bash
# cluster-down.sh — Tear down the dev cluster.
#
# Currently uses Kind. Future: detect platform and use k0s on Linux.

set -euo pipefail

kind delete cluster --name platform
rm -f "${HOME}/.kube/platform"
# Clean up legacy kubeconfig + registry container
rm -f "${HOME}/.kube/kind-platform"
docker rm -f kind-registry 2>/dev/null || true
# Remove ephemeral test/build artifacts so cluster-up starts truly fresh
rm -rf /tmp/platform-e2e

echo "Cluster removed."
