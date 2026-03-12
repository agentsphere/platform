DROP TABLE IF EXISTS release_assets;
DROP TABLE IF EXISTS releases;

ALTER TABLE pipelines DROP CONSTRAINT IF EXISTS pipelines_trigger_check;
ALTER TABLE pipelines ADD CONSTRAINT pipelines_trigger_check
    CHECK (trigger IN ('push', 'api', 'schedule', 'mr'));
