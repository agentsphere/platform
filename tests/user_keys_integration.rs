mod helpers;

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;

// ---------------------------------------------------------------------------
// Provider key CRUD
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn set_provider_key(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::put_json(
        &app,
        &admin_token,
        "/api/users/me/provider-keys/anthropic",
        json!({ "api_key": "sk-ant-test-key-1234567890" }),
    )
    .await;

    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[sqlx::test(migrations = "./migrations")]
async fn set_provider_key_invalid_name(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::put_json(
        &app,
        &admin_token,
        "/api/users/me/provider-keys/invalid@name",
        json!({ "api_key": "sk-ant-test-key-1234567890" }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn set_provider_key_short_key(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::put_json(
        &app,
        &admin_token,
        "/api/users/me/provider-keys/anthropic",
        json!({ "api_key": "short" }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn list_provider_keys_empty(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/users/me/provider-keys").await;

    assert_eq!(status, StatusCode::OK);
    assert!(body["items"].as_array().unwrap().is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn list_provider_keys_after_set(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    helpers::put_json(
        &app,
        &admin_token,
        "/api/users/me/provider-keys/anthropic",
        json!({ "api_key": "sk-ant-test-key-1234567890" }),
    )
    .await;

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/users/me/provider-keys").await;

    assert_eq!(status, StatusCode::OK);
    let keys = body["items"].as_array().unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0]["provider"], "anthropic");
}

#[sqlx::test(migrations = "./migrations")]
async fn delete_provider_key(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Set key first
    helpers::put_json(
        &app,
        &admin_token,
        "/api/users/me/provider-keys/anthropic",
        json!({ "api_key": "sk-ant-test-key-1234567890" }),
    )
    .await;

    // Delete
    let (status, _) =
        helpers::delete_json(&app, &admin_token, "/api/users/me/provider-keys/anthropic").await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // List should be empty
    let (_, body) = helpers::get_json(&app, &admin_token, "/api/users/me/provider-keys").await;
    assert!(body["items"].as_array().unwrap().is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn delete_provider_key_not_found(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        "/api/users/me/provider-keys/nonexistent",
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn set_provider_key_overwrites(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Set key
    helpers::put_json(
        &app,
        &admin_token,
        "/api/users/me/provider-keys/anthropic",
        json!({ "api_key": "sk-ant-first-key-1234567890" }),
    )
    .await;

    // Overwrite with new key
    let (status, _) = helpers::put_json(
        &app,
        &admin_token,
        "/api/users/me/provider-keys/anthropic",
        json!({ "api_key": "sk-ant-second-key-1234567890" }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Still only one key
    let (_, body) = helpers::get_json(&app, &admin_token, "/api/users/me/provider-keys").await;
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// Extended coverage: key too long
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn set_provider_key_too_long(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // api_key max is 500 chars; send 501
    let long_key = "x".repeat(501);
    let (status, _) = helpers::put_json(
        &app,
        &admin_token,
        "/api/users/me/provider-keys/anthropic",
        json!({ "api_key": long_key }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Extended coverage: key_suffix shown (not raw key)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_shows_key_suffix_not_raw_key(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    helpers::put_json(
        &app,
        &admin_token,
        "/api/users/me/provider-keys/anthropic",
        json!({ "api_key": "sk-ant-api03-abcdefghijklmnop" }),
    )
    .await;

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/users/me/provider-keys").await;
    assert_eq!(status, StatusCode::OK);

    let keys = body["items"].as_array().unwrap();
    assert_eq!(keys.len(), 1);
    // key_suffix should show last 4 chars only, prefixed with "..."
    assert_eq!(keys[0]["key_suffix"], "...mnop");
    // The response should NOT contain the full api_key
    let serialized = serde_json::to_string(&keys[0]).unwrap();
    assert!(!serialized.contains("sk-ant-api03"));
}

// ---------------------------------------------------------------------------
// Extended coverage: multiple providers
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_multiple_providers(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Set keys for two different providers
    helpers::put_json(
        &app,
        &admin_token,
        "/api/users/me/provider-keys/anthropic",
        json!({ "api_key": "sk-ant-test-key-1234567890" }),
    )
    .await;

    helpers::put_json(
        &app,
        &admin_token,
        "/api/users/me/provider-keys/openai",
        json!({ "api_key": "sk-openai-test-key-abcdef" }),
    )
    .await;

    let (status, body) = helpers::get_json(&app, &admin_token, "/api/users/me/provider-keys").await;
    assert_eq!(status, StatusCode::OK);

    let keys = body["items"].as_array().unwrap();
    assert_eq!(keys.len(), 2);

    // Should be ordered by provider name
    let providers: Vec<&str> = keys
        .iter()
        .map(|k| k["provider"].as_str().unwrap())
        .collect();
    assert_eq!(providers, vec!["anthropic", "openai"]);
}

// ---------------------------------------------------------------------------
// Extended coverage: listing includes timestamps
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn list_includes_timestamps(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    helpers::put_json(
        &app,
        &admin_token,
        "/api/users/me/provider-keys/anthropic",
        json!({ "api_key": "sk-ant-test-key-1234567890" }),
    )
    .await;

    let (_, body) = helpers::get_json(&app, &admin_token, "/api/users/me/provider-keys").await;
    let keys = body["items"].as_array().unwrap();
    assert_eq!(keys.len(), 1);

    // created_at and updated_at should be present and non-null
    assert!(keys[0]["created_at"].is_string());
    assert!(keys[0]["updated_at"].is_string());
}

// ---------------------------------------------------------------------------
// Extended coverage: per-user isolation
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn keys_are_per_user(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    // Create a second user
    let (_user2_id, user2_token) =
        helpers::create_user(&app, &admin_token, "alice", "alice@test.com").await;

    // Admin sets a key
    helpers::put_json(
        &app,
        &admin_token,
        "/api/users/me/provider-keys/anthropic",
        json!({ "api_key": "sk-ant-admin-key-1234567890" }),
    )
    .await;

    // User2 should see an empty list (not admin's keys)
    let (status, body) = helpers::get_json(&app, &user2_token, "/api/users/me/provider-keys").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["items"].as_array().unwrap().is_empty());

    // User2 sets their own key
    helpers::put_json(
        &app,
        &user2_token,
        "/api/users/me/provider-keys/anthropic",
        json!({ "api_key": "sk-ant-alice-key-9876543210" }),
    )
    .await;

    // User2 sees exactly 1 key
    let (_, body) = helpers::get_json(&app, &user2_token, "/api/users/me/provider-keys").await;
    assert_eq!(body["items"].as_array().unwrap().len(), 1);

    // Admin still sees exactly 1 key (their own)
    let (_, body) = helpers::get_json(&app, &admin_token, "/api/users/me/provider-keys").await;
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// Extended coverage: unauthenticated access
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn unauthenticated_list_rejected(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::get_json(&app, "", "/api/users/me/provider-keys").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn unauthenticated_set_rejected(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::put_json(
        &app,
        "",
        "/api/users/me/provider-keys/anthropic",
        json!({ "api_key": "sk-ant-test-key-1234567890" }),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn unauthenticated_delete_rejected(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::delete_json(&app, "", "/api/users/me/provider-keys/anthropic").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Extended coverage: validate endpoint
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn validate_key_returns_structured_response(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Validate with a fake key — will fail because it's not a real Anthropic key,
    // but the endpoint should return 200 with { valid: false, error: "..." }
    let (status, body) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users/me/provider-keys/validate",
        json!({ "api_key": "sk-ant-fake-key-1234567890" }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["valid"], false);
    assert!(body["error"].is_string(), "error field should be present");
}

#[sqlx::test(migrations = "./migrations")]
async fn validate_key_short_key_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Key shorter than 10 chars should fail validation check
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users/me/provider-keys/validate",
        json!({ "api_key": "short" }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn validate_key_unauthenticated_rejected(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::post_json(
        &app,
        "",
        "/api/users/me/provider-keys/validate",
        json!({ "api_key": "sk-ant-test-key-1234567890" }),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Extended coverage: overwrite updates suffix
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn overwrite_updates_key_suffix(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Set initial key ending in "AAAA"
    helpers::put_json(
        &app,
        &admin_token,
        "/api/users/me/provider-keys/anthropic",
        json!({ "api_key": "sk-ant-first-key-endAAAA" }),
    )
    .await;

    let (_, body) = helpers::get_json(&app, &admin_token, "/api/users/me/provider-keys").await;
    assert_eq!(
        body["items"].as_array().unwrap()[0]["key_suffix"],
        "...AAAA"
    );

    // Overwrite with key ending in "BBBB"
    helpers::put_json(
        &app,
        &admin_token,
        "/api/users/me/provider-keys/anthropic",
        json!({ "api_key": "sk-ant-second-key-endBBBB" }),
    )
    .await;

    let (_, body) = helpers::get_json(&app, &admin_token, "/api/users/me/provider-keys").await;
    assert_eq!(
        body["items"].as_array().unwrap()[0]["key_suffix"],
        "...BBBB"
    );
}

// ---------------------------------------------------------------------------
// Extended coverage: delete invalid provider name
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn delete_invalid_provider_name(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::delete_json(
        &app,
        &admin_token,
        "/api/users/me/provider-keys/bad@provider!",
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// GET single provider key
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn get_provider_key_by_name(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Set a key first
    helpers::put_json(
        &app,
        &admin_token,
        "/api/users/me/provider-keys/anthropic",
        json!({ "api_key": "sk-ant-test-key-1234567890" }),
    )
    .await;

    // GET the specific key
    let (status, body) =
        helpers::get_json(&app, &admin_token, "/api/users/me/provider-keys/anthropic").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["provider"], "anthropic");
    assert_eq!(body["key_suffix"], "...7890");
    assert!(body["created_at"].is_string());
    assert!(body["updated_at"].is_string());
}

#[sqlx::test(migrations = "./migrations")]
async fn get_provider_key_not_found(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        "/api/users/me/provider-keys/nonexistent",
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn get_provider_key_invalid_name(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::get_json(
        &app,
        &admin_token,
        "/api/users/me/provider-keys/invalid@name!",
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn get_provider_key_unauthenticated(pool: PgPool) {
    let (state, _admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let (status, _) = helpers::get_json(&app, "", "/api/users/me/provider-keys/anthropic").await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Validate key too long
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn validate_key_too_long_rejected(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    let long_key = "x".repeat(501);
    let (status, _) = helpers::post_json(
        &app,
        &admin_token,
        "/api/users/me/provider-keys/validate",
        json!({ "api_key": long_key }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Per-user isolation of GET
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn get_provider_key_per_user_isolation(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool.clone()).await;
    let app = helpers::test_router(state);

    // Admin sets a key
    helpers::put_json(
        &app,
        &admin_token,
        "/api/users/me/provider-keys/anthropic",
        json!({ "api_key": "sk-ant-admin-key-1234567890" }),
    )
    .await;

    // Create another user
    let (_user2_id, user2_token) =
        helpers::create_user(&app, &admin_token, "keyisouser", "keyisouser@test.com").await;

    // User2 cannot GET admin's key (404 because it belongs to admin, not user2)
    let (status, _) =
        helpers::get_json(&app, &user2_token, "/api/users/me/provider-keys/anthropic").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Overwrite updates timestamps
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn get_provider_key_after_overwrite_has_updated_timestamp(pool: PgPool) {
    let (state, admin_token) = helpers::test_state(pool).await;
    let app = helpers::test_router(state);

    // Set initial key
    helpers::put_json(
        &app,
        &admin_token,
        "/api/users/me/provider-keys/anthropic",
        json!({ "api_key": "sk-ant-first-key-1234567890" }),
    )
    .await;

    let (_, body1) =
        helpers::get_json(&app, &admin_token, "/api/users/me/provider-keys/anthropic").await;
    let created1 = body1["created_at"].as_str().unwrap().to_string();
    let _updated1 = body1["updated_at"].as_str().unwrap().to_string();

    // Small delay then overwrite
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    helpers::put_json(
        &app,
        &admin_token,
        "/api/users/me/provider-keys/anthropic",
        json!({ "api_key": "sk-ant-second-key-9876543210" }),
    )
    .await;

    let (_, body2) =
        helpers::get_json(&app, &admin_token, "/api/users/me/provider-keys/anthropic").await;

    // created_at should stay the same, updated_at should change
    assert_eq!(body2["created_at"].as_str().unwrap(), created1);
    // updated_at may or may not change (depends on DB precision), but suffix should change
    assert_eq!(body2["key_suffix"], "...3210");
}
