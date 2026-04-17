// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! `WebAuthn` passkey ceremony functions.
//!
//! Stores challenge state in Valkey with a 120-second TTL so that the begin/finish
//! round-trip completes within a reasonable window.

use fred::interfaces::KeysInterface;
use uuid::Uuid;
use webauthn_rs::prelude::*;

const CHALLENGE_TTL_SECS: i64 = 120;

/// Start a passkey registration ceremony.
///
/// Returns a `CreationChallengeResponse` to send to the browser.
/// The registration state is stored in Valkey keyed by `passkey:reg:{user_id}`.
pub async fn begin_registration(
    webauthn: &Webauthn,
    valkey: &fred::clients::Pool,
    user_id: Uuid,
    user_name: &str,
    display_name: &str,
    exclude_credentials: Vec<CredentialID>,
) -> anyhow::Result<CreationChallengeResponse> {
    let (ccr, reg_state) = webauthn.start_passkey_registration(
        user_id,
        user_name,
        display_name,
        Some(exclude_credentials),
    )?;

    let state_json = serde_json::to_string(&reg_state)?;
    let key = format!("passkey:reg:{user_id}");
    valkey
        .set::<(), _, _>(
            &key,
            state_json.as_str(),
            Some(fred::types::Expiration::EX(CHALLENGE_TTL_SECS)),
            None,
            false,
        )
        .await?;

    Ok(ccr)
}

/// Complete a passkey registration ceremony.
///
/// Retrieves the stored state from Valkey and verifies the credential.
pub async fn finish_registration(
    webauthn: &Webauthn,
    valkey: &fred::clients::Pool,
    user_id: Uuid,
    credential: &RegisterPublicKeyCredential,
) -> anyhow::Result<Passkey> {
    let key = format!("passkey:reg:{user_id}");
    let state_json: Option<String> = valkey.get(&key).await?;
    let state_json = state_json.ok_or_else(|| anyhow::anyhow!("registration challenge expired"))?;
    let _: () = valkey.del(&key).await?;

    let reg_state: PasskeyRegistration = serde_json::from_str(&state_json)?;
    let pk = webauthn.finish_passkey_registration(credential, &reg_state)?;
    Ok(pk)
}

/// Start a discoverable authentication ceremony (passwordless login).
///
/// Returns the challenge response and a unique challenge ID.
pub async fn begin_discoverable_authentication(
    webauthn: &Webauthn,
    valkey: &fred::clients::Pool,
) -> anyhow::Result<(RequestChallengeResponse, String)> {
    let (rcr, auth_state) = webauthn.start_discoverable_authentication()?;

    let challenge_id = Uuid::new_v4().to_string();
    let state_json = serde_json::to_string(&auth_state)?;
    let key = format!("passkey:auth:{challenge_id}");
    valkey
        .set::<(), _, _>(
            &key,
            state_json.as_str(),
            Some(fred::types::Expiration::EX(CHALLENGE_TTL_SECS)),
            None,
            false,
        )
        .await?;

    Ok((rcr, challenge_id))
}

/// Complete a discoverable authentication ceremony.
///
/// Returns the `AuthenticationResult` for clone detection + counter update.
pub async fn finish_discoverable_authentication(
    webauthn: &Webauthn,
    valkey: &fred::clients::Pool,
    challenge_id: &str,
    credential: &PublicKeyCredential,
    discoverable_keys: &[DiscoverableKey],
) -> anyhow::Result<AuthenticationResult> {
    let key = format!("passkey:auth:{challenge_id}");
    let state_json: Option<String> = valkey.get(&key).await?;
    let state_json =
        state_json.ok_or_else(|| anyhow::anyhow!("authentication challenge expired"))?;
    let _: () = valkey.del(&key).await?;

    let auth_state: DiscoverableAuthentication = serde_json::from_str(&state_json)?;
    let auth_result =
        webauthn.finish_discoverable_authentication(credential, auth_state, discoverable_keys)?;

    Ok(auth_result)
}
