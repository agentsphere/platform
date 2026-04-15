// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! `WebAuthn` relying-party builder.

use webauthn_rs::prelude::*;

/// Initialize the `WebAuthn` relying party from explicit parameters.
pub fn build_webauthn(rp_id: &str, rp_origin: &str, rp_name: &str) -> anyhow::Result<Webauthn> {
    let rp_origin =
        Url::parse(rp_origin).map_err(|e| anyhow::anyhow!("invalid WEBAUTHN_RP_ORIGIN: {e}"))?;
    let builder = WebauthnBuilder::new(rp_id, &rp_origin)?.rp_name(rp_name);
    Ok(builder.build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_webauthn_valid() {
        assert!(build_webauthn("localhost", "http://localhost:8080", "Test Platform").is_ok());
    }

    #[test]
    fn build_webauthn_invalid_origin() {
        assert!(build_webauthn("localhost", "not-a-url", "Test").is_err());
    }
}
