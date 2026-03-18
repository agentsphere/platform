-- Reverse: drop workspace scope from platform_commands.

ALTER TABLE platform_commands DROP CONSTRAINT IF EXISTS chk_commands_scope;
DROP INDEX IF EXISTS idx_platform_commands_scoped;

-- Delete any workspace-scoped commands before dropping the column.
DELETE FROM platform_commands WHERE workspace_id IS NOT NULL AND project_id IS NULL;

ALTER TABLE platform_commands DROP COLUMN workspace_id;

-- Restore original constraints.
ALTER TABLE platform_commands ADD CONSTRAINT platform_commands_project_id_name_key UNIQUE (project_id, name);
CREATE UNIQUE INDEX idx_platform_commands_global_name
    ON platform_commands(name) WHERE project_id IS NULL;
