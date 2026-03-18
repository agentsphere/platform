ALTER TABLE pipeline_steps ADD COLUMN depends_on TEXT[] NOT NULL DEFAULT '{}';
ALTER TABLE pipeline_steps ADD COLUMN environment JSONB;
ALTER TABLE pipeline_steps ADD COLUMN gate BOOLEAN NOT NULL DEFAULT false;
