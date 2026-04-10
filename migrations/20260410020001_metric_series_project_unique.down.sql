ALTER TABLE metric_series DROP CONSTRAINT metric_series_name_labels_project_key;
ALTER TABLE metric_series ADD CONSTRAINT metric_series_name_labels_key UNIQUE (name, labels);
