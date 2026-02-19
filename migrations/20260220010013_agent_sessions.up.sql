CREATE TABLE agent_sessions (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id      UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    user_id         UUID NOT NULL REFERENCES users(id),
    agent_user_id   UUID REFERENCES users(id),
    prompt          TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'pending'
                    CHECK (status IN ('pending', 'running', 'completed', 'failed', 'stopped')),
    branch          TEXT,
    pod_name        TEXT,
    provider        TEXT NOT NULL DEFAULT 'claude-code',
    provider_config JSONB,
    cost_tokens     BIGINT,
    cost_usd        NUMERIC(10, 4),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at     TIMESTAMPTZ
);

CREATE INDEX idx_agent_sessions_project ON agent_sessions(project_id);
CREATE INDEX idx_agent_sessions_user ON agent_sessions(user_id);
CREATE INDEX idx_agent_sessions_status ON agent_sessions(status);

CREATE TABLE agent_messages (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id  UUID NOT NULL REFERENCES agent_sessions(id) ON DELETE CASCADE,
    role        TEXT NOT NULL CHECK (role IN ('user', 'assistant', 'system', 'tool')),
    content     TEXT NOT NULL,
    metadata    JSONB,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
