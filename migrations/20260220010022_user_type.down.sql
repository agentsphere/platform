DROP INDEX idx_users_user_type;
ALTER TABLE users DROP CONSTRAINT chk_users_user_type;
ALTER TABLE users DROP COLUMN metadata;
ALTER TABLE users DROP COLUMN user_type;
