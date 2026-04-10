DROP INDEX IF EXISTS idx_projects_owner_name_active;
ALTER TABLE projects ADD CONSTRAINT projects_owner_id_name_key UNIQUE (owner_id, name);
