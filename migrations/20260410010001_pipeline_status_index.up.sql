-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_pipelines_project_status
ON pipelines(project_id, status, created_at DESC);
