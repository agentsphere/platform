-- Platform commands: skill prompt templates (e.g. /dev, /plan, /review).
CREATE TABLE platform_commands (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    -- NULL = global command, set = project-scoped
    project_id UUID REFERENCES projects(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    -- The prompt template (markdown). Supports $ARGUMENTS placeholder.
    prompt_template TEXT NOT NULL,
    -- Whether to keep session alive after execution
    persistent_session BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE(project_id, name)
);

-- PostgreSQL UNIQUE treats NULL as distinct (NULL != NULL), so
-- UNIQUE(project_id, name) allows duplicate global commands.
-- This partial unique index enforces uniqueness for global commands.
CREATE UNIQUE INDEX idx_platform_commands_global_name
    ON platform_commands(name) WHERE project_id IS NULL;
