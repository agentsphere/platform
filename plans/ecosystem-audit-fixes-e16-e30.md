# Plan: Ecosystem Audit Fixes E16–E30

## Context

Second batch of ecosystem audit fixes from `plans/ecosystem-audit-2026-03-24.md`. The first batch (E1-E15) addressed 3 critical and 10 high-severity findings. This batch addresses 2 remaining high findings (E16, E17) and 13 medium findings (E18-E30).

E15 was already addressed in the first batch. E34 (test RBAC) was partially addressed — `apps` and `gateway.networking.k8s.io` rules were added.

## Design Principles

- **Consistency over cleverness** — apply the same pattern everywhere (e.g., all MCP servers get try/catch, all Dockerfiles get the same Rust version)
- **No Rust src/ changes except E22, E28** — E22 is in `cli/agent-runner`, E28 needs a backend change in `src/api/users.rs`
- **Pin to what's on the machine** — Rust version pinned to 1.93, matching local toolchain

---

## PR 1: Security — Iframe sandboxing, entrypoint secret leak, password re-auth

Addresses: **E16** (iframe sandbox), **E17** (entrypoint git add leaks token), **E28** (password change without current password)

- [x] Implementation complete
- [x] UI build passes
- [ ] Entrypoint tested manually (requires Docker build)
- [ ] Password change verified (requires running platform)

### E16: Add sandbox attribute to all iframes

**Files:**

| File | Change |
|---|---|
| `ui/src/pages/ProjectDetail.tsx:103` | Add `sandbox="allow-scripts allow-same-origin allow-forms allow-popups"` |
| `ui/src/components/ProjectCard.tsx:77` | Add same `sandbox` attribute |

**Changes:**

In `ProjectDetail.tsx`, change:
```tsx
<iframe src={previewUrl} tabIndex={-1} loading="lazy" />
```
To:
```tsx
<iframe src={previewUrl} tabIndex={-1} loading="lazy" sandbox="allow-scripts allow-same-origin allow-forms allow-popups" />
```

Same change in `ProjectCard.tsx`. Matches the existing pattern in `IframePanel.tsx:50`.

### E17: Prevent entrypoint from committing API token to git

**File:** `docker/entrypoint.sh`

Two changes:

1. **Set restrictive permissions on the env file** (after line 89):
```bash
chmod 600 /workspace/.platform/.env
```

2. **Exclude `.platform/` from git add** (line 102):
```bash
# Before:
git add -A
# After:
git add -A -- ':!.platform/'
```

3. **Add .platform to .gitignore** (before the git add block):
```bash
# Ensure platform secrets are never committed
grep -qxF '.platform/' /workspace/.gitignore 2>/dev/null || echo '.platform/' >> /workspace/.gitignore
```

### E28: Require current password for password change

This needs both UI and backend changes.

**Backend — `src/api/users.rs`:**

Add `current_password` to `UpdateUserRequest`:
```rust
#[derive(Debug, Deserialize)]
pub struct UpdateUserRequest {
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub password: Option<String>,
    pub current_password: Option<String>,
}
```

In the `update_user` handler, when `body.password` is `Some`, verify `current_password`:
```rust
if body.password.is_some() && auth.user_id == id {
    // Non-admin users must provide current password
    let cp = body.current_password.as_deref()
        .ok_or_else(|| ApiError::Validation("current_password required".into()))?;
    let current_hash = sqlx::query_scalar!(
        "SELECT password_hash FROM users WHERE id = $1",
        id,
    )
    .fetch_one(&state.pool)
    .await?;
    if !password::verify_password(cp, &current_hash).unwrap_or(false) {
        return Err(ApiError::Validation("current password is incorrect".into()));
    }
}
// Admin changing another user's password doesn't need current_password
```

**Frontend — `ui/src/pages/AccountSettings.tsx:36`:**

Change the API call to send `current_password`:
```tsx
await api.patch(`/api/users/${user!.id}`, {
  password: newPw,
  current_password: currentPw,
});
```

### Test Outline — PR 1

**E16:** Manual — iframes in ProjectDetail and ProjectCard have `sandbox` attribute in rendered HTML.

**E17:** Manual — after agent session, `.platform/` not in git status; `.env` file has `0600` permissions.

**E28:**
- Integration test: PATCH `/api/users/{id}` with `password` but no `current_password` → 400
- Integration test: PATCH with wrong `current_password` → 400
- Integration test: PATCH with correct `current_password` → 200
- Integration test: Admin changing another user's password without `current_password` → 200 (admin bypass)
- Estimated: 4 integration tests

---

## PR 2: Helm & Infrastructure — NetworkPolicy, Kustomize fixes, Docker version alignment

Addresses: **E18** (NetworkPolicy egress), **E19** (Kustomize readiness probe), **E20** (Kustomize secrets in ConfigMap), **E21** (Kustomize missing rbac.yaml), **E30** (Rust version mismatch)

- [x] Implementation complete
- [x] `helm lint` passes
- [x] `helm template` shows preview egress rule (`platform.io/managed-by`)
- [ ] Kustomize overlays apply cleanly (requires kubectl)

### E18: Add preview proxy egress to NetworkPolicy

**File:** `helm/platform/templates/networkpolicy-platform.yaml`

Add after the K8s API server egress rule (line 52), before the Internet rule:

```yaml
    # Preview proxy — egress to platform-managed namespaces (agent sessions, deploy previews)
    - to:
        - namespaceSelector:
            matchLabels:
              platform.io/managed-by: platform
```

No port restriction — preview services may listen on any port. The namespace label `platform.io/managed-by: platform` is already set by `build_namespace_object()` in `src/deployer/namespace.rs:258-263` on every platform-managed namespace. The platform namespace itself does NOT have this label (created by Helm, not by the platform binary), so this doesn't open egress back to the platform's own data services.

### E19: Fix Kustomize readiness probe path

**File:** `deploy/base/deployment.yaml`

Change readiness probe:
```yaml
# Before:
readinessProbe:
  httpGet:
    path: /healthz
# After:
readinessProbe:
  httpGet:
    path: /readyz
```

Matches the Helm chart, which already correctly uses `/readyz`.

### E20: Move secrets from Kustomize ConfigMap to Secret

**File:** `deploy/base/configmap.yaml`

Remove `DATABASE_URL`, `MINIO_ACCESS_KEY`, `MINIO_SECRET_KEY` from ConfigMap.

**New file:** `deploy/base/secret.yaml`

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: platform-secrets
type: Opaque
stringData:
  DATABASE_URL: "postgres://platform:dev@platform-db-rw:5432/platform_dev"
  MINIO_ACCESS_KEY: "platform"
  MINIO_SECRET_KEY: "devdevdev"
```

**File:** `deploy/base/deployment.yaml`

Add `secretRef` alongside the existing `configMapRef`:
```yaml
envFrom:
  - configMapRef:
      name: platform-config
  - secretRef:
      name: platform-secrets
```

**File:** `deploy/base/kustomization.yaml`

Add `secret.yaml` to resources (also fixes E21 — see below).

### E21: Add rbac.yaml to Kustomize resources

**File:** `deploy/base/kustomization.yaml`

Change from:
```yaml
resources:
  - deployment.yaml
  - service.yaml
  - configmap.yaml
```
To:
```yaml
resources:
  - deployment.yaml
  - service.yaml
  - configmap.yaml
  - secret.yaml
  - rbac.yaml
```

### E30: Align Rust version across all Dockerfiles

Pin all Dockerfiles to Rust **1.93** (matches local toolchain `rustc 1.93.1`).

| File | Before | After |
|---|---|---|
| `docker/Dockerfile:10` (planner) | `rust:1.88-slim-bookworm` | `rust:1.93-slim-bookworm` |
| `docker/Dockerfile:17` (agent-runner-builder) | `rust:1.88-slim-bookworm` | `rust:1.93-slim-bookworm` |
| `docker/Dockerfile:31` (builder) | `rust:1.88-slim-bookworm` | `rust:1.93-slim-bookworm` |
| `docker/Dockerfile.platform-runner:8` (builder) | `rust:1-slim-bookworm` | `rust:1.93-slim-bookworm` |
| `docker/Dockerfile.dev-pod:8` | `rust:1.93-bookworm` | `rust:1.93-slim-bookworm` (also switch to slim) |

### Test Outline — PR 2

**E18:** `helm template test helm/platform/ | grep -A3 "platform.io/managed-by"` — verify egress rule.

**E19:** Inspect `deploy/base/deployment.yaml` readiness path is `/readyz`.

**E20:** `grep DATABASE_URL deploy/base/configmap.yaml` returns nothing. `grep DATABASE_URL deploy/base/secret.yaml` returns the value.

**E21:** `kubectl kustomize deploy/base/` succeeds and includes ServiceAccount, Role, RoleBinding resources.

**E30:** `grep "rust:" docker/Dockerfile*` — all show `1.93`.

---

## PR 3: MCP hardening — Error handling, client timeout, schema fixes

Addresses: **E24** (missing try/catch), **E25** (Content-Type on GET), **E26** (no timeout), **E27** (list_alerts schema)

- [x] Implementation complete
- [x] `cd mcp && npm test` passes (40/40)

### E24: Add try/catch to 3 MCP servers

**Files:** `mcp/servers/platform-core.js`, `mcp/servers/platform-issues.js`, `mcp/servers/platform-pipeline.js`

Wrap the `switch` body in each `CallToolRequestSchema` handler with the same pattern used in `platform-admin.js`, `platform-deploy.js`, and `platform-observe.js`:

```javascript
server.setRequestHandler(CallToolRequestSchema, async (request) => {
  const { name, arguments: args = {} } = request.params;
  // ... existing setup ...

  try {
    switch (name) {
      // ... existing cases (unchanged) ...
    }
  } catch (err) {
    return {
      content: [{ type: "text", text: `Error: ${err.message}` }],
      isError: true,
    };
  }
});
```

For `platform-issues.js` and `platform-pipeline.js`, this means wrapping the existing bare `switch` in try/catch.

For `platform-core.js`, there's already partial try/catch inside individual cases (e.g., `ask_for_secret`). Add the outer try/catch around the entire switch — the inner ones still work for case-specific error messages.

### E25: Only send Content-Type header when body is present

**File:** `mcp/lib/client.js`

Change `request()` function:
```javascript
// Before:
const options = {
  method,
  headers: {
    Authorization: `Bearer ${API_TOKEN}`,
    "Content-Type": "application/json",
  },
};

// After:
const headers = {
  Authorization: `Bearer ${API_TOKEN}`,
};
if (body !== undefined) {
  headers["Content-Type"] = "application/json";
}
const options = { method, headers };
```

### E26: Add request timeout to MCP client

**File:** `mcp/lib/client.js`

Add `AbortController` with 30s timeout:
```javascript
const controller = new AbortController();
const timeout = setTimeout(() => controller.abort(), 30_000);
const options = { method, headers, signal: controller.signal };
// ... existing body logic ...
try {
  const res = await fetch(url.toString(), options);
  // ... existing response handling ...
} finally {
  clearTimeout(timeout);
}
```

### E27: Fix list_alerts schema — replace `status` with `enabled`

**File:** `mcp/servers/platform-observe.js`

Change `list_alerts` tool schema:
```javascript
// Before:
status: {
  type: "string",
  description: "Filter by alert status (firing/resolved/pending)",
},

// After:
enabled: {
  type: "boolean",
  description: "Filter by enabled/disabled status",
},
```

Change handler:
```javascript
// Before:
query: { project_id: p, status: args.status, limit: args.limit, offset: args.offset }

// After:
query: { project_id: p, enabled: args.enabled, limit: args.limit, offset: args.offset }
```

### Test Outline — PR 3

**E24:** Existing MCP tests already call tools against a mock server. If the mock returns 500, the test should now get `isError: true` instead of an unhandled rejection. Add one test per server verifying error handling.

**E25:** Manual — `Content-Type` header not sent on GET requests (inspect in debug).

**E26:** Manual — timeout fires if mock server delays >30s (hard to test in CI, document as manually verified).

**E27:** Update `mcp/tests/test-observe.js` if a `list_alerts` test exists, or add one.

**Estimated test count:** ~3 new tests (error handling per server)

---

## PR 4: CLI hardening — Secret file permissions, pub-sub reconnection

Addresses: **E22** (secret file permissions), **E23** (pub-sub reconnection)

- [x] Implementation complete
- [x] `cd cli/agent-runner && cargo check && cargo clippy` passes

### E22: Set restrictive permissions on secrets env file

**File:** `cli/agent-runner/src/main.rs`

After `File::create()` for `/workspace/.env.dev`, set permissions to `0o600`:

```rust
#[cfg(unix)]
{
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)) {
        tracing::warn!(error = %e, "failed to set .env.dev permissions");
    }
}
```

Place this after the file is written and closed (after the `write!` calls and file drop).

### E23: Add reconnection logic to pub-sub subscriber

**File:** `cli/agent-runner/src/pubsub.rs`

The subscriber task currently exits permanently when the connection drops. Add a reconnection loop with exponential backoff:

```rust
// Outer reconnection loop
let mut backoff = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

loop {
    // Create subscriber connection
    let subscriber = match self.client.clone_new() {
        // ... existing subscription setup ...
    };

    // Inner message loop (existing code)
    while let Ok(message) = subscriber.recv().await {
        // ... existing message handling ...
        backoff = Duration::from_millis(500); // reset on successful message
    }

    // Connection lost — reconnect with backoff
    tracing::warn!(
        backoff_ms = backoff.as_millis(),
        "pub/sub subscriber disconnected, reconnecting..."
    );
    tokio::time::sleep(backoff).await;
    backoff = (backoff * 2).min(MAX_BACKOFF);
}
```

Key design decisions:
- Reset backoff on successful message receipt (not on reconnect)
- Max backoff 30s — don't wait too long, Valkey restarts are usually fast
- Log at `warn` level on each reconnection attempt
- Keep the outer loop running until the `mpsc` receiver is dropped (session termination)

### Test Outline — PR 4

**E22:** Unit test — create a temp file with `write_secrets_env_file()`, verify permissions are `0o600`.

**E23:** Hard to unit test reconnection (needs Valkey). Document as manually verified. Consider an integration test that kills Valkey, verifies reconnection.

**Estimated test count:** 1 unit test

---

## PR 5: UI + Tokens fix

Addresses: **E29** (Tokens page response type mismatch)

- [x] ~~Implementation complete~~ → FALSE POSITIVE, no change needed
- [x] UI build passes

### E29: ~~Fix~~ Tokens page API response parsing — FALSE POSITIVE

**File:** `ui/src/pages/admin/Tokens.tsx`

The server returns `Vec<TokenResponse>` directly (not wrapped in `ListResponse`). Per the exploration, the Rust handler at `src/api/users.rs:726-755` returns `Json<Vec<TokenResponse>>`.

Check whether the API actually returns `Vec<T>` or `ListResponse<T>`:
- If `Vec<T>`: the current `api.get<ApiToken[]>` type annotation is correct, but verify `.then(r => setTokens(r))` works (it should — `r` is already the array)
- If `ListResponse<T>`: change to `api.get<ListResponse<ApiToken>>` and access `.items`

Based on the investigation, the handler returns `Json<Vec<TokenResponse>>` — a bare array. The current code `api.get<ApiToken[]>('/api/tokens?limit=100').then(r => setTokens(r))` should work correctly. The audit finding may be a false positive.

**Verified:** `list_api_tokens()` at `src/api/users.rs:742` returns `Json<Vec<TokenResponse>>` — a bare array, not `ListResponse<T>`. The UI code is correct. **No change needed.**

### Test Outline — PR 5

Manual — navigate to admin/Tokens page, verify tokens list renders.

---

## Summary

| PR | Scope | Findings | Effort |
|---|---|---|---|
| PR 1 | Security: iframes, entrypoint, password re-auth | E16, E17, E28 | Medium (E28 needs backend change) |
| PR 2 | Helm & infra: NetworkPolicy, Kustomize, Rust version | E18, E19, E20, E21, E30 | Medium |
| PR 3 | MCP: error handling, client timeout, schema | E24, E25, E26, E27 | Low-Medium |
| PR 4 | CLI: file perms, reconnection | E22, E23 | Medium (reconnection logic) |
| PR 5 | UI: Tokens page | E29 | Low (may be false positive) |

**Recommended merge order:** PR 1 → PR 2 → PR 3 → PR 4 → PR 5 (or PR 1-5 in parallel — no dependencies between them)
