use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Master key
// ---------------------------------------------------------------------------

/// Parse a hex-encoded 32-byte master key (64 hex chars).
pub fn parse_master_key(hex_str: &str) -> anyhow::Result<[u8; 32]> {
    let bytes = hex::decode(hex_str.trim())
        .map_err(|e| anyhow::anyhow!("invalid PLATFORM_MASTER_KEY hex: {e}"))?;
    let key: [u8; 32] = bytes.try_into().map_err(|v: Vec<u8>| {
        anyhow::anyhow!("PLATFORM_MASTER_KEY must be 32 bytes, got {}", v.len())
    })?;
    Ok(key)
}

/// Derive a deterministic dev-mode key (NOT for production).
pub fn dev_master_key() -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"platform-dev-master-key-not-for-production");
    let result = hasher.finalize();
    let mut key = [0u8; 32];
    key.copy_from_slice(&result);
    key
}

// ---------------------------------------------------------------------------
// Encrypt / Decrypt
// ---------------------------------------------------------------------------

/// Encrypt plaintext with AES-256-GCM. Returns `nonce (12) || ciphertext || tag`.
pub fn encrypt(plaintext: &[u8], master_key: &[u8; 32]) -> anyhow::Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(master_key)
        .map_err(|e| anyhow::anyhow!("failed to create cipher: {e}"))?;

    let mut nonce_bytes = [0u8; 12];
    rand::fill(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| anyhow::anyhow!("encryption failed: {e}"))?;

    let mut result = Vec::with_capacity(12 + ciphertext.len());
    result.extend_from_slice(&nonce_bytes);
    result.extend_from_slice(&ciphertext);
    Ok(result)
}

/// Decrypt data produced by [`encrypt`]. Input: `nonce (12) || ciphertext || tag`.
pub fn decrypt(encrypted: &[u8], master_key: &[u8; 32]) -> anyhow::Result<Vec<u8>> {
    if encrypted.len() < 12 {
        anyhow::bail!("encrypted data too short (need at least 12 bytes for nonce)");
    }

    let cipher = Aes256Gcm::new_from_slice(master_key)
        .map_err(|e| anyhow::anyhow!("failed to create cipher: {e}"))?;

    let nonce = Nonce::from_slice(&encrypted[..12]);
    let ciphertext = &encrypted[12..];

    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| anyhow::anyhow!("decryption failed (wrong key or corrupted data): {e}"))
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct SecretMetadata {
    pub id: Uuid,
    pub project_id: Option<Uuid>,
    pub name: String,
    pub scope: String,
    pub version: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// CRUD
// ---------------------------------------------------------------------------

/// Create or update a secret. On conflict (same project + name), bumps version.
#[tracing::instrument(skip(pool, master_key, value), fields(?project_id, %name, %scope), err)]
pub async fn create_secret(
    pool: &PgPool,
    master_key: &[u8; 32],
    project_id: Option<Uuid>,
    name: &str,
    value: &[u8],
    scope: &str,
    created_by: Uuid,
) -> anyhow::Result<SecretMetadata> {
    let encrypted = encrypt(value, master_key)?;

    let row = sqlx::query!(
        r#"
        INSERT INTO secrets (project_id, name, encrypted_value, scope, created_by)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (project_id, name) WHERE project_id IS NOT NULL
        DO UPDATE SET
            encrypted_value = EXCLUDED.encrypted_value,
            scope = EXCLUDED.scope,
            version = secrets.version + 1,
            updated_at = now()
        RETURNING id, project_id, name, scope, version,
                  created_at, updated_at
        "#,
        project_id,
        name,
        encrypted,
        scope,
        created_by,
    )
    .fetch_one(pool)
    .await?;

    Ok(SecretMetadata {
        id: row.id,
        project_id: row.project_id,
        name: row.name,
        scope: row.scope,
        version: row.version,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

/// Create or update a global secret (`project_id` IS NULL).
#[tracing::instrument(skip(pool, master_key, value), fields(%name, %scope), err)]
pub async fn create_global_secret(
    pool: &PgPool,
    master_key: &[u8; 32],
    name: &str,
    value: &[u8],
    scope: &str,
    created_by: Uuid,
) -> anyhow::Result<SecretMetadata> {
    let encrypted = encrypt(value, master_key)?;

    // The unique index idx_secrets_global_name handles conflicts for NULL project_id
    let row = sqlx::query!(
        r#"
        INSERT INTO secrets (project_id, name, encrypted_value, scope, created_by)
        VALUES (NULL, $1, $2, $3, $4)
        ON CONFLICT (name) WHERE project_id IS NULL
        DO UPDATE SET
            encrypted_value = EXCLUDED.encrypted_value,
            scope = EXCLUDED.scope,
            version = secrets.version + 1,
            updated_at = now()
        RETURNING id, project_id, name, scope, version,
                  created_at, updated_at
        "#,
        name,
        encrypted,
        scope,
        created_by,
    )
    .fetch_one(pool)
    .await?;

    Ok(SecretMetadata {
        id: row.id,
        project_id: row.project_id,
        name: row.name,
        scope: row.scope,
        version: row.version,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

/// Delete a secret by project + name. Returns whether a row was deleted.
#[tracing::instrument(skip(pool), fields(?project_id, %name), err)]
pub async fn delete_secret(
    pool: &PgPool,
    project_id: Option<Uuid>,
    name: &str,
) -> anyhow::Result<bool> {
    let result = if let Some(pid) = project_id {
        sqlx::query!(
            "DELETE FROM secrets WHERE project_id = $1 AND name = $2",
            pid,
            name,
        )
        .execute(pool)
        .await?
    } else {
        sqlx::query!(
            "DELETE FROM secrets WHERE project_id IS NULL AND name = $1",
            name,
        )
        .execute(pool)
        .await?
    };

    Ok(result.rows_affected() > 0)
}

/// List secret metadata for a project (or global if `project_id` is None).
/// Never returns encrypted values.
pub async fn list_secrets(
    pool: &PgPool,
    project_id: Option<Uuid>,
) -> anyhow::Result<Vec<SecretMetadata>> {
    if let Some(pid) = project_id {
        let rows = sqlx::query!(
            r#"
            SELECT id, project_id, name, scope, version, created_at, updated_at
            FROM secrets WHERE project_id = $1
            ORDER BY name
            "#,
            pid,
        )
        .fetch_all(pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| SecretMetadata {
                id: r.id,
                project_id: r.project_id,
                name: r.name,
                scope: r.scope,
                version: r.version,
                created_at: r.created_at,
                updated_at: r.updated_at,
            })
            .collect())
    } else {
        let rows = sqlx::query!(
            r#"
            SELECT id, project_id, name, scope, version, created_at, updated_at
            FROM secrets WHERE project_id IS NULL
            ORDER BY name
            "#,
        )
        .fetch_all(pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| SecretMetadata {
                id: r.id,
                project_id: r.project_id,
                name: r.name,
                scope: r.scope,
                version: r.version,
                created_at: r.created_at,
                updated_at: r.updated_at,
            })
            .collect())
    }
}

/// Resolve (decrypt) a secret by name. Internal-only — NOT exposed via API.
/// Enforces scope matching: a `pipeline`-scoped secret is only resolved when
/// `requested_scope` is `pipeline` or `all`.
#[tracing::instrument(skip(pool, master_key), fields(%project_id, %name, %requested_scope), err)]
pub async fn resolve_secret(
    pool: &PgPool,
    master_key: &[u8; 32],
    project_id: Uuid,
    name: &str,
    requested_scope: &str,
) -> anyhow::Result<String> {
    let row = sqlx::query!(
        r#"
        SELECT encrypted_value, scope
        FROM secrets
        WHERE (project_id = $1 OR project_id IS NULL)
          AND name = $2
        ORDER BY project_id IS NULL  -- prefer project-scoped over global
        LIMIT 1
        "#,
        project_id,
        name,
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| anyhow::anyhow!("secret '{name}' not found"))?;

    // Scope enforcement: secret scope must match the requested scope
    if row.scope != "all" && row.scope != requested_scope && requested_scope != "all" {
        anyhow::bail!(
            "secret '{name}' has scope '{}' but '{}' was requested",
            row.scope,
            requested_scope
        );
    }

    let plaintext = decrypt(&row.encrypted_value, master_key)?;
    String::from_utf8(plaintext)
        .map_err(|e| anyhow::anyhow!("secret value is not valid UTF-8: {e}"))
}

/// Replace `${{ secrets.NAME }}` patterns in a template string.
/// Only resolves secrets matching the given scope.
#[tracing::instrument(skip(pool, master_key, template), fields(%project_id, %scope), err)]
pub async fn resolve_secrets_for_env(
    pool: &PgPool,
    master_key: &[u8; 32],
    project_id: Uuid,
    scope: &str,
    template: &str,
) -> anyhow::Result<String> {
    let mut result = template.to_owned();
    let mut search_from = 0;

    // Match the exact pattern ${{ secrets.NAME }} — no general template engine
    while let Some(start) = result[search_from..].find("${{ secrets.") {
        let abs_start = search_from + start;
        let after_prefix = abs_start + "${{ secrets.".len();
        let Some(end) = result[after_prefix..].find(" }}") else {
            break;
        };
        let abs_end = after_prefix + end + " }}".len();
        let secret_name = &result[after_prefix..after_prefix + end];

        // Validate: secret names must be alphanumeric + - _
        if secret_name.is_empty()
            || !secret_name
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        {
            search_from = abs_end;
            continue;
        }

        match resolve_secret(pool, master_key, project_id, secret_name, scope).await {
            Ok(value) => {
                result.replace_range(abs_start..abs_end, &value);
                search_from = abs_start + value.len();
            }
            Err(e) => {
                tracing::warn!(secret_name, error = %e, "failed to resolve secret in template");
                search_from = abs_end;
            }
        }
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = [42u8; 32];
        let plaintext = b"super-secret-value-123";
        let encrypted = encrypt(plaintext, &key).unwrap();

        // Encrypted should be larger (12 nonce + 16 tag + plaintext len)
        assert!(encrypted.len() > plaintext.len());

        let decrypted = decrypt(&encrypted, &key).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn decrypt_wrong_key_fails() {
        let key1 = [42u8; 32];
        let key2 = [99u8; 32];
        let encrypted = encrypt(b"secret", &key1).unwrap();
        assert!(decrypt(&encrypted, &key2).is_err());
    }

    #[test]
    fn decrypt_corrupted_data_fails() {
        let key = [42u8; 32];
        let mut encrypted = encrypt(b"secret", &key).unwrap();
        // Corrupt a byte in the ciphertext
        if let Some(byte) = encrypted.last_mut() {
            *byte ^= 0xFF;
        }
        assert!(decrypt(&encrypted, &key).is_err());
    }

    #[test]
    fn decrypt_too_short_fails() {
        let key = [42u8; 32];
        assert!(decrypt(&[0u8; 5], &key).is_err());
    }

    #[test]
    fn different_encryptions_differ() {
        let key = [42u8; 32];
        let e1 = encrypt(b"same", &key).unwrap();
        let e2 = encrypt(b"same", &key).unwrap();
        // Different nonces → different ciphertext
        assert_ne!(e1, e2);
    }

    #[test]
    fn parse_master_key_valid() {
        let hex_key = "aa".repeat(32); // 64 hex chars = 32 bytes
        let key = parse_master_key(&hex_key).unwrap();
        assert_eq!(key, [0xaa; 32]);
    }

    #[test]
    fn parse_master_key_wrong_length() {
        assert!(parse_master_key("aabb").is_err());
    }

    #[test]
    fn parse_master_key_invalid_hex() {
        let bad = "zz".repeat(32);
        assert!(parse_master_key(&bad).is_err());
    }

    #[test]
    fn dev_master_key_is_deterministic() {
        assert_eq!(dev_master_key(), dev_master_key());
    }
}
