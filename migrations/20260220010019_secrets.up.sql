CREATE TABLE secrets (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id      UUID REFERENCES projects(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    encrypted_value BYTEA NOT NULL,
    scope           TEXT NOT NULL DEFAULT 'pipeline'
                    CHECK (scope IN ('pipeline', 'agent', 'deploy', 'all')),
    version         INTEGER NOT NULL DEFAULT 1,
    created_by      UUID REFERENCES users(id),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (project_id, name)
);

CREATE TRIGGER trg_secrets_updated_at
    BEFORE UPDATE ON secrets
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

CREATE UNIQUE INDEX idx_secrets_global_name ON secrets(name) WHERE project_id IS NULL;
