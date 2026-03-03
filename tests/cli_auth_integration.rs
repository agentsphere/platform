mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;

// ---------------------------------------------------------------------------
// Store credentials
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn test_store_setup_token_credentials(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/auth/cli-credentials",
        serde_json::json!({
            "auth_type": "setup_token",
            "token": "sk-ant-ccode01-abc123"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "store failed: {body}");
    assert_eq!(body["exists"], true);
    assert_eq!(body["auth_type"], "setup_token");
}

#[sqlx::test(migrations = "./migrations")]
async fn test_store_oauth_credentials(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let oauth_json = serde_json::json!({
        "access_token": "at-123",
        "refresh_token": "rt-456",
        "expires_at": "2026-12-31T23:59:59Z"
    });

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/auth/cli-credentials",
        serde_json::json!({
            "auth_type": "oauth",
            "token": oauth_json.to_string(),
            "token_expires_at": "2026-12-31T23:59:59Z"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "store oauth failed: {body}");
    assert_eq!(body["exists"], true);
    assert_eq!(body["auth_type"], "oauth");
}

// ---------------------------------------------------------------------------
// Get credentials (existence check)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn test_get_credentials_returns_existence_only(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Store first
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/auth/cli-credentials",
        serde_json::json!({
            "auth_type": "setup_token",
            "token": "my-secret-token-value"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // GET — should return existence info, no secrets
    let (status, body) = helpers::get_json(&app, &admin_token, "/api/auth/cli-credentials").await;
    assert_eq!(status, StatusCode::OK, "get failed: {body}");
    assert_eq!(body["exists"], true);
    assert_eq!(body["auth_type"], "setup_token");
    // Must NOT contain the actual token
    assert!(body.get("token").is_none(), "token must not be returned");
    assert!(
        !body.to_string().contains("my-secret-token-value"),
        "actual token value must never appear in response"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn test_get_credentials_not_found(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/auth/cli-credentials").await;
    assert_eq!(status, StatusCode::OK, "get failed: {body}");
    assert_eq!(body["exists"], false);
}

// ---------------------------------------------------------------------------
// Delete credentials
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn test_delete_credentials(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Store first
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/auth/cli-credentials",
        serde_json::json!({
            "auth_type": "setup_token",
            "token": "token-to-delete"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Delete
    let (status, _) = helpers::delete_json(&app, &admin_token, "/api/auth/cli-credentials").await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Verify gone
    let (status, body) = helpers::get_json(&app, &admin_token, "/api/auth/cli-credentials").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["exists"], false);
}

#[sqlx::test(migrations = "./migrations")]
async fn test_delete_credentials_idempotent(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Delete when nothing stored — should still return 204
    let (status, _) = helpers::delete_json(&app, &admin_token, "/api/auth/cli-credentials").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

// ---------------------------------------------------------------------------
// Upsert
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn test_store_credentials_upsert(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Store initial
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/auth/cli-credentials",
        serde_json::json!({
            "auth_type": "setup_token",
            "token": "first-token"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Upsert with new value
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/auth/cli-credentials",
        serde_json::json!({
            "auth_type": "setup_token",
            "token": "second-token"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "upsert failed: {body}");
    assert_eq!(body["auth_type"], "setup_token");
}

// ---------------------------------------------------------------------------
// Auth required
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn test_store_credentials_requires_auth(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::post_json(
        &app,
        "",
        "/api/auth/cli-credentials",
        serde_json::json!({
            "auth_type": "setup_token",
            "token": "no-auth-token"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn test_get_credentials_requires_auth(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::get_json(&app, "", "/api/auth/cli-credentials").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn test_store_credentials_validates_auth_type(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/auth/cli-credentials",
        serde_json::json!({
            "auth_type": "api_key",
            "token": "some-token"
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "invalid auth_type should 400: {body}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn test_empty_token_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/auth/cli-credentials",
        serde_json::json!({
            "auth_type": "setup_token",
            "token": ""
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Whitespace-only should also be rejected
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/auth/cli-credentials",
        serde_json::json!({
            "auth_type": "setup_token",
            "token": "   "
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Audit logging
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn test_store_credentials_audit_logged(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/auth/cli-credentials",
        serde_json::json!({
            "auth_type": "setup_token",
            "token": "audit-test-token"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Check audit_log
    let row: (String,) = sqlx::query_as(
        "SELECT action FROM audit_log WHERE action = 'cli_creds.store' ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("audit log entry should exist");
    assert_eq!(row.0, "cli_creds.store");
}

#[sqlx::test(migrations = "./migrations")]
async fn test_delete_credentials_audit_logged(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    // Store first
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/auth/cli-credentials",
        serde_json::json!({
            "auth_type": "setup_token",
            "token": "to-delete-for-audit"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Delete
    let (status, _) = helpers::delete_json(&app, &admin_token, "/api/auth/cli-credentials").await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Check audit_log
    let row: (String,) = sqlx::query_as(
        "SELECT action FROM audit_log WHERE action = 'cli_creds.delete' ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("delete audit log entry should exist");
    assert_eq!(row.0, "cli_creds.delete");
}

// ---------------------------------------------------------------------------
// Cascade on user delete
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn test_credentials_cascade_on_user_delete(pool: PgPool) {
    let (_state, _admin_token) = helpers::test_state(pool.clone()).await;

    // Create a throwaway user with no workspace/project ownership
    let user_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, name, display_name, email, password_hash, is_active)
         VALUES ($1, 'cascade-user', 'Cascade User', 'cascade@test.com', 'dummy', true)",
    )
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    // Insert a credential row directly
    sqlx::query(
        "INSERT INTO cli_credentials (user_id, auth_type, encrypted_data)
         VALUES ($1, 'setup_token', $2)",
    )
    .bind(user_id)
    .bind(vec![0u8; 28]) // dummy encrypted data (12 nonce + 16 tag)
    .execute(&pool)
    .await
    .unwrap();

    // Verify it exists
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM cli_credentials WHERE user_id = $1")
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count.0, 1);

    // Hard-delete the user — ON DELETE CASCADE should remove credentials
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();

    // Verify credentials are cascade-deleted
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM cli_credentials WHERE user_id = $1")
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        count.0, 0,
        "credentials should be cascade-deleted when user is deleted"
    );
}

// ---------------------------------------------------------------------------
// User isolation
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn test_user_cannot_read_other_user_creds(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Store credentials for admin
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/auth/cli-credentials",
        serde_json::json!({
            "auth_type": "setup_token",
            "token": "admin-secret-token"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Create second user
    let (_user_id, user_token) =
        helpers::create_user(&app, &admin_token, "other-user", "other@test.com").await;

    // Second user checks their own creds — should see nothing
    let (status, body) = helpers::get_json(&app, &user_token, "/api/auth/cli-credentials").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["exists"], false,
        "user should not see other user's creds"
    );
}
