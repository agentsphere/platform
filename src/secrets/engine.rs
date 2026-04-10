// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

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

/// Validate master key format without panicking.
/// Returns `Ok(())` if the key is a valid 64-char hex string (32 bytes).
pub fn validate_master_key(key: &str) -> Result<(), String> {
    let trimmed = key.trim();
    if trimmed.len() != 64 {
        return Err(format!("expected 64 hex characters, got {}", trimmed.len()));
    }
    hex::decode(trimmed).map_err(|e| format!("not valid hex: {e}"))?;
    Ok(())
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

/// Version byte prepended to encrypted output (S44: multi-key rotation support).
const ENCRYPTION_VERSION: u8 = 0x01;

/// Encrypt plaintext with AES-256-GCM. Returns `0x01 || nonce (12) || ciphertext || tag`.
///
/// The leading version byte (`0x01`) identifies data encrypted with the current format.
/// Legacy data (without version byte) is still decryptable via [`decrypt`].
pub fn encrypt(plaintext: &[u8], master_key: &[u8; 32]) -> anyhow::Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(master_key)
        .map_err(|e| anyhow::anyhow!("failed to create cipher: {e}"))?;

    let mut nonce_bytes = [0u8; 12];
    rand::fill(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| anyhow::anyhow!("encryption failed: {e}"))?;

    let mut result = Vec::with_capacity(1 + 12 + ciphertext.len());
    result.push(ENCRYPTION_VERSION);
    result.extend_from_slice(&nonce_bytes);
    result.extend_from_slice(&ciphertext);
    Ok(result)
}

/// Decrypt data produced by [`encrypt`], with optional previous key for rotation (S44).
///
/// Format detection:
/// - First byte `0x01` → versioned format: strip version byte, decrypt `nonce(12) || ciphertext`.
/// - Any other first byte → legacy format (no version byte): decrypt entire blob as `nonce(12) || ciphertext`.
///
/// If decryption with `master_key` fails and `previous_key` is `Some`, retries with the previous key.
/// This allows seamless key rotation: re-encrypt data at leisure while both keys work.
///
// TODO(A85): Add `zeroize` crate to clear decrypted plaintext from memory after use.
// The returned Vec<u8> should implement Drop with zeroize to prevent secrets from
// lingering in memory after the caller drops the buffer.
pub fn decrypt(
    encrypted: &[u8],
    master_key: &[u8; 32],
    previous_key: Option<&[u8; 32]>,
) -> anyhow::Result<Vec<u8>> {
    // Determine payload (strip version byte if present)
    let payload = if encrypted.first() == Some(&ENCRYPTION_VERSION) {
        &encrypted[1..]
    } else {
        encrypted
    };

    if payload.len() < 12 {
        anyhow::bail!("encrypted data too short (need at least 12 bytes for nonce)");
    }

    // Try current key first
    match decrypt_raw(payload, master_key) {
        Ok(plaintext) => Ok(plaintext),
        Err(current_err) => {
            // If a previous key is available, try it
            if let Some(prev) = previous_key
                && let Ok(plaintext) = decrypt_raw(payload, prev)
            {
                return Ok(plaintext);
            }
            // Return the original error from the current key attempt
            Err(current_err)
        }
    }
}

/// Low-level AES-256-GCM decryption: `nonce (12) || ciphertext || tag`.
fn decrypt_raw(payload: &[u8], key: &[u8; 32]) -> anyhow::Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| anyhow::anyhow!("failed to create cipher: {e}"))?;

    let nonce = Nonce::from_slice(&payload[..12]);
    let ciphertext = &payload[12..];

    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| anyhow::anyhow!("decryption failed (wrong key or corrupted data): {e}"))
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, ts_rs::TS)]
#[ts(export, rename = "Secret")]
pub struct SecretMetadata {
    pub id: Uuid,
    pub project_id: Option<Uuid>,
    pub workspace_id: Option<Uuid>,
    pub environment: Option<String>,
    pub name: String,
    pub scope: String,
    pub version: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// CRUD
// ---------------------------------------------------------------------------

/// Parameters for creating/updating a secret.
pub struct CreateSecretParams<'a> {
    pub project_id: Option<Uuid>,
    pub workspace_id: Option<Uuid>,
    pub environment: Option<&'a str>,
    pub name: &'a str,
    pub value: &'a [u8],
    pub scope: &'a str,
    pub created_by: Uuid,
}

/// Create or update a secret with full hierarchy support.
/// Uniqueness is enforced by the `idx_secrets_scoped` index on
/// (`workspace_id`, `project_id`, `environment`, `name`) with COALESCE.
#[tracing::instrument(skip(pool, master_key, params), err)]
pub async fn create_secret(
    pool: &PgPool,
    master_key: &[u8; 32],
    params: CreateSecretParams<'_>,
) -> anyhow::Result<SecretMetadata> {
    let encrypted = encrypt(params.value, master_key)?;

    // Use the scoped index for conflict detection
    let row = sqlx::query!(
        r#"
        INSERT INTO secrets (project_id, workspace_id, environment, name, encrypted_value, scope, created_by)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (
            COALESCE(workspace_id, '00000000-0000-0000-0000-000000000000'::uuid),
            COALESCE(project_id,   '00000000-0000-0000-0000-000000000000'::uuid),
            COALESCE(environment,  '__none__'),
            name
        )
        DO UPDATE SET
            encrypted_value = EXCLUDED.encrypted_value,
            scope = EXCLUDED.scope,
            version = secrets.version + 1,
            updated_at = now()
        RETURNING id, project_id, workspace_id, environment, name, scope, version,
                  created_at, updated_at
        "#,
        params.project_id,
        params.workspace_id,
        params.environment,
        params.name,
        encrypted,
        params.scope,
        params.created_by,
    )
    .fetch_one(pool)
    .await?;

    Ok(SecretMetadata {
        id: row.id,
        project_id: row.project_id,
        workspace_id: row.workspace_id,
        environment: row.environment,
        name: row.name,
        scope: row.scope,
        version: row.version,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

/// Create or update a global secret (`project_id`, `workspace_id`, `environment` all NULL).
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

    // The unique index idx_secrets_global_name handles conflicts for fully-NULL scoping
    let row = sqlx::query!(
        r#"
        INSERT INTO secrets (project_id, workspace_id, environment, name, encrypted_value, scope, created_by)
        VALUES (NULL, NULL, NULL, $1, $2, $3, $4)
        ON CONFLICT (name) WHERE project_id IS NULL AND workspace_id IS NULL AND environment IS NULL
        DO UPDATE SET
            encrypted_value = EXCLUDED.encrypted_value,
            scope = EXCLUDED.scope,
            version = secrets.version + 1,
            updated_at = now()
        RETURNING id, project_id, workspace_id, environment, name, scope, version,
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
        workspace_id: row.workspace_id,
        environment: row.environment,
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
/// Never returns encrypted values. Optionally filter by environment.
pub async fn list_secrets(
    pool: &PgPool,
    project_id: Option<Uuid>,
    environment: Option<&str>,
) -> anyhow::Result<Vec<SecretMetadata>> {
    if let Some(pid) = project_id {
        let rows = sqlx::query!(
            r#"
            SELECT id, project_id, workspace_id, environment, name, scope, version, created_at, updated_at
            FROM secrets
            WHERE project_id = $1
              AND ($2::text IS NULL OR environment IS NOT DISTINCT FROM $2)
            ORDER BY name
            "#,
            pid,
            environment,
        )
        .fetch_all(pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| SecretMetadata {
                id: r.id,
                project_id: r.project_id,
                workspace_id: r.workspace_id,
                environment: r.environment,
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
            SELECT id, project_id, workspace_id, environment, name, scope, version, created_at, updated_at
            FROM secrets
            WHERE project_id IS NULL AND workspace_id IS NULL AND environment IS NULL
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
                workspace_id: r.workspace_id,
                environment: r.environment,
                name: r.name,
                scope: r.scope,
                version: r.version,
                created_at: r.created_at,
                updated_at: r.updated_at,
            })
            .collect())
    }
}

/// List secret metadata for a workspace. Never returns encrypted values.
pub async fn list_workspace_secrets(
    pool: &PgPool,
    workspace_id: Uuid,
) -> anyhow::Result<Vec<SecretMetadata>> {
    let rows = sqlx::query!(
        r#"
        SELECT id, project_id, workspace_id, environment, name, scope, version, created_at, updated_at
        FROM secrets
        WHERE workspace_id = $1 AND project_id IS NULL
        ORDER BY name
        "#,
        workspace_id,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| SecretMetadata {
            id: r.id,
            project_id: r.project_id,
            workspace_id: r.workspace_id,
            environment: r.environment,
            name: r.name,
            scope: r.scope,
            version: r.version,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
        .collect())
}

/// Resolve (decrypt) a secret by name. Internal-only — NOT exposed via API.
/// Enforces scope matching: a `pipeline`-scoped secret is only resolved when
/// `requested_scope` is `pipeline` or `all`.
///
/// This is the simple resolution: project-scoped > global.
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
          AND workspace_id IS NULL
          AND environment IS NULL
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

    let plaintext = decrypt(&row.encrypted_value, master_key, None)?;
    String::from_utf8(plaintext)
        .map_err(|e| anyhow::anyhow!("secret value is not valid UTF-8: {e}"))
}

/// Resolve (decrypt) a global secret by name (`project_id IS NULL`).
/// Enforces scope matching like [`resolve_secret`].
#[tracing::instrument(skip(pool, master_key), fields(%name, %requested_scope), err)]
pub async fn resolve_global_secret(
    pool: &PgPool,
    master_key: &[u8; 32],
    name: &str,
    requested_scope: &str,
) -> anyhow::Result<String> {
    let row = sqlx::query!(
        r#"
        SELECT encrypted_value, scope
        FROM secrets
        WHERE project_id IS NULL AND workspace_id IS NULL AND environment IS NULL
          AND name = $1
        LIMIT 1
        "#,
        name,
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| anyhow::anyhow!("global secret '{name}' not found"))?;

    if row.scope != "all" && row.scope != requested_scope && requested_scope != "all" {
        anyhow::bail!(
            "global secret '{name}' has scope '{}' but '{requested_scope}' was requested",
            row.scope
        );
    }

    let plaintext = decrypt(&row.encrypted_value, master_key, None)?;
    String::from_utf8(plaintext)
        .map_err(|e| anyhow::anyhow!("secret value is not valid UTF-8: {e}"))
}

/// Resolve (decrypt) a secret using the full hierarchy (most specific wins):
///
/// 1. Project + Environment  (`project_id = X, environment = staging`)
/// 2. Project                (`project_id = X, environment IS NULL`)
/// 3. Workspace              (`workspace_id = W, project_id IS NULL`)
/// 4. Global                 (all NULLs)
///
/// Enforces scope matching like `resolve_secret`.
#[tracing::instrument(skip(pool, master_key), fields(%project_id, ?workspace_id, ?environment, %name, %requested_scope), err)]
pub async fn resolve_secret_hierarchical(
    pool: &PgPool,
    master_key: &[u8; 32],
    project_id: Uuid,
    workspace_id: Option<Uuid>,
    environment: Option<&str>,
    name: &str,
    requested_scope: &str,
) -> anyhow::Result<String> {
    // Query all matching rows ordered by specificity, take the first one.
    // Specificity: project+env > project > workspace > global
    let row = sqlx::query!(
        r#"
        SELECT encrypted_value, scope,
               project_id, workspace_id, environment
        FROM secrets
        WHERE name = $1
          AND (
              -- Level 1: project + environment
              (project_id = $2 AND environment = $3)
              -- Level 2: project (no env)
              OR (project_id = $2 AND environment IS NULL)
              -- Level 3: workspace (no project)
              OR (workspace_id = $4 AND project_id IS NULL AND environment IS NULL)
              -- Level 4: global
              OR (project_id IS NULL AND workspace_id IS NULL AND environment IS NULL)
          )
        ORDER BY
            CASE
                WHEN project_id = $2 AND environment = $3 THEN 0
                WHEN project_id = $2 AND environment IS NULL THEN 1
                WHEN workspace_id = $4 AND project_id IS NULL THEN 2
                ELSE 3
            END
        LIMIT 1
        "#,
        name,
        project_id,
        environment,
        workspace_id,
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| anyhow::anyhow!("secret '{name}' not found"))?;

    if row.scope != "all" && row.scope != requested_scope && requested_scope != "all" {
        anyhow::bail!(
            "secret '{name}' has scope '{}' but '{}' was requested",
            row.scope,
            requested_scope
        );
    }

    let plaintext = decrypt(&row.encrypted_value, master_key, None)?;
    String::from_utf8(plaintext)
        .map_err(|e| anyhow::anyhow!("secret value is not valid UTF-8: {e}"))
}

/// Extract secret names from a template string matching `${{ secrets.NAME }}` patterns.
/// Returns (`start_pos`, `end_pos`, `secret_name`) for each valid match.
fn extract_secret_patterns(template: &str) -> Vec<(usize, usize, String)> {
    let mut results = Vec::new();
    let mut search_from = 0;

    while let Some(start) = template[search_from..].find("${{ secrets.") {
        let abs_start = search_from + start;
        let after_prefix = abs_start + "${{ secrets.".len();
        let Some(end) = template[after_prefix..].find(" }}") else {
            break;
        };
        let abs_end = after_prefix + end + " }}".len();
        let secret_name = &template[after_prefix..after_prefix + end];

        if !secret_name.is_empty()
            && secret_name
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        {
            results.push((abs_start, abs_end, secret_name.to_string()));
        }

        search_from = abs_end;
    }

    results
}

/// Query and decrypt all secrets for a project matching the given scopes
/// and environment. Returns `(name, decrypted_value)` pairs.
///
/// When `environment` is `Some("production")`, matches secrets with that
/// environment plus env-less (NULL) secrets. When `None`, only matches
/// env-less secrets.
#[tracing::instrument(skip(pool, master_key), fields(%project_id, ?environment), err)]
pub async fn query_scoped_secrets(
    pool: &PgPool,
    master_key: &[u8; 32],
    project_id: Uuid,
    scopes: &[&str],
    environment: Option<&str>,
) -> anyhow::Result<Vec<(String, String)>> {
    let rows = sqlx::query!(
        r#"
        SELECT name, encrypted_value
        FROM secrets
        WHERE project_id = $1
          AND scope = ANY($2)
          AND (environment = $3 OR environment IS NULL)
        ORDER BY name
        "#,
        project_id,
        scopes as &[&str],
        environment,
    )
    .fetch_all(pool)
    .await?;

    let mut result = Vec::with_capacity(rows.len());
    for row in rows {
        match decrypt(&row.encrypted_value, master_key, None) {
            Ok(plaintext) => {
                let value = String::from_utf8(plaintext).map_err(|e| {
                    anyhow::anyhow!("secret '{}' is not valid UTF-8: {e}", row.name)
                })?;
                result.push((row.name, value));
            }
            Err(e) => {
                tracing::warn!(secret = %row.name, error = %e, "skipping undecryptable secret");
            }
        }
    }

    Ok(result)
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

        // Encrypted should be larger (1 version + 12 nonce + 16 tag + plaintext len)
        assert!(encrypted.len() > plaintext.len());
        // S44: First byte should be the version byte
        assert_eq!(encrypted[0], ENCRYPTION_VERSION);

        let decrypted = decrypt(&encrypted, &key, None).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn decrypt_wrong_key_fails() {
        let key1 = [42u8; 32];
        let key2 = [99u8; 32];
        let encrypted = encrypt(b"secret", &key1).unwrap();
        let err = decrypt(&encrypted, &key2, None).unwrap_err();
        assert!(
            err.to_string().contains("decryption failed"),
            "wrong key should produce decryption failure, got: {err}"
        );
    }

    #[test]
    fn decrypt_corrupted_data_fails() {
        let key = [42u8; 32];
        let mut encrypted = encrypt(b"secret", &key).unwrap();
        // Corrupt a byte in the ciphertext
        if let Some(byte) = encrypted.last_mut() {
            *byte ^= 0xFF;
        }
        let err = decrypt(&encrypted, &key, None).unwrap_err();
        assert!(
            err.to_string().contains("decryption failed"),
            "corrupted data should produce decryption failure, got: {err}"
        );
    }

    #[test]
    fn decrypt_too_short_fails() {
        let key = [42u8; 32];
        let err = decrypt(&[0u8; 5], &key, None).unwrap_err();
        assert!(
            err.to_string().contains("too short"),
            "too-short data should mention 'too short', got: {err}"
        );
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
        let err = parse_master_key("aabb").unwrap_err();
        assert!(
            err.to_string().contains("32 bytes"),
            "wrong-length key should mention '32 bytes', got: {err}"
        );
    }

    #[test]
    fn parse_master_key_invalid_hex() {
        let bad = "zz".repeat(32);
        let err = parse_master_key(&bad).unwrap_err();
        assert!(
            err.to_string().contains("hex"),
            "invalid hex should mention 'hex', got: {err}"
        );
    }

    #[test]
    fn dev_master_key_is_not_all_zeros_and_works_as_key() {
        let key = dev_master_key();
        assert_ne!(key, [0u8; 32], "dev key should not be all zeros");
        // Verify it works as a valid encryption key
        let encrypted = encrypt(b"test", &key).unwrap();
        let decrypted = decrypt(&encrypted, &key, None).unwrap();
        assert_eq!(decrypted, b"test");
    }

    #[test]
    fn encrypt_empty_plaintext_roundtrips() {
        let key = [42u8; 32];
        let encrypted = encrypt(b"", &key).unwrap();
        // Should have 1 version + nonce (12) + tag (16) even for empty plaintext
        assert_eq!(encrypted.len(), 1 + 12 + 16);
        let decrypted = decrypt(&encrypted, &key, None).unwrap();
        assert!(decrypted.is_empty());
    }

    #[test]
    fn parse_master_key_63_hex_chars_fails() {
        // Odd number of hex chars — hex::decode fails
        let hex_key = "a".repeat(63);
        let err = parse_master_key(&hex_key).unwrap_err();
        assert!(
            err.to_string().contains("hex"),
            "63-char hex key should fail with hex error, got: {err}"
        );
    }

    #[test]
    fn parse_master_key_65_hex_chars_fails() {
        // 65 hex chars is odd, so hex::decode fails
        let hex_key = "a".repeat(65);
        let err = parse_master_key(&hex_key).unwrap_err();
        assert!(
            err.to_string().contains("hex"),
            "65-char hex key should fail with hex error, got: {err}"
        );
    }

    #[test]
    fn parse_master_key_66_hex_chars_fails() {
        // 66 hex chars = 33 bytes, hex::decode succeeds but try_into fails
        let hex_key = "a".repeat(66);
        let err = parse_master_key(&hex_key).unwrap_err();
        assert!(
            err.to_string().contains("32 bytes"),
            "66-char hex key should fail with length error, got: {err}"
        );
    }

    #[test]
    fn parse_master_key_trims_whitespace() {
        let hex_key = format!("  {}  ", "aa".repeat(32));
        let key = parse_master_key(&hex_key).unwrap();
        assert_eq!(key, [0xaa; 32]);
    }

    #[test]
    fn decrypt_nonce_only_no_ciphertext_fails() {
        // Exactly 12 bytes (nonce) but no ciphertext or tag — treated as legacy (no version byte)
        let key = [42u8; 32];
        let err = decrypt(&[0u8; 12], &key, None).unwrap_err();
        assert!(
            err.to_string().contains("decryption failed"),
            "nonce-only data should fail decryption, got: {err}"
        );
    }

    #[test]
    fn encrypt_large_plaintext_roundtrip() {
        let key = [42u8; 32];
        let large = "x".repeat(100_000); // 100KB
        let encrypted = encrypt(large.as_bytes(), &key).unwrap();
        let decrypted = decrypt(&encrypted, &key, None).unwrap();
        assert_eq!(decrypted, large.as_bytes());
    }

    // -- S44: Multi-key rotation tests --

    #[test]
    fn decrypt_with_previous_key() {
        let old_key = [42u8; 32];
        let new_key = [99u8; 32];
        // Data encrypted with old key
        let encrypted = encrypt(b"rotated-secret", &old_key).unwrap();
        // Decrypt with new (current) key fails, but previous key succeeds
        let decrypted = decrypt(&encrypted, &new_key, Some(&old_key)).unwrap();
        assert_eq!(decrypted, b"rotated-secret");
    }

    #[test]
    fn decrypt_prefers_current_key_over_previous() {
        let current_key = [42u8; 32];
        let previous_key = [99u8; 32];
        // Data encrypted with current key
        let encrypted = encrypt(b"current-secret", &current_key).unwrap();
        // Should succeed with current key even when previous key is also provided
        let decrypted = decrypt(&encrypted, &current_key, Some(&previous_key)).unwrap();
        assert_eq!(decrypted, b"current-secret");
    }

    #[test]
    fn decrypt_legacy_data_without_version_prefix() {
        // Simulate legacy data: nonce(12) || ciphertext+tag (no 0x01 prefix)
        let key = [42u8; 32];
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let mut nonce_bytes = [0u8; 12];
        rand::fill(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = cipher.encrypt(nonce, b"legacy-data".as_ref()).unwrap();
        let mut legacy_blob = Vec::new();
        legacy_blob.extend_from_slice(&nonce_bytes);
        legacy_blob.extend_from_slice(&ciphertext);
        // First byte is NOT 0x01 (it's part of the nonce), so treated as legacy
        assert_ne!(legacy_blob[0], ENCRYPTION_VERSION);
        let decrypted = decrypt(&legacy_blob, &key, None).unwrap();
        assert_eq!(decrypted, b"legacy-data");
    }

    #[test]
    fn decrypt_legacy_data_with_previous_key_fallback() {
        // Legacy data encrypted with old key, current key is different
        let old_key = [42u8; 32];
        let new_key = [99u8; 32];
        let cipher = Aes256Gcm::new_from_slice(&old_key).unwrap();
        let mut nonce_bytes = [0u8; 12];
        rand::fill(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = cipher.encrypt(nonce, b"old-secret".as_ref()).unwrap();
        let mut legacy_blob = Vec::new();
        legacy_blob.extend_from_slice(&nonce_bytes);
        legacy_blob.extend_from_slice(&ciphertext);
        // Should fail with new key, succeed with old key as previous
        let decrypted = decrypt(&legacy_blob, &new_key, Some(&old_key)).unwrap();
        assert_eq!(decrypted, b"old-secret");
    }

    #[test]
    fn decrypt_fails_when_neither_key_works() {
        let key1 = [42u8; 32];
        let key2 = [99u8; 32];
        let key3 = [77u8; 32];
        let encrypted = encrypt(b"secret", &key1).unwrap();
        let err = decrypt(&encrypted, &key2, Some(&key3)).unwrap_err();
        assert!(
            err.to_string().contains("decryption failed"),
            "should fail when neither key works, got: {err}"
        );
    }

    #[test]
    fn encrypt_output_has_version_prefix() {
        let key = [42u8; 32];
        let encrypted = encrypt(b"test", &key).unwrap();
        assert_eq!(encrypted[0], 0x01, "first byte should be version 0x01");
        // Total: 1 (version) + 12 (nonce) + 4 (plaintext "test") + 16 (tag)
        assert_eq!(encrypted.len(), 1 + 12 + 4 + 16);
    }

    // -- extract_secret_patterns --

    #[test]
    fn secret_pattern_single() {
        let patterns = extract_secret_patterns("DB_URL=${{ secrets.DATABASE_URL }}");
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].2, "DATABASE_URL");
    }

    #[test]
    fn secret_pattern_multiple() {
        let template = "${{ secrets.DB_HOST }}:${{ secrets.DB_PORT }}";
        let patterns = extract_secret_patterns(template);
        assert_eq!(patterns.len(), 2);
        assert_eq!(patterns[0].2, "DB_HOST");
        assert_eq!(patterns[1].2, "DB_PORT");
    }

    #[test]
    fn secret_pattern_no_match() {
        let patterns = extract_secret_patterns("just a plain string");
        assert!(patterns.is_empty());
    }

    #[test]
    fn secret_pattern_invalid_name_skipped() {
        // Spaces in name
        let patterns = extract_secret_patterns("${{ secrets.bad name }}");
        assert!(patterns.is_empty());
    }

    #[test]
    fn secret_pattern_empty_name_skipped() {
        let patterns = extract_secret_patterns("${{ secrets. }}");
        assert!(patterns.is_empty());
    }

    #[test]
    fn secret_pattern_unclosed_brace() {
        let patterns = extract_secret_patterns("${{ secrets.OPEN");
        assert!(patterns.is_empty());
    }

    #[test]
    fn secret_pattern_hyphen_underscore_valid() {
        let patterns = extract_secret_patterns("${{ secrets.my-secret_v2 }}");
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].2, "my-secret_v2");
    }

    #[test]
    fn secret_pattern_positions_correct() {
        let template = "prefix${{ secrets.KEY }}suffix";
        let patterns = extract_secret_patterns(template);
        assert_eq!(patterns.len(), 1);
        let (start, end, _) = &patterns[0];
        assert_eq!(&template[*start..*end], "${{ secrets.KEY }}");
    }

    #[test]
    fn secret_pattern_adjacent() {
        let template = "${{ secrets.A }}${{ secrets.B }}";
        let patterns = extract_secret_patterns(template);
        assert_eq!(patterns.len(), 2);
        assert_eq!(patterns[0].2, "A");
        assert_eq!(patterns[1].2, "B");
    }

    // -- Additional encrypt/decrypt tests --

    #[test]
    fn encrypt_binary_data_roundtrip() {
        let key = [42u8; 32];
        // Binary data with null bytes, high bytes, etc.
        let plaintext: Vec<u8> = (0..=255u8).collect();
        let encrypted = encrypt(&plaintext, &key).unwrap();
        let decrypted = decrypt(&encrypted, &key, None).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn decrypt_version_byte_only_fails() {
        let key = [42u8; 32];
        // Just the version byte and nothing else
        let err = decrypt(&[ENCRYPTION_VERSION], &key, None).unwrap_err();
        assert!(
            err.to_string().contains("too short"),
            "single version byte should fail with 'too short', got: {err}"
        );
    }

    #[test]
    fn decrypt_empty_input_fails() {
        let key = [42u8; 32];
        let err = decrypt(&[], &key, None).unwrap_err();
        assert!(
            err.to_string().contains("too short"),
            "empty input should fail with 'too short', got: {err}"
        );
    }

    #[test]
    fn decrypt_version_byte_plus_short_nonce_fails() {
        let key = [42u8; 32];
        // Version byte + 5 bytes (not enough for 12-byte nonce)
        let err = decrypt(&[ENCRYPTION_VERSION, 1, 2, 3, 4, 5], &key, None).unwrap_err();
        assert!(
            err.to_string().contains("too short"),
            "version + short nonce should fail with 'too short', got: {err}"
        );
    }

    #[test]
    fn decrypt_with_previous_key_both_wrong_fails() {
        let key1 = [10u8; 32];
        let key2 = [20u8; 32];
        let key3 = [30u8; 32];
        let encrypted = encrypt(b"test data", &key1).unwrap();
        let err = decrypt(&encrypted, &key2, Some(&key3)).unwrap_err();
        assert!(
            err.to_string().contains("decryption failed"),
            "both wrong keys should fail, got: {err}"
        );
    }

    #[test]
    fn decrypt_previous_key_none_uses_only_current() {
        let key = [42u8; 32];
        let encrypted = encrypt(b"test", &key).unwrap();
        let decrypted = decrypt(&encrypted, &key, None).unwrap();
        assert_eq!(decrypted, b"test");
    }

    #[test]
    fn dev_master_key_is_deterministic() {
        let key1 = dev_master_key();
        let key2 = dev_master_key();
        assert_eq!(key1, key2, "dev_master_key should be deterministic");
    }

    #[test]
    fn parse_master_key_empty_string_fails() {
        let err = parse_master_key("").unwrap_err();
        assert!(
            err.to_string().contains("32 bytes") || err.to_string().contains("hex"),
            "empty string should fail, got: {err}"
        );
    }

    #[test]
    fn parse_master_key_with_leading_trailing_whitespace() {
        let hex_key = format!("\t{}\n", "bb".repeat(32));
        let key = parse_master_key(&hex_key).unwrap();
        assert_eq!(key, [0xbb; 32]);
    }

    #[test]
    fn parse_master_key_uppercase_hex() {
        let hex_key = "AA".repeat(32);
        let key = parse_master_key(&hex_key).unwrap();
        assert_eq!(key, [0xaa; 32]);
    }

    #[test]
    fn parse_master_key_mixed_case_hex() {
        // 64 hex chars (32 bytes) with mixed case — should succeed
        let hex_key = "aAbBcCdDeEfF0011".repeat(4);
        assert_eq!(hex_key.len(), 64);
        let result = parse_master_key(&hex_key);
        assert!(result.is_ok(), "mixed case hex should parse: {result:?}");
    }

    // -- Additional extract_secret_patterns tests --

    #[test]
    fn secret_pattern_special_chars_in_name_rejected() {
        let patterns = extract_secret_patterns("${{ secrets.MY.SECRET }}");
        assert!(
            patterns.is_empty(),
            "dots in secret name should be rejected"
        );
    }

    #[test]
    fn secret_pattern_with_extra_spaces() {
        // The pattern requires exact match: "${{ secrets." ... " }}"
        let patterns = extract_secret_patterns("${{  secrets.KEY  }}");
        assert!(
            patterns.is_empty(),
            "extra spaces should not match the pattern"
        );
    }

    #[test]
    fn secret_pattern_nested_braces() {
        // Nested braces produce unpredictable matches — just verify no panic
        let patterns = extract_secret_patterns("${{ secrets.${{ secrets.A }} }}");
        // The regex may match "A " or nothing depending on greedy/lazy — just check no panic
        let _ = patterns;
    }

    #[test]
    fn secret_pattern_numeric_name() {
        let patterns = extract_secret_patterns("${{ secrets.12345 }}");
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].2, "12345");
    }

    #[test]
    fn secret_pattern_single_char_name() {
        let patterns = extract_secret_patterns("${{ secrets.X }}");
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].2, "X");
    }

    // -- Key rotation simulation --

    #[test]
    fn key_rotation_old_data_decryptable_after_rotation() {
        let old_key = [11u8; 32];
        let new_key = [22u8; 32];

        // Encrypt multiple values with old key
        let enc1 = encrypt(b"secret-1", &old_key).unwrap();
        let enc2 = encrypt(b"secret-2", &old_key).unwrap();

        // After rotation, new_key is current, old_key is previous
        let dec1 = decrypt(&enc1, &new_key, Some(&old_key)).unwrap();
        let dec2 = decrypt(&enc2, &new_key, Some(&old_key)).unwrap();
        assert_eq!(dec1, b"secret-1");
        assert_eq!(dec2, b"secret-2");

        // New data encrypted with new key
        let enc3 = encrypt(b"secret-3", &new_key).unwrap();
        let dec3 = decrypt(&enc3, &new_key, Some(&old_key)).unwrap();
        assert_eq!(dec3, b"secret-3");
    }

    #[test]
    fn key_rotation_re_encrypt_with_new_key() {
        let old_key = [11u8; 32];
        let new_key = [22u8; 32];

        // Original encryption with old key
        let encrypted_old = encrypt(b"rotating-secret", &old_key).unwrap();

        // Decrypt with old key as fallback
        let plaintext = decrypt(&encrypted_old, &new_key, Some(&old_key)).unwrap();
        assert_eq!(plaintext, b"rotating-secret");

        // Re-encrypt with new key
        let encrypted_new = encrypt(&plaintext, &new_key).unwrap();

        // Now decryptable with new key only (no previous key needed)
        let decrypted = decrypt(&encrypted_new, &new_key, None).unwrap();
        assert_eq!(decrypted, b"rotating-secret");
    }

    // -- decrypt legacy vs versioned format edge cases --

    #[test]
    fn decrypt_versioned_with_version_byte_first() {
        // Data starts with 0x01 → treated as versioned, strip first byte
        let key = [42u8; 32];
        let encrypted = encrypt(b"versioned", &key).unwrap();
        assert_eq!(encrypted[0], ENCRYPTION_VERSION);
        let decrypted = decrypt(&encrypted, &key, None).unwrap();
        assert_eq!(decrypted, b"versioned");
    }

    #[test]
    fn decrypt_legacy_format_first_byte_not_version() {
        // Manually create a legacy blob without version prefix
        let key = [42u8; 32];
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        // Use a nonce that doesn't start with 0x01
        let nonce_bytes: [u8; 12] = [0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = cipher.encrypt(nonce, b"legacy-value".as_ref()).unwrap();
        let mut blob = Vec::new();
        blob.extend_from_slice(&nonce_bytes);
        blob.extend_from_slice(&ciphertext);
        assert_ne!(
            blob[0], ENCRYPTION_VERSION,
            "first byte should not be version"
        );
        let decrypted = decrypt(&blob, &key, None).unwrap();
        assert_eq!(decrypted, b"legacy-value");
    }

    #[test]
    fn decrypt_version_byte_plus_exactly_12_bytes_nonce_fails() {
        // Version (1) + nonce (12) = 13 bytes, no ciphertext → decryption should fail
        let key = [42u8; 32];
        let mut data = vec![ENCRYPTION_VERSION];
        data.extend_from_slice(&[0u8; 12]);
        let err = decrypt(&data, &key, None).unwrap_err();
        assert!(
            err.to_string().contains("decryption failed"),
            "nonce-only versioned data should fail decryption, got: {err}"
        );
    }

    // -- extract_secret_patterns with multiple patterns interspersed --

    #[test]
    fn secret_pattern_with_text_between() {
        let template = "host=${{ secrets.HOST }} port=${{ secrets.PORT }} db=${{ secrets.DB }}";
        let patterns = extract_secret_patterns(template);
        assert_eq!(patterns.len(), 3);
        assert_eq!(patterns[0].2, "HOST");
        assert_eq!(patterns[1].2, "PORT");
        assert_eq!(patterns[2].2, "DB");
    }

    #[test]
    fn secret_pattern_mixed_valid_and_invalid() {
        // First is valid, second has spaces (invalid), third is valid
        let template = "${{ secrets.A }} ${{ secrets.bad name }} ${{ secrets.B }}";
        let patterns = extract_secret_patterns(template);
        assert_eq!(patterns.len(), 2);
        assert_eq!(patterns[0].2, "A");
        assert_eq!(patterns[1].2, "B");
    }

    #[test]
    fn secret_pattern_only_prefix_no_closing() {
        let patterns = extract_secret_patterns("${{ secrets.OPEN_ENDED");
        assert!(patterns.is_empty());
    }

    // -- SecretMetadata serialization --

    #[test]
    fn secret_metadata_serializes() {
        let meta = SecretMetadata {
            id: Uuid::nil(),
            project_id: Some(Uuid::nil()),
            workspace_id: None,
            environment: Some("production".into()),
            name: "DB_URL".into(),
            scope: "pipeline".into(),
            version: 3,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let json = serde_json::to_value(&meta).unwrap();
        assert_eq!(json["name"], "DB_URL");
        assert_eq!(json["scope"], "pipeline");
        assert_eq!(json["version"], 3);
        assert_eq!(json["environment"], "production");
        assert!(json["workspace_id"].is_null());
    }

    #[test]
    fn secret_metadata_debug() {
        let meta = SecretMetadata {
            id: Uuid::nil(),
            project_id: None,
            workspace_id: None,
            environment: None,
            name: "test".into(),
            scope: "all".into(),
            version: 1,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let debug = format!("{meta:?}");
        assert!(debug.contains("SecretMetadata"));
        assert!(debug.contains("test"));
    }

    #[test]
    fn validate_master_key_valid() {
        let hex_key = "aa".repeat(32);
        assert!(validate_master_key(&hex_key).is_ok());
    }

    #[test]
    fn validate_master_key_too_short() {
        let err = validate_master_key("aabb").unwrap_err();
        assert!(err.contains("64 hex characters"));
    }

    #[test]
    fn validate_master_key_invalid_hex() {
        let bad = "zz".repeat(32);
        let err = validate_master_key(&bad).unwrap_err();
        assert!(err.contains("hex"));
    }

    #[test]
    fn validate_master_key_trims_whitespace() {
        let hex_key = format!("  {}  ", "aa".repeat(32));
        assert!(validate_master_key(&hex_key).is_ok());
    }
}
