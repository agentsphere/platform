CREATE TABLE issues (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id  UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    number      INTEGER NOT NULL,
    author_id   UUID NOT NULL REFERENCES users(id),
    title       TEXT NOT NULL,
    body        TEXT,
    status      TEXT NOT NULL DEFAULT 'open'
                CHECK (status IN ('open', 'closed')),
    labels      TEXT[] NOT NULL DEFAULT '{}',
    assignee_id UUID REFERENCES users(id),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (project_id, number)
);

CREATE TRIGGER trg_issues_updated_at
    BEFORE UPDATE ON issues
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();
