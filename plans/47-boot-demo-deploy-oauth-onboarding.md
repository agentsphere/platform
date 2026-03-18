# Plan 47: Auto-Deploy Demo on Boot + OAuth/API Key Onboarding

## Context

Currently the platform demo project is only created when the admin selects it during the onboarding wizard. This means users don't see a running deployment until after they finish onboarding and manually trigger a pipeline. The goal is to have the demo project automatically created and its pipeline triggered on every fresh boot, so by the time a user finishes onboarding, they see a live deployment.

Additionally, the onboarding wizard's provider token step currently only supports pasting an Anthropic API key. We want to offer two auth methods: **OAuth token** (uses Claude subscription, no extra cost) and **API key** (billed separately through Anthropic API). The OAuth path should support both pasting an existing token and a CLI-mediated flow where the backend runs `claude setup-token` to extract an OAuth URL for the user.

### Current state

- `src/store/bootstrap.rs` — creates admin user (dev) or setup token (prod) on first boot
- `src/api/setup.rs` — POST /api/setup creates first admin in prod mode
- `src/onboarding/demo_project.rs` — creates demo project (git repo, DB row, issues, K8s infra)
- `src/api/onboarding.rs` — wizard endpoints, demo project creation tied to wizard completion
- `src/pipeline/trigger.rs` — `on_api()` triggers pipeline programmatically
- `src/store/eventbus.rs` — `ImageBuilt` event auto-creates deployment after pipeline success
- `ui/src/pages/Onboarding.tsx` — 4-step wizard (org type → security → API key → demo checkbox)
- `src/auth/cli_creds.rs` — stores `oauth` or `setup_token` credentials (AES-256-GCM encrypted)
- `src/api/cli_auth.rs` — CRUD for CLI credentials

### Pipeline flow (already works end-to-end)

```
on_api() → create pipeline + steps → notify_executor()
  → executor runs kaniko build step → detect_and_write_deployment()
  → publish ImageBuilt event → eventbus handler
  → upsert deployment row (desired_status='active', current_status='pending')
  → deploy_notify.notify_one() → reconciler applies manifests to K8s
```

Steps with `only: events: [mr]` are automatically skipped by `step_matches()` when trigger is `"api"`, so only `build-app` runs.

## Design Principles

- **Zero-touch demo**: Fresh boot → demo project building automatically. User sees deployment in progress or complete by the time they finish onboarding.
- **Idempotent**: Demo creation checks `platform_settings.demo_project_id` before creating. Safe to restart.
- **Two trigger points**: Dev mode → after bootstrap. Prod mode → after POST /api/setup.
- **Simple OAuth flow**: Direct token paste always available. CLI-mediated flow as enhancement.
- **No wizard dependency**: Demo project is decoupled from the wizard — wizard is for org config + provider token only.

---

## PR 1: Auto-Deploy Demo Project on Fresh Boot

Decouple demo project creation from the wizard. Create + trigger pipeline automatically on first admin creation (both dev and prod).

- [x] Types & errors defined
- [x] Migration applied (none needed)
- [ ] Tests written (red phase)
- [x] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### No migration needed

No schema changes — uses existing `platform_settings`, `projects`, `pipelines`, `deployments` tables.

### Code Changes

| File | Change |
|---|---|
| `src/onboarding/demo_project.rs` | Add `create_and_trigger_demo()` — wraps `create_demo_project()` + triggers pipeline + notifies executor |
| `src/main.rs` | After bootstrap `DevAdmin`, spawn background task calling `create_and_trigger_demo()` |
| `src/api/setup.rs` | After creating admin user, spawn background task calling `create_and_trigger_demo()` |
| `src/api/onboarding.rs` | Remove demo project creation from `complete_wizard()`. Remove `create_demo` field from `WizardRequest`. Keep standalone `POST /api/onboarding/demo-project` endpoint for manual re-trigger. |
| `ui/src/pages/Onboarding.tsx` | Remove step 4 "Get Started" with demo checkbox. Wizard goes straight from provider token → submit. "Just Exploring" still fast-paths but without demo creation. |

### New function: `create_and_trigger_demo()`

```rust
// src/onboarding/demo_project.rs

/// Create demo project + trigger initial pipeline. Idempotent.
/// Designed to be spawned as a background task.
#[tracing::instrument(skip(state), fields(%admin_id), err)]
pub async fn create_and_trigger_demo(
    state: &AppState,
    admin_id: Uuid,
) -> Result<(), anyhow::Error> {
    // Idempotency: skip if demo project already exists
    if let Ok(Some(_)) = presets::get_setting(&state.pool, "demo_project_id").await {
        tracing::info!("demo project already exists, skipping auto-creation");
        return Ok(());
    }

    let (project_id, _name) = create_demo_project(state, admin_id).await?;

    // Look up repo_path for pipeline trigger
    let repo_path: String = sqlx::query_scalar(
        "SELECT repo_path FROM projects WHERE id = $1"
    )
    .bind(project_id)
    .fetch_one(&state.pool)
    .await?
    .ok_or_else(|| anyhow::anyhow!("demo project has no repo_path"))?;

    // Trigger pipeline on main branch
    match crate::pipeline::trigger::on_api(
        &state.pool,
        std::path::Path::new(&repo_path),
        project_id,
        "refs/heads/main",
        admin_id,
    ).await {
        Ok(pipeline_id) => {
            crate::pipeline::trigger::notify_executor(state, pipeline_id).await;
            tracing::info!(%project_id, %pipeline_id, "demo pipeline triggered");
        }
        Err(e) => {
            // Non-fatal: project exists, pipeline can be triggered later
            tracing::warn!(error = %e, %project_id, "demo pipeline trigger failed");
        }
    }

    Ok(())
}
```

### main.rs changes

After line 170 (bootstrap match), add:

```rust
// In dev mode, auto-create demo project after bootstrap
store::bootstrap::BootstrapResult::DevAdmin => {
    tracing::info!("dev mode: admin user created with default credentials");
    // Spawn demo project creation after background tasks are running
    let demo_state = state.clone();
    tokio::spawn(async move {
        // Small delay to let background tasks (executor, reconciler) start
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        // Look up admin user ID
        if let Ok(Some(admin_id)) = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM users WHERE name = 'admin'"
        ).fetch_optional(&demo_state.pool).await {
            if let Err(e) = onboarding::demo_project::create_and_trigger_demo(
                &demo_state, admin_id
            ).await {
                tracing::warn!(error = %e, "auto demo project creation failed");
            }
        }
    });
}
```

### setup.rs changes

At the end of the `setup()` handler (after audit log, before returning), add:

```rust
// Spawn demo project creation in background
let demo_state = state.clone();
let demo_admin_id = admin_id;
tokio::spawn(async move {
    // Small delay for executor/reconciler startup
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    if let Err(e) = crate::onboarding::demo_project::create_and_trigger_demo(
        &demo_state, demo_admin_id
    ).await {
        tracing::warn!(error = %e, "auto demo project creation failed");
    }
});
```

### Onboarding wizard changes

In `src/api/onboarding.rs`, `complete_wizard()`:
- Remove the `create_demo` field handling (lines 140-153)
- Remove `demo_project_id` from `WizardResponse` (always None now — demo is created separately)
- Keep the standalone `POST /api/onboarding/demo-project` endpoint for manual use

In `WizardRequest`:
```rust
pub struct WizardRequest {
    pub org_type: OrgType,
    pub passkey_policy: Option<PasskeyPolicy>,
    pub provider_key: Option<String>,
    // Removed: pub create_demo: Option<bool>,
}
```

In `WizardResponse`:
```rust
pub struct WizardResponse {
    pub success: bool,
    // Removed: pub demo_project_id: Option<Uuid>,
}
```

### UI changes

In `ui/src/pages/Onboarding.tsx`:
- Remove step 4 ("Get Started" with demo checkbox)
- `totalSteps` becomes `orgType === 'solo' || orgType === 'exploring' ? 2 : 3`
- Provider token step is now the last step, its "Continue" button becomes "Finish Setup"
- "Just Exploring" fast-path: still auto-submits (org_type=exploring, no demo field)
- After wizard completion, redirect to `/` (dashboard) where user will see demo project (created in background)

### Test Outline — PR 1

**New behaviors to test:**
- `create_and_trigger_demo()` creates project + triggers pipeline — integration
- `create_and_trigger_demo()` is idempotent (skips if demo_project_id exists) — integration
- Wizard completion no longer creates demo project — integration
- Wizard API accepts request without `create_demo` field — integration

**Error paths to test:**
- `create_and_trigger_demo()` fails gracefully if pipeline trigger fails — unit (mock)
- Demo creation after setup endpoint — integration

**Existing tests affected:**
- `tests/setup_integration.rs` — setup tests may need update if response changes
- Onboarding wizard tests (if any) — remove demo_project_id assertions

**Estimated test count:** ~3 unit + 4 integration

### Verification
- `just cluster-up && just run` — on fresh DB, demo project appears, pipeline runs, deployment reconciles
- Wizard skips demo creation step
- `POST /api/onboarding/wizard` works without `create_demo` field
- Restart server → `create_and_trigger_demo()` skips (idempotent)

---

## PR 2: OAuth/API Key Choice in Onboarding + Claude CLI Auth Flow

Revamp the provider token step to offer two auth methods with clear cost explanation. Add backend support for CLI-mediated OAuth flow via `claude setup-token`.

- [x] Types & errors defined
- [x] Migration applied (none needed)
- [x] Tests written (red phase) — 12 unit tests for parsing
- [x] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### No migration needed

Uses existing `cli_credentials` table (auth_type='oauth' or 'setup_token') and `user_provider_keys` table. No schema changes.

### Claude CLI `setup-token` behavior (verified)

```
$ claude setup-token
# Shows ASCII art banner, then:
# "Browser didn't open? Use the url below to sign in (c to copy)"
# https://claude.ai/oauth/authorize?code=true&client_id=...&response_type=code&redirect_uri=...&scope=user:inference&code_challenge=...&code_challenge_method=S256&state=...
# "Paste code here if prompted >"
# <waits for user input>
```

**Complete flow observed:**
```
$ claude setup-token
# <ASCII art banner>
# "Browser didn't open? Use the url below to sign in (c to copy)"
# https://claude.ai/oauth/authorize?code=true&client_id=...&response_type=code&redirect_uri=...&scope=user:inference&code_challenge=...&state=...
# "Paste code here if prompted >"
# <user pastes code, presses enter>
# "✓ Long-lived authentication token created successfully!"
# "Your OAuth token (valid for 1 year):"
# sk-ant-oat01-XXXXXXXXXXXXXXXXXXXX
# "Store this token securely. You won't be able to see it again."
# "Use this token by setting: export CLAUDE_CODE_OAUTH_TOKEN=<token>"
```

**Key constraints:**
- Requires a PTY (uses Ink/React terminal UI) — `script -q /dev/null` wrapper needed
- Does NOT support `-p`, `--session-id`, `--resume`, `--json-schema`, or `--output-format` flags
- Outputs ANSI-escaped content (needs stripping to extract URL and token)
- Uses OAuth 2.0 PKCE (code_challenge_method=S256)
- URL format: `https://claude.ai/oauth/authorize?code=true&client_id=...`
- Redirect: `https://platform.claude.com/oauth/code/callback` where user sees the code
- After code is pasted: CLI exchanges code+verifier → outputs token to stdout
- Token format: `sk-ant-oat01-...` (valid for 1 year)
- Token is printed to stdout (NOT written to a file) — we capture it from stdout

### Code Changes

| File | Change |
|---|---|
| `src/onboarding/mod.rs` | Add `pub mod claude_auth;` |
| `src/onboarding/claude_auth.rs` | **New** — Claude CLI subprocess management for OAuth flow |
| `src/api/onboarding.rs` | Add endpoints for claude-auth flow. Add `cli_token` field to wizard. Add to router. |
| `src/store/mod.rs` | Add `cli_auth_manager` field to `AppState` |
| `src/main.rs` | Initialize `CliAuthManager` in AppState construction |
| `tests/helpers/mod.rs` | Add `cli_auth_manager` to `test_state()` |
| `tests/e2e_helpers/mod.rs` | Add `cli_auth_manager` to `e2e_state()` |
| `ui/src/pages/Onboarding.tsx` | Revamp provider step: two cards (OAuth vs API Key), OAuth flow with link + auth code input |
| `ui/src/style.css` | Auth option card styles |

### Backend: Claude CLI Auth Flow

**New module: `src/onboarding/claude_auth.rs`**

Manages a stateful `claude setup-token` subprocess spawned via PTY wrapper.

```rust
use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::Mutex;
use uuid::Uuid;

/// State of a Claude CLI auth session.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AuthSessionState {
    /// Process starting, extracting URL from stdout.
    Starting,
    /// URL extracted, waiting for user to visit link and provide code.
    UrlReady { auth_url: String },
    /// Code sent to process, waiting for CLI to exchange for token.
    Verifying,
    /// Token received, flow complete.
    Completed,
    /// Process failed or timed out.
    Failed { error: String },
}

/// An active CLI auth session (not Clone — owns process handle).
struct AuthSession {
    state: AuthSessionState,
    stdin: Option<ChildStdin>,
    child: Option<Child>,
    user_id: Uuid,
    created_at: std::time::Instant,
    /// Isolated config dir (temp dir) where CLI writes credentials.
    config_dir: Option<std::path::PathBuf>,
}

/// Manages active CLI auth sessions.
pub struct CliAuthManager {
    sessions: Mutex<HashMap<Uuid, AuthSession>>,
}
```

**`start_auth()` — spawn CLI, extract URL, return immediately**

```rust
/// Spawn `claude setup-token` via PTY wrapper, extract the OAuth URL.
/// Returns (session_id, auth_url) or error.
///
/// The process stays alive waiting for the auth code on stdin.
#[tracing::instrument(skip(self), fields(%user_id), err)]
pub async fn start_auth(
    &self,
    user_id: Uuid,
    claude_cli_path: &str, // e.g. "claude" or "/usr/local/bin/claude"
) -> Result<(Uuid, String), anyhow::Error> {
    let mut sessions = self.sessions.lock().await;

    // Max 1 concurrent session per user — kill existing if any
    sessions.retain(|_, s| s.user_id != user_id);

    // Create isolated config dir so CLI writes creds there (not ~/.claude/)
    let config_dir = tempfile::tempdir()?.into_path();

    // Spawn via `script` PTY wrapper (required by Ink TUI)
    // macOS: script -q /dev/null <cmd>
    // Linux: script -qc "<cmd>" /dev/null
    let (script_cmd, script_args) = if cfg!(target_os = "macos") {
        ("script", vec!["-q", "/dev/null", claude_cli_path, "setup-token"])
    } else {
        let cmd_str = format!("{} setup-token", claude_cli_path);
        ("script", vec!["-qc", &cmd_str, "/dev/null"])
    };

    let mut child = tokio::process::Command::new(script_cmd)
        .args(&script_args)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
        .env("TMPDIR", std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into()))
        .env("CLAUDE_CONFIG_DIR", &config_dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    let stdout = child.stdout.take()
        .ok_or_else(|| anyhow::anyhow!("failed to capture stdout"))?;
    let stdin = child.stdin.take()
        .ok_or_else(|| anyhow::anyhow!("failed to capture stdin"))?;

    // Read stdout until we find the OAuth URL (with timeout)
    let auth_url = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        extract_oauth_url(stdout),
    ).await
    .map_err(|_| anyhow::anyhow!("timeout waiting for OAuth URL from CLI"))?
    .map_err(|e| anyhow::anyhow!("failed to extract OAuth URL: {e}"))?;

    let session_id = Uuid::new_v4();
    sessions.insert(session_id, AuthSession {
        state: AuthSessionState::UrlReady { auth_url: auth_url.clone() },
        stdin: Some(stdin),
        child: Some(child),
        user_id,
        created_at: std::time::Instant::now(),
        config_dir: Some(config_dir),
    });

    tracing::info!(%session_id, %user_id, "claude auth session started");
    Ok((session_id, auth_url))
}
```

**URL extraction from ANSI output:**

```rust
/// Read PTY stdout, strip ANSI escape codes, find `https://claude.ai/oauth/authorize?...` URL.
async fn extract_oauth_url(
    stdout: tokio::process::ChildStdout,
) -> Result<String, anyhow::Error> {
    let mut reader = BufReader::new(stdout);
    let mut buf = Vec::new();
    let mut accumulated = String::new();

    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf).await?;
        if n == 0 {
            return Err(anyhow::anyhow!("CLI exited before producing URL"));
        }
        let line = String::from_utf8_lossy(&buf);
        let clean = strip_ansi_escapes(&line);
        accumulated.push_str(&clean);

        // Look for the OAuth URL in accumulated output
        if let Some(url) = find_oauth_url(&accumulated) {
            return Ok(url);
        }
    }
}

/// Strip ANSI escape sequences from terminal output.
fn strip_ansi_escapes(s: &str) -> String {
    // Regex: ESC [ ... final_byte  or  ESC ] ... ST
    let re = regex::Regex::new(r"\x1b\[[0-9;?]*[a-zA-Z]|\x1b\][^\x07]*\x07|\x1b\[[\d;]*m")
        .expect("valid regex");
    re.replace_all(s, "").to_string()
}

/// Find OAuth URL in text. Returns the full URL starting with https://claude.ai/oauth/
fn find_oauth_url(text: &str) -> Option<String> {
    let marker = "https://claude.ai/oauth/authorize?";
    if let Some(start) = text.find(marker) {
        // URL ends at whitespace or newline
        let url_part = &text[start..];
        let end = url_part.find(|c: char| c.is_whitespace() || c == '\n' || c == '\r')
            .unwrap_or(url_part.len());
        Some(url_part[..end].to_string())
    } else {
        None
    }
}
```

**`send_code()` — pipe auth code to CLI, capture token from stdout:**

The key insight: `claude setup-token` outputs the token (`sk-ant-oat01-...`) directly to stdout
after the code is submitted. We capture it from the same stdout stream we used for the URL.

To support this, the `start_auth()` function must keep a handle to the stdout reader.
We store a `tokio::sync::oneshot::Receiver<String>` in the session that will resolve
when the background stdout reader finds the token.

**Modified session struct:**

```rust
struct AuthSession {
    state: AuthSessionState,
    stdin: Option<ChildStdin>,
    child: Option<Child>,
    user_id: Uuid,
    created_at: std::time::Instant,
    config_dir: Option<std::path::PathBuf>,
    /// Receives the token from the background stdout reader task.
    token_rx: Option<tokio::sync::oneshot::Receiver<String>>,
}
```

**Modified `start_auth()` — spawn background reader that continues after URL:**

After extracting the URL from stdout, the background task keeps reading. When it finds
`sk-ant-oat01-` in the output, it sends the token through the oneshot channel.

```rust
// In start_auth(), after extracting auth_url:
let (token_tx, token_rx) = tokio::sync::oneshot::channel::<String>();

// Spawn background task to keep reading stdout for the token
tokio::spawn(async move {
    if let Ok(token) = extract_token_from_stdout(remaining_stdout_reader).await {
        let _ = token_tx.send(token);
    }
});

// Store token_rx in session
sessions.insert(session_id, AuthSession {
    // ...
    token_rx: Some(token_rx),
});
```

**Token extraction from stdout:**

```rust
/// Continue reading stdout after URL was found, looking for the token.
/// Token format: sk-ant-oat01-XXXXX (on its own line, after "Your OAuth token" text).
async fn extract_token_from_stdout(
    mut reader: BufReader<tokio::process::ChildStdout>,
) -> Result<String, anyhow::Error> {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf).await?;
        if n == 0 {
            return Err(anyhow::anyhow!("CLI exited before producing token"));
        }
        let line = String::from_utf8_lossy(&buf);
        let clean = strip_ansi_escapes(&line);

        if let Some(token) = find_oauth_token(&clean) {
            return Ok(token);
        }
    }
}

/// Find `sk-ant-oat01-...` token in text.
fn find_oauth_token(text: &str) -> Option<String> {
    // Token starts with sk-ant-oat and is a long alphanumeric+hyphen+underscore string
    let re = regex::Regex::new(r"(sk-ant-oat\S+)").expect("valid regex");
    re.find(text).map(|m| m.as_str().trim().to_string())
}
```

**`send_code()` implementation:**

```rust
/// Send the authentication code to the waiting CLI process.
/// Waits for the background stdout reader to capture the resulting token.
#[tracing::instrument(skip(self, code), fields(%session_id), err)]
pub async fn send_code(
    &self,
    session_id: Uuid,
    code: &str,
    pool: &sqlx::PgPool,
    master_key: &[u8; 32],
) -> Result<(), anyhow::Error> {
    let (user_id, token_rx) = {
        let mut sessions = self.sessions.lock().await;
        let session = sessions.get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("session not found"))?;

        // Write code to stdin
        let stdin = session.stdin.as_mut()
            .ok_or_else(|| anyhow::anyhow!("stdin already consumed"))?;
        stdin.write_all(code.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;

        session.state = AuthSessionState::Verifying;

        let user_id = session.user_id;
        let token_rx = session.token_rx.take()
            .ok_or_else(|| anyhow::anyhow!("token receiver already consumed"))?;

        (user_id, token_rx)
    }; // Release lock

    // Wait for token from background stdout reader (timeout 30s)
    let token = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        token_rx,
    ).await
    .map_err(|_| anyhow::anyhow!("timeout waiting for token"))?
    .map_err(|_| anyhow::anyhow!("stdout reader task failed"))?;

    // Store token in cli_credentials (auth_type = "setup_token" for long-lived OAT)
    crate::auth::cli_creds::store_credentials(
        pool, master_key, user_id, "setup_token", &token, None,
    ).await?;

    // Update session state + clean up
    let mut sessions = self.sessions.lock().await;
    if let Some(session) = sessions.get_mut(&session_id) {
        session.state = AuthSessionState::Completed;
        if let Some(mut child) = session.child.take() {
            let _ = child.kill().await;
        }
        session.stdin.take();
    }

    tracing::info!(%session_id, %user_id, "claude auth completed — token stored");
    Ok(())
}
```

**Cleanup and timeout:**

```rust
/// Cancel an auth session, kill the process, clean up temp dir.
pub async fn cancel(&self, session_id: Uuid) {
    let mut sessions = self.sessions.lock().await;
    if let Some(mut session) = sessions.remove(&session_id) {
        if let Some(mut child) = session.child.take() {
            let _ = child.kill().await;
        }
        if let Some(dir) = session.config_dir.take() {
            let _ = tokio::fs::remove_dir_all(&dir).await;
        }
    }
}

/// Get current state of a session.
pub async fn get_state(&self, session_id: Uuid) -> Option<AuthSessionState> {
    let sessions = self.sessions.lock().await;
    sessions.get(&session_id).map(|s| s.state.clone())
}

/// Evict sessions older than 5 minutes (called periodically).
pub async fn evict_stale(&self) {
    let mut sessions = self.sessions.lock().await;
    let threshold = std::time::Duration::from_secs(300);
    let stale: Vec<Uuid> = sessions.iter()
        .filter(|(_, s)| s.created_at.elapsed() > threshold)
        .map(|(id, _)| *id)
        .collect();
    for id in stale {
        if let Some(mut session) = sessions.remove(&id) {
            if let Some(mut child) = session.child.take() {
                let _ = child.kill().await;
            }
            if let Some(dir) = session.config_dir.take() {
                let _ = tokio::fs::remove_dir_all(&dir).await;
            }
            tracing::debug!(%id, "evicted stale claude auth session");
        }
    }
}
```

### API Endpoints

Added to `src/api/onboarding.rs` router:

```rust
.route("/api/onboarding/claude-auth/start", post(start_claude_auth))
.route("/api/onboarding/claude-auth/:id", get(claude_auth_status).delete(cancel_claude_auth))
.route("/api/onboarding/claude-auth/:id/code", post(submit_auth_code))
```

**`POST /api/onboarding/claude-auth/start`** — Start CLI auth flow
- Auth: `AuthUser` + `require_admin`
- Rate limit: 5 per hour
- Spawns `claude setup-token` via PTY wrapper
- Blocks until URL is extracted from stdout (up to 30s)
- Returns: `{ session_id: Uuid, auth_url: String }` or error
- This is the endpoint the frontend calls when user clicks the OAuth card — shows spinner until URL arrives

**`GET /api/onboarding/claude-auth/:id`** — Check auth status
- Auth: `AuthUser`
- Returns: `AuthSessionState` (serialized as tagged JSON)
- Frontend uses this to check if code submission completed successfully

**`POST /api/onboarding/claude-auth/:id/code`** — Submit the authentication code
- Auth: `AuthUser`
- Body: `{ code: string }`
- Pipes code to CLI stdin, waits for token, stores in `cli_credentials`
- Returns: `{ success: bool }` or error

**`DELETE /api/onboarding/claude-auth/:id`** — Cancel flow
- Auth: `AuthUser`
- Kills process, cleans up temp dir
- Returns: 204

### AppState change

```rust
// src/store/mod.rs
pub struct AppState {
    // ... existing fields ...
    pub cli_auth_manager: Arc<crate::onboarding::claude_auth::CliAuthManager>,
}
```

**Impact:** Must update `test_state()` in both `tests/helpers/mod.rs` and `tests/e2e_helpers/mod.rs`.

### UI Changes: Revamped Provider Token Step

The provider step shows two cards. When "OAuth" is selected, it immediately calls `/start` to spawn the CLI and get the URL. The UX flow:

1. User clicks **"Claude Subscription"** card
2. Spinner appears ("Connecting to Claude...")
3. Backend spawns CLI, extracts URL, returns it
4. UI shows: link to open Claude auth page + **"Authentication Code"** input
5. User clicks link → opens `claude.ai` in new tab → authenticates → sees code
6. **OR** user already has an OAuth token → pastes it directly (separate input visible before clicking link)
7. User pastes code into "Authentication Code" input
8. On input change (debounced): auto-submit code to backend
9. Spinner while verifying → green checkmark on success

```tsx
{step === providerStep && (
  <div class="wizard-step">
    <h1>Connect to Claude</h1>
    <p>Choose how agents authenticate with Claude</p>

    {/* Two option cards */}
    <div class="auth-option-grid">
      <div
        class={`auth-option-card${authMethod === 'oauth' ? ' selected' : ''}`}
        onClick={() => { setAuthMethod('oauth'); startOAuthFlow(); }}
      >
        <div class="auth-option-title">Claude Subscription</div>
        <div class="auth-option-desc">
          Uses your existing Claude Pro/Team plan. No extra cost — counts
          toward your subscription usage.
        </div>
        <div class="auth-option-badge">Recommended</div>
      </div>

      <div
        class={`auth-option-card${authMethod === 'api_key' ? ' selected' : ''}`}
        onClick={() => setAuthMethod('api_key')}
      >
        <div class="auth-option-title">Anthropic API Key</div>
        <div class="auth-option-desc">
          Billed separately through the Anthropic API. Pay per token used.
        </div>
      </div>
    </div>

    {/* OAuth flow */}
    {authMethod === 'oauth' && (
      <div class="auth-input-area">
        {/* Phase 1: Starting CLI / loading URL */}
        {oauthPhase === 'starting' && (
          <div class="auth-loading">
            <span class="spinner" /> Connecting to Claude...
          </div>
        )}

        {/* Phase 1b: Error spawning CLI */}
        {oauthPhase === 'error' && (
          <div>
            <p class="text-danger text-sm">
              Could not start Claude login. You can paste an existing token instead.
            </p>
            <label>OAuth Token</label>
            <input type="password" placeholder="sk-ant-ccode01-..." value={manualToken}
              onInput={e => setManualToken(e.currentTarget.value)} />
          </div>
        )}

        {/* Phase 2: URL ready — show link + auth code input */}
        {oauthPhase === 'url_ready' && (
          <div>
            {/* Option A: User has existing token — show paste field */}
            {!clickedLink && (
              <div style="margin-bottom: 1rem">
                <label>Have an existing OAuth token?</label>
                <input type="password" placeholder="sk-ant-ccode01-..." value={manualToken}
                  onInput={e => setManualToken(e.currentTarget.value)} />
              </div>
            )}

            {/* Option B: Use the link flow */}
            <div>
              <a href={authUrl} target="_blank" class="btn btn-primary"
                onClick={() => { setClickedLink(true); setManualToken(''); }}>
                Open Claude Login Page
              </a>
            </div>

            {/* After clicking link: show Authentication Code input */}
            {clickedLink && (
              <div style="margin-top: 1rem">
                <label>Authentication Code</label>
                <p class="text-muted text-sm">
                  After authenticating on claude.ai, paste the code shown on the callback page.
                </p>
                <input class="input" placeholder="Paste authentication code..."
                  value={authCode}
                  onInput={e => {
                    const code = e.currentTarget.value;
                    setAuthCode(code);
                    if (code.length > 10) submitCode(code); // auto-submit
                  }} />
              </div>
            )}
          </div>
        )}

        {/* Phase 3: Verifying code */}
        {oauthPhase === 'verifying' && (
          <div class="auth-loading">
            <span class="spinner" /> Verifying authentication code...
          </div>
        )}

        {/* Phase 4: Complete */}
        {oauthPhase === 'completed' && (
          <div class="auth-success">
            <span class="checkmark">✓</span> Connected to Claude successfully
          </div>
        )}
      </div>
    )}

    {/* API Key flow (unchanged) */}
    {authMethod === 'api_key' && (
      <div class="auth-input-area">
        <label>Anthropic API Key</label>
        <div style="display:flex;gap:0.5rem">
          <input class="input" type="password" placeholder="sk-ant-api03-..."
            value={apiKey}
            onInput={e => { setApiKey(e.currentTarget.value); setKeyValid(null); }} />
          <button class="btn btn-primary" onClick={validateKey}
            disabled={!apiKey.trim() || validating}>
            {validating ? 'Checking...' : 'Validate'}
          </button>
        </div>
        {keyValid === true && <p class="text-success text-sm">API key verified</p>}
        {keyValid === false && <p class="text-danger text-sm">Invalid API key</p>}
      </div>
    )}

    <button class="btn btn-ghost text-sm" style="width:100%;margin-top:0.25rem"
      onClick={() => finishSetup()}>
      Skip — I'll do this later
    </button>

    <div class="wizard-actions">
      <button class="btn btn-ghost" onClick={goBack}>Back</button>
      <button class="btn btn-primary" onClick={finishSetup}
        disabled={submitting}>
        {submitting ? 'Finishing...' : 'Finish Setup'}
      </button>
    </div>
  </div>
)}
```

**Frontend helper functions:**

```tsx
const startOAuthFlow = async () => {
  if (oauthPhase !== 'idle' && oauthPhase !== 'error') return;
  setOauthPhase('starting');
  try {
    const resp = await api.post<{ session_id: string; auth_url: string }>(
      '/api/onboarding/claude-auth/start'
    );
    setAuthSessionId(resp.session_id);
    setAuthUrl(resp.auth_url);
    setOauthPhase('url_ready');
  } catch {
    setOauthPhase('error');
  }
};

const submitCode = async (code: string) => {
  if (!authSessionId || oauthPhase === 'verifying') return;
  setOauthPhase('verifying');
  try {
    await api.post(`/api/onboarding/claude-auth/${authSessionId}/code`, { code });
    setOauthPhase('completed');
  } catch {
    setOauthPhase('url_ready'); // let them retry
  }
};
```

### Wizard submit changes

When submitting, the wizard now:
- If OAuth completed via CLI flow: token already stored in `cli_credentials` by the `/code` endpoint
- If manual OAuth token pasted: stores via `cli_token` field in wizard request
- If API key provided: stores via existing `provider_key` field in wizard request
- If skipped: neither is set

The `WizardRequest` gets a new optional field:

```rust
pub struct WizardRequest {
    pub org_type: OrgType,
    pub passkey_policy: Option<PasskeyPolicy>,
    /// Anthropic API key (stored in user_provider_keys)
    pub provider_key: Option<String>,
    /// CLI credential token — for manual paste (stored in cli_credentials)
    pub cli_token: Option<CliTokenInput>,
}

#[derive(Debug, Deserialize)]
pub struct CliTokenInput {
    pub auth_type: String,  // "oauth" or "setup_token"
    pub token: String,
}
```

In `complete_wizard()`, after saving provider_key, also handle `cli_token`:

```rust
if let Some(ref cli_token) = body.cli_token {
    let master_key_hex = state.config.master_key.as_deref()
        .ok_or_else(|| ApiError::BadRequest("master key not configured".into()))?;
    let master_key = crate::secrets::engine::parse_master_key(master_key_hex)
        .map_err(ApiError::Internal)?;
    crate::auth::cli_creds::store_credentials(
        &state.pool, &master_key, auth.user_id,
        &cli_token.auth_type, &cli_token.token, None,
    ).await.map_err(ApiError::Internal)?;
}
```

### CSS additions

Add to `ui/src/style.css`:

```css
.auth-option-grid {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 0.75rem;
  margin-bottom: 1rem;
}

.auth-option-card {
  padding: 1rem;
  border: 1px solid var(--border);
  border-radius: 8px;
  cursor: pointer;
  transition: all 0.15s ease;
}

.auth-option-card:hover {
  border-color: var(--accent);
}

.auth-option-card.selected {
  border-color: var(--accent);
  background: var(--accent-bg);
}

.auth-option-badge {
  display: inline-block;
  font-size: 11px;
  padding: 2px 6px;
  border-radius: 4px;
  background: var(--accent);
  color: white;
  margin-top: 0.5rem;
}

.auth-input-area {
  padding: 1rem;
  border: 1px solid var(--border);
  border-radius: 8px;
  margin-bottom: 1rem;
}

.auth-loading {
  display: flex;
  align-items: center;
  gap: 0.5rem;
  color: var(--text-muted);
  font-size: 13px;
  padding: 0.5rem 0;
}

.auth-success {
  display: flex;
  align-items: center;
  gap: 0.5rem;
  color: var(--success);
  font-size: 13px;
  padding: 0.5rem 0;
}

.auth-success .checkmark {
  font-size: 16px;
  font-weight: bold;
}
```

### Test Outline — PR 2

**New behaviors to test:**
- `strip_ansi_escapes()` correctly strips ANSI from PTY output — unit
- `find_oauth_url()` extracts URL from mixed ANSI + text — unit
- `find_oauth_token()` extracts `sk-ant-oat01-...` from CLI output — unit
- `find_oauth_token()` ignores partial/invalid tokens — unit
- `CliAuthManager` concurrent session limit (1 per user, old evicted) — unit
- `CliAuthManager` stale session eviction (>5 min) — unit
- OAuth token storage via wizard `cli_token` field — integration
- API key storage still works via `provider_key` field — integration
- `POST /api/onboarding/claude-auth/start` returns error when CLI not found — integration
- `GET /api/onboarding/claude-auth/:id` returns 404 for unknown session — integration
- `DELETE /api/onboarding/claude-auth/:id` cleans up — integration

**Error paths to test:**
- CLI not installed → start returns error — integration
- Invalid session_id → 404 — integration
- Code submission on non-existent session → error — integration
- Rate limit on auth start — integration

**Existing tests affected:**
- `tests/helpers/mod.rs` — add `cli_auth_manager` to `test_state()`
- `tests/e2e_helpers/mod.rs` — add `cli_auth_manager` to `e2e_state()`
- Wizard integration tests — update request bodies (remove `create_demo`, add optional `cli_token`)

**Estimated test count:** ~5 unit + 6 integration

### Verification
- Clicking OAuth card → spinner → link appears (or error with paste fallback)
- Clicking link → opens claude.ai in new tab
- OAuth token paste input hidden after clicking link, Auth Code input shown
- Pasting code auto-verifies → green checkmark
- API key flow still works unchanged
- "Skip" still works
- Token stored correctly in `cli_credentials` table
- `CLAUDE_CODE_OAUTH_TOKEN` injected into agent pods when token is stored
- Cleanup kills subprocess and removes temp config dir

---

## Cross-cutting Concerns

### Both PRs
- [ ] Auth: all new endpoints use `AuthUser` extractor
- [ ] Permissions: `require_admin` on all onboarding endpoints
- [ ] Input validation: token length checks, auth_type whitelist
- [ ] Audit logging: auth flow start/complete/cancel logged (never log token values)
- [ ] No `.unwrap()` in production code
- [ ] `tracing::instrument` on new async functions
- [ ] Sensitive data never logged (tokens, OAuth codes)
- [ ] AppState changes → test helper updates in BOTH `tests/helpers/mod.rs` AND `tests/e2e_helpers/mod.rs`
- [ ] `.sqlx/` offline cache unchanged (no new compile-time queries in src/)
- [ ] Rate limiting on brute-forceable endpoints (auth flow start)

### Security
- CLI process spawned with cleared environment (only PATH/HOME/TMPDIR/CLAUDE_CONFIG_DIR)
- Isolated `CLAUDE_CONFIG_DIR` (temp dir) prevents reading/writing user's real `~/.claude/`
- 5-minute timeout prevents zombie processes (evict_stale called periodically)
- Max 1 concurrent auth session per user prevents resource exhaustion
- Tokens encrypted at rest (AES-256-GCM via PLATFORM_MASTER_KEY)
- OAuth codes are one-use and time-limited (handled by Claude's OAuth server)
- No token values in audit logs or tracing output
- PTY wrapper (`script`) is standard on Linux/macOS — no additional dependencies
