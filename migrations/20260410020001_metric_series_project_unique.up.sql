-- Drop the old constraint that ignores project_id.
ALTER TABLE metric_series DROP CONSTRAINT metric_series_name_labels_key;

-- Add new constraint including project_id.
-- NULLS NOT DISTINCT: (name, labels, NULL) is also unique — prevents
-- two "unscoped" series with the same name from colliding silently.
ALTER TABLE metric_series
  ADD CONSTRAINT metric_series_name_labels_project_key
  UNIQUE NULLS NOT DISTINCT (name, labels, project_id);
