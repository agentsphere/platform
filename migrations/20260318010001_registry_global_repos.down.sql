-- Reverse: delete system repos (no project to re-attach), restore NOT NULL
DELETE FROM registry_repositories WHERE project_id IS NULL;

ALTER TABLE registry_repositories ALTER COLUMN project_id SET NOT NULL;
