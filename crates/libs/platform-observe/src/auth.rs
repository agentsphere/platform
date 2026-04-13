// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Axum `FromRequestParts` extractor for the observe crate.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use platform_types::{ApiError, AuthUser};

use crate::state::ObserveState;

impl FromRequestParts<ObserveState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &ObserveState,
    ) -> Result<Self, Self::Rejection> {
        let ip = platform_auth::extract_ip(parts, state.config.trust_proxy, &[]);

        // Try Bearer token first
        if let Some(raw) = platform_auth::extract_bearer_token(parts)
            && let Some(lookup) = platform_auth::lookup_api_token(&state.pool, raw).await?
        {
            if !lookup.is_active {
                return Err(ApiError::Unauthorized);
            }
            let user_type = platform_auth::auth_user::parse_user_type(&lookup.user_type)?;
            return Ok(AuthUser {
                user_id: lookup.user_id,
                user_name: lookup.user_name,
                user_type,
                ip_addr: ip,
                token_scopes: Some(lookup.scopes),
                boundary_project_id: lookup.scope_project_id,
                boundary_workspace_id: lookup.scope_workspace_id,
                session_id: None,
                session_token_hash: None,
            });
        }

        // Try session cookie
        if let Some(raw) = platform_auth::extract_session_cookie(parts)
            && let Some(lookup) = platform_auth::lookup_session(&state.pool, raw).await?
        {
            if !lookup.is_active {
                return Err(ApiError::Unauthorized);
            }
            let user_type = platform_auth::auth_user::parse_user_type(&lookup.user_type)?;
            let hash = platform_auth::token::hash_token(raw);
            return Ok(AuthUser {
                user_id: lookup.user_id,
                user_name: lookup.user_name,
                user_type,
                ip_addr: ip,
                token_scopes: None,
                boundary_project_id: None,
                boundary_workspace_id: None,
                session_id: None,
                session_token_hash: Some(hash),
            });
        }

        Err(ApiError::Unauthorized)
    }
}
