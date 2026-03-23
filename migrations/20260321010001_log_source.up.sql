-- Add source column to log_entries for log origin classification
ALTER TABLE log_entries ADD COLUMN source TEXT NOT NULL DEFAULT 'external'
  CHECK (source IN ('system', 'api', 'session', 'external'));

CREATE INDEX idx_log_source ON log_entries(source, timestamp DESC);
CREATE INDEX idx_log_attrs ON log_entries USING GIN (attributes jsonb_path_ops);
