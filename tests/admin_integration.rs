mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;

// ---------------------------------------------------------------------------
// E3: Admin Integration Tests (14 tests)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn admin_create_user(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users",
        serde_json::json!({
            "name": "newuser",
            "email": "new@test.com",
            "password": "securepassword",
            "display_name": "New User",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert!(body["id"].is_string());
    assert_eq!(body["name"], "newuser");
    assert_eq!(body["email"], "new@test.com");
    assert_eq!(body["display_name"], "New User");
    assert_eq!(body["is_active"], true);
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_create_user_duplicate_name(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    helpers::create_user(&app, &admin_token, "dupeuser", "dupe@test.com").await;

    // Try again with the same name
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users",
        serde_json::json!({
            "name": "dupeuser",
            "email": "dupe2@test.com",
            "password": "securepassword",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_create_user_invalid_email(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users",
        serde_json::json!({
            "name": "bademail",
            "email": "not-an-email",
            "password": "securepassword",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_create_user_short_password(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users",
        serde_json::json!({
            "name": "shortpw",
            "email": "shortpw@test.com",
            "password": "abc",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_list_users(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    helpers::create_user(&app, &admin_token, "listuser1", "list1@test.com").await;
    helpers::create_user(&app, &admin_token, "listuser2", "list2@test.com").await;

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/users/list").await;

    assert_eq!(status, StatusCode::OK);
    assert!(body["total"].as_i64().unwrap() >= 3); // admin + 2 users
    assert!(body["items"].as_array().unwrap().len() >= 3);
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_get_user_by_id(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (user_id, _) = helpers::create_user(&app, &admin_token, "getuser", "get@test.com").await;

    let (status, body) =
        helpers::get_json(&app, &admin_token, &format!("/api/users/{user_id}")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "getuser");
    assert_eq!(body["email"], "get@test.com");
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_update_user(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (user_id, _) =
        helpers::create_user(&app, &admin_token, "updateuser", "update@test.com").await;

    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/users/{user_id}"),
        serde_json::json!({ "display_name": "Updated Name" }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["display_name"], "Updated Name");
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_deactivate_user(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (user_id, _) = helpers::create_user(&app, &admin_token, "deacuser", "deac@test.com").await;

    let (status, _) =
        helpers::delete_json(&app, &admin_token, &format!("/api/users/{user_id}")).await;
    assert_eq!(status, StatusCode::OK);

    // Verify user is inactive
    let (status, body) =
        helpers::get_json(&app, &admin_token, &format!("/api/users/{user_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["is_active"], false);
}

#[sqlx::test(migrations = "./migrations")]
async fn deactivated_user_cannot_login(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (user_id, _) =
        helpers::create_user(&app, &admin_token, "nologin", "nologin@test.com").await;

    // Deactivate
    helpers::delete_json(&app, &admin_token, &format!("/api/users/{user_id}")).await;

    // Try login
    let (status, _) = helpers::post_json(
        &app,
        "",
        "/api/auth/login",
        serde_json::json!({ "name": "nologin", "password": "testpass123" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn deactivated_user_token_revoked(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "tokenrevoke", "tokenrev@test.com").await;

    // Verify token works
    let (status, _) = helpers::get_json(&app, &user_token, "/api/auth/me").await;
    assert_eq!(status, StatusCode::OK);

    // Deactivate user (this should revoke sessions)
    helpers::delete_json(&app, &admin_token, &format!("/api/users/{user_id}")).await;

    // Token should no longer work
    let (status, _) = helpers::get_json(&app, &user_token, "/api/auth/me").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn non_admin_cannot_create_user(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (_, user_token) =
        helpers::create_user(&app, &admin_token, "regular", "regular@test.com").await;

    let (status, _) = helpers::post_json(
        &app,
        &user_token,
        "/api/users",
        serde_json::json!({
            "name": "shouldfail",
            "email": "fail@test.com",
            "password": "securepassword",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "./migrations")]
async fn non_admin_cannot_list_users(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    let (_, user_token) =
        helpers::create_user(&app, &admin_token, "nolist", "nolist@test.com").await;

    let (status, _) = helpers::get_json(&app, &user_token, "/api/users/list").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_create_token_for_user(pool: PgPool) {
    let state = helpers::test_state(pool).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    // Create a service account with auto-token
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/admin/service-accounts",
        serde_json::json!({
            "name": "sa-with-token",
            "email": "sa@test.com",
            "scopes": ["project:read"],
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert!(
        body["token"].is_object(),
        "service account should have a token: {body}"
    );
    assert!(
        body["token"]["token"]
            .as_str()
            .unwrap()
            .starts_with("plat_")
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_actions_create_audit_log(pool: PgPool) {
    let state = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);
    let admin_token = helpers::admin_login(&app).await;

    helpers::create_user(&app, &admin_token, "audituser", "audit@test.com").await;

    // Query audit_log directly
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM audit_log WHERE action = 'user.create'")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert!(row.0 >= 1, "expected audit_log entry for user.create");
}
