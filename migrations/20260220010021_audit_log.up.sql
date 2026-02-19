CREATE TABLE audit_log (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    actor_id    UUID NOT NULL,
    actor_name  TEXT NOT NULL,
    action      TEXT NOT NULL,
    resource    TEXT NOT NULL,
    resource_id UUID,
    project_id  UUID REFERENCES projects(id) ON DELETE SET NULL,
    detail      JSONB,
    ip_addr     INET,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_audit_actor ON audit_log(actor_id, created_at DESC);
CREATE INDEX idx_audit_resource ON audit_log(resource, resource_id, created_at DESC);
