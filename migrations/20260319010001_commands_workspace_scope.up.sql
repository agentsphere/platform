-- Add workspace tier to platform_commands for hierarchical skill inheritance.
-- Hierarchy: global → workspace → project → repo (.claude/commands/*.md)

ALTER TABLE platform_commands
    ADD COLUMN workspace_id UUID REFERENCES workspaces(id) ON DELETE CASCADE;

-- Drop old constraints that only supported global + project scoping.
ALTER TABLE platform_commands DROP CONSTRAINT IF EXISTS platform_commands_project_id_name_key;
DROP INDEX IF EXISTS idx_platform_commands_global_name;

-- Scoped uniqueness using COALESCE for NULL handling (same pattern as secrets).
-- Ensures (name) is unique within each scope tier.
CREATE UNIQUE INDEX idx_platform_commands_scoped ON platform_commands (
    COALESCE(workspace_id, '00000000-0000-0000-0000-000000000000'::uuid),
    COALESCE(project_id,   '00000000-0000-0000-0000-000000000000'::uuid),
    name
);

-- Exactly one scope tier: global, workspace, or project.
-- project-scoped commands may optionally also have workspace_id set (denormalized),
-- but the constraint ensures at least one valid tier.
ALTER TABLE platform_commands ADD CONSTRAINT chk_commands_scope CHECK (
    (workspace_id IS NULL AND project_id IS NULL)           -- global
    OR (workspace_id IS NOT NULL AND project_id IS NULL)    -- workspace
    OR (project_id IS NOT NULL)                             -- project
);
