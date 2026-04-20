// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Registry auth extractors for `PlatformState`.
//!
//! Implements `FromRequestParts` to extract a `RegistryUser` from Bearer token
//! or Basic auth headers, matching the OCI Distribution Spec auth flow.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use platform_registry::RegistryUser;

use crate::state::PlatformState;

/// OCI-compliant 401 rejection with `Www-Authenticate` header.
pub struct RegistryAuthRejection;

impl IntoResponse for RegistryAuthRejection {
    fn into_response(self) -> Response {
        let body = serde_json::json!({
            "errors": [{
                "code": "UNAUTHORIZED",
                "message": "authentication required",
                "detail": null
            }]
        });
        let mut resp = (StatusCode::UNAUTHORIZED, axum::Json(body)).into_response();
        resp.headers_mut().insert(
            "www-authenticate",
            HeaderValue::from_static(r#"Basic realm="platform registry""#),
        );
        resp
    }
}

/// Optional registry user — returns `None` for anonymous requests (public pulls).
pub struct OptionalRegistryUser(pub Option<RegistryUser>);

impl FromRequestParts<PlatformState> for RegistryUser {
    type Rejection = RegistryAuthRejection;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &PlatformState,
    ) -> Result<Self, Self::Rejection> {
        let auth_header = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(String::from);

        let Some(auth) = auth_header else {
            return Err(RegistryAuthRejection);
        };

        if let Some(token) = auth.strip_prefix("Bearer ") {
            return lookup_bearer_token(&state.pool, token.trim())
                .await
                .ok_or(RegistryAuthRejection);
        }

        if let Some(encoded) = auth.strip_prefix("Basic ") {
            return lookup_basic_auth(&state.pool, encoded.trim())
                .await
                .ok_or(RegistryAuthRejection);
        }

        Err(RegistryAuthRejection)
    }
}

impl FromRequestParts<PlatformState> for OptionalRegistryUser {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &PlatformState,
    ) -> Result<Self, Self::Rejection> {
        Ok(OptionalRegistryUser(
            RegistryUser::from_request_parts(parts, state).await.ok(),
        ))
    }
}

/// Look up a Bearer API token and return the authenticated user.
async fn lookup_bearer_token(pool: &sqlx::PgPool, raw_token: &str) -> Option<RegistryUser> {
    let token_hash = platform_auth::token::hash_token(raw_token);

    let row = sqlx::query!(
        r#"SELECT u.id AS "user_id!", u.name AS "user_name!",
                  t.project_id, t.scope_workspace_id, t.scopes,
                  t.registry_tag_pattern
           FROM api_tokens t
           JOIN users u ON t.user_id = u.id
           WHERE t.token_hash = $1
             AND u.is_active = true
             AND (t.expires_at IS NULL OR t.expires_at > now())"#,
        &token_hash,
    )
    .fetch_optional(pool)
    .await
    .ok()??;

    // Fire-and-forget update last_used_at
    let pool2 = pool.clone();
    let hash = token_hash.clone();
    tokio::spawn(async move {
        let _ = sqlx::query!(
            "UPDATE api_tokens SET last_used_at = now() WHERE token_hash = $1",
            hash,
        )
        .execute(&pool2)
        .await;
    });

    Some(RegistryUser {
        user_id: row.user_id,
        user_name: row.user_name,
        boundary_project_id: row.project_id,
        boundary_workspace_id: row.scope_workspace_id,
        registry_tag_pattern: row.registry_tag_pattern,
        token_scopes: Some(row.scopes),
    })
}

/// Decode Basic auth and look up by username + password (treated as API token).
async fn lookup_basic_auth(pool: &sqlx::PgPool, encoded: &str) -> Option<RegistryUser> {
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    let decoded_str = String::from_utf8(decoded).ok()?;
    let (username, password) = decoded_str.split_once(':')?;

    let token_hash = platform_auth::token::hash_token(password);

    // Try username-scoped lookup first
    let row = sqlx::query!(
        r#"SELECT u.id AS "user_id!", u.name AS "user_name!",
                  t.project_id, t.scope_workspace_id, t.scopes,
                  t.registry_tag_pattern
           FROM api_tokens t
           JOIN users u ON t.user_id = u.id
           WHERE t.token_hash = $1
             AND u.name = $2
             AND u.is_active = true
             AND (t.expires_at IS NULL OR t.expires_at > now())"#,
        &token_hash,
        username,
    )
    .fetch_optional(pool)
    .await
    .ok()?;

    if let Some(r) = row {
        return Some(RegistryUser {
            user_id: r.user_id,
            user_name: r.user_name,
            boundary_project_id: r.project_id,
            boundary_workspace_id: r.scope_workspace_id,
            registry_tag_pattern: r.registry_tag_pattern,
            token_scopes: Some(r.scopes),
        });
    }

    // Fallback: token-only auth (username might be the token itself)
    lookup_bearer_token(pool, password).await
}
