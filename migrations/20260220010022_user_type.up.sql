-- Add user_type discriminator
ALTER TABLE users
    ADD COLUMN user_type TEXT NOT NULL DEFAULT 'human';

-- Add CHECK constraint
ALTER TABLE users
    ADD CONSTRAINT chk_users_user_type
    CHECK (user_type IN ('human', 'agent', 'service_account'));

-- Add metadata column for type-specific config (JSON)
ALTER TABLE users
    ADD COLUMN metadata JSONB;

-- Index for listing by type
CREATE INDEX idx_users_user_type ON users (user_type);
