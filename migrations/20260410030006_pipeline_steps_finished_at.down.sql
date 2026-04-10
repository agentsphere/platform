DROP INDEX IF EXISTS idx_pipeline_steps_finished;
ALTER TABLE pipeline_steps DROP COLUMN finished_at;
