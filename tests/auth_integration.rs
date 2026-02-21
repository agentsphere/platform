mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;

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
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Send 11 rapid login attempts — rate limit is 10 per 5min
    let mut got_429 = false;
    for i in 0..12 {
        let (status, _) = helpers::post_json(
            &app,
            "",
            "/api/auth/login",
            serde_json::json!({ "name": "admin", "password": format!("wrong{i}") }),
        )
        .await;

        if status == StatusCode::TOO_MANY_REQUESTS {
            got_429 = true;
            break;
        }
    }

    assert!(got_429, "expected 429 after exceeding rate limit");
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
