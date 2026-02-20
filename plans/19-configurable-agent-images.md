# Plan 19 — Configurable Agent Container Images

## Overview

Remove the hardcoded `platform-claude-runner:latest` agent container image and allow projects to specify custom runtime environments. This enables agents to work in Go, Rust, Python, or multi-language stacks instead of being limited to the Node.js-based default image.

**This corresponds to Agent DX Phase B from Plan 14.**

---

## Motivation

- The current agent image is `node:22-slim`-based (`docker/Dockerfile.claude-runner`), suitable only for Node.js/JavaScript projects
- Projects using Go, Rust, Python, Java, or multi-language stacks need different base images with appropriate toolchains
- DevOps teams need the ability to include custom tools (terraform, helm, kubectl, etc.) in agent environments
- Per-session image override enables experimentation without changing project defaults

---

## Prerequisites

| Requirement | Status |
|---|---|
| Agent orchestration (Phase 07) | Complete |
| Pod builder (`src/agent/claude_code/pod.rs`) | Complete |
| Agent service (`src/agent/service.rs`) | Complete |
| Session API (`src/api/sessions.rs`) | Complete |
| Validation module (`src/validation.rs`) | Complete |

---

## Architecture

### Image Resolution Priority

When building an agent pod, the container image is resolved in this order:

1. **Session override**: `config.image` in `CreateSessionRequest` (per-session)
2. **Project default**: `agent_image` column on `projects` table (per-project)
3. **Platform default**: `platform-claude-runner:latest` (hardcoded fallback)

```
Session config.image → Project agent_image → "platform-claude-runner:latest"
```

### Setup Commands

In addition to a custom image, users can specify `setup_commands` — shell commands that run after git clone but before Claude starts. These run in a second init container using the resolved image.

Use cases:
- `cargo build` — pre-compile Rust project so Claude can iterate faster
- `pip install -r requirements.txt` — install Python dependencies
- `go mod download` — cache Go modules
- `npm install` — install Node dependencies (for non-default images)

### Security Considerations

- **Image names must be validated** — reject shell metacharacters to prevent injection into pod specs
- **Setup commands run in the agent's namespace** — sandboxed by K8s pod security policies
- **No arbitrary Dockerfile execution** — only pre-built images are supported
- **Image pull policy**: Use `IfNotPresent` for tagged images, `Always` for `:latest`

---

## Detailed Implementation

### Step B1: Database Migration

**New: `migrations/YYYYMMDDHHMMSS_agent_image_config.up.sql`**

```sql
-- Add optional agent container image override per project
ALTER TABLE projects ADD COLUMN agent_image TEXT;

-- Comment for documentation
COMMENT ON COLUMN projects.agent_image IS
  'Custom container image for agent sessions. Null uses platform default.';
```

**New: `migrations/YYYYMMDDHHMMSS_agent_image_config.down.sql`**

```sql
ALTER TABLE projects DROP COLUMN IF EXISTS agent_image;
```

After migration: `just db-migrate && just db-prepare`

---

### Step B2: Image Validation (`src/validation.rs`)

**Add: `check_container_image()` function**

```rust
/// Validates a container image reference.
///
/// Accepts: `registry/image:tag`, `image:tag`, `image@sha256:abc...`,
///          `gcr.io/project/image:tag`, `localhost:5000/image:tag`
///
/// Rejects: shell metacharacters, empty strings, strings > 500 chars,
///          strings containing `;`, `&`, `|`, `$`, backtick, quotes, `\`, newlines
pub fn check_container_image(image: &str) -> Result<(), ApiError> {
    check_length("image", image, 1, 500)?;

    // Block shell injection characters
    let forbidden = [';', '&', '|', '$', '`', '\'', '"', '\\', '\n', '\r', ' ', '\t'];
    if image.chars().any(|c| forbidden.contains(&c)) {
        return Err(ApiError::BadRequest(
            "image: contains forbidden characters".into(),
        ));
    }

    // Must contain at least one alphanumeric character
    if !image.chars().any(|c| c.is_ascii_alphanumeric()) {
        return Err(ApiError::BadRequest(
            "image: must contain alphanumeric characters".into(),
        ));
    }

    Ok(())
}
```

**Add: `check_setup_commands()` function**

```rust
/// Validates setup commands for agent sessions.
///
/// Max 20 commands, each 1-2000 characters.
/// Commands are joined with `&&` and executed in a shell.
pub fn check_setup_commands(commands: &[String]) -> Result<(), ApiError> {
    if commands.len() > 20 {
        return Err(ApiError::BadRequest(
            "setup_commands: max 20 commands".into(),
        ));
    }
    for (i, cmd) in commands.iter().enumerate() {
        check_length(&format!("setup_commands[{i}]"), cmd, 1, 2000)?;
    }
    Ok(())
}
```

**Unit tests for both validators:**

```rust
#[cfg(test)]
mod tests {
    // check_container_image
    #[test]
    fn valid_images() {
        for img in ["golang:1.23", "node:22-slim", "rust:1.80", "ghcr.io/org/image:v1.2",
                     "localhost:5000/my-app:latest", "image@sha256:abcdef1234567890"] {
            assert!(check_container_image(img).is_ok(), "should accept: {img}");
        }
    }

    #[test]
    fn rejected_images() {
        for img in ["", "a".repeat(501).as_str(), "image;rm -rf /", "img & echo",
                     "img | cat", "$(evil)", "`evil`", "img\nevil", "has space"] {
            assert!(check_container_image(img).is_err(), "should reject: {img}");
        }
    }

    // check_setup_commands
    #[test]
    fn valid_setup_commands() {
        assert!(check_setup_commands(&["npm install".into()]).is_ok());
        assert!(check_setup_commands(&vec!["cmd".into(); 20]).is_ok());
    }

    #[test]
    fn rejected_setup_commands() {
        // Too many commands
        assert!(check_setup_commands(&vec!["cmd".into(); 21]).is_err());
        // Empty command
        assert!(check_setup_commands(&["".into()]).is_err());
        // Command too long
        assert!(check_setup_commands(&["a".repeat(2001)]).is_err());
    }
}
```

---

### Step B3: Extend `ProviderConfig` (`src/agent/provider.rs`)

**Add fields to `ProviderConfig`:**

```rust
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct ProviderConfig {
    pub model: Option<String>,
    pub max_turns: Option<i32>,
    pub role: Option<String>,
    pub image: Option<String>,              // NEW: container image override
    pub setup_commands: Option<Vec<String>>, // NEW: post-clone setup commands
}
```

No additional changes needed — `ProviderConfig` is already stored as JSON in the `provider_config` column.

---

### Step B4: Pod Builder Changes (`src/agent/claude_code/pod.rs`)

**Extend `PodBuildParams`:**

```rust
pub struct PodBuildParams<'a> {
    pub session: &'a AgentSession,
    pub config: &'a ProviderConfig,
    pub agent_api_token: &'a str,
    pub platform_api_url: &'a str,
    pub repo_clone_url: &'a str,
    pub namespace: &'a str,
    pub project_agent_image: Option<&'a str>,  // NEW: project-level default
}
```

**Add image resolution function:**

```rust
/// Resolves the container image for an agent pod.
///
/// Priority: session config > project default > platform default
fn resolve_image(config: &ProviderConfig, project_image: Option<&str>) -> String {
    config.image.as_deref()
        .or(project_image)
        .unwrap_or("platform-claude-runner:latest")
        .to_string()
}

/// Determines the image pull policy based on the image tag.
fn image_pull_policy(image: &str) -> String {
    if image.ends_with(":latest") || !image.contains(':') {
        "Always".to_string()
    } else {
        "IfNotPresent".to_string()
    }
}
```

**Modify `build_main_container()`:**

Current:
```rust
let main_container = Container {
    name: "claude".into(),
    image: Some("platform-claude-runner:latest".into()),
    // ...
};
```

New:
```rust
let resolved_image = resolve_image(params.config, params.project_agent_image);
let pull_policy = image_pull_policy(&resolved_image);

let main_container = Container {
    name: "claude".into(),
    image: Some(resolved_image),
    image_pull_policy: Some(pull_policy),
    // ... rest unchanged
};
```

**Add optional setup init container:**

If `config.setup_commands` is provided and non-empty, add a second init container that runs after `git-clone` but before the main container:

```rust
fn build_init_containers(params: &PodBuildParams) -> Vec<Container> {
    let mut init_containers = vec![
        build_git_clone_container(params),  // Always first
    ];

    // Optional setup container (runs after clone, before claude)
    if let Some(ref commands) = params.config.setup_commands {
        if !commands.is_empty() {
            let resolved_image = resolve_image(params.config, params.project_agent_image);
            let joined = commands.join(" && ");
            init_containers.push(Container {
                name: "setup".into(),
                image: Some(resolved_image),
                command: Some(vec!["sh".into(), "-c".into(), joined]),
                working_dir: Some("/workspace".into()),
                volume_mounts: Some(vec![workspace_volume_mount()]),
                resources: Some(setup_resource_requirements()),
                ..Default::default()
            });
        }
    }

    init_containers
}
```

**Unit tests:**

```rust
#[cfg(test)]
mod tests {
    #[test]
    fn resolve_image_session_override() {
        let config = ProviderConfig { image: Some("golang:1.23".into()), ..Default::default() };
        assert_eq!(resolve_image(&config, Some("rust:1.80")), "golang:1.23");
    }

    #[test]
    fn resolve_image_project_default() {
        let config = ProviderConfig::default();
        assert_eq!(resolve_image(&config, Some("rust:1.80")), "rust:1.80");
    }

    #[test]
    fn resolve_image_platform_fallback() {
        let config = ProviderConfig::default();
        assert_eq!(resolve_image(&config, None), "platform-claude-runner:latest");
    }

    #[test]
    fn pull_policy_latest_is_always() {
        assert_eq!(image_pull_policy("golang:latest"), "Always");
        assert_eq!(image_pull_policy("golang"), "Always");  // no tag = latest
    }

    #[test]
    fn pull_policy_tagged_is_if_not_present() {
        assert_eq!(image_pull_policy("golang:1.23"), "IfNotPresent");
        assert_eq!(image_pull_policy("image@sha256:abc123"), "IfNotPresent");
    }

    #[test]
    fn setup_container_added_when_commands_present() {
        let config = ProviderConfig {
            setup_commands: Some(vec!["npm install".into(), "npm run build".into()]),
            ..Default::default()
        };
        let params = /* ... test params ... */;
        let containers = build_init_containers(&params);
        assert_eq!(containers.len(), 2);  // git-clone + setup
        assert_eq!(containers[1].name, "setup");
    }

    #[test]
    fn no_setup_container_when_commands_empty() {
        let config = ProviderConfig::default();
        let params = /* ... test params ... */;
        let containers = build_init_containers(&params);
        assert_eq!(containers.len(), 1);  // git-clone only
    }
}
```

---

### Step B5: Thread Project Image Through Service Layer

**Modify: `src/agent/service.rs`** — in `create_session()`:

Current:
```rust
let pod = provider.build_pod(PodBuildParams {
    session: &session,
    config: &config,
    agent_api_token: &identity.api_token,
    platform_api_url: &platform_api_url,
    repo_clone_url: &repo_clone_url,
    namespace: &state.config.pipeline_namespace,
});
```

New:
```rust
// Fetch project's agent_image setting
let project_image = sqlx::query_scalar!(
    "SELECT agent_image FROM projects WHERE id = $1 AND is_active = true",
    project_id,
)
.fetch_optional(&state.pool)
.await?
.flatten();  // Option<Option<String>> → Option<String>

let pod = provider.build_pod(PodBuildParams {
    session: &session,
    config: &config,
    agent_api_token: &identity.api_token,
    platform_api_url: &platform_api_url,
    repo_clone_url: &repo_clone_url,
    namespace: &state.config.pipeline_namespace,
    project_agent_image: project_image.as_deref(),
});
```

---

### Step B6: API Validation

**Modify: `src/api/sessions.rs`** — validate `config.image` and `config.setup_commands`:

```rust
async fn create_session(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(project_id): Path<Uuid>,
    Json(body): Json<CreateSessionRequest>,
) -> Result<Json<SessionResponse>, ApiError> {
    // ... existing validation ...

    // Validate image if provided
    if let Some(ref config) = body.config {
        if let Some(ref image) = config.image {
            validation::check_container_image(image)?;
        }
        if let Some(ref commands) = config.setup_commands {
            validation::check_setup_commands(commands)?;
        }
    }

    // ... rest of handler ...
}
```

**Modify: `src/api/projects.rs`** — allow setting `agent_image` on project update:

```rust
#[derive(Debug, serde::Deserialize)]
pub struct UpdateProjectRequest {
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub visibility: Option<String>,
    pub default_branch: Option<String>,
    pub agent_image: Option<String>,  // NEW
}

async fn update_project(/* ... */) -> Result<Json<ProjectResponse>, ApiError> {
    // ... existing validation ...

    // Validate agent_image if provided
    if let Some(ref image) = body.agent_image {
        validation::check_container_image(image)?;
    }

    // Update query includes agent_image
    sqlx::query!(
        r#"UPDATE projects SET
            display_name = COALESCE($1, display_name),
            description = COALESCE($2, description),
            visibility = COALESCE($3, visibility),
            default_branch = COALESCE($4, default_branch),
            agent_image = COALESCE($5, agent_image),
            updated_at = now()
        WHERE id = $6 AND is_active = true"#,
        body.display_name,
        body.description,
        body.visibility,
        body.default_branch,
        body.agent_image,
        project_id,
    )
    .execute(&state.pool)
    .await?;

    // ... return updated project ...
}
```

**Modify: `src/api/projects.rs`** — include `agent_image` in `ProjectResponse`:

```rust
pub struct ProjectResponse {
    // ... existing fields ...
    pub agent_image: Option<String>,  // NEW
}
```

---

### Step B7: Regenerate SQLx Offline Cache

After the migration:
```bash
just db-migrate
just db-prepare
```

Commit the updated `.sqlx/` directory.

---

## Custom Image Requirements

For a custom image to work as an agent container, it must have:

| Requirement | Why |
|---|---|
| `git` CLI | Init container clones the repo |
| `node` + `npm` | Claude Code CLI and MCP servers are Node.js |
| `claude` CLI (or install via npm) | The entrypoint runs `claude` |
| Non-root user | Pod security policies |
| `/workspace` writable | Code checkout destination |

### Example: Go Agent Image

```dockerfile
FROM golang:1.23

# Add Node.js for Claude Code + MCP servers
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
    && apt-get install -y nodejs git \
    && npm install -g @anthropic-ai/claude-code

# Copy MCP servers (from platform build context)
COPY --from=platform-claude-runner:latest /usr/local/lib/mcp/ /usr/local/lib/mcp/
COPY --from=platform-claude-runner:latest /usr/local/bin/entrypoint.sh /usr/local/bin/

# Create workspace
RUN useradd -m -s /bin/bash agent && mkdir -p /workspace && chown agent:agent /workspace
USER agent
WORKDIR /workspace

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
```

### Documentation Note

The project should document these requirements so users know what's needed in custom images. This can be added to a future docs page or README section.

---

## Files Changed

| File | Action | Description |
|------|--------|-------------|
| `migrations/YYYYMMDDHHMMSS_agent_image_config.up.sql` | **New** | Add `agent_image` column to projects |
| `migrations/YYYYMMDDHHMMSS_agent_image_config.down.sql` | **New** | Drop `agent_image` column |
| `src/validation.rs` | **Modify** | Add `check_container_image()` and `check_setup_commands()` |
| `src/agent/provider.rs` | **Modify** | Add `image` and `setup_commands` to `ProviderConfig` |
| `src/agent/claude_code/pod.rs` | **Modify** | Image resolution, setup container, `PodBuildParams` update |
| `src/agent/service.rs` | **Modify** | Fetch `agent_image` from project, pass to pod builder |
| `src/api/sessions.rs` | **Modify** | Validate `config.image` and `config.setup_commands` |
| `src/api/projects.rs` | **Modify** | Add `agent_image` to update/response |
| `.sqlx/` | **Modify** | Regenerated offline cache |

---

## Verification

### Automated
1. `just db-migrate && just db-prepare` — migration applies cleanly
2. `just ci` — all tests pass, including new validation tests
3. `just lint` — no clippy warnings

### Manual Testing

1. **Project default image**:
   ```bash
   # Set project agent image
   curl -X PATCH /api/projects/{id} \
     -H "Authorization: Bearer $TOKEN" \
     -d '{"agent_image": "golang:1.23"}'

   # Create session (should use project image)
   curl -X POST /api/projects/{id}/sessions \
     -d '{"prompt": "help me write Go code"}'
   # Verify pod spec has image: golang:1.23
   ```

2. **Session override**:
   ```bash
   curl -X POST /api/projects/{id}/sessions \
     -d '{"prompt": "...", "config": {"image": "rust:1.80"}}'
   # Verify pod spec has image: rust:1.80 (overrides project default)
   ```

3. **Setup commands**:
   ```bash
   curl -X POST /api/projects/{id}/sessions \
     -d '{"prompt": "...", "config": {"setup_commands": ["go mod download", "go build ./..."]}}'
   # Verify pod has 3 init containers: git-clone, setup, then main
   ```

4. **Validation rejection**:
   ```bash
   curl -X POST /api/projects/{id}/sessions \
     -d '{"prompt": "...", "config": {"image": "evil;rm -rf /"}}'
   # Expect 400 Bad Request
   ```

---

## Risks and Mitigations

| Risk | Impact | Mitigation |
|------|--------|-----------|
| Custom image missing Node.js | Claude Code and MCP servers fail to start | Document requirements; consider validating at session creation |
| Shell injection via image name | Arbitrary K8s pod spec manipulation | Strict character validation in `check_container_image()` |
| Setup commands fail | Session stuck in init | K8s init container has restart backoff; pod eventually enters Error state; reaper captures logs |
| Large custom images | Slow pod startup | No mitigation needed — user choice; document recommendation to keep images small |
| Image pull errors | Pod stuck in ImagePullBackOff | Agent reaper will eventually clean up; status visible in session API |

---

## Estimated Scope

| Metric | Value |
|--------|-------|
| New migrations | 1 (up + down) |
| New files | 2 (migration pair) |
| Modified files | 6 (Rust) + `.sqlx/` |
| Estimated LOC | ~300 (migrations + Rust changes + tests) |
| New validation functions | 2 |
| New unit tests | ~12 |
