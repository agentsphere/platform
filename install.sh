#!/usr/bin/env bash
# AgentSphere Platform Installer
# Usage: curl -fsSL https://get.agentsphere.dev | sh
#    or: bash install.sh
set -euo pipefail

# ---------------------------------------------------------------------------
# Colors & helpers
# ---------------------------------------------------------------------------
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m'

info()  { echo -e "${BLUE}[INFO]${NC} $*"; }
ok()    { echo -e "${GREEN}[OK]${NC} $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC} $*"; }
err()   { echo -e "${RED}[ERROR]${NC} $*" >&2; }
fatal() { err "$@"; exit 1; }

# ---------------------------------------------------------------------------
# OS / arch detection
# ---------------------------------------------------------------------------
detect_os() {
  case "$(uname -s)" in
    Linux*)  OS="linux" ;;
    Darwin*) OS="darwin" ;;
    *)       fatal "Unsupported OS: $(uname -s). Linux and macOS are supported." ;;
  esac

  # WSL detection
  IS_WSL=false
  if [ "$OS" = "linux" ] && grep -qi microsoft /proc/version 2>/dev/null; then
    IS_WSL=true
  fi

  case "$(uname -m)" in
    x86_64|amd64)  ARCH="amd64" ;;
    aarch64|arm64) ARCH="arm64" ;;
    *)             fatal "Unsupported architecture: $(uname -m)" ;;
  esac

  info "Detected: OS=$OS ARCH=$ARCH WSL=$IS_WSL"
}

# ---------------------------------------------------------------------------
# Prerequisite checks
# ---------------------------------------------------------------------------
check_prereqs() {
  for cmd in curl git; do
    if ! command -v "$cmd" &>/dev/null; then
      fatal "'$cmd' is required but not found. Install it and re-run."
    fi
  done
  ok "Prerequisites: curl, git"
}

# ---------------------------------------------------------------------------
# kubectl
# ---------------------------------------------------------------------------
ensure_kubectl() {
  if command -v kubectl &>/dev/null; then
    ok "kubectl found: $(kubectl version --client --short 2>/dev/null || kubectl version --client 2>/dev/null | head -1)"
    return
  fi

  if [ "$OS" = "darwin" ]; then
    echo ""
    warn "kubectl not found on macOS."
    echo ""
    echo "  Install one of these (both include kubectl + Kubernetes):"
    echo ""
    echo "    Docker Desktop: https://www.docker.com/products/docker-desktop/"
    echo "    OrbStack:       https://orbstack.dev/"
    echo ""
    echo "  After installing, enable Kubernetes in settings, then re-run this script."
    echo ""
    exit 1
  fi

  # Linux / WSL: offer to install k0s
  echo ""
  info "kubectl not found. Would you like to install k0s (lightweight Kubernetes)?"
  read -rp "  Install k0s? [Y/n] " answer
  answer="${answer:-Y}"
  if [[ ! "$answer" =~ ^[Yy] ]]; then
    fatal "kubectl is required. Install Kubernetes and re-run."
  fi

  install_k0s
}

install_k0s() {
  info "Installing k0s..."
  curl -sSLf https://get.k0s.sh | sudo sh

  info "Installing k0s controller (single-node)..."
  sudo k0s install controller --single
  sudo k0s start

  info "Waiting for k0s API server..."
  local retries=30
  while [ $retries -gt 0 ]; do
    if sudo k0s kubectl get nodes &>/dev/null; then
      break
    fi
    sleep 2
    retries=$((retries - 1))
  done

  if [ $retries -eq 0 ]; then
    fatal "k0s API server did not become ready. Check: sudo k0s status"
  fi

  # Export kubeconfig
  local kubeconfig_dir="$HOME/.k0s"
  mkdir -p "$kubeconfig_dir"
  sudo k0s kubeconfig admin > "$kubeconfig_dir/kubeconfig"
  export KUBECONFIG="$kubeconfig_dir/kubeconfig"
  echo "export KUBECONFIG=$kubeconfig_dir/kubeconfig" >> "$HOME/.bashrc" 2>/dev/null || true

  # Install local-path-provisioner for default StorageClass
  info "Installing local-path-provisioner..."
  kubectl apply -f https://raw.githubusercontent.com/rancher/local-path-provisioner/v0.0.28/deploy/local-path-storage.yaml
  kubectl patch storageclass local-path -p '{"metadata":{"annotations":{"storageclass.kubernetes.io/is-default-class":"true"}}}'

  K0S_INSTALLED=true
  ok "k0s installed and running"
}

# ---------------------------------------------------------------------------
# Helm
# ---------------------------------------------------------------------------
ensure_helm() {
  if command -v helm &>/dev/null; then
    ok "helm found: $(helm version --short 2>/dev/null)"
    return
  fi

  info "Installing Helm..."
  curl -fsSL https://raw.githubusercontent.com/helm/helm/main/scripts/get-helm-3 | bash
  ok "Helm installed"
}

# ---------------------------------------------------------------------------
# Interactive prompts
# ---------------------------------------------------------------------------
K0S_INSTALLED=false
DOMAIN=""
SIZE="small"
SSH_ENABLED="false"

prompt_config() {
  echo ""
  echo -e "${BOLD}Configuration${NC}"
  echo ""

  # Domain
  read -rp "  Domain (leave empty for NodePort access): " DOMAIN

  # Size
  echo "  Size presets:"
  echo "    small  — 1 CPU, 1Gi RAM, 2 concurrent pipelines (default)"
  echo "    medium — 2 CPU, 4Gi RAM, 4 concurrent pipelines"
  echo "    large  — 4 CPU, 8Gi RAM, 8 concurrent pipelines"
  read -rp "  Size [small]: " SIZE
  SIZE="${SIZE:-small}"
  if [[ ! "$SIZE" =~ ^(small|medium|large)$ ]]; then
    warn "Invalid size '$SIZE', using 'small'"
    SIZE="small"
  fi

  # SSH
  read -rp "  Enable SSH git access? [y/N] " ssh_answer
  if [[ "$ssh_answer" =~ ^[Yy] ]]; then
    SSH_ENABLED="true"
  fi

  echo ""
  info "Config: domain=${DOMAIN:-<none>} size=$SIZE ssh=$SSH_ENABLED"
}

# ---------------------------------------------------------------------------
# Ingress + cert-manager (when domain is set + k0s installed by us)
# ---------------------------------------------------------------------------
install_ingress_stack() {
  if [ -z "$DOMAIN" ] || [ "$K0S_INSTALLED" != "true" ]; then
    return
  fi

  info "Installing Traefik ingress controller..."
  helm repo add traefik https://traefik.github.io/charts 2>/dev/null || true
  helm repo update traefik
  helm upgrade --install traefik traefik/traefik \
    -n traefik --create-namespace \
    --set ports.web.redirectTo.port=websecure \
    --wait --timeout 120s

  info "Installing cert-manager + Let's Encrypt..."
  helm repo add jetstack https://charts.jetstack.io 2>/dev/null || true
  helm repo update jetstack
  helm upgrade --install cert-manager jetstack/cert-manager \
    -n cert-manager --create-namespace \
    --set crds.enabled=true \
    --wait --timeout 120s

  # Create ClusterIssuer for Let's Encrypt
  kubectl apply -f - <<EOF
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata:
  name: letsencrypt-prod
spec:
  acme:
    server: https://acme-v02.api.letsencrypt.org/directory
    email: admin@${DOMAIN}
    privateKeySecretRef:
      name: letsencrypt-prod-key
    solvers:
      - http01:
          ingress:
            class: traefik
EOF

  ok "Ingress stack installed"
}

# ---------------------------------------------------------------------------
# Install platform via Helm
# ---------------------------------------------------------------------------
install_platform() {
  info "Installing AgentSphere Platform..."

  # Add helm repo (placeholder — replace with real URL when published)
  # For now, install from local chart if available
  local chart_source="agentsphere/platform"
  local script_dir
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

  if [ -d "$script_dir/helm/platform" ]; then
    chart_source="$script_dir/helm/platform"
    info "Using local chart at $chart_source"
    # Build dependencies
    helm dependency build "$chart_source" 2>/dev/null || true
  else
    helm repo add agentsphere https://charts.agentsphere.dev 2>/dev/null || true
    helm repo update agentsphere 2>/dev/null || true
  fi

  # Build helm set args
  local -a helm_args=(
    upgrade --install platform "$chart_source"
    -n platform --create-namespace
  )

  # Size preset
  if [ -f "$script_dir/helm/platform/values-${SIZE}.yaml" ]; then
    helm_args+=(-f "$script_dir/helm/platform/values-${SIZE}.yaml")
  fi

  # Domain / ingress
  if [ -n "$DOMAIN" ]; then
    helm_args+=(
      --set "ingress.enabled=true"
      --set "ingress.hosts[0].host=$DOMAIN"
      --set "ingress.hosts[0].paths[0].path=/"
      --set "ingress.hosts[0].paths[0].pathType=Prefix"
      --set "ingress.tls[0].secretName=platform-tls"
      --set "ingress.tls[0].hosts[0]=$DOMAIN"
      --set "platform.env.secureCookies=true"
    )
  fi

  # SSH
  if [ "$SSH_ENABLED" = "true" ]; then
    helm_args+=(--set "platform.ssh.enabled=true")
  fi

  helm "${helm_args[@]}" --wait --timeout 300s

  ok "Platform installed"
}

# ---------------------------------------------------------------------------
# Post-install: extract setup token
# ---------------------------------------------------------------------------
post_install() {
  info "Waiting for platform pod to be ready..."
  kubectl rollout status -n platform deploy/platform --timeout=300s 2>/dev/null || true

  # Wait a few seconds for logs to appear
  sleep 5

  # Extract setup token from pod logs
  local token=""
  local pod_name
  pod_name=$(kubectl get pods -n platform -l app.kubernetes.io/name=platform -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || echo "")

  if [ -n "$pod_name" ]; then
    token=$(kubectl logs -n platform "$pod_name" 2>/dev/null | grep -oE '[a-f0-9]{64}' | head -1 || echo "")
  fi

  # Determine URL
  local url=""
  if [ -n "$DOMAIN" ]; then
    url="https://$DOMAIN/setup"
  else
    local node_port
    node_port=$(kubectl get svc -n platform platform-nodeport -o jsonpath='{.spec.ports[0].nodePort}' 2>/dev/null || echo "")
    local node_ip
    node_ip=$(kubectl get nodes -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}' 2>/dev/null || echo "localhost")
    if [ -n "$node_port" ]; then
      url="http://$node_ip:$node_port/setup"
    else
      url="http://localhost:8080/setup (use kubectl port-forward -n platform svc/platform 8080:8080)"
    fi
  fi

  echo ""
  echo -e "${GREEN}${BOLD}"
  echo "  ┌─────────────────────────────────────────────────┐"
  echo "  │  Platform is ready!                             │"
  echo "  │                                                 │"
  if [ -n "$url" ]; then
  echo "  │  URL:   $url"
  fi
  if [ -n "$token" ]; then
  echo "  │  Token: ${token:0:16}...${token: -8}"
  else
  echo "  │  Token: (check pod logs if not shown)           │"
  echo "  │         kubectl logs -n platform -l app.kubernetes.io/name=platform"
  fi
  echo "  │                                                 │"
  echo "  │  Open the URL and paste the token to            │"
  echo "  │  create your admin account.                     │"
  echo "  └─────────────────────────────────────────────────┘"
  echo -e "${NC}"

  if [ -n "$token" ]; then
    info "Full token: $token"
  fi
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
main() {
  echo ""
  echo -e "${BOLD}AgentSphere Platform Installer${NC}"
  echo ""

  detect_os
  check_prereqs
  ensure_kubectl
  ensure_helm
  prompt_config
  install_ingress_stack
  install_platform
  post_install

  echo ""
  ok "Installation complete!"
}

main "$@"
