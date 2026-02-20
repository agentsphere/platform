use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::token;
use crate::auth::user_type::UserType;
use crate::error::ApiError;
use crate::store::AppState;

/// Authenticated user extracted from request.
/// Set as request extension by the auth middleware.
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub user_id: Uuid,
    pub user_name: String,
    pub user_type: UserType,
    pub ip_addr: Option<String>,
    /// Token scopes if authenticated via API token.
    /// None = session auth (no scope restriction).
    /// Some(vec![]) or Some(vec!["*"]) = unrestricted token.
    /// Some(vec!["project:read", ...]) = scoped token.
    pub token_scopes: Option<Vec<String>>,
}

/// Row returned when looking up an API token.
struct TokenAuthLookup {
    user_id: Uuid,
    user_name: String,
    user_type: String,
    is_active: bool,
    scopes: Vec<String>,
}

/// Row returned when looking up a session.
struct SessionAuthLookup {
    user_id: Uuid,
    user_name: String,
    user_type: String,
    is_active: bool,
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let trust_proxy = state.config.trust_proxy_headers;
        let ip_addr = extract_ip(parts, trust_proxy);

        // Try Bearer token first
        if let Some(raw_token) = extract_bearer_token(parts)
            && let Some(user) = lookup_api_token(&state.pool, &raw_token).await?
        {
            if !user.is_active {
                return Err(ApiError::Unauthorized);
            }
            let user_type: UserType = user
                .user_type
                .parse()
                .map_err(|e: anyhow::Error| ApiError::Internal(e))?;
            return Ok(Self {
                user_id: user.user_id,
                user_name: user.user_name,
                user_type,
                ip_addr,
                token_scopes: Some(user.scopes),
            });
        }

        // Try session cookie
        if let Some(session_token) = extract_session_cookie(parts)
            && let Some(user) = lookup_session(&state.pool, &session_token).await?
        {
            if !user.is_active {
                return Err(ApiError::Unauthorized);
            }
            let user_type: UserType = user
                .user_type
                .parse()
                .map_err(|e: anyhow::Error| ApiError::Internal(e))?;
            // Non-human users cannot use session-based auth
            if !user_type.can_login() {
                return Err(ApiError::Unauthorized);
            }
            return Ok(Self {
                user_id: user.user_id,
                user_name: user.user_name,
                user_type,
                ip_addr,
                token_scopes: None,
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

fn extract_ip(parts: &Parts, trust_proxy: bool) -> Option<String> {
    // Only trust X-Forwarded-For when behind a configured reverse proxy
    if trust_proxy
        && let Some(forwarded) = parts.headers.get("x-forwarded-for")
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
async fn lookup_api_token(
    pool: &PgPool,
    raw_token: &str,
) -> Result<Option<TokenAuthLookup>, ApiError> {
    let hash = token::hash_token(raw_token);

    let row = sqlx::query_as!(
        TokenAuthLookup,
        r#"
        SELECT u.id as "user_id!", u.name as "user_name!",
               u.user_type as "user_type!", u.is_active as "is_active!",
               t.scopes as "scopes!"
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
async fn lookup_session(
    pool: &PgPool,
    raw_token: &str,
) -> Result<Option<SessionAuthLookup>, ApiError> {
    let hash = token::hash_token(raw_token);

    let row = sqlx::query_as!(
        SessionAuthLookup,
        r#"
        SELECT u.id as "user_id!", u.name as "user_name!",
               u.user_type as "user_type!", u.is_active as "is_active!"
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;

    fn make_parts(headers: &[(&str, &str)]) -> Parts {
        let mut builder = Request::builder().uri("/test");
        for &(k, v) in headers {
            builder = builder.header(k, v);
        }
        let (parts, _) = builder.body(()).unwrap().into_parts();
        parts
    }

    // -- extract_bearer_token --

    #[test]
    fn bearer_token_valid() {
        let parts = make_parts(&[("authorization", "Bearer abc123")]);
        assert_eq!(extract_bearer_token(&parts), Some("abc123".into()));
    }

    #[test]
    fn bearer_token_missing_header() {
        let parts = make_parts(&[]);
        assert_eq!(extract_bearer_token(&parts), None);
    }

    #[test]
    fn bearer_token_wrong_scheme() {
        let parts = make_parts(&[("authorization", "Basic dXNlcjpwYXNz")]);
        assert_eq!(extract_bearer_token(&parts), None);
    }

    #[test]
    fn bearer_token_empty_after_prefix() {
        let parts = make_parts(&[("authorization", "Bearer ")]);
        assert_eq!(extract_bearer_token(&parts), None);
    }

    #[test]
    fn bearer_token_preserves_full_value() {
        let token = "plat_aVeryLongToken1234567890abcdefghijklmnop";
        let parts = make_parts(&[("authorization", &format!("Bearer {token}"))]);
        assert_eq!(extract_bearer_token(&parts), Some(token.into()));
    }

    #[test]
    fn bearer_token_case_sensitive_prefix() {
        let parts = make_parts(&[("authorization", "bearer abc123")]);
        assert_eq!(extract_bearer_token(&parts), None);
    }

    // -- extract_session_cookie --

    #[test]
    fn session_cookie_valid() {
        let parts = make_parts(&[("cookie", "session=tok123")]);
        assert_eq!(extract_session_cookie(&parts), Some("tok123".into()));
    }

    #[test]
    fn session_cookie_among_others() {
        let parts = make_parts(&[("cookie", "foo=bar; session=tok123; baz=qux")]);
        assert_eq!(extract_session_cookie(&parts), Some("tok123".into()));
    }

    #[test]
    fn session_cookie_missing() {
        let parts = make_parts(&[("cookie", "foo=bar; other=val")]);
        assert_eq!(extract_session_cookie(&parts), None);
    }

    #[test]
    fn session_cookie_empty_value() {
        let parts = make_parts(&[("cookie", "session=")]);
        assert_eq!(extract_session_cookie(&parts), None);
    }

    #[test]
    fn session_cookie_no_header() {
        let parts = make_parts(&[]);
        assert_eq!(extract_session_cookie(&parts), None);
    }

    // -- extract_ip --

    #[test]
    fn ip_from_forwarded_for_trusted() {
        let parts = make_parts(&[("x-forwarded-for", "1.2.3.4, 5.6.7.8")]);
        assert_eq!(extract_ip(&parts, true), Some("1.2.3.4".into()));
    }

    #[test]
    fn ip_forwarded_for_ignored_when_not_trusted() {
        let parts = make_parts(&[("x-forwarded-for", "1.2.3.4")]);
        assert_eq!(extract_ip(&parts, false), None);
    }

    #[test]
    fn ip_from_connect_info() {
        let mut parts = make_parts(&[]);
        let addr: std::net::SocketAddr = "127.0.0.1:9000".parse().unwrap();
        parts.extensions.insert(axum::extract::ConnectInfo(addr));
        assert_eq!(extract_ip(&parts, false), Some("127.0.0.1".into()));
    }
}
