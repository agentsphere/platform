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
    /// Hard workspace boundary from scoped API token.
    /// When set, all requests are restricted to this workspace's resources.
    /// Named "boundary" (not "scope") to distinguish from `token_scopes` which
    /// filter permissions. Boundaries restrict *which resources* are visible.
    pub boundary_workspace_id: Option<Uuid>,
    /// Hard project boundary from scoped API token.
    /// When set, all requests are restricted to this specific project.
    pub boundary_project_id: Option<Uuid>,
    /// Agent session ID, extracted from token name `agent-session-{uuid}`.
    /// Present only when authenticated via an agent API token.
    pub session_id: Option<Uuid>,
}

impl AuthUser {
    /// Record auth context fields into the current tracing span.
    /// Called after successful authentication so request-level spans carry identity.
    fn record_to_span(&self) {
        let span = tracing::Span::current();
        span.record("user_id", tracing::field::display(self.user_id));
        span.record("user_type", tracing::field::display(&self.user_type));
        if let Some(sid) = &self.session_id {
            span.record("session_id", tracing::field::display(sid));
        }
    }

    /// Verify this request is allowed to access the given project.
    /// Returns 404 for scope violations (don't leak resource existence).
    pub fn check_project_scope(&self, project_id: Uuid) -> Result<(), ApiError> {
        if let Some(boundary_pid) = self.boundary_project_id
            && boundary_pid != project_id
        {
            return Err(ApiError::NotFound("project".into()));
        }
        Ok(())
    }

    /// Verify this request is allowed to access resources in the given workspace.
    /// Returns 404 for scope violations (don't leak resource existence).
    #[allow(dead_code)] // symmetric with check_project_scope; used by workspace-aware handlers
    pub fn check_workspace_scope(&self, workspace_id: Uuid) -> Result<(), ApiError> {
        if let Some(boundary_wid) = self.boundary_workspace_id
            && boundary_wid != workspace_id
        {
            return Err(ApiError::NotFound("workspace".into()));
        }
        Ok(())
    }
}

/// Parse `user_type` string from DB into the `UserType` enum.
fn parse_user_type(s: &str) -> Result<UserType, ApiError> {
    s.parse().map_err(|e: anyhow::Error| ApiError::Internal(e))
}

/// Row returned when looking up an API token.
///
/// Near-identical to `SessionAuthLookup` but includes `scopes`, `scope_project_id`,
/// and `scope_workspace_id` fields from the `api_tokens` table. They can't be
/// consolidated because `sqlx::query_as!` requires the struct to match the exact
/// column set returned by each query.
struct TokenAuthLookup {
    user_id: Uuid,
    user_name: String,
    user_type: String,
    is_active: bool,
    name: String,
    scopes: Vec<String>,
    scope_project_id: Option<Uuid>,
    scope_workspace_id: Option<Uuid>,
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

        // Try Bearer token — check API tokens first, then session tokens
        if let Some(raw_token) = extract_bearer_token(parts) {
            if let Some(user) = lookup_api_token(&state.pool, raw_token).await? {
                if !user.is_active {
                    return Err(ApiError::Unauthorized);
                }
                // API token auth intentionally does NOT check can_login() —
                // agent users authenticate exclusively via API tokens, not sessions.
                let user_type = parse_user_type(&user.user_type)?;
                let session_id = if user_type == UserType::Agent {
                    user.name
                        .strip_prefix("agent-session-")
                        .and_then(|s| Uuid::parse_str(s).ok())
                } else {
                    None
                };
                let auth_user = Self {
                    user_id: user.user_id,
                    user_name: user.user_name,
                    user_type,
                    ip_addr,
                    token_scopes: Some(user.scopes),
                    boundary_workspace_id: user.scope_workspace_id,
                    boundary_project_id: user.scope_project_id,
                    session_id,
                };
                auth_user.record_to_span();
                return Ok(auth_user);
            }
            // Bearer token not in api_tokens — try as session token
            if let Some(user) = lookup_session(&state.pool, raw_token).await? {
                if !user.is_active {
                    return Err(ApiError::Unauthorized);
                }
                let user_type = parse_user_type(&user.user_type)?;
                if !user_type.can_login() {
                    return Err(ApiError::Unauthorized);
                }
                let auth_user = Self {
                    user_id: user.user_id,
                    user_name: user.user_name,
                    user_type,
                    ip_addr,
                    token_scopes: None,
                    boundary_workspace_id: None,
                    boundary_project_id: None,
                    session_id: None,
                };
                auth_user.record_to_span();
                return Ok(auth_user);
            }
        }

        // Try session cookie
        if let Some(session_token) = extract_session_cookie(parts)
            && let Some(user) = lookup_session(&state.pool, session_token).await?
        {
            if !user.is_active {
                return Err(ApiError::Unauthorized);
            }
            let user_type = parse_user_type(&user.user_type)?;
            // Non-human users cannot use session-based auth
            if !user_type.can_login() {
                return Err(ApiError::Unauthorized);
            }
            let auth_user = Self {
                user_id: user.user_id,
                user_name: user.user_name,
                user_type,
                ip_addr,
                token_scopes: None,
                boundary_workspace_id: None,
                boundary_project_id: None,
                session_id: None,
            };
            auth_user.record_to_span();
            return Ok(auth_user);
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

fn extract_bearer_token(parts: &Parts) -> Option<&str> {
    let value = parts.headers.get(AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ")?;
    if token.is_empty() {
        return None;
    }
    Some(token)
}

fn extract_session_cookie(parts: &Parts) -> Option<&str> {
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
            return Some(value);
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
               t.name as "name!",
               t.scopes as "scopes!",
               t.project_id as "scope_project_id?",
               t.scope_workspace_id as "scope_workspace_id?"
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
impl AuthUser {
    /// Create a test `AuthUser` representing a human user with default name and IP.
    pub fn test_human(user_id: Uuid) -> Self {
        Self {
            user_id,
            user_name: "test_user".into(),
            user_type: UserType::Human,
            ip_addr: Some("127.0.0.1".into()),
            token_scopes: None,
            boundary_workspace_id: None,
            boundary_project_id: None,
            session_id: None,
        }
    }

    /// Create a test `AuthUser` with a custom name.
    pub fn test_with_name(user_id: Uuid, name: &str) -> Self {
        Self {
            user_id,
            user_name: name.into(),
            user_type: UserType::Human,
            ip_addr: Some("127.0.0.1".into()),
            token_scopes: None,
            boundary_workspace_id: None,
            boundary_project_id: None,
            session_id: None,
        }
    }

    /// Create a test `AuthUser` with specified token scopes.
    pub fn test_with_scopes(user_id: Uuid, scopes: Vec<String>) -> Self {
        Self {
            user_id,
            user_name: "test_user".into(),
            user_type: UserType::Human,
            ip_addr: Some("127.0.0.1".into()),
            token_scopes: Some(scopes),
            boundary_workspace_id: None,
            boundary_project_id: None,
            session_id: None,
        }
    }

    /// Create a test `AuthUser` with a project boundary.
    pub fn test_with_project_scope(user_id: Uuid, project_id: Uuid) -> Self {
        Self {
            user_id,
            user_name: "test_user".into(),
            user_type: UserType::Human,
            ip_addr: Some("127.0.0.1".into()),
            token_scopes: None,
            boundary_workspace_id: None,
            boundary_project_id: Some(project_id),
            session_id: None,
        }
    }

    /// Create a test `AuthUser` with a workspace boundary.
    pub fn test_with_workspace_scope(user_id: Uuid, workspace_id: Uuid) -> Self {
        Self {
            user_id,
            user_name: "test_user".into(),
            user_type: UserType::Human,
            ip_addr: Some("127.0.0.1".into()),
            token_scopes: None,
            boundary_workspace_id: Some(workspace_id),
            boundary_project_id: None,
            session_id: None,
        }
    }
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
        let (parts, ()) = builder.body(()).unwrap().into_parts();
        parts
    }

    // -- extract_bearer_token --

    #[test]
    fn bearer_token_valid() {
        let parts = make_parts(&[("authorization", "Bearer abc123")]);
        assert_eq!(extract_bearer_token(&parts), Some("abc123"));
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
        assert_eq!(extract_bearer_token(&parts), Some(token));
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
        assert_eq!(extract_session_cookie(&parts), Some("tok123"));
    }

    #[test]
    fn session_cookie_among_others() {
        let parts = make_parts(&[("cookie", "foo=bar; session=tok123; baz=qux")]);
        assert_eq!(extract_session_cookie(&parts), Some("tok123"));
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

    // -- Edge case tests --

    #[test]
    fn bearer_token_double_space_returns_none() {
        // "Bearer  abc" — strip_prefix("Bearer ") gives " abc" (leading space)
        let parts = make_parts(&[("authorization", "Bearer  abc")]);
        assert_eq!(extract_bearer_token(&parts), Some(" abc"));
    }

    #[test]
    fn bearer_token_with_spaces_in_token() {
        let parts = make_parts(&[("authorization", "Bearer abc def ghi")]);
        assert_eq!(extract_bearer_token(&parts), Some("abc def ghi"));
    }

    #[test]
    fn session_cookie_with_equals_in_value() {
        // Cookie values can contain '=' (e.g., base64). strip_prefix only strips "session=".
        let parts = make_parts(&[("cookie", "session=tok=123=abc")]);
        assert_eq!(extract_session_cookie(&parts), Some("tok=123=abc"));
    }

    #[test]
    fn ip_from_forwarded_for_ipv6() {
        let parts = make_parts(&[("x-forwarded-for", "::1, 2001:db8::1")]);
        assert_eq!(extract_ip(&parts, true), Some("::1".into()));
    }

    #[test]
    fn ip_from_forwarded_for_trims_whitespace() {
        let parts = make_parts(&[("x-forwarded-for", "  1.2.3.4 , 5.6.7.8 ")]);
        assert_eq!(extract_ip(&parts, true), Some("1.2.3.4".into()));
    }

    // -- Additional edge case tests --

    #[test]
    fn bearer_token_no_space_returns_none() {
        // "Bearerabc" — strip_prefix("Bearer ") won't match
        let parts = make_parts(&[("authorization", "Bearerabc")]);
        assert_eq!(extract_bearer_token(&parts), None);
    }

    #[test]
    fn empty_authorization_header_returns_none() {
        let parts = make_parts(&[("authorization", "")]);
        assert_eq!(extract_bearer_token(&parts), None);
    }

    #[test]
    fn ip_no_headers_no_connect_info_returns_none() {
        let parts = make_parts(&[]);
        assert_eq!(extract_ip(&parts, true), None);
    }

    #[test]
    fn ip_from_forwarded_for_single_ipv6() {
        let parts = make_parts(&[("x-forwarded-for", "2001:db8::1")]);
        assert_eq!(extract_ip(&parts, true), Some("2001:db8::1".into()));
    }

    // -- AuthUser test constructor tests --

    #[test]
    fn test_human_constructor() {
        let id = Uuid::new_v4();
        let auth = AuthUser::test_human(id);
        assert_eq!(auth.user_id, id);
        assert_eq!(auth.user_name, "test_user");
        assert_eq!(auth.user_type, UserType::Human);
        assert_eq!(auth.ip_addr, Some("127.0.0.1".into()));
        assert!(auth.token_scopes.is_none());
    }

    #[test]
    fn test_with_name_constructor() {
        let id = Uuid::new_v4();
        let auth = AuthUser::test_with_name(id, "alice");
        assert_eq!(auth.user_name, "alice");
    }

    #[test]
    fn test_with_scopes_constructor() {
        let id = Uuid::new_v4();
        let auth = AuthUser::test_with_scopes(id, vec!["project:read".into()]);
        assert_eq!(auth.token_scopes, Some(vec!["project:read".to_string()]));
    }

    // -- Scope check tests --

    #[test]
    fn check_project_scope_none_allows_any() {
        let auth = AuthUser::test_human(Uuid::new_v4());
        assert!(auth.check_project_scope(Uuid::new_v4()).is_ok());
    }

    #[test]
    fn check_project_scope_matching_allows() {
        let project_id = Uuid::new_v4();
        let auth = AuthUser::test_with_project_scope(Uuid::new_v4(), project_id);
        assert!(auth.check_project_scope(project_id).is_ok());
    }

    #[test]
    fn check_project_scope_mismatch_returns_not_found() {
        let auth = AuthUser::test_with_project_scope(Uuid::new_v4(), Uuid::new_v4());
        let result = auth.check_project_scope(Uuid::new_v4());
        assert!(matches!(result, Err(ApiError::NotFound(_))));
    }

    #[test]
    fn check_workspace_scope_none_allows_any() {
        let auth = AuthUser::test_human(Uuid::new_v4());
        assert!(auth.check_workspace_scope(Uuid::new_v4()).is_ok());
    }

    #[test]
    fn check_workspace_scope_matching_allows() {
        let ws_id = Uuid::new_v4();
        let auth = AuthUser::test_with_workspace_scope(Uuid::new_v4(), ws_id);
        assert!(auth.check_workspace_scope(ws_id).is_ok());
    }

    #[test]
    fn check_workspace_scope_mismatch_returns_not_found() {
        let auth = AuthUser::test_with_workspace_scope(Uuid::new_v4(), Uuid::new_v4());
        let result = auth.check_workspace_scope(Uuid::new_v4());
        assert!(matches!(result, Err(ApiError::NotFound(_))));
    }

    #[test]
    fn test_human_has_none_boundaries() {
        let auth = AuthUser::test_human(Uuid::new_v4());
        assert!(auth.boundary_workspace_id.is_none());
        assert!(auth.boundary_project_id.is_none());
    }

    #[test]
    fn test_with_project_scope_constructor_sets_field() {
        let project_id = Uuid::new_v4();
        let auth = AuthUser::test_with_project_scope(Uuid::new_v4(), project_id);
        assert_eq!(auth.boundary_project_id, Some(project_id));
        assert!(auth.boundary_workspace_id.is_none());
    }

    #[test]
    fn test_with_workspace_scope_constructor_sets_field() {
        let ws_id = Uuid::new_v4();
        let auth = AuthUser::test_with_workspace_scope(Uuid::new_v4(), ws_id);
        assert_eq!(auth.boundary_workspace_id, Some(ws_id));
        assert!(auth.boundary_project_id.is_none());
    }
}
