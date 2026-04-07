#!/usr/bin/env bash
# Build platform-proxy for linux/amd64 and linux/arm64 (dev/test use).
# Outputs go to /tmp/platform-e2e/{worktree}/proxy/{amd64,arm64}.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
WORKTREE="$(bash "${SCRIPT_DIR}/detect-worktree.sh")"
PROXY_DIR="/tmp/platform-e2e/${WORKTREE}/proxy"
mkdir -p "${PROXY_DIR}"

HOST_ARCH="$(uname -m)"

build_for_target() {
  local target="$1" dest="$2"
  if command -v cross &>/dev/null; then
    echo "    cross build (${target})"
    SQLX_OFFLINE=true cross build --bin platform-proxy --release --target "${target}"
    cp "${PROJECT_DIR}/target/${target}/release/platform-proxy" "${PROXY_DIR}/${dest}"
  else
    echo "    native build (no cross — ${dest} only works if host matches)"
    SQLX_OFFLINE=true cargo build --bin platform-proxy --release
    cp "${PROJECT_DIR}/target/release/platform-proxy" "${PROXY_DIR}/${dest}"
  fi
  chmod +x "${PROXY_DIR}/${dest}"
}

echo "==> Building platform-proxy binaries"

# Always build for host architecture
case "${HOST_ARCH}" in
  x86_64)
    build_for_target "x86_64-unknown-linux-musl" "amd64"
    # Build arm64 if cross is available
    if command -v cross &>/dev/null; then
      build_for_target "aarch64-unknown-linux-musl" "arm64"
    else
      # Copy host binary as fallback (won't run on arm64 but avoids missing file)
      cp "${PROXY_DIR}/amd64" "${PROXY_DIR}/arm64" 2>/dev/null || true
    fi
    ;;
  aarch64|arm64)
    build_for_target "aarch64-unknown-linux-musl" "arm64"
    # Build amd64 if cross is available
    if command -v cross &>/dev/null; then
      build_for_target "x86_64-unknown-linux-musl" "amd64"
    else
      cp "${PROXY_DIR}/arm64" "${PROXY_DIR}/amd64" 2>/dev/null || true
    fi
    ;;
  *)
    echo "Unsupported arch: ${HOST_ARCH}"; exit 1
    ;;
esac

# Create a `platform-proxy` symlink for the architecture that Kind nodes use.
# Kind on macOS with Apple Silicon uses amd64 (Rosetta) by default.
# The test manifests reference /proxy/platform-proxy (not /proxy/amd64).
CLUSTER_ARCH="amd64"
if docker exec platform-control-plane uname -m 2>/dev/null | grep -q aarch64; then
  CLUSTER_ARCH="arm64"
fi
ln -sf "${CLUSTER_ARCH}" "${PROXY_DIR}/platform-proxy"
echo "  Binaries ready: ${PROXY_DIR}/{amd64,arm64,platform-proxy → ${CLUSTER_ARCH}}"
