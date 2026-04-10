-- Drop table-level constraint (includes soft-deleted rows).
ALTER TABLE projects DROP CONSTRAINT projects_owner_id_name_key;

-- Replace with partial unique index (active rows only).
-- Matches the pattern used for namespace_slug.
CREATE UNIQUE INDEX idx_projects_owner_name_active
  ON projects(owner_id, name)
  WHERE is_active = true;
