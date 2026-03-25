# Plan: Ecosystem Audit Fixes E39–E62 (Low Findings)

## Context

Final batch — 24 low-severity findings from the ecosystem audit. These are style nits, minor inconsistencies, and hardening opportunities. None are blocking, but cleaning them up improves overall code quality.

Several are already fixed by earlier batches:
- **E47** (dompurify) — fixed in E1-E15 batch
- **E54** (kubectl amd64) — fixed in E31-E38 batch (dev-pod now uses `dpkg --print-architecture`)

Some should be accepted/skipped:
- **E43** (SVG dangerouslySetInnerHTML) — safe, static data. Skip.
- **E55** (`--dangerously-skip-permissions`) — intentional for agent operation. Accepted in DD-1. Skip.
- **E60** (Helm repo URL) — the placeholder text is clearly documented. Skip.
- **E61** (template Dockerfile copies missing files) — by design, agent creates the files. Skip.
- **E62** (changeme password) — acceptable for demo templates. Skip.

That leaves **17 actionable items**, grouped into 2 PRs.

---

## PR 1: CLI hardening — E39, E40, E41, E42

All in `cli/agent-runner/src/`. Small, self-contained.

- [x] Implementation complete
- [x] `cd cli/agent-runner && cargo check && cargo clippy` passes

### E39: Use closed enum for desktop notification kinds

**File:** `cli/agent-runner/src/render.rs`

The `notify_desktop()` function takes `&str` parameters that get interpolated into AppleScript. Currently safe because all callers pass hardcoded strings, but the API doesn't enforce this.

Replace `notify_desktop(title: &str, body: &str)` with a typed enum:

```rust
enum Notification {
    SessionReady,
    SessionError,
    SessionComplete,
}

impl Notification {
    fn title(&self) -> &'static str {
        match self {
            Self::SessionReady => "Session Ready",
            Self::SessionError => "Session Error",
            Self::SessionComplete => "Session Complete",
        }
    }
    fn body(&self) -> &'static str {
        match self {
            Self::SessionReady => "Agent session is ready",
            Self::SessionError => "Agent session encountered an error",
            Self::SessionComplete => "Agent session completed",
        }
    }
}

fn notify_desktop(notif: Notification) {
    let title = notif.title();
    let body = notif.body();
    // ... existing AppleScript/notify-send logic unchanged
}
```

Update all callers to use the enum instead of string literals.

### E40: Cross-check PLATFORM_SECRET_NAMES against RESERVED_ENV_VARS

**File:** `cli/agent-runner/src/main.rs`

In `write_secrets_env_file()`, after splitting `PLATFORM_SECRET_NAMES`, filter out any name that appears in `RESERVED_ENV_VARS`:

```rust
for name in names.split(',') {
    let name = name.trim();
    if name.is_empty() {
        continue;
    }
    if RESERVED_ENV_VARS.contains(&name) {
        eprintln!("[warn] skipping reserved env var '{name}' in PLATFORM_SECRET_NAMES");
        continue;
    }
    // ... existing env::var + write logic
}
```

### E41: Reduce init timeout for pod mode

**File:** `cli/agent-runner/src/repl.rs`

The 600s timeout is for both local REPL and pod mode. In pod mode, Claude CLI is pre-installed and should init within ~60s. In local mode, the user might need to authenticate first (OAuth flow), which can take longer.

Change from hardcoded 600s to mode-dependent:

```rust
// In the caller that invokes wait_for_init:
let timeout_secs = if is_pod_mode { 180 } else { 600 };
wait_for_init(&transport, timeout_secs).await
```

If the `is_pod_mode` flag isn't available at that call site, use a constant:
```rust
const INIT_TIMEOUT_POD: u64 = 180;
const INIT_TIMEOUT_LOCAL: u64 = 600;
```

### E42: Unify truncation to character semantics

**Files:** `cli/agent-runner/src/pubsub.rs`, `cli/agent-runner/src/render.rs`

Both have a `truncate` function. `pubsub.rs` operates on bytes, `render.rs` on characters.

Extract a shared `truncate_str` function in a common location (e.g., a small `util.rs` module or inline in `render.rs` and import from `pubsub.rs`):

```rust
/// Truncate a string to at most `max_chars` characters, appending "..." if truncated.
pub fn truncate_str(s: &str, max_chars: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_chars {
        s.to_owned()
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{truncated}...")
    }
}
```

Replace both existing implementations with calls to this shared function.

### Test Outline — PR 1

- E39: Existing callers should compile without changes (just different type). No new tests needed.
- E40: Unit test — `write_secrets_env_file` with `PLATFORM_SECRET_NAMES=ANTHROPIC_API_KEY,MY_SECRET` should only write `MY_SECRET`.
- E41: Verify timeout values are correct constants.
- E42: Unit test — `truncate_str("héllo", 3)` returns `"hél..."` (char-based, not byte-based).

---

## PR 2: Everything else — UI, MCP, Docker, CI, scripts

- [x] Implementation complete
- [x] UI build passes
- [x] MCP tests pass (40/40)
- [x] Bash syntax checks pass

### E44: Remove dead localStorage token code from Health page

**File:** `ui/src/pages/admin/Health.tsx`

Remove the `localStorage.getItem('token')` code (lines 139-141). EventSource sends cookies automatically via same-origin:

```tsx
// Before:
const token = localStorage.getItem('token');
const es = new EventSource(`/api/health/stream?token=${encodeURIComponent(token)}`);

// After:
const es = new EventSource('/api/health/stream');
```

### E45: Fix trace logs link to use trace_id

**File:** `ui/src/pages/observe/Traces.tsx`

Change the "View related logs" link (line 237):

```tsx
// Before:
<a href={`/observe/logs?trace_id=${selected.span_id}`}

// After:
<a href={`/observe/logs?trace_id=${traceId}`}
```

Where `traceId` is the trace-level ID from the URL params or the span's parent trace context. Check how `traceId` is available in the component — it should be a prop or URL param.

### E46: Add console.warn for silently swallowed API errors

**File:** Multiple UI pages

The pattern `.catch(() => {})` appears in many pages. Replace with `.catch(e => console.warn(e))` so errors are at least visible in devtools. This is a simple find-and-replace.

Target files: `Dashboard.tsx`, `ProjectDetail.tsx`, `Sessions.tsx`, and any others with `.catch(() => {})`.

### E48: Fix pipeline test parameter name

**File:** `mcp/tests/test-pipeline.js`

Change:
```javascript
// Before:
await client.callTool("trigger_pipeline", { branch: "main" });

// After:
await client.callTool("trigger_pipeline", { git_ref: "main" });
```

Also add body assertion:
```javascript
assert.equal(req.body.git_ref, "main");
```

### E49: Add missing tool assertions to core test

**File:** `mcp/tests/test-core.js`

Add assertions for all tools defined in `platform-core.js`. Check which tools are currently missing from the assertion list and add them.

### E50: Wrap browser server env var parse in try/catch

**File:** `mcp/servers/platform-browser.js`

```javascript
// Before:
const ALLOWED_ORIGINS = JSON.parse(process.env.BROWSER_ALLOWED_ORIGINS || "[]");

// After:
let ALLOWED_ORIGINS = [];
try {
  ALLOWED_ORIGINS = JSON.parse(process.env.BROWSER_ALLOWED_ORIGINS || "[]");
} catch {
  console.error("Invalid BROWSER_ALLOWED_ORIGINS JSON, using empty list");
}
```

### E51: Wrap client JSON.parse on success response

**File:** `mcp/lib/client.js`

```javascript
// Before:
if (!text) return null;
return JSON.parse(text);

// After:
if (!text) return null;
try {
  return JSON.parse(text);
} catch {
  throw new Error(`API ${method} ${path} returned non-JSON: ${text.slice(0, 200)}`);
}
```

### E52: Replace deprecated npm install --production

**File:** `docker/Dockerfile.platform-runner`

```dockerfile
# Before:
RUN cd /usr/local/lib/mcp && npm install --production

# After:
RUN cd /usr/local/lib/mcp && npm ci --omit=dev
```

**Note:** `npm ci` requires a `package-lock.json` in `mcp/`. If one doesn't exist, use `npm install --omit=dev` instead.

### E53: Use pipx instead of pip with --break-system-packages

**File:** `docker/Dockerfile.dev-pod`

```dockerfile
# Before:
RUN pip install --break-system-packages diff-cover

# After:
RUN pipx install diff-cover
```

Check that `pipx` is already installed in the dev-pod image. If not, install it first.

### E56: Add semver tag to release alongside :latest

**File:** `.github/workflows/release.yaml`

This is already addressed by E35 (workflow_run trigger). The `:latest` overwrite is acceptable for now — semver tagging would require a separate release process with tag-based triggers. Add a comment documenting this is intentional:

```yaml
          # :latest is overwritten on each CI-gated main push.
          # For versioned releases, create a git tag (vX.Y.Z) —
          # a tag-based release workflow can be added later.
          docker manifest create ${IMAGE}:latest \
```

Skip — no code change, just document.

### E57: Add missing dependabot entries

**File:** `.github/dependabot.yml`

Add entries for `mcp/`, `docs/viewer/`, and `cli/agent-runner`:

```yaml
  - package-ecosystem: "npm"
    directory: "/mcp"
    schedule:
      interval: "weekly"

  - package-ecosystem: "npm"
    directory: "/docs/viewer"
    schedule:
      interval: "weekly"

  - package-ecosystem: "cargo"
    directory: "/cli/agent-runner"
    schedule:
      interval: "weekly"
```

### E58: Delete legacy kind-up.sh / kind-down.sh

**Files:** `hack/kind-up.sh`, `hack/kind-down.sh`

Delete both files. `cluster-up.sh` / `cluster-down.sh` is the canonical approach (used by Justfile). These legacy scripts have diverged (different kubeconfig paths, missing Envoy Gateway install).

### E59: Fail on NodePort connectivity timeout

**File:** `hack/test-in-cluster.sh`

After the `for` loop (line ~147), add a failure check:

```bash
# After the loop:
if ! nc -z "$NODE_IP" "$PG_PORT" 2>/dev/null; then
  echo ""
  echo "ERROR: Could not connect to services after 15s"
  exit 1
fi
```

### Test Outline — PR 2

- E44, E45, E46: Manual — verify in browser
- E48, E49: `cd mcp && npm test` passes
- E50, E51: MCP tests cover error paths
- E52: Docker build test
- E57: `dependabot.yml` validates
- E59: `bash -n hack/test-in-cluster.sh` syntax check

---

## Skipped Findings (with rationale)

| Finding | Rationale |
|---|---|
| **E43** | SVG paths are hardcoded constants — `dangerouslySetInnerHTML` is safe here |
| **E47** | Already fixed in E1 batch (dompurify added) |
| **E54** | Already fixed in E31 batch (kubectl uses `dpkg --print-architecture`) |
| **E55** | Accepted design trade-off (DD-1) — agents need `--dangerously-skip-permissions` |
| **E56** | `:latest` overwrite acceptable for now; semver needs a separate release workflow |
| **E60** | Placeholder Helm repo URL is clearly documented as such |
| **E61** | By design — agent creates `requirements.txt`, `app/`, `static/` before first build |
| **E62** | `changeme` is acceptable for demo templates; comment warns about production |

---

## Summary

| PR | Scope | Findings | Effort |
|---|---|---|---|
| PR 1 | CLI: notification enum, secret filter, timeout, truncation | E39, E40, E41, E42 | Medium |
| PR 2 | UI + MCP + Docker + CI + scripts | E44-E53, E57-E59 | Medium |

**8 findings skipped** (already fixed or intentional).
