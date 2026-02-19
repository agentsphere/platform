CREATE TABLE delegations (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    delegator_id  UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    delegate_id   UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    permission_id UUID NOT NULL REFERENCES permissions(id) ON DELETE CASCADE,
    project_id    UUID REFERENCES projects(id) ON DELETE CASCADE,
    expires_at    TIMESTAMPTZ,
    reason        TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at    TIMESTAMPTZ,
    UNIQUE (delegator_id, delegate_id, permission_id, project_id)
);

CREATE INDEX idx_delegations_delegate ON delegations(delegate_id);
