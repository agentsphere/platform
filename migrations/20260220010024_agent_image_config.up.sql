-- Add optional agent container image override per project
ALTER TABLE projects ADD COLUMN agent_image TEXT;

COMMENT ON COLUMN projects.agent_image IS
  'Custom container image for agent sessions. Null uses platform default.';
