-- Add namespace_slug to projects for per-project K8s namespace isolation.
-- Three-step: add nullable, backfill, set NOT NULL (required for existing rows).

ALTER TABLE projects ADD COLUMN namespace_slug TEXT;

-- Backfill: lowercase FIRST, then replace non-alphanumeric with hyphens, collapse runs, trim
UPDATE projects SET namespace_slug = regexp_replace(
    regexp_replace(lower(name), '[^a-z0-9]', '-', 'g'),
    '-{2,}', '-', 'g'
);
-- Strip leading/trailing hyphens
UPDATE projects SET namespace_slug = trim(both '-' from namespace_slug);
-- Truncate to 40 chars (matching Rust slugify_namespace), then strip any trailing hyphen
UPDATE projects SET namespace_slug = left(namespace_slug, 40);
UPDATE projects SET namespace_slug = rtrim(namespace_slug, '-');

ALTER TABLE projects ALTER COLUMN namespace_slug SET NOT NULL;

-- Partial unique index: only active projects can collide on namespace_slug
CREATE UNIQUE INDEX idx_projects_namespace_slug ON projects(namespace_slug) WHERE is_active = true;

-- Link ops_repos 1:1 to projects (auto-created ops repo per project)
ALTER TABLE ops_repos ADD COLUMN project_id UUID REFERENCES projects(id) ON DELETE CASCADE;
CREATE UNIQUE INDEX idx_ops_repos_project ON ops_repos(project_id) WHERE project_id IS NOT NULL;
