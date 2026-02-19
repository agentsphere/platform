CREATE TABLE projects (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_id          UUID NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    name              TEXT NOT NULL,
    display_name      TEXT,
    description       TEXT,
    visibility        TEXT NOT NULL DEFAULT 'private'
                      CHECK (visibility IN ('private', 'internal', 'public')),
    default_branch    TEXT NOT NULL DEFAULT 'main',
    repo_path         TEXT,
    is_active         BOOLEAN NOT NULL DEFAULT true,
    next_issue_number INTEGER NOT NULL DEFAULT 0,
    next_mr_number    INTEGER NOT NULL DEFAULT 0,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (owner_id, name)
);

CREATE TRIGGER trg_projects_updated_at
    BEFORE UPDATE ON projects
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();
