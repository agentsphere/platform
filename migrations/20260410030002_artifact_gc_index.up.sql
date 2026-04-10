-- Artifact GC worker: SELECT ... WHERE expires_at IS NOT NULL AND expires_at < now()
-- Without this index, cleanup does a sequential scan of the entire artifacts table.
CREATE INDEX idx_artifacts_expires_at
  ON artifacts(expires_at)
  WHERE expires_at IS NOT NULL;
