use uuid::Uuid;
use webauthn_rs::prelude::*;

use crate::config::Config;
use crate::store::valkey;

const CHALLENGE_TTL_SECS: i64 = 120;

/// Initialize the `WebAuthn` relying party from config.
pub fn build_webauthn(config: &Config) -> anyhow::Result<Webauthn> {
    let rp_origin = Url::parse(&config.webauthn_rp_origin)
        .map_err(|e| anyhow::anyhow!("invalid WEBAUTHN_RP_ORIGIN: {e}"))?;
    let builder =
        WebauthnBuilder::new(&config.webauthn_rp_id, &rp_origin)?.rp_name(&config.webauthn_rp_name);
    Ok(builder.build()?)
}

// ---------------------------------------------------------------------------
// Registration ceremony
// ---------------------------------------------------------------------------

/// Begin passkey registration. Returns the challenge JSON for the browser and
/// stores the registration state in Valkey (120s TTL).
#[tracing::instrument(skip(webauthn, valkey_pool, existing_credentials), fields(%user_id), err)]
pub async fn begin_registration(
    webauthn: &Webauthn,
    valkey_pool: &fred::clients::Pool,
    user_id: Uuid,
    user_name: &str,
    display_name: &str,
    existing_credentials: Vec<CredentialID>,
) -> anyhow::Result<CreationChallengeResponse> {
    let exclude = existing_credentials;

    let (ccr, reg_state) =
        webauthn.start_passkey_registration(user_id, user_name, display_name, Some(exclude))?;

    // Store registration state in Valkey
    let state_json = serde_json::to_string(&reg_state)?;
    let key = format!("webauthn:reg:{user_id}");
    valkey::set_cached(valkey_pool, &key, &state_json, CHALLENGE_TTL_SECS).await?;

    Ok(ccr)
}

/// Complete passkey registration. Verifies the browser's response against
/// the stored challenge state. Returns the credential to store in DB.
#[tracing::instrument(skip(webauthn, valkey_pool, response), fields(%user_id), err)]
pub async fn finish_registration(
    webauthn: &Webauthn,
    valkey_pool: &fred::clients::Pool,
    user_id: Uuid,
    response: &RegisterPublicKeyCredential,
) -> anyhow::Result<Passkey> {
    let key = format!("webauthn:reg:{user_id}");
    let state_json: String = valkey::get_cached(valkey_pool, &key)
        .await
        .ok_or_else(|| anyhow::anyhow!("registration challenge expired or not found"))?;

    // Clean up state after retrieval
    let _ = valkey::invalidate(valkey_pool, &key).await;

    let reg_state: PasskeyRegistration = serde_json::from_str(&state_json)?;
    let passkey = webauthn.finish_passkey_registration(response, &reg_state)?;

    Ok(passkey)
}

// ---------------------------------------------------------------------------
// Authentication ceremony
// ---------------------------------------------------------------------------

/// Begin passkey authentication with discoverable credentials (usernameless).
/// Returns the challenge JSON and a challenge ID to send back.
#[tracing::instrument(skip(webauthn, valkey_pool), err)]
pub async fn begin_discoverable_authentication(
    webauthn: &Webauthn,
    valkey_pool: &fred::clients::Pool,
) -> anyhow::Result<(RequestChallengeResponse, String)> {
    let (rcr, auth_state) = webauthn.start_discoverable_authentication()?;

    let challenge_id = Uuid::new_v4().to_string();
    let state_json = serde_json::to_string(&auth_state)?;
    let key = format!("webauthn:auth:{challenge_id}");
    valkey::set_cached(valkey_pool, &key, &state_json, CHALLENGE_TTL_SECS).await?;

    Ok((rcr, challenge_id))
}

/// Complete passkey authentication (discoverable flow). Verifies the signed
/// challenge. Returns the user handle (UUID) and updated authentication result.
#[tracing::instrument(skip(webauthn, valkey_pool, response), err)]
pub async fn finish_discoverable_authentication(
    webauthn: &Webauthn,
    valkey_pool: &fred::clients::Pool,
    challenge_id: &str,
    response: &PublicKeyCredential,
    credentials: &[DiscoverableKey],
) -> anyhow::Result<(DiscoverableAuthentication, AuthenticationResult)> {
    let key = format!("webauthn:auth:{challenge_id}");
    let state_json: String = valkey::get_cached(valkey_pool, &key)
        .await
        .ok_or_else(|| anyhow::anyhow!("authentication challenge expired or not found"))?;

    let _ = valkey::invalidate(valkey_pool, &key).await;

    let auth_state: DiscoverableAuthentication = serde_json::from_str(&state_json)?;

    let auth_result =
        webauthn.finish_discoverable_authentication(response, auth_state.clone(), credentials)?;

    Ok((auth_state, auth_result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_webauthn_valid_config() {
        let config = Config {
            webauthn_rp_id: "localhost".into(),
            webauthn_rp_origin: "http://localhost:8080".into(),
            webauthn_rp_name: "Test Platform".into(),
            ..test_config()
        };
        assert!(build_webauthn(&config).is_ok());
    }

    #[test]
    fn build_webauthn_invalid_origin() {
        let config = Config {
            webauthn_rp_id: "localhost".into(),
            webauthn_rp_origin: "not-a-url".into(),
            webauthn_rp_name: "Test".into(),
            ..test_config()
        };
        assert!(build_webauthn(&config).is_err());
    }

    fn test_config() -> Config {
        Config::test_default()
    }
}
