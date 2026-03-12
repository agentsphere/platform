-- Plan 46: Tag trigger type + releases

-- Drop the existing inline CHECK constraint on pipelines.trigger
-- PostgreSQL names inline constraints as {table}_{column}_check
ALTER TABLE pipelines DROP CONSTRAINT IF EXISTS pipelines_trigger_check;
ALTER TABLE pipelines ADD CONSTRAINT pipelines_trigger_check
    CHECK (trigger IN ('push', 'api', 'schedule', 'mr', 'tag'));

CREATE TABLE releases (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id    UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    tag_name      TEXT NOT NULL,
    name          TEXT NOT NULL,
    body          TEXT,
    is_draft      BOOLEAN NOT NULL DEFAULT false,
    is_prerelease BOOLEAN NOT NULL DEFAULT false,
    created_by    UUID NOT NULL REFERENCES users(id),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (project_id, tag_name)
);

CREATE TABLE release_assets (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    release_id    UUID NOT NULL REFERENCES releases(id) ON DELETE CASCADE,
    name          TEXT NOT NULL,
    minio_path    TEXT NOT NULL,
    content_type  TEXT,
    size_bytes    BIGINT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
