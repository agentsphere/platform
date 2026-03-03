//! Shared helper for creating K8s `imagePullSecrets`-compatible Secrets.
//!
//! Used by both the pipeline executor (for kaniko steps) and the agent service
//! (for agent pods) to authenticate with the platform's built-in registry.

use std::collections::BTreeMap;

use base64::Engine;
use k8s_openapi::api::core::v1::Secret;
use kube::api::{Api, PostParams};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::token;

/// Result of creating a registry pull secret.
#[allow(dead_code)]
pub struct PullSecretResult {
    /// K8s Secret name (to reference in `imagePullSecrets`).
    pub secret_name: String,
    /// Token hash (for DB cleanup of the short-lived API token).
    pub token_hash: String,
}

/// Create a short-lived API token and a K8s `dockerconfigjson` Secret so that
/// pods can authenticate image pulls from the platform registry.
///
/// The Secret is created in the given namespace with a label for identification.
/// The API token expires after 1 hour.
#[tracing::instrument(skip(pool, kube), fields(%user_id, %namespace, %label_value), err)]
pub async fn create_pull_secret(
    pool: &PgPool,
    kube: &kube::Client,
    registry_url: &str,
    user_id: Uuid,
    namespace: &str,
    label_key: &str,
    label_value: &str,
) -> anyhow::Result<PullSecretResult> {
    // Create a short-lived API token (1 hour)
    let (raw_token, token_hash) = token::generate_api_token();

    sqlx::query(
        "INSERT INTO api_tokens (id, user_id, name, token_hash, expires_at)
         VALUES ($1, $2, $3, $4, now() + interval '1 hour')",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(format!("registry-pull-{label_value}"))
    .bind(&token_hash)
    .execute(pool)
    .await?;

    // Look up the username for Docker config
    let user_name: String = sqlx::query_scalar("SELECT name FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| anyhow::anyhow!("user not found: {user_id}"))?;

    let docker_config = build_docker_config(registry_url, &user_name, &raw_token);
    let secret_name = build_secret_name(label_value);

    let secret = Secret {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(secret_name.clone()),
            labels: Some(BTreeMap::from([(
                label_key.to_string(),
                label_value.to_string(),
            )])),
            ..Default::default()
        },
        type_: Some("kubernetes.io/dockerconfigjson".into()),
        string_data: Some(BTreeMap::from([(
            ".dockerconfigjson".into(),
            docker_config.to_string(),
        )])),
        ..Default::default()
    };

    let secrets: Api<Secret> = Api::namespaced(kube.clone(), namespace);
    secrets.create(&PostParams::default(), &secret).await?;

    tracing::debug!(%secret_name, "created registry pull secret");
    Ok(PullSecretResult {
        secret_name,
        token_hash,
    })
}

/// Build Docker config JSON for registry authentication.
fn build_docker_config(registry_url: &str, user_name: &str, raw_token: &str) -> serde_json::Value {
    let basic_auth =
        base64::engine::general_purpose::STANDARD.encode(format!("{user_name}:{raw_token}"));
    serde_json::json!({
        "auths": {
            registry_url: {
                "auth": basic_auth
            }
        }
    })
}

/// Build a K8s-safe secret name from a label value (truncated to 8 chars).
fn build_secret_name(label_value: &str) -> String {
    let short_label = label_value.get(..8).unwrap_or(label_value);
    format!("regpull-{short_label}")
}

/// Clean up a registry pull secret and its associated API token.
#[allow(dead_code)]
pub async fn cleanup_pull_secret(
    pool: &PgPool,
    kube: &kube::Client,
    secret_name: &str,
    token_hash: &str,
    namespace: &str,
) {
    let secrets: Api<Secret> = Api::namespaced(kube.clone(), namespace);
    if let Err(e) = secrets
        .delete(secret_name, &kube::api::DeleteParams::default())
        .await
    {
        tracing::warn!(error = %e, %secret_name, "failed to delete registry pull secret");
    }

    if let Err(e) = sqlx::query("DELETE FROM api_tokens WHERE token_hash = $1")
        .bind(token_hash)
        .execute(pool)
        .await
    {
        tracing::warn!(error = %e, "failed to delete registry pull token");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_config_structure() {
        let config = build_docker_config("registry.example.com:5000", "alice", "tok_abc");
        let auths = config["auths"].as_object().unwrap();
        assert!(auths.contains_key("registry.example.com:5000"));
        let auth_entry = &auths["registry.example.com:5000"]["auth"];
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(auth_entry.as_str().unwrap())
            .unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "alice:tok_abc");
    }

    #[test]
    fn docker_config_different_registry() {
        let config = build_docker_config("host.docker.internal:8080", "bob", "tok_xyz");
        let auths = config["auths"].as_object().unwrap();
        assert!(auths.contains_key("host.docker.internal:8080"));
    }

    #[test]
    fn secret_name_uuid_label() {
        let name = build_secret_name("abcd1234-5678-9abc-def0");
        assert_eq!(name, "regpull-abcd1234");
    }

    #[test]
    fn secret_name_short_label() {
        let name = build_secret_name("abc");
        assert_eq!(name, "regpull-abc");
    }

    #[test]
    fn secret_name_exact_8() {
        let name = build_secret_name("12345678");
        assert_eq!(name, "regpull-12345678");
    }

    #[test]
    fn secret_name_empty_label() {
        let name = build_secret_name("");
        assert_eq!(name, "regpull-");
    }
}
