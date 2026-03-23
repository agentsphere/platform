-- Progressive Delivery: replace deployments/preview_deployments with deploy_targets + releases
-- and add feature flags.

-- Drop old tables (triggers + indexes drop with them)
DROP TABLE IF EXISTS deployment_history CASCADE;
DROP TABLE IF EXISTS deployments CASCADE;
DROP TABLE IF EXISTS preview_deployments CASCADE;

-- Deploy targets: one per (project, environment, branch_slug)
CREATE TABLE deploy_targets (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id      UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    environment     TEXT NOT NULL DEFAULT 'production'
                    CHECK (environment IN ('preview', 'staging', 'production')),
    branch          TEXT,
    branch_slug     TEXT,
    ttl_hours       INT,
    expires_at      TIMESTAMPTZ,
    default_strategy TEXT NOT NULL DEFAULT 'rolling'
                    CHECK (default_strategy IN ('rolling', 'canary', 'ab_test')),
    ops_repo_id     UUID REFERENCES ops_repos(id),
    manifest_path   TEXT,
    is_active       BOOLEAN NOT NULL DEFAULT true,
    created_by      UUID REFERENCES users(id),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE NULLS NOT DISTINCT (project_id, environment, branch_slug)
);

CREATE TRIGGER trg_deploy_targets_updated_at
    BEFORE UPDATE ON deploy_targets
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- Releases: each deployment attempt
CREATE TABLE deploy_releases (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    target_id       UUID NOT NULL REFERENCES deploy_targets(id) ON DELETE CASCADE,
    project_id      UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    image_ref       TEXT NOT NULL,
    commit_sha      TEXT,
    strategy        TEXT NOT NULL DEFAULT 'rolling'
                    CHECK (strategy IN ('rolling', 'canary', 'ab_test')),
    phase           TEXT NOT NULL DEFAULT 'pending'
                    CHECK (phase IN ('pending','progressing','holding','paused','promoting',
                                     'completed','rolling_back','rolled_back','cancelled','failed')),
    traffic_weight  INT NOT NULL DEFAULT 0 CHECK (traffic_weight BETWEEN 0 AND 100),
    health          TEXT NOT NULL DEFAULT 'unknown'
                    CHECK (health IN ('unknown','healthy','degraded','unhealthy')),
    current_step    INT NOT NULL DEFAULT 0,
    rollout_config  JSONB NOT NULL DEFAULT '{}',
    analysis_config JSONB,
    values_override JSONB,
    tracked_resources JSONB NOT NULL DEFAULT '[]',
    deployed_by     UUID REFERENCES users(id),
    pipeline_id     UUID REFERENCES pipelines(id),
    started_at      TIMESTAMPTZ,
    completed_at    TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TRIGGER trg_deploy_releases_updated_at
    BEFORE UPDATE ON deploy_releases
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

CREATE INDEX idx_deploy_releases_reconcile ON deploy_releases(phase)
    WHERE phase IN ('pending','progressing','holding','promoting','rolling_back');

CREATE INDEX idx_deploy_releases_target ON deploy_releases(target_id, created_at DESC);

-- Rollout analyses: per-step metric evaluations
CREATE TABLE rollout_analyses (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    release_id      UUID NOT NULL REFERENCES deploy_releases(id) ON DELETE CASCADE,
    step_index      INT NOT NULL,
    config          JSONB NOT NULL,
    verdict         TEXT NOT NULL DEFAULT 'running'
                    CHECK (verdict IN ('running','pass','fail','inconclusive','cancelled')),
    metric_results  JSONB,
    started_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at    TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Release history: audit trail
CREATE TABLE release_history (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    release_id      UUID NOT NULL REFERENCES deploy_releases(id) ON DELETE CASCADE,
    target_id       UUID NOT NULL REFERENCES deploy_targets(id) ON DELETE CASCADE,
    action          TEXT NOT NULL CHECK (action IN (
        'created','step_advanced','analysis_started','analysis_completed',
        'promoted','paused','resumed','rolled_back','cancelled','failed',
        'health_changed','traffic_shifted')),
    phase           TEXT NOT NULL,
    traffic_weight  INT,
    image_ref       TEXT NOT NULL,
    detail          JSONB,
    actor_id        UUID REFERENCES users(id),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_release_history_release ON release_history(release_id, created_at DESC);

-- Feature flags
CREATE TABLE feature_flags (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id      UUID REFERENCES projects(id) ON DELETE CASCADE,
    key             TEXT NOT NULL,
    flag_type       TEXT NOT NULL DEFAULT 'boolean'
                    CHECK (flag_type IN ('boolean', 'percentage', 'variant', 'json')),
    default_value   JSONB NOT NULL DEFAULT 'false'::jsonb,
    environment     TEXT CHECK (environment IS NULL OR environment IN ('staging','production')),
    enabled         BOOLEAN NOT NULL DEFAULT false,
    description     TEXT,
    created_by      UUID REFERENCES users(id),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (key, project_id, environment)
);

CREATE TRIGGER trg_feature_flags_updated_at
    BEFORE UPDATE ON feature_flags
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

CREATE TABLE feature_flag_rules (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    flag_id         UUID NOT NULL REFERENCES feature_flags(id) ON DELETE CASCADE,
    priority        INT NOT NULL DEFAULT 0,
    rule_type       TEXT NOT NULL CHECK (rule_type IN ('user_id','user_attribute','percentage')),
    attribute_name  TEXT,
    attribute_values TEXT[] NOT NULL DEFAULT '{}',
    percentage      INT CHECK (percentage IS NULL OR percentage BETWEEN 0 AND 100),
    serve_value     JSONB NOT NULL,
    enabled         BOOLEAN NOT NULL DEFAULT true,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE feature_flag_overrides (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    flag_id         UUID NOT NULL REFERENCES feature_flags(id) ON DELETE CASCADE,
    user_id         UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    serve_value     JSONB NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (flag_id, user_id)
);

CREATE TABLE feature_flag_history (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    flag_id         UUID NOT NULL REFERENCES feature_flags(id) ON DELETE CASCADE,
    action          TEXT NOT NULL CHECK (action IN ('created','updated','toggled','deleted',
                                                    'rule_added','rule_updated','rule_deleted',
                                                    'override_set','override_deleted')),
    actor_id        UUID REFERENCES users(id),
    previous_value  JSONB,
    new_value       JSONB,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_feature_flag_history_flag ON feature_flag_history(flag_id, created_at DESC);

-- Seed flag:manage permission
INSERT INTO permissions (id, name, resource, action, description)
VALUES (gen_random_uuid(), 'flag:manage', 'flag', 'manage', 'Manage feature flags')
ON CONFLICT (name) DO NOTHING;
