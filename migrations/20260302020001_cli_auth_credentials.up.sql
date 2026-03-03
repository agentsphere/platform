-- Store Claude CLI auth credentials per user (encrypted).
-- Uses single encrypted_data BYTEA column matching the existing pattern
-- in secrets.encrypted_value — engine::encrypt() returns nonce(12) || ciphertext || tag.
CREATE TABLE cli_credentials (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- 'oauth' (subscription) or 'setup_token' (1-year headless token)
    auth_type TEXT NOT NULL CHECK (auth_type IN ('oauth', 'setup_token')),
    -- Encrypted credential blob: nonce(12) || ciphertext || tag (AES-256-GCM)
    -- For oauth: JSON { access_token, refresh_token, expires_at }
    -- For setup_token: the raw token string
    encrypted_data BYTEA NOT NULL,
    -- When the access token expires (for proactive refresh)
    token_expires_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE(user_id, auth_type)
);

CREATE INDEX idx_cli_credentials_user ON cli_credentials(user_id);
