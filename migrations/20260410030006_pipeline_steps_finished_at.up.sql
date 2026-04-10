-- #19: Add finished_at for SARGable queries on step completion time.
ALTER TABLE pipeline_steps ADD COLUMN finished_at TIMESTAMPTZ;
CREATE INDEX idx_pipeline_steps_finished ON pipeline_steps(finished_at)
    WHERE finished_at IS NOT NULL;
