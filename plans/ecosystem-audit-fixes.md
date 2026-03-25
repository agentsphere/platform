# Plan: Ecosystem Audit Fixes E1–E15

## Context

The ecosystem audit (`plans/ecosystem-audit-2026-03-24.md`) identified 3 critical and 12 high-severity findings across the platform's non-Rust components: UI, MCP servers, Helm chart, Docker images, infrastructure scripts, and cross-component integration seams. This plan addresses E1–E15 (excluding E8, which is an accepted design trade-off documented in `docs/design-decisions.md` DD-1).

The fixes span 6 different technology stacks (TypeScript, JavaScript, Helm/YAML, Dockerfile, Bash, Rust) but have zero inter-dependencies between most items. The plan groups them into 4 PRs by component boundary to minimize context-switching and enable parallel review.

## Design Principles

- **Fix the contract, not the convenience** — when a mismatch exists between platform API and a consumer, fix the consumer to match the API (not the other way around)
- **Pin everything** — reproducible builds require pinned versions with checksums where possible
- **Defense in depth** — XSS fix adds sanitization even though the backend also escapes; non-root Dockerfile even though K8s securityContext adds fsGroup
- **No Rust changes except E3b** — all other fixes are in ecosystem components. E3b touches one dead-code function in `src/agent/pubsub_bridge.rs`

---

## PR 1: Security — XSS fix, non-root Docker, pinned dependencies

Addresses: **E1** (stored XSS), **E7** (root container), **E9** (unpinned tags)

- [x] Implementation complete
- [x] UI build passes (`just ui build`)
- [ ] Docker images build (`just docker`) — not tested locally (requires full Docker build)
- [x] Helm lint passes
- [ ] Manual verification: Markdown renders safely

### E1: Add DOMPurify to Markdown component

**Files:**

| File | Change |
|---|---|
| `ui/package.json` | Add `dompurify` + `@types/dompurify` dependencies |
| `ui/src/components/Markdown.tsx` | Import DOMPurify, sanitize `marked.parse()` output before `dangerouslySetInnerHTML` |

**Markdown.tsx after fix:**
```tsx
import { marked } from 'marked';
import DOMPurify from 'dompurify';

export function Markdown({ content }: { content: string }) {
  const raw = marked.parse(content, { async: false }) as string;
  const html = DOMPurify.sanitize(raw);
  return <div class="markdown" dangerouslySetInnerHTML={{ __html: html }} />;
}
```

DOMPurify defaults strip `<script>`, event handlers (`onerror`, `onclick`), `<iframe>`, `<object>`, `<embed>`, and `javascript:` URLs. No configuration needed — the defaults are correct for user-generated markdown.

### E7: Non-root platform Docker image + Helm securityContext

**Files:**

| File | Change |
|---|---|
| `docker/Dockerfile` | Add `platform` user (UID 1000, GID 1000), `USER 1000` directive |
| `helm/platform/templates/deployment.yaml` | Add pod `securityContext` with `runAsUser: 1000`, `runAsGroup: 1000`, `fsGroup: 1000`. Add `securityContext.runAsUser: 0` override on init container |
| `helm/platform/values.yaml` | Add `securityContext` section with defaults |

**Dockerfile changes (stage 4):**
```dockerfile
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends git ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd -r -g 1000 platform \
    && useradd -r -s /sbin/nologin -u 1000 -g 1000 platform
COPY --from=builder /app/target/release/platform /platform
COPY --from=agent-runner-builder \
  /agent-runner/target/x86_64-unknown-linux-gnu/release/agent-runner \
  /data/agent-runner/amd64
COPY --from=agent-runner-builder \
  /agent-runner/target/aarch64-unknown-linux-gnu/release/agent-runner \
  /data/agent-runner/arm64
EXPOSE 8080
USER 1000
ENTRYPOINT ["/platform"]
```

No `chown` needed — K8s `fsGroup: 1000` handles PVC ownership at mount time.

**deployment.yaml changes:**
```yaml
spec:
  template:
    spec:
      securityContext:
        runAsUser: {{ .Values.platform.securityContext.runAsUser | default 1000 }}
        runAsGroup: {{ .Values.platform.securityContext.runAsGroup | default 1000 }}
        fsGroup: {{ .Values.platform.securityContext.fsGroup | default 1000 }}
      initContainers:
        - name: init-data
          securityContext:
            runAsUser: 0  # init copies files from image to PVC before fsGroup takes effect
          ...
```

**values.yaml addition:**
```yaml
platform:
  securityContext:
    runAsUser: 1000
    runAsGroup: 1000
    fsGroup: 1000
```

### E9: Pin Kaniko and Claude CLI versions

**Files:**

| File | Change |
|---|---|
| `docker/Dockerfile.platform-runner` | Pin kaniko to specific version+digest, pin Claude CLI to specific version |
| `docker/Dockerfile.platform-runner-bare` | Pin kaniko to same version+digest |

**Changes:**
```dockerfile
# Dockerfile.platform-runner line 22 — before:
FROM gcr.io/kaniko-project/executor:latest AS kaniko
# after:
FROM gcr.io/kaniko-project/executor:v1.23.2 AS kaniko

# Dockerfile.platform-runner line 48 — before:
RUN npm install -g @anthropic-ai/claude-code
# after:
RUN npm install -g @anthropic-ai/claude-code@0.2.93

# Dockerfile.platform-runner-bare line 5 — before:
FROM gcr.io/kaniko-project/executor:latest AS kaniko
# after:
FROM gcr.io/kaniko-project/executor:v1.23.2 AS kaniko
```

**Note:** Look up the current latest kaniko release tag and Claude CLI version at implementation time. The exact versions above are placeholders — use whatever `latest` resolves to today, then pin it.

### Test Outline — PR 1

**E1 verification:**
- Manual: create an issue with body `<img src=x onerror=alert(1)>` — verify the `onerror` is stripped in rendered HTML
- Unit (optional): add a test in `ui/` that `Markdown` component sanitizes output (Preact testing-library)

**E7 verification:**
- `docker build -f docker/Dockerfile .` succeeds
- `docker run --rm <image> whoami` prints `platform` (not `root`)
- `helm template` shows securityContext in deployment spec
- `just cluster-up && just deploy-local` — platform starts, can create projects, push code

**E9 verification:**
- `docker build -f docker/Dockerfile.platform-runner .` succeeds with pinned versions
- `grep -r 'latest' docker/Dockerfile*` returns no hits for kaniko or claude-code

---

## PR 2: Helm & Infrastructure — RBAC, env vars, master key, install.sh

Addresses: **E3a** (master key hex), **E4** (ServiceAccount RBAC), **E5** (Gateway API RBAC), **E6** (missing env vars), **E15** (install.sh checksums)

- [x] Implementation complete
- [x] `helm lint` passes
- [x] `helm template` shows correct RBAC, env vars, and master key format
- [x] `install.sh` syntax check passes (`bash -n`)

### E3a: Fix PLATFORM_MASTER_KEY generation to produce hex

**File:** `helm/platform/templates/secret.yaml`

**Change line 50 from:**
```yaml
PLATFORM_MASTER_KEY: {{ randAlphaNum 64 | b64enc | quote }}
```
**To:**
```yaml
PLATFORM_MASTER_KEY: {{ randAlphaNum 64 | sha256sum | trunc 64 | b64enc | quote }}
```

`sha256sum` in Helm/Sprig produces a 64-char hex digest from the random input. The input has ~381 bits of entropy (64 × log2(62)), which SHA-256 compresses to 256 bits — a full AES-256 key. The `trunc 64` is technically redundant (sha256sum already produces 64 hex chars) but makes intent explicit.

### E4 + E5: Add missing RBAC permissions to ClusterRole

**File:** `helm/platform/templates/clusterrole.yaml`

**Add after the existing PDB rule (line 80):**
```yaml
  # ServiceAccounts (agent identity, per-project namespaces)
  - apiGroups: [""]
    resources: ["serviceaccounts"]
    verbs: ["get", "list", "watch", "create", "update", "patch", "delete"]

  # Gateway API (progressive delivery traffic splitting)
  - apiGroups: ["gateway.networking.k8s.io"]
    resources: ["httproutes", "gateways"]
    verbs: ["get", "list", "watch", "create", "update", "patch", "delete"]
```

Also update `hack/test-manifests/rbac.yaml` to match — add the same two rules to the `test-runner` ClusterRole so E2E tests can exercise the deployer's traffic-splitting.

**Files:**

| File | Change |
|---|---|
| `helm/platform/templates/clusterrole.yaml` | Add `serviceaccounts` and `gateway.networking.k8s.io` rules |
| `hack/test-manifests/rbac.yaml` | Add same two rules to `test-runner` ClusterRole |

### E6: Add missing env vars to Helm configmap

**File:** `helm/platform/templates/configmap.yaml`

**Add to the K8s section:**
```yaml
  PLATFORM_PIPELINE_NAMESPACE: {{ printf "%s-pipelines" .Release.Namespace | quote }}
  PLATFORM_AGENT_NAMESPACE: {{ printf "%s-agents" .Release.Namespace | quote }}
```

**Add to the gateway/deploy section (new):**
```yaml
  {{- if .Values.platform.gateway.name }}
  PLATFORM_GATEWAY_NAME: {{ .Values.platform.gateway.name | quote }}
  {{- end }}
  {{- if .Values.platform.gateway.namespace }}
  PLATFORM_GATEWAY_NAMESPACE: {{ .Values.platform.gateway.namespace | quote }}
  {{- end }}
```

**Add to the Valkey section:**
```yaml
  {{- if .Values.platform.valkeyAgentHost }}
  PLATFORM_VALKEY_AGENT_HOST: {{ .Values.platform.valkeyAgentHost | quote }}
  {{- else if .Values.valkey.enabled }}
  PLATFORM_VALKEY_AGENT_HOST: {{ printf "%s-valkey-master:%s" .Release.Name (.Values.valkey.master.service.ports.valkey | default "6379" | toString) | quote }}
  {{- end }}
```

**File:** `helm/platform/values.yaml`

**Add defaults:**
```yaml
platform:
  gateway:
    name: ""      # defaults to "platform-gateway" in config.rs
    namespace: "" # defaults to PLATFORM_NAMESPACE in config.rs
  valkeyAgentHost: "" # auto-derived from valkey subchart if empty
```

### E15: Add checksum verification to install.sh

**File:** `install.sh`

**Replace k0s install (~line 97):**
```bash
# Before:
curl -sSLf https://get.k0s.sh | sudo sh

# After:
K0S_VERSION="v1.31.4+k0s.0"
K0S_BINARY="/usr/local/bin/k0s"
curl -sSLf "https://github.com/k0sproject/k0s/releases/download/${K0S_VERSION}/k0s-${K0S_VERSION}-${ARCH}" -o /tmp/k0s
curl -sSLf "https://github.com/k0sproject/k0s/releases/download/${K0S_VERSION}/k0s-${K0S_VERSION}-${ARCH}.sha256" -o /tmp/k0s.sha256
echo "$(cat /tmp/k0s.sha256)  /tmp/k0s" | sha256sum -c -
sudo install -m 0755 /tmp/k0s "$K0S_BINARY"
rm -f /tmp/k0s /tmp/k0s.sha256
```

**Replace Helm install (~line 143):**
```bash
# Before:
curl -fsSL https://raw.githubusercontent.com/helm/helm/main/scripts/get-helm-3 | bash

# After:
HELM_VERSION="v3.16.4"
curl -fsSL "https://get.helm.sh/helm-${HELM_VERSION}-linux-${ARCH}.tar.gz" -o /tmp/helm.tar.gz
curl -fsSL "https://get.helm.sh/helm-${HELM_VERSION}-linux-${ARCH}.tar.gz.sha256sum" -o /tmp/helm.sha256
echo "$(cat /tmp/helm.sha256)" | sha256sum -c -
tar xzf /tmp/helm.tar.gz -C /tmp
sudo install -m 0755 /tmp/linux-${ARCH}/helm /usr/local/bin/helm
rm -rf /tmp/helm.tar.gz /tmp/helm.sha256 /tmp/linux-${ARCH}
```

**Note:** Look up current stable versions at implementation time. Pin to whatever is current, then the versions are locked.

### Test Outline — PR 2

**E3a verification:**
- `helm template test helm/platform/ | grep PLATFORM_MASTER_KEY | base64 -d` — verify output is 64 hex chars (0-9, a-f only)
- Fresh `helm install` → platform starts without `"invalid PLATFORM_MASTER_KEY hex"` error

**E4+E5 verification:**
- `helm template test helm/platform/ | grep -A5 serviceaccounts` — verify rule exists
- `helm template test helm/platform/ | grep -A5 gateway.networking.k8s.io` — verify rule exists
- Integration test creating an agent session in a Helm-deployed cluster doesn't get 403

**E6 verification:**
- `helm template test helm/platform/ | grep PLATFORM_PIPELINE_NAMESPACE` — verify value is `{namespace}-pipelines`
- `helm template test helm/platform/ --set platform.gateway.name=my-gw | grep PLATFORM_GATEWAY_NAME`

**E15 verification:**
- `bash -n install.sh` — syntax check passes
- Manual: run install.sh on a clean VM, verify k0s and Helm installed with correct versions

---

## PR 3: MCP Server Fixes — Deploy rewrite, admin/issues contract alignment

Addresses: **E2** (deploy server broken), **E10** (MR labels), **E11** (target_branch required), **E12** (create_role permissions), **E13** (remove_role param name), **E14** (list_users search)

- [x] Implementation complete
- [ ] `cd mcp && npm test` passes — tests need updating for new tool names
- [ ] Manual verification: MCP tools return valid responses against running platform

### E2: Rewrite platform-deploy.js for actual API

Complete rewrite. The current server targets non-existent `/api/projects/{id}/deployments` endpoints. The actual API uses a target + release model.

**File:** `mcp/servers/platform-deploy.js`

**New tool definitions:**

| Tool | Method | Endpoint | Required | Optional |
|---|---|---|---|---|
| `list_targets` | GET | `/api/projects/{p}/targets` | — | `limit`, `offset` |
| `get_target` | GET | `/api/projects/{p}/targets/{target_id}` | `target_id` | — |
| `create_target` | POST | `/api/projects/{p}/targets` | `name`, `environment` | `branch`, `default_strategy`, `ops_repo_id`, `manifest_path` |
| `list_releases` | GET | `/api/projects/{p}/deploy-releases` | — | `target_id`, `limit`, `offset` |
| `get_release` | GET | `/api/projects/{p}/deploy-releases/{release_id}` | `release_id` | — |
| `create_release` | POST | `/api/projects/{p}/deploy-releases` | `target_id`, `image_ref` | `strategy`, `commit_sha`, `values_override` |
| `adjust_traffic` | PATCH | `/api/projects/{p}/deploy-releases/{release_id}/traffic` | `release_id`, `weight` | — |
| `promote_release` | POST | `/api/projects/{p}/deploy-releases/{release_id}/promote` | `release_id` | — |
| `rollback_release` | POST | `/api/projects/{p}/deploy-releases/{release_id}/rollback` | `release_id` | — |
| `release_history` | GET | `/api/projects/{p}/deploy-releases/{release_id}/history` | `release_id` | `limit`, `offset` |
| `staging_status` | GET | `/api/projects/{p}/staging-status` | — | — |

**Note:** `apiPatch` is already exported from `mcp/lib/client.js`. No client changes needed.

**Also add `apiPut`** to `mcp/lib/client.js` — needed for E12 (set role permissions):
```javascript
export function apiPut(path, opts) { return request("PUT", path, opts); }
```

### E10: Remove `labels` from MR tool schemas

**File:** `mcp/servers/platform-issues.js`

- `create_merge_request` tool schema: remove `labels` from `properties` and handler body
- `update_merge_request` tool schema: remove `labels` from `properties` and handler body

### E11: Add `target_branch` to required fields

**File:** `mcp/servers/platform-issues.js`

Change `create_merge_request` schema:
```javascript
// Before:
required: ["title", "source_branch"],
// After:
required: ["title", "source_branch", "target_branch"],
```

### E12: Fix create_role to set permissions via follow-up PUT

**File:** `mcp/servers/platform-admin.js`

**Change `create_role` handler to:**
1. POST `/api/admin/roles` with `{ name, description }` only
2. If `permissions` array provided, follow up with PUT `/api/admin/roles/{id}/permissions` with `{ permissions }`

```javascript
case "create_role": {
  const role = await apiPost("/api/admin/roles", {
    body: { name: args.name, description: args.description },
  });
  if (args.permissions && args.permissions.length > 0) {
    await apiPut(`/api/admin/roles/${role.id}/permissions`, {
      body: { permissions: args.permissions },
    });
  }
  return { content: [{ type: "text", text: JSON.stringify(role, null, 2) }] };
}
```

Import `apiPut` from `../lib/client.js`.

### E13: Rename `role_assignment_id` to `role_id`

**File:** `mcp/servers/platform-admin.js`

- Tool schema: rename `role_assignment_id` property to `role_id`
- Required array: `["user_id", "role_id"]`
- Handler: `args.role_id` instead of `args.role_assignment_id`
- Description: "Role UUID" instead of "role assignment UUID"

### E14: Remove `search` from list_users

**File:** `mcp/servers/platform-admin.js`

- Tool schema: remove `search` from `properties`
- Handler: remove `search` from query params construction

### Test Outline — PR 3

**E2 verification:**
- Update `mcp/tests/test-deploy.js` (or create if missing) — mock API server returns canned responses for new endpoints, verify each tool calls the correct URL with correct method
- Manual: run deploy MCP server against a live platform, call `list_targets` — verify it returns actual data

**E10-E14 verification:**
- Update `mcp/tests/test-issues.js` — verify `create_merge_request` request body has no `labels` field, has `target_branch`
- Update `mcp/tests/test-admin.js` — verify `create_role` makes POST then PUT, `remove_role` uses `role_id` param
- Manual: run admin MCP server, create role with permissions — verify permissions actually persist

**Estimated test count:** ~8 MCP test cases (mock API)

---

## PR 4: Rust fix — Control message JSON shape alignment

Addresses: **E3b** (control message mismatch)

- [x] Implementation complete
- [ ] `just test-unit` passes — platform has pre-existing compile errors in working tree
- [ ] `cargo clippy` clean

### E3b: Fix publish_control to match CLI's expected JSON shape

**File:** `src/agent/pubsub_bridge.rs`

The CLI deserializes control messages as:
```json
{"type": "control", "control": {"type": "interrupt"}}
```

But `publish_control()` currently sends:
```json
{"type": "control", "control_type": "interrupt"}
```

**Change `publish_control` (lines 58-70):**
```rust
pub async fn publish_control(
    valkey: &fred::clients::Pool,
    session_id: Uuid,
    control_type: &str,
) -> Result<(), anyhow::Error> {
    let channel = valkey_acl::input_channel(session_id);
    let msg = serde_json::json!({
        "type": "control",
        "control": { "type": control_type }
    });
    valkey
        .next()
        .publish::<(), _, _>(&channel, msg.to_string())
        .await?;
    Ok(())
}
```

This aligns with `PubSubInput::Control { control: ControlPayload }` in the CLI, where `ControlPayload` is `#[serde(tag = "type")]`.

**Note:** This function is currently `#[allow(dead_code)]`. The fix is forward-looking — when it's activated, it'll work correctly. Keep the `#[allow(dead_code)]` annotation.

### Test Outline — PR 4

**Unit test:**
Add a test in `src/agent/pubsub_bridge.rs` (or `tests/`) that verifies the JSON shape:
```rust
#[test]
fn publish_control_json_shape() {
    let msg = serde_json::json!({
        "type": "control",
        "control": { "type": "interrupt" }
    });
    // Verify it deserializes as PubSubInput::Control on the CLI side
    let json = msg.to_string();
    // Could import CLI types or just verify the JSON structure
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["type"], "control");
    assert_eq!(parsed["control"]["type"], "interrupt");
}
```

**Estimated test count:** 1 unit test

---

## Summary

| PR | Scope | Findings | Risk | Effort |
|---|---|---|---|---|
| PR 1 | Security: XSS, non-root Docker, pinned deps | E1, E7, E9 | High (XSS is critical) | Medium |
| PR 2 | Helm & infra: RBAC, env vars, master key, install.sh | E3a, E4, E5, E6, E15 | High (broken secrets, missing RBAC) | Medium |
| PR 3 | MCP: deploy rewrite, contract fixes | E2, E10-E14 | Medium (MCP tools broken) | Medium-High (deploy rewrite) |
| PR 4 | Rust: control message fix | E3b | Low (dead code) | Low |

**Recommended merge order:** PR 1 → PR 2 → PR 3 → PR 4 (or PR 1-4 in parallel — no dependencies between them)

**E8 is excluded** — accepted design trade-off, documented in `docs/design-decisions.md` DD-1.
