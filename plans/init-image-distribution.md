# Plan: Replace HTTP Download with Seeded Init Container Images

## Problem

Currently, proxy and agent-runner binaries are distributed to pods via HTTP downloads from the platform API (`GET /api/downloads/platform-proxy`, `GET /api/downloads/agent-runner`). This introduces:
- Runtime dependency on platform API being reachable during pod startup
- 9.4 MB download on every pod creation (no node-level caching)
- Auth token complexity (K8s Secret injection for Bearer token)
- curl/wget dependency in init container images

## Solution

Create two minimal seed images containing pre-built binaries. These get seeded to the platform registry on startup (using existing `registry::seed::seed_all()` infrastructure) and used as init containers that `cp` binaries to shared emptyDir volumes.

### New Images

1. **`platform-proxy-init`** — proxy binary only (~14 MB with busybox base)
   - Contains: `/platform-proxy` (9.4 MB release binary)
   - Base: `busybox:stable` (~4 MB, provides sh/cp/chmod)
   - Used by: deployer init containers for deployed workloads
   - Replaces: curl-based download in `src/deployer/applier.rs`

2. **`platform-tools-init`** — agent-runner + proxy binaries (~25 MB with busybox base)
   - Contains: `/agent-runner` + `/platform-proxy`
   - Base: `busybox:stable`
   - Used by: agent pod init containers
   - Replaces: download step in `src/agent/claude_code/pod.rs` setup-tools

### Init Container Pattern Change

Before (curl download):
```yaml
initContainers:
- name: proxy-init
  image: platform-runner-bare:latest
  command: ["sh", "-c"]
  args: ["curl -sf ${PLATFORM_API_URL}/api/downloads/platform-proxy?arch=$(uname -m) -o /proxy/platform-proxy && chmod +x /proxy/platform-proxy"]
```

After (cp from image):
```yaml
initContainers:
- name: proxy-init
  image: {registry}/platform-proxy-init:v1
  command: ["sh", "-c", "cp /platform-proxy /shared/platform-proxy && chmod +x /shared/platform-proxy"]
```

### Benefits
- **No runtime API dependency** — pod startup doesn't fail if platform is temporarily down
- **Node-level caching** — K8s kubelet caches images; second pod on same node is instant
- **No auth complexity** — image pull uses existing imagePullSecrets, no separate API token
- **Atomic** — image pull either succeeds or fails, no partial/corrupt downloads
- **Simpler init containers** — no curl/wget/arch-detection scripts

## Files to Change

1. **`docker/Dockerfile.platform-proxy-init`** (new) — multi-arch Dockerfile for proxy-only image
2. **`docker/Dockerfile.platform-tools-init`** (new) — multi-arch Dockerfile for agent-runner + proxy image
3. **`hack/build-agent-images.sh`** — add builds for both new images, output OCI tarballs to seed-images/
4. **`src/deployer/applier.rs`** — replace curl script with `cp` from `platform-proxy-init` image
5. **`src/agent/claude_code/pod.rs`** — replace agent-runner download with `cp` from `platform-tools-init` image
6. **`src/config.rs`** — new config fields for image names (`platform_proxy_init_image`, `platform_tools_init_image`)

## Notes

- The download API endpoints (`/api/downloads/platform-proxy`, `/api/downloads/agent-runner`) stay for CLI/manual use
- `busybox:stable` chosen over `FROM scratch` because scratch has no shell — can't run `cp`/`chmod`
- Both images seeded on startup via existing `registry::seed::seed_all()` — no new seeding code needed
- Multi-arch support: build separate amd64/arm64 layers, create OCI image index (same pattern as platform-runner)
