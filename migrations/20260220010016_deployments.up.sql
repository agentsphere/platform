CREATE TABLE deployments (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id      UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    environment     TEXT NOT NULL DEFAULT 'production'
                    CHECK (environment IN ('preview', 'staging', 'production')),
    ops_repo_id     UUID REFERENCES ops_repos(id),
    manifest_path   TEXT,
    image_ref       TEXT NOT NULL,
    values_override JSONB,
    desired_status  TEXT NOT NULL DEFAULT 'active'
                    CHECK (desired_status IN ('active', 'stopped', 'rollback')),
    current_status  TEXT NOT NULL DEFAULT 'pending'
                    CHECK (current_status IN ('pending', 'syncing', 'healthy', 'degraded', 'failed')),
    current_sha     TEXT,
    deployed_by     UUID REFERENCES users(id),
    deployed_at     TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (project_id, environment)
);

CREATE TRIGGER trg_deployments_updated_at
    BEFORE UPDATE ON deployments
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

CREATE INDEX idx_deployments_reconcile ON deployments(desired_status, current_status);

CREATE TABLE deployment_history (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    deployment_id UUID NOT NULL REFERENCES deployments(id) ON DELETE CASCADE,
    image_ref     TEXT NOT NULL,
    ops_repo_sha  TEXT,
    action        TEXT NOT NULL CHECK (action IN ('deploy', 'rollback', 'stop', 'scale')),
    status        TEXT NOT NULL CHECK (status IN ('success', 'failure')),
    deployed_by   UUID REFERENCES users(id),
    message       TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
