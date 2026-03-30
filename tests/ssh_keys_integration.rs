mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

// Real test keys (generated with ssh-keygen, used only for testing)
const TEST_ED25519_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKjB6KC6pSWW2pW828DmK4uouTNB2a0nJQx0qLZW+2++ test@example.com";
const TEST_RSA_4096_KEY: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAACAQD0r4jcO3nQBNRQvnCCDco9DYcn32asUYJGRx7OXuirMA1+HZtMUTM/wAwpXa4huPmajhmK8Lh5gpQmeyvdZx9r55lmiH1RJp73y5iC6HXWMgLJ6Q9Q9D/gHxc7Te9hyoayJsGUzS/fvxeaaONJKC/OiMBmeHXnrJcPzUGek/UJtz7bXFRdBTXKMb03hMR0VYieXe4Uh31XO8O/2+lLfpUad8qsgbxAa22Ga/S6BLTFe0IYqrrM44LIcVcIW5ODvwM5UAU4ohZkP43JjfS4sQVyPv8XOD/546giFyB6kUGXj0/sxIZ5iNEbNBsRMamTfkbvbunMUW5nc9XkKI+YxNrnzRE573t1+ePyZZ3fMhTDMfkLymZju3cH2ICEHQIqcAXh4CBqaajbkrZ7goVbdWmtGyMGWPtQLeUTc30WgKiqAswr4u69ekN4RfOcV7SGIASSOW5cH40bCfOxTjT6XlfkrxpFtB2Pmb/ldmkLfV3w531JY+mdOPNVBFXwGn5kW2V5Ihz9XL1MIGohPV9kIHAMLbL08ZXZJFt2uaZeBhfJJz1idAC6d7qrNF0hwzm1AVJrTvp+vkK9rGlbHSvAGdpv2um3Q4trqxz1E5ikl3Lv8kAQbo6TeWvpD6xXRbYRxmyvPm+xSBEn++kx+rOeGgwBz7ccAcbN8+vzVIXEMfQIiQ== test@example.com";
const TEST_RSA_1024_KEY: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAAAgQDZg0pevgw1D7sN+GUm+gHWCj+rF9voGPIkv+Au9MqYdxOR7YRC6Mh87I7v7WOyVSw2ByyI482Cr6nayq4D6dxRfpUCi1jfq+BytStZjyZYqt7bq5UfTEC2tZaQY8E17izcDhOU1EFAfwEvBGO0U8nwuIQT6+1OKmKDkantymX69w== test@example.com";
#[sqlx::test(migrations = "./migrations")]
async fn test_add_ssh_key_success(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users/me/ssh-keys",
        serde_json::json!({
            "name": "my-laptop",
            "public_key": TEST_ED25519_KEY,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    assert_eq!(body["name"], "my-laptop");
    assert_eq!(body["algorithm"], "ssh-ed25519");
    assert!(body["fingerprint"].as_str().unwrap().starts_with("SHA256:"));
    assert!(body["id"].as_str().is_some());
}

#[sqlx::test(migrations = "./migrations")]
async fn test_list_ssh_keys_empty(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/users/me/ssh-keys").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn test_list_ssh_keys_with_data(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Add a key
    helpers::post_json(
        &app,
        &admin_token,
        "/api/users/me/ssh-keys",
        serde_json::json!({
            "name": "test-key",
            "public_key": TEST_ED25519_KEY,
        }),
    )
    .await;

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/users/me/ssh-keys").await;
    assert_eq!(status, StatusCode::OK);
    let keys = body["items"].as_array().unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0]["name"], "test-key");
}

#[sqlx::test(migrations = "./migrations")]
async fn test_delete_ssh_key_success(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (_, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users/me/ssh-keys",
        serde_json::json!({
            "name": "to-delete",
            "public_key": TEST_ED25519_KEY,
        }),
    )
    .await;
    let key_id = body["id"].as_str().unwrap();

    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/users/me/ssh-keys/{key_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Verify it's gone
    let (status, body) = helpers::get_json(&app, &admin_token, "/api/users/me/ssh-keys").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn test_delete_ssh_key_not_found(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let fake_id = Uuid::new_v4();
    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/users/me/ssh-keys/{fake_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn test_delete_other_users_key_returns_not_found(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    // Add key as admin
    let (_, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users/me/ssh-keys",
        serde_json::json!({
            "name": "admin-key",
            "public_key": TEST_ED25519_KEY,
        }),
    )
    .await;
    let key_id = body["id"].as_str().unwrap();

    // Create another user and try to delete admin's key
    let (_user_id, user_token) =
        helpers::create_user(&app, &admin_token, "other", "other@test.com").await;

    let (status, _) = helpers::delete_json(
        &app,
        &user_token,
        &format!("/api/users/me/ssh-keys/{key_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn test_add_duplicate_fingerprint_returns_conflict(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Add key first time
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users/me/ssh-keys",
        serde_json::json!({
            "name": "first",
            "public_key": TEST_ED25519_KEY,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Add same key again
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users/me/ssh-keys",
        serde_json::json!({
            "name": "second",
            "public_key": TEST_ED25519_KEY,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[sqlx::test(migrations = "./migrations")]
async fn test_add_ssh_key_invalid_key_returns_400(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users/me/ssh-keys",
        serde_json::json!({
            "name": "bad-key",
            "public_key": "this is not a valid ssh key at all but long enough to pass length check",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn test_add_ssh_key_name_too_long_returns_400(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users/me/ssh-keys",
        serde_json::json!({
            "name": "a".repeat(256),
            "public_key": TEST_ED25519_KEY,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn test_add_ssh_key_empty_name_returns_400(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users/me/ssh-keys",
        serde_json::json!({
            "name": "",
            "public_key": TEST_ED25519_KEY,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn test_add_ssh_key_public_key_too_short_returns_400(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users/me/ssh-keys",
        serde_json::json!({
            "name": "short",
            "public_key": "ssh-ed25519 AAAA",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn test_add_ssh_key_public_key_too_long_returns_400(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users/me/ssh-keys",
        serde_json::json!({
            "name": "long",
            "public_key": "a".repeat(16385),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn test_add_ssh_key_max_50_limit(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    // Insert 50 keys directly into DB (much faster than 50 API calls)
    let admin_row: (Uuid,) = sqlx::query_as("SELECT id FROM users WHERE name = 'admin'")
        .fetch_one(&pool)
        .await
        .unwrap();

    for i in 0..50 {
        sqlx::query(
            "INSERT INTO user_ssh_keys (user_id, name, algorithm, fingerprint, public_key_openssh)
             VALUES ($1, $2, 'ssh-ed25519', $3, 'ssh-ed25519 AAAA')",
        )
        .bind(admin_row.0)
        .bind(format!("key-{i}"))
        .bind(format!("SHA256:fake{i}"))
        .execute(&pool)
        .await
        .unwrap();
    }

    // 51st key via API should fail
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users/me/ssh-keys",
        serde_json::json!({
            "name": "one-too-many",
            "public_key": TEST_ED25519_KEY,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
}

#[sqlx::test(migrations = "./migrations")]
async fn test_list_ssh_keys_unauthenticated(pool: PgPool) {
    let (state, _) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::get_json(&app, "", "/api/users/me/ssh-keys").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn test_add_ssh_key_unauthenticated(pool: PgPool) {
    let (state, _) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::post_json(
        &app,
        "",
        "/api/users/me/ssh-keys",
        serde_json::json!({
            "name": "test",
            "public_key": TEST_ED25519_KEY,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn test_delete_ssh_key_unauthenticated(pool: PgPool) {
    let (state, _) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let fake_id = Uuid::new_v4();
    let (status, _) =
        helpers::delete_json(&app, "", &format!("/api/users/me/ssh-keys/{fake_id}")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn test_admin_list_user_ssh_keys(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    // Create a user and add a key
    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "sshuser", "sshuser@test.com").await;

    helpers::post_json(
        &app,
        &user_token,
        "/api/users/me/ssh-keys",
        serde_json::json!({
            "name": "user-key",
            "public_key": TEST_ED25519_KEY,
        }),
    )
    .await;

    // Admin can see user's keys
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/admin/users/{user_id}/ssh-keys"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let keys = body["items"].as_array().unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0]["name"], "user-key");
}

#[sqlx::test(migrations = "./migrations")]
async fn test_admin_list_ssh_keys_non_admin_denied(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (user_id, user_token) =
        helpers::create_user(&app, &admin_token, "nonadmin", "nonadmin@test.com").await;

    let (status, _) = helpers::get_json(
        &app,
        &user_token,
        &format!("/api/admin/users/{user_id}/ssh-keys"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "./migrations")]
async fn test_add_ssh_key_creates_audit_log(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    helpers::post_json(
        &app,
        &admin_token,
        "/api/users/me/ssh-keys",
        serde_json::json!({
            "name": "audit-test",
            "public_key": TEST_ED25519_KEY,
        }),
    )
    .await;

    // Audit entries are written asynchronously — poll until visible
    let count = helpers::wait_for_audit(&pool, "ssh_key.add", 2000).await;
    assert!(count > 0, "expected audit log entry for ssh_key.add");
}

#[sqlx::test(migrations = "./migrations")]
async fn test_delete_ssh_key_creates_audit_log(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    let (_, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users/me/ssh-keys",
        serde_json::json!({
            "name": "audit-del",
            "public_key": TEST_ED25519_KEY,
        }),
    )
    .await;
    let key_id = body["id"].as_str().unwrap();

    helpers::delete_json(
        &app,
        &admin_token,
        &format!("/api/users/me/ssh-keys/{key_id}"),
    )
    .await;

    // Audit entries are written asynchronously — poll until visible
    let count = helpers::wait_for_audit(&pool, "ssh_key.delete", 2000).await;
    assert!(count > 0, "expected audit log entry for ssh_key.delete");
}

#[sqlx::test(migrations = "./migrations")]
async fn test_list_ssh_keys_only_own_keys(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Add key as admin
    helpers::post_json(
        &app,
        &admin_token,
        "/api/users/me/ssh-keys",
        serde_json::json!({
            "name": "admin-key",
            "public_key": TEST_ED25519_KEY,
        }),
    )
    .await;

    // Create another user — they should see 0 keys
    let (_user_id, user_token) =
        helpers::create_user(&app, &admin_token, "other2", "other2@test.com").await;

    let (status, body) = helpers::get_json(&app, &user_token, "/api/users/me/ssh-keys").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn test_add_ssh_key_rsa_4096(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users/me/ssh-keys",
        serde_json::json!({
            "name": "rsa-key",
            "public_key": TEST_RSA_4096_KEY,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    assert_eq!(body["algorithm"], "ssh-rsa");
}

#[sqlx::test(migrations = "./migrations")]
async fn test_add_ssh_key_rsa_1024_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users/me/ssh-keys",
        serde_json::json!({
            "name": "weak-rsa",
            "public_key": TEST_RSA_1024_KEY,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
}

#[sqlx::test(migrations = "./migrations")]
async fn test_admin_list_ssh_keys_unauthenticated(pool: PgPool) {
    let (state, _) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let fake_user_id = Uuid::new_v4();
    let (status, _) = helpers::get_json(
        &app,
        "bad-token",
        &format!("/api/admin/users/{fake_user_id}/ssh-keys"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// GET /api/users/me/ssh-keys/{id} — individual key retrieval
// ---------------------------------------------------------------------------

/// Successfully retrieve a single SSH key by ID.
#[sqlx::test(migrations = "./migrations")]
async fn test_get_ssh_key_success(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Add key first
    let (status, created) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users/me/ssh-keys",
        serde_json::json!({
            "name": "get-test-key",
            "public_key": TEST_ED25519_KEY,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let key_id = created["id"].as_str().unwrap();

    // Retrieve it
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/users/me/ssh-keys/{key_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"].as_str().unwrap(), key_id);
    assert_eq!(body["name"].as_str().unwrap(), "get-test-key");
    assert_eq!(body["algorithm"].as_str().unwrap(), "ssh-ed25519");
    assert!(body["fingerprint"].is_string());
    assert!(body["created_at"].is_string());
    assert!(body["last_used_at"].is_null());
}

/// GET for a nonexistent SSH key returns 404.
#[sqlx::test(migrations = "./migrations")]
async fn test_get_ssh_key_not_found(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let fake_id = Uuid::new_v4();
    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/users/me/ssh-keys/{fake_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// GET another user's SSH key returns 404 (user_id filter).
#[sqlx::test(migrations = "./migrations")]
async fn test_get_ssh_key_other_user_returns_not_found(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    // Admin adds a key
    let (status, created) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users/me/ssh-keys",
        serde_json::json!({
            "name": "admin-key",
            "public_key": TEST_ED25519_KEY,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let key_id = created["id"].as_str().unwrap();

    // Create a different user
    let (_other_id, other_token) =
        helpers::create_user(&app, &admin_token, "ssh-other-get", "sshother-get@test.com").await;

    // Other user cannot retrieve admin's key
    let (status, _) = helpers::get_json(
        &app,
        &other_token,
        &format!("/api/users/me/ssh-keys/{key_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// GET SSH key unauthenticated returns 401.
#[sqlx::test(migrations = "./migrations")]
async fn test_get_ssh_key_unauthenticated(pool: PgPool) {
    let (state, _) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let fake_id = Uuid::new_v4();
    let (status, _) = helpers::get_json(
        &app,
        "bad-token",
        &format!("/api/users/me/ssh-keys/{fake_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Admin list SSH keys for nonexistent user
// ---------------------------------------------------------------------------

/// Admin listing SSH keys for a nonexistent user returns empty list (not 404).
#[sqlx::test(migrations = "./migrations")]
async fn test_admin_list_ssh_keys_nonexistent_user(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let fake_user_id = Uuid::new_v4();
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/admin/users/{fake_user_id}/ssh-keys"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"].as_i64().unwrap(), 0);
    assert!(body["items"].as_array().unwrap().is_empty());
}

/// Deleting a key that doesn't belong to the user returns 404 and key still exists.
#[sqlx::test(migrations = "./migrations")]
async fn test_delete_ssh_key_wrong_user_key_persists(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    // Admin adds a key
    let (status, created) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users/me/ssh-keys",
        serde_json::json!({
            "name": "persist-key",
            "public_key": TEST_ED25519_KEY,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let key_id = created["id"].as_str().unwrap();

    // Create another user
    let (_other_id, other_token) =
        helpers::create_user(&app, &admin_token, "ssh-delwrong", "ssh-delwrong@test.com").await;

    // Other user tries to delete admin's key - should get 404
    let (status, _) = helpers::delete_json(
        &app,
        &other_token,
        &format!("/api/users/me/ssh-keys/{key_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Key should still be visible to admin
    let (status, body) = helpers::get_json(
        &app,
        &admin_token,
        &format!("/api/users/me/ssh-keys/{key_id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"].as_str().unwrap(), "persist-key");
}
