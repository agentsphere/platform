DROP INDEX IF EXISTS idx_log_attrs;
DROP INDEX IF EXISTS idx_log_source;
ALTER TABLE log_entries DROP COLUMN IF EXISTS source;
