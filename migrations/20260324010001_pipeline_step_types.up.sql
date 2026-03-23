-- Add step_type, step_config, and started_at to pipeline_steps for new step types
-- (imagebuild, gitops_sync, deploy_watch).
ALTER TABLE pipeline_steps
    ADD COLUMN step_type TEXT NOT NULL DEFAULT 'command'
        CHECK (step_type IN ('command', 'imagebuild', 'deploy_test', 'gitops_sync', 'deploy_watch')),
    ADD COLUMN step_config JSONB,
    ADD COLUMN started_at TIMESTAMPTZ;

-- Backfill: existing rows with deploy_test JSON → step_type = 'deploy_test'
UPDATE pipeline_steps SET step_type = 'deploy_test' WHERE deploy_test IS NOT NULL;

-- Add include_staging project setting (controls gitops_sync target branch).
-- Stored in projects table so dev agents cannot override via .platform.yaml.
ALTER TABLE projects
    ADD COLUMN include_staging BOOLEAN NOT NULL DEFAULT false;

-- Expand secret scopes: replace 'deploy' with environment-specific scopes.
-- Current: pipeline, agent, deploy, all
-- New:     pipeline, agent, test, staging, prod, all
-- Step 1: drop old constraint first so the update can happen.
ALTER TABLE secrets DROP CONSTRAINT IF EXISTS secrets_scope_check;
-- Step 2: update existing 'deploy' rows to 'staging' (safe default).
UPDATE secrets SET scope = 'staging' WHERE scope = 'deploy';
-- Step 3: add new constraint.
ALTER TABLE secrets ADD CONSTRAINT secrets_scope_check
    CHECK (scope IN ('all', 'pipeline', 'agent', 'test', 'staging', 'prod'));
