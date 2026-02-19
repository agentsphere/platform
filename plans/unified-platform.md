# Unified AI-First Platform: Replacing the Service Zoo

## Context

The current setup stitches together 8+ off-the-shelf tools (Gitea, Woodpecker, Authelia, OpenObserve, Maddy, OpenBao, OTel Collector, MinIO) with ~30 manual integration steps, hardcoded credentials, and 17 documented gotchas. Each tool has its own data model, auth system, UI, and API — creating a fragmented experience held together with YAML glue.

The vision: **one custom Rust binary** that replaces Gitea + Woodpecker + Authelia + OpenObserve + Maddy + OpenBao with a unified platform. Keep Postgres, Valkey, MinIO, Traefik as infrastructure. Primary users are AI agents (Claude Code); humans are auditors/monitors.

---

## What Gets Replaced vs Kept

| Replaced (custom-built) | Kept (infrastructure) |
|---|---|
| Gitea → lightweight git server + project mgmt | PostgreSQL (CNPG) |
| Woodpecker → build engine (pipelines only) | Valkey (cache/pubsub) |
| Authelia → built-in auth (tokens + sessions + RBAC) | MinIO (object storage) |
| OpenObserve → custom OTEL viewer/query engine | Traefik (ingress/TLS) |
| Maddy → notification service (not full mail server) | OTel Collector (keep as DaemonSet) |
| OpenBao → secrets in Postgres (encrypted at rest) | |

---

## Why Rust

| Factor | Rust | Go |
|--------|------|-----|
| **OTEL ingest** | `arrow-rs` + `parquet` = zero-copy columnar writes. No GC pauses during high-throughput ingest. | Fight GC under load. Manual buffer management. |
| **Compile-time SQL** | `sqlx` checks every query against the live schema at compile time. 20+ tables, RBAC joins — bugs caught before running. | Runtime SQL errors only. |
| **Type system** | RBAC permission resolution, delegation chains, status state machines — enforced at compile time. `enum` for all status fields. | Runtime checks, hope tests cover edge cases. |
| **Memory footprint** | Predictable, no GC. More headroom for agent pods on a single VM. 5-15MB binary. | 20-50MB binary, GC spikes. |
| **Error handling** | `Result<T, E>` propagation across 11 modules. `thiserror` for domain errors. | `if err != nil` x 10,000. |
| **K8s client** | `kube-rs` — mature, async, typed. Pod exec/attach works (spike first). | `client-go` — canonical, but Go-only advantage. |
| **CI build time** | ~3-5min clean (GitHub Enterprise runners — not a constraint). | ~15s clean. |
| **Existing code** | Rewrite `mgr/` (~2,600 LOC Go). ~2-3 week cost, but would rewrite most of it anyway during unification. | Direct reuse. |

**Decision**: Rust. This is a stateful platform with correctness requirements (RBAC, observability ingest, deployer reconciliation), not a CRUD API. The type system and zero-cost abstractions pay for themselves.

**Risk mitigation**: Spike kube-rs pod exec/attach for interactive agent sessions (1-2 days) before committing to the full build.

---

## Crate Stack

| Concern | Crate | Notes |
|---------|-------|-------|
| HTTP + WebSocket | `axum` | Tower-based, first-class WebSocket |
| Middleware | `tower` | Auth, RBAC, logging, rate limiting as composable layers |
| Postgres | `sqlx` | Compile-time checked queries, migrations, connection pool |
| K8s client | `kube` + `k8s-openapi` | Pod lifecycle, exec/attach, watch |
| OTLP protobuf | `prost` | Decode OTLP proto payloads |
| Parquet writes | `arrow` + `parquet` | Columnar storage for OTEL cold tier |
| MinIO / S3 | `aws-sdk-s3` or `opendal` | Object storage for artifacts, logs, LFS |
| Valkey / Redis | `fred` or `redis` | Cache, pub/sub, metric buffer |
| Password hashing | `argon2` | argon2id |
| Encryption | `aes-gcm` | Secret storage encryption |
| Async runtime | `tokio` | Multi-threaded runtime |
| Serialization | `serde` + `serde_json` | JSON API, JSONB columns |
| Tracing | `tracing` + `tracing-subscriber` | Structured logging, span context |
| SMTP | `lettre` | Email notifications |
| Git operations | `std::process::Command` | Shell out to `git` CLI (same as Go plan) |
| Templating | `minijinja` or `tera` | Manifest templating for deployer |
| Embedded UI | `rust-embed` or `include_dir` | Serve Preact SPA from binary |
| CLI / config | `clap` + `config` | CLI args + env/file config |

---

## Unified Data Model

The core insight: all these tools operate on the same concepts (users, projects, events, credentials) but each maintains its own isolated data store. A unified schema in Postgres eliminates 90% of the integration glue.

### Core Tables

```sql
-- ── Utility ─────────────────────────────────

-- Auto-update updated_at on any row modification
CREATE FUNCTION set_updated_at() RETURNS trigger AS $$
BEGIN NEW.updated_at = now(); RETURN NEW; END;
$$ LANGUAGE plpgsql;

-- ── Identity & RBAC ──────────────────────────

CREATE TABLE users (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name        TEXT NOT NULL UNIQUE,          -- login handle
    display_name TEXT,
    email       TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,               -- argon2id
    is_active   BOOLEAN NOT NULL DEFAULT true,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TRIGGER trg_users_updated_at BEFORE UPDATE ON users FOR EACH ROW EXECUTE FUNCTION set_updated_at();

CREATE TABLE roles (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name        TEXT NOT NULL UNIQUE,          -- 'admin', 'developer', 'ops', 'viewer', 'agent'
    description TEXT,
    is_system   BOOLEAN NOT NULL DEFAULT false, -- system roles can't be deleted
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE permissions (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name        TEXT NOT NULL UNIQUE,          -- 'project:read', 'project:write', 'agent:run', 'deploy:promote', 'observe:read', 'admin:users'
    resource    TEXT NOT NULL,                 -- 'project', 'agent', 'deploy', 'observe', 'admin', 'secret'
    action      TEXT NOT NULL,                 -- 'read', 'write', 'run', 'promote', 'delete', 'delegate'
    description TEXT
);

CREATE TABLE role_permissions (
    role_id       UUID NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
    permission_id UUID NOT NULL REFERENCES permissions(id) ON DELETE CASCADE,
    PRIMARY KEY (role_id, permission_id)
);

-- user ↔ role binding (global or project-scoped)
CREATE TABLE user_roles (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role_id     UUID NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
    project_id  UUID REFERENCES projects(id) ON DELETE CASCADE, -- NULL = global
    granted_by  UUID REFERENCES users(id),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (user_id, role_id, project_id)
);
CREATE INDEX idx_user_roles_user ON user_roles(user_id);

-- delegation: a user/agent grants a subset of their own permissions to another user/agent
-- e.g. user delegates 'ops:observe' + 'deploy:promote' to an agent for a project
CREATE TABLE delegations (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    delegator_id  UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,  -- who grants
    delegate_id   UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,  -- who receives (can be agent user)
    permission_id UUID NOT NULL REFERENCES permissions(id) ON DELETE CASCADE,
    project_id    UUID REFERENCES projects(id) ON DELETE CASCADE,        -- NULL = global scope
    expires_at    TIMESTAMPTZ,                                           -- optional TTL
    reason        TEXT,                                                  -- "monitoring shift 2025-02-19"
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at    TIMESTAMPTZ,
    UNIQUE (delegator_id, delegate_id, permission_id, project_id)
);
CREATE INDEX idx_delegations_delegate ON delegations(delegate_id);
-- constraint: delegator must themselves hold the permission they're delegating.
-- enforced in application logic (not DB constraint) because it depends on role resolution.

CREATE TABLE auth_sessions (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash  TEXT NOT NULL UNIQUE,          -- sha256 of session token
    ip_addr     INET,
    user_agent  TEXT,
    expires_at  TIMESTAMPTZ NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_auth_sessions_user ON auth_sessions(user_id);

CREATE TABLE api_tokens (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    token_hash  TEXT NOT NULL UNIQUE,          -- sha256 of token
    scopes      TEXT[] NOT NULL DEFAULT '{}',  -- e.g. {'project:read', 'agent:run'}
    project_id  UUID REFERENCES projects(id) ON DELETE CASCADE, -- NULL = all projects
    last_used_at TIMESTAMPTZ,
    expires_at  TIMESTAMPTZ,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_api_tokens_user ON api_tokens(user_id);

-- ── Projects ──────────────────────────────

CREATE TABLE projects (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_id        UUID NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    name            TEXT NOT NULL,                  -- slug: my-todo-app
    display_name    TEXT,
    description     TEXT,
    visibility      TEXT NOT NULL DEFAULT 'private' CHECK (visibility IN ('private','internal','public')),
    default_branch  TEXT NOT NULL DEFAULT 'main',
    repo_path       TEXT,                           -- path to bare repo on disk
    is_active       BOOLEAN NOT NULL DEFAULT true,
    next_issue_number   INTEGER NOT NULL DEFAULT 0,  -- atomic counter for issue/MR numbers
    next_mr_number      INTEGER NOT NULL DEFAULT 0,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (owner_id, name)
);
CREATE TRIGGER trg_projects_updated_at BEFORE UPDATE ON projects FOR EACH ROW EXECUTE FUNCTION set_updated_at();

CREATE TABLE issues (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id  UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    number      INTEGER NOT NULL,                   -- project-scoped auto-increment
    author_id   UUID NOT NULL REFERENCES users(id),
    title       TEXT NOT NULL,
    body        TEXT,
    status      TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open','closed')),
    labels      TEXT[] NOT NULL DEFAULT '{}',
    assignee_id UUID REFERENCES users(id),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (project_id, number)
);
CREATE TRIGGER trg_issues_updated_at BEFORE UPDATE ON issues FOR EACH ROW EXECUTE FUNCTION set_updated_at();

CREATE TABLE comments (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id  UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    issue_id    UUID REFERENCES issues(id) ON DELETE CASCADE,
    mr_id       UUID REFERENCES merge_requests(id) ON DELETE CASCADE,
    author_id   UUID NOT NULL REFERENCES users(id),
    body        TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (issue_id IS NOT NULL OR mr_id IS NOT NULL)
);
CREATE TRIGGER trg_comments_updated_at BEFORE UPDATE ON comments FOR EACH ROW EXECUTE FUNCTION set_updated_at();

CREATE TABLE webhooks (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id  UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    url         TEXT NOT NULL,
    events      TEXT[] NOT NULL DEFAULT '{}',       -- {'push','mr','issue','build','deploy'}
    secret      TEXT,                               -- HMAC secret for payload verification
    active      BOOLEAN NOT NULL DEFAULT true,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ── Git ───────────────────────────────────
--    bare repos on disk, metadata in Postgres

CREATE TABLE merge_requests (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id      UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    number          INTEGER NOT NULL,
    author_id       UUID NOT NULL REFERENCES users(id),
    source_branch   TEXT NOT NULL,
    target_branch   TEXT NOT NULL,
    title           TEXT NOT NULL,
    body            TEXT,
    status          TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open','merged','closed')),
    merged_by       UUID REFERENCES users(id),
    merged_at       TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (project_id, number)
);
CREATE TRIGGER trg_merge_requests_updated_at BEFORE UPDATE ON merge_requests FOR EACH ROW EXECUTE FUNCTION set_updated_at();

CREATE TABLE mr_reviews (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id  UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    mr_id       UUID NOT NULL REFERENCES merge_requests(id) ON DELETE CASCADE,
    reviewer_id UUID NOT NULL REFERENCES users(id),
    verdict     TEXT NOT NULL CHECK (verdict IN ('approve','request_changes','comment')),
    body        TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ── Agent Sessions ────────────────────────

CREATE TABLE agent_sessions (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id      UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    user_id         UUID NOT NULL REFERENCES users(id),          -- who started it
    agent_user_id   UUID REFERENCES users(id),                   -- agent's identity (for RBAC)
    prompt          TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'pending'
                    CHECK (status IN ('pending','running','completed','failed','stopped')),
    branch          TEXT,
    pod_name        TEXT,
    provider        TEXT NOT NULL DEFAULT 'claude-code',
    provider_config JSONB,
    cost_tokens     BIGINT,
    cost_usd        NUMERIC(10,4),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at     TIMESTAMPTZ
);
CREATE INDEX idx_agent_sessions_project ON agent_sessions(project_id);
CREATE INDEX idx_agent_sessions_user ON agent_sessions(user_id);
CREATE INDEX idx_agent_sessions_status ON agent_sessions(status);

CREATE TABLE agent_messages (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id  UUID NOT NULL REFERENCES agent_sessions(id) ON DELETE CASCADE,
    role        TEXT NOT NULL CHECK (role IN ('user','assistant','system','tool')),
    content     TEXT NOT NULL,
    metadata    JSONB,                          -- tool calls, token counts, etc.
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ── Build & Deploy ────────────────────────
--
-- Build: pipelines run in the platform (build images, run tests, produce artifacts).
-- Deploy: the platform writes desired state to Postgres. A lightweight continuous
-- deployer (running as a controller) reads desired state and applies manifests
-- from ops repos to the cluster. This separates "what to deploy" from "how to deploy".

CREATE TABLE pipelines (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id  UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    trigger     TEXT NOT NULL CHECK (trigger IN ('push','api','schedule','mr')),
    git_ref     TEXT NOT NULL,                      -- branch or tag (renamed from 'ref' to avoid SQL reserved word)
    commit_sha  TEXT,
    status      TEXT NOT NULL DEFAULT 'pending'
                CHECK (status IN ('pending','running','success','failure','cancelled')),
    triggered_by UUID REFERENCES users(id),
    started_at  TIMESTAMPTZ,
    finished_at TIMESTAMPTZ,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_pipelines_project ON pipelines(project_id, created_at DESC);
CREATE INDEX idx_pipelines_status ON pipelines(status);

CREATE TABLE pipeline_steps (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    pipeline_id UUID NOT NULL REFERENCES pipelines(id) ON DELETE CASCADE,
    project_id  UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    step_order  INTEGER NOT NULL,                    -- execution order (0-based)
    name        TEXT NOT NULL,
    image       TEXT NOT NULL,
    commands    TEXT[] NOT NULL DEFAULT '{}',
    status      TEXT NOT NULL DEFAULT 'pending'
                CHECK (status IN ('pending','running','success','failure','skipped')),
    log_ref     TEXT,                               -- MinIO path to step logs
    exit_code   INTEGER,
    duration_ms INTEGER,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE artifacts (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    pipeline_id UUID NOT NULL REFERENCES pipelines(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    minio_path  TEXT NOT NULL,
    content_type TEXT,
    size_bytes  BIGINT,
    expires_at  TIMESTAMPTZ,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ops_repos: git repos containing K8s manifests / Helm charts that the deployer syncs
CREATE TABLE ops_repos (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name        TEXT NOT NULL UNIQUE,                -- e.g. 'platform-ops', 'app-ops'
    repo_url    TEXT NOT NULL,                        -- git clone URL (can be local platform repo)
    branch      TEXT NOT NULL DEFAULT 'main',
    path        TEXT NOT NULL DEFAULT '/',            -- subdirectory within repo
    sync_interval_s INTEGER NOT NULL DEFAULT 60,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- deployments: desired state declarations — the deployer reconciles these
CREATE TABLE deployments (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id      UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    environment     TEXT NOT NULL DEFAULT 'production' CHECK (environment IN ('preview','staging','production')),
    ops_repo_id     UUID REFERENCES ops_repos(id),       -- which ops repo has the manifests
    manifest_path   TEXT,                                 -- path within ops repo to the manifest template
    image_ref       TEXT NOT NULL,                        -- container image to deploy
    values_override JSONB,                                -- Helm values / template vars
    desired_status  TEXT NOT NULL DEFAULT 'active'
                    CHECK (desired_status IN ('active','stopped','rollback')),
    current_status  TEXT NOT NULL DEFAULT 'pending'
                    CHECK (current_status IN ('pending','syncing','healthy','degraded','failed')),
    current_sha     TEXT,                                 -- last applied commit SHA from ops repo
    deployed_by     UUID REFERENCES users(id),
    deployed_at     TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (project_id, environment)
);
CREATE TRIGGER trg_deployments_updated_at BEFORE UPDATE ON deployments FOR EACH ROW EXECUTE FUNCTION set_updated_at();
-- Index for the deployer's main reconciliation query
CREATE INDEX idx_deployments_reconcile ON deployments(desired_status, current_status);

-- deployment_history: audit trail of every deploy action
CREATE TABLE deployment_history (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    deployment_id   UUID NOT NULL REFERENCES deployments(id) ON DELETE CASCADE,
    image_ref       TEXT NOT NULL,
    ops_repo_sha    TEXT,
    action          TEXT NOT NULL CHECK (action IN ('deploy','rollback','stop','scale')),
    status          TEXT NOT NULL CHECK (status IN ('success','failure')),
    deployed_by     UUID REFERENCES users(id),
    message         TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ── Observability ─────────────────────────
--
-- Design: agents and platform services emit structured telemetry using a
-- unified schema. All logs carry a correlation envelope (trace_id, span_id,
-- session_id, project_id, user_id) so any event can be traced back to the
-- agent session, user, and project that caused it.
--
-- Storage strategy:
--   Postgres: structured indexes + recent hot data (last 24-48h logs, active traces)
--   MinIO: cold storage for raw payloads (Parquet-partitioned by day)

CREATE TABLE traces (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    trace_id    TEXT NOT NULL UNIQUE,            -- W3C trace ID (32-char hex)
    project_id  UUID REFERENCES projects(id),
    session_id  UUID REFERENCES agent_sessions(id),
    user_id     UUID REFERENCES users(id),
    root_span   TEXT NOT NULL,                   -- name of root span
    service     TEXT NOT NULL,                   -- originating service/agent
    status      TEXT NOT NULL DEFAULT 'ok' CHECK (status IN ('ok','error','unset')),
    duration_ms INTEGER,
    started_at  TIMESTAMPTZ NOT NULL,
    finished_at TIMESTAMPTZ,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE spans (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    trace_id    TEXT NOT NULL,                   -- references traces.trace_id (no FK — spans may arrive before their trace)
    span_id     TEXT NOT NULL UNIQUE,            -- 16-char hex
    parent_span_id TEXT,                         -- NULL for root span
    name        TEXT NOT NULL,                   -- e.g. 'tool_call:bash', 'http:POST /api/apps'
    service     TEXT NOT NULL,
    kind        TEXT NOT NULL DEFAULT 'internal'
                CHECK (kind IN ('internal','server','client','producer','consumer')),
    status      TEXT NOT NULL DEFAULT 'ok' CHECK (status IN ('ok','error','unset')),
    attributes  JSONB,                           -- arbitrary key-value pairs
    events      JSONB,                           -- span events (exceptions, annotations)
    duration_ms INTEGER,
    started_at  TIMESTAMPTZ NOT NULL,
    finished_at TIMESTAMPTZ
);
CREATE INDEX idx_spans_trace ON spans(trace_id);

-- structured log entries (hot storage — last 48h, older rotated to MinIO)
CREATE TABLE log_entries (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    timestamp   TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- correlation envelope: every log can be traced to its origin
    trace_id    TEXT,                             -- links to traces
    span_id     TEXT,                             -- links to spans
    project_id  UUID REFERENCES projects(id),
    session_id  UUID REFERENCES agent_sessions(id),
    user_id     UUID REFERENCES users(id),
    -- log content
    service     TEXT NOT NULL,                   -- e.g. 'platform', 'agent-session-xyz', 'pipeline-abc'
    level       TEXT NOT NULL DEFAULT 'info'
                CHECK (level IN ('trace','debug','info','warn','error','fatal')),
    message     TEXT NOT NULL,
    attributes  JSONB,                           -- structured metadata (tool_name, file_path, exit_code, etc.)
    -- source
    namespace   TEXT,
    pod         TEXT,
    container   TEXT
);
CREATE INDEX idx_log_ts ON log_entries(timestamp DESC);
CREATE INDEX idx_log_project ON log_entries(project_id, timestamp DESC);
CREATE INDEX idx_log_session ON log_entries(session_id, timestamp DESC);
CREATE INDEX idx_log_trace ON log_entries(trace_id);
CREATE INDEX idx_log_level ON log_entries(level, timestamp DESC);

-- metric series catalog (metadata only — actual samples in MinIO or Valkey)
CREATE TABLE metric_series (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name        TEXT NOT NULL,                   -- e.g. 'http_requests_total', 'agent_session_duration_seconds'
    labels      JSONB NOT NULL DEFAULT '{}',
    metric_type TEXT NOT NULL DEFAULT 'gauge'     -- renamed from 'type' to avoid SQL reserved word
                CHECK (metric_type IN ('gauge','counter','histogram','summary')),
    unit        TEXT,                            -- 'bytes', 'seconds', 'requests', etc.
    project_id  UUID REFERENCES projects(id),    -- NULL = platform-level metric
    last_value  DOUBLE PRECISION,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (name, labels)
);
CREATE TRIGGER trg_metric_series_updated_at BEFORE UPDATE ON metric_series FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- metric samples (hot buffer — last 1h, flushed to MinIO periodically)
CREATE TABLE metric_samples (
    series_id   UUID NOT NULL REFERENCES metric_series(id) ON DELETE CASCADE,
    timestamp   TIMESTAMPTZ NOT NULL,
    value       DOUBLE PRECISION NOT NULL,
    PRIMARY KEY (series_id, timestamp)
);

-- alerts: threshold-based rules evaluated against metrics/logs
CREATE TABLE alert_rules (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name        TEXT NOT NULL,
    description TEXT,
    query       TEXT NOT NULL,                   -- metric query expression or log filter
    condition   TEXT NOT NULL,                   -- 'gt', 'lt', 'eq', 'absent'
    threshold   DOUBLE PRECISION,
    for_seconds INTEGER NOT NULL DEFAULT 60,     -- how long condition must hold
    severity    TEXT NOT NULL DEFAULT 'warning'
                CHECK (severity IN ('info','warning','critical')),
    notify_channels TEXT[] NOT NULL DEFAULT '{}', -- {'email','webhook','in_app'}
    project_id  UUID REFERENCES projects(id),    -- NULL = platform-wide
    enabled     BOOLEAN NOT NULL DEFAULT true,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE alert_events (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    rule_id     UUID NOT NULL REFERENCES alert_rules(id) ON DELETE CASCADE,
    status      TEXT NOT NULL CHECK (status IN ('firing','resolved')),
    value       DOUBLE PRECISION,
    message     TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    resolved_at TIMESTAMPTZ
);
CREATE INDEX idx_alert_events_status ON alert_events(status, created_at DESC);

-- ── Secrets ───────────────────────────────

CREATE TABLE secrets (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id      UUID REFERENCES projects(id) ON DELETE CASCADE,  -- NULL = global secret
    name            TEXT NOT NULL,
    encrypted_value BYTEA NOT NULL,              -- AES-256-GCM with platform master key
    scope           TEXT NOT NULL DEFAULT 'pipeline'
                    CHECK (scope IN ('pipeline','agent','deploy','all')),
    version         INTEGER NOT NULL DEFAULT 1,
    created_by      UUID REFERENCES users(id),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (project_id, name)
);
CREATE TRIGGER trg_secrets_updated_at BEFORE UPDATE ON secrets FOR EACH ROW EXECUTE FUNCTION set_updated_at();
-- Separate unique index for global secrets (project_id IS NULL) since NULL != NULL in UNIQUE constraints
CREATE UNIQUE INDEX idx_secrets_global_name ON secrets(name) WHERE project_id IS NULL;

-- ── Notifications ─────────────────────────

CREATE TABLE notifications (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    notification_type TEXT NOT NULL,              -- renamed from 'type'; e.g. 'build_failed', 'mr_created', 'agent_completed', 'alert_firing'
    subject     TEXT NOT NULL,
    body        TEXT,
    channel     TEXT NOT NULL DEFAULT 'in_app'
                CHECK (channel IN ('in_app','email','webhook')),
    status      TEXT NOT NULL DEFAULT 'pending'
                CHECK (status IN ('pending','sent','read','failed')),
    ref_type    TEXT,                             -- 'pipeline', 'mr', 'session', 'alert'
    ref_id      UUID,                             -- FK to the relevant entity
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_notifications_user_status ON notifications(user_id, status);

-- ── Audit Log ─────────────────────────────

CREATE TABLE audit_log (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    actor_id    UUID NOT NULL,                   -- user or agent who performed action (no FK — audit logs are immutable)
    actor_name  TEXT NOT NULL,                   -- denormalized for when user is deleted
    action      TEXT NOT NULL,                   -- 'user.create', 'project.delete', 'secret.read', 'role.delegate'
    resource    TEXT NOT NULL,                   -- 'user', 'project', 'secret', 'delegation'
    resource_id UUID,                            -- ID of affected resource
    project_id  UUID REFERENCES projects(id) ON DELETE SET NULL,
    detail      JSONB,                           -- extra context (old/new values, etc.)
    ip_addr     INET,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_audit_actor ON audit_log(actor_id, created_at DESC);
CREATE INDEX idx_audit_resource ON audit_log(resource, resource_id, created_at DESC);
```

### Rust Type Safety for Status Fields

The SQL `CHECK` constraints above map directly to Rust enums with `sqlx::Type` derive. Illegal state transitions become compile-time errors:

```rust
#[derive(sqlx::Type, Serialize, Deserialize)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum PipelineStatus {
    Pending,
    Running,
    Success,
    Failure,
    Cancelled,
}

#[derive(sqlx::Type, Serialize, Deserialize)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum DeployDesiredStatus {
    Active,
    Stopped,
    Rollback,
}

// State machine: only valid transitions compile
impl PipelineStatus {
    pub fn can_transition_to(&self, next: &PipelineStatus) -> bool {
        matches!(
            (self, next),
            (Self::Pending, Self::Running)
                | (Self::Running, Self::Success | Self::Failure | Self::Cancelled)
        )
    }
}
```

### What's NOT in the database

- **Wiki/docs** — docs live in the project's git repo as `docs/*.md`. They're already versioned, reviewable via MRs, and accessible to agents. No separate wiki table needed — the file browser API serves them directly from the repo.
- **Raw OTEL payloads** — log bodies, trace spans, metric samples at full resolution go to MinIO as time-partitioned Parquet. Postgres holds indexes and hot-window data only.
- **Git objects** — bare repos live on disk (PV). Postgres holds metadata (projects, MRs, branches) not git objects.

### Why This Simplifies Everything

- **Auth + RBAC**: One `users` table, roles with permissions, project-scoped bindings. Agents are users with the `agent` role. Delegation lets a human say "agent X can do ops monitoring on project Y" — no external IAM.
- **Git + CI + Deploy**: A push triggers a pipeline. Build artifacts go to MinIO. The deployer reads desired state from `deployments` and syncs manifests from ops repos. Clean separation: platform decides *what* to deploy, ops repos decide *how*.
- **Observability**: Structured correlation envelope (trace_id → span → log) means every agent action, pipeline step, and deploy event can be traced end-to-end. Agents follow guidelines to emit structured logs with metadata.
- **Secrets**: Encrypted column in Postgres replaces OpenBao. Platform master key from K8s Secret. Agents and pipelines access secrets via the same API, scoped by RBAC.
- **Notifications**: `lettre` SMTP client with a relay replaces Maddy entirely. Platform decides when to notify based on events + alert rules.

---

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                      Traefik (ingress + TLS)                 │
└──────────┬─────────────────────────────────┬─────────────────┘
           │                                 │
     HTTPS (humans)                   HTTPS (agents)
           │                                 │
┌──────────▼─────────────────────────────────▼─────────────────┐
│                                                               │
│                   Platform Binary (Rust)                      │
│                                                               │
│  ┌─────────┐ ┌──────────┐ ┌──────────┐ ┌──────────────────┐ │
│  │ HTTP API│ │ Git Smart │ │ OTEL     │ │ WebSocket        │ │
│  │ (axum)  │ │ HTTP      │ │ Ingest   │ │ (live logs/chat) │ │
│  └────┬────┘ └────┬─────┘ └────┬─────┘ └───────┬──────────┘ │
│       │           │            │                │            │
│  ┌────▼───────────▼────────────▼────────────────▼──────────┐ │
│  │            Service Layer (tower middleware)               │ │
│  │  auth · rbac · projects · git · pipelines · deploy ·    │ │
│  │  agents · observe · secrets · notify                    │ │
│  └────┬────────────┬───────────────────────┬───────────────┘ │
│       │            │                       │                 │
│  ┌────▼────┐  ┌────▼─────┐  ┌──────────────▼──────────────┐ │
│  │Postgres │  │ Valkey   │  │ MinIO                        │ │
│  │(sqlx,   │  │(fred,    │  │(opendal/s3,                  │ │
│  │ compile │  │ cache,   │  │ artifacts, logs, LFS, OTEL)  │ │
│  │ checked)│  │ pubsub)  │  │                              │ │
│  └─────────┘  └──────────┘  └──────────────────────────────┘ │
│                                                               │
│  ┌────────────────────────────────────────────────────────┐  │
│  │  K8s Client (kube-rs)                                  │  │
│  │  → spawn agent pods, pipeline pods                     │  │
│  └────────────────────────────────────────────────────────┘  │
│                                                               │
│  ┌──────────────────┐                                        │
│  │ Embedded UI      │  (Preact, served via rust-embed)       │
│  │ dashboard, logs, │                                        │
│  │ project mgmt,    │                                        │
│  │ RBAC admin       │                                        │
│  └──────────────────┘                                        │
└──────────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────────┐
│  Continuous Deployer (tokio task in same binary)             │
│                                                               │
│  Watches: deployments table (desired state)                  │
│  Reads:   ops repos (git clone/pull on interval)             │
│  Applies: kubectl apply / kube-rs dynamic client             │
│  Reports: updates current_status + deployment_history        │
│                                                               │
│  Runs as: tokio::spawn background task. Extract to separate  │
│           binary later if needed.                            │
└──────────────────────────────────────────────────────────────┘
```

**Single binary.** One Deployment in K8s. One IngressRoute. The entire platform is one process talking to Postgres, Valkey, MinIO, and the K8s API.

---

## Project Structure

```
platform/
├── Cargo.toml
├── Cargo.lock
├── migrations/                    # sqlx migrations
│   ├── 001_identity.sql
│   ├── 002_rbac.sql
│   ├── 003_projects.sql
│   ├── 004_git.sql
│   ├── 005_agents.sql
│   ├── 006_pipelines.sql
│   ├── 007_deploy.sql
│   ├── 008_observability.sql
│   ├── 009_secrets.sql
│   ├── 010_notifications.sql
│   └── 011_audit.sql
├── src/
│   ├── main.rs                    # entry point, signal handling, tokio runtime
│   ├── config.rs                  # unified config (env + file)
│   ├── error.rs                   # thiserror domain errors
│   ├── auth/
│   │   ├── mod.rs
│   │   ├── password.rs            # argon2id hashing
│   │   ├── token.rs               # API token generation, session tokens
│   │   └── middleware.rs          # axum extractor: extract user from cookie/bearer
│   ├── rbac/
│   │   ├── mod.rs
│   │   ├── types.rs               # Permission, Role enums
│   │   ├── resolver.rs            # effective_permissions(user, project), Valkey-cached
│   │   ├── delegation.rs          # create/revoke delegations, validation
│   │   └── middleware.rs          # tower layer: require_permission("project:write")
│   ├── api/
│   │   ├── mod.rs                 # axum Router, merged routes
│   │   ├── users.rs               # user CRUD
│   │   ├── projects.rs            # project CRUD
│   │   ├── issues.rs              # issues + comments
│   │   ├── merge_requests.rs      # MR CRUD, review, merge
│   │   ├── sessions.rs            # agent session handlers
│   │   ├── pipelines.rs           # pipeline triggers, status, logs
│   │   ├── deployments.rs         # deployment CRUD, promote, rollback
│   │   ├── observe.rs             # log/trace/metric query API
│   │   ├── secrets.rs             # secret CRUD
│   │   ├── admin.rs               # roles, permissions, delegations, users
│   │   └── health.rs              # health check
│   ├── git/
│   │   ├── mod.rs
│   │   ├── smart_http.rs          # git-upload-pack / git-receive-pack handlers
│   │   ├── hooks.rs               # pre-receive: auth check, trigger pipeline
│   │   ├── browser.rs             # list branches, browse tree, read files
│   │   └── lfs.rs                 # LFS batch API → MinIO redirect
│   ├── pipeline/
│   │   ├── mod.rs
│   │   ├── executor.rs            # create K8s pods per step, stream logs
│   │   ├── definition.rs          # parse .platform.yaml
│   │   └── trigger.rs             # push/api/schedule triggers
│   ├── deployer/
│   │   ├── mod.rs
│   │   ├── reconciler.rs          # main loop: desired vs current state
│   │   ├── ops_repo.rs            # git clone/pull ops repos
│   │   ├── renderer.rs            # template manifests with image_ref + values
│   │   └── applier.rs             # kube-rs dynamic apply + health check
│   ├── agent/
│   │   ├── mod.rs
│   │   ├── service.rs             # session lifecycle (create, stream, stop)
│   │   ├── identity.rs            # ephemeral agent user + delegation
│   │   ├── provider.rs            # AgentProvider trait
│   │   └── claude_code/
│   │       ├── mod.rs
│   │       ├── adapter.rs         # Claude Code implementation of AgentProvider
│   │       ├── pod.rs             # K8s pod spec builder
│   │       └── progress.rs        # stream-json → milestone parser
│   ├── observe/
│   │   ├── mod.rs
│   │   ├── ingest.rs              # OTLP HTTP receiver (prost protobuf)
│   │   ├── store.rs               # write to Postgres (hot) + MinIO (cold)
│   │   ├── parquet.rs             # arrow-rs batch → parquet file → MinIO
│   │   ├── query.rs               # log/trace/metric query engine
│   │   ├── alert.rs               # periodic rule evaluation, notification dispatch
│   │   └── correlation.rs         # inject/resolve trace_id, session_id, project_id
│   ├── secrets/
│   │   ├── mod.rs
│   │   └── engine.rs              # AES-256-GCM encrypt/decrypt, CRUD
│   ├── notify/
│   │   ├── mod.rs
│   │   ├── dispatch.rs            # multi-channel dispatch
│   │   ├── email.rs               # lettre SMTP client
│   │   └── webhook.rs             # HTTP POST delivery
│   └── store/
│       ├── mod.rs
│       ├── pool.rs                # sqlx PgPool setup
│       └── valkey.rs              # fred client, pub/sub helpers
├── ui/                            # Preact SPA (same as before)
│   ├── src/
│   │   ├── index.tsx
│   │   ├── pages/
│   │   └── components/
│   ├── index.html
│   └── package.json
├── docker/
│   ├── Dockerfile                 # multi-stage: rust build → scratch/distroless
│   └── Dockerfile.claude-runner   # agent runtime image
└── Justfile                       # task runner (see rust-dev-process.md)
```

### Key Rust Patterns

**Axum extractors for auth + RBAC**:
```rust
// Handler that requires project:write permission
async fn create_issue(
    State(ctx): State<AppState>,
    AuthUser(user): AuthUser,                        // extracts from cookie/bearer
    RequirePermission(Perm::ProjectWrite): RequirePermission, // tower layer checks RBAC
    Path(project_id): Path<Uuid>,
    Json(body): Json<CreateIssueRequest>,
) -> Result<Json<Issue>, ApiError> {
    // if we get here, user is authenticated and has project:write on this project
    ctx.issues.create(project_id, user.id, body).await
}
```

**sqlx compile-time checked queries**:
```rust
let perms = sqlx::query_as!(
    PermissionRow,
    r#"
    SELECT p.name, p.resource, p.action
    FROM permissions p
    JOIN role_permissions rp ON rp.permission_id = p.id
    JOIN user_roles ur ON ur.role_id = rp.role_id
    WHERE ur.user_id = $1
      AND (ur.project_id IS NULL OR ur.project_id = $2)
    UNION
    SELECT p.name, p.resource, p.action
    FROM permissions p
    JOIN delegations d ON d.permission_id = p.id
    WHERE d.delegate_id = $1
      AND (d.project_id IS NULL OR d.project_id = $2)
      AND d.revoked_at IS NULL
      AND (d.expires_at IS NULL OR d.expires_at > now())
    "#,
    user_id,
    project_id
)
.fetch_all(&ctx.pool)
.await?;
```

---

## RBAC Design

### Principles

1. **Users and agents are the same entity** — agents get a `users` row with a machine identity. This means RBAC rules apply uniformly.
2. **Roles are named permission bundles** — a role groups permissions. Users/agents get roles, either globally or per-project.
3. **Delegation is explicit** — a user can delegate specific permissions they hold to another user/agent. Delegation can be time-bounded.
4. **Least privilege** — agents start with minimal permissions. Humans explicitly grant what's needed.

### System Roles (bootstrapped on first run)

| Role | Permissions | Intended for |
|------|------------|-------------|
| `admin` | `*:*` (all) | Platform administrators |
| `developer` | `project:read`, `project:write`, `agent:run`, `deploy:read`, `observe:read`, `secret:read` | Human developers |
| `ops` | `deploy:read`, `deploy:promote`, `observe:read`, `observe:write`, `alert:manage`, `secret:read` | Operations staff |
| `agent` | (none by default — granted via delegation) | AI agents — get permissions per-session from delegating user |
| `viewer` | `project:read`, `observe:read`, `deploy:read` | Read-only access |

### Delegation Flow

```
User (role: ops) delegates to Agent:
  permissions: ['observe:read', 'deploy:promote', 'alert:manage']
  project: my-app
  expires: 2025-03-01
  reason: "monitoring shift"

→ Agent can now:
  - Read logs/metrics for my-app
  - Promote deployments for my-app
  - Manage alerts for my-app
  - Nothing else

→ Delegation recorded in audit_log
→ Agent's effective permissions = union of (own roles ∩ project scope) + delegations
```

### Permission Resolution

```
effective_permissions(user, project) =
  global_role_permissions(user)
  ∪ project_role_permissions(user, project)
  ∪ active_delegations(user, project)
```

Cached in Valkey per `(user_id, project_id)` with TTL. Invalidated on role/delegation change via pub/sub.

---

## Build & Deploy — Ops Repos Pattern

### Why separate build from deploy?

The current plan tangles pipeline logic (build images, run tests) with deployment logic (apply manifests, update k8s). This creates problems:
- Deploy manifests are buried in pipeline config, not version-controlled independently
- Rolling back means re-running a pipeline, not just pointing at a previous known-good state
- Hard to have different deploy strategies per environment
- Deploy state lives only in the pipeline's execution — lose the pipeline logs, lose deploy history

### Architecture

```
Build side (platform pipelines):
  git push → pipeline triggers → build image → push to registry → store artifact
  → write to deployments table: {project: X, image: registry/app:sha-abc, environment: production}

Deploy side (continuous deployer):
  loop:
    read deployments where desired_status != current_status
    for each:
      git pull ops_repo (contains manifests/helm charts)
      template manifest with image_ref + values_override
      kubectl apply -f rendered_manifest
      update current_status + deployment_history
```

### Ops repos structure

```
platform-ops/              ← ops repo tracked in ops_repos table
├── apps/
│   ├── my-todo-app/
│   │   ├── deployment.yaml    ← K8s Deployment template ({{ .ImageRef }})
│   │   ├── service.yaml
│   │   ├── ingress.yaml
│   │   └── values.yaml        ← defaults, overridden by deployments.values_override
│   └── dashboard/
│       └── ...
├── platform/
│   ├── postgres/
│   ├── valkey/
│   └── traefik/
└── README.md
```

### Benefits

- **Manifests are version-controlled** in their own repo — auditable, reviewable
- **Rollback** = update `deployments.image_ref` to previous value, deployer applies it
- **Deploy != build** — you can re-deploy without rebuilding
- **Multiple environments** — same manifest template, different values per environment
- **Agent-friendly** — an agent with `deploy:promote` permission can update the `deployments` row; the deployer handles the rest. Agent never needs direct kubectl access.

---

## Observability — Structured Agent Telemetry

### Design Goals

Agents built on this platform follow strong guidelines for structured telemetry. Every log message, metric, and trace carries a correlation envelope that connects it to its origin.

### Correlation Envelope

Every telemetry signal carries these attributes (injected automatically by the platform):

| Field | Source | Purpose |
|-------|--------|---------|
| `trace_id` | W3C trace context | Links request chain |
| `span_id` | Current span | Links to specific operation |
| `session_id` | Agent session | Which agent session produced this |
| `project_id` | Context | Which project this relates to |
| `user_id` | Auth context | Who initiated the action |
| `service` | Config | Which service/agent emitted it |

### Agent Logging Guidelines

Agents emit structured JSON logs. The platform uses the `tracing` crate with correlation fields injected via span context:

```rust
// correlation injected automatically from the current span
tracing::info!(
    image = %image_ref,
    environment = "production",
    replicas = 3,
    "deploying app"
);
// → {"level":"info","msg":"deploying app","image":"...","environment":"production",
//    "replicas":3,"trace_id":"abc","session_id":"xyz","project_id":"123",...}
```

### Storage Tiers

| Tier | Store | Retention | What |
|------|-------|-----------|------|
| Hot | Postgres `log_entries` | 48h | Full log entries, queryable by any field |
| Hot | Postgres `metric_samples` | 1h | Recent metric values for live dashboards |
| Hot | Valkey | 5min | Real-time metric buffer, live tail subscriptions |
| Cold | MinIO (Parquet via `arrow-rs`) | 90d+ | All raw OTEL payloads, partitioned by day |

### Query Capabilities

- **Log search**: filter by project, session, level, time range, full-text on message, JSON path on attributes
- **Trace view**: waterfall of spans for a trace, linked to the agent session that produced it
- **Metric charts**: time-series graphs for any metric, with label filtering and aggregation
- **Session replay**: reconstruct what an agent did by querying all logs/spans for a session_id
- **Alert evaluation**: periodic queries against metrics/logs, fire notifications when thresholds breach

### What the OTel Collector does vs the platform

| Concern | OTel Collector (DaemonSet) | Platform (ingest endpoint) |
|---------|---------------------------|---------------------------|
| Scrape node/kubelet metrics | Yes | No |
| Collect pod stdout/stderr | Yes | No |
| Receive OTLP from apps | Forward to platform | Ingest, index, store |
| Enrich with k8s metadata | Yes (k8sattributes) | No |
| Store data | No | Yes (Postgres + MinIO) |
| Query API | No | Yes |
| Alerting | No | Yes |

---

## Module Breakdown & Effort

### 1. Core Framework (~1,800 LOC Rust, ~1-1.5 weeks)

- `src/main.rs` — tokio runtime, signal handling, graceful shutdown
- `src/config.rs` — unified config (env vars + optional TOML file)
- `src/error.rs` — `thiserror` domain error types, axum `IntoResponse` impl
- `src/auth/` — argon2id password hashing, token generation, axum extractor middleware
- `src/store/` — sqlx PgPool setup, migration runner, Valkey client

Includes: Cargo.toml with all dependencies, multi-stage Dockerfile, CI pipeline.

### 2. Identity, Auth & RBAC (~1,500 LOC Rust, ~1-1.5 weeks)

- User CRUD, login/logout, session management
- API token create/revoke/list with scoping (project-level, global)
- `AuthUser` axum extractor: from session cookie or `Authorization: Bearer` header
- `src/rbac/` — Permission/Role enums, `effective_permissions()` resolver, Valkey-cached
- `RequirePermission` tower layer: declarative permission checks on routes
- Delegation: create/revoke, validate delegator holds permission, TTL expiry
- Agent identity: register agent users, delegate permissions per session
- Admin bootstrap: create initial admin user + system roles on first run
- Audit logging for all auth/RBAC mutations

Replaces: Authelia entirely.

### 3. Git Server (~1,400 LOC Rust, ~1-1.5 weeks)

- `src/git/smart_http.rs` — `git-upload-pack` and `git-receive-pack` via `tokio::process::Command`
- Store bare repos at `/data/repos/{owner}/{name}.git` (persistent volume)
- `src/git/hooks.rs` — pre-receive: validate auth + RBAC (`project:write`), trigger pipeline
- `src/git/browser.rs` — list branches, browse tree, read file contents (for UI and agents)
- `src/git/lfs.rs` — LFS batch API → MinIO redirect

Replaces: Gitea's core git hosting.

### 4. Project Management (~1,000 LOC Rust, ~4-5 days)

- Projects: create, list, settings, visibility
- Issues: CRUD, status transitions, labels, comments
- Merge Requests: create from branch, review, merge (git merge via CLI)
- Docs: served directly from `docs/` in the project's git repo (read via file browser API)
- Webhooks: fire on events (push, MR, issue, build)

### 5. Build Engine (~1,400 LOC Rust, ~1-1.5 weeks)

- `src/pipeline/definition.rs` — parse `.platform.yaml` pipeline config
- `src/pipeline/executor.rs` — create K8s pods per step via kube-rs, stream logs
- Artifact upload: step outputs → MinIO via opendal/s3
- Container image building: Kaniko step
- Triggers: git push (pre-receive hook), API call, schedule
- **Output**: pipeline writes `deployments` row with new image_ref — does NOT apply manifests

Replaces: Woodpecker CI's build capabilities.

### 6. Continuous Deployer (~800 LOC Rust, ~3-4 days)

- `src/deployer/reconciler.rs` — tokio background task, poll loop
- Watch `deployments` table for desired_status != current_status
- `src/deployer/ops_repo.rs` — clone/pull ops repos on sync interval
- `src/deployer/renderer.rs` — template manifests with minijinja (image_ref + values)
- `src/deployer/applier.rs` — kube-rs dynamic client: apply + rollout health check
- Update current_status, write deployment_history

### 7. Agent Orchestration (~800 LOC Rust, ~3-4 days)

Port existing `mgr/` Go logic to Rust:

- Agent session lifecycle (create, stream, message, stop)
- `src/agent/identity.rs` — ephemeral agent user, delegate permissions from requesting user
- `AgentProvider` trait + Claude Code implementation
- kube-rs pod lifecycle: create, attach stdin/stdout, stream logs
- WebSocket streaming for live agent output (axum WebSocket)
- stream-json → milestone progress parser

### 8. Observability Ingest & Query (~2,200 LOC Rust, ~1.5-2 weeks)

- `src/observe/ingest.rs` — OTLP HTTP receiver (prost protobuf decoding)
- `src/observe/store.rs` — write to Postgres (hot tier) + batch to MinIO (cold)
- `src/observe/parquet.rs` — arrow-rs RecordBatch → parquet file → MinIO (zero-copy columnar writes)
- Trace/span storage in Postgres with correlation indexing
- Log storage: hot tier in Postgres (48h), background task rotates to MinIO Parquet
- `src/observe/query.rs` — time range, filter by project/session/trace, full-text search, JSON path
- `src/observe/alert.rs` — periodic rule evaluation (tokio interval), notification dispatch
- Dashboard: pre-built views for cluster health, pipeline history, agent activity, trace waterfall

Replaces: OpenObserve.

### 9. Secrets Engine (~500 LOC Rust, ~2 days)

- `src/secrets/engine.rs` — AES-256-GCM encrypt/decrypt via `aes-gcm` crate
- Platform master key from env var or K8s Secret
- Project-scoped and global secrets, RBAC-gated (`secret:read` permission)
- Secret CRUD API (write-only: can create/delete but not read plaintext via API)
- Pipeline/agent integration: resolve `${{ secrets.NAME }}` in configs

Replaces: OpenBao.

### 10. Notifications (~500 LOC Rust, ~2 days)

- `src/notify/dispatch.rs` — multi-channel notification dispatch
- `src/notify/email.rs` — `lettre` SMTP client to external relay
- `src/notify/webhook.rs` — reqwest HTTP POST delivery with HMAC signing
- In-app: store in `notifications` table, serve via API
- Event triggers: build failed, MR created, agent completed, deployment status, alert firing

Replaces: Maddy.

### 11. Web UI (~2,500 LOC TypeScript, ~1.5-2 weeks)

Extend the existing Preact UI:

- **Dashboard**: cluster health, recent activity, active agents
- **Projects**: list, detail, files browser, issues, MRs, docs (from repo)
- **Builds**: pipeline list, step logs, artifacts
- **Deploy**: deployment status, history, rollback, ops repo config
- **Agents**: session list, live streaming, chat
- **Observe**: log viewer, metric charts, trace waterfall, alerts
- **Admin**: users, roles, permissions, delegations, tokens, platform config

Served from binary via `rust-embed`. Reuse existing Preact setup, esbuild config, components.

---

## Total Effort Estimate

| Module | New Rust LOC | New TS LOC | Days |
|--------|-------------|-----------|------|
| Core framework | 1,800 | — | 7-10 |
| Identity, auth & RBAC | 1,500 | 300 | 7-10 |
| Git server | 1,400 | 300 | 7-10 |
| Project management | 1,000 | 300 | 4-5 |
| Build engine | 1,400 | 200 | 7-10 |
| Continuous deployer | 800 | 100 | 3-4 |
| Agent orchestration | 800 | 200 | 3-4 |
| Observability ingest + query | 2,200 | 500 | 10-14 |
| Secrets engine | 500 | 100 | 2 |
| Notifications | 500 | 100 | 2 |
| Web UI (remaining pages) | — | 600 | 4-6 |
| Integration testing + hardening | 1,200 | — | 4-6 |
| **Total** | **~13,100** | **~2,700** | **~60-85 days** |

**~15,800 LOC total. 3-4 months for one developer. Faster with Claude Code assisting.**

Rust LOC is ~20-25% higher than Go equivalent due to type definitions, trait impls, and explicit error handling. But the code that compiles is more likely to be correct.

The `mgr/` Go codebase (~2,600 LOC) provides design patterns and logic to port. Not direct reuse, but the architecture transfers 1:1.

---

## Phased Delivery Plan

### Phase 0 — Spike (3-5 days)
kube-rs pod exec/attach for interactive agent sessions. Validate that WebSocket bridging works. If it doesn't, evaluate workarounds (kubectl exec shelling, API proxy). **Gate**: must pass before committing to Rust.

### Phase 1 — Foundation (week 1-3)
Core framework + auth + RBAC + git server. At the end: can create users, assign roles, push/clone repos, authenticate with tokens, delegate permissions. CI pipeline building + pushing container image.

### Phase 2 — Agent Loop (week 4-5)
Port agent orchestration from `mgr/`. Agent identity + delegation. At the end: agents can create projects, get workspaces, build code, push results — with RBAC-scoped permissions.

### Phase 3 — Build Engine + Deployer (week 6-8)
Pipeline execution, artifact storage, continuous deployer, ops repos. At the end: push triggers build → artifact to MinIO → desired state in DB → deployer applies manifests from ops repo.

### Phase 4 — Observability (week 8-11)
OTEL ingest with prost, arrow-rs Parquet writes, structured log/trace/metric storage, correlation queries, alerts. At the end: full visibility into what agents and pipelines are doing, traceable end-to-end by session.

### Phase 5 — Project Management + Polish (week 11-14)
Issues, MRs, notifications, secrets, UI polish. At the end: complete platform.

### Phase 6 — Migration (week 14-16)
Migrate existing data from Gitea/Woodpecker, set up ops repos with current manifests, switch over, remove old deployments.

---

## Key Design Decisions

1. **Single binary** — one Rust binary with embedded UI. One K8s Deployment, one IngressRoute. Eliminates all cross-service integration.

2. **Rust, not Go** — RBAC delegation chains, OTEL ingest with Parquet writes, deployer reconciliation — this is a stateful platform with correctness requirements. Compile-time SQL checks (sqlx), enum state machines, and zero-copy OTEL ingest justify the language. GitHub Enterprise CI removes the build-time concern.

3. **Postgres as the brain** — unified schema eliminates 6 separate databases. One database, one migration path, one backup. sqlx compile-time validation across 20+ tables.

4. **MinIO for blobs** — logs, artifacts, LFS objects, OTEL Parquet payloads. Keeps Postgres lean. Already deployed and working. arrow-rs writes Parquet natively.

5. **OTel Collector stays** — scrapes node/kubelet metrics, collects pod logs, enriches with k8s metadata. Points at the platform's OTLP ingest endpoint.

6. **No OIDC complexity** — built-in auth with API tokens and sessions. External OIDC can be added later as optional login method.

7. **Git via smart HTTP + Command** — `git-upload-pack`/`git-receive-pack` via `tokio::process::Command`. Same exec approach as Go, just async.

8. **Docs in repo, not wiki tables** — `docs/*.md` in the project repo. Already versioned, reviewable, agent-accessible. No separate wiki system.

9. **Ops repos for deploy** — manifests live in version-controlled ops repos. Platform declares desired state, deployer reconciles. Clean separation of build vs deploy.

10. **RBAC with delegation** — users and agents share the same identity model. Delegation lets humans grant scoped, time-bounded permissions to agents. Every action is audited.

11. **Structured telemetry with correlation** — every log/span/metric carries trace_id, session_id, project_id. `tracing` crate provides span context propagation natively. Enables end-to-end traceability.

12. **Type-safe state machines** — pipeline status, deployment status, agent session lifecycle — all modeled as Rust enums with `sqlx::Type`. Invalid state transitions don't compile.

---

## Verification Plan

1. **Auth + RBAC**: create user → assign role → get API token → verify permission enforcement → delegate to agent → verify scoped access
2. **Git**: `git clone`, `git push`, browse files via API, verify RBAC on push
3. **Agent loop**: prompt → agent session with delegated permissions → code committed to branch → verify agent can only access granted resources
4. **Pipeline**: push triggers build → logs stream → artifact stored
5. **Deploy**: pipeline writes desired state → deployer picks up → applies from ops repo → health check passes → history recorded
6. **Observability**: OTel Collector data appears in platform → query logs by session_id → view trace waterfall → alert fires on threshold → Parquet cold storage verified
7. **E2E**: human types idea in UI → agent builds it → pipeline deploys it → observable in dashboard → traceable end-to-end
