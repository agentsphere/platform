CREATE TABLE merge_requests (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id    UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    number        INTEGER NOT NULL,
    author_id     UUID NOT NULL REFERENCES users(id),
    source_branch TEXT NOT NULL,
    target_branch TEXT NOT NULL,
    title         TEXT NOT NULL,
    body          TEXT,
    status        TEXT NOT NULL DEFAULT 'open'
                  CHECK (status IN ('open', 'merged', 'closed')),
    merged_by     UUID REFERENCES users(id),
    merged_at     TIMESTAMPTZ,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (project_id, number)
);

CREATE TRIGGER trg_merge_requests_updated_at
    BEFORE UPDATE ON merge_requests
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

CREATE TABLE mr_reviews (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id  UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    mr_id       UUID NOT NULL REFERENCES merge_requests(id) ON DELETE CASCADE,
    reviewer_id UUID NOT NULL REFERENCES users(id),
    verdict     TEXT NOT NULL CHECK (verdict IN ('approve', 'request_changes', 'comment')),
    body        TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
