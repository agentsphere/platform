# mgr/ Go Prototype — Implementation Reference

Extracted from the `mgr/` Go codebase (~2,600 LOC) before removal. These are the concrete implementation details worth preserving for porting to Rust in `src/agent/`. The architectural design is already captured in `07-agent-orchestration.md` and `unified-platform.md`.

---

## AgentProvider Interface

The Go interface that maps to the Rust `AgentProvider` trait:

```go
type AgentProvider interface {
    Info() Info
    CreateSession(ctx context.Context, cfg SessionConfig) (*SessionHandle, error)
    StopSession(ctx context.Context, handle *SessionHandle) error
    SendMessage(ctx context.Context, handle *SessionHandle, msg string) error
    StreamOutput(ctx context.Context, handle *SessionHandle) (io.ReadCloser, error)
    GetStatus(ctx context.Context, handle *SessionHandle) (Status, error)
}

type SessionConfig struct {
    SessionID  uuid.UUID
    AppName    string
    RepoClone  string          // authenticated clone URL
    Branch     string          // e.g. "agent/{session_id[:8]}"
    Prompt     string
    Provider   json.RawMessage // provider-specific config
}

type SessionHandle struct {
    SessionID uuid.UUID
    PodName   string
}

type Status string // "pending", "running", "completed", "failed", "stopped"
```

Provider capabilities metadata:

```go
type Info struct {
    Name         string       // "claude-code"
    DisplayName  string       // "Claude Code"
    Description  string
    Capabilities []Capability // create-app, edit-code, interactive
    ConfigSchema json.RawMessage
}
```

---

## Claude Code Pod Spec

Concrete K8s pod structure for agent sessions. This is the most critical reference.

```
Namespace:       agent-mgr  (→ will change to "platform" or configurable)
Pod name:        agent-{session_id[:8]}
Restart policy:  Never
Service account: agent-runner

Labels:
  app:        agent-session
  session-id: {session_id}

Init container (git-clone):
  Image:   alpine/git:latest
  Command: sh -c "set -eu; git clone <repo_url> /workspace; cd /workspace;
           git checkout -b <branch>; git config user.name 'agent-mgr-bot';
           git config user.email 'bot@asp.now'"
  Volume:  workspace → /workspace
  Resources: 50m/64Mi requests, 200m/128Mi limits

Main container (claude):
  Image:      docker/Dockerfile.claude-runner (see below)
  Args:       --output-format stream-json --permission-mode auto-accept-only
              [optional: --model <model>, --max-turns <n>]
  Stdin:      true (for follow-up messages via SPDY attach)
  TTY:        false
  WorkingDir: /workspace
  Env:
    ANTHROPIC_API_KEY: from K8s Secret "agent-mgr-secrets" key "anthropic-api-key"
    SESSION_ID:        {session_id}
  Volume:  workspace → /workspace
  Resources: 200m/256Mi requests, 500m/512Mi limits

Volume:
  workspace: EmptyDir, 1Gi size limit
```

Provider-specific config for Claude Code:

```go
type Config struct {
    Model    string `json:"model,omitempty"`    // override Claude model
    MaxTurns int    `json:"max_turns,omitempty"` // limit agentic turns
}
```

---

## K8s SPDY Attach for Sending Messages

The trickiest pattern — attaching to a running pod's stdin via SPDY:

```go
func (p *Provider) SendMessage(ctx context.Context, handle *SessionHandle, msg string) error {
    req := p.client.CoreV1().RESTClient().Post().
        Resource("pods").
        Name(handle.PodName).
        Namespace(Namespace).
        SubResource("attach").
        VersionedParams(&corev1.PodAttachOptions{
            Container: "claude",
            Stdin:     true,
            Stdout:    false,
            Stderr:    false,
        }, scheme.ParameterCodec)

    exec, err := remotecommand.NewSPDYExecutor(p.restConfig, "POST", req.URL())
    if err != nil {
        return fmt.Errorf("create attach executor: %w", err)
    }

    return exec.StreamWithContext(ctx, remotecommand.StreamOptions{
        Stdin: bytes.NewReader([]byte(msg + "\n")),
    })
}
```

Rust equivalent uses `kube::api::AttachParams`:
```rust
let attached = pods.attach("agent-xxx", &AttachParams {
    container: Some("claude".into()),
    stdin: true,
    stdout: false,
    stderr: false,
    ..Default::default()
}).await?;
attached.stdin().unwrap().write_all(format!("{msg}\n").as_bytes()).await?;
```

---

## Stream Output (Pod Logs)

Follow pod logs for real-time streaming to WebSocket clients:

```go
func (p *Provider) StreamOutput(ctx context.Context, handle *SessionHandle) (io.ReadCloser, error) {
    req := p.client.CoreV1().Pods(Namespace).GetLogs(handle.PodName, &corev1.PodLogOptions{
        Container: "claude",
        Follow:    true,
    })
    return req.Stream(ctx)
}
```

---

## Progress Milestone Tracker

Keyword-based progress detection from Claude Code's stream-json output:

```
Milestones (in order):
  0: "Setting up project"   — triggered by: tool "write"/"create" usage
  1: "Creating data models"  — triggered by: model, schema, database, migration, struct, type
  2: "Building UI components" — triggered by: component, page, route, template, html, tsx, jsx, vue
  3: "Adding styling and polish" — triggered by: css, style, tailwind, theme, color, font, layout
  4: "Final checks"          — triggered by: test, readme, done, complete, finish, final
```

Logic: each line is lowercased and checked against keyword sets. When a milestone's keywords match, all prior milestones are marked "done" and the matching one becomes "active". Milestones only advance forward, never backward.

---

## Agent Runner Docker Image

See `docker/Dockerfile.claude-runner` and `docker/entrypoint.sh` (preserved at platform root).

Key details:
- Base: `node:22-slim` (Claude Code CLI is an npm package)
- Installs: git, ca-certificates, `@anthropic-ai/claude-code`
- Runs as non-root `agent` user
- Entrypoint: runs `claude --output-format stream-json "$@"`, then git add/commit/push on exit
- Branch naming: `agent/{SESSION_ID}`

---

## Pod Status Mapping

```
K8s PodPhase → Session Status:
  Pending   → pending
  Running   → running
  Succeeded → completed
  Failed    → failed
  (deleted) → stopped (set by service before deleting pod)
```

---

## Session Lifecycle (Service Layer)

```
CreateSession:
  1. Parse provider config (model, max_turns)
  2. Build pod spec via provider
  3. Create pod in K8s
  4. Return SessionHandle {session_id, pod_name}

StopSession:
  1. Delete pod from K8s (by pod_name in namespace)

SendMessage:
  1. SPDY attach to pod stdin (container: "claude")
  2. Write message + newline

StreamOutput:
  1. Follow pod logs (container: "claude")
  2. Return io.ReadCloser for line-by-line reading
```

Note: The Go prototype had no auth, no RBAC, no agent identity. The Rust version adds ephemeral agent identities with delegated permissions (see `07-agent-orchestration.md`).

---

## Original DB Schema (minimal)

For reference — the Go prototype used a much simpler schema than the unified platform:

```sql
-- apps: simple project concept (no users, no RBAC)
CREATE TABLE apps (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name        TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    status      TEXT NOT NULL DEFAULT 'draft',
    repo_owner  TEXT NOT NULL DEFAULT '',
    repo_name   TEXT NOT NULL DEFAULT '',
    preview_url TEXT NOT NULL DEFAULT '',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- sessions: agent session tied to an app
CREATE TABLE sessions (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    app_id     UUID NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    provider   TEXT NOT NULL DEFAULT 'claude-code',
    status     TEXT NOT NULL DEFAULT 'pending',
    prompt     TEXT NOT NULL DEFAULT '',
    branch     TEXT NOT NULL DEFAULT '',
    pod_name   TEXT NOT NULL DEFAULT '',
    config     JSONB NOT NULL DEFAULT '{}',
    cost_json  JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

This is superseded by `agent_sessions` + `agent_messages` in the unified schema (`unified-platform.md`).

---

## Valkey Event Bus Pattern

The Go prototype set up Valkey pub/sub for session lifecycle events (though not fully wired):

```
Channel: "agent-mgr:events"
Event envelope: { "type": "<event_type>", "payload": <json> }

Event types:
  session.started   — when pod is created
  session.completed — when pod succeeds
  session.failed    — when pod fails
  session.stopped   — when manually stopped
  app.status        — when app status changes
```

The Rust platform will use Valkey pub/sub more broadly (RBAC cache invalidation, live UI updates).
