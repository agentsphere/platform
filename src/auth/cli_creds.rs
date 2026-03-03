use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::secrets::engine;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Valid auth types for CLI credentials.
const VALID_AUTH_TYPES: &[&str] = &["oauth", "setup_token"];

/// Metadata about stored CLI credentials (never exposes the actual credential).
#[derive(Debug, Serialize)]
pub struct CliCredentialInfo {
    pub id: Uuid,
    pub user_id: Uuid,
    pub auth_type: String,
    pub token_expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The decrypted credential value (used internally for session spawning).
#[allow(dead_code)]
pub struct DecryptedCredential {
    pub auth_type: String,
    pub value: String,
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validate that `auth_type` is one of the allowed values.
pub fn validate_auth_type(auth_type: &str) -> Result<(), String> {
    if VALID_AUTH_TYPES.contains(&auth_type) {
        Ok(())
    } else {
        Err(format!(
            "auth_type must be one of: {}",
            VALID_AUTH_TYPES.join(", ")
        ))
    }
}

// ---------------------------------------------------------------------------
// CRUD
// ---------------------------------------------------------------------------

/// Store (or update) encrypted CLI credentials for a user.
///
/// Uses `ON CONFLICT (user_id, auth_type)` to upsert.
#[tracing::instrument(skip(pool, master_key, credential_value), fields(%user_id, %auth_type), err)]
pub async fn store_credentials(
    pool: &PgPool,
    master_key: &[u8; 32],
    user_id: Uuid,
    auth_type: &str,
    credential_value: &str,
    token_expires_at: Option<DateTime<Utc>>,
) -> anyhow::Result<CliCredentialInfo> {
    let encrypted = engine::encrypt(credential_value.as_bytes(), master_key)?;

    let row = sqlx::query!(
        r#"
        INSERT INTO cli_credentials (user_id, auth_type, encrypted_data, token_expires_at)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (user_id, auth_type)
        DO UPDATE SET
            encrypted_data = EXCLUDED.encrypted_data,
            token_expires_at = EXCLUDED.token_expires_at,
            updated_at = now()
        RETURNING id, user_id, auth_type, token_expires_at, created_at, updated_at
        "#,
        user_id,
        auth_type,
        encrypted,
        token_expires_at,
    )
    .fetch_one(pool)
    .await?;

    Ok(CliCredentialInfo {
        id: row.id,
        user_id: row.user_id,
        auth_type: row.auth_type,
        token_expires_at: row.token_expires_at,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

/// Get credential metadata for a user (without decrypting). Returns `None` if
/// no credentials are stored.
#[tracing::instrument(skip(pool), fields(%user_id), err)]
pub async fn get_credential_info(
    pool: &PgPool,
    user_id: Uuid,
) -> anyhow::Result<Option<CliCredentialInfo>> {
    let row = sqlx::query!(
        r#"
        SELECT id, user_id, auth_type, token_expires_at, created_at, updated_at
        FROM cli_credentials
        WHERE user_id = $1
        ORDER BY updated_at DESC
        LIMIT 1
        "#,
        user_id,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| CliCredentialInfo {
        id: r.id,
        user_id: r.user_id,
        auth_type: r.auth_type,
        token_expires_at: r.token_expires_at,
        created_at: r.created_at,
        updated_at: r.updated_at,
    }))
}

/// Decrypt and return the stored credential for a user.
/// Used internally when spawning agent sessions.
#[tracing::instrument(skip(pool, master_key), fields(%user_id), err)]
pub async fn get_decrypted_credential(
    pool: &PgPool,
    master_key: &[u8; 32],
    user_id: Uuid,
) -> anyhow::Result<Option<DecryptedCredential>> {
    let row = sqlx::query!(
        r#"
        SELECT auth_type, encrypted_data
        FROM cli_credentials
        WHERE user_id = $1
        ORDER BY updated_at DESC
        LIMIT 1
        "#,
        user_id,
    )
    .fetch_optional(pool)
    .await?;

    match row {
        Some(r) => {
            let plaintext = engine::decrypt(&r.encrypted_data, master_key)?;
            let value = String::from_utf8(plaintext)
                .map_err(|e| anyhow::anyhow!("credential is not valid UTF-8: {e}"))?;
            Ok(Some(DecryptedCredential {
                auth_type: r.auth_type,
                value,
            }))
        }
        None => Ok(None),
    }
}

/// Delete all CLI credentials for a user. Returns whether any rows were deleted.
#[tracing::instrument(skip(pool), fields(%user_id), err)]
pub async fn delete_credentials(pool: &PgPool, user_id: Uuid) -> anyhow::Result<bool> {
    let result = sqlx::query!("DELETE FROM cli_credentials WHERE user_id = $1", user_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

// ---------------------------------------------------------------------------
// Auth resolution for pod sessions
// ---------------------------------------------------------------------------

/// Resolve CLI credentials for injecting into an agent pod as `CLAUDE_CODE_OAUTH_TOKEN`.
///
/// Returns `Some(token_value)` if the user has stored CLI credentials (oauth or `setup_token`),
/// or `None` if no credentials are found. Used by `create_session()` to decide between
/// subscription auth and API key auth.
#[tracing::instrument(skip(pool, master_key), fields(%user_id), err)]
pub async fn resolve_cli_auth(
    pool: &PgPool,
    master_key: &[u8; 32],
    user_id: Uuid,
) -> anyhow::Result<Option<String>> {
    match get_decrypted_credential(pool, master_key, user_id).await? {
        Some(cred) => Ok(Some(cred.value)),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_auth_type_accepts_oauth() {
        assert!(validate_auth_type("oauth").is_ok());
    }

    #[test]
    fn validate_auth_type_accepts_setup_token() {
        assert!(validate_auth_type("setup_token").is_ok());
    }

    #[test]
    fn validate_auth_type_rejects_unknown() {
        assert!(validate_auth_type("api_key").is_err());
        assert!(validate_auth_type("").is_err());
        assert!(validate_auth_type("OAUTH").is_err());
    }

    #[test]
    fn encrypt_decrypt_roundtrip_setup_token() {
        let key = [42u8; 32];
        let token = "sk-ant-ccode01-aBcDeFgHiJkLmNoPqRsTuVwXyZ";
        let encrypted = engine::encrypt(token.as_bytes(), &key).unwrap();
        let decrypted = engine::decrypt(&encrypted, &key).unwrap();
        assert_eq!(String::from_utf8(decrypted).unwrap(), token);
    }

    #[test]
    fn encrypt_decrypt_roundtrip_oauth_json() {
        let key = [42u8; 32];
        let oauth_json = r#"{"access_token":"at-123","refresh_token":"rt-456","expires_at":"2026-03-02T12:00:00Z"}"#;
        let encrypted = engine::encrypt(oauth_json.as_bytes(), &key).unwrap();
        let decrypted = engine::decrypt(&encrypted, &key).unwrap();
        assert_eq!(String::from_utf8(decrypted).unwrap(), oauth_json);
    }

    #[test]
    fn decrypt_with_wrong_key_fails() {
        let key1 = [42u8; 32];
        let key2 = [99u8; 32];
        let encrypted = engine::encrypt(b"my-token", &key1).unwrap();
        let result = engine::decrypt(&encrypted, &key2);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("decryption failed")
        );
    }

    #[test]
    fn decrypt_with_truncated_data_fails() {
        let key = [42u8; 32];
        // Less than 12 bytes (nonce size)
        let result = engine::decrypt(&[0u8; 5], &key);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too short"));
    }
}
