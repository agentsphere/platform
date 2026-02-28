-- Add resource tracking for inventory-based cascade deletes (ArgoCD/Flux pattern).
-- Stores the list of K8s resources applied by each deployment for orphan detection.
ALTER TABLE deployments ADD COLUMN tracked_resources JSONB NOT NULL DEFAULT '[]';
