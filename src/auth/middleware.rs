use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::token;
use crate::error::ApiError;
use crate::store::AppState;

/// Authenticated user extracted from request.
/// Set as request extension by the auth middleware.
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub user_id: Uuid,
    pub user_name: String,
    pub ip_addr: Option<String>,
}

/// Row returned when looking up an API token or session.
struct AuthLookup {
    user_id: Uuid,
    user_name: String,
    is_active: bool,
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let ip_addr = extract_ip(parts);

        // Try Bearer token first
        if let Some(raw_token) = extract_bearer_token(parts)
            && let Some(user) = lookup_api_token(&state.pool, &raw_token).await?
        {
            if !user.is_active {
                return Err(ApiError::Unauthorized);
            }
            return Ok(Self {
                user_id: user.user_id,
                user_name: user.user_name,
                ip_addr,
            });
        }

        // Try session cookie
        if let Some(session_token) = extract_session_cookie(parts)
            && let Some(user) = lookup_session(&state.pool, &session_token).await?
        {
            if !user.is_active {
                return Err(ApiError::Unauthorized);
            }
            return Ok(Self {
                user_id: user.user_id,
                user_name: user.user_name,
                ip_addr,
            });
        }

        Err(ApiError::Unauthorized)
    }
}

/// Optional auth — returns `None` for unauthenticated requests instead of 401.
#[derive(Debug, Clone)]
#[allow(dead_code)] // used by public endpoints in later modules
pub struct OptionalAuthUser(pub Option<AuthUser>);

impl FromRequestParts<AppState> for OptionalAuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        match AuthUser::from_request_parts(parts, state).await {
            Ok(user) => Ok(Self(Some(user))),
            Err(ApiError::Unauthorized) => Ok(Self(None)),
            Err(e) => Err(e),
        }
    }
}

fn extract_bearer_token(parts: &Parts) -> Option<String> {
    let value = parts.headers.get(AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ")?;
    if token.is_empty() {
        return None;
    }
    Some(token.to_owned())
}

fn extract_session_cookie(parts: &Parts) -> Option<String> {
    let cookies = parts
        .headers
        .get(axum::http::header::COOKIE)?
        .to_str()
        .ok()?;
    for cookie in cookies.split(';') {
        let cookie = cookie.trim();
        if let Some(value) = cookie.strip_prefix("session=")
            && !value.is_empty()
        {
            return Some(value.to_owned());
        }
    }
    None
}

fn extract_ip(parts: &Parts) -> Option<String> {
    // Try x-forwarded-for first (proxied requests)
    if let Some(forwarded) = parts.headers.get("x-forwarded-for")
        && let Ok(val) = forwarded.to_str()
        && let Some(first_ip) = val.split(',').next()
    {
        return Some(first_ip.trim().to_owned());
    }
    // Fall back to ConnectInfo if available
    parts
        .extensions
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip().to_string())
}

/// Look up an API token by its raw value. Updates `last_used_at` on success.
async fn lookup_api_token(pool: &PgPool, raw_token: &str) -> Result<Option<AuthLookup>, ApiError> {
    let hash = token::hash_token(raw_token);

    let row = sqlx::query_as!(
        AuthLookup,
        r#"
        SELECT u.id as "user_id!", u.name as "user_name!", u.is_active as "is_active!"
        FROM api_tokens t
        JOIN users u ON u.id = t.user_id
        WHERE t.token_hash = $1
          AND (t.expires_at IS NULL OR t.expires_at > now())
        "#,
        hash,
    )
    .fetch_optional(pool)
    .await?;

    if row.is_some() {
        // Update last_used_at (fire-and-forget, non-blocking)
        let pool = pool.clone();
        let hash = hash.clone();
        tokio::spawn(async move {
            let _ = sqlx::query!(
                "UPDATE api_tokens SET last_used_at = now() WHERE token_hash = $1",
                hash,
            )
            .execute(&pool)
            .await;
        });
    }

    Ok(row)
}

/// Look up a session by its raw cookie value.
async fn lookup_session(pool: &PgPool, raw_token: &str) -> Result<Option<AuthLookup>, ApiError> {
    let hash = token::hash_token(raw_token);

    // Query type override: expires_at is TIMESTAMPTZ, compare with now()
    let row = sqlx::query_as!(
        AuthLookup,
        r#"
        SELECT u.id as "user_id!", u.name as "user_name!", u.is_active as "is_active!"
        FROM auth_sessions s
        JOIN users u ON u.id = s.user_id
        WHERE s.token_hash = $1
          AND s.expires_at > now()
        "#,
        hash,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row)
}
