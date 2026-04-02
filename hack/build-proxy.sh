#!/usr/bin/env bash
# Build platform-proxy for the current architecture (dev/test use).
# For cross-compilation, use: cross build --bin platform-proxy --release --target x86_64-unknown-linux-musl
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
WORKTREE="$(bash "${SCRIPT_DIR}/detect-worktree.sh")"
PROXY_DIR="/tmp/platform-e2e/${WORKTREE}/proxy"
mkdir -p "${PROXY_DIR}"

ARCH="$(uname -m)"
case "${ARCH}" in
  x86_64)  TARGET="x86_64-unknown-linux-musl"; DEST="amd64" ;;
  aarch64|arm64) TARGET="aarch64-unknown-linux-musl"; DEST="arm64" ;;
  *) echo "Unsupported arch: ${ARCH}"; exit 1 ;;
esac

echo "==> Building platform-proxy (${TARGET})"
if command -v cross &>/dev/null; then
  cross build --bin platform-proxy --release --target "${TARGET}"
  cp "${PROJECT_DIR}/target/${TARGET}/release/platform-proxy" "${PROXY_DIR}/${DEST}"
else
  echo "  'cross' not found, building native (won't run in Kind on different arch)"
  cargo build --bin platform-proxy --release
  cp "${PROJECT_DIR}/target/release/platform-proxy" "${PROXY_DIR}/${DEST}"
fi

chmod +x "${PROXY_DIR}/${DEST}"
echo "  Binary ready: ${PROXY_DIR}/${DEST}"
