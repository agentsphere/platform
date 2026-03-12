-- Plan 45: Pipeline version, MR head_sha, stale reviews
ALTER TABLE pipelines ADD COLUMN version TEXT;
ALTER TABLE merge_requests ADD COLUMN head_sha TEXT;
ALTER TABLE mr_reviews ADD COLUMN is_stale BOOLEAN NOT NULL DEFAULT false;
