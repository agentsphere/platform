#!/usr/bin/env bash
# Build platform-proxy for linux/amd64 and linux/arm64 (dev/test use).
# Uses Docker for cross-compilation (same approach as agent-runner).
# Outputs go to /tmp/platform-e2e/{worktree}/proxy/{amd64,arm64,platform-proxy}.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
WORKTREE="$(bash "${SCRIPT_DIR}/detect-worktree.sh")"
PROXY_DIR="/tmp/platform-e2e/${WORKTREE}/proxy"
mkdir -p "${PROXY_DIR}"

echo "==> Building platform-proxy binaries (linux, via Docker)"

# Cross-compile inside a Docker container (produces Linux ELF binaries on any host)
docker run --rm \
  -v "${PROJECT_DIR}:/src" \
  -v "${PROXY_DIR}:/out" \
  -v "platform-cross-proxy-registry:/usr/local/cargo/registry" \
  -v "platform-cross-proxy-git:/usr/local/cargo/git" \
  -v "platform-cross-proxy-target:/src/target" \
  rust:1.88-slim-bookworm sh -c '
    apt-get update && apt-get install -y --no-install-recommends \
      gcc-aarch64-linux-gnu libc6-dev-arm64-cross \
      gcc-x86-64-linux-gnu libc6-dev-amd64-cross \
      pkg-config libssl-dev && \
    rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu && \
    cd /src && \
    SQLX_OFFLINE=true CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
      cargo build --bin platform-proxy --release --target aarch64-unknown-linux-gnu && \
    SQLX_OFFLINE=true CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
      cargo build --bin platform-proxy --release --target x86_64-unknown-linux-gnu && \
    cp target/aarch64-unknown-linux-gnu/release/platform-proxy /out/arm64 && \
    cp target/x86_64-unknown-linux-gnu/release/platform-proxy /out/amd64 && \
    chmod +x /out/arm64 /out/amd64'

# Create `platform-proxy` as a copy of the arch the Kind node uses.
# Kind on macOS typically runs amd64 (Rosetta). Check if arm64.
CLUSTER_ARCH="amd64"
if docker exec platform-control-plane uname -m 2>/dev/null | grep -q aarch64; then
  CLUSTER_ARCH="arm64"
fi
cp "${PROXY_DIR}/${CLUSTER_ARCH}" "${PROXY_DIR}/platform-proxy"
chmod +x "${PROXY_DIR}/platform-proxy"

echo "  Binaries ready: ${PROXY_DIR}/{amd64,arm64,platform-proxy (${CLUSTER_ARCH})}"
