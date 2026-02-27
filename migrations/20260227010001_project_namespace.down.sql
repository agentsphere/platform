DROP INDEX IF EXISTS idx_ops_repos_project;
ALTER TABLE ops_repos DROP COLUMN IF EXISTS project_id;
DROP INDEX IF EXISTS idx_projects_namespace_slug;
ALTER TABLE projects DROP COLUMN IF EXISTS namespace_slug;
