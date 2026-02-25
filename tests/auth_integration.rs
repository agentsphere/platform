mod helpers;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Helper: make a GET request with a session cookie instead of Bearer token
// ---------------------------------------------------------------------------

async fn get_with_cookie(
    app: &axum::Router,
    cookie_value: &str,
    path: &str,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method("GET")
        .uri(path)
        .header("Cookie", format!("session={cookie_value}"))
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, body)
}

/// Make a GET request with a malformed Authorization header.
async fn get_with_auth_header(
    app: &axum::Router,
    header_value: &str,
    path: &str,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method("GET")
        .uri(path)
        .header("Authorization", header_value)
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, body)
}

// ---------------------------------------------------------------------------
// E2: Auth Integration Tests (13 tests)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn login_valid_credentials(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        "",
        "/api/auth/login",
        serde_json::json!({ "name": "admin", "password": "testpassword" }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(body["token"].is_string(), "response must include token");
    assert_eq!(body["user"]["name"], "admin");
    let expires = body["expires_at"].as_str().unwrap();
    let parsed = chrono::DateTime::parse_from_rfc3339(expires).unwrap();
    assert!(parsed > chrono::Utc::now());
}

#[sqlx::test(migrations = "./migrations")]
async fn login_wrong_password(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        "",
        "/api/auth/login",
        serde_json::json!({ "name": "admin", "password": "wrongpassword" }),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body["error"].is_string());
    assert!(body.get("token").is_none() || body["token"].is_null());
}

#[sqlx::test(migrations = "./migrations")]
async fn login_nonexistent_user(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        "",
        "/api/auth/login",
        serde_json::json!({ "name": "doesnotexist", "password": "somepassword" }),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body["error"].is_string());
}

#[sqlx::test(migrations = "./migrations")]
async fn login_inactive_user(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let admin_token = helpers::admin_login(&app).await;
    let (user_id, _user_token) =
        helpers::create_user(&app, &admin_token, "inactiveuser", "inactive@test.com").await;

    // Deactivate user
    let (status, _) =
        helpers::delete_json(&app, &admin_token, &format!("/api/users/{user_id}")).await;
    assert_eq!(status, StatusCode::OK);

    // Try to login
    let (status, _) = helpers::post_json(
        &app,
        "",
        "/api/auth/login",
        serde_json::json!({ "name": "inactiveuser", "password": "testpass123" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn login_rate_limited(pool: PgPool) {
    use fred::interfaces::KeysInterface;

    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state.clone());

    // Use a UUID-based username so the rate limit key never collides with other tests.
    let unique_name = format!("rl-{}", Uuid::new_v4());
    let admin_token = helpers::admin_login(&app).await;
    helpers::create_user(
        &app,
        &admin_token,
        &unique_name,
        &format!("{unique_name}@test.com"),
    )
    .await;

    // Pre-set the rate limit counter to just below the threshold (10) so we only
    // need one login attempt to trigger 429. This avoids the race where another
    // test's FLUSHDB resets our counter mid-loop.
    let rate_key = format!("rate:login:{unique_name}");
    let _: () = state
        .valkey
        .set(&rate_key, 10i64, None, None, false)
        .await
        .unwrap();
    let _: () = state.valkey.expire(&rate_key, 300, None).await.unwrap();

    // This attempt should exceed the limit (count goes from 10 → 11 > 10)
    let (status, _) = helpers::post_json(
        &app,
        "",
        "/api/auth/login",
        serde_json::json!({ "name": &unique_name, "password": "wrong" }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "expected 429 after exceeding rate limit"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn get_me_with_valid_token(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let token = helpers::admin_login(&app).await;
    let (status, body) = helpers::get_json(&app, &token, "/api/auth/me").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "admin");
}

#[sqlx::test(migrations = "./migrations")]
async fn get_me_without_token(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::get_json(&app, "", "/api/auth/me").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn get_me_with_expired_token(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let token = helpers::admin_login(&app).await;

    // Verify token works
    let (status, _) = helpers::get_json(&app, &token, "/api/auth/me").await;
    assert_eq!(status, StatusCode::OK);

    // Expire all sessions in DB
    sqlx::query("UPDATE auth_sessions SET expires_at = now() - interval '1 hour'")
        .execute(&pool)
        .await
        .unwrap();

    // Token should now be expired
    let (status, _) = helpers::get_json(&app, &token, "/api/auth/me").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn create_api_token(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let token = helpers::admin_login(&app).await;

    let (status, body) = helpers::post_json(
        &app,
        &token,
        "/api/tokens",
        serde_json::json!({
            "name": "test-token",
            "scopes": [],
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert!(body["token"].as_str().unwrap().starts_with("plat_"));
    assert_eq!(body["name"], "test-token");
}

#[sqlx::test(migrations = "./migrations")]
async fn create_api_token_scope_escalation_blocked(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let admin_token = helpers::admin_login(&app).await;
    // Create a regular user with viewer role
    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "vieweruser", "viewer@test.com").await;
    helpers::assign_role(&app, &admin_token, user_id, "viewer", None, &pool).await;

    // Try creating a token with admin scope — user doesn't have admin:users
    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        "/api/tokens",
        serde_json::json!({
            "name": "escalation-token",
            "scopes": ["admin:users"],
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn list_and_delete_api_token(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let token = helpers::admin_login(&app).await;

    // Create a token
    let (status, create_body) = helpers::post_json(
        &app,
        &token,
        "/api/tokens",
        serde_json::json!({ "name": "to-delete" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let token_id = create_body["id"].as_str().unwrap();

    // List tokens
    let (status, list_body) = helpers::get_json(&app, &token, "/api/tokens").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        list_body
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["id"] == token_id)
    );

    // Delete
    let (status, _) = helpers::delete_json(&app, &token, &format!("/api/tokens/{token_id}")).await;
    assert_eq!(status, StatusCode::OK);

    // Verify gone
    let (status, list_body) = helpers::get_json(&app, &token, "/api/tokens").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !list_body
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["id"] == token_id)
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn update_own_profile(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let admin_token = helpers::admin_login(&app).await;

    // Get admin user ID
    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = me["id"].as_str().unwrap();

    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/users/{admin_id}"),
        serde_json::json!({ "display_name": "New Admin Name" }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["display_name"], "New Admin Name");
}

#[sqlx::test(migrations = "./migrations")]
async fn non_human_user_cannot_login(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let admin_token = helpers::admin_login(&app).await;

    // Create a service account (non-human) via admin API
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/service-accounts",
        serde_json::json!({
            "name": "bot-account",
            "email": "bot@test.com",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Try to login as the service account
    let (status, _) = helpers::post_json(
        &app,
        "",
        "/api/auth/login",
        serde_json::json!({ "name": "bot-account", "password": "anypassword" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ===========================================================================
// Auth Middleware Integration Tests — edge cases for coverage
// ===========================================================================

// ---------------------------------------------------------------------------
// Bearer token: expired API token
// ---------------------------------------------------------------------------

/// An expired API token should return 401.
#[sqlx::test(migrations = "./migrations")]
async fn expired_api_token_returns_401(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let user_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    // Create an API token that's already expired by inserting directly
    let raw_token = format!(
        "plat_api_expired_{}",
        Uuid::new_v4().to_string().replace('-', "")
    );
    let token_hash = platform::auth::token::hash_token(&raw_token);
    let expired_at = chrono::Utc::now() - chrono::Duration::hours(1);

    sqlx::query(
        "INSERT INTO api_tokens (user_id, name, token_hash, expires_at, scopes)
         VALUES ($1, 'expired-token', $2, $3, ARRAY[]::text[])",
    )
    .bind(user_id)
    .bind(&token_hash)
    .bind(expired_at)
    .execute(&pool)
    .await
    .unwrap();

    // Attempt to use the expired token
    let (status, _) = helpers::get_json(&app, &raw_token, "/api/auth/me").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Bearer token: malformed Authorization header
// ---------------------------------------------------------------------------

/// Malformed Authorization header (wrong scheme) should return 401.
#[sqlx::test(migrations = "./migrations")]
async fn malformed_auth_header_basic_scheme(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Basic auth header instead of Bearer
    let (status, _) = get_with_auth_header(&app, "Basic dXNlcjpwYXNz", "/api/auth/me").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// Authorization header with "Bearer " but no token should return 401.
#[sqlx::test(migrations = "./migrations")]
async fn auth_header_empty_bearer_token(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = get_with_auth_header(&app, "Bearer ", "/api/auth/me").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// Authorization header with lowercase "bearer" should return 401
/// (case-sensitive prefix check).
#[sqlx::test(migrations = "./migrations")]
async fn auth_header_lowercase_bearer(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = get_with_auth_header(&app, "bearer sometoken", "/api/auth/me").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// A completely bogus bearer token (not in any table) returns 401.
#[sqlx::test(migrations = "./migrations")]
async fn bearer_token_not_in_db(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) =
        helpers::get_json(&app, "plat_totally_bogus_token_value", "/api/auth/me").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Cookie-based session auth
// ---------------------------------------------------------------------------

/// Valid session cookie should authenticate the user.
#[sqlx::test(migrations = "./migrations")]
async fn session_cookie_auth_works(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    // Login to get a session token
    let (status, body) = helpers::post_json(
        &app,
        "",
        "/api/auth/login",
        serde_json::json!({ "name": "admin", "password": "testpassword" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let session_token = body["token"].as_str().unwrap();

    // Use the token as a cookie instead of Bearer header
    let (status, body) = get_with_cookie(&app, session_token, "/api/auth/me").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "admin");
}

/// Missing session cookie (no Cookie header at all) returns 401.
#[sqlx::test(migrations = "./migrations")]
async fn missing_cookie_header_returns_401(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Request with neither Bearer nor Cookie
    let req = Request::builder()
        .method("GET")
        .uri("/api/auth/me")
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// Session cookie with empty value should return 401.
#[sqlx::test(migrations = "./migrations")]
async fn empty_session_cookie_returns_401(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = get_with_cookie(&app, "", "/api/auth/me").await;
    // extract_session_cookie returns None for empty value
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// Invalid session token in cookie (not matching any session in DB) returns 401.
#[sqlx::test(migrations = "./migrations")]
async fn invalid_session_cookie_returns_401(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = get_with_cookie(&app, "plat_bogus_session_token_value", "/api/auth/me").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// Expired session cookie returns 401.
#[sqlx::test(migrations = "./migrations")]
async fn expired_session_cookie_returns_401(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    // Login to get a session
    let (_, body) = helpers::post_json(
        &app,
        "",
        "/api/auth/login",
        serde_json::json!({ "name": "admin", "password": "testpassword" }),
    )
    .await;
    let session_token = body["token"].as_str().unwrap().to_owned();

    // Expire the session
    sqlx::query("UPDATE auth_sessions SET expires_at = now() - interval '1 hour'")
        .execute(&pool)
        .await
        .unwrap();

    // Cookie-based auth with expired session should fail
    let (status, _) = get_with_cookie(&app, &session_token, "/api/auth/me").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Deactivated user: API token
// ---------------------------------------------------------------------------

/// An active API token belonging to a deactivated user should return 401.
#[sqlx::test(migrations = "./migrations")]
async fn deactivated_user_api_token_returns_401(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let admin_token = helpers::admin_login(&app).await;

    // Create a user and get their token
    let (user_id, _user_token) =
        helpers::create_user(&app, &admin_token, "deact-api", "deactapi@test.com").await;

    // Create an API token for this user
    let raw_token = format!(
        "plat_api_deact_{}",
        Uuid::new_v4().to_string().replace('-', "")
    );
    let token_hash = platform::auth::token::hash_token(&raw_token);
    let expires_at = chrono::Utc::now() + chrono::Duration::days(30);

    sqlx::query(
        "INSERT INTO api_tokens (user_id, name, token_hash, expires_at, scopes)
         VALUES ($1, 'deact-token', $2, $3, ARRAY[]::text[])",
    )
    .bind(user_id)
    .bind(&token_hash)
    .bind(expires_at)
    .execute(&pool)
    .await
    .unwrap();

    // Verify API token works before deactivation
    let (status, body) = helpers::get_json(&app, &raw_token, "/api/auth/me").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "deact-api");

    // Deactivate the user
    let (status, _) =
        helpers::delete_json(&app, &admin_token, &format!("/api/users/{user_id}")).await;
    assert_eq!(status, StatusCode::OK);

    // API token for deactivated user should now return 401
    let (status, _) = helpers::get_json(&app, &raw_token, "/api/auth/me").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Deactivated user: session token
// ---------------------------------------------------------------------------

/// A session token belonging to a deactivated user should return 401.
#[sqlx::test(migrations = "./migrations")]
async fn deactivated_user_session_token_returns_401(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let admin_token = helpers::admin_login(&app).await;
    let (_user_id, user_token) =
        helpers::create_user(&app, &admin_token, "deact-sess", "deactsess@test.com").await;

    // Verify session works
    let (status, _) = helpers::get_json(&app, &user_token, "/api/auth/me").await;
    assert_eq!(status, StatusCode::OK);

    // Deactivate the user
    let (status, _) =
        helpers::delete_json(&app, &admin_token, &format!("/api/users/{_user_id}")).await;
    assert_eq!(status, StatusCode::OK);

    // Session token for deactivated user should now return 401
    // (Bearer token path: tries api_tokens first (miss), then falls back to session lookup)
    let (status, _) = helpers::get_json(&app, &user_token, "/api/auth/me").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// Deactivated user's session cookie should also return 401.
#[sqlx::test(migrations = "./migrations")]
async fn deactivated_user_session_cookie_returns_401(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let admin_token = helpers::admin_login(&app).await;
    let (user_id, _) =
        helpers::create_user(&app, &admin_token, "deact-cookie", "deactcookie@test.com").await;

    // Login as user to get session token
    let (_, login_body) = helpers::post_json(
        &app,
        "",
        "/api/auth/login",
        serde_json::json!({ "name": "deact-cookie", "password": "testpass123" }),
    )
    .await;
    let session_token = login_body["token"].as_str().unwrap().to_owned();

    // Verify cookie auth works
    let (status, body) = get_with_cookie(&app, &session_token, "/api/auth/me").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "deact-cookie");

    // Deactivate the user
    let (status, _) =
        helpers::delete_json(&app, &admin_token, &format!("/api/users/{user_id}")).await;
    assert_eq!(status, StatusCode::OK);

    // Cookie-based auth for deactivated user should return 401
    let (status, _) = get_with_cookie(&app, &session_token, "/api/auth/me").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Non-human user: session auth blocked
// ---------------------------------------------------------------------------

/// A service_account with a valid session should be blocked by can_login() check.
#[sqlx::test(migrations = "./migrations")]
async fn service_account_bearer_session_blocked(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    // Create a service account directly in DB
    let svc_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, name, email, password_hash, user_type, is_active)
         VALUES ($1, 'svc-mid-test', 'svcmid@test.com', 'not-a-hash', 'service_account', true)",
    )
    .bind(svc_id)
    .execute(&pool)
    .await
    .unwrap();

    // Create a session for this service account (bypassing login which blocks non-humans)
    let raw_token = format!(
        "plat_svc_sess_{}",
        Uuid::new_v4().to_string().replace('-', "")
    );
    let token_hash = platform::auth::token::hash_token(&raw_token);
    let expires_at = chrono::Utc::now() + chrono::Duration::hours(24);

    sqlx::query("INSERT INTO auth_sessions (user_id, token_hash, expires_at) VALUES ($1, $2, $3)")
        .bind(svc_id)
        .bind(&token_hash)
        .bind(expires_at)
        .execute(&pool)
        .await
        .unwrap();

    // Try to authenticate using the session as Bearer token
    // The middleware should find it in auth_sessions but reject because
    // user_type is service_account and can_login() returns false.
    let (status, _) = helpers::get_json(&app, &raw_token, "/api/auth/me").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// A service_account with a session cookie should also be blocked.
#[sqlx::test(migrations = "./migrations")]
async fn service_account_cookie_session_blocked(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    // Create a service account directly
    let svc_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, name, email, password_hash, user_type, is_active)
         VALUES ($1, 'svc-cookie-test', 'svccookie@test.com', 'not-a-hash', 'service_account', true)",
    )
    .bind(svc_id)
    .execute(&pool)
    .await
    .unwrap();

    // Create a session for this service account
    let raw_token = format!(
        "plat_svc_cook_{}",
        Uuid::new_v4().to_string().replace('-', "")
    );
    let token_hash = platform::auth::token::hash_token(&raw_token);
    let expires_at = chrono::Utc::now() + chrono::Duration::hours(24);

    sqlx::query("INSERT INTO auth_sessions (user_id, token_hash, expires_at) VALUES ($1, $2, $3)")
        .bind(svc_id)
        .bind(&token_hash)
        .bind(expires_at)
        .execute(&pool)
        .await
        .unwrap();

    // Try cookie-based auth — should be blocked by can_login() check
    let (status, _) = get_with_cookie(&app, &raw_token, "/api/auth/me").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// API token: service account CAN use API tokens (no can_login check)
// ---------------------------------------------------------------------------

/// A service_account with a valid API token should authenticate successfully.
/// The middleware only checks can_login() for session auth, not for API tokens.
#[sqlx::test(migrations = "./migrations")]
async fn service_account_api_token_succeeds(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    // Create a service account directly
    let svc_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, name, email, password_hash, user_type, is_active)
         VALUES ($1, 'svc-api-ok', 'svcapiok@test.com', 'not-a-hash', 'service_account', true)",
    )
    .bind(svc_id)
    .execute(&pool)
    .await
    .unwrap();

    // Create an API token for the service account
    let raw_token = format!(
        "plat_api_svc_{}",
        Uuid::new_v4().to_string().replace('-', "")
    );
    let token_hash = platform::auth::token::hash_token(&raw_token);
    let expires_at = chrono::Utc::now() + chrono::Duration::days(30);

    sqlx::query(
        "INSERT INTO api_tokens (user_id, name, token_hash, expires_at, scopes)
         VALUES ($1, 'svc-api-token', $2, $3, ARRAY[]::text[])",
    )
    .bind(svc_id)
    .bind(&token_hash)
    .bind(expires_at)
    .execute(&pool)
    .await
    .unwrap();

    // API token auth should work for service accounts
    let (status, body) = helpers::get_json(&app, &raw_token, "/api/auth/me").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "svc-api-ok");
}

// ---------------------------------------------------------------------------
// Bearer token: session token used as Bearer (fallback path)
// ---------------------------------------------------------------------------

/// A session token used as Bearer header (not cookie) should also work.
/// The middleware tries api_tokens first, then falls back to auth_sessions.
#[sqlx::test(migrations = "./migrations")]
async fn session_token_as_bearer_fallback(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    // Login to get a session token
    let (_, body) = helpers::post_json(
        &app,
        "",
        "/api/auth/login",
        serde_json::json!({ "name": "admin", "password": "testpassword" }),
    )
    .await;
    let session_token = body["token"].as_str().unwrap();

    // Use session token as Bearer (not in api_tokens, found in auth_sessions)
    let (status, body) = helpers::get_json(&app, session_token, "/api/auth/me").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "admin");
}

// ---------------------------------------------------------------------------
// API token: last_used_at gets updated
// ---------------------------------------------------------------------------

/// Using an API token should update its last_used_at.
#[sqlx::test(migrations = "./migrations")]
async fn api_token_updates_last_used_at(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let admin_token = helpers::admin_login(&app).await;
    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let user_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    // Create an API token directly
    let raw_token = format!(
        "plat_api_lastused_{}",
        Uuid::new_v4().to_string().replace('-', "")
    );
    let token_hash = platform::auth::token::hash_token(&raw_token);
    let expires_at = chrono::Utc::now() + chrono::Duration::days(30);

    let token_id: Uuid = sqlx::query_as(
        "INSERT INTO api_tokens (user_id, name, token_hash, expires_at, scopes)
         VALUES ($1, 'lastused-token', $2, $3, ARRAY[]::text[])
         RETURNING id",
    )
    .bind(user_id)
    .bind(&token_hash)
    .bind(expires_at)
    .fetch_one(&pool)
    .await
    .map(|r: (Uuid,)| r.0)
    .unwrap();

    // Verify last_used_at is NULL initially
    let row: (Option<chrono::DateTime<chrono::Utc>>,) =
        sqlx::query_as("SELECT last_used_at FROM api_tokens WHERE id = $1")
            .bind(token_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(row.0.is_none());

    // Use the token
    let (status, _) = helpers::get_json(&app, &raw_token, "/api/auth/me").await;
    assert_eq!(status, StatusCode::OK);

    // Wait a bit for the fire-and-forget update to complete
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Verify last_used_at is now set
    let row: (Option<chrono::DateTime<chrono::Utc>>,) =
        sqlx::query_as("SELECT last_used_at FROM api_tokens WHERE id = $1")
            .bind(token_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        row.0.is_some(),
        "last_used_at should be set after token use"
    );
}

// ---------------------------------------------------------------------------
// API token scopes are preserved in AuthUser
// ---------------------------------------------------------------------------

/// Scoped API token should have token_scopes set.
/// We verify indirectly by creating a scoped token and attempting an action.
#[sqlx::test(migrations = "./migrations")]
async fn scoped_api_token_authenticates(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let admin_token = helpers::admin_login(&app).await;

    // Create a scoped API token via the API
    let (status, create_body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/tokens",
        serde_json::json!({
            "name": "scoped-token",
            "scopes": ["project:read"],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let scoped_token = create_body["token"].as_str().unwrap();

    // Scoped token should authenticate for /api/auth/me
    let (status, body) = helpers::get_json(&app, scoped_token, "/api/auth/me").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "admin");
}

// ---------------------------------------------------------------------------
// Cookie with other cookies present
// ---------------------------------------------------------------------------

/// Session cookie extraction works when mixed with other cookies.
#[sqlx::test(migrations = "./migrations")]
async fn session_cookie_among_other_cookies(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    // Login to get a session token
    let (_, body) = helpers::post_json(
        &app,
        "",
        "/api/auth/login",
        serde_json::json!({ "name": "admin", "password": "testpassword" }),
    )
    .await;
    let session_token = body["token"].as_str().unwrap();

    // Send with session cookie among other cookies
    let req = Request::builder()
        .method("GET")
        .uri("/api/auth/me")
        .header(
            "Cookie",
            format!("theme=dark; session={session_token}; lang=en"),
        )
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// Cookie header present but no "session=" cookie falls through to 401.
#[sqlx::test(migrations = "./migrations")]
async fn cookie_header_without_session_returns_401(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let req = Request::builder()
        .method("GET")
        .uri("/api/auth/me")
        .header("Cookie", "theme=dark; lang=en")
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Logout invalidation
// ---------------------------------------------------------------------------

/// After logout, the session token should no longer be valid.
#[sqlx::test(migrations = "./migrations")]
async fn logout_invalidates_session(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let token = helpers::admin_login(&app).await;

    // Verify token works
    let (status, _) = helpers::get_json(&app, &token, "/api/auth/me").await;
    assert_eq!(status, StatusCode::OK);

    // Logout
    let (status, _) =
        helpers::post_json(&app, &token, "/api/auth/logout", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::OK);

    // Token should no longer work (Bearer path: not in api_tokens, session deleted)
    let (status, _) = helpers::get_json(&app, &token, "/api/auth/me").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Cookie path should also fail
    let (status, _) = get_with_cookie(&app, &token, "/api/auth/me").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
