-- Revert secret scopes (drop constraint first to allow update)
ALTER TABLE secrets DROP CONSTRAINT IF EXISTS secrets_scope_check;
UPDATE secrets SET scope = 'deploy' WHERE scope IN ('staging', 'prod', 'test');
ALTER TABLE secrets ADD CONSTRAINT secrets_scope_check
    CHECK (scope IN ('pipeline', 'agent', 'deploy', 'all'));

-- Remove include_staging from projects
ALTER TABLE projects DROP COLUMN IF EXISTS include_staging;

-- Remove columns from pipeline_steps
ALTER TABLE pipeline_steps DROP COLUMN IF EXISTS started_at;
ALTER TABLE pipeline_steps DROP COLUMN IF EXISTS step_config;
ALTER TABLE pipeline_steps DROP COLUMN IF EXISTS step_type;
