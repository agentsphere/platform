-- Make registry_repositories.project_id nullable for system/global repos
-- (e.g. platform-runner, platform-runner-bare)

ALTER TABLE registry_repositories ALTER COLUMN project_id DROP NOT NULL;

-- Detach seed images from the synthetic project
UPDATE registry_repositories
   SET project_id = NULL
 WHERE name IN ('platform-runner', 'platform-runner-bare');

-- Remove the synthetic platform-runner project (soft-deleted or real)
DELETE FROM projects WHERE name = 'platform-runner';
