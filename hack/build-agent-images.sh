#!/usr/bin/env bash
# build-agent-images.sh — Build seed images + cross-compiled agent-runner binaries.
#
# Builds OCI tarballs for platform-runner and platform-runner-bare, plus
# cross-compiled agent-runner binaries for linux/amd64 and linux/arm64.
# All outputs are cached by source checksum and worktree-scoped.
#
# Usage:
#   bash hack/build-agent-images.sh          # build everything
#   bash hack/build-agent-images.sh --force  # rebuild even if cached

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

FORCE=false
if [[ "${1:-}" == "--force" ]]; then
  FORCE=true
fi

WORKTREE="$(bash "${SCRIPT_DIR}/detect-worktree.sh")"
SEED_DIR="/tmp/platform-e2e/seed-images"
RUNNER_DIR="/tmp/platform-e2e/${WORKTREE}/agent-runner"
mkdir -p "${SEED_DIR}" "${RUNNER_DIR}"

echo "==> Building agent images (worktree: ${WORKTREE})"

# ── Ensure buildx builder ────────────────────────────────────────────────
if ! docker buildx inspect platform-oci &>/dev/null; then
  docker buildx create --name platform-oci --driver docker-container --bootstrap 2>/dev/null || true
fi

# ── build_seed_image <name> <dockerfile> [extra_checksum_dirs...] ────────
build_seed_image() {
  local name="$1" dockerfile="$2"
  shift 2
  local tarball="${SEED_DIR}/${name}.tar"
  local checksum_file="${SEED_DIR}/.${name}-checksum"
  local current_checksum
  current_checksum=$(
    { shasum -a 256 "${dockerfile}"
      for dir in "$@"; do
        find "${PROJECT_DIR}/${dir}" -type f -exec shasum -a 256 {} +
      done
    } | sort | shasum -a 256 | awk '{print $1}'
  )

  if [[ "$FORCE" == "false" && -f "${tarball}" && -f "${checksum_file}" && \
        "$(cat "${checksum_file}")" == "${current_checksum}" ]]; then
    echo "  ${name}: cached"
    return
  fi

  echo "  ${name}: building..."
  docker buildx build \
    --builder platform-oci \
    --file "${dockerfile}" \
    --output "type=oci,dest=${tarball}" \
    "${PROJECT_DIR}"
  echo "${current_checksum}" > "${checksum_file}"
}

# ── Seed images ──────────────────────────────────────────────────────────
echo "  Seed images:"
build_seed_image "platform-runner" "${PROJECT_DIR}/docker/Dockerfile.platform-runner" \
  "cli/agent-runner/src" "mcp"
build_seed_image "platform-runner-bare" "${PROJECT_DIR}/docker/Dockerfile.platform-runner-bare"

# ── Agent-runner cross-compiled binaries (worktree-scoped) ───────────────
echo "  Agent-runner binaries (→ ${RUNNER_DIR}):"
RUNNER_CHECKSUM_FILE="${RUNNER_DIR}/.checksum"
RUNNER_CURRENT_CHECKSUM=$(find "${PROJECT_DIR}/cli/agent-runner/src" -name '*.rs' -exec shasum -a 256 {} + | sort | shasum -a 256 | awk '{print $1}')

if [[ "$FORCE" == "false" && -f "${RUNNER_DIR}/arm64" && -f "${RUNNER_DIR}/amd64" && \
      -f "${RUNNER_CHECKSUM_FILE}" && "$(cat "${RUNNER_CHECKSUM_FILE}")" == "${RUNNER_CURRENT_CHECKSUM}" ]]; then
  echo "    cached"
else
  echo "    building..."
  cd "${PROJECT_DIR}" && just cli-cross "${RUNNER_DIR}"
  echo "${RUNNER_CURRENT_CHECKSUM}" > "${RUNNER_CHECKSUM_FILE}"
fi

echo ""
echo "==> Done"
echo "  Seed images: ${SEED_DIR}/"
ls -lh "${SEED_DIR}"/*.tar 2>/dev/null | awk '{print "    " $NF " (" $5 ")"}'
echo "  Agent-runner: ${RUNNER_DIR}/"
ls -lh "${RUNNER_DIR}/arm64" "${RUNNER_DIR}/amd64" 2>/dev/null | awk '{print "    " $NF " (" $5 ")"}'
