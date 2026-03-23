use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use super::engine;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Metadata returned when listing configs (no decrypted secrets).
#[derive(Debug, Serialize)]
pub struct ProviderConfigMeta {
    pub id: Uuid,
    pub provider_type: String,
    pub label: String,
    pub model: Option<String>,
    pub validation_status: String,
    pub last_validated_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Decrypted config for internal use (never sent directly to API).
#[allow(dead_code)]
pub struct ProviderConfig {
    pub id: Uuid,
    pub provider_type: String,
    pub label: String,
    pub env_vars: HashMap<String, String>,
    pub model: Option<String>,
    pub validation_status: String,
}

/// The JSON structure encrypted inside `encrypted_config`.
#[derive(Debug, Serialize, Deserialize)]
struct EncryptedBlob {
    env_vars: HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

/// Resolved auth credentials from the user's active LLM provider selection.
#[allow(dead_code)]
pub struct ResolvedProvider {
    pub oauth_token: Option<String>,
    pub anthropic_api_key: Option<String>,
    pub extra_env: Vec<(String, String)>,
    pub model: Option<String>,
}

// Custom Debug to avoid logging sensitive env var values
impl std::fmt::Debug for ResolvedProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedProvider")
            .field("oauth_token", &self.oauth_token.as_ref().map(|_| "****"))
            .field(
                "anthropic_api_key",
                &self.anthropic_api_key.as_ref().map(|_| "****"),
            )
            .field("extra_env_count", &self.extra_env.len())
            .field("model", &self.model)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Required env var keys per provider type.
pub fn required_env_vars(provider_type: &str) -> &'static [&'static str] {
    match provider_type {
        "bedrock" => &["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"],
        "vertex" => &["ANTHROPIC_VERTEX_PROJECT_ID"],
        "azure_foundry" => &["ANTHROPIC_FOUNDRY_API_KEY"],
        "custom_endpoint" => &["ANTHROPIC_BASE_URL", "ANTHROPIC_API_KEY"],
        _ => &[],
    }
}

/// Validate provider type is in the allowed set.
pub fn validate_provider_type(pt: &str) -> Result<(), String> {
    match pt {
        "bedrock" | "vertex" | "azure_foundry" | "custom_endpoint" => Ok(()),
        _ => Err(format!("invalid provider_type: {pt}")),
    }
}

/// Validate that all required env vars are present and values are within bounds.
pub fn validate_env_vars<S: std::hash::BuildHasher>(
    provider_type: &str,
    env_vars: &HashMap<String, String, S>,
) -> Result<(), String> {
    for key in required_env_vars(provider_type) {
        if !env_vars.contains_key(*key) {
            return Err(format!("missing required env var: {key}"));
        }
    }
    for (key, value) in env_vars {
        if key.is_empty() || key.len() > 255 {
            return Err(format!("env var key must be 1-255 chars: {key}"));
        }
        if value.is_empty() || value.len() > 2048 {
            return Err(format!("env var value must be 1-2048 chars for key: {key}"));
        }
        // Only allow safe env var name characters
        if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!(
                "env var key must be alphanumeric/underscore: {key}"
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// CRUD
// ---------------------------------------------------------------------------

/// Create a new custom LLM provider config. Returns the config ID.
#[tracing::instrument(skip(pool, master_key, env_vars), fields(%user_id, %provider_type), err)]
pub async fn create_config<S: std::hash::BuildHasher>(
    pool: &PgPool,
    master_key: &[u8; 32],
    user_id: Uuid,
    provider_type: &str,
    label: &str,
    env_vars: &HashMap<String, String, S>,
    model: Option<&str>,
) -> anyhow::Result<Uuid> {
    validate_provider_type(provider_type).map_err(|e| anyhow::anyhow!("{e}"))?;
    validate_env_vars(provider_type, env_vars).map_err(|e| anyhow::anyhow!("{e}"))?;

    let blob = EncryptedBlob {
        env_vars: env_vars
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        model: model.map(String::from),
    };
    let plaintext = serde_json::to_vec(&blob)?;
    let encrypted = engine::encrypt(&plaintext, master_key)?;

    let id = sqlx::query_scalar!(
        r#"
        INSERT INTO llm_provider_configs (user_id, provider_type, label, encrypted_config, model)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id
        "#,
        user_id,
        provider_type,
        label,
        encrypted,
        model,
    )
    .fetch_one(pool)
    .await?;

    Ok(id)
}

/// Update an existing config. Resets `validation_status` to 'untested'.
#[tracing::instrument(skip(pool, master_key, env_vars), fields(%config_id, %user_id), err)]
pub async fn update_config<S: std::hash::BuildHasher>(
    pool: &PgPool,
    master_key: &[u8; 32],
    config_id: Uuid,
    user_id: Uuid,
    env_vars: &HashMap<String, String, S>,
    model: Option<&str>,
    label: &str,
) -> anyhow::Result<bool> {
    // Fetch existing to validate provider_type
    let row = sqlx::query!(
        "SELECT provider_type FROM llm_provider_configs WHERE id = $1 AND user_id = $2",
        config_id,
        user_id,
    )
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Ok(false);
    };

    validate_env_vars(&row.provider_type, env_vars).map_err(|e| anyhow::anyhow!("{e}"))?;

    let blob = EncryptedBlob {
        env_vars: env_vars
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        model: model.map(String::from),
    };
    let plaintext = serde_json::to_vec(&blob)?;
    let encrypted = engine::encrypt(&plaintext, master_key)?;

    let result = sqlx::query!(
        r#"
        UPDATE llm_provider_configs
        SET encrypted_config = $3, model = $4, label = $5,
            validation_status = 'untested', last_validated_at = NULL, updated_at = now()
        WHERE id = $1 AND user_id = $2
        "#,
        config_id,
        user_id,
        encrypted,
        model,
        label,
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}

/// Decrypt and return a single config. Returns None if not found or wrong user.
#[tracing::instrument(skip(pool, master_key), fields(%config_id, %user_id), err)]
pub async fn get_config(
    pool: &PgPool,
    master_key: &[u8; 32],
    config_id: Uuid,
    user_id: Uuid,
) -> anyhow::Result<Option<ProviderConfig>> {
    let row = sqlx::query!(
        r#"
        SELECT id, provider_type, label, encrypted_config, model, validation_status
        FROM llm_provider_configs
        WHERE id = $1 AND user_id = $2
        "#,
        config_id,
        user_id,
    )
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };

    let plaintext = engine::decrypt(&row.encrypted_config, master_key)?;
    let blob: EncryptedBlob = serde_json::from_slice(&plaintext)
        .map_err(|e| anyhow::anyhow!("failed to parse encrypted config: {e}"))?;

    Ok(Some(ProviderConfig {
        id: row.id,
        provider_type: row.provider_type,
        label: row.label,
        env_vars: blob.env_vars,
        model: row.model,
        validation_status: row.validation_status,
    }))
}

/// List all configs for a user (metadata only, no decryption).
pub async fn list_configs(pool: &PgPool, user_id: Uuid) -> anyhow::Result<Vec<ProviderConfigMeta>> {
    let rows = sqlx::query!(
        r#"
        SELECT id, provider_type, label, model, validation_status,
               last_validated_at, created_at, updated_at
        FROM llm_provider_configs
        WHERE user_id = $1
        ORDER BY created_at
        "#,
        user_id,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| ProviderConfigMeta {
            id: r.id,
            provider_type: r.provider_type,
            label: r.label,
            model: r.model,
            validation_status: r.validation_status,
            last_validated_at: r.last_validated_at,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
        .collect())
}

/// Delete a config. If it was the user's active provider, revert to 'auto'.
#[tracing::instrument(skip(pool), fields(%config_id, %user_id), err)]
pub async fn delete_config(pool: &PgPool, config_id: Uuid, user_id: Uuid) -> anyhow::Result<bool> {
    let mut tx = pool.begin().await?;

    let result = sqlx::query!(
        "DELETE FROM llm_provider_configs WHERE id = $1 AND user_id = $2",
        config_id,
        user_id
    )
    .execute(&mut *tx)
    .await?;

    if result.rows_affected() > 0 {
        // Revert active_llm_provider to 'auto' if this config was active
        let active_value = format!("custom:{config_id}");
        sqlx::query!(
            "UPDATE users SET active_llm_provider = 'auto' WHERE id = $1 AND active_llm_provider = $2",
            user_id,
            active_value,
        )
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(result.rows_affected() > 0)
}

/// Update validation status after running tests.
#[tracing::instrument(skip(pool), fields(%config_id, %status), err)]
pub async fn update_validation_status(
    pool: &PgPool,
    config_id: Uuid,
    status: &str,
) -> anyhow::Result<()> {
    sqlx::query!(
        r#"
        UPDATE llm_provider_configs
        SET validation_status = $2, last_validated_at = now(), updated_at = now()
        WHERE id = $1
        "#,
        config_id,
        status,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Read the user's `active_llm_provider` column.
pub async fn get_active_provider(pool: &PgPool, user_id: Uuid) -> anyhow::Result<String> {
    let value = sqlx::query_scalar!(
        r#"SELECT active_llm_provider as "active_llm_provider!" FROM users WHERE id = $1"#,
        user_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(value)
}

/// Set the user's `active_llm_provider` column.
pub async fn set_active_provider(pool: &PgPool, user_id: Uuid, value: &str) -> anyhow::Result<()> {
    sqlx::query!(
        "UPDATE users SET active_llm_provider = $2 WHERE id = $1",
        user_id,
        value,
    )
    .execute(pool)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_provider_type_valid() {
        assert!(validate_provider_type("bedrock").is_ok());
        assert!(validate_provider_type("vertex").is_ok());
        assert!(validate_provider_type("azure_foundry").is_ok());
        assert!(validate_provider_type("custom_endpoint").is_ok());
    }

    #[test]
    fn validate_provider_type_invalid() {
        assert!(validate_provider_type("openai").is_err());
        assert!(validate_provider_type("").is_err());
    }

    #[test]
    fn required_env_vars_bedrock() {
        let keys = required_env_vars("bedrock");
        assert!(keys.contains(&"AWS_ACCESS_KEY_ID"));
        assert!(keys.contains(&"AWS_SECRET_ACCESS_KEY"));
    }

    #[test]
    fn required_env_vars_vertex() {
        let keys = required_env_vars("vertex");
        assert!(keys.contains(&"ANTHROPIC_VERTEX_PROJECT_ID"));
    }

    #[test]
    fn required_env_vars_azure() {
        let keys = required_env_vars("azure_foundry");
        assert!(keys.contains(&"ANTHROPIC_FOUNDRY_API_KEY"));
    }

    #[test]
    fn required_env_vars_custom() {
        let keys = required_env_vars("custom_endpoint");
        assert!(keys.contains(&"ANTHROPIC_BASE_URL"));
        assert!(keys.contains(&"ANTHROPIC_API_KEY"));
    }

    #[test]
    fn required_env_vars_unknown() {
        assert!(required_env_vars("unknown").is_empty());
    }

    #[test]
    fn validate_env_vars_missing_required() {
        let vars = HashMap::from([("AWS_ACCESS_KEY_ID".into(), "key".into())]);
        let result = validate_env_vars("bedrock", &vars);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("AWS_SECRET_ACCESS_KEY"));
    }

    #[test]
    fn validate_env_vars_valid_bedrock() {
        let vars = HashMap::from([
            ("AWS_ACCESS_KEY_ID".into(), "AKIA123".into()),
            ("AWS_SECRET_ACCESS_KEY".into(), "secret123".into()),
        ]);
        assert!(validate_env_vars("bedrock", &vars).is_ok());
    }

    #[test]
    fn validate_env_vars_empty_value() {
        let vars = HashMap::from([
            ("AWS_ACCESS_KEY_ID".into(), "".into()),
            ("AWS_SECRET_ACCESS_KEY".into(), "secret".into()),
        ]);
        assert!(validate_env_vars("bedrock", &vars).is_err());
    }

    #[test]
    fn validate_env_vars_invalid_key_chars() {
        let vars = HashMap::from([
            ("AWS_ACCESS_KEY_ID".into(), "key".into()),
            ("AWS_SECRET_ACCESS_KEY".into(), "secret".into()),
            ("bad-key".into(), "value".into()),
        ]);
        assert!(validate_env_vars("bedrock", &vars).is_err());
    }

    #[test]
    fn validate_env_vars_value_too_long() {
        let vars = HashMap::from([
            ("AWS_ACCESS_KEY_ID".into(), "key".into()),
            ("AWS_SECRET_ACCESS_KEY".into(), "x".repeat(2049)),
        ]);
        assert!(validate_env_vars("bedrock", &vars).is_err());
    }

    #[test]
    fn encrypted_blob_roundtrip() {
        let blob = EncryptedBlob {
            env_vars: HashMap::from([("KEY".into(), "VALUE".into())]),
            model: Some("claude-3".into()),
        };
        let json = serde_json::to_vec(&blob).unwrap();
        let back: EncryptedBlob = serde_json::from_slice(&json).unwrap();
        assert_eq!(back.env_vars["KEY"], "VALUE");
        assert_eq!(back.model.as_deref(), Some("claude-3"));
    }

    #[test]
    fn encrypted_blob_no_model() {
        let blob = EncryptedBlob {
            env_vars: HashMap::from([("KEY".into(), "VALUE".into())]),
            model: None,
        };
        let json = serde_json::to_string(&blob).unwrap();
        assert!(!json.contains("model"));
    }

    #[test]
    fn resolved_provider_debug_masks_secrets() {
        let rp = ResolvedProvider {
            oauth_token: Some("secret-token".into()),
            anthropic_api_key: Some("sk-ant-123".into()),
            extra_env: vec![("KEY".into(), "VALUE".into())],
            model: Some("claude-3".into()),
        };
        let debug = format!("{rp:?}");
        assert!(!debug.contains("secret-token"));
        assert!(!debug.contains("sk-ant-123"));
        assert!(debug.contains("****"));
        assert!(debug.contains("extra_env_count: 1"));
    }

    #[test]
    fn validate_env_vars_custom_endpoint_all_required() {
        let vars = HashMap::from([
            ("ANTHROPIC_BASE_URL".into(), "https://example.com".into()),
            ("ANTHROPIC_API_KEY".into(), "sk-123".into()),
        ]);
        assert!(validate_env_vars("custom_endpoint", &vars).is_ok());
    }

    #[test]
    fn validate_env_vars_with_optional_extra() {
        let vars = HashMap::from([
            ("AWS_ACCESS_KEY_ID".into(), "key".into()),
            ("AWS_SECRET_ACCESS_KEY".into(), "secret".into()),
            ("AWS_REGION".into(), "us-east-1".into()),
            ("AWS_SESSION_TOKEN".into(), "token".into()),
        ]);
        assert!(validate_env_vars("bedrock", &vars).is_ok());
    }
}
