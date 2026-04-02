-- Add hostname column to deploy_targets for custom domain exposure
ALTER TABLE deploy_targets ADD COLUMN hostname TEXT;
