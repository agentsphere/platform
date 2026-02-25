//! Integration tests for passkey (WebAuthn) credential management.
//! Tests list, rename, delete, and begin_register/begin_login endpoints.
//! NOTE: complete_register and complete_login require a real WebAuthn ceremony
//! which can't be simulated in integration tests.

mod helpers;

use axum::http::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

use helpers::{admin_login, create_user, test_router, test_state};

// ---------------------------------------------------------------------------
// List passkeys
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_passkeys_empty(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/auth/passkeys").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn list_passkeys_with_data(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let user_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    // Insert a fake passkey credential directly
    sqlx::query(
        "INSERT INTO passkey_credentials (user_id, credential_id, public_key, name, transports)
         VALUES ($1, $2, $3, 'Test Key', ARRAY['usb']::text[])",
    )
    .bind(user_id)
    .bind(vec![1u8, 2, 3, 4])
    .bind(serde_json::to_vec(&serde_json::json!({"type": "public-key"})).unwrap())
    .execute(&pool)
    .await
    .unwrap();

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/auth/passkeys").await;
    assert_eq!(status, StatusCode::OK);
    let keys = body.as_array().unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0]["name"], "Test Key");
    assert_eq!(keys[0]["transports"][0], "usb");
}

// ---------------------------------------------------------------------------
// Delete passkey
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn delete_passkey_success(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let user_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let cred_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO passkey_credentials (id, user_id, credential_id, public_key, name, transports)
         VALUES ($1, $2, $3, $4, 'DeleteMe', ARRAY[]::text[])",
    )
    .bind(cred_id)
    .bind(user_id)
    .bind(vec![5u8, 6, 7, 8])
    .bind(serde_json::to_vec(&serde_json::json!({"type": "public-key"})).unwrap())
    .execute(&pool)
    .await
    .unwrap();

    let (status, body) =
        helpers::delete_json(&app, &admin_token, &format!("/api/auth/passkeys/{cred_id}")).await;
    assert_eq!(status, StatusCode::OK, "delete failed: {body}");

    // Verify gone
    let (status, body) = helpers::get_json(&app, &admin_token, "/api/auth/passkeys").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn delete_passkey_not_found(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let fake_id = Uuid::new_v4();
    let (status, _) =
        helpers::delete_json(&app, &admin_token, &format!("/api/auth/passkeys/{fake_id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn delete_passkey_other_users_key_returns_not_found(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (other_user_id, _other_token) =
        create_user(&app, &admin_token, "pk-other", "pkother@test.com").await;

    // Insert credential owned by other user
    let cred_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO passkey_credentials (id, user_id, credential_id, public_key, name, transports)
         VALUES ($1, $2, $3, $4, 'OtherKey', ARRAY[]::text[])",
    )
    .bind(cred_id)
    .bind(other_user_id)
    .bind(vec![10u8, 11, 12])
    .bind(serde_json::to_vec(&serde_json::json!({"type": "public-key"})).unwrap())
    .execute(&pool)
    .await
    .unwrap();

    // Admin can't delete other user's passkey (scoped by user_id)
    let (status, _) =
        helpers::delete_json(&app, &admin_token, &format!("/api/auth/passkeys/{cred_id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Rename passkey
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn rename_passkey_success(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let user_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let cred_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO passkey_credentials (id, user_id, credential_id, public_key, name, transports)
         VALUES ($1, $2, $3, $4, 'OldName', ARRAY[]::text[])",
    )
    .bind(cred_id)
    .bind(user_id)
    .bind(vec![20u8, 21, 22])
    .bind(serde_json::to_vec(&serde_json::json!({"type": "public-key"})).unwrap())
    .execute(&pool)
    .await
    .unwrap();

    let (status, body) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/auth/passkeys/{cred_id}"),
        serde_json::json!({"name": "NewName"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "rename failed: {body}");

    // Verify name changed
    let (_, keys) = helpers::get_json(&app, &admin_token, "/api/auth/passkeys").await;
    assert_eq!(keys[0]["name"], "NewName");
}

#[sqlx::test(migrations = "./migrations")]
async fn rename_passkey_not_found(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let fake_id = Uuid::new_v4();
    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/auth/passkeys/{fake_id}"),
        serde_json::json!({"name": "Won'tWork"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Begin register (returns challenge)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn begin_register_returns_challenge(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/auth/passkeys/register/begin",
        serde_json::json!({"name": "My YubiKey"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "begin_register failed: {body}");

    // Response should contain WebAuthn challenge fields
    assert!(
        body.get("publicKey").is_some() || body.get("rp").is_some(),
        "response should contain WebAuthn challenge data: {body}"
    );
}

// ---------------------------------------------------------------------------
// Begin login (returns challenge)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn begin_login_returns_challenge(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);

    // begin_login is unauthenticated
    let (status, body) = helpers::post_json(
        &app,
        "",
        "/api/auth/passkey/login/begin",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "begin_login failed: {body}");
    assert!(
        body.get("challenge").is_some(),
        "response should contain challenge: {body}"
    );
    assert!(
        body.get("challenge_id").is_some(),
        "response should contain challenge_id: {body}"
    );
}

// ---------------------------------------------------------------------------
// Additional passkey coverage tests
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn complete_login_invalid_challenge_id(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);

    // Insert a passkey credential so the discoverable keys list isn't empty.
    // First we need a user.
    let admin_token = admin_login(&app).await;
    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let user_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    sqlx::query(
        "INSERT INTO passkey_credentials (user_id, credential_id, public_key, name, transports)
         VALUES ($1, $2, $3, 'LoginKey', ARRAY[]::text[])",
    )
    .bind(user_id)
    .bind(vec![30u8, 31, 32, 33])
    .bind(serde_json::to_vec(&serde_json::json!({"type": "public-key"})).unwrap())
    .execute(&pool)
    .await
    .unwrap();

    // Try complete_login with a bogus challenge_id (no matching Valkey state)
    let (status, _) = helpers::post_json(
        &app,
        "",
        "/api/auth/passkey/login/complete",
        serde_json::json!({
            "challenge_id": "nonexistent-challenge-id",
            "credential": {
                "id": "dGVzdA",
                "rawId": "dGVzdA",
                "type": "public-key",
                "response": {
                    "authenticatorData": "dGVzdA",
                    "clientDataJSON": "dGVzdA",
                    "signature": "dGVzdA"
                }
            }
        }),
    )
    .await;

    // Should fail with 401 (invalid challenge)
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn rename_passkey_empty_name_rejected(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let user_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let cred_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO passkey_credentials (id, user_id, credential_id, public_key, name, transports)
         VALUES ($1, $2, $3, $4, 'ToRename', ARRAY[]::text[])",
    )
    .bind(cred_id)
    .bind(user_id)
    .bind(vec![40u8, 41, 42])
    .bind(serde_json::to_vec(&serde_json::json!({"type": "public-key"})).unwrap())
    .execute(&pool)
    .await
    .unwrap();

    // Empty name should be rejected (min length 1)
    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/auth/passkeys/{cred_id}"),
        serde_json::json!({"name": ""}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn rename_passkey_name_too_long(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let user_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let cred_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO passkey_credentials (id, user_id, credential_id, public_key, name, transports)
         VALUES ($1, $2, $3, $4, 'ShortName', ARRAY[]::text[])",
    )
    .bind(cred_id)
    .bind(user_id)
    .bind(vec![50u8, 51, 52])
    .bind(serde_json::to_vec(&serde_json::json!({"type": "public-key"})).unwrap())
    .execute(&pool)
    .await
    .unwrap();

    let long_name = "x".repeat(256);
    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/auth/passkeys/{cred_id}"),
        serde_json::json!({"name": long_name}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn rename_other_users_passkey_not_found(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (other_user_id, _other_token) =
        create_user(&app, &admin_token, "pk-rename-other", "pkrename@test.com").await;

    // Insert credential owned by the other user
    let cred_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO passkey_credentials (id, user_id, credential_id, public_key, name, transports)
         VALUES ($1, $2, $3, $4, 'OtherUserKey', ARRAY[]::text[])",
    )
    .bind(cred_id)
    .bind(other_user_id)
    .bind(vec![60u8, 61, 62])
    .bind(serde_json::to_vec(&serde_json::json!({"type": "public-key"})).unwrap())
    .execute(&pool)
    .await
    .unwrap();

    // Admin tries to rename other user's passkey — should get 404 (scoped by user_id)
    let (status, _) = helpers::patch_json(
        &app,
        &admin_token,
        &format!("/api/auth/passkeys/{cred_id}"),
        serde_json::json!({"name": "Hijacked"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn begin_register_service_account_rejected(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let _admin_token = admin_login(&app).await;

    // Create a service account user directly in DB
    let svc_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, name, email, password_hash, user_type, is_active)
         VALUES ($1, $2, $3, $4, 'service_account', true)",
    )
    .bind(svc_id)
    .bind("svc-passkey-test")
    .bind("svc-passkey@test.com")
    .bind("not-a-real-hash")
    .execute(&pool)
    .await
    .unwrap();

    // Create an API token for the service account
    let token_val = format!(
        "plat_svc_test_{}",
        Uuid::new_v4().to_string().replace('-', "")
    );
    let token_hash = platform::auth::token::hash_token(&token_val);
    let expires_at = chrono::Utc::now() + chrono::Duration::days(30);
    sqlx::query(
        "INSERT INTO api_tokens (user_id, name, token_hash, expires_at) VALUES ($1, $2, $3, $4)",
    )
    .bind(svc_id)
    .bind("svc-token")
    .bind(token_hash)
    .bind(expires_at)
    .execute(&pool)
    .await
    .unwrap();

    // Service account should be rejected when trying to register a passkey
    let (status, body) = helpers::post_json(
        &app,
        &token_val,
        "/api/auth/passkeys/register/begin",
        serde_json::json!({"name": "SvcKey"}),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "svc account register should fail: {body}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn begin_register_name_too_long(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let long_name = "k".repeat(256);
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/auth/passkeys/register/begin",
        serde_json::json!({"name": long_name}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn list_passkeys_unauthenticated(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);

    // Listing passkeys without authentication should fail
    let (status, _) = helpers::get_json(&app, "", "/api/auth/passkeys").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Additional coverage: complete_register error paths
// ---------------------------------------------------------------------------

/// complete_register with invalid/garbage credential JSON returns 422 (axum
/// deserialization error) or 400 because the RegisterPublicKeyCredential
/// struct cannot be parsed from arbitrary JSON.
#[sqlx::test(migrations = "./migrations")]
async fn complete_register_invalid_credential_json(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    // Send garbage that doesn't match RegisterPublicKeyCredential
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/auth/passkeys/register/complete",
        serde_json::json!({"not": "a credential"}),
    )
    .await;

    // Should fail at deserialization (422) since it's not a valid RegisterPublicKeyCredential
    assert!(
        status == StatusCode::UNPROCESSABLE_ENTITY || status == StatusCode::BAD_REQUEST,
        "expected 422 or 400, got {status}"
    );
}

/// complete_register without authentication should fail.
#[sqlx::test(migrations = "./migrations")]
async fn complete_register_unauthenticated(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);

    let (status, _) = helpers::post_json(
        &app,
        "",
        "/api/auth/passkeys/register/complete",
        serde_json::json!({
            "id": "dGVzdA",
            "rawId": "dGVzdA",
            "type": "public-key",
            "response": {
                "attestationObject": "dGVzdA",
                "clientDataJSON": "dGVzdA"
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Additional coverage: complete_login error paths
// ---------------------------------------------------------------------------

/// complete_login with no passkey credentials in DB at all returns 401
/// (discoverable_keys is empty).
#[sqlx::test(migrations = "./migrations")]
async fn complete_login_no_credentials_in_db(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);

    // No passkey credentials exist in DB at all
    let (status, _) = helpers::post_json(
        &app,
        "",
        "/api/auth/passkey/login/complete",
        serde_json::json!({
            "challenge_id": "some-challenge-id",
            "credential": {
                "id": "dGVzdA",
                "rawId": "dGVzdA",
                "type": "public-key",
                "response": {
                    "authenticatorData": "dGVzdA",
                    "clientDataJSON": "dGVzdA",
                    "signature": "dGVzdA"
                }
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// complete_login with invalid credential JSON returns 422 (deserialization).
#[sqlx::test(migrations = "./migrations")]
async fn complete_login_invalid_credential_json(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);

    let (status, _) = helpers::post_json(
        &app,
        "",
        "/api/auth/passkey/login/complete",
        serde_json::json!({"not": "valid"}),
    )
    .await;
    // Missing required fields in CompleteLoginRequest
    assert!(
        status == StatusCode::UNPROCESSABLE_ENTITY || status == StatusCode::BAD_REQUEST,
        "expected 422 or 400, got {status}"
    );
}

/// complete_login with credentials from a deactivated user returns 401.
/// The discoverable_keys query filters for is_active = true, so inactive
/// users' credentials are excluded.
#[sqlx::test(migrations = "./migrations")]
async fn complete_login_deactivated_user_credential(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    // Create a user, add a passkey credential, then deactivate them
    let (user_id, _user_token) =
        create_user(&app, &admin_token, "pk-deactivated", "pkdeact@test.com").await;

    sqlx::query(
        "INSERT INTO passkey_credentials (user_id, credential_id, public_key, name, transports)
         VALUES ($1, $2, $3, 'DeactKey', ARRAY[]::text[])",
    )
    .bind(user_id)
    .bind(vec![70u8, 71, 72, 73])
    .bind(serde_json::to_vec(&serde_json::json!({"type": "public-key"})).unwrap())
    .execute(&pool)
    .await
    .unwrap();

    // Deactivate the user
    let (status, _) =
        helpers::delete_json(&app, &admin_token, &format!("/api/users/{user_id}")).await;
    assert_eq!(status, StatusCode::OK);

    // Now try passkey login complete — should fail because user is inactive
    let (status, _) = helpers::post_json(
        &app,
        "",
        "/api/auth/passkey/login/complete",
        serde_json::json!({
            "challenge_id": "fake-challenge",
            "credential": {
                "id": "dGVzdA",
                "rawId": "dGVzdA",
                "type": "public-key",
                "response": {
                    "authenticatorData": "dGVzdA",
                    "clientDataJSON": "dGVzdA",
                    "signature": "dGVzdA"
                }
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Additional coverage: begin_register edge cases
// ---------------------------------------------------------------------------

/// begin_register with empty name should be rejected (min length 1).
#[sqlx::test(migrations = "./migrations")]
async fn begin_register_empty_name_rejected(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/auth/passkeys/register/begin",
        serde_json::json!({"name": ""}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// begin_register with max-length name (255) should succeed.
#[sqlx::test(migrations = "./migrations")]
async fn begin_register_max_length_name_ok(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let name = "a".repeat(255);
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/auth/passkeys/register/begin",
        serde_json::json!({"name": name}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "begin_register with 255-char name failed: {body}"
    );
}

/// begin_register with existing credentials excludes them in the challenge.
/// We can't verify the exclude list directly, but we can verify the endpoint
/// succeeds when credentials already exist.
#[sqlx::test(migrations = "./migrations")]
async fn begin_register_with_existing_credentials(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let user_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    // Insert two existing credentials
    for i in 0..2u8 {
        sqlx::query(
            "INSERT INTO passkey_credentials (user_id, credential_id, public_key, name, transports)
             VALUES ($1, $2, $3, $4, ARRAY[]::text[])",
        )
        .bind(user_id)
        .bind(vec![80u8 + i, 81, 82])
        .bind(serde_json::to_vec(&serde_json::json!({"type": "public-key"})).unwrap())
        .bind(format!("Existing Key {i}"))
        .execute(&pool)
        .await
        .unwrap();
    }

    // begin_register should still work, with those credentials in the exclude list
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/auth/passkeys/register/begin",
        serde_json::json!({"name": "Third Key"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "begin_register with existing creds failed: {body}"
    );
}

// ---------------------------------------------------------------------------
// Additional coverage: list passkeys response fields
// ---------------------------------------------------------------------------

/// Verify list_passkeys response includes backup and last_used_at fields.
#[sqlx::test(migrations = "./migrations")]
async fn list_passkeys_response_fields(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let user_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let cred_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO passkey_credentials (id, user_id, credential_id, public_key, name,
         backup_eligible, backup_state, transports)
         VALUES ($1, $2, $3, $4, 'Full Key', true, false, ARRAY['usb', 'nfc']::text[])",
    )
    .bind(cred_id)
    .bind(user_id)
    .bind(vec![90u8, 91, 92])
    .bind(serde_json::to_vec(&serde_json::json!({"type": "public-key"})).unwrap())
    .execute(&pool)
    .await
    .unwrap();

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/auth/passkeys").await;
    assert_eq!(status, StatusCode::OK);
    let keys = body.as_array().unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0]["id"], cred_id.to_string());
    assert_eq!(keys[0]["name"], "Full Key");
    assert_eq!(keys[0]["backup_eligible"], true);
    assert_eq!(keys[0]["backup_state"], false);
    assert!(keys[0]["last_used_at"].is_null());
    assert!(keys[0]["created_at"].is_string());
    let transports = keys[0]["transports"].as_array().unwrap();
    assert_eq!(transports.len(), 2);
    assert_eq!(transports[0], "usb");
    assert_eq!(transports[1], "nfc");
}

/// Verify list_passkeys only returns the current user's passkeys, not all.
#[sqlx::test(migrations = "./migrations")]
async fn list_passkeys_only_own_credentials(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let admin_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let (other_user_id, other_token) =
        create_user(&app, &admin_token, "pk-other-list", "pkotherlist@test.com").await;

    // Insert credential for admin
    sqlx::query(
        "INSERT INTO passkey_credentials (user_id, credential_id, public_key, name, transports)
         VALUES ($1, $2, $3, 'Admin Key', ARRAY[]::text[])",
    )
    .bind(admin_id)
    .bind(vec![100u8, 101, 102])
    .bind(serde_json::to_vec(&serde_json::json!({"type": "public-key"})).unwrap())
    .execute(&pool)
    .await
    .unwrap();

    // Insert credential for other user
    sqlx::query(
        "INSERT INTO passkey_credentials (user_id, credential_id, public_key, name, transports)
         VALUES ($1, $2, $3, 'Other Key', ARRAY[]::text[])",
    )
    .bind(other_user_id)
    .bind(vec![103u8, 104, 105])
    .bind(serde_json::to_vec(&serde_json::json!({"type": "public-key"})).unwrap())
    .execute(&pool)
    .await
    .unwrap();

    // Admin sees only their key
    let (status, body) = helpers::get_json(&app, &admin_token, "/api/auth/passkeys").await;
    assert_eq!(status, StatusCode::OK);
    let keys = body.as_array().unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0]["name"], "Admin Key");

    // Other user sees only their key
    let (status, body) = helpers::get_json(&app, &other_token, "/api/auth/passkeys").await;
    assert_eq!(status, StatusCode::OK);
    let keys = body.as_array().unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0]["name"], "Other Key");
}

// ---------------------------------------------------------------------------
// Additional coverage: delete passkey audit trail
// ---------------------------------------------------------------------------

/// Verify that deleting a passkey creates an audit log entry.
#[sqlx::test(migrations = "./migrations")]
async fn delete_passkey_creates_audit_log(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let user_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    let cred_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO passkey_credentials (id, user_id, credential_id, public_key, name, transports)
         VALUES ($1, $2, $3, $4, 'AuditKey', ARRAY[]::text[])",
    )
    .bind(cred_id)
    .bind(user_id)
    .bind(vec![110u8, 111, 112])
    .bind(serde_json::to_vec(&serde_json::json!({"type": "public-key"})).unwrap())
    .execute(&pool)
    .await
    .unwrap();

    let (status, _) =
        helpers::delete_json(&app, &admin_token, &format!("/api/auth/passkeys/{cred_id}")).await;
    assert_eq!(status, StatusCode::OK);

    // Check that audit log has the delete entry
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT action, resource FROM audit_log WHERE resource_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(cred_id)
    .fetch_optional(&pool)
    .await
    .unwrap();

    let (action, resource) = row.expect("audit log entry should exist");
    assert_eq!(action, "auth.passkey_delete");
    assert_eq!(resource, "passkey_credential");
}

// ---------------------------------------------------------------------------
// Additional coverage: begin_login multiple calls produce different challenges
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn begin_login_produces_unique_challenge_ids(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);

    let (status1, body1) = helpers::post_json(
        &app,
        "",
        "/api/auth/passkey/login/begin",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status1, StatusCode::OK);

    let (status2, body2) = helpers::post_json(
        &app,
        "",
        "/api/auth/passkey/login/begin",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status2, StatusCode::OK);

    let id1 = body1["challenge_id"].as_str().unwrap();
    let id2 = body2["challenge_id"].as_str().unwrap();
    assert_ne!(
        id1, id2,
        "each begin_login should produce a unique challenge_id"
    );
}

// ---------------------------------------------------------------------------
// Additional coverage: begin_register unauthenticated
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn begin_register_unauthenticated(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);

    let (status, _) = helpers::post_json(
        &app,
        "",
        "/api/auth/passkeys/register/begin",
        serde_json::json!({"name": "NoAuth"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Additional coverage: delete and rename passkey unauthenticated
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn delete_passkey_unauthenticated(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);

    let fake_id = Uuid::new_v4();
    let (status, _) =
        helpers::delete_json(&app, "", &format!("/api/auth/passkeys/{fake_id}")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn rename_passkey_unauthenticated(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);

    let fake_id = Uuid::new_v4();
    let (status, _) = helpers::patch_json(
        &app,
        "",
        &format!("/api/auth/passkeys/{fake_id}"),
        serde_json::json!({"name": "NoAuth"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Additional coverage: list passkeys ordered by created_at DESC
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_passkeys_ordered_by_created_at_desc(pool: PgPool) {
    let state = test_state(pool.clone()).await;
    let app = test_router(state);
    let admin_token = admin_login(&app).await;

    let (_, me) = helpers::get_json(&app, &admin_token, "/api/auth/me").await;
    let user_id = Uuid::parse_str(me["id"].as_str().unwrap()).unwrap();

    // Insert two credentials with a small delay
    sqlx::query(
        "INSERT INTO passkey_credentials (user_id, credential_id, public_key, name, transports, created_at)
         VALUES ($1, $2, $3, 'First Key', ARRAY[]::text[], now() - interval '1 hour')",
    )
    .bind(user_id)
    .bind(vec![120u8, 121])
    .bind(serde_json::to_vec(&serde_json::json!({"type": "public-key"})).unwrap())
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO passkey_credentials (user_id, credential_id, public_key, name, transports, created_at)
         VALUES ($1, $2, $3, 'Second Key', ARRAY[]::text[], now())",
    )
    .bind(user_id)
    .bind(vec![122u8, 123])
    .bind(serde_json::to_vec(&serde_json::json!({"type": "public-key"})).unwrap())
    .execute(&pool)
    .await
    .unwrap();

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/auth/passkeys").await;
    assert_eq!(status, StatusCode::OK);
    let keys = body.as_array().unwrap();
    assert_eq!(keys.len(), 2);
    // Most recent first (DESC)
    assert_eq!(keys[0]["name"], "Second Key");
    assert_eq!(keys[1]["name"], "First Key");
}
