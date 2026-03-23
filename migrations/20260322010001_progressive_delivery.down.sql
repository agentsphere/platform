-- Reverse progressive delivery migration

DROP TABLE IF EXISTS feature_flag_history CASCADE;
DROP TABLE IF EXISTS feature_flag_overrides CASCADE;
DROP TABLE IF EXISTS feature_flag_rules CASCADE;
DROP TABLE IF EXISTS feature_flags CASCADE;
DROP TABLE IF EXISTS release_history CASCADE;
DROP TABLE IF EXISTS rollout_analyses CASCADE;
DROP TABLE IF EXISTS deploy_releases CASCADE;
DROP TABLE IF EXISTS deploy_targets CASCADE;

-- Remove flag:manage permission
DELETE FROM permissions WHERE name = 'flag:manage';

-- Restore original tables
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
