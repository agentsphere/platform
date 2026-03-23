-- Custom LLM provider configurations (multiple per user, one active at a time)
CREATE TABLE llm_provider_configs (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    provider_type TEXT NOT NULL CHECK (provider_type IN ('bedrock', 'vertex', 'azure_foundry', 'custom_endpoint')),
    label TEXT NOT NULL DEFAULT '',
    encrypted_config BYTEA NOT NULL,
    model TEXT,
    validation_status TEXT NOT NULL DEFAULT 'untested' CHECK (validation_status IN ('untested', 'valid', 'invalid')),
    last_validated_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_llm_provider_configs_user ON llm_provider_configs(user_id);

-- Per-user active LLM provider selection.
-- Values: 'auto', 'oauth', 'api_key', 'custom:{config_id}', 'global'
ALTER TABLE users ADD COLUMN active_llm_provider TEXT NOT NULL DEFAULT 'auto';
