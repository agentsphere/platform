CREATE TABLE pipelines (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id   UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    trigger      TEXT NOT NULL CHECK (trigger IN ('push', 'api', 'schedule', 'mr')),
    git_ref      TEXT NOT NULL,
    commit_sha   TEXT,
    status       TEXT NOT NULL DEFAULT 'pending'
                 CHECK (status IN ('pending', 'running', 'success', 'failure', 'cancelled')),
    triggered_by UUID REFERENCES users(id),
    started_at   TIMESTAMPTZ,
    finished_at  TIMESTAMPTZ,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_pipelines_project ON pipelines(project_id, created_at DESC);
CREATE INDEX idx_pipelines_status ON pipelines(status);

CREATE TABLE pipeline_steps (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    pipeline_id UUID NOT NULL REFERENCES pipelines(id) ON DELETE CASCADE,
    project_id  UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    step_order  INTEGER NOT NULL,
    name        TEXT NOT NULL,
    image       TEXT NOT NULL,
    commands    TEXT[] NOT NULL DEFAULT '{}',
    status      TEXT NOT NULL DEFAULT 'pending'
                CHECK (status IN ('pending', 'running', 'success', 'failure', 'skipped')),
    log_ref     TEXT,
    exit_code   INTEGER,
    duration_ms INTEGER,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE artifacts (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    pipeline_id  UUID NOT NULL REFERENCES pipelines(id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    minio_path   TEXT NOT NULL,
    content_type TEXT,
    size_bytes   BIGINT,
    expires_at   TIMESTAMPTZ,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
