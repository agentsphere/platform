-- Plan 47: Auto-merge fields on merge_requests
ALTER TABLE merge_requests ADD COLUMN auto_merge BOOLEAN NOT NULL DEFAULT false;
ALTER TABLE merge_requests ADD COLUMN auto_merge_by UUID REFERENCES users(id);
ALTER TABLE merge_requests ADD COLUMN auto_merge_method TEXT;
