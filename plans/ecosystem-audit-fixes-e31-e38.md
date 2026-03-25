# Plan: Ecosystem Audit Fixes E31–E38

## Context

Third and final batch of ecosystem audit fixes. E34 was already addressed in the E1-E15 batch (test RBAC now includes `apps`, `gateway.networking.k8s.io`, `serviceaccounts`, `rbac`, `networkpolicies`, `ingresses`). This batch covers the remaining 7 medium findings across Docker, Justfile, CI/CD, security tooling, and templates.

All items are small, independent, low-risk changes. Single PR.

---

## PR 1: Pin versions, fix CI, clean up templates

Addresses: **E31** (kubectl checksum), **E32** (agent image tag collision), **E33** (test image pins), **E34** (already done), **E35** (release CI gate), **E36** (gitleaks allowlist), **E37** (staging docs), **E38** (dead template)

- [x] Implementation complete
- [x] All verifications pass (no floating tags, distinct image names, workflow_run trigger, regex gitleaks, staging docs removed, dead template deleted)

### E31: Pin kubectl and verify checksum in Docker images

**Files:** `docker/Dockerfile.platform-runner`, `docker/Dockerfile.dev-pod`

**Dockerfile.platform-runner** — replace the kubectl download (lines 32-34 inside the `RUN` chain):
```dockerfile
# Before:
  && ARCH=$(dpkg --print-architecture) \
  && curl -sLO "https://dl.k8s.io/release/$(curl -sL https://dl.k8s.io/release/stable.txt)/bin/linux/${ARCH}/kubectl" \
  && chmod +x kubectl && mv kubectl /usr/local/bin/kubectl

# After:
  && ARCH=$(dpkg --print-architecture) \
  && KUBECTL_VERSION=v1.31.4 \
  && curl -sfSL "https://dl.k8s.io/release/${KUBECTL_VERSION}/bin/linux/${ARCH}/kubectl" -o kubectl \
  && curl -sfSL "https://dl.k8s.io/release/${KUBECTL_VERSION}/bin/linux/${ARCH}/kubectl.sha256" -o kubectl.sha256 \
  && echo "$(cat kubectl.sha256)  kubectl" | sha256sum -c - \
  && chmod +x kubectl && mv kubectl /usr/local/bin/kubectl && rm -f kubectl.sha256
```

**Dockerfile.dev-pod** — replace the kubectl download (lines 33-35). Also fix the hardcoded `amd64` (E54):
```dockerfile
# Before:
RUN curl -fsSL "https://dl.k8s.io/release/$(curl -sL https://dl.k8s.io/release/stable.txt)/bin/linux/amd64/kubectl" \
    -o /usr/local/bin/kubectl && chmod +x /usr/local/bin/kubectl

# After:
RUN ARCH=$(dpkg --print-architecture) \
  && KUBECTL_VERSION=v1.31.4 \
  && curl -sfSL "https://dl.k8s.io/release/${KUBECTL_VERSION}/bin/linux/${ARCH}/kubectl" -o /tmp/kubectl \
  && curl -sfSL "https://dl.k8s.io/release/${KUBECTL_VERSION}/bin/linux/${ARCH}/kubectl.sha256" -o /tmp/kubectl.sha256 \
  && echo "$(cat /tmp/kubectl.sha256)  /tmp/kubectl" | sha256sum -c - \
  && mv /tmp/kubectl /usr/local/bin/kubectl && chmod +x /usr/local/bin/kubectl \
  && rm -f /tmp/kubectl.sha256
```

**Note:** Look up the current stable kubectl version at implementation time. v1.31.4 is a placeholder.

### E32: Use distinct tags for full and bare runner images

**File:** `Justfile`

Change the `agent-image-bare` recipe to use a different tag:
```just
# Before:
agent-image-bare registry_url="${PLATFORM_REGISTRY_URL:-localhost:8080}":
    docker build -f docker/Dockerfile.platform-runner-bare -t {{registry_url}}/platform-runner:latest .
    docker push {{registry_url}}/platform-runner:latest

# After:
agent-image-bare registry_url="${PLATFORM_REGISTRY_URL:-localhost:8080}":
    docker build -f docker/Dockerfile.platform-runner-bare -t {{registry_url}}/platform-runner-bare:latest .
    docker push {{registry_url}}/platform-runner-bare:latest
```

Also verify that any code referencing the bare runner image tag is updated. Check `src/agent/provider.rs` for image resolution logic — the bare image is used when `auto_setup` is enabled.

### E33: Pin test manifest image tags

**File:** `hack/test-manifests/minio.yaml:12`
```yaml
# Before:
image: minio/minio:latest
# After:
image: minio/minio:RELEASE.2025-02-28T09-55-16Z
```

**File:** `hack/deploy-services.sh:73-74`
```bash
# Before:
image: alpine/socat:latest
# After:
image: alpine/socat:1.8.0.1
```

**Note:** Look up current stable versions at implementation time.

### E34: Already done

Test RBAC (`hack/test-manifests/rbac.yaml`) was fully addressed in the E1-E15 batch. Gateway API, apps, serviceaccounts, RBAC, networkpolicies, and ingresses rules are all present. No further changes needed.

### E35: Gate release workflow behind CI passing

**File:** `.github/workflows/release.yaml`

Change the trigger from `push` to `workflow_run` so it only runs after CI passes:

```yaml
# Before:
on:
  push:
    branches: [main]

# After:
on:
  workflow_run:
    workflows: ["CI"]
    branches: [main]
    types: [completed]
```

Add a condition to the first job to only run if CI succeeded:

```yaml
jobs:
  build-binary:
    if: ${{ github.event.workflow_run.conclusion == 'success' }}
    strategy:
      ...
```

This ensures a broken commit that fails CI will not get a Docker image published.

### E36: Narrow gitleaks allowlist

**File:** `.gitleaks.toml`

Replace the blanket file allowlist with targeted regex patterns for the known dev-only secrets in `test-in-cluster.sh`:

```toml
# Before:
[allowlist]
paths = [
  "hack/test-in-cluster.sh",
]

# After:
[allowlist]
description = "Allow known dev-only test credentials"

[[allowlist.rules]]
description = "Dev-only PLATFORM_MASTER_KEY in test scripts"
regex = '''[0-9a-f]{64}'''
paths = ["hack/test-in-cluster\\.sh"]

[[allowlist.rules]]
description = "Dev-only database/minio credentials in test scripts and kustomize"
regex = '''devdevdev|dev@.*:5432'''
paths = ["hack/.*\\.sh", "deploy/base/.*\\.yaml"]
```

This is more targeted — only allows hex strings and known dev credentials in test scripts, rather than blanket-allowing any secret in the file.

### E37: Remove staging docs from git template CLAUDE.md

**File:** `src/git/templates/CLAUDE.md`

Per user note: dev agents should not decide whether to use staging — remove the entire staging promotion section.

Delete lines 525-530:
```markdown
### Staging Promotion

When `deploy.enable_staging: true`:
- Pipeline commits to `staging` branch first
- Promote to production: `POST /api/projects/{id}/promote-staging`
- Check status: `GET /api/projects/{id}/staging-status`
```

Also remove the commented `enable_staging` reference in the git template `platform.yaml` at `src/git/templates/platform.yaml:45`:
```yaml
#   enable_staging: false
```

### E38: Delete dead onboarding template

**File:** `src/onboarding/templates/platform.yaml`

Delete the entire file. It references `Dockerfile.canary` (deleted), `build-canary` step, and the old deployment model. Not referenced by any Rust code — only `platform_v0.1.yaml` and `platform_v0.2.yaml` are used by `demo_project.rs`.

### Test Outline

All changes are configuration/infrastructure — no unit or integration tests needed.

**Verification:**
- `grep "latest" docker/Dockerfile.platform-runner docker/Dockerfile.dev-pod hack/test-manifests/minio.yaml` — returns no hits for unpinned images
- `grep "platform-runner:latest" Justfile` — only appears in the `agent-image` recipe (not `agent-image-bare`)
- `.github/workflows/release.yaml` has `workflow_run` trigger
- `.gitleaks.toml` has regex-based allowlist rules (not blanket path)
- `src/onboarding/templates/platform.yaml` does not exist
- `grep "enable_staging" src/git/templates/CLAUDE.md` — returns nothing

---

## Summary

| Finding | Effort | Description |
|---|---|---|
| E31 | Low | Pin kubectl v1.31.4 + sha256 verification in 2 Dockerfiles |
| E32 | Low | Rename bare runner tag to `platform-runner-bare:latest` |
| E33 | Low | Pin minio + socat to specific versions in test manifests |
| E34 | Done | Already fixed in E1-E15 batch |
| E35 | Low | Change release trigger to `workflow_run` gated on CI |
| E36 | Low | Replace blanket gitleaks allowlist with targeted regex rules |
| E37 | Low | Remove staging promotion section from git template CLAUDE.md |
| E38 | Low | Delete dead `src/onboarding/templates/platform.yaml` |

Single PR, all independent changes. No inter-dependencies.
