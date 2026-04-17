// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

/// Hash a setup token using SHA-256 (same algorithm as API token hashing).
pub fn hash_setup_token(token: &str) -> String {
    platform_auth::hash_token(token)
}
