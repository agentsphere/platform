#!/usr/bin/env bash
# setup.sh — One-time bootstrap for the k3s dev environment on a VPS.
#
# Usage:
#   bash hack/k3s/setup.sh                    # default namespace: platform-dev
#   bash hack/k3s/setup.sh platform-dev-2     # custom namespace (second instance)
#
# Prerequisites:
#   - Linux x86_64 VPS with root access
#   - Internet connectivity (pulls k3s, container images)

set -euo pipefail

NS="${1:-platform-dev}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# ── Install k3s if not present ─────────────────────────────────────
if ! command -v k3s &>/dev/null; then
  echo "==> Installing k3s (no Traefik)"
  curl -sfL https://get.k3s.io | INSTALL_K3S_EXEC="--disable traefik" sh -
  echo "  Waiting for k3s to be ready..."
  sleep 5
  kubectl wait --for=condition=Ready node --all --timeout=60s
fi

export KUBECONFIG=/etc/rancher/k3s/k3s.yaml

# ── Shared directory for E2E git repos ─────────────────────────────
mkdir -p /tmp/platform-e2e

# ── Apply manifests ────────────────────────────────────────────────
echo "==> Deploying dev environment in namespace: ${NS}"
if [[ "$NS" != "platform-dev" ]]; then
  sed "s/platform-dev/${NS}/g" "${SCRIPT_DIR}/dev-env.yaml" | kubectl apply -f -
else
  kubectl apply -f "${SCRIPT_DIR}/dev-env.yaml"
fi

# ── Wait for services ──────────────────────────────────────────────
echo "==> Waiting for services..."
kubectl wait -n "${NS}" --for=condition=Available deploy/postgres --timeout=120s
kubectl wait -n "${NS}" --for=condition=Available deploy/valkey --timeout=60s
kubectl wait -n "${NS}" --for=condition=Available deploy/minio --timeout=60s

# ── Post-deploy: CREATEDB + MinIO buckets ──────────────────────────
echo "==> Post-deploy setup"

# Grant CREATEDB (required by #[sqlx::test] macro which creates per-test DBs)
kubectl exec -n "${NS}" deploy/postgres -- \
  psql -U postgres -c "ALTER USER platform CREATEDB;" -q

# Create MinIO buckets
sleep 2
kubectl exec -n "${NS}" deploy/minio -- sh -c \
  'mc alias set local http://localhost:9000 platform devdevdev 2>/dev/null; \
   mc mb local/platform --ignore-existing; \
   mc mb local/platform-e2e --ignore-existing'

# ── Claude Code credentials ────────────────────────────────────────
if ! kubectl get secret claude-credentials -n "${NS}" &>/dev/null; then
  echo ""
  echo "Enter your ANTHROPIC_API_KEY:"
  read -rs API_KEY
  kubectl create secret generic claude-credentials -n "${NS}" \
    --from-literal=api-key="${API_KEY}"
  echo "  Secret created."
fi

# ── Wait for dev pod ───────────────────────────────────────────────
echo "==> Waiting for dev pod..."
kubectl wait -n "${NS}" pod/dev --for=condition=Ready --timeout=300s

echo ""
echo "================================================================"
echo "Dev environment ready in namespace: ${NS}"
echo ""
echo "  kubectl exec -it -n ${NS} dev -- bash"
echo ""
echo "Inside the pod:"
echo "  cd /workspace"
echo "  git clone <your-repo-url> platform"
echo "  cd platform"
echo "  just test-unit        # sanity check (no infra needed)"
echo "  just test-integration # uses ephemeral K8s namespaces"
echo "  just ci-full          # full CI gate"
echo "================================================================"
