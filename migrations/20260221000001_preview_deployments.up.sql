CREATE TABLE preview_deployments (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id      UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    branch          TEXT NOT NULL,
    branch_slug     TEXT NOT NULL,
    image_ref       TEXT NOT NULL,
    pipeline_id     UUID REFERENCES pipelines(id),
    desired_status  TEXT NOT NULL DEFAULT 'active'
        CHECK (desired_status IN ('active', 'stopped')),
    current_status  TEXT NOT NULL DEFAULT 'pending'
        CHECK (current_status IN ('pending', 'syncing', 'healthy', 'degraded', 'failed', 'stopped')),
    ttl_hours       INT NOT NULL DEFAULT 24,
    expires_at      TIMESTAMPTZ NOT NULL DEFAULT now() + INTERVAL '24 hours',
    created_by      UUID REFERENCES users(id),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (project_id, branch_slug)
);

CREATE INDEX idx_preview_deployments_status
    ON preview_deployments(current_status)
    WHERE desired_status = 'active';

CREATE INDEX idx_preview_deployments_expires
    ON preview_deployments(expires_at)
    WHERE desired_status = 'active';
