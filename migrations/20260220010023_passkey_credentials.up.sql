CREATE TABLE passkey_credentials (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id         UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- WebAuthn credential ID (base64url-encoded), sent by browser during auth
    credential_id   BYTEA NOT NULL UNIQUE,
    -- COSE public key (CBOR-encoded)
    public_key      BYTEA NOT NULL,
    -- Signature counter for clone detection
    sign_count      BIGINT NOT NULL DEFAULT 0,
    -- Whether this is a discoverable credential (resident key)
    discoverable    BOOLEAN NOT NULL DEFAULT true,
    -- Transports hint: usb, nfc, ble, internal, hybrid
    transports      TEXT[] NOT NULL DEFAULT '{}',
    -- User-provided name: "MacBook Touch ID", "YubiKey 5C"
    name            TEXT NOT NULL,
    -- Attestation data (optional, stored for enterprise audit)
    attestation     BYTEA,
    -- Backup eligibility and state (from WebAuthn Level 3)
    backup_eligible BOOLEAN NOT NULL DEFAULT false,
    backup_state    BOOLEAN NOT NULL DEFAULT false,
    last_used_at    TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_passkey_credentials_user ON passkey_credentials(user_id);
